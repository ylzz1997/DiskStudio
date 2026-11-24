//! Bounded, aligned reads and verified streaming copies. No device discovery here.
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub struct Region<R> {
    inner: R,
    start: u64,
    len: u64,
    pos: u64,
    window_size: usize,
    max_windows: usize,
    windows: VecDeque<Window>,
}
struct Window {
    start: u64,
    bytes: Vec<u8>,
}
impl<R: Read + Seek> Region<R> {
    pub fn new(inner: R, start: u64, len: u64) -> io::Result<Self> {
        Self::with_cache(inner, start, len, 1024 * 1024, 1)
    }

    pub(crate) fn for_metadata(inner: R, start: u64, len: u64) -> io::Result<Self> {
        Self::with_cache(inner, start, len, 4096, 1)
    }

    pub(crate) fn for_file_view(inner: R, start: u64, len: u64) -> io::Result<Self> {
        // Directory lookups alternate among indexes, inodes and allocation
        // maps. Keep recent windows to avoid repeatedly fetching the same data.
        Self::with_cache(inner, start, len, 1024 * 1024, 8)
    }

    fn with_cache(
        inner: R, start: u64, len: u64, window_size: usize, max_windows: usize,
    ) -> io::Result<Self> {
        if start.checked_add(len).is_none() || len == 0 || start % 512 != 0 || len % 512 != 0 {
            return Err(io::Error::other("Invalid aligned source range"));
        }
        Ok(Self {
            inner,
            start,
            len,
            pos: 0,
            window_size,
            max_windows,
            windows: VecDeque::new(),
        })
    }
}
impl<R: Read + Seek> Read for Region<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let start = self.pos / self.window_size as u64 * self.window_size as u64;
        let window = if let Some(i) = self.windows.iter().position(|w| w.start == start && !w.bytes.is_empty()) {
            self.windows.remove(i).unwrap()
        } else {
            let mut bytes = if self.windows.len() == self.max_windows {
                self.windows.pop_back().unwrap().bytes
            } else {
                Vec::new()
            };
            let valid = (self.len - start).min(self.window_size as u64) as usize;
            bytes.resize(valid, 0);
            self.inner.seek(SeekFrom::Start(self.start + start))?;
            self.inner.read_exact(&mut bytes).map_err(|e| {
                io::Error::new(e.kind(), format!("读取所选范围失败：相对偏移 {start}，请求 {valid} 字节，范围 {} 字节：{e}", self.len))
            })?;
            Window { start, bytes }
        };
        let offset = (self.pos - start) as usize;
        let n = out.len().min(window.bytes.len() - offset);
        out[..n].copy_from_slice(&window.bytes[offset..offset + n]);
        self.windows.push_front(window);
        self.pos += n as u64;
        Ok(n)
    }
}
impl<R: Read + Seek> Seek for Region<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
        };
        if next < 0 || next > self.len as i128 {
            return Err(io::Error::other("Seek outside selected source"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

impl Write for Region<File> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .pos
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| io::Error::other("Write outside selected range"))?;
        if self.pos % 512 != 0 || end % 512 != 0 {
            return Err(io::Error::other("Unaligned write in selected range"));
        }
        for window in &mut self.windows {
            window.bytes.clear();
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        self.inner.write_all(bytes)?;
        self.pos = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.sync_all()
    }
}

pub fn copy_exact<R: Read + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    size: u64,
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    // Only one ordered writer. Two queued buffers bound read-ahead to 16 MiB
    // including the producer and consumer buffers. Dropping rx unblocks send;
    // scope joins the reader on every exit, including cancellation and panic.
    progress(0)?;
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let worker = scope.spawn(move || {
            let mut left = size;
            while left > 0 {
                let n = left.min(4 * 1024 * 1024) as usize;
                let mut buf = vec![0; n];
                let result = reader
                    .read_exact(&mut buf)
                    .map(|_| buf)
                    .map_err(|e| format!("读取失败，未跳过坏扇区：{e}"));
                let failed = result.is_err();
                if tx.send(result).is_err() || failed {
                    break;
                }
                left -= n as u64;
            }
        });
        let result = (|| {
            let mut done = 0;
            let mut hash = Sha256::new();
            while done < size {
                progress(done)?;
                let buf = rx.recv().map_err(|_| "读取线程提前结束")??;
                progress(done)?;
                writer
                    .write_all(&buf)
                    .map_err(|e| format!("写入失败：{e}"))?;
                hash.update(&buf);
                done += buf.len() as u64;
            }
            writer.flush().map_err(|e| e.to_string())?;
            progress(done)?;
            Ok(format!("{:x}", hash.finalize()))
        })();
        drop(rx);
        worker.join().map_err(|_| "读取线程异常退出".to_string())?;
        result
    })
}

/// Advance each logical stream by one bounded chunk before starting the next
/// round. Keep reads on one thread because the streams may share a device
/// handle and its current file position.
pub(crate) fn copy_rounds<R: Read, W: Write>(
    copies: &mut [(R, W, u64)],
    mut checkpoint: impl FnMut(usize, u64, bool) -> Result<(), String>,
) -> Result<Vec<String>, (usize, String)> {
    let mut buffer = vec![0; 4 * 1024 * 1024];
    let mut hashes = vec![Sha256::new(); copies.len()];
    let largest = copies.iter().map(|(_, _, size)| *size).max().unwrap_or(0);
    let mut done = 0;
    for offset in (0..largest).step_by(buffer.len()) {
        for (index, (input, output, size)) in copies.iter_mut().enumerate() {
            let len = size.saturating_sub(offset).min(buffer.len() as u64) as usize;
            if len == 0 {
                continue;
            }
            checkpoint(index, done, false).map_err(|error| (index, error))?;
            let bytes = &mut buffer[..len];
            input.read_exact(bytes).map_err(|error| (index, format!("读取失败，未跳过坏扇区：{error}")))?;
            checkpoint(index, done, true).map_err(|error| (index, error))?;
            output.write_all(bytes).map_err(|error| (index, format!("写入失败：{error}")))?;
            hashes[index].update(bytes);
            done += len as u64;
        }
    }
    Ok(hashes.into_iter().map(|hash| format!("{:x}", hash.finalize())).collect())
}

pub fn hash_exact<R: Read + Send>(
    reader: &mut R,
    size: u64,
    progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    copy_exact(reader, &mut io::sink(), size, progress)
}

pub fn safe_name(name: &str) -> String {
    let value: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    let value = value.trim_matches(['.', ' ']);
    if value.is_empty() {
        "recovered-file".into()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn bounded_reader_never_exposes_adjacent_partition() {
        let mut r = Region::new(Cursor::new(vec![7; 2048]), 512, 512).unwrap();
        let mut bytes = vec![];
        r.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 512);
        assert!(r.seek(SeekFrom::Start(513)).is_err());
        assert!(r.seek(SeekFrom::End(-513)).is_err());
        assert!(Region::new(Cursor::new(vec![]), u64::MAX, 512).is_err());
    }
    #[test]
    fn failed_cache_fill_never_exposes_partial_or_stale_bytes() {
        let mut r = Region::new(Cursor::new(vec![9; 512]), 0, 1024).unwrap();
        let mut byte = [0];
        assert!(r.read(&mut byte).is_err());
        assert!(r.read(&mut byte).is_err());
        r.seek(SeekFrom::Start(512)).unwrap();
        assert!(r.read(&mut byte).is_err());
    }
    #[test]
    fn streaming_copy_detects_short_input_and_cancel() {
        let mut dest = vec![];
        assert!(copy_exact(&mut Cursor::new(vec![1; 3]), &mut dest, 4, |_| Ok(())).is_err());
        assert!(dest.is_empty());
        assert!(
            copy_exact(&mut Cursor::new(vec![1; 4]), &mut dest, 4, |_| Err(
                "cancel".into()
            ))
            .is_err()
        );
        assert!(dest.is_empty());
    }
    #[test]
    fn streaming_copy_and_readback_hash_match() {
        let source = vec![42; 4 * 1024 * 1024 + 512];
        let mut dest = vec![];
        let hash = copy_exact(
            &mut Cursor::new(&source),
            &mut dest,
            source.len() as u64,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(source, dest);
        assert_eq!(
            hash,
            hash_exact(&mut Cursor::new(dest), source.len() as u64, |_| Ok(())).unwrap()
        );
    }
    #[test]
    fn recovery_names_cannot_traverse_or_make_ads() {
        assert_eq!(safe_name("../..\\file.txt:stream"), "_.._file.txt_stream");
        assert_eq!(safe_name("..."), "recovered-file");
    }
    #[test]
    fn pipeline_stops_after_cancel_and_propagates_writer_failure() {
        let source = vec![1; 32 * 1024 * 1024];
        let mut output = vec![];
        let result = copy_exact(
            &mut Cursor::new(&source),
            &mut output,
            source.len() as u64,
            |n| {
                if n >= 4 * 1024 * 1024 {
                    Err("cancel".into())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result.unwrap_err(), "cancel");
        assert_eq!(output.len(), 4 * 1024 * 1024);
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(copy_exact(
            &mut Cursor::new(source),
            &mut Broken,
            32 * 1024 * 1024,
            |_| Ok(())
        )
        .unwrap_err()
        .contains("fixture failure"));
    }
}

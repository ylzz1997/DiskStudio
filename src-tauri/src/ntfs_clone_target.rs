//! Random access to the confirmed target extent, with a final ordered readback.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
};

pub(crate) const PAGE: u64 = 64 * 1024;
const CHUNK: usize = 4 * 1024 * 1024;

pub(super) struct Target<R> {
    device: R,
    size: u64,
    written: BTreeMap<u64, [u8; 32]>,
    page: Vec<u8>,
    sync: fn(&mut R) -> std::io::Result<()>,
    preserve: bool,
}

fn io_error(error: std::io::Error) -> String {
    format!("NTFS 克隆目标读写失败：{error}")
}

impl<R: Read + Write + Seek> Target<R> {
    pub(super) fn new(device: R, size: u64, sync: fn(&mut R) -> std::io::Result<()>) -> Self {
        Self {
            device,
            size,
            written: BTreeMap::new(),
            page: vec![0; PAGE as usize],
            sync,
            preserve: false,
        }
    }

    pub(super) fn prefilled(device: R, size: u64, sync: fn(&mut R) -> std::io::Result<()>) -> Self {
        let mut target = Self::new(device, size, sync);
        target.preserve = true;
        target
    }

    fn bounded(&self, offset: u64, count: usize) -> Result<(), String> {
        if offset
            .checked_add(count as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err("NTFS 克隆工具的读写请求超出目标分区。".into());
        }
        Ok(())
    }

    /// An unwritten page is logically zero throughout engine execution. This
    /// hides old target contents, even when a request changes only part of a page.
    fn load_page(&mut self, base: u64, size: usize) -> Result<(), String> {
        let page = &mut self.page[..size];
        let expected = self.written.get(&base);
        if expected.is_some() || self.preserve {
            self.device.seek(SeekFrom::Start(base)).map_err(io_error)?;
            self.device.read_exact(page).map_err(io_error)?;
            if expected.is_some_and(|digest| <[u8; 32]>::from(Sha256::digest(page)) != *digest) {
                return Err("NTFS 目标的页数据与此前写入内容不一致。".into());
            }
        } else {
            page.fill(0);
        }
        Ok(())
    }

    pub(super) fn read(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
        self.bounded(offset, bytes.len())?;
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let base = at / PAGE * PAGE;
            let size = (self.size - base).min(PAGE) as usize;
            let first = (at - base) as usize;
            let n = (bytes.len() - done).min(size - first);
            self.load_page(base, size)?;
            bytes[done..done + n].copy_from_slice(&self.page[first..first + n]);
            done += n;
        }
        Ok(())
    }

    pub(super) fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), String> {
        self.bounded(offset, bytes.len())?;
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let base = at / PAGE * PAGE;
            let size = (self.size - base).min(PAGE) as usize;
            let first = (at - base) as usize;
            let n = (bytes.len() - done).min(size - first);
            if n != size {
                self.load_page(base, size)?;
            }
            self.page[first..first + n].copy_from_slice(&bytes[done..done + n]);
            let page = &self.page[..size];
            self.device.seek(SeekFrom::Start(base)).map_err(io_error)?;
            self.device.write_all(page).map_err(io_error)?;
            self.written.insert(base, Sha256::digest(page).into());
            done += n;
        }
        Ok(())
    }

    pub(super) fn flush(&mut self) -> Result<(), String> {
        (self.sync)(&mut self.device).map_err(io_error)
    }

    pub(super) fn verify_changes(mut self, mut checkpoint: impl FnMut() -> Result<(), String>) -> Result<BTreeMap<u64, [u8; 32]>, String> {
        self.flush()?;
        let pages: Vec<_> = self.written.keys().copied().collect();
        for base in pages {
            checkpoint()?;
            self.load_page(base, (self.size - base).min(PAGE) as usize)?;
        }
        Ok(self.written)
    }

    pub(super) fn finish(
        mut self,
        mut progress: impl FnMut(&str, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let mut buffer = vec![0; CHUNK];
        let mut from = 0;
        for to in self
            .written
            .keys()
            .copied()
            .chain(std::iter::once(self.size))
        {
            while from < to {
                progress("清零未使用范围", from)?;
                let n = (to - from).min(CHUNK as u64) as usize;
                self.device.seek(SeekFrom::Start(from)).map_err(io_error)?;
                self.device.write_all(&buffer[..n]).map_err(io_error)?;
                from += n as u64;
            }
            from = to.saturating_add(PAGE).min(self.size);
        }
        progress("清零未使用范围", self.size)?;
        self.flush()?;

        let mut digest = Sha256::new();
        self.device.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut at = 0;
        while at < self.size {
            progress("校验目标", at)?;
            let n = (self.size - at).min(CHUNK as u64) as usize;
            self.device.read_exact(&mut buffer[..n]).map_err(io_error)?;
            for page in buffer[..n].chunks(PAGE as usize) {
                let matches = match self.written.get(&at) {
                    Some(written) => <[u8; 32]>::from(Sha256::digest(page)) == *written,
                    None => page.iter().all(|&byte| byte == 0),
                };
                if !matches {
                    return Err("NTFS 目标完整回读与写入内容不一致。".into());
                }
                at += page.len() as u64;
            }
            digest.update(&buffer[..n]);
        }
        progress("校验目标", self.size)?;
        Ok(format!("{:x}", digest.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn partial_and_unwritten_pages_hide_previous_data_and_zero_the_tail() {
        let size = 2 * PAGE + 512;
        let mut output = Cursor::new(vec![0x5e; size as usize]);
        let mut target = Target::new(&mut output, size, Write::flush);
        target.write(PAGE - 1, b"abc").unwrap();
        target.write(PAGE + 1, b"d").unwrap();
        let mut around = [0xff; 5];
        target.read(PAGE - 2, &mut around).unwrap();
        assert_eq!(around, [0, b'a', b'b', b'd', 0]);
        assert!(target.write(size - 1, b"bad").is_err());
        let digest = target.finish(|_, _| Ok(())).unwrap();
        let mut expected = vec![0; size as usize];
        expected[PAGE as usize - 1..PAGE as usize + 2].copy_from_slice(b"abd");
        assert_eq!(digest, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(output.into_inner(), expected);
    }

    #[test]
    fn full_readback_rejects_a_corrupted_written_page() {
        let mut target = Target::new(Cursor::new(vec![0; PAGE as usize]), PAGE, |output| {
            output.get_mut()[0] ^= 1;
            Ok(())
        });
        target.write(7, b"contents").unwrap();
        assert!(target.finish(|_, _| Ok(())).is_err());
    }
}

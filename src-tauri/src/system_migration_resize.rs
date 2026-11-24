//! Give the NTFS tool an ordinary volume consisting of the confirmed target
//! extent and a temporary read-through view of any discarded source tail.
use crate::{
    data_io::Region, extfs::Engine, ntfs_file_clone, surface_scan::Range,
    system_migration::VolumeChange, used_space::View,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

const PAGE: u64 = ntfs_file_clone::TARGET_PAGE;

struct Volume<'a> {
    source: Region<&'a File>,
    target: Option<View<&'a mut File>>,
    change: &'a VolumeChange,
    size: u64,
    cursor: u64,
    tail: BTreeMap<u64, Vec<u8>>,
}

impl Read for Volume<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let end = self.cursor + (self.size - self.cursor).min(bytes.len() as u64);
        let start = self.cursor;
        while self.cursor < end {
            let count = if let Some(target) = self
                .target
                .as_mut()
                .filter(|_| self.cursor < self.change.new_bytes)
            {
                let count = (end.min(self.change.new_bytes) - self.cursor) as usize;
                let from = (self.cursor - start) as usize;
                target.seek(SeekFrom::Start(self.cursor))?;
                target.read_exact(&mut bytes[from..from + count])?;
                count
            } else {
                let base = self.cursor / PAGE * PAGE;
                let count = (end.min(base + PAGE) - self.cursor) as usize;
                let from = (self.cursor - start) as usize;
                let output = &mut bytes[from..from + count];
                if let Some(compressed) = self.tail.get(&base) {
                    let expected = (self.size - base).min(PAGE) as usize;
                    let page = zstd::bulk::decompress(compressed, expected)?;
                    if page.len() != expected {
                        return Err(io::Error::other("Invalid temporary NTFS tail page"));
                    }
                    let at = (self.cursor - base) as usize;
                    output.copy_from_slice(&page[at..at + count]);
                } else {
                    let original = self
                        .change
                        .old_bytes
                        .saturating_sub(self.cursor)
                        .min(count as u64) as usize;
                    if original != 0 {
                        self.source.seek(SeekFrom::Start(self.cursor))?;
                        self.source.read_exact(&mut output[..original])?;
                    }
                    output[original..].fill(0);
                }
                count
            };
            self.cursor += count as u64;
        }
        Ok((end - start) as usize)
    }
}

impl Seek for Volume<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let offset = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.cursor) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.size) + i128::from(n),
        };
        if !(0..=i128::from(self.size)).contains(&offset) {
            return Err(io::Error::other(
                "NTFS resize seek exceeds verified geometry",
            ));
        }
        self.cursor = offset as u64;
        Ok(self.cursor)
    }
}

impl Write for Volume<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let target = self
            .target
            .as_mut()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let count = bytes.len() as u64;
        if self.cursor % PAGE != 0
            || count == 0
            || count != (self.size - self.cursor).min(PAGE)
            || self.change.new_bytes % PAGE != 0
        {
            return Err(io::Error::other("Invalid NTFS resize page"));
        }
        if self.cursor < self.change.new_bytes {
            target.seek(SeekFrom::Start(self.cursor))?;
            target.write_all(bytes)?;
        } else {
            self.tail
                .insert(self.cursor, zstd::bulk::compress(bytes, 1)?);
        }
        self.cursor += count;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(target) = &mut self.target {
            target.inner_mut().sync_all()?;
        }
        Ok(())
    }
}

pub(super) fn resize(
    engine: &Engine,
    source: &File,
    target: Option<(&mut File, u64)>,
    change: &VolumeChange,
    sector: u64,
    cancel: &AtomicBool,
) -> Result<BTreeMap<u64, [u8; 32]>, String> {
    let dry_run = target.is_none();
    let target = target
        .map(|(file, size)| View::bounded(file, change.new_offset, change.new_bytes, size))
        .transpose()
        .map_err(|e| e.to_string())?;
    let view = Volume {
        source: Region::for_file_view(source, change.offset, change.old_bytes)
            .map_err(|e| e.to_string())?,
        target,
        change,
        size: change.old_bytes.max(change.new_bytes),
        cursor: 0,
        tail: BTreeMap::new(),
    };
    // The engine sees a volume-relative, seekable image of the target NTFS
    // partition. Its old tail can still be read from the verified source.
    let old = Range {
        offset: 0,
        size: change.old_bytes,
        sector,
    };
    let new = Range {
        size: change.new_bytes,
        ..old
    };
    let relative = ntfs_file_clone::resize_prepared_volume(
        engine,
        view,
        old,
        new,
        cancel,
        Write::flush,
        dry_run,
    )?;
    Ok(relative
        .into_iter()
        .filter(|(offset, _)| *offset < change.new_bytes)
        .map(|(offset, digest)| (change.new_offset + offset, digest))
        .collect())
}

/// The migration stream provides the expected bytes outside the pages written
/// by the NTFS tool. Each written page has a digest recorded as it was issued.
pub(super) fn verify_copy(
    mut original: impl Read + Seek,
    target: &mut File,
    size: u64,
    writes: BTreeMap<u64, [u8; 32]>,
    mut step: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    let err = |e: io::Error| e.to_string();
    target.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut at = 0;
    let mut expected = vec![0; 4 * 1024 * 1024];
    let mut actual = vec![0; expected.len()];
    let mut hash = Sha256::new();
    for (next, page) in writes
        .into_iter()
        .map(|(at, hash)| (at, Some(hash)))
        .chain(std::iter::once((size, None)))
    {
        if next < at || next > size || (page.is_some() && size - next < PAGE) {
            return Err("迁移调容写入记录超出确认的目标范围。".into());
        }
        if at < next {
            original.seek(SeekFrom::Start(at)).map_err(err)?;
        }
        while at < next {
            step(at)?;
            let count = (next - at).min(actual.len() as u64) as usize;
            original.read_exact(&mut expected[..count]).map_err(err)?;
            target.read_exact(&mut actual[..count]).map_err(err)?;
            if actual[..count] != expected[..count] {
                return Err("调容后目标未修改范围与源迁移流不一致。".into());
            }
            hash.update(&actual[..count]);
            at += count as u64;
        }
        if let Some(page) = page {
            step(at)?;
            let current = &mut actual[..PAGE as usize];
            target.read_exact(current).map_err(err)?;
            if <[u8; 32]>::from(Sha256::digest(&*current)) != page {
                return Err("调容后目标修改页 SHA-256 回读不匹配。".into());
            }
            hash.update(current);
            at += PAGE;
        }
    }
    step(size)?;
    Ok(format!("{:x}", hash.finalize()))
}

//! A bounded, read-only view that substitutes verified copies in memory.
use super::ntfs_reconstruct as ntfs;
use crate::ext_recovery::reconstruct as ext;
use crate::fat_recovery::reconstruct as fat;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Proof {
    Ntfs(ntfs::Proof),
    Fat(fat::Proof),
    Ext(ext::Proof),
}

impl Proof {
    pub(crate) fn open<R: Read + Seek + 'static>(
        mut reader: R,
        sector: u64,
        cancel: &AtomicBool,
    ) -> Result<(Reader<R>, Self), String> {
        if let Some(evidence) = ext::inspect(&mut reader, sector, cancel)? {
            return evidence
                .open(reader, sector, cancel)
                .map(|(reader, proof)| (reader, Self::Ext(proof)));
        }
        Self::open_boot(reader, sector, cancel)
    }

    pub(crate) fn open_boot<R: Read + Seek>(
        mut reader: R,
        sector: u64,
        cancel: &AtomicBool,
    ) -> Result<(Reader<R>, Self), String> {
        if let Some(found) = fat::inspect(&mut reader, sector, cancel)? {
            let proof = Self::Fat(found.proof.clone());
            return Ok((found.reader(reader), proof));
        }
        ntfs::open(reader, sector, cancel).map(|(reader, proof)| (reader, Self::Ntfs(proof)))
    }

    pub(crate) fn reopen<R: Read + Seek + 'static>(
        &self,
        reader: R,
        cancel: &AtomicBool,
    ) -> Result<Reader<R>, String> {
        let sector = match self {
            Self::Ntfs(proof) => proof.sector_size,
            Self::Fat(proof) => proof.sector_size,
            Self::Ext(proof) => proof.sector_size,
        };
        let (reader, observed) = Self::open(reader, sector, cancel)?;
        if observed != *self {
            return Err("文件视图的重建依据已变化，请重新读取。".into());
        }
        Ok(reader)
    }

    pub(crate) fn reopen_boot<R: Read + Seek>(
        &self,
        reader: R,
        cancel: &AtomicBool,
    ) -> Result<Reader<R>, String> {
        let sector = match self {
            Self::Ntfs(proof) => proof.sector_size,
            Self::Fat(proof) => proof.sector_size,
            Self::Ext(_) => return Err("当前候选没有备用引导凭据。".into()),
        };
        let (reader, observed) = Self::open_boot(reader, sector, cancel)?;
        if observed != *self {
            return Err("文件视图的重建依据已变化，请重新读取。".into());
        }
        Ok(reader)
    }

    #[cfg(windows)]
    pub(crate) fn description(&self) -> String {
        let locale = crate::native_locale::current();
        match self {
            Self::Ntfs(proof) => proof.description(),
            Self::Fat(proof) => locale
                .text(match proof.filesystem {
                    fat::Format::Fat32 => "已采用 FAT32 备用引导扇区。",
                    fat::Format::ExFat => "已采用 exFAT 备用引导区。",
                })
                .to_owned(),
            Self::Ext(_) => locale.text("已采用 Ext4 备用超级块。").to_owned(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Patch {
    pub(crate) at: u64,
    pub(crate) bytes: Vec<u8>,
}

/// The existing recovery engine reads patched byte slices as ordinary metadata.
/// The original handle is always read-only and all seeks stay in the volume.
pub(crate) struct Reader<R> {
    inner: R,
    patches: Vec<Patch>,
    size: u64,
    pos: u64,
}

impl<R> Reader<R> {
    pub(crate) fn new(inner: R, patches: Vec<Patch>, size: u64) -> Self {
        Self {
            inner,
            patches,
            size,
            pos: 0,
        }
    }
}

impl<R: Read + Seek> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut n = (self.size - self.pos).min(out.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        for patch in &self.patches {
            if self.pos < patch.at {
                n = (n as u64).min(patch.at - self.pos) as usize;
                break;
            }
            let at = self.pos - patch.at;
            if at < patch.bytes.len() as u64 {
                let tail = &patch.bytes[at as usize..];
                n = n.min(tail.len());
                out[..n].copy_from_slice(&tail[..n]);
                self.pos += n as u64;
                return Ok(n);
            }
        }
        self.inner.seek(SeekFrom::Start(self.pos))?;
        let n = self.inner.read(&mut out[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}
impl<R> Seek for Reader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(at) => i128::from(at),
            SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.size) + i128::from(n),
        };
        if !(0..=i128::from(self.size)).contains(&next) {
            return Err(io::Error::other("Seek outside reconstructed volume"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

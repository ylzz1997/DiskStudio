//! Backup boot records are hints until the complete bounded view corroborates them.
use super::{canceled, io, Candidate, Range, Read, Region, Seek, SeekFrom, Validation};
use crate::{
    fat_recovery::reconstruct as fat,
    recovery::{ntfs_reconstruct as ntfs, view},
};
use std::sync::atomic::AtomicBool;

pub(super) const BUDGET: u64 = 128 * 1024 * 1024;
const BUDGET_ERROR: &str = "备用引导候选元数据超过搜索预算，请缩小范围。";

#[derive(Clone, Copy)]
pub(super) struct Anchor {
    pub(super) start: u64,
    pub(super) size: u64,
    pub(super) filesystem: &'static str,
}

pub(super) fn supported(filesystem: &str) -> Option<&'static str> {
    match filesystem {
        "NTFS" => Some("NTFS"),
        "FAT32" => Some("FAT32"),
        "exFAT" => Some("exFAT"),
        _ => None,
    }
}

pub(super) fn hint(bytes: &[u8], offset: u64, range: Range) -> Option<Anchor> {
    let (filesystem, distance, size) =
        if let Some((distance, size)) = ntfs::backup_extent(bytes, range.sector) {
            ("NTFS", distance, size)
        } else {
            let (format, distance, size) = fat::backup_extent(bytes, range.sector)?;
            (format.name(), distance, size)
        };
    let start = offset.checked_sub(distance)?;
    if start < range.offset || start.checked_add(size)? > range.offset + range.size {
        return None;
    }
    Some(Anchor {
        start,
        size,
        filesystem,
    })
}

struct Evidence<'a, R> {
    reader: R,
    budget: &'a mut u64,
    read_error: Option<String>,
    cancel: &'a AtomicBool,
}

impl<R: Read + Seek> Read for Evidence<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        canceled(self.cancel).map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if *self.budget < bytes.len() as u64 {
            self.read_error = Some(BUDGET_ERROR.into());
            return Err(io::Error::other(BUDGET_ERROR));
        }
        *self.budget -= bytes.len() as u64;
        self.reader
            .read_exact(bytes)
            .map(|_| bytes.len())
            .inspect_err(|err| self.read_error = Some(err.to_string()))
    }
}

impl<R: Read + Seek> Seek for Evidence<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.reader
            .seek(from)
            .inspect_err(|err| self.read_error = Some(err.to_string()))
    }
}

pub(super) fn probe<R: Read + Seek>(
    reader: &mut R,
    sector: u64,
    anchor: Anchor,
    cancel: &AtomicBool,
    budget: &mut u64,
) -> Result<Option<Candidate>, String> {
    let mut evidence = Evidence {
        reader,
        budget,
        read_error: None,
        cancel,
    };
    let region = Region::for_metadata(&mut evidence, anchor.start, anchor.size)
        .map_err(|e| e.to_string())?;
    let checked = view::Proof::open(region, sector, cancel).map(|(_, proof)| proof);
    if let Some(message) = evidence.read_error {
        return Err(message);
    }
    canceled(cancel)?;
    let Ok(proof) = checked else {
        return Ok(None);
    };
    let label = match &proof {
        view::Proof::Ntfs(p)
            if anchor.filesystem == "NTFS" && p.backup_verified && p.bytes == anchor.size =>
        {
            "NTFS 备用引导、MFT 镜像与根目录已核对。"
        }
        view::Proof::Fat(p) if p.filesystem.name() == anchor.filesystem => match p.filesystem {
            fat::Format::Fat32 => "FAT32 备用引导、FSInfo、FAT 与根目录链已核对。",
            fat::Format::ExFat => "exFAT 备用引导区、分配位图、大小写表与根目录链已核对。",
        },
        _ => return Ok(None),
    };
    Ok(Some(Candidate {
        id: uuid::Uuid::new_v4().to_string(),
        filesystem: anchor.filesystem.into(),
        offset: anchor.start,
        size: anchor.size,
        evidence: label.into(),
        warning: "使用备用元数据重建文件视图。容量来自文件系统，可预览和提取已核对的文件。".into(),
        can_restore: false,
        can_browse: true,
        btrfs_member_count: None,
        validation: Validation::BackupBoot(proof),
        btrfs_member: None,
    }))
}

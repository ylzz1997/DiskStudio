//! Re-register checked filesystem extents in an intact GPT without removing or
//! changing existing entries. No paths or device access in this module.
use crate::{
    lost_partitions::{self, Candidate},
    surface_scan::Range,
    table_archive::{Archive, Kind},
    table_restore,
};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub size: u64,
    pub kind: String,
    pub name: String,
}
#[derive(Clone)]
pub struct Evidence {
    pub range: Range,
    pub candidate: Candidate,
    pub extent: Extent,
}
pub fn extent(
    reader: &mut (impl Read + Seek),
    range: Range,
    candidate: &Candidate,
    cancel: &AtomicBool,
) -> Result<Extent, String> {
    if !candidate.can_restore {
        return Err("该候选仅允许浏览和提取，无法登记分区记录。".into());
    }
    // Filesystem probes include sub-sector reads (e.g. FAT's 8-byte reserved
    // entries). Bound and align the raw stream before any evidence probe.
    drop(lost_partitions::candidate_reader(
        &mut *reader,
        range.offset.checked_add(range.size).ok_or("搜索范围溢出")?,
        range.sector,
        range,
        candidate,
        cancel,
    )?);
    let mut size = candidate.size;
    let kind = match candidate.filesystem.as_str() {
        "NTFS" => {
            // NTFS's recorded sector count excludes its final backup boot
            // sector. Include it only when the full sector matches the primary.
            size = size.checked_add(range.sector).ok_or("NTFS 容量溢出")?;
            let end = candidate.offset.checked_add(size).ok_or("NTFS 边界溢出")?;
            if end > range.offset + range.size {
                return Err("搜索范围未包含 NTFS 备用引导扇区，不能确定恢复边界。".into());
            }
            let mut first = vec![0; range.sector as usize];
            let mut last = vec![0; range.sector as usize];
            let mut reader = crate::data_io::Region::new(&mut *reader, candidate.offset, size)
                .map_err(|e| e.to_string())?;
            reader
                .seek(SeekFrom::Start(0))
                .and_then(|_| reader.read_exact(&mut first))
                .map_err(|e| e.to_string())?;
            reader
                .seek(SeekFrom::Start(size - range.sector))
                .and_then(|_| reader.read_exact(&mut last))
                .map_err(|e| e.to_string())?;
            if first != last {
                return Err("NTFS 主备引导扇区不一致，不能恢复分区记录。".into());
            }
            crate::partitions::policy::BASIC_DATA
        }
        "FAT12" | "FAT16" | "FAT32" | "exFAT" => crate::partitions::policy::BASIC_DATA,
        "Ext2" | "Ext3" | "Ext4" | "ext2" | "ext3" | "ext4" | "Btrfs" => {
            crate::partitions::policy::LINUX_DATA
        }
        _ => return Err("尚不支持为该文件系统恢复分区记录。".into()),
    };
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消候选边界核对。".into());
    }
    Ok(Extent {
        offset: candidate.offset,
        size,
        kind: kind.into(),
        name: format!("Recovered {}", candidate.filesystem),
    })
}
pub fn verify(
    reader: &mut (impl Read + Seek),
    evidence: &[Evidence],
    cancel: &AtomicBool,
) -> Result<(), String> {
    for e in evidence {
        if extent(reader, e.range, &e.candidate, cancel)? != e.extent {
            return Err("候选恢复范围已变化，请重新搜索。".into());
        }
    }
    Ok(())
}
pub fn add_entries(current: &Archive, extents: &[Extent]) -> Result<Archive, String> {
    if current.includes_boot || extents.is_empty() || extents.len() > 128 {
        return Err("请选择 1–128 个候选，仅恢复分区记录。".into());
    }
    // Validation requires both GPT copies and their entry arrays to agree.
    let layout = table_restore::archive_layout(current)?;
    let mut occupied: Vec<_> = layout
        .entries
        .iter()
        .map(|e| (e.offset, e.offset + e.size))
        .collect();
    let mut desired = current.clone();
    let mut entries = current
        .extents
        .iter()
        .find(|e| e.kind == Kind::PrimaryEntries)
        .ok_or("缺少主 GPT 条目")?
        .bytes
        .clone();
    let table_len = layout.count as usize * 128;
    for e in extents {
        let end = e.offset.checked_add(e.size).ok_or("候选边界溢出")?;
        if e.size == 0
            || e.offset % current.sector != 0
            || e.size % current.sector != 0
            || e.offset / current.sector < layout.first
            || end / current.sector > layout.last + 1
            || occupied.iter().any(|&(a, b)| e.offset < b && a < end)
        {
            return Err("候选与现有分区、其他候选或 GPT 保留区域重叠，不能恢复。".into());
        }
        if !matches!(
            e.kind.as_str(),
            crate::partitions::policy::BASIC_DATA | crate::partitions::policy::LINUX_DATA
        ) {
            return Err("只能恢复普通 Windows 或 Linux 数据分区记录。".into());
        }
        let name: Vec<_> = e.name.encode_utf16().collect();
        if name.len() > 36 || name.contains(&0) {
            return Err("恢复分区名称无效。".into());
        }
        let slot = entries[..table_len]
            .chunks_exact_mut(128)
            .find(|slot| slot.iter().all(|b| *b == 0))
            .ok_or("没有完整空白 GPT 条目可用。")?;
        slot[..16].copy_from_slice(
            &uuid::Uuid::parse_str(&e.kind)
                .map_err(|_| "分区类型无效")?
                .to_bytes_le(),
        );
        slot[16..32].copy_from_slice(&uuid::Uuid::new_v4().to_bytes_le());
        slot[32..40].copy_from_slice(&(e.offset / current.sector).to_le_bytes());
        slot[40..48].copy_from_slice(&(end / current.sector - 1).to_le_bytes());
        for (i, c) in name.iter().enumerate() {
            slot[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        occupied.push((e.offset, end));
    }
    let crc = crc32fast::hash(&entries[..table_len]);
    for e in &mut desired.extents {
        match e.kind {
            Kind::PrimaryEntries | Kind::BackupEntries => {
                e.bytes[..table_len].copy_from_slice(&entries[..table_len])
            }
            Kind::PrimaryHeader | Kind::BackupHeader => {
                e.bytes[88..92].copy_from_slice(&crc.to_le_bytes());
                e.bytes[16..20].fill(0);
                let len = u32::from_le_bytes(e.bytes[12..16].try_into().unwrap()) as usize;
                let crc = crc32fast::hash(&e.bytes[..len]);
                e.bytes[16..20].copy_from_slice(&crc.to_le_bytes());
            }
            Kind::Mbr => {}
            _ => return Err("恢复计划含有非 GPT 表范围。".into()),
        }
    }
    let after = table_restore::archive_layout(&desired)?;
    if after.guid != layout.guid
        || after.entries.len() != layout.entries.len() + extents.len()
        || layout.entries.iter().any(|e| !after.entries.contains(e))
    {
        return Err("恢复计划改变了既有 GPT 条目。".into());
    }
    Ok(desired)
}

#[cfg(test)]
#[path = "lost_partition_restore_tests.rs"]
mod tests;

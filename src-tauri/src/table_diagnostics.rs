//! Explicit, bounded partition-table inspection and reconstruction from one
//! independently valid GPT copy. No device APIs, paths, or automatic writes.
use crate::{
    table_archive::{read_at as read, read_ebrs, Archive, Extent, Kind},
    table_restore,
};
use disk_core::{
    mbr::EbrSector,
    table_check::{self, CheckItem, CheckState, TableReport},
    MIB,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek};

#[derive(Clone)]
pub struct Snapshot {
    pub size: u64,
    pub sector: u64,
    pub report: TableReport,
    head: Vec<u8>,
    tail: Vec<u8>,
    ebrs: Vec<EbrSector>,
    pub mbr_ranges: Option<Vec<(u64, u64)>>,
    pub mbr_container: Option<(u64, u64)>,
}
pub struct Repair {
    pub archive: Archive,
    pub description: String,
}
pub fn inspect(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Snapshot, String> {
    if !(4 * MIB..=1 << 43).contains(&size) || !matches!(sector, 512 | 4096) || size % sector != 0 {
        return Err("分区表检查支持 4 MiB–8 TiB、512/4096 字节逻辑扇区。".into());
    }
    checkpoint()?;
    let head = read(reader, 0, MIB as usize, size)?;
    checkpoint()?;
    let tail = read(reader, size - MIB, MIB as usize, size)?;
    let ebrs = read_ebrs(reader, &head, size, sector, &mut checkpoint)?;
    let inspected = table_check::inspect_image_with_geometry(
        &head,
        &tail,
        size,
        "partition-table.img",
        Some(sector),
        &ebrs,
    )?;
    let mbr_ranges = inspected.disk.filter(|d| d.scheme == "MBR").map(|d| {
        d.partitions
            .into_iter()
            .filter(|p| p.color != "free")
            .map(|p| (p.start, p.size))
            .collect()
    });
    let mut report = inspected.report;
    if report.sector_size.is_some_and(|n| n != sector) {
        add(
            &mut report,
            "device-sector",
            "设备逻辑扇区",
            CheckState::Failed,
            format!("分区表签名使用的扇区大小与设备报告的 {sector} B 不一致。"),
        );
    } else {
        add(
            &mut report,
            "device-sector",
            "设备逻辑扇区",
            CheckState::Passed,
            format!("设备句柄报告 {sector} B；读取范围按设备几何定位。"),
        );
    }
    let snapshot = Snapshot {
        size,
        sector,
        report,
        head,
        tail,
        ebrs,
        mbr_ranges,
        mbr_container: inspected.mbr_container,
    };
    snapshot.matches(reader, &mut checkpoint)?;
    Ok(snapshot)
}
pub fn add(report: &mut TableReport, code: &str, title: &str, state: CheckState, detail: String) {
    report.items.push(CheckItem {
        code: code.into(),
        title: title.into(),
        state,
        detail,
    });
    report.state = if report.items.iter().any(|i| i.state == CheckState::Failed) {
        CheckState::Failed
    } else if report
        .items
        .iter()
        .any(|i| i.state == CheckState::Unchecked)
    {
        CheckState::Unchecked
    } else if report.items.iter().any(|i| i.state == CheckState::Warning) {
        CheckState::Warning
    } else {
        CheckState::Passed
    };
}
impl Snapshot {
    pub fn retained_bytes(&self) -> u64 {
        (self.head.len() + self.tail.len()) as u64 + self.ebrs.len() as u64 * self.sector
    }
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.size.to_le_bytes());
        h.update(self.sector.to_le_bytes());
        h.update(&self.head);
        h.update(&self.tail);
        for e in &self.ebrs {
            h.update(e.offset.to_le_bytes());
            h.update(&e.bytes);
        }
        format!("{:x}", h.finalize())
    }
    pub fn matches(
        &self,
        reader: &mut (impl Read + Seek),
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for (at, bytes) in [(0, &self.head), (self.size - MIB, &self.tail)]
            .into_iter()
            .chain(self.ebrs.iter().map(|e| (e.offset, &e.bytes)))
        {
            checkpoint()?;
            if read(reader, at, bytes.len(), self.size)? != *bytes {
                return Err("分区表在检查后发生变化，请重新检查。".into());
            }
        }
        Ok(())
    }
    pub fn layout(&self) -> Result<table_restore::Layout, String> {
        table_restore::current_layout(&self.head, &self.tail, self.size, self.sector).map(|p| p.0)
    }
    fn good(&self, code: &str) -> bool {
        self.report
            .items
            .iter()
            .any(|i| i.code == code && matches!(i.state, CheckState::Passed | CheckState::Warning))
    }
    /// Only one valid copy, ordinary 128-byte entries, no PMBR correction and
    /// no choice between independently valid conflicting copies.
    pub fn repair(&self, identity: &str) -> Result<Option<Repair>, String> {
        if self.report.scheme != "GPT"
            || self.report.sector_size != Some(self.sector)
            || !self.ebrs.is_empty()
            || !self.good("device-sector")
        {
            return Err("当前只支持已确认逻辑扇区的 GPT 单份表修复。".into());
        }
        if !self.good("protective-mbr") {
            return Err("保护性 MBR 无效或为混合布局；当前不会重建 MBR。".into());
        }
        let current = self.layout()?;
        let primary = self.good("primary-gpt") && self.good("primary-layout");
        let backup = self.good("backup-gpt") && self.good("backup-layout");
        if primary && backup {
            return Ok(None);
        }
        if primary == backup {
            return Err("需要一份完整有效的 GPT，双表损坏不能推测恢复。".into());
        }
        let sector = self.sector as usize;
        let last = self.size / self.sector - 1;
        let source_header = if primary {
            &self.head[sector..2 * sector]
        } else {
            &self.tail[self.tail.len() - sector..]
        };
        let u64at = |n| u64::from_le_bytes(source_header[n..n + 8].try_into().unwrap());
        let table_len = current.count as usize * 128;
        let rounded = table_len.div_ceil(sector) * sector;
        let source_at = u64at(72) * self.sector;
        // Conventional GPT positions are reconstructed inside already checked
        // reserved areas. No partition entry, PMBR or source-copy byte changes.
        let target_at = if primary {
            (last - rounded as u64 / self.sector) * self.sector
        } else {
            2 * self.sector
        };
        let target_header_at = if primary {
            self.size - self.sector
        } else {
            self.sector
        };
        if target_at < 2 * self.sector
            || target_at + rounded as u64 > self.size - self.sector
            || (!primary && target_at + rounded as u64 > current.first * self.sector)
            || (primary && target_at <= current.last * self.sector)
        {
            return Err("重建 GPT 的保留范围不足。".into());
        }
        let from = self.bytes(source_at, rounded)?;
        let mut to = self.bytes(target_at, rounded)?.to_vec();
        to[..table_len].copy_from_slice(&from[..table_len]);
        let mut header = source_header.to_vec();
        header[24..32].copy_from_slice(&(if primary { last } else { 1 }).to_le_bytes());
        header[32..40].copy_from_slice(&(if primary { 1 } else { last }).to_le_bytes());
        header[72..80].copy_from_slice(&(target_at / self.sector).to_le_bytes());
        header[16..20].fill(0);
        let crc = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        let mut extents = vec![
            Extent {
                kind: Kind::Mbr,
                offset: 0,
                bytes: self.head[..sector].to_vec(),
            },
            Extent {
                kind: if primary {
                    Kind::PrimaryHeader
                } else {
                    Kind::BackupHeader
                },
                offset: if primary {
                    self.sector
                } else {
                    self.size - self.sector
                },
                bytes: source_header.to_vec(),
            },
            Extent {
                kind: if primary {
                    Kind::PrimaryEntries
                } else {
                    Kind::BackupEntries
                },
                offset: source_at,
                bytes: from.to_vec(),
            },
            Extent {
                kind: if primary {
                    Kind::BackupHeader
                } else {
                    Kind::PrimaryHeader
                },
                offset: target_header_at,
                bytes: header,
            },
            Extent {
                kind: if primary {
                    Kind::BackupEntries
                } else {
                    Kind::PrimaryEntries
                },
                offset: target_at,
                bytes: to,
            },
        ];
        extents.sort_by_key(|e| e.offset);
        let archive = Archive {
            version: 1,
            size: self.size,
            sector: self.sector,
            source: identity.into(),
            includes_boot: false,
            extents,
        };
        archive.validate()?;
        if !table_restore::archive_layout(&archive)?.agrees(&current) {
            return Err("修复会改变现有分区布局，已拒绝。".into());
        }
        Ok(Some(Repair {
            archive,
            description: if primary {
                "依据完整主 GPT 修复备份表"
            } else {
                "依据完整备份 GPT 修复主表"
            }
            .into(),
        }))
    }
    fn bytes(&self, at: u64, len: usize) -> Result<&[u8], String> {
        if at.checked_add(len as u64).is_some_and(|end| end <= MIB) {
            return Ok(&self.head[at as usize..at as usize + len]);
        }
        if at >= self.size - MIB
            && at
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.size)
        {
            let start = (at - (self.size - MIB)) as usize;
            return Ok(&self.tail[start..start + len]);
        }
        Err("GPT 表超出首尾检查窗口。".into())
    }
}
#[cfg(test)]
#[path = "table_diagnostics_tests.rs"]
mod tests;

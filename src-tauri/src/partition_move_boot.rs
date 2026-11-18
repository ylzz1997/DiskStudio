//! Verify filesystem identity and derive the only sectors whose contents may
//! change after the byte-for-byte transfer. All other data stays identical.
use super::{read_at, write_verified, Range};
use crate::{fat_recovery, ntfs_allocation};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

pub(crate) fn supports_filesystem(name: &str) -> bool {
    name.eq_ignore_ascii_case("FAT") || Kind::parse(name).is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    Exfat,
}

impl Kind {
    fn parse(name: &str) -> Result<Self, String> {
        match name.to_ascii_uppercase().as_str() {
            "NTFS" => Ok(Self::Ntfs),
            "FAT12" => Ok(Self::Fat12),
            "FAT16" => Ok(Self::Fat16),
            "FAT32" => Ok(Self::Fat32),
            "EXFAT" => Ok(Self::Exfat),
            _ => Err("移动要求可核对的 NTFS、FAT12/16/32 或 exFAT 卷。".into()),
        }
    }

    fn geometry(self, first: &[u8], range: Range) -> Result<(usize, Option<u64>), String> {
        let sector = range.sector();
        if first.len() < sector as usize || first[510..512] != [0x55, 0xaa] {
            return Err("待移动卷的引导签名无效。".into());
        }
        let word = |at| u16::from_le_bytes(first[at..at + 2].try_into().unwrap()) as u64;
        let long = |at| u32::from_le_bytes(first[at..at + 4].try_into().unwrap()) as u64;
        let quad = |at| u64::from_le_bytes(first[at..at + 8].try_into().unwrap());
        let (count, backup) = match self {
            Self::Exfat => {
                if &first[3..11] != b"EXFAT   "
                    || 1u64.checked_shl(u32::from(first[108])) != Some(sector)
                    || word(106) != 0
                    || first[110] != 1
                    || quad(72).checked_mul(sector) != Some(range.length())
                {
                    return Err("exFAT 引导、容量或卷状态无法核对。".into());
                }
                (12, Some(12 * sector))
            }
            Self::Ntfs => {
                let backup = quad(40)
                    .checked_mul(sector)
                    .filter(|&n| n >= sector)
                    .ok_or("NTFS 备份引导位置无效")?;
                if &first[3..11] != b"NTFS    " || word(11) != sector {
                    return Err("NTFS 引导或扇区大小无法核对。".into());
                }
                (1, Some(backup))
            }
            _ => {
                let fat32 = self == Self::Fat32;
                let sectors = if word(19) == 0 { long(32) } else { word(19) };
                if word(11) != sector
                    || (word(22) == 0) != fat32
                    || sectors == 0
                    || sectors > range.length() / sector
                {
                    return Err("FAT 引导、类型或容量无法核对。".into());
                }
                let backup = if fat32 {
                    let offset = word(50);
                    if offset == 0 || offset >= word(14) {
                        return Err("FAT32 备份引导不在保留区内。".into());
                    }
                    Some(offset * sector)
                } else {
                    None
                };
                (1, backup)
            }
        };
        let length = count * sector;
        if length > range.length()
            || backup.is_some_and(|n| {
                n < length || n.checked_add(length).is_none_or(|end| end > range.length())
            })
        {
            return Err("卷引导范围重叠或越界。".into());
        }
        Ok((length as usize, backup))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct BootRecord {
    kind: Kind,
    primary: Vec<u8>,
    backup: Option<Vec<u8>>,
}

impl BootRecord {
    pub(super) fn read(
        volume: &mut (impl Read + Seek),
        range: Range,
        reported: &str,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        if volume.seek(SeekFrom::End(0)).map_err(super::error)? != range.length() {
            return Err("文件系统读取范围与所选分区不同。".into());
        }
        let name = reported.to_ascii_uppercase();
        let kind = if name == "NTFS" {
            ntfs_allocation::inspect(volume, cancel)?;
            Kind::Ntfs
        } else {
            if !supports_filesystem(&name) {
                return Err("所选文件系统暂不支持移动。".into());
            }
            let actual =
                Kind::parse(&fat_recovery::allocation::inspect(volume, cancel)?.filesystem)?;
            if !(name == "FAT" && matches!(actual, Kind::Fat12 | Kind::Fat16))
                && Kind::parse(&name)? != actual
            {
                return Err("Windows 报告的文件系统与原始引导结构不同。".into());
            }
            actual
        };
        let first = read_at(volume, 0, range.sector() as usize, range.length())?;
        let (bytes, backup) = kind.geometry(&first, range)?;
        let primary = read_at(volume, 0, bytes, range.length())?;
        if !primary.starts_with(&first) {
            return Err("读取期间卷引导信息已变化。".into());
        }
        let backup = backup
            .map(|at| read_at(volume, at, bytes, range.length()))
            .transpose()?;
        Ok(Self {
            kind,
            primary,
            backup,
        })
    }

    pub(super) fn plan(&self, range: Range) -> Result<BootPlan, String> {
        let (bytes, backup) = self.kind.geometry(&self.primary, range)?;
        if self.primary.len() != bytes
            || self.backup.as_ref().map(Vec::len) != backup.map(|_| bytes)
        {
            return Err("移动记录中的文件系统引导范围不匹配。".into());
        }
        let sector = range.sector() as usize;
        if let Some(copy) = &self.backup {
            let agrees = if self.kind == Kind::Exfat {
                self.primary[..11 * sector]
                    .iter()
                    .zip(&copy[..11 * sector])
                    .enumerate()
                    .all(|(i, (a, b))| [106, 107, 112].contains(&i) || a == b)
            } else {
                copy == &self.primary
            };
            if !agrees {
                return Err("待移动卷的主备引导数据不一致。".into());
            }
        }
        let mut changes = Vec::new();
        for (offset, before) in backup
            .zip(self.backup.as_deref())
            .into_iter()
            .chain([(0, self.primary.as_slice())])
        {
            let mut after = before.to_vec();
            let old_lba = range.source() / range.sector();
            let new_lba = range.destination() / range.sector();
            if self.kind == Kind::Exfat {
                let checksum =
                    fat_recovery::exfat_boot_checksum(&before[..11 * sector]).to_le_bytes();
                if before[11 * sector..]
                    .chunks_exact(4)
                    .any(|word| word != checksum)
                {
                    return Err("exFAT 引导区校验和不匹配。".into());
                }
                let field = u64::from_le_bytes(before[64..72].try_into().unwrap());
                if field != 0 {
                    if field != old_lba {
                        return Err("exFAT 引导区记录的分区起点与 GPT 不同。".into());
                    }
                    after[64..72].copy_from_slice(&new_lba.to_le_bytes());
                    let checksum =
                        fat_recovery::exfat_boot_checksum(&after[..11 * sector]).to_le_bytes();
                    for word in after[11 * sector..].chunks_exact_mut(4) {
                        word.copy_from_slice(&checksum);
                    }
                }
            } else {
                let field = u32::from_le_bytes(before[28..32].try_into().unwrap());
                if field != 0 {
                    if u64::from(field) != old_lba {
                        return Err("引导区记录的隐藏扇区数量与 GPT 起点不同。".into());
                    }
                    let new = u32::try_from(new_lba).map_err(|_| "新起点超出卷引导地址范围。")?;
                    after[28..32].copy_from_slice(&new.to_le_bytes());
                }
            }
            for (index, (from, to)) in before
                .chunks_exact(sector)
                .zip(after.chunks_exact(sector))
                .enumerate()
            {
                if from != to {
                    changes.push(Change {
                        offset: offset + (index * sector) as u64,
                        before: from.to_vec(),
                        after: to.to_vec(),
                    });
                }
            }
        }
        Ok(BootPlan { range, changes })
    }
}

struct Change {
    offset: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}

pub(super) struct BootPlan {
    range: Range,
    changes: Vec<Change>,
}

impl BootPlan {
    pub(super) fn verify_source(&self, disk: &mut (impl Read + Seek)) -> Result<(), String> {
        for change in &self.changes {
            let bytes = read_at(
                disk,
                self.range.source() + change.offset,
                change.before.len(),
                self.range.capacity(),
            )?;
            if bytes != change.before {
                return Err("源卷的引导扇区与移动计划不同。".into());
            }
        }
        Ok(())
    }

    pub(super) fn normalize(&self, offset: u64, data: &mut [u8]) -> Result<(), String> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or("文件系统校验范围溢出")?;
        for change in &self.changes {
            if change.offset >= end || offset >= change.offset + change.before.len() as u64 {
                continue;
            }
            let start = change
                .offset
                .checked_sub(offset)
                .ok_or("文件系统校验块未对齐")? as usize;
            let actual = data
                .get_mut(start..start + change.before.len())
                .ok_or("文件系统校验块不完整")?;
            if &*actual == change.after.as_slice() {
                actual.copy_from_slice(&change.before);
            } else if &*actual != change.before.as_slice() {
                return Err("目标引导扇区既不匹配原始也不匹配移动后的内容。".into());
            }
        }
        Ok(())
    }

    fn completed(&self, disk: &mut (impl Read + Seek)) -> Result<usize, String> {
        let mut complete = 0;
        for (i, change) in self.changes.iter().enumerate() {
            let data = read_at(
                disk,
                self.range.destination() + change.offset,
                change.before.len(),
                self.range.capacity(),
            )?;
            if data == change.after && complete == i {
                complete += 1;
            } else if data != change.before {
                return Err("目标引导扇区与记录的内容或写入顺序不一致。".into());
            }
        }
        Ok(complete)
    }

    pub(super) fn verified(&self, disk: &mut (impl Read + Seek)) -> Result<bool, String> {
        Ok(self.completed(disk)? == self.changes.len())
    }

    pub(super) fn apply<D: Read + Write + Seek>(
        &self,
        disk: &mut D,
        mut sync: impl FnMut(&mut D) -> io::Result<()>,
    ) -> Result<(), String> {
        let start = self.completed(disk)?;
        for change in self.changes.iter().skip(start) {
            write_verified(
                disk,
                self.range.destination() + change.offset,
                &change.after,
                self.range.capacity(),
                &mut sync,
            )?;
        }
        if !self.verified(disk)? {
            return Err("目标文件系统引导变更尚未完整核对。".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn offsets_and_exfat_checksums_are_recalculated_in_both_boot_regions() {
        for sector in [512, 4096] {
            let range =
                Range::new(32 * sector, 36 * sector, 128 * sector, 256 * sector, sector).unwrap();
            let sector = sector as usize;
            let mut before = vec![0; 12 * sector];
            before[3..11].copy_from_slice(b"EXFAT   ");
            before[64..72].copy_from_slice(&32u64.to_le_bytes());
            before[72..80].copy_from_slice(&128u64.to_le_bytes());
            before[108] = sector.trailing_zeros() as u8;
            before[110] = 1;
            before[510..512].copy_from_slice(&[0x55, 0xaa]);
            let old = fat_recovery::exfat_boot_checksum(&before[..11 * sector]).to_le_bytes();
            for word in before[11 * sector..].chunks_exact_mut(4) {
                word.copy_from_slice(&old);
            }
            let record = BootRecord {
                kind: Kind::Exfat,
                primary: before.clone(),
                backup: Some(before.clone()),
            };
            let plan = record.plan(range).unwrap();
            let mut disk = Cursor::new(vec![0; range.capacity() as usize]);
            let base = range.destination() as usize;
            for offset in [base, base + 12 * sector] {
                disk.get_mut()[offset..offset + 12 * sector].copy_from_slice(&before);
            }
            plan.apply(&mut disk, |_| Ok(())).unwrap();
            for offset in [base, base + 12 * sector] {
                let region = &disk.get_ref()[offset..offset + 12 * sector];
                assert_eq!(u64::from_le_bytes(region[64..72].try_into().unwrap()), 36);
                let new = fat_recovery::exfat_boot_checksum(&region[..11 * sector]).to_le_bytes();
                assert_ne!(new, old);
                assert!(region[11 * sector..]
                    .chunks_exact(4)
                    .all(|word| word == new));
            }
            let mut check = disk.get_ref()[base..base + 24 * sector].to_vec();
            plan.normalize(0, &mut check).unwrap();
            assert_eq!(&check[..12 * sector], before);
            assert_eq!(&check[12 * sector..], before);
        }
    }

    #[test]
    fn inconsistent_ntfs_boot_geometry_or_address_is_rejected() {
        let range = Range::new(32 * 512, 40 * 512, 128 * 512, 256 * 512, 512).unwrap();
        let mut data = vec![0; 512];
        data[3..11].copy_from_slice(b"NTFS    ");
        data[11..13].copy_from_slice(&512u16.to_le_bytes());
        data[28..32].copy_from_slice(&32u32.to_le_bytes());
        data[40..48].copy_from_slice(&127u64.to_le_bytes());
        data[510..512].copy_from_slice(&[0x55, 0xaa]);
        let mut record = BootRecord {
            kind: Kind::Ntfs,
            primary: data.clone(),
            backup: Some(data.clone()),
        };
        assert_eq!(record.plan(range).unwrap().changes.len(), 2);
        record.backup.as_mut().unwrap()[48] ^= 1;
        assert!(record.plan(range).is_err());
        data[28] = 30;
        record.primary = data.clone();
        record.backup = Some(data.clone());
        assert!(record.plan(range).is_err());
        data[28] = 0;
        record.primary = data.clone();
        record.backup = Some(data);
        assert!(record.plan(range).unwrap().changes.is_empty());
    }
}

//! Derive the in-place GPT change from validated LDM extents.
use crate::{
    partitions::policy::BASIC_DATA,
    table_archive::{self, Archive, Kind},
    table_restore::{self, Entry, Layout},
};
use disk_core::ldm::SimpleExtent;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use uuid::Uuid;

pub(crate) const LDM_DATA: &str = "af9b60a0-1431-4f62-bc68-3311714a69ad";
pub(crate) const LDM_METADATA: &str = "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3";
pub(crate) const MICROSOFT_RESERVED: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
const ORDER: [Kind; 4] = [
    Kind::BackupEntries,
    Kind::BackupHeader,
    Kind::PrimaryEntries,
    Kind::PrimaryHeader,
];

pub(crate) struct Plan {
    before: Archive,
    after: Archive,
    pub(crate) layout: Layout,
}

impl Plan {
    pub(crate) fn new(before: Archive, volumes: &[SimpleExtent]) -> Result<Self, String> {
        let layout = table_restore::archive_layout(&before)?;
        let metadata = layout
            .entries
            .iter()
            .find(|entry| entry.kind == LDM_METADATA);
        let data = layout.entries.iter().find(|entry| entry.kind == LDM_DATA);
        let (Some(metadata), Some(data)) = (metadata, data) else {
            return Err("转换要求同时存在 LDM 数据和元数据的 GPT。".into());
        };
        let reserved = layout
            .entries
            .iter()
            .find(|entry| entry.kind == MICROSOFT_RESERVED);
        if before.includes_boot
            || layout.entries.len() != 2 + usize::from(reserved.is_some())
            || data.attributes != 0
            || metadata.attributes & !1 != 0
            || !(1..=layout.count as usize - usize::from(reserved.is_some()))
                .contains(&volumes.len())
        {
            return Err("磁盘包含受保护或不支持的 GPT 条目，或简单卷数量超出范围。".into());
        }
        let mut next = data.offset;
        let end = data
            .offset
            .checked_add(data.size)
            .ok_or("LDM 数据范围溢出。")?;
        let mut slots =
            (1..=layout.count).filter(|slot| reserved.is_none_or(|entry| entry.slot != *slot));
        let mut entries = Vec::with_capacity(volumes.len() + usize::from(reserved.is_some()));
        for volume in volumes {
            if volume.offset < next
                || volume.offset % before.sector != 0
                || volume.size == 0
                || volume.size % before.sector != 0
            {
                return Err("简单卷存在重叠、越界或未按逻辑扇区对齐。".into());
            }
            next = volume
                .offset
                .checked_add(volume.size)
                .filter(|last| *last <= end)
                .ok_or("简单卷超出 LDM 数据分区。")?;
            let mut name = Vec::new();
            for character in volume.name.chars().filter(|c| !c.is_control()) {
                if name.len() + character.len_utf16() > 36 {
                    break;
                }
                name.extend_from_slice(character.encode_utf16(&mut [0; 2]));
            }
            entries.push(Entry {
                slot: slots.next().ok_or("GPT 空闲槽位不足。")?,
                guid: Uuid::new_v4().to_string(),
                kind: BASIC_DATA.into(),
                offset: volume.offset,
                size: volume.size,
                attributes: 0,
                name,
            });
        }
        entries.extend(reserved.cloned());
        entries.sort_unstable_by_key(|entry| entry.slot);
        let replacement = table_restore::new_gpt(
            before.size,
            before.sector,
            before.source.clone(),
            Uuid::parse_str(&layout.guid).map_err(|error| error.to_string())?,
            &entries,
        )?;
        let table = &replacement
            .extents
            .iter()
            .find(|extent| extent.kind == Kind::PrimaryEntries)
            .ok_or("生成的 GPT 缺少条目表。")?
            .bytes;
        let after = table_restore::rewrite_gpt_entries(&before, &layout, |output| {
            output.copy_from_slice(&table[..output.len()]);
        })?;
        let expected = table_restore::archive_layout(&after)?;
        for entry in &mut entries {
            entry.name.resize(36, 0);
        }
        if expected != (Layout { entries, ..layout }) {
            return Err("基本磁盘表的独立解码结果与计划不一致。".into());
        }
        Ok(Self {
            before,
            after,
            layout: expected,
        })
    }

    pub(crate) fn matches(&self, current: &Archive) -> bool {
        &self.before == current
    }

    pub(crate) fn write<R: Read + Write + Seek>(
        &self,
        disk: &mut R,
        mut sync: impl FnMut(&mut R) -> Result<(), String>,
        mut checkpoint: impl FnMut(&R) -> Result<(), String>,
    ) -> Result<(), String> {
        checkpoint(disk)?;
        self.before.matches(disk, || Ok(()))?;
        // Every write is a complete, previously validated GPT range, in
        // backup-first order. No former LDM metadata or volume payload is written.
        for kind in ORDER {
            checkpoint(disk)?;
            let range = self
                .after
                .extents
                .iter()
                .find(|extent| extent.kind == kind)
                .ok_or("转换计划缺少 GPT 片段。")?;
            disk.seek(SeekFrom::Start(range.offset))
                .map_err(|error| error.to_string())?;
            let count = disk
                .write(&range.bytes)
                .map_err(|error| error.to_string())?;
            if count != range.bytes.len() {
                return Err(format!("GPT 片段短写 {count}/{} 字节。", range.bytes.len()));
            }
            sync(disk)?;
            let actual = table_archive::read_at(disk, range.offset, count, self.after.size)?;
            if Sha256::digest(&actual) != Sha256::digest(&range.bytes) {
                return Err("GPT 片段 SHA-256 回读不同。".into());
            }
        }
        checkpoint(disk)?;
        self.verify(disk, || Ok(()))
    }

    pub(crate) fn verify(
        &self,
        reader: &mut (impl Read + Seek),
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.after.matches(reader, checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::MIB;
    use std::io::Cursor;

    fn fixture(sector: u64) -> (Archive, Vec<SimpleExtent>, Cursor<Vec<u8>>) {
        let bytes = 12 * MIB;
        let entry = |slot, kind: &str, offset, size| Entry {
            slot,
            guid: Uuid::new_v4().to_string(),
            kind: kind.into(),
            offset,
            size,
            attributes: 0,
            name: vec![],
        };
        let archive = table_restore::new_gpt(
            bytes,
            sector,
            "a".repeat(64),
            Uuid::new_v4(),
            &[
                entry(1, LDM_METADATA, MIB, MIB),
                entry(2, LDM_DATA, 3 * MIB, 7 * MIB),
                entry(3, MICROSOFT_RESERVED, 2 * MIB, MIB),
            ],
        )
        .unwrap();
        let volumes = [
            SimpleExtent {
                volume: "volume1".into(),
                name: "One".into(),
                offset: 3 * MIB,
                size: MIB,
            },
            SimpleExtent {
                volume: "volume2".into(),
                name: "Two".into(),
                offset: 7 * MIB,
                size: 2 * MIB,
            },
        ];
        let mut disk = vec![0u8; bytes as usize];
        disk[MIB as usize..(10 * MIB) as usize].fill(0xa5);
        for extent in &archive.extents {
            let at = extent.offset as usize;
            disk[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
        }
        (archive, volumes.to_vec(), Cursor::new(disk))
    }

    #[test]
    fn in_place_conversion_preserves_bytes_outside_confirmed_gpt_copies() {
        for sector in [512, 4096] {
            let (archive, volumes, mut disk) = fixture(sector);
            let old = disk.get_ref().clone();
            let plan = Plan::new(archive.clone(), &volumes).unwrap();
            let original = table_restore::archive_layout(&archive).unwrap();
            assert_eq!(original.guid, plan.layout.guid);
            assert_eq!(original.entries[2], plan.layout.entries[2]);
            for (volume, partition) in volumes.iter().zip(&plan.layout.entries) {
                assert_eq!(
                    (volume.offset, volume.size),
                    (partition.offset, partition.size)
                );
                assert_eq!(partition.kind, BASIC_DATA);
            }
            let mut syncs = 0;
            plan.write(
                &mut disk,
                |_| {
                    syncs += 1;
                    Ok(())
                },
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(syncs, 4);
            assert!(plan.verify(&mut disk, || Ok(())).is_ok());
            let mut outside = disk.into_inner();
            for extent in archive.extents.iter().filter(|e| ORDER.contains(&e.kind)) {
                let at = extent.offset as usize;
                outside[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
            }
            assert_eq!(outside, old);
        }
    }

    #[test]
    fn protected_layout_overlap_and_stale_table_are_refused_before_writes() {
        let (archive, mut volumes, mut disk) = fixture(4096);
        let plan = Plan::new(archive.clone(), &volumes).unwrap();
        let layout = table_restore::archive_layout(&archive).unwrap();
        let protected = table_restore::edit_gpt_entry(&archive, &layout, 2, |entry| {
            entry[48..56].copy_from_slice(&1u64.to_le_bytes());
        })
        .unwrap();
        assert!(Plan::new(protected, &volumes).is_err());
        volumes[1].offset = volumes[0].offset;
        assert!(Plan::new(archive, &volumes).is_err());
        disk.get_mut()[440] ^= 1;
        assert!(plan
            .write(
                &mut disk,
                |_| panic!("stale table reached write"),
                |_| Ok(())
            )
            .is_err());
    }
}

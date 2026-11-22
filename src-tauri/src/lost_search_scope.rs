//! Derive search extents from complete, checked partition-table evidence.
use crate::{
    surface_scan::{Options, Range},
    table_archive::{Archive, Kind},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Scope {
    Range(Options),
    All(Unallocated),
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unallocated {
    scope: Choice,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum Choice {
    AllUnallocated,
}

impl Scope {
    pub fn explicit(self, size: u64, sector: u64) -> Result<Option<Range>, String> {
        match self {
            Self::Range(options) => options.range(0, size, sector).map(Some),
            Self::All(_) => Ok(None),
        }
    }
}

pub(crate) fn containing(ranges: &[Range], offset: u64, size: u64) -> Result<Range, String> {
    ranges
        .iter()
        .copied()
        .find(|range| {
            offset >= range.offset
                && offset
                    .checked_add(size)
                    .is_some_and(|end| end <= range.offset + range.size)
        })
        .ok_or_else(|| "候选已超出保留的搜索范围。".into())
}

pub(crate) fn unallocated(
    archive: &Archive,
    windows_layout: Option<(&str, &[crate::used_space::Span])>,
) -> Result<Vec<Range>, String> {
    let info = archive.validate()?;
    let mut occupied = info
        .layout
        .iter()
        .map(|part| (part.offset, part.offset + part.size))
        .collect::<Vec<_>>();
    occupied.sort_unstable();
    if let Some((scheme, spans)) = windows_layout {
        let mut expected = spans
            .iter()
            .map(|span| (span.start, span.end))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        if info.scheme != scheme || occupied != expected {
            return Err("原始分区表与 Windows 分区范围不一致。".into());
        }
    }
    let (first, end) = match info.scheme.as_str() {
        "GPT" => {
            let header = &archive
                .extents
                .iter()
                .find(|extent| extent.kind == Kind::PrimaryHeader)
                .ok_or("缺少 GPT 主表头。")?
                .bytes;
            // Archive validation checks both header CRCs, their usable bounds
            // and that all declared partitions are within those same bounds.
            let first = u64::from_le_bytes(header[40..48].try_into().unwrap());
            let last = u64::from_le_bytes(header[48..56].try_into().unwrap());
            (first * info.sector, (last + 1) * info.sector)
        }
        "MBR" => {
            occupied.extend(
                archive
                    .extents
                    .iter()
                    .filter(|e| e.kind == Kind::Ebr)
                    .map(|e| (e.offset, e.offset + e.bytes.len() as u64)),
            );
            occupied.sort_unstable();
            (info.sector, info.size)
        }
        _ => return Err("搜索全部未分配空间需要完整的 GPT 或 MBR 分区表。".into()),
    };
    let mut ranges = Vec::new();
    let mut position = first;
    for (start, stop) in occupied.into_iter().chain(std::iter::once((end, end))) {
        if start < position || stop < start || stop > end {
            return Err("分区或分区表范围重叠或越界。".into());
        }
        if start > position {
            ranges.push(Range {
                offset: position,
                size: start - position,
                sector: info.sector,
            });
        }
        position = stop;
    }
    if ranges.is_empty() {
        return Err("没有可搜索的未分配空间。".into());
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lost_partitions, table_restore};
    use disk_core::{
        fat_format::{FatType, Plan},
        MIB,
    };
    use std::{
        io::{self, Cursor, Read, Seek, SeekFrom},
        sync::{atomic::AtomicBool, Arc},
    };

    struct GapReader {
        bytes: Cursor<Vec<u8>>,
        ranges: Vec<Range>,
    }
    impl Read for GapReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let at = self.bytes.position();
            if containing(&self.ranges, at, bytes.len() as u64).is_err() {
                return Err(io::Error::other("read outside the search gaps"));
            }
            self.bytes.read(bytes)
        }
    }
    impl Seek for GapReader {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(from)
        }
    }

    #[test]
    fn gpt_skips_allocated_ranges_and_metadata_and_binds_each_candidate() {
        for sector in [512, 4096] {
            let size = 16 * MIB;
            let partition = table_restore::Entry {
                slot: 1,
                guid: uuid::Uuid::new_v4().to_string(),
                kind: crate::partitions::policy::BASIC_DATA.into(),
                offset: 4 * MIB,
                size: 4 * MIB,
                attributes: 0,
                name: Vec::new(),
            };
            let archive = table_restore::new_gpt(
                size,
                sector,
                "a".repeat(64),
                uuid::Uuid::new_v4(),
                &[partition],
            )
            .unwrap();
            let native = [crate::used_space::Span {
                start: 4 * MIB,
                end: 8 * MIB,
            }];
            let ranges = unallocated(&archive, Some(("GPT", &native))).unwrap();
            assert!(unallocated(&archive, Some(("GPT", &[]))).is_err());
            assert!(unallocated(&archive, Some(("MBR", &native))).is_err());
            assert_eq!(ranges.len(), 2);
            assert!(ranges[0].offset > sector);
            assert_eq!(ranges[0].offset + ranges[0].size, 4 * MIB);
            assert_eq!(ranges[1].offset, 8 * MIB);
            assert!(ranges[1].offset + ranges[1].size < size - sector);
            assert!(containing(&ranges, 3 * MIB, 6 * MIB).is_err());

            let mut volume = Cursor::new(vec![0; (2 * MIB) as usize]);
            Plan::new(FatType::Fat12, 2 * MIB, sector as u32, 0, "LOST", 123, 0)
                .unwrap()
                .format(&mut volume, |_, _| Ok(()))
                .unwrap();
            let mut image = vec![0u8; size as usize];
            for base in [MIB, 4 * MIB, 9 * MIB] {
                image[base as usize..(base + 2 * MIB) as usize].copy_from_slice(volume.get_ref());
            }
            let reader = GapReader {
                bytes: Cursor::new(image),
                ranges: ranges.clone(),
            };
            let mut candidates = Vec::new();
            let mut checked = 0;
            lost_partitions::scan_ranges(
                reader,
                &ranges,
                &Arc::new(AtomicBool::new(false)),
                |done, candidate| {
                    assert!(done >= checked);
                    checked = done;
                    candidates.extend(candidate);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(checked, ranges.iter().map(|r| r.size).sum::<u64>());
            assert_eq!(
                candidates.iter().map(|c| c.offset).collect::<Vec<_>>(),
                [MIB, 9 * MIB]
            );
            for (candidate, range) in candidates.iter().zip(ranges) {
                assert_eq!(
                    containing(&[range], candidate.offset, candidate.size).unwrap(),
                    range
                );
            }
        }
    }

    #[test]
    fn extended_mbr_reserves_the_ebr_and_uses_its_remaining_gaps() {
        let sector = 4096u64;
        let size = 8 * MIB;
        let extended_lba = 256u32;
        let ebr = (u64::from(extended_lba) * sector) as usize;
        let mut disk = vec![0u8; size as usize];
        fn entry(bytes: &mut [u8], kind: u8, start: u32, count: u32) {
            bytes[446 + 4] = kind;
            bytes[446 + 8..446 + 12].copy_from_slice(&start.to_le_bytes());
            bytes[446 + 12..446 + 16].copy_from_slice(&count.to_le_bytes());
            bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        }
        entry(&mut disk[..sector as usize], 0x0f, extended_lba, 1024);
        entry(&mut disk[ebr..ebr + sector as usize], 0x07, 16, 256);
        let table = crate::table_archive::capture(
            &mut Cursor::new(disk),
            size,
            sector,
            "b".repeat(64),
            false,
            || Ok(()),
        )
        .unwrap();
        let ranges = unallocated(&table, None).unwrap();
        assert!(containing(&ranges, 0, sector).is_err());
        assert!(containing(&ranges, ebr as u64, sector).is_err());
        assert!(containing(&ranges, ebr as u64 + 16 * sector, sector).is_err());
        assert!(containing(&ranges, ebr as u64 + sector, 15 * sector).is_ok());
    }
}

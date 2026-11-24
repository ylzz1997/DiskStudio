//! Add independently verified lost filesystems to a new whole-image copy.
use super::{
    empty_mbr,
    table_archive::{self, Archive, Extent, Kind},
    table_restore, u32le, Conversion, PartitionChange, Scheme, Uuid, MIB,
};
use crate::{
    lost_partition_restore::{self, Evidence},
    lost_partitions::Candidate,
    lost_search_scope,
    surface_scan::Range,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek},
    sync::atomic::{AtomicBool, Ordering},
};

#[path = "image_mbr_registration.rs"]
mod mbr;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Form {
    Auto,
    Primary,
    Logical,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Options {
    pub form: Form,
    pub new_table: Scheme,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub rebuilt_table: bool,
}

struct StartingTable {
    archive: Archive,
    scheme: Scheme,
    retained: Vec<(u64, Vec<u8>)>,
    rebuilt: bool,
}

fn checked_source(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    source_key: String,
    allow_rebuild: bool,
    new_table: Scheme,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<StartingTable, String> {
    let result =
        table_archive::capture(reader, size, sector, source_key.clone(), false, &mut *tick);
    let error = match result {
        Ok(archive) => {
            let info = archive.validate()?;
            let scheme = if info.scheme == "GPT" {
                Scheme::Gpt
            } else {
                Scheme::Mbr
            };
            let mut retained: Vec<_> = archive
                .extents
                .iter()
                .map(|e| (e.offset, e.bytes.clone()))
                .collect();
            if scheme == Scheme::Mbr {
                let tail = table_archive::read_at(reader, size - 4096, 4096, size)?;
                if &tail[..8] == b"EFI PART" || &tail[3584..3592] == b"EFI PART" {
                    return Err("镜像仍含 GPT 备份签名，无法安全登记 MBR 分区。".into());
                }
                retained.push((size - 4096, tail));
            }
            return Ok(StartingTable {
                archive,
                scheme,
                retained,
                rebuilt: false,
            });
        }
        Err(error) => error,
    };
    if !allow_rebuild
        || !(4 * MIB..=1 << 43).contains(&size)
        || !matches!(sector, 512 | 4096)
        || size % sector != 0
    {
        return Err(error);
    }
    tick()?;
    let mut head = table_archive::read_at(reader, 0, MIB as usize, size)?;
    tick()?;
    let tail = table_archive::read_at(reader, size - MIB, MIB as usize, size)?;
    if [512, 4096].into_iter().any(|block| {
        &head[block..block + 8] == b"EFI PART"
            || &tail[MIB as usize - block..MIB as usize - block + 8] == b"EFI PART"
    }) || head[446..510]
        .chunks_exact(16)
        .any(|entry| entry[4] == 0xee)
    {
        return Err("镜像仍有 GPT 证据，请先检查并修复 GPT 或恢复备份。".into());
    }
    let mut retained = vec![(0, head.clone()), (size - MIB, tail)];
    let blank = head[..sector as usize].iter().all(|&b| b == 0);
    let scheme = if blank { new_table } else { Scheme::Mbr };
    let archive = if scheme == Scheme::Gpt {
        let archive = table_restore::new_gpt(size, sector, source_key, Uuid::new_v4(), &[])?;
        for extent in &archive.extents {
            tick()?;
            let read = table_archive::read_at(reader, extent.offset, extent.bytes.len(), size)?;
            if read.iter().any(|&b| b != 0) {
                return Err("新 GPT 所需的保留扇区含有数据，已拒绝覆盖。".into());
            }
            retained.push((extent.offset, read));
        }
        archive
    } else {
        if !blank && (head[510..512] == [0x55, 0xaa] || head[446..510].iter().all(|&b| b == 0)) {
            return Err("现有 MBR 或扩展链损坏，无法安全保留现有条目。".into());
        }
        let mut bytes = if blank {
            empty_mbr(sector)
        } else {
            head[..sector as usize].to_vec()
        };
        bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        head[..sector as usize].copy_from_slice(&bytes);
        let mut extents = vec![Extent {
            kind: Kind::Mbr,
            offset: 0,
            bytes,
        }];
        for ebr in table_archive::read_ebrs(reader, &head, size, sector, &mut *tick)? {
            retained.push((ebr.offset, ebr.bytes.clone()));
            extents.push(Extent {
                kind: Kind::Ebr,
                offset: ebr.offset,
                bytes: ebr.bytes,
            });
        }
        extents.sort_unstable_by_key(|extent| extent.offset);
        let archive = Archive {
            version: 1,
            size,
            sector,
            source: source_key,
            includes_boot: false,
            extents,
        };
        archive.validate()?;
        archive
    };
    Ok(StartingTable {
        archive,
        scheme,
        retained,
        rebuilt: true,
    })
}

pub(crate) fn prepare(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    source_key: String,
    allow_rebuild: bool,
    options: Options,
    selected: Vec<(Range, Candidate)>,
    cancel: &AtomicBool,
) -> Result<(Conversion, Vec<Evidence>), String> {
    if selected.is_empty() || selected.len() > 128 {
        return Err("请选择 1–128 个候选，仅恢复分区记录。".into());
    }
    let mut tick = || {
        if cancel.load(Ordering::Relaxed) {
            Err("已取消候选边界核对。".into())
        } else {
            Ok(())
        }
    };
    let StartingTable {
        archive,
        scheme,
        mut retained,
        rebuilt,
    } = checked_source(
        reader,
        size,
        sector,
        source_key,
        allow_rebuild,
        options.new_table,
        &mut tick,
    )?;
    let gaps = lost_search_scope::unallocated(&archive, None)?;
    let mut evidence = Vec::with_capacity(selected.len());
    for (range, candidate) in selected {
        if range.sector != sector
            || range
                .offset
                .checked_add(range.size)
                .is_none_or(|end| end > size)
        {
            return Err("候选已超出保留的搜索范围。".into());
        }
        lost_search_scope::containing(&gaps, candidate.offset, candidate.size)?;
        let extent = lost_partition_restore::extent(reader, range, &candidate, cancel)?;
        lost_search_scope::containing(&gaps, extent.offset, extent.size)?;
        evidence.push(Evidence {
            range,
            candidate,
            extent,
        });
    }
    evidence.sort_unstable_by_key(|e| e.extent.offset);
    if evidence
        .windows(2)
        .any(|e| e[0].extent.offset + e[0].extent.size > e[1].extent.offset)
    {
        return Err("选中的候选分区互相重叠。".into());
    }
    let (output, disk_id, partitions) = match scheme {
        Scheme::Gpt => {
            if options.form != Form::Auto {
                return Err("主分区和逻辑分区选择只适用于 MBR。".into());
            }
            let entries: Vec<_> = evidence.iter().map(|e| e.extent.clone()).collect();
            let output = lost_partition_restore::add_entries(&archive, &entries)?;
            let id = table_restore::archive_layout(&output)?.guid;
            let partitions = evidence
                .iter()
                .map(|e| PartitionChange {
                    offset: e.extent.offset,
                    size: e.extent.size,
                    old_type: e.candidate.filesystem.clone(),
                    new_type: e.extent.kind.clone(),
                    logical: false,
                })
                .collect();
            (output, id, partitions)
        }
        Scheme::Mbr => {
            let (output, partitions) = mbr::add(
                reader,
                archive,
                &evidence,
                options.form,
                &mut retained,
                &mut tick,
            )?;
            let id = u32le(&output.extents[0].bytes[440..444]);
            (output, format!("{id:08X}"), partitions)
        }
    };
    output.validate()?;
    let plan = Conversion::replace_table(
        reader,
        retained,
        output,
        super::Preview {
            from: scheme,
            to: scheme,
            sector,
            disk_id,
            partitions,
            table_bytes: 0,
            placement: None,
            restored_from: None,
            gpt_repair: None,
            registration: Some(Preview {
                rebuilt_table: rebuilt,
            }),
        },
    )?;
    lost_partition_restore::verify(reader, &evidence, cancel)?;
    Ok((plan, evidence))
}

#[cfg(test)]
#[path = "image_partition_registration_tests.rs"]
mod tests;

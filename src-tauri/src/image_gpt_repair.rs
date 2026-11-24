//! Repair planning from bounded, independently checked GPT metadata.
use super::{
    table_archive::{self, Archive, Extent, Kind},
    table_restore, u32le, Conversion, PartitionChange, Preview, Scheme, MIB,
};
use disk_core::table_check::{check_gpt_copy, CheckState, CheckedGptCopy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek},
    ops::Range,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Side {
    Primary,
    Backup,
}
impl Side {
    fn opposite(self) -> Self {
        match self {
            Self::Primary => Self::Backup,
            Self::Backup => Self::Primary,
        }
    }
    fn header_at(self, size: u64, sector: u64) -> u64 {
        match self {
            Self::Primary => sector,
            Self::Backup => size - sector,
        }
    }
    fn kinds(self) -> (Kind, Kind) {
        match self {
            Self::Primary => (Kind::PrimaryHeader, Kind::PrimaryEntries),
            Self::Backup => (Kind::BackupHeader, Kind::BackupEntries),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Changes {
    pub authority: Side,
    pub repair_peer: bool,
    pub repair_protective_mbr: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Partition {
    slot: usize,
    guid: String,
    kind: String,
    offset: u64,
    size: u64,
    name: String,
    attributes: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Layout {
    disk_id: String,
    first_lba: u64,
    last_lba: u64,
    partitions: Vec<Partition>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CopyReport {
    side: Side,
    layout: Option<Layout>,
    error: Option<String>,
    changes: Option<Changes>,
    repair_error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    digest: String,
    sector: u64,
    conflict: bool,
    protective_mbr: &'static str,
    copies: [CopyReport; 2],
}
struct Snapshot {
    size: u64,
    sector: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
}

fn u64le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}
fn disk_id(copy: &CheckedGptCopy<'_>) -> String {
    Uuid::from_bytes_le(copy.header[56..72].try_into().unwrap()).to_string()
}
fn layout(copy: &CheckedGptCopy<'_>, sector: u64) -> Layout {
    let partitions = copy
        .array
        .chunks_exact(copy.entry_stride)
        .enumerate()
        .filter(|(_, e)| e[..16].iter().any(|&b| b != 0))
        .map(|(slot, e)| {
            let start = u64le(&e[32..40]);
            let end = u64le(&e[40..48]);
            let units: Vec<u16> = e[56..128]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take_while(|&c| c != 0)
                .collect();
            Partition {
                slot: slot + 1,
                guid: Uuid::from_bytes_le(e[16..32].try_into().unwrap()).to_string(),
                kind: Uuid::from_bytes_le(e[..16].try_into().unwrap()).to_string(),
                offset: start * sector,
                size: (end - start + 1) * sector,
                name: String::from_utf16_lossy(&units),
                attributes: format!("0x{:016X}", u64le(&e[48..56])),
            }
        })
        .collect();
    Layout {
        disk_id: disk_id(copy),
        first_lba: copy.first_usable,
        last_lba: copy.last_usable,
        partitions,
    }
}
fn agrees(a: &CheckedGptCopy<'_>, b: &CheckedGptCopy<'_>) -> bool {
    a.header[40..72] == b.header[40..72]
        && a.header[80..88] == b.header[80..88]
        && a.array == b.array
}

impl Snapshot {
    fn read(reader: &mut (impl Read + Seek), size: u64, sector: u64) -> Result<Self, String> {
        if !(4 * MIB..=1 << 43).contains(&size)
            || !matches!(sector, 512 | 4096)
            || size % sector != 0
        {
            return Err("分区表检查支持 4 MiB–8 TiB、512/4096 字节逻辑扇区。".into());
        }
        let head = table_archive::read_at(reader, 0, MIB as usize, size)?;
        let tail = table_archive::read_at(reader, size - MIB, MIB as usize, size)?;
        let other = if sector == 512 { 4096 } else { 512 };
        if &head[other..other + 8] == b"EFI PART"
            || size % other as u64 == 0
                && &tail[MIB as usize - other..MIB as usize - other + 8] == b"EFI PART"
        {
            return Err("检测到不同逻辑扇区大小的 GPT 签名。请核对镜像的逻辑扇区大小。".into());
        }
        table_archive::match_ranges(
            reader,
            size,
            [(0, head.as_slice()), (size - MIB, tail.as_slice())],
            || Ok(()),
        )?;
        Ok(Self {
            size,
            sector,
            head,
            tail,
        })
    }
    fn copies(&self) -> [Result<CheckedGptCopy<'_>, (CheckState, String)>; 2] {
        [false, true]
            .map(|backup| check_gpt_copy(&self.head, &self.tail, self.size, self.sector, backup))
    }
    fn digest(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.size.to_le_bytes());
        hash.update(self.sector.to_le_bytes());
        hash.update(&self.head);
        hash.update(&self.tail);
        format!("{:x}", hash.finalize())
    }
    fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end <= MIB {
            return self.head.get(at as usize..end as usize);
        }
        let base = self.size - MIB;
        if at >= base && end <= self.size {
            return self.tail.get((at - base) as usize..(end - base) as usize);
        }
        None
    }
    fn protective(&self) -> Result<(Vec<u8>, bool), String> {
        let mut mbr = self.head[..self.sector as usize].to_vec();
        let mut occupied = mbr[446..510]
            .chunks_exact(16)
            .filter(|entry| entry.iter().any(|&b| b != 0));
        let entry = occupied.next();
        if occupied.next().is_some() || entry.is_some_and(|e| e[4] != 0xee) {
            return Err("检测到混合或传统 MBR 条目，无法安全修复此镜像。".into());
        }
        let last = self.size / self.sector - 1;
        let correct = mbr[510..512] == [0x55, 0xaa]
            && entry.is_some_and(|e| {
                e[0] == 0
                    && u32le(&e[8..12]) == 1
                    && [last.min(u32::MAX as u64) as u32, u32::MAX].contains(&u32le(&e[12..16]))
            });
        if !correct {
            table_restore::write_protective_entries(&mut mbr, last);
        }
        Ok((mbr, !correct))
    }
    fn original_extents(
        &self,
        side: Side,
        copy: &CheckedGptCopy<'_>,
    ) -> Result<[Extent; 2], String> {
        let (header, array) = side.kinds();
        let padded = (copy.array.len() as u64).div_ceil(self.sector) * self.sector;
        let bytes = self
            .bytes(copy.array_offset, padded)
            .ok_or("GPT 条目表超出检查窗口。")?;
        Ok([
            Extent {
                kind: header,
                offset: side.header_at(self.size, self.sector),
                bytes: copy.header.to_vec(),
            },
            Extent {
                kind: array,
                offset: copy.array_offset,
                bytes: bytes.to_vec(),
            },
        ])
    }

    fn replacement_array(
        &self,
        target: Side,
        source: &CheckedGptCopy<'_>,
        peer: Option<&CheckedGptCopy<'_>>,
    ) -> Result<(u64, Vec<u8>), String> {
        let sectors = (source.array.len() as u64).div_ceil(self.sector);
        let len = sectors * self.sector;
        let last = self.size / self.sector - 1;
        let reserved = |copy: &CheckedGptCopy<'_>| match target {
            Side::Primary => 2 * self.sector..copy.first_usable * self.sector,
            Side::Backup => (copy.last_usable + 1) * self.sector..last * self.sector,
        };
        let allowed = |r: Range<u64>, start: u64| r.start <= start && start + len <= r.end;
        let conventional = match target {
            Side::Primary => 2 * self.sector,
            Side::Backup => (last - sectors) * self.sector,
        };
        // A valid competing copy makes its layout evidence as well. Preserve
        // both copies' usable data ranges and any unknown reserved bytes.
        let positions = [peer.map(|p| p.array_offset), Some(conventional)];
        for at in positions.into_iter().flatten() {
            if !allowed(reserved(source), at) || peer.is_some_and(|p| !allowed(reserved(p), at)) {
                continue;
            }
            let Some(old) = self.bytes(at, len) else {
                continue;
            };
            if let Some(p) = peer {
                let provisioned = p.array_offset..p.array_offset + p.array.len() as u64;
                let new = old[..source.array.len()]
                    .iter()
                    .zip(source.array)
                    .enumerate();
                if new.into_iter().any(|(i, (&previous, &next))| {
                    previous != 0 && previous != next && !provisioned.contains(&(at + i as u64))
                }) {
                    continue;
                }
            }
            let mut bytes = old.to_vec();
            bytes[..source.array.len()].copy_from_slice(source.array);
            return Ok((at, bytes));
        }
        Err("重建的条目表无法放入已核对的保留区域。".into())
    }

    fn output(
        &self,
        side: Side,
        source: &CheckedGptCopy<'_>,
        peer: &Result<CheckedGptCopy<'_>, (CheckState, String)>,
        digest: &str,
    ) -> Result<(Archive, Changes), String> {
        if matches!(peer, Err((CheckState::Unchecked, _))) {
            return Err("另一份 GPT 的格式或位置超出可核对范围，无法安全修复。".into());
        }
        let peer = peer.as_ref().ok();
        let (mbr, repair_protective_mbr) = self.protective()?;
        let repair_peer = peer.is_none_or(|p| !agrees(source, p));
        if !repair_peer && !repair_protective_mbr {
            return Err("GPT 主表、备份表和保护性 MBR 均已通过检查。".into());
        }
        let mut extents = vec![Extent {
            kind: Kind::Mbr,
            offset: 0,
            bytes: mbr,
        }];
        extents.extend(self.original_extents(side, source)?);
        let target = side.opposite();
        if let Some(unchanged) = peer.filter(|_| !repair_peer) {
            extents.extend(self.original_extents(target, unchanged)?);
        } else {
            let (at, bytes) = self.replacement_array(target, source, peer)?;
            let mut header = source.header.to_vec();
            let header_at = target.header_at(self.size, self.sector);
            let source_at = side.header_at(self.size, self.sector);
            header[24..32].copy_from_slice(&(header_at / self.sector).to_le_bytes());
            header[32..40].copy_from_slice(&(source_at / self.sector).to_le_bytes());
            header[72..80].copy_from_slice(&(at / self.sector).to_le_bytes());
            header[16..20].fill(0);
            let crc = crc32fast::hash(&header[..92]);
            header[16..20].copy_from_slice(&crc.to_le_bytes());
            let (header_kind, array_kind) = target.kinds();
            extents.extend([
                Extent {
                    kind: header_kind,
                    offset: header_at,
                    bytes: header,
                },
                Extent {
                    kind: array_kind,
                    offset: at,
                    bytes,
                },
            ]);
        }
        extents.sort_by_key(|e| e.offset);
        let output = Archive {
            version: 1,
            size: self.size,
            sector: self.sector,
            source: digest.into(),
            includes_boot: false,
            extents,
        };
        output.validate()?;
        Ok((
            output,
            Changes {
                authority: side,
                repair_peer,
                repair_protective_mbr,
            },
        ))
    }
}

pub(crate) fn inspect(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
) -> Result<Report, String> {
    let snapshot = Snapshot::read(reader, size, sector)?;
    let digest = snapshot.digest();
    let copies = snapshot.copies();
    let conflict = matches!(&copies, [Ok(a), Ok(b)] if !agrees(a, b));
    let protective_mbr = match snapshot.protective() {
        Ok((_, false)) => "valid",
        Ok((_, true)) => "repairable",
        Err(_) => "blocked",
    };
    let [primary, backup] = [Side::Primary, Side::Backup].map(|side| {
        let (current, other) = if side == Side::Primary {
            (&copies[0], &copies[1])
        } else {
            (&copies[1], &copies[0])
        };
        match current {
            Ok(current) => {
                let (changes, repair_error) = match snapshot.output(side, current, other, &digest) {
                    Ok((_, changes)) => (Some(changes), None),
                    Err(error) => (None, Some(error)),
                };
                CopyReport {
                    side,
                    layout: Some(layout(current, sector)),
                    error: None,
                    changes,
                    repair_error,
                }
            }
            Err((_, error)) => CopyReport {
                side,
                layout: None,
                error: Some(error.clone()),
                changes: None,
                repair_error: None,
            },
        }
    });
    Ok(Report {
        digest,
        sector,
        conflict,
        protective_mbr,
        copies: [primary, backup],
    })
}

pub(crate) fn prepare(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    side: Side,
    digest: &str,
) -> Result<Conversion, String> {
    let snapshot = Snapshot::read(reader, size, sector)?;
    if snapshot.digest() != digest {
        return Err("分区表在检查后发生变化，请重新检查。".into());
    }
    let [primary, backup] = snapshot.copies();
    let (current, other) = if side == Side::Primary {
        (primary, backup)
    } else {
        (backup, primary)
    };
    let current = current.map_err(|(_, reason)| reason)?;
    let (archive, changes) = snapshot.output(side, &current, &other, digest)?;
    let layout = layout(&current, sector);
    let partitions = layout
        .partitions
        .into_iter()
        .map(|p| PartitionChange {
            offset: p.offset,
            size: p.size,
            old_type: String::new(),
            new_type: p.name,
            logical: false,
        })
        .collect();
    Conversion::replace_table(
        reader,
        vec![(0, snapshot.head), (size - MIB, snapshot.tail)],
        archive,
        Preview {
            from: Scheme::Gpt,
            to: Scheme::Gpt,
            sector,
            disk_id: layout.disk_id,
            partitions,
            table_bytes: 0,
            placement: None,
            restored_from: None,
            gpt_repair: Some(changes),
            registration: None,
        },
    )
}

#[cfg(test)]
#[path = "image_gpt_repair_tests.rs"]
mod tests;

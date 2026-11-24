use super::{
    super::{
        empty_mbr, extended, label, mbr_record, u32le, zero_ebr, Archive, Extent, Kind,
        PartitionChange, Uuid, EXTENDED,
    },
    Evidence, Form,
};
use std::{
    io::{Read, Seek},
    ops::Range,
};

struct Container {
    slot: usize,
    kind: u8,
    range: Range<u64>,
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}
fn candidate_range(e: &Evidence) -> Range<u64> {
    e.extent.offset..e.extent.offset + e.extent.size
}
fn code(e: &Evidence) -> Result<u8, String> {
    match e.candidate.filesystem.as_str() {
        "FAT12" => Ok(0x01),
        "FAT16" => Ok(0x0e),
        "FAT32" => Ok(0x0c),
        "NTFS" | "exFAT" => Ok(0x07),
        "Ext2" | "Ext3" | "Ext4" | "ext2" | "ext3" | "ext4" | "Btrfs" => Ok(0x83),
        _ => Err("该文件系统没有可登记的 MBR 类型。".into()),
    }
}
fn set_slot(
    bytes: &mut [u8],
    slot: usize,
    kind: u8,
    range: Range<u64>,
    sector: u64,
) -> Result<(), String> {
    bytes[446 + slot * 16..462 + slot * 16].fill(0);
    mbr_record(
        bytes,
        slot,
        kind,
        range.start / sector,
        (range.end - range.start) / sector,
    )
}

pub(super) fn add(
    reader: &mut (impl Read + Seek),
    mut archive: Archive,
    selected: &[Evidence],
    form: Form,
    retained: &mut Vec<(u64, Vec<u8>)>,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<(Archive, Vec<PartitionChange>), String> {
    let sector = archive.sector;
    if archive.size / sector > 1u64 << 32 || archive.extents[0].kind != Kind::Mbr {
        return Err("镜像容量或分区表不符合 MBR 登记要求。".into());
    }
    let info = archive.validate()?;
    let old_data: Vec<_> = info
        .layout
        .iter()
        .map(|p| p.offset..p.offset + p.size)
        .collect();
    let mut primary = Vec::new();
    let mut free = Vec::new();
    let mut container = None;
    let mut root = archive.extents[0].bytes.clone();
    for (slot, bytes) in root[446..510].chunks_exact(16).enumerate() {
        let kind = bytes[4];
        if kind == 0 {
            free.push(slot);
            continue;
        }
        let start = u64::from(u32le(&bytes[8..12])) * sector;
        let end = start + u64::from(u32le(&bytes[12..16])) * sector;
        if extended(kind) {
            container = Some(Container {
                slot,
                kind,
                range: start..end,
            });
        } else {
            primary.push(start..end);
        }
    }
    let candidates: Vec<_> = selected.iter().map(candidate_range).collect();
    let old_container = container.as_ref().map(|c| &c.range);
    let mut required = candidates
        .iter()
        .enumerate()
        .filter(|(_, r)| old_container.is_some_and(|c| overlap(c, r)))
        .map(|(i, _)| i);
    let must_include = required
        .next()
        .map(|first| first..required.next_back().unwrap_or(first) + 1);
    let new_chain = container.is_none();
    let requested = match form {
        Form::Primary if must_include.is_none() && selected.len() <= free.len() => 0,
        Form::Primary => return Err("所选分区与扩展容器相交，或没有足够的主分区表项。".into()),
        Form::Logical => selected.len(),
        Form::Auto if must_include.is_none() && selected.len() <= free.len() => 0,
        Form::Auto => selected
            .len()
            .saturating_sub(free.len().saturating_sub(usize::from(new_chain)))
            .max(must_include.as_ref().map_or(0, |r| r.len())),
    };
    let mut ebr_positions = Vec::new();
    let mut logical = 0..0;
    if requested > 0 {
        if new_chain && free.is_empty() {
            return Err("MBR 中没有可用于扩展分区的空闲表项。".into());
        }
        if archive.extents.len() - 1 + requested > disk_core::mbr::MAX_EBRS {
            return Err("登记后的 EBR 链超过 64 个扇区的支持上限。".into());
        }
        for (i, r) in candidates.iter().enumerate() {
            tick()?;
            let lower = old_data
                .iter()
                .chain(candidates[..i].iter())
                .filter(|p| p.end <= r.start)
                .map(|p| p.end)
                .max()
                .unwrap_or(sector);
            ebr_positions.push(zero_ebr(reader, &archive, lower, r.start)?);
        }
        let first = (0..=selected.len() - requested)
            .rev()
            .find(|&start| {
                let end = start + requested;
                if must_include
                    .as_ref()
                    .is_some_and(|r| start > r.start || end < r.end)
                    || ebr_positions[start..end].iter().any(Option::is_none)
                {
                    return false;
                }
                let mut range = ebr_positions[start].unwrap()..candidates[end - 1].end;
                if let Some(old) = old_container {
                    range.start = range.start.min(old.start);
                    range.end = range.end.max(old.end);
                }
                primary
                    .iter()
                    .chain(candidates[..start].iter())
                    .chain(candidates[end..].iter())
                    .all(|part| !overlap(part, &range))
            })
            .ok_or("没有足够的全零 EBR 扇区，或扩展容器将与主分区相交。")?;
        logical = first..first + requested;
    }

    let container_slot = if logical.is_empty() {
        None
    } else {
        container.as_ref().map(|c| c.slot).or_else(|| free.pop())
    };
    let mut free = free.into_iter();
    let mut added = Vec::new();
    let mut partitions = Vec::with_capacity(selected.len());
    for (i, e) in selected.iter().enumerate() {
        let kind = code(e)?;
        let range = candidates[i].clone();
        if logical.contains(&i) {
            let at = ebr_positions[i].ok_or("缺少已核对的 EBR 位置。")?;
            retained.push((at, vec![0; sector as usize]));
            let mut bytes = empty_mbr(sector);
            set_slot(
                &mut bytes,
                0,
                kind,
                range.start - at..range.end - at,
                sector,
            )?;
            added.push(Extent {
                kind: Kind::Ebr,
                offset: at,
                bytes,
            });
        } else {
            let slot = free.next().ok_or("没有足够的主分区表项。")?;
            set_slot(&mut root, slot, kind, range.clone(), sector)?;
        }
        partitions.push(PartitionChange {
            offset: range.start,
            size: e.extent.size,
            old_type: e.candidate.filesystem.clone(),
            new_type: label(kind),
            logical: logical.contains(&i),
        });
    }
    if let Some(slot) = container_slot {
        archive.extents.append(&mut added);
        archive.extents.sort_unstable_by_key(|e| e.offset);
        let ebrs = &mut archive.extents[1..];
        let base = ebrs[0].offset;
        let kind = container.as_ref().map_or(EXTENDED, |c| c.kind);
        let end = container
            .as_ref()
            .map_or(0, |c| c.range.end)
            .max(candidates[logical.end - 1].end);
        if old_container != Some(&(base..end)) {
            set_slot(&mut root, slot, kind, base..end, sector)?;
        }
        for index in 0..ebrs.len() {
            let next = ebrs.get(index + 1).map(|e| e.offset);
            let bytes = &mut ebrs[index].bytes;
            match next {
                Some(next)
                    if old_container.is_some_and(|old| {
                        old.start == base
                            && extended(bytes[466])
                            && u64::from(u32le(&bytes[470..474])) * sector + base == next
                    }) => {}
                Some(next) => set_slot(bytes, 1, kind, next - base..end - base, sector)?,
                None => bytes[462..478].fill(0),
            }
        }
    }
    if u32le(&root[440..444]) == 0 {
        root[440..444]
            .copy_from_slice(&u32le(&Uuid::new_v4().as_bytes()[..4]).max(1).to_le_bytes());
    }
    archive.extents[0].bytes = root;
    let mut expected = old_data;
    expected.extend(candidates);
    expected.sort_unstable_by_key(|p| p.start);
    let mut actual: Vec<_> = archive
        .validate()?
        .layout
        .into_iter()
        .map(|p| p.offset..p.offset + p.size)
        .collect();
    actual.sort_unstable_by_key(|p| p.start);
    if actual != expected {
        return Err("登记后分区表的布局与候选范围不一致。".into());
    }
    Ok((archive, partitions))
}

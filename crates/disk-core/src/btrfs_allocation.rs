//! Allocation-aware reads of a verified, committed single-device filesystem.
use super::*;
use btrfs_disk::raw::{
    BTRFS_BLOCK_GROUP_ITEM_KEY, BTRFS_BLOCK_GROUP_TREE_OBJECTID, BTRFS_EXTENT_ITEM_KEY,
    BTRFS_FEATURE_COMPAT_RO_BLOCK_GROUP_TREE, BTRFS_METADATA_ITEM_KEY,
};
use std::ops::Range;

const EXTENT_TREE: u64 = 2;
const BLOCK_GROUP_TREE: u64 = BTRFS_BLOCK_GROUP_TREE_OBJECTID as u64;
const BLOCK_GROUP_ITEM: u8 = BTRFS_BLOCK_GROUP_ITEM_KEY as u8;
const EXTENT_ITEM: u8 = BTRFS_EXTENT_ITEM_KEY as u8;
const METADATA_ITEM: u8 = BTRFS_METADATA_ITEM_KEY as u8;
const BLOCK_GROUP_TREE_FLAG: u64 = BTRFS_FEATURE_COMPAT_RO_BLOCK_GROUP_TREE as u64;
const SUPER_STRIPE: u64 = 64 * 1024;
const DEVICE_PREFIX: u64 = 1024 * 1024;
const MAX_EXTENTS: usize = 1_048_576;

pub enum CopyAllocation {
    /// Byte ranges known to be free, relative to this device.
    SingleDevice(Vec<Range<u64>>),
    /// A lone member cannot prove the allocation of the entire filesystem.
    MultiDevice,
}

#[derive(Default)]
struct GroupUsage {
    declared: Option<u64>,
    accounted: u64,
}

/// Determine which sectors an allocated backup or clone may omit.
/// Preserve system and metadata chunks in full, including their unused sectors.
/// The caller supplies a stable bounded reader and tracks the bytes read for proof.
pub fn allocation_for_copy(
    reader: &mut (impl Read + Seek),
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<CopyAllocation, String> {
    let size = reader.seek(SeekFrom::End(0)).map_err(err)?;
    let sb = read_super(reader, &mut checkpoint)?;
    if sb.device_count != 1 {
        return Ok(CopyAllocation::MultiDevice);
    }
    if sb.device.size > size {
        return Err("Btrfs 成员容量超出已验证分区。".into());
    }
    let mut primary = [0; SUPER_BYTES];
    for at in SUPER_OFFSETS
        .into_iter()
        .filter(|at| at + SUPER_BYTES as u64 <= sb.device.size)
    {
        checkpoint()?;
        let mut raw = [0; SUPER_BYTES];
        read_at(reader, at, &mut raw)?;
        if Super::parse(&raw, at, size)?.identity != sb.identity {
            return Err("Btrfs 超级块副本不一致，无法确认空闲范围。".into());
        }
        if at == SUPER_OFFSETS[0] {
            primary = raw;
        }
    }
    let flags = le64(&primary, 0xb4)?;
    if le64(&primary, 0x38)? != 1 || flags & !0xf != 0 {
        return Err("Btrfs 超级块状态或只读兼容特性不支持分配检查。".into());
    }
    let mut volume = open(reader, &mut checkpoint)?;
    if volume.identity() != sb.identity
        || volume
            .mapping
            .values()
            .any(|chunk| !matches!(chunk.kind & !7, 0 | DUP))
    {
        return Err("Btrfs 卷状态或块组配置不支持分配检查。".into());
    }
    volume.checked_device_extents(true, &mut checkpoint)?;
    let mut usages: BTreeMap<u64, GroupUsage> = volume
        .mapping
        .keys()
        .map(|&start| (start, GroupUsage::default()))
        .collect();
    let mut retained = Vec::new();
    retained.push(0..DEVICE_PREFIX.min(sb.device.size));
    for at in SUPER_OFFSETS {
        let start = at / SUPER_STRIPE * SUPER_STRIPE;
        if start < sb.device.size {
            retained.push(start..(start + SUPER_STRIPE).min(sb.device.size));
        }
    }
    for chunk in volume.mapping.values().filter(|chunk| chunk.kind & 7 != 1) {
        retained.extend(
            chunk
                .stripes
                .iter()
                .map(|stripe| stripe.offset..stripe.offset + chunk.span()),
        );
    }

    let separate_groups = flags & BLOCK_GROUP_TREE_FLAG != 0;
    let mut extent_count = 0;
    let mut last_end = 0;
    let sector = u64::from(sb.sector_size);
    let root = volume.current.extent.ok_or("Btrfs 缺少分配树。")?;
    scan_tree(
        &mut volume.reader,
        &sb,
        &volume.mapping,
        root,
        EXTENT_TREE,
        None,
        |key, item| {
            if key.kind == BLOCK_GROUP_ITEM {
                if separate_groups {
                    return Err("Btrfs 块组记录出现在错误的树中。".into());
                }
                return record_group(&volume.mapping, &mut usages, key, item);
            }
            let expected_type = match key.kind {
                EXTENT_ITEM => None,
                METADATA_ITEM if key.offset < 8 => Some(2),
                METADATA_ITEM => return Err("Btrfs 元数据分配层级无效。".into()),
                _ => return Ok(()),
            };
            extent_count += 1;
            if extent_count > MAX_EXTENTS {
                return Err("Btrfs 分配记录超过检查上限。".into());
            }
            let kind = le64(item, 16)? & 3;
            let length = if key.kind == METADATA_ITEM {
                u64::from(sb.node_size)
            } else {
                key.offset
            };
            let logical_end = key
                .object_id
                .checked_add(length)
                .ok_or("Btrfs 分配范围溢出。")?;
            if !matches!(kind, 1 | 2)
                || expected_type.is_some_and(|expected| expected != kind)
                || le64(item, 0)? == 0
                || !(1..=sb.generation).contains(&le64(item, 8)?)
                || length == 0
                || length % sector != 0
                || key.object_id % sector != 0
                || key.object_id < last_end
                || (kind == 2 && length != u64::from(sb.node_size))
            {
                return Err("Btrfs 分配记录的类型、范围或代次无效。".into());
            }
            last_end = logical_end;
            let (&start, chunk) = volume
                .mapping
                .range(..=key.object_id)
                .next_back()
                .ok_or("Btrfs 分配记录缺少块组。")?;
            if logical_end > start + chunk.length
                || (kind == 1 && chunk.kind & 1 == 0)
                || (kind == 2 && chunk.kind & (SYSTEM | METADATA) == 0)
            {
                return Err("Btrfs 分配记录跨越块组或类型不符。".into());
            }
            usages.get_mut(&start).unwrap().accounted += length;
            if chunk.kind & 7 == 1 {
                let delta = key.object_id - start;
                retained.extend(
                    chunk
                        .stripes
                        .iter()
                        .map(|stripe| stripe.offset + delta..stripe.offset + delta + length),
                );
            }
            Ok(())
        },
        &mut checkpoint,
    )?;

    if separate_groups {
        let mut group_root = None;
        let key = Key {
            object_id: BLOCK_GROUP_TREE,
            kind: ROOT_ITEM,
            offset: 0,
        };
        scan_tree(
            &mut volume.reader,
            &sb,
            &volume.mapping,
            sb.root,
            ROOT_TREE,
            Some((key, key)),
            |_, item| {
                group_root = Some(BlockRef {
                    logical: le64(item, 176)?,
                    generation: le64(item, 160)?,
                    level: word::<1>(item, 238)?[0],
                });
                Ok(())
            },
            &mut checkpoint,
        )?;
        let root = group_root.ok_or("Btrfs 缺少独立块组树。")?;
        scan_tree(
            &mut volume.reader,
            &sb,
            &volume.mapping,
            root,
            BLOCK_GROUP_TREE,
            None,
            |key, item| record_group(&volume.mapping, &mut usages, key, item),
            &mut checkpoint,
        )?;
    }
    let accounted = usages.values().try_fold(0u64, |sum, usage| {
        if usage.declared != Some(usage.accounted) {
            return Err("Btrfs 分配记录总量与块组用量不一致。");
        }
        sum.checked_add(usage.accounted)
            .ok_or("Btrfs 分配用量溢出。")
    })?;
    if accounted != le64(&primary, 0x78)? {
        return Err("Btrfs 块组用量之和与超级块不一致。".into());
    }
    volume.verify_view(&mut checkpoint)?;

    retained.sort_unstable_by_key(|range| (range.start, range.end));
    let mut free = Vec::new();
    let mut end = 0;
    for range in retained {
        if range.start > end {
            free.push(end..range.start);
        }
        end = end.max(range.end);
    }
    if end < sb.device.size {
        free.push(end..sb.device.size);
    }
    Ok(CopyAllocation::SingleDevice(free))
}

fn record_group(
    chunks: &BTreeMap<u64, Chunk>,
    usages: &mut BTreeMap<u64, GroupUsage>,
    key: Key,
    item: &[u8],
) -> Result<(), String> {
    let chunk = chunks
        .get(&key.object_id)
        .ok_or("Btrfs 块组缺少物理映射。")?;
    let used = le64(item, 0)?;
    if key.kind != BLOCK_GROUP_ITEM
        || item.len() != 24
        || key.offset != chunk.length
        || le64(item, 16)? != chunk.kind
        || used > chunk.length
        || usages
            .get_mut(&key.object_id)
            .unwrap()
            .declared
            .replace(used)
            .is_some()
    {
        return Err("Btrfs 块组用量记录异常。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::fixture::{allocation_image, MIB};
    use std::io::Cursor;

    #[test]
    fn copy_keeps_both_system_stripes_and_the_allocated_data() {
        for separate in [false, true] {
            let mut reader = Cursor::new(allocation_image(separate, 4096));
            let CopyAllocation::SingleDevice(free) =
                allocation_for_copy(&mut reader, || Ok(())).unwrap()
            else {
                panic!("single-device fixture");
            };
            assert_eq!(free, [4 * MIB + 4096..8 * MIB]);
            let mut inconsistent = Cursor::new(allocation_image(separate, 0));
            assert!(allocation_for_copy(&mut inconsistent, || Ok(())).is_err());
        }
    }
}

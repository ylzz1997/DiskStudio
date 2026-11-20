//! Stage whole Btrfs stripes in unused physical space and change their
//! mappings in the same copy-on-write transaction as the device size.
use super::*;
use btrfs_disk::tree::{DiskKey, KeyType};
use btrfs_transaction::{
    items, path::BtrfsPath, search, search::SearchIntent, Filesystem, Transaction,
};
use std::{
    collections::BTreeSet,
    io::{self, Read, Seek, SeekFrom, Write},
};

const FIRST_ALLOCATABLE: u64 = 1024 * 1024;
const STRIPE_ALIGNMENT: u64 = 64 * 1024;
const COPYING_WINDOW: usize = 1024 * 1024;

pub(super) fn touches_super_stripe(start: u64, end: u64) -> bool {
    SUPER_OFFSETS
        .iter()
        .any(|&at| start < at + STRIPE_ALIGNMENT && at < end)
}

#[derive(Clone, Copy, Debug)]
struct Move {
    logical: u64,
    stripe: usize,
    from: u64,
    to: u64,
    len: u64,
}

struct DeviceEdit {
    old_key: u64,
    old_item: [u8; 48],
    new_key: u64,
    new_item: [u8; 48],
}

pub(super) struct Relocation {
    pub(super) mapping: BTreeMap<u64, Chunk>,
    pub(super) system: BTreeMap<u64, Chunk>,
    moves: Vec<Move>,
    device_edits: Vec<DeviceEdit>,
    changed: BTreeSet<u64>,
}

impl Relocation {
    pub(super) fn plan(
        sb: &Super,
        mapping: &BTreeMap<u64, Chunk>,
        devices: &BTreeMap<u64, [u8; 48]>,
        after: u64,
    ) -> Result<Self, String> {
        let mut result = Self {
            mapping: mapping.clone(),
            system: sb.bootstrap.clone(),
            moves: Vec::new(),
            device_edits: Vec::new(),
            changed: BTreeSet::new(),
        };
        for (&logical, chunk) in mapping {
            for (stripe, placement) in chunk.stripes.iter().enumerate() {
                let from = placement.offset;
                if from + chunk.length <= after {
                    continue;
                }
                // Moving a superblock stripe also changes logical reservations
                // in the free-space tree. Keep those reservations in place.
                if touches_super_stripe(from, from + chunk.length) {
                    return Err("Btrfs 尾部块组含超级块保留区，当前无法搬移。".into());
                }
                result.moves.push(Move {
                    logical,
                    stripe,
                    from,
                    to: 0,
                    len: chunk.length,
                });
            }
        }
        if result.moves.is_empty() {
            return Ok(result);
        }

        let mut free = available_gaps(mapping, after);
        result
            .moves
            .sort_unstable_by_key(|m| (std::cmp::Reverse(m.len), m.from));
        let mut moved_devices = devices.clone();
        for m in &mut result.moves {
            let &(available, start) = free
                .range((m.len, 0)..)
                .next()
                .ok_or("Btrfs 新边界内没有足够连续的未分配设备空间来搬移尾部块组。")?;
            free.remove(&(available, start));
            m.to = start;
            add_gap(&mut free, start + m.len, start + available);
            result.changed.insert(m.logical);
            result.mapping.get_mut(&m.logical).unwrap().stripes[m.stripe].offset = start;
            if let Some(chunk) = result.system.get_mut(&m.logical) {
                chunk.stripes[m.stripe].offset = start;
            }
            let entry = moved_devices
                .remove(&m.from)
                .ok_or("Btrfs 缺少待搬移设备范围。")?;
            if start >= m.from || moved_devices.insert(start, entry).is_some() {
                return Err("Btrfs 搬移目标与现有设备范围重叠。".into());
            }
        }
        chunks_consistent(
            &result.mapping,
            &BTreeMap::from([(sb.device.id, sb.device.clone())]),
        )?;
        // Every moved physical key decreases. The sorted replacement keys
        // therefore never cross the next unmodified key. Reuse the existing
        // device-tree slots, including leaf boundaries and item sizes.
        result.device_edits = devices
            .iter()
            .zip(moved_devices.iter())
            .filter_map(|((&old_key, &old_item), (&new_key, &new_item))| {
                (old_key != new_key || old_item != new_item).then_some(DeviceEdit {
                    old_key,
                    old_item,
                    new_key,
                    new_item,
                })
            })
            .collect();
        Ok(result)
    }

    pub(super) fn bytes(&self) -> u64 {
        self.moves.iter().map(|m| m.len).sum()
    }

    pub(super) fn projected_ranges(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.moves.iter().map(|m| (m.to, m.from, m.len))
    }

    pub(super) fn bind_moves(&self, hash: &mut Sha256) {
        for m in &self.moves {
            for n in [m.logical, m.from, m.to, m.len] {
                hash.update(n.to_le_bytes());
            }
        }
    }

    pub(super) fn copy_and_verify(
        &self,
        stream: &mut (impl Read + Write + Seek),
    ) -> Result<(), String> {
        if self.moves.is_empty() {
            return Ok(());
        }
        let mut buffer = vec![0; COPYING_WINDOW];
        for m in &self.moves {
            let mut source = Sha256::new();
            let mut offset = 0;
            while offset < m.len {
                let length = (m.len - offset).min(buffer.len() as u64) as usize;
                let bytes = &mut buffer[..length];
                read_at(stream, m.from + offset, bytes)?;
                source.update(&*bytes);
                stream.seek(SeekFrom::Start(m.to + offset)).map_err(err)?;
                stream.write_all(bytes).map_err(err)?;
                offset += length as u64;
            }
            stream.flush().map_err(err)?;
            let mut target = Sha256::new();
            let mut offset = 0;
            while offset < m.len {
                let length = (m.len - offset).min(buffer.len() as u64) as usize;
                let bytes = &mut buffer[..length];
                read_at(stream, m.to + offset, bytes)?;
                target.update(&*bytes);
                offset += length as u64;
            }
            if target.finalize() != source.finalize() {
                return Err("Btrfs 搬移范围回读 SHA-256 核对失败。".into());
            }
        }
        Ok(())
    }

    pub(super) fn remap(
        &self,
        previous: &BTreeMap<u64, Chunk>,
        sb: &Super,
        fs: &mut Filesystem<impl Read + Write + Seek>,
    ) -> io::Result<()> {
        for &logical in &self.changed {
            let old = &previous[&logical];
            let new = &self.mapping[&logical];
            let cache = fs.reader_mut().chunk_cache_mut();
            let mut entry = cache
                .lookup(logical)
                .cloned()
                .ok_or_else(|| io::Error::other("Missing Btrfs chunk mapping"))?;
            if entry.logical != logical
                || entry.length != old.length
                || entry.chunk_type != old.kind
                || entry.stripes.len() != old.stripes.len()
                || !entry.stripes.iter().zip(&old.stripes).all(|(s, stripe)| {
                    s.devid == stripe.device
                        && s.dev_uuid.as_bytes() == &stripe.uuid
                        && s.offset == stripe.offset
                })
            {
                return Err(io::Error::other(
                    "Btrfs transaction chunk mapping differs from the verified source",
                ));
            }
            for (stripe, placement) in entry.stripes.iter_mut().zip(&new.stripes) {
                stripe.offset = placement.offset;
            }
            cache.insert(entry);
        }
        if !self.changed.iter().any(|key| self.system.contains_key(key)) {
            return Ok(());
        }
        let array_len = fs.superblock.sys_chunk_array_size as usize;
        let array = fs
            .superblock
            .sys_chunk_array
            .get_mut(..array_len)
            .ok_or_else(|| io::Error::other("Invalid Btrfs system array length"))?;
        let mut pos = 0;
        while pos < array.len() {
            let key = Key::parse(array, pos).map_err(io::Error::other)?;
            let old = sb
                .bootstrap
                .get(&key.offset)
                .ok_or_else(|| io::Error::other("Unrecognized Btrfs system chunk"))?;
            let end = pos + KEY_BYTES + 48 + 32 * old.stripes.len();
            let item = array
                .get_mut(pos + KEY_BYTES..end)
                .ok_or_else(|| io::Error::other("Truncated Btrfs system chunk"))?;
            patch_chunk(key, item, old, &self.system[&key.offset], sb)?;
            pos = end;
        }
        Ok(())
    }

    pub(super) fn edit_trees<R: Read + Write + Seek>(
        &self,
        previous: &BTreeMap<u64, Chunk>,
        sb: &Super,
        fs: &mut Filesystem<R>,
        transaction: &mut Transaction<R>,
    ) -> io::Result<()> {
        for &logical in &self.changed {
            let key = DiskKey {
                objectid: 256,
                key_type: KeyType::ChunkItem,
                offset: logical,
            };
            let mut path = BtrfsPath::new();
            if !search::search_slot(
                Some(transaction),
                fs,
                CHUNK_TREE,
                &key,
                &mut path,
                SearchIntent::ReadOnly,
                true,
            )? {
                return Err(io::Error::other("Missing Btrfs chunk item"));
            }
            let slot = path.slots[0];
            let leaf = path.nodes[0]
                .as_mut()
                .ok_or_else(|| io::Error::other("Missing Btrfs chunk leaf"))?;
            patch_chunk(
                Key {
                    object_id: 256,
                    kind: CHUNK_ITEM,
                    offset: logical,
                },
                leaf.item_data_mut(slot),
                &previous[&logical],
                &self.mapping[&logical],
                sb,
            )?;
            fs.mark_dirty(leaf);
        }
        for edit in &self.device_edits {
            let key = DiskKey {
                objectid: sb.device.id,
                key_type: KeyType::DeviceExtent,
                offset: edit.old_key,
            };
            let mut path = BtrfsPath::new();
            if !search::search_slot(
                Some(transaction),
                fs,
                DEVICE_TREE,
                &key,
                &mut path,
                SearchIntent::ReadOnly,
                true,
            )? {
                return Err(io::Error::other("Missing Btrfs device extent"));
            }
            let slot = path.slots[0];
            let leaf = path.nodes[0]
                .as_mut()
                .ok_or_else(|| io::Error::other("Missing Btrfs device leaf"))?;
            if leaf.item_data(slot) != edit.old_item || edit.new_key > edit.old_key {
                return Err(io::Error::other("Btrfs device extent changed"));
            }
            let new_key = DiskKey {
                offset: edit.new_key,
                ..key
            };
            leaf.set_item_key(slot, &new_key);
            items::update_item(leaf, slot, &edit.new_item)?;
            fs.mark_dirty(leaf);
            if slot == 0 && edit.old_key != edit.new_key {
                for (node, &slot) in path.nodes.iter_mut().zip(&path.slots).skip(1) {
                    let Some(node) = node else { break };
                    node.set_key_ptr_key(slot, &new_key);
                    fs.mark_dirty(node);
                    if slot != 0 {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

fn patch_chunk(key: Key, item: &mut [u8], old: &Chunk, new: &Chunk, sb: &Super) -> io::Result<()> {
    if Chunk::parse(key, item, sb.sector_size).map_err(io::Error::other)? != *old {
        return Err(io::Error::other("Btrfs chunk item changed"));
    }
    for (index, stripe) in new.stripes.iter().enumerate() {
        let from = 56 + index * 32;
        item[from..from + 8].copy_from_slice(&stripe.offset.to_le_bytes());
    }
    Ok(())
}

fn available_gaps(mapping: &BTreeMap<u64, Chunk>, after: u64) -> BTreeSet<(u64, u64)> {
    let mut reservations: Vec<_> = mapping
        .values()
        .flat_map(|chunk| {
            chunk
                .stripes
                .iter()
                .map(|stripe| (stripe.offset, stripe.offset + chunk.length))
        })
        .chain(SUPER_OFFSETS.iter().map(|&at| (at, at + STRIPE_ALIGNMENT)))
        .collect();
    reservations.sort_unstable();
    let mut free = BTreeSet::new();
    let mut cursor = FIRST_ALLOCATABLE;
    for (start, end) in reservations
        .into_iter()
        .take_while(|&(start, _)| start < after)
    {
        add_gap(&mut free, cursor, start);
        cursor = cursor.max(end);
    }
    add_gap(&mut free, cursor, after);
    free
}

fn add_gap(free: &mut BTreeSet<(u64, u64)>, start: u64, end: u64) {
    let aligned = start
        .div_ceil(STRIPE_ALIGNMENT)
        .saturating_mul(STRIPE_ALIGNMENT);
    if aligned < end {
        free.insert((end - aligned, aligned));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::members::Stripe;
    use std::io::Cursor;

    #[test]
    fn gaps_exclude_both_replicas_the_old_tail_and_superblock_stripes() {
        const MIB: u64 = 1024 * 1024;
        let mapping = BTreeMap::from([(
            8 * MIB,
            Chunk {
                length: 8 * MIB,
                kind: METADATA | DUP,
                stripes: [4 * MIB, 72 * MIB]
                    .map(|offset| Stripe {
                        device: 1,
                        uuid: [0; 16],
                        offset,
                    })
                    .to_vec(),
                columns: 1,
                copies: 2,
            },
        )]);
        let gaps = available_gaps(&mapping, 76 * MIB);
        let after_mirror = 64 * MIB + STRIPE_ALIGNMENT;
        assert_eq!(
            gaps,
            BTreeSet::from([
                (3 * MIB, MIB),
                (52 * MIB, 12 * MIB),
                (72 * MIB - after_mirror, after_mirror),
            ])
        );
        assert!(touches_super_stripe(64 * MIB + 4096, 64 * MIB + 8192));
    }

    #[test]
    fn stripe_copy_verifies_readback_and_keeps_the_original_bytes() {
        struct CorruptAfterFlush(Cursor<Vec<u8>>);

        impl Read for CorruptAfterFlush {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Seek for CorruptAfterFlush {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.0.seek(from)
            }
        }
        impl Write for CorruptAfterFlush {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.get_mut()[0] ^= 1;
                Ok(())
            }
        }

        let plan = Relocation {
            mapping: BTreeMap::new(),
            system: BTreeMap::new(),
            moves: vec![Move {
                logical: 0,
                stripe: 0,
                from: 4096,
                to: 0,
                len: 4096,
            }],
            device_edits: Vec::new(),
            changed: BTreeSet::new(),
        };
        let mut image = vec![0; 8192];
        image[4096..].fill(0xa7);
        let mut ordinary = Cursor::new(image.clone());
        plan.copy_and_verify(&mut ordinary).unwrap();
        assert_eq!(ordinary.into_inner(), [0xa7; 8192]);
        let mut fault = CorruptAfterFlush(Cursor::new(image));
        let error = plan.copy_and_verify(&mut fault).unwrap_err();
        assert!(error.contains("SHA-256"));
        assert_eq!(fault.0.get_ref()[4096..], [0xa7; 4096]);
    }
}

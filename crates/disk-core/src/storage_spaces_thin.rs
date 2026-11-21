//! Direct and cached reads for the standalone two-column thin layout.
use super::{cache, CopyLocation, Group, INVALID, RESERVED_SLABS, SLAB, UNSUPPORTED};
use crate::storage_spaces::Allocation;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, ErrorKind, Read, Seek, SeekFrom},
};

const INTERLEAVE: u64 = 64 * 1024;
const PAIR: u64 = 2 * SLAB;

struct Stripes {
    capacity: u64,
    pairs: BTreeMap<u64, [CopyLocation; 2]>,
}

impl Stripes {
    fn assemble<R: Read + Seek>(
        group: &Group<R>,
        capacity: u64,
        allocations: Vec<&Allocation>,
        physical: &mut BTreeSet<(u64, u64)>,
        complete: bool,
    ) -> Result<Self, String> {
        let meta = group.metadata();
        let mut pairs = BTreeMap::new();
        for item in allocations {
            if item.span != 1 || item.column > 1 || item.copy != 0 || item.virtual_block % 2 != 0 {
                return Err(UNSUPPORTED.into());
            }
            let logical = item
                .virtual_block
                .checked_mul(SLAB)
                .filter(|start| *start < capacity)
                .ok_or(INVALID)?;
            if !physical.insert((item.member, item.member_block)) {
                return Err(INVALID.into());
            }
            let identity = meta.members.get(&item.member).ok_or(INVALID)?;
            let (reader, member) = group
                .members
                .iter()
                .enumerate()
                .find(|(_, member)| &member.member == identity)
                .ok_or("缺少精简存储空间所需的条带成员。")?;
            let offset = item
                .member_block
                .checked_add(RESERVED_SLABS)
                .and_then(|block| block.checked_mul(SLAB))
                .and_then(|offset| member.partition.start.checked_add(offset))
                .filter(|offset| {
                    offset
                        .checked_add(SLAB)
                        .is_some_and(|end| end <= member.partition.end)
                })
                .ok_or(INVALID)?;
            let columns = pairs.entry(logical).or_insert([None; 2]);
            if columns[item.column as usize]
                .replace(CopyLocation { reader, offset })
                .is_some()
            {
                return Err(INVALID.into());
            }
        }
        let pairs = pairs
            .into_iter()
            .map(|(offset, columns)| match columns {
                [Some(left), Some(right)] if left.reader != right.reader => {
                    Ok((offset, [left, right]))
                }
                _ => Err(INVALID),
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        // All keys start on a pair boundary. A complete cache stream has no holes.
        if complete && (capacity % PAIR != 0 || pairs.len() as u64 != capacity / PAIR) {
            return Err(INVALID.into());
        }
        Ok(Self { capacity, pairs })
    }

    fn read<R: Read + Seek>(
        &self,
        readers: &mut [R],
        position: u64,
        output: &mut [u8],
        allow_hole: bool,
    ) -> io::Result<usize> {
        let mut count = (output.len() as u64).min(self.capacity.saturating_sub(position)) as usize;
        if count == 0 {
            return Ok(0);
        }
        let start = position / PAIR * PAIR;
        let Some(columns) = self.pairs.get(&start) else {
            if !allow_hole {
                return Err(ErrorKind::InvalidData.into());
            }
            if let Some((&start, _)) = self.pairs.range(position..).next() {
                count = (count as u64).min(start - position) as usize;
            }
            output[..count].fill(0);
            return Ok(count);
        };
        let within = position - start;
        let column = columns[(within / INTERLEAVE % 2) as usize];
        let fragment = within % INTERLEAVE;
        let offset = column.offset + within / (2 * INTERLEAVE) * INTERLEAVE + fragment;
        count = count.min((INTERLEAVE - fragment) as usize);
        let reader = &mut readers[column.reader];
        reader.seek(SeekFrom::Start(offset))?;
        reader.read_exact(&mut output[..count])?;
        Ok(count)
    }
}

pub(super) struct ThinPlan {
    data: Stripes,
    journal: Stripes,
    pages: BTreeMap<u64, u64>,
}

impl ThinPlan {
    pub(super) fn open<R: Read + Seek>(
        group: &mut Group<R>,
        id: u64,
        capacity: u64,
    ) -> Result<Self, String> {
        let metadata = group.metadata();
        let root = metadata.spaces.get(&id).ok_or(INVALID)?;
        let guid = root.guid;
        let mut bindings = metadata
            .thin_cache_owners
            .iter()
            .filter(|(_, owner)| owner.group == root.group);
        let (&owner_id, owner) = bindings.next().ok_or(UNSUPPORTED)?;
        if bindings.next().is_some()
            || metadata.spaces.get(&owner.space).is_none_or(|space| {
                space.group != root.group || space.thin_cache != Some((id, owner.capacity))
            })
        {
            return Err(UNSUPPORTED.into());
        }
        let cache_capacity = owner.capacity;
        let mut data = Vec::new();
        let mut cached = Vec::new();
        for item in &metadata.allocations {
            if item.group != root.group && item.space != id && item.space != owner_id {
                continue;
            }
            if item.group != root.group {
                return Err(INVALID.into());
            }
            match (item.space, item.flags) {
                (space, 0) if space == id => data.push(item),
                (space, 4) if space == owner_id => cached.push(item),
                _ => return Err(UNSUPPORTED.into()),
            }
        }
        let mut physical = BTreeSet::new();
        let data = Stripes::assemble(group, capacity, data, &mut physical, false)?;
        let journal = Stripes::assemble(group, cache_capacity, cached, &mut physical, true)?;
        let mut bytes = vec![0; cache::THIN_PAYLOAD];
        let mut offset = 0;
        while offset < bytes.len() {
            let count = journal
                .read(
                    &mut group.readers,
                    offset as u64,
                    &mut bytes[offset..],
                    false,
                )
                .map_err(|error| format!("无法读取存储空间缓存：{error}"))?;
            if count == 0 {
                return Err(cache::PENDING.into());
            }
            offset += count;
        }
        let pages = cache::thin_pages(&bytes, guid, capacity, cache_capacity)?;
        Ok(Self {
            data,
            journal,
            pages,
        })
    }

    pub(super) fn capacity(&self) -> u64 {
        self.data.capacity
    }

    pub(super) fn read<R: Read + Seek>(
        &self,
        readers: &mut [R],
        position: u64,
        output: &mut [u8],
    ) -> io::Result<usize> {
        let block = position / cache::SECTOR as u64 * cache::SECTOR as u64;
        if let Some(&cache_offset) = self.pages.get(&block) {
            let fragment = (position - block) as usize;
            let count = output.len().min(cache::SECTOR - fragment);
            return self.journal.read(
                readers,
                cache_offset + fragment as u64,
                &mut output[..count],
                false,
            );
        }
        let count = self
            .pages
            .range(position..)
            .next()
            .map_or(output.len(), |(&next, _)| {
                (output.len() as u64).min(next - position) as usize
            });
        self.data
            .read(readers, position, &mut output[..count], true)
    }
}

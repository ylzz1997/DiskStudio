use super::{cache, missing_members, read_at, read_member, Member, Record, INVALID};
use crate::MIB;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, ErrorKind, Read, Seek, SeekFrom},
};

#[path = "storage_spaces_thin.rs"]
mod thin;

const SLAB: u64 = 256 * MIB;
const RESERVED_SLABS: u64 = 2;
const UNSUPPORTED: &str = "存储空间的逻辑布局超出当前重组支持范围。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpaceLayout {
    FixedMirror(u64),
    ThinStripe(u64),
}

fn bytes(record: &mut Record<'_>, expected: &[u8]) -> Result<(), &'static str> {
    if record.take(expected.len())? != expected {
        return Err(UNSUPPORTED);
    }
    Ok(())
}

pub(super) fn space(mut record: Record<'_>) -> Result<SpaceLayout, &'static str> {
    bytes(&mut record, &[0, 0, 2])?;
    let capacity = record.number()?;
    if capacity == 0 || capacity % 4096 != 0 {
        return Err(UNSUPPORTED);
    }
    let layout = match record.number()? {
        0 => {
            fixed_mirror_fields(&mut record, capacity)?;
            SpaceLayout::FixedMirror(capacity)
        }
        3 => {
            bytes(&mut record, &[1])?;
            if record.number()? != SLAB || capacity % (128 * 1024) != 0 {
                return Err(UNSUPPORTED);
            }
            bytes(&mut record, &[0, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 2, 16])?;
            if record.number()? != capacity {
                return Err(UNSUPPORTED);
            }
            SpaceLayout::ThinStripe(capacity)
        }
        _ => return Err(UNSUPPORTED),
    };
    bytes(&mut record, &[0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0])?;
    if !record.0.is_empty() {
        return Err(UNSUPPORTED);
    }
    Ok(layout)
}

fn fixed_mirror_fields(record: &mut Record<'_>, capacity: u64) -> Result<(), &'static str> {
    bytes(record, &[2])?;
    if record.number()? != capacity {
        return Err(UNSUPPORTED);
    }
    bytes(record, &[0, 1, 0, 1, 0, 0, 2, 1, 1, 1, 2, 1, 1, 1, 1])?;
    if !(12..=20).contains(&record.take(1)?[0]) || record.number()? != capacity {
        return Err(UNSUPPORTED);
    }
    Ok(())
}

pub(super) fn thin_cache_space(mut record: Record<'_>) -> Result<(u64, u64), &'static str> {
    bytes(&mut record, &[0, 1, 11])?;
    let capacity = record.number()?;
    bytes(&mut record, &[1, 4, 2])?;
    if record.number()? != SLAB {
        return Err(UNSUPPORTED);
    }
    bytes(&mut record, &[2, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 2, 16])?;
    if record.number()? != capacity {
        return Err(UNSUPPORTED);
    }
    bytes(&mut record, &[0, 0, 0, 0, 1, 1])?;
    let space_id = record.number()?;
    if record.0 != [0; 4] || capacity == 0 || capacity % (2 * SLAB) != 0 || space_id == 0 {
        return Err(UNSUPPORTED);
    }
    Ok((space_id, capacity))
}

pub(super) fn thin_cache_owner(mut record: Record<'_>) -> Result<(u64, u64), &'static str> {
    bytes(&mut record, &[0, 0, 0, 0, 0, 0, 7, 2])?;
    if record.number()? != SLAB {
        return Err(UNSUPPORTED);
    }
    bytes(
        &mut record,
        &[2, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 2, 16, 0],
    )?;
    let cache_space = record.number()?;
    bytes(
        &mut record,
        &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    )?;
    let size = u32::from_be_bytes(record.take(4)?.try_into().unwrap());
    if !record.0.is_empty() || cache_space == 0 || size == 0 || u64::from(size) % (2 * SLAB) != 0 {
        return Err(UNSUPPORTED);
    }
    Ok((cache_space, u64::from(size)))
}

/// Readers must represent stable images or verified offline member handles.
pub struct Group<R> {
    readers: Vec<R>,
    members: Vec<Member>,
}

#[derive(Clone, Copy)]
struct CopyLocation {
    reader: usize,
    offset: u64,
}

struct Run {
    length: u64,
    copies: Vec<CopyLocation>,
}

struct MirrorPlan {
    capacity: u64,
    runs: BTreeMap<u64, Run>,
}

enum Plan {
    Mirror(MirrorPlan),
    Thin(thin::ThinPlan),
}

impl Plan {
    fn capacity(&self) -> u64 {
        match self {
            Self::Mirror(plan) => plan.capacity,
            Self::Thin(plan) => plan.capacity(),
        }
    }

    fn degraded(&self) -> bool {
        match self {
            Self::Mirror(plan) => plan.runs.values().any(|run| run.copies.len() < 2),
            Self::Thin(_) => false,
        }
    }
}

impl<R: Read + Seek> Group<R> {
    pub fn open(mut readers: Vec<R>, member_sector: u64) -> Result<Self, String> {
        if member_sector != 4096
            || readers.is_empty()
            || readers.len() > crate::virtual_raid::MAX_MEMBERS
        {
            return Err(UNSUPPORTED.into());
        }
        let members = readers
            .iter_mut()
            .map(|reader| read_member(reader, member_sector))
            .collect::<Result<Vec<_>, _>>()?;
        missing_members(&members)?;
        Ok(Self { readers, members })
    }

    pub fn metadata(&self) -> &Member {
        &self.members[0]
    }

    pub fn reader(
        &mut self,
        space_id: u64,
        allow_degraded: bool,
    ) -> Result<SpaceReader<&mut R>, String> {
        let plan = self.plan(space_id, allow_degraded)?;
        Ok(SpaceReader {
            readers: self.readers.iter_mut().collect(),
            plan,
            position: 0,
            comparison: Vec::new(),
        })
    }

    pub fn availability(&mut self, space_id: u64) -> Result<(u64, bool), String> {
        let plan = self.plan(space_id, true)?;
        Ok((plan.capacity(), plan.degraded()))
    }

    pub fn member_layouts(&self) -> Vec<(super::Guid, [u8; 32])> {
        self.members
            .iter()
            .map(|member| (member.member, member.layout_digest))
            .collect()
    }

    pub fn into_reader(
        mut self,
        space_id: u64,
        allow_degraded: bool,
    ) -> Result<SpaceReader<R>, String> {
        let plan = self.plan(space_id, allow_degraded)?;
        Ok(SpaceReader {
            readers: self.readers,
            plan,
            position: 0,
            comparison: Vec::new(),
        })
    }

    fn plan(&mut self, space_id: u64, allow_degraded: bool) -> Result<Plan, String> {
        let layout = self
            .metadata()
            .spaces
            .get(&space_id)
            .filter(|space| !space.name.is_empty())
            .and_then(|space| space.layout)
            .ok_or(UNSUPPORTED)?;
        match layout {
            SpaceLayout::FixedMirror(capacity) => {
                let plan = self.mirror_plan(space_id, capacity, allow_degraded)?;
                self.check_cache(space_id, &plan)?;
                Ok(Plan::Mirror(plan))
            }
            SpaceLayout::ThinStripe(capacity) => {
                thin::ThinPlan::open(self, space_id, capacity).map(Plan::Thin)
            }
        }
    }

    fn mirror_plan(
        &self,
        space_id: u64,
        capacity: u64,
        allow_degraded: bool,
    ) -> Result<MirrorPlan, String> {
        let metadata = self.metadata();
        let root = metadata
            .spaces
            .get(&space_id)
            .filter(|space| !space.name.is_empty())
            .ok_or(UNSUPPORTED)?;
        let slots: BTreeMap<_, _> = self
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| (member.member, index))
            .collect();
        let mut allocations = BTreeMap::new();
        for item in metadata
            .allocations
            .iter()
            .filter(|item| item.space == space_id)
        {
            if item.flags != 0
                || item.group != root.group
                || item.span == 0
                || item.column != 0
                || item.copy > 1
            {
                return Err(INVALID.into());
            }
            let copies = allocations.entry(item.virtual_block).or_insert([None; 2]);
            if copies[item.copy as usize].replace(item).is_some() {
                return Err(INVALID.into());
            }
        }

        let last_slab = capacity.div_ceil(SLAB);
        let mut next = 0;
        let mut runs = BTreeMap::new();
        let mut physical = Vec::new();
        for (first, copies) in allocations {
            let [Some(left), Some(right)] = copies else {
                return Err(INVALID.into());
            };
            if first != next || left.span != right.span || left.member == right.member {
                return Err(INVALID.into());
            }
            next = first
                .checked_add(left.span)
                .filter(|&end| end <= last_slab)
                .ok_or(INVALID)?;
            let mut locations = Vec::new();
            let run_length = left.span.checked_mul(SLAB).ok_or(INVALID)?;
            for item in [left, right] {
                let limit = item.member_block.checked_add(item.span).ok_or(INVALID)?;
                physical.push((item.member, item.member_block, limit));
                let member = metadata.members.get(&item.member).ok_or(INVALID)?;
                if let Some(&reader) = slots.get(member) {
                    let extent = &self.members[reader].partition;
                    let offset = item
                        .member_block
                        .checked_add(RESERVED_SLABS)
                        .and_then(|block| block.checked_mul(SLAB))
                        .and_then(|offset| extent.start.checked_add(offset))
                        .filter(|offset| {
                            offset
                                .checked_add(run_length)
                                .is_some_and(|end| end <= extent.end)
                        })
                        .ok_or(INVALID)?;
                    locations.push(CopyLocation { reader, offset });
                }
            }
            if locations.is_empty() || (locations.len() == 1 && !allow_degraded) {
                return Err("缺少所需的存储空间镜像副本。".into());
            }
            runs.insert(
                first * SLAB,
                Run {
                    length: run_length,
                    copies: locations,
                },
            );
        }
        physical.sort_unstable();
        if next != last_slab
            || physical
                .windows(2)
                .any(|p| p[0].0 == p[1].0 && p[0].2 > p[1].1)
        {
            return Err(INVALID.into());
        }
        Ok(MirrorPlan { capacity, runs })
    }

    fn check_cache(&mut self, space_id: u64, plan: &MirrorPlan) -> Result<(), String> {
        let metadata = &self.members[0];
        let space = metadata.spaces.get(&space_id).ok_or(INVALID)?;
        let group = space.group;
        let guid = space.guid;
        let allocations: Vec<_> = metadata
            .allocations
            .iter()
            .filter(|item| item.group == group && item.flags != 0)
            .copied()
            .collect();
        if allocations.is_empty() {
            return Ok(());
        }
        let present: BTreeSet<_> = plan
            .runs
            .values()
            .flat_map(|run| run.copies.iter().map(|copy| copy.reader))
            .collect();
        let mut checked = BTreeSet::new();
        let mut reference = None;
        let mut header = [0; cache::SECTOR];
        for item in allocations {
            if item.flags != 4
                || item.span != 1
                || !metadata.allocation_owners.contains_key(&item.space)
            {
                return Err(cache::PENDING.into());
            }
            let member = metadata.members.get(&item.member).ok_or(INVALID)?;
            let Some((index, source)) = self
                .members
                .iter()
                .enumerate()
                .find(|(_, source)| &source.member == member)
            else {
                continue;
            };
            let range = &source.partition;
            let start = item
                .member_block
                .checked_add(RESERVED_SLABS)
                .and_then(|block| block.checked_mul(SLAB))
                .and_then(|offset| range.start.checked_add(offset))
                .filter(|start| start.checked_add(SLAB).is_some_and(|end| end <= range.end))
                .ok_or(INVALID)?;
            let reader = &mut self.readers[index];
            read_at(reader, start, &mut header)?;
            if &header[..8] == b"SPCACHE\0" {
                let mut directory = vec![0; cache::DIRECTORY_END];
                read_at(reader, start, &mut directory)?;
                let digest = cache::clean_digest(&directory, guid)?;
                if !checked.insert(index) || reference.is_some_and(|first| first != digest) {
                    return Err(cache::PENDING.into());
                }
                reference = Some(digest);
            } else if &header[..8] != b"SPACEDRT" && header.iter().any(|&byte| byte != 0) {
                return Err(cache::PENDING.into());
            }
        }
        if checked != present {
            return Err(cache::PENDING.into());
        }
        Ok(())
    }
}

pub struct SpaceReader<R> {
    readers: Vec<R>,
    plan: Plan,
    position: u64,
    comparison: Vec<u8>,
}

impl<R> Seek for SpaceReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.plan.capacity().checked_add_signed(offset),
        }
        .ok_or(ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}

impl<R: Read + Seek> Read for SpaceReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.plan.capacity() {
            return Ok(0);
        }
        let plan = match &self.plan {
            Plan::Mirror(plan) => plan,
            Plan::Thin(plan) => {
                let count = plan.read(&mut self.readers, self.position, output)?;
                self.position += count as u64;
                return Ok(count);
            }
        };
        let (start, run) = plan
            .runs
            .range(..=self.position)
            .next_back()
            .ok_or(ErrorKind::InvalidData)?;
        let within = self.position - start;
        let remaining = run
            .length
            .checked_sub(within)
            .filter(|&count| count != 0)
            .ok_or(ErrorKind::InvalidData)?;
        let count = (plan.capacity - self.position)
            .min(remaining)
            .min(MIB)
            .min(output.len() as u64) as usize;
        let bytes = &mut output[..count];
        let first = run.copies[0];
        let reader = &mut self.readers[first.reader];
        reader.seek(SeekFrom::Start(first.offset + within))?;
        reader.read_exact(bytes)?;
        for copy in &run.copies[1..] {
            self.comparison.resize(count, 0);
            let reader = &mut self.readers[copy.reader];
            reader.seek(SeekFrom::Start(copy.offset + within))?;
            reader.read_exact(&mut self.comparison)?;
            if self.comparison != bytes {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "存储空间镜像副本内容不一致。",
                ));
            }
        }
        self.position += count as u64;
        Ok(count)
    }
}

//! Read-only assembly for explicitly selected Windows dynamic disk members.
#[path = "ldm_metadata.rs"]
mod metadata;

use crate::virtual_raid;
use metadata::{guid_string, Component, Database, Extent, Guid, Id, Layout};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
};

pub const MAX_MEMBERS: usize = virtual_raid::MAX_MEMBERS;
const LDM_BLOCK: u64 = 512;
const INVALID: &str = "LDM 卷的组件、列或数据范围不一致。";

pub struct Member<R> {
    pub reader: R,
    pub sector_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VolumeKind {
    Simple,
    Spanned,
    Striped,
    Mirrored,
    Raid5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    Complete,
    Degraded,
    Missing,
    Unclean,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub kind: VolumeKind,
    pub availability: Availability,
    pub member_count: usize,
    pub missing_members: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub id: String,
    pub name: String,
    pub sequence: String,
    pub sector_size: u64,
    pub volumes: Vec<VolumeInfo>,
}

struct Mapping {
    slot: Option<usize>,
    start: u64,
    length: u64,
    position: u64,
}

enum Plan {
    Linear(Vec<Vec<Mapping>>),
    Raid(virtual_raid::Config, Vec<Mapping>),
    Unavailable,
}

pub struct Group<R> {
    readers: Vec<R>,
    inventory: Inventory,
    plans: Vec<Plan>,
    disk_count: usize,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimpleExtent {
    pub volume: String,
    pub name: String,
    pub offset: u64,
    pub size: u64,
}

struct Present {
    slot: usize,
    public: Range<u64>,
}

fn blocks(count: u64) -> Result<u64, String> {
    count
        .checked_mul(LDM_BLOCK)
        .filter(|&n| n <= i64::MAX as u64)
        .ok_or_else(|| INVALID.into())
}

fn mapping(
    extent: &Extent,
    present: &BTreeMap<Id, Present>,
    sector: u64,
) -> Result<Mapping, String> {
    let start = blocks(extent.start)?;
    let length = blocks(extent.blocks)?;
    let position = blocks(extent.position)?;
    if length == 0 || start % sector != 0 || length % sector != 0 || position % sector != 0 {
        return Err(INVALID.into());
    }
    let member = present.get(&extent.disk);
    let start = if let Some(member) = member {
        if start
            .checked_add(length)
            .filter(|&end| end <= member.public.end - member.public.start)
            .is_none()
        {
            return Err("LDM 数据范围超出成员公开区域。".into());
        }
        member.public.start + start
    } else {
        0
    };
    Ok(Mapping {
        slot: member.map(|member| member.slot),
        start,
        length,
        position,
    })
}

fn check_extents<'a>(
    tables: &'a metadata::Objects,
    present: &BTreeMap<Id, Present>,
    sector: u64,
) -> Result<BTreeMap<Id, Vec<&'a Extent>>, String> {
    let mut by_component = BTreeMap::<_, Vec<_>>::new();
    let mut by_disk = Vec::with_capacity(tables.extents.len());
    for extent in tables.extents.values() {
        if !tables.disks.contains_key(&extent.disk)
            || !tables.components.contains_key(&extent.component)
        {
            return Err(INVALID.into());
        }
        mapping(extent, present, sector)?;
        by_component
            .entry(extent.component)
            .or_default()
            .push(extent);
        by_disk.push((
            extent.disk,
            extent.start,
            extent.start.checked_add(extent.blocks).ok_or(INVALID)?,
        ));
    }
    by_disk.sort_unstable();
    if by_disk
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0 && pair[0].2 > pair[1].1)
    {
        return Err("LDM 对象表中同一成员的数据范围相互重叠。".into());
    }
    Ok(by_component)
}

fn linear(
    mut extents: Vec<Mapping>,
    component: &Component,
    size: u64,
) -> Result<Vec<Mapping>, String> {
    if component.columns != 0 || component.chunk != 0 {
        return Err(INVALID.into());
    }
    extents.sort_unstable_by_key(|extent| extent.position);
    let mut end = 0u64;
    for extent in &extents {
        if extent.position != end {
            return Err(INVALID.into());
        }
        end = end.checked_add(extent.length).ok_or(INVALID)?;
    }
    if end != size {
        return Err(INVALID.into());
    }
    Ok(extents)
}

fn stripe(
    component: &Component,
    extents: &[&Extent],
    present: &BTreeMap<Id, Present>,
    sector: u64,
    size: u64,
) -> Result<(virtual_raid::Config, Vec<Mapping>), String> {
    let parity = component.kind == Layout::Parity;
    let count = extents.len();
    let chunk = blocks(component.chunk)?;
    if count < 2 + usize::from(parity)
        || count > MAX_MEMBERS
        || component.columns != count as u64
        || !chunk.is_power_of_two()
        || !(sector..=1024 * 1024).contains(&chunk)
    {
        return Err(INVALID.into());
    }
    let mut columns = extents.to_vec();
    columns.sort_unstable_by_key(|entry| entry.column);
    let member_blocks = columns[0].blocks;
    let member_length = blocks(member_blocks)?;
    let mut disks = BTreeSet::new();
    if columns.iter().enumerate().any(|(i, extent)| {
        extent.column != i as u64
            || extent.blocks != member_blocks
            || extent.position != 0
            || !disks.insert(extent.disk)
    }) || member_length % chunk != 0
        || member_length.checked_mul((count - usize::from(parity)) as u64) != Some(size)
    {
        return Err(INVALID.into());
    }
    // Windows LDM RAID-5 assigns columns in left-symmetric order.
    let layout = if parity {
        virtual_raid::Layout::Raid5LeftSymmetric
    } else {
        virtual_raid::Layout::Raid0
    };
    let config = virtual_raid::Config {
        layout,
        sector,
        stripe: chunk,
    };
    let mapping = columns
        .into_iter()
        .map(|extent| mapping(extent, present, sector))
        .collect::<Result<_, _>>()?;
    Ok((config, mapping))
}

fn volume_plan(
    volume: &metadata::Volume,
    components: &[(&Component, Vec<&Extent>)],
    present: &BTreeMap<Id, Present>,
    sector: u64,
) -> Result<(VolumeInfo, Plan), String> {
    let size = blocks(volume.blocks)?;
    if volume.components != components.len() as u64 || components.is_empty() || size % sector != 0 {
        return Err(INVALID.into());
    }
    let members: BTreeSet<_> = components
        .iter()
        .flat_map(|(_, extents)| extents.iter().map(|extent| extent.disk))
        .collect();
    let missing = members
        .iter()
        .filter(|disk| !present.contains_key(disk))
        .count();
    let (first, extents) = &components[0];
    let (kind, availability, plan) = if first.kind == Layout::Linear {
        if volume.parity
            || components
                .iter()
                .any(|(component, _)| component.kind != Layout::Linear)
        {
            return Err(INVALID.into());
        }
        let kind = if components.len() > 1 {
            VolumeKind::Mirrored
        } else if members.len() > 1 {
            VolumeKind::Spanned
        } else {
            VolumeKind::Simple
        };
        let mut copies = Vec::new();
        let mut clean_present = false;
        for (component, extents) in components {
            let ranges = extents
                .iter()
                .map(|extent| mapping(extent, present, sector))
                .collect::<Result<_, _>>()?;
            let ranges = linear(ranges, component, size)?;
            if ranges.iter().all(|range| range.slot.is_some()) {
                clean_present = true;
                if component.clean && extents.iter().all(|extent| extent.clean) {
                    copies.push(ranges);
                }
            }
        }
        let state = if !volume.clean || copies.is_empty() && clean_present {
            Availability::Unclean
        } else if copies.is_empty() {
            Availability::Missing
        } else if copies.len() != components.len() {
            Availability::Degraded
        } else {
            Availability::Complete
        };
        (kind, state, Plan::Linear(copies))
    } else {
        if components.len() != 1 || volume.parity != (first.kind == Layout::Parity) {
            return Err(INVALID.into());
        }
        let (config, columns) = stripe(first, extents, present, sector, size)?;
        let kind = if volume.parity {
            VolumeKind::Raid5
        } else {
            VolumeKind::Striped
        };
        let state = if !volume.clean || !first.clean || extents.iter().any(|extent| !extent.clean) {
            Availability::Unclean
        } else if missing == 0 {
            Availability::Complete
        } else if missing == 1 && volume.parity {
            Availability::Degraded
        } else {
            Availability::Missing
        };
        (kind, state, Plan::Raid(config, columns))
    };
    let plan = if matches!(availability, Availability::Missing | Availability::Unclean) {
        Plan::Unavailable
    } else {
        plan
    };
    Ok((
        VolumeInfo {
            id: guid_string(volume.id),
            name: volume.name.clone(),
            size,
            kind,
            availability,
            member_count: members.len(),
            missing_members: missing,
        },
        plan,
    ))
}

/// Cross-check each selected member against the same clean, committed database.
/// Reopening a view is expected to re-run this check and compare the fingerprint.
pub fn open<R: Read + Seek>(
    mut members: Vec<Member<R>>,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Group<R>, String> {
    if !(1..=MAX_MEMBERS).contains(&members.len()) {
        return Err("请选择 1–64 个 LDM 磁盘或镜像。".into());
    }
    checkpoint()?;
    let first = &mut members[0];
    let basis = Database::read(&mut first.reader, first.sector_size)?;
    let mut present = BTreeMap::new();
    let mut disk_objects = BTreeMap::<Guid, Id>::new();
    for (&id, object) in &basis.objects.disks {
        if disk_objects.insert(object.guid, id).is_some() {
            return Err("LDM 数据库中的成员 GUID 重复。".into());
        }
    }
    let mut fingerprint = Sha256::new();
    fingerprint.update(basis.header.group);
    fingerprint.update(basis.digest);
    let sector = basis.header.sector;
    for (slot, member) in members.iter_mut().enumerate() {
        checkpoint()?;
        let inspected;
        let database = if slot == 0 {
            &basis
        } else {
            inspected = Database::read(&mut member.reader, member.sector_size)?;
            &inspected
        };
        if database.header.group != basis.header.group
            || database.digest != basis.digest
            || database.sequence != basis.sequence
            || database.header.sector != sector
        {
            return Err("LDM 成员不属于同一个扇区规格与提交版本的磁盘组。".into());
        }
        let id = *disk_objects
            .get(&database.header.disk)
            .ok_or("LDM 私有头所指成员不在数据库中。")?;
        let public = database.header.public.clone();
        fingerprint.update(database.header.disk);
        fingerprint.update(database.member_digest);
        fingerprint.update(public.start.to_le_bytes());
        fingerprint.update(public.end.to_le_bytes());
        if present.insert(id, Present { slot, public }).is_some() {
            return Err("LDM 成员被重复选择。".into());
        }
    }

    let mut extents = check_extents(&basis.objects, &present, sector)?;
    let mut components = BTreeMap::<_, Vec<_>>::new();
    for (id, component) in &basis.objects.components {
        let extents = extents.remove(id).ok_or(INVALID)?;
        if extents.len() as u64 != component.extents {
            return Err(INVALID.into());
        }
        components
            .entry(component.volume)
            .or_default()
            .push((component, extents));
    }
    let mut volumes = Vec::new();
    let mut plans = Vec::new();
    let mut volume_ids = HashSet::new();
    for (id, volume) in &basis.objects.volumes {
        if !volume_ids.insert(volume.id) {
            return Err("LDM 卷 GUID 重复。".into());
        }
        let (info, plan) = volume_plan(
            volume,
            &components.remove(id).ok_or(INVALID)?,
            &present,
            sector,
        )?;
        volumes.push(info);
        plans.push(plan);
    }
    if !components.is_empty() {
        return Err(INVALID.into());
    }
    Ok(Group {
        readers: members.into_iter().map(|member| member.reader).collect(),
        inventory: Inventory {
            id: guid_string(basis.header.group),
            name: basis.name,
            sequence: basis.sequence.to_string(),
            sector_size: sector,
            volumes,
        },
        plans,
        disk_count: disk_objects.len(),
        fingerprint: fingerprint.finalize().into(),
    })
}

impl<R: Read + Seek> Group<R> {
    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// In-place conversion must cover the entire one-disk group. A readable
    /// degraded view is insufficient evidence for a persistent layout change.
    pub fn standalone_simple_extents(&self) -> Result<Vec<SimpleExtent>, String> {
        let invalid = "转换要求完整的单盘动态磁盘组，且每个卷都是连续、干净的简单卷。";
        if self.disk_count != 1 || self.readers.len() != 1 || self.plans.is_empty() {
            return Err(invalid.into());
        }
        let mut extents = Vec::with_capacity(self.plans.len());
        for (info, plan) in self.inventory.volumes.iter().zip(&self.plans) {
            let Plan::Linear(copies) = plan else {
                return Err(invalid.into());
            };
            let [copy] = copies.as_slice() else {
                return Err(invalid.into());
            };
            if info.kind != VolumeKind::Simple || info.availability != Availability::Complete {
                return Err(invalid.into());
            }
            let start = copy.first().ok_or(invalid)?.start;
            let mut end = start;
            for extent in copy {
                if extent.slot != Some(0) || extent.start != end || extent.position != end - start {
                    return Err(invalid.into());
                }
                end = end.checked_add(extent.length).ok_or(invalid)?;
            }
            if end - start != info.size {
                return Err(invalid.into());
            }
            extents.push(SimpleExtent {
                volume: info.id.clone(),
                name: info.name.clone(),
                offset: start,
                size: info.size,
            });
        }
        extents.sort_unstable_by_key(|extent| extent.offset);
        Ok(extents)
    }

    pub fn into_volume(self, volume: &str) -> Result<VolumeReader<R>, String> {
        let (info, plan) = self
            .inventory
            .volumes
            .iter()
            .zip(self.plans)
            .find(|(item, _)| item.id == volume)
            .ok_or("LDM 卷不存在。")?;
        let backing = match plan {
            Plan::Unavailable => return Err("LDM 卷缺少成员或元数据需要恢复。".into()),
            Plan::Linear(copies) => Backing::Linear {
                readers: self.readers,
                copies,
                compare: Vec::new(),
            },
            Plan::Raid(config, columns) => {
                let mut readers: Vec<_> = self.readers.into_iter().map(Some).collect();
                let members = columns
                    .into_iter()
                    .map(|column| virtual_raid::Member {
                        reader: column.slot.and_then(|slot| readers[slot].take()),
                        range: virtual_raid::Range {
                            start: column.start,
                            size: column.length,
                        },
                    })
                    .collect();
                let mut reader = virtual_raid::Reader::open(config, members)?;
                if info.kind == VolumeKind::Raid5 && info.availability == Availability::Complete {
                    reader
                        .check_parity_samples()
                        .map_err(|error| format!("LDM RAID-5 校验不一致：{error}"))?;
                }
                Backing::Raid(reader)
            }
        };
        Ok(VolumeReader {
            backing,
            size: info.size,
            position: 0,
        })
    }
}

enum Backing<R> {
    Linear {
        readers: Vec<R>,
        copies: Vec<Vec<Mapping>>,
        compare: Vec<u8>,
    },
    Raid(virtual_raid::Reader<R>),
}

pub struct VolumeReader<R> {
    backing: Backing<R>,
    size: u64,
    position: u64,
}

impl<R: Read + Seek> Read for VolumeReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let limit = out
            .len()
            .min(self.size.saturating_sub(self.position).min(65536) as usize);
        if limit == 0 {
            return Ok(0);
        }
        let count = match &mut self.backing {
            Backing::Raid(reader) => {
                reader.seek(SeekFrom::Start(self.position))?;
                reader.read(&mut out[..limit])?
            }
            Backing::Linear {
                readers,
                copies,
                compare,
            } => {
                let extents = copies.iter().map(|copy| {
                    let index = copy
                        .partition_point(|range| range.position + range.length <= self.position);
                    &copy[index]
                });
                let count = extents.clone().fold(limit, |n, range| {
                    (n as u64).min(range.position + range.length - self.position) as usize
                });
                let out = &mut out[..count];
                for (index, range) in extents.enumerate() {
                    let reader = &mut readers[range.slot.unwrap()];
                    reader.seek(SeekFrom::Start(
                        range.start + self.position - range.position,
                    ))?;
                    if index == 0 {
                        reader.read_exact(out)?;
                    } else {
                        compare.resize(count, 0);
                        reader.read_exact(compare)?;
                        if compare != out {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "LDM 镜像副本内容不一致。",
                            ));
                        }
                    }
                }
                count
            }
        };
        self.position += count as u64;
        Ok(count)
    }
}

impl<R> Seek for VolumeReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = position
            .filter(|&position| position <= self.size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "LDM 读取位置无效。"))?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn simple_volume_conversion_accepts_physically_adjacent_extents_in_logical_order() {
        let volume = metadata::Volume {
            name: "Extended on one disk".into(),
            id: [1; 16],
            components: 1,
            blocks: 6,
            parity: false,
            clean: true,
        };
        let component = Component {
            volume: 1,
            kind: Layout::Linear,
            extents: 2,
            chunk: 0,
            columns: 0,
            clean: true,
        };
        let first = Extent {
            component: 2,
            disk: 3,
            start: 2,
            position: 0,
            blocks: 4,
            column: 0,
            clean: true,
        };
        let present = BTreeMap::from([(
            3,
            Present {
                slot: 0,
                public: 16384..131072,
            },
        )]);
        for (second_start, convertible) in [(6, true), (7, false), (0, false)] {
            let second = Extent {
                start: second_start,
                position: 4,
                blocks: 2,
                ..first.clone()
            };
            let (info, plan) = volume_plan(
                &volume,
                &[(&component, vec![&first, &second])],
                &present,
                512,
            )
            .unwrap();
            assert_eq!(info.kind, VolumeKind::Simple);
            let group = Group {
                readers: vec![Cursor::new(vec![0; 131072])],
                inventory: Inventory {
                    id: String::new(),
                    name: String::new(),
                    sequence: "1".into(),
                    sector_size: 512,
                    volumes: vec![info],
                },
                plans: vec![plan],
                disk_count: 1,
                fingerprint: [0; 32],
            };
            let extents = group.standalone_simple_extents();
            if convertible {
                let extents = extents.unwrap();
                assert_eq!((extents[0].offset, extents[0].size), (17408, 3072));
            } else {
                assert!(extents.is_err());
            }
        }
    }

    #[test]
    fn linear_reader_stops_at_extent_and_volume_boundaries() {
        let mut volume = VolumeReader {
            backing: Backing::Linear {
                readers: vec![Cursor::new(b"-AB+".to_vec()), Cursor::new(b"-CD+".to_vec())],
                copies: vec![vec![
                    Mapping {
                        slot: Some(0),
                        start: 1,
                        length: 2,
                        position: 0,
                    },
                    Mapping {
                        slot: Some(1),
                        start: 1,
                        length: 2,
                        position: 2,
                    },
                ]],
                compare: Vec::new(),
            },
            size: 4,
            position: 0,
        };
        assert!(volume.seek(SeekFrom::End(1)).is_err());
        volume.seek(SeekFrom::Start(1)).unwrap();
        let mut bytes = Vec::new();
        volume.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"BCD");
    }

    #[test]
    fn mirror_rejects_divergent_readable_copies() {
        let mut volume = VolumeReader {
            backing: Backing::Linear {
                readers: vec![Cursor::new(b"ABC".to_vec()), Cursor::new(b"ABD".to_vec())],
                copies: (0..2)
                    .map(|slot| {
                        vec![Mapping {
                            slot: Some(slot),
                            start: 0,
                            length: 3,
                            position: 0,
                        }]
                    })
                    .collect(),
                compare: Vec::new(),
            },
            size: 3,
            position: 0,
        };
        let mut prefix = [0; 2];
        volume.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"AB");
        let error = volume.read_exact(&mut [0]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(volume.stream_position().unwrap(), 2);
    }
}

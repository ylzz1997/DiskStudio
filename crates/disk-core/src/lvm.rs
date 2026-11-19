//! Explicit, read-only LVM2 reconstruction from bounded member streams.
//! Format reference: LVM2 label, format_text layout and striped segment definitions.
#[path = "lvm_raid.rs"]
mod raid;
#[path = "lvm_snapshot.rs"]
mod snapshot;
#[path = "lvm_text.rs"]
mod text;
#[path = "lvm_thin.rs"]
mod thin;
#[path = "lvm_thin_map.rs"]
mod thin_map;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    sync::Arc,
};
use text::{Record, Value};

const SECTOR: u64 = 512;
const MDA_MAGIC: &[u8; 16] = b" LVM2 x[5A%r0N*>";
pub const MAX_MEMBERS: usize = 64;
const MAX_METADATA: u64 = 4 * super::MIB;
const BAD_FIELDS: &str = "LVM2 卷组字段缺失或无效。";
const BAD_MAP: &str = "LVM2 逻辑卷段重叠、越界或不连续。";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub profile: String,
    pub readable: bool,
    pub degraded_raid: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub name: String,
    pub sequence: u64,
    pub member_count: usize,
    pub supplied_members: usize,
    pub allow_degraded_raid: bool,
    pub extent_size: u64,
    pub volumes: Vec<VolumeInfo>,
}

struct Physical {
    id: String,
    size: u64,
    data: Range<u64>,
    claims: Vec<(String, Record)>,
    fingerprint: [u8; 32],
}

#[derive(Clone)]
struct Extents {
    member: Option<usize>,
    count: u64,
    base: u64,
}

struct Stripe {
    member: Option<usize>,
    offset: u64,
    pv: String,
}

struct Segment {
    logical: Range<u64>,
    unit: u64,
    mapping: Mapping,
}

enum Mapping {
    Stripes(Vec<Stripe>),
    Raid(raid::Array),
    Thin(thin::Image),
    Snapshot(snapshot::Image),
}

struct Volume {
    segments: Vec<Segment>,
    size: u64,
    degraded_raid: bool,
}

struct Candidate {
    info: VolumeInfo,
    mapped: Option<Arc<Volume>>,
    visible: bool,
}

impl Candidate {
    fn bind(&mut self, profile: &str, result: Result<Segment, String>) {
        self.info.profile = profile.into();
        match result {
            Ok(segment) => {
                let degraded_raid = match &segment.mapping {
                    Mapping::Raid(array) => array.degraded(),
                    Mapping::Thin(image) => image.degraded_raid(),
                    Mapping::Snapshot(image) => image.degraded_raid(),
                    Mapping::Stripes(_) => false,
                };
                self.info.readable = true;
                self.info.degraded_raid = degraded_raid;
                self.info.reason = None;
                self.mapped = Some(Arc::new(Volume {
                    size: self.info.size,
                    segments: vec![segment],
                    degraded_raid,
                }));
            }
            Err(reason) => {
                self.info.readable = false;
                self.info.degraded_raid = false;
                self.info.reason = Some(reason);
                self.mapped = None;
            }
        }
    }
}

pub struct Group<R> {
    readers: Vec<R>,
    inventory: Inventory,
    fingerprint: [u8; 32],
    volumes: BTreeMap<String, Arc<Volume>>,
}

impl<R: Read + Seek> Group<R> {
    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn reader(self, volume: &str) -> Result<LogicalReader<R>, String> {
        let mut volumes = self.volumes;
        let volume = volumes.remove(volume).ok_or("LVM2 逻辑卷不可读取。")?;
        Ok(LogicalReader {
            readers: self.readers,
            volume,
            position: 0,
            scratch: Vec::new(),
        })
    }
}

fn crc(bytes: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new_with_initial(!0xf597a6cf);
    h.update(bytes);
    !h.finalize()
}

fn u32le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64le(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn read_at(reader: &mut (impl Read + Seek), at: u64, out: &mut [u8]) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(at))
        .and_then(|_| reader.read_exact(out))
        .map_err(|e| format!("LVM2 读取失败：{e}"))
}

fn uuid(value: &str) -> Result<String, String> {
    let id = value.replace('-', "");
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("LVM2 标识无效。".into());
    }
    Ok(id)
}

fn supported_metadata_flags(record: &Record) -> bool {
    ["status", "flags"].into_iter().all(|key| {
        record.get(key).is_none_or(|value| {
            value.list().is_some_and(|items| {
                items.iter().all(|item| {
                    matches!(
                        item.text(),
                        Some(
                            "READ"
                                | "WRITE"
                                | "VISIBLE"
                                | "FIXED_MINOR"
                                | "WRITE_LOCKED"
                                | "ACTIVATION_SKIP"
                                | "NOAUTOACTIVATE"
                                | "WRITEMOSTLY"
                        )
                    )
                })
            })
        })
    })
}

fn valid_name(name: &str) -> bool {
    !name.starts_with('-')
        && !matches!(name, "." | "..")
        && (1..128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-' | b'.'))
}

fn locations(label: &[u8], pos: &mut usize, limit: usize) -> Result<Vec<(u64, u64)>, String> {
    let mut entries = Vec::new();
    while let Some(entry) = label.get(*pos..*pos + 16) {
        *pos += 16;
        let (offset, size) = (u64le(entry, 0), u64le(entry, 8));
        if offset == 0 && size == 0 {
            return Ok(entries);
        }
        if offset < SECTOR * 4 || offset % SECTOR != 0 || entries.len() >= limit {
            break;
        }
        entries.push((offset, size));
    }
    Err("LVM2 物理卷区域描述无效。".into())
}

fn read_mda(
    reader: &mut (impl Read + Seek),
    area: &Range<u64>,
    hash: &mut Sha256,
) -> Result<Option<(String, Record)>, String> {
    let mut header = [0u8; 512];
    read_at(reader, area.start, &mut header)?;
    hash.update(header);
    let size = area.end - area.start;
    if crc(&header[4..]) != u32le(&header, 0)
        || &header[4..20] != MDA_MAGIC
        || u32le(&header, 20) != 1
        || u64le(&header, 24) != area.start
        || u64le(&header, 32) != size
    {
        return Err("LVM2 元数据区头部校验失败。".into());
    }
    let flags = u32le(&header, 60);
    if flags == 1 {
        return Ok(None);
    }
    let offset = u64le(&header, 40);
    let count = u64le(&header, 48);
    if flags != 0 {
        return Err("LVM2 元数据区标志不受支持。".into());
    }
    if offset == 0 && count == 0 {
        return Ok(None);
    }
    if u64le(&header, 64) != 0 || u64le(&header, 72) != 0 {
        return Err("LVM2 存在尚未提交的元数据。".into());
    }
    if !(SECTOR..size).contains(&offset)
        || offset % SECTOR != 0
        || !(1..=MAX_METADATA.min(size - SECTOR)).contains(&count)
    {
        return Err("LVM2 元数据文本区域越界。".into());
    }
    let mut bytes = vec![0; count as usize];
    let first = count.min(size - offset) as usize;
    read_at(reader, area.start + offset, &mut bytes[..first])?;
    if first < bytes.len() {
        read_at(reader, area.start + SECTOR, &mut bytes[first..])?;
    }
    if crc(&bytes) != u32le(&header, 56) {
        return Err("LVM2 卷组文本校验失败。".into());
    }
    hash.update(bytes.as_slice());
    let record = text::parse(&bytes)?;
    if record.get("contents").and_then(Value::text) != Some("Text Format Volume Group")
        || record.get("version").and_then(Value::number) != Some(1)
    {
        return Err("LVM2 元数据格式不受支持。".into());
    }
    let mut sections = record.into_iter().filter_map(|(name, value)| match value {
        Value::Section(fields) => Some((name, fields)),
        _ => None,
    });
    let (name, fields) = sections.next().ok_or(BAD_FIELDS)?;
    if !valid_name(&name) || sections.next().is_some() {
        return Err(BAD_FIELDS.into());
    }
    Ok(Some((name, fields)))
}

fn physical(
    reader: &mut (impl Read + Seek),
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<Physical, String> {
    checkpoint()?;
    let length = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let mut raw = [0u8; 2048];
    if length < raw.len() as u64 {
        return Err("LVM2 来源范围过小。".into());
    }
    read_at(reader, 0, &mut raw)?;
    let mut found = raw
        .chunks_exact(SECTOR as usize)
        .enumerate()
        .filter(|(_, sector)| &sector[..8] == b"LABELONE" && &sector[24..32] == b"LVM2 001");
    let (sector, label) = found.next().ok_or("未找到 LVM2 物理卷标签。")?;
    if found.next().is_some()
        || u64le(label, 8) != sector as u64
        || crc(&label[20..]) != u32le(label, 16)
    {
        return Err("LVM2 物理卷标签校验失败或有多个标签。".into());
    }
    let header = u32le(label, 20) as usize;
    let body = label
        .get(header..)
        .filter(|b| header >= 32 && b.len() >= 56)
        .ok_or("LVM2 物理卷标签偏移无效。")?;
    let id = uuid(std::str::from_utf8(&body[..32]).map_err(|_| BAD_FIELDS)?)?;
    if id.len() != body[..32].len() {
        return Err(BAD_FIELDS.into());
    }
    let size = u64le(body, 32);
    if size > length || size % SECTOR != 0 {
        return Err("LVM2 物理卷大小超出所选范围。".into());
    }
    let mut pos = 40;
    let data = locations(body, &mut pos, 1)?;
    let [(data_start, data_size)] = data.as_slice() else {
        return Err("LVM2 物理卷需要一段连续的数据区域。".into());
    };
    let mut areas = Vec::new();
    for (offset, count) in locations(body, &mut pos, 2)? {
        let end = offset
            .checked_add(count)
            .filter(|&end| end <= size)
            .ok_or("LVM2 元数据区越界。")?;
        if count <= SECTOR || count % SECTOR != 0 {
            return Err("LVM2 元数据区大小无效。".into());
        }
        areas.push(offset..end);
    }
    let data_end = if *data_size == 0 {
        areas
            .iter()
            .filter(|area| area.start > *data_start)
            .map(|area| area.start)
            .min()
            .unwrap_or(size)
    } else {
        data_start.checked_add(*data_size).ok_or(BAD_MAP)?
    };
    if *data_start >= data_end || data_end > size || data_end % SECTOR != 0 {
        return Err("LVM2 数据区域无效。".into());
    }
    let data = *data_start..data_end;
    let mut ranges = areas.clone();
    ranges.push(data.clone());
    ranges.sort_by_key(|area| area.start);
    if ranges.windows(2).any(|w| w[0].end > w[1].start) {
        return Err("LVM2 数据区域和元数据区重叠。".into());
    }
    let mut hash = Sha256::new();
    hash.update(label);
    let mut claims = Vec::new();
    for area in areas {
        checkpoint()?;
        claims.extend(read_mda(reader, &area, &mut hash)?);
    }
    Ok(Physical {
        id,
        size,
        data,
        claims,
        fingerprint: hash.finalize().into(),
    })
}

fn field<'a, T>(
    record: &'a Record,
    name: &str,
    kind: impl FnOnce(&'a Value) -> Option<T>,
) -> Result<T, String> {
    record
        .get(name)
        .and_then(kind)
        .ok_or_else(|| format!("{BAD_FIELDS} {name}"))
}

fn bytes(sectors: u64) -> Result<u64, String> {
    sectors.checked_mul(SECTOR).ok_or_else(|| BAD_MAP.into())
}

fn pv_map(
    group: &Record,
    extent: u64,
    sources: &[Physical],
) -> Result<BTreeMap<String, Extents>, String> {
    let records = field(group, "physical_volumes", Value::section)?;
    if records.is_empty() || records.len() > MAX_MEMBERS {
        return Err("LVM2 物理卷数量无效。".into());
    }
    let mut ids = HashSet::new();
    let mut paths = BTreeMap::new();
    for (alias, value) in records {
        let pv = value.section().ok_or(BAD_FIELDS)?;
        let id = uuid(field(pv, "id", Value::text)?)?;
        if !ids.insert(id.clone()) {
            return Err("LVM2 物理卷标识重复。".into());
        }
        let base = bytes(field(pv, "pe_start", Value::number)?)?;
        let count = field(pv, "pe_count", Value::number)?;
        let declared = bytes(field(pv, "dev_size", Value::number)?)?;
        let end = count
            .checked_mul(extent)
            .and_then(|len| base.checked_add(len))
            .filter(|&end| end <= declared)
            .ok_or(BAD_MAP)?;
        let member = sources.iter().position(|source| source.id == id);
        if base == 0
            || member.is_some_and(|index| {
                let source = &sources[index];
                declared > source.size || base != source.data.start || end > source.data.end
            })
        {
            return Err("LVM2 物理范围与卷组记录不一致。".into());
        }
        paths.insert(
            alias.clone(),
            Extents {
                member: member.filter(|_| {
                    !["status", "flags"].iter().any(|key| {
                        pv.get(*key).and_then(Value::list).is_some_and(|items| {
                            items.iter().any(|item| item.text() == Some("MISSING"))
                        })
                    })
                }),
                count,
                base,
            },
        );
    }
    if sources.iter().any(|pv| !ids.contains(&pv.id)) {
        return Err("选择的 LVM2 物理卷不属于同一卷组。".into());
    }
    Ok(paths)
}

fn stripe_segment(
    record: &Record,
    logical: Range<u64>,
    paths: &BTreeMap<String, Extents>,
    extent: u64,
) -> Result<Segment, String> {
    let lanes = usize::try_from(field(record, "stripe_count", Value::number)?)
        .ok()
        .filter(|count| (1..=MAX_MEMBERS).contains(count))
        .ok_or(BAD_MAP)?;
    let length = logical.end - logical.start;
    let list = field(record, "stripes", Value::list)?;
    if list.len() != lanes * 2 || length / extent % lanes as u64 != 0 {
        return Err(BAD_MAP.into());
    }
    let unit = if lanes == 1 {
        length
    } else {
        let unit = bytes(field(record, "stripe_size", Value::number)?)?;
        if unit == 0 || !unit.is_power_of_two() || extent % unit != 0 {
            return Err("LVM2 条带大小无效。".into());
        }
        unit
    };
    let per_lane = length / lanes as u64 / extent;
    let mut stripes = Vec::with_capacity(lanes);
    for pair in list.chunks_exact(2) {
        let pv = pair[0].text().ok_or(BAD_FIELDS)?;
        let start = pair[1].number().ok_or(BAD_FIELDS)?;
        let bounds = paths.get(pv).ok_or(BAD_FIELDS)?;
        start
            .checked_add(per_lane)
            .filter(|&end| end <= bounds.count)
            .ok_or(BAD_MAP)?;
        let offset = bounds.base + start * extent;
        stripes.push(Stripe {
            member: bounds.member,
            offset,
            pv: pv.to_owned(),
        });
    }
    Ok(Segment {
        logical,
        unit,
        mapping: Mapping::Stripes(stripes),
    })
}

type Allocation = (String, Range<u64>);

fn volume(
    name: &str,
    record: &Record,
    paths: &BTreeMap<String, Extents>,
    extent: u64,
    allocations: &mut Vec<Allocation>,
) -> Result<Candidate, String> {
    let id = uuid(field(record, "id", Value::text)?)?;
    let statuses = field(record, "status", Value::list)?;
    let mut raw_segments = Vec::new();
    for (key, value) in record {
        if key
            .strip_prefix("segment")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            let section = value.section().ok_or(BAD_FIELDS)?;
            let start = field(section, "start_extent", Value::number)?;
            let count = field(section, "extent_count", Value::number)?;
            raw_segments.push((start, count, section));
        }
    }
    if raw_segments.is_empty()
        || raw_segments.len() as u64 != field(record, "segment_count", Value::number)?
    {
        return Err(BAD_MAP.into());
    }
    raw_segments.sort_by_key(|segment| segment.0);
    let mut end = 0;
    let mut segments = Vec::new();
    let mut unsupported = !statuses.iter().any(|v| v.text() == Some("READ"));
    for (start, count, record) in raw_segments {
        if count == 0 || start != end {
            return Err(BAD_MAP.into());
        }
        end = start.checked_add(count).ok_or(BAD_MAP)?;
        let lo = start.checked_mul(extent).ok_or(BAD_MAP)?;
        let hi = end
            .checked_mul(extent)
            .filter(|&end| end <= i64::MAX as u64)
            .ok_or(BAD_MAP)?;
        match field(record, "type", Value::text)? {
            "striped" => {
                let segment = stripe_segment(record, lo..hi, paths, extent)?;
                if let Mapping::Stripes(lanes) = &segment.mapping {
                    for stripe in lanes {
                        let len = (hi - lo) / lanes.len() as u64;
                        allocations.push((stripe.pv.clone(), stripe.offset..stripe.offset + len));
                    }
                }
                segments.push(segment);
            }
            _ => unsupported = true,
        }
    }
    let size = end * extent;
    let mut missing = false;
    let mut linear = false;
    let mut striped = false;
    for segment in &segments {
        if let Mapping::Stripes(lanes) = &segment.mapping {
            missing |= lanes.iter().any(|lane| lane.member.is_none());
            linear |= lanes.len() == 1;
            striped |= lanes.len() > 1;
        }
    }
    let profile = match (linear, striped, unsupported) {
        (_, _, true) => "unsupported",
        (true, true, false) => "mixed",
        (false, true, false) => "striped",
        _ => "linear",
    };
    let reason = if unsupported {
        Some("LVM2 逻辑卷包含不受支持的段类型或状态。".into())
    } else if missing {
        Some("LVM2 逻辑卷需要未选择的物理卷。".into())
    } else {
        None
    };
    let readable = reason.is_none();
    let info = VolumeInfo {
        id,
        name: name.into(),
        size,
        profile: profile.into(),
        readable,
        degraded_raid: false,
        reason,
    };
    Ok(Candidate {
        info,
        mapped: readable.then(|| {
            Arc::new(Volume {
                size,
                segments,
                degraded_raid: false,
            })
        }),
        visible: statuses.iter().any(|v| v.text() == Some("VISIBLE")),
    })
}

/// The caller supplies only explicit, read-only, bounded physical volumes.
/// Device hints in the text metadata are ignored.
pub fn open<R: Read + Seek>(
    readers: Vec<R>,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Group<R>, String> {
    open_with_raid_policy(readers, false, checkpoint)
}

/// With explicit permission, reconstruct known absences in clean redundant arrays.
pub fn open_with_raid_policy<R: Read + Seek>(
    mut readers: Vec<R>,
    allow_degraded_raid: bool,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Group<R>, String> {
    if readers.is_empty() || readers.len() > MAX_MEMBERS {
        return Err("LVM2 物理卷数量无效。".into());
    }
    let mut sources = Vec::with_capacity(readers.len());
    let mut agreed = None;
    let mut ids = HashSet::new();
    for reader in &mut readers {
        let mut source = physical(reader, &mut checkpoint)?;
        if !ids.insert(source.id.clone()) {
            return Err("LVM2 物理卷标识重复。".into());
        }
        for claim in source.claims.drain(..) {
            match &agreed {
                Some(committed) if committed != &claim => {
                    return Err("LVM2 卷组元数据副本不一致。".into());
                }
                None => agreed = Some(claim),
                _ => {}
            }
        }
        sources.push(source);
    }
    let (name, fields) = agreed.ok_or("所选物理卷没有可用的 LVM2 卷组元数据。")?;
    let fields = &fields;
    uuid(field(fields, "id", Value::text)?)?;
    let sequence = field(fields, "seqno", Value::number)?;
    let extent = bytes(field(fields, "extent_size", Value::number)?)?;
    if sequence == 0
        || !extent.is_power_of_two()
        || !(SECTOR..=16 * 1024 * super::MIB).contains(&extent)
    {
        return Err(BAD_FIELDS.into());
    }
    let paths = pv_map(fields, extent, &sources)?;
    let mut map = BTreeMap::new();
    let mut inventory = Inventory {
        name,
        sequence,
        member_count: paths.len(),
        supplied_members: paths.values().filter(|pv| pv.member.is_some()).count(),
        allow_degraded_raid,
        extent_size: extent,
        volumes: Vec::new(),
    };
    let mut hash = Sha256::new();
    hash.update(b"DiskStudio LVM2 volume group");
    hash.update([u8::from(allow_degraded_raid)]);
    for source in sources {
        hash.update(source.fingerprint);
    }
    let mut allocations = Vec::new();
    let mut lv_ids = HashSet::new();
    if let Some(record) = fields.get("logical_volumes") {
        let volumes = record.section().ok_or(BAD_FIELDS)?;
        if volumes.len() > 256 {
            return Err("LVM2 逻辑卷数量超出限制。".into());
        }
        let mut candidates = BTreeMap::new();
        for (name, value) in volumes {
            checkpoint()?;
            if !valid_name(name) {
                return Err(BAD_FIELDS.into());
            }
            let candidate = volume(
                name,
                value.section().ok_or(BAD_FIELDS)?,
                &paths,
                extent,
                &mut allocations,
            )?;
            if !lv_ids.insert(candidate.info.id.clone()) {
                return Err("LVM2 逻辑卷标识重复。".into());
            }
            candidates.insert(name.as_str(), candidate);
        }
        validate_allocations(&mut allocations)?;
        let raid_volumes = {
            let mut bindings = raid::Bindings::new(
                &mut readers,
                &candidates,
                volumes,
                extent,
                allow_degraded_raid,
            );
            let mut resolved = Vec::new();
            for (&name, candidate) in &candidates {
                if candidate.mapped.is_none() {
                    if let Some(kind) = bindings.kind(name)? {
                        let result =
                            bindings.bind(name, candidate.info.size, kind, &mut checkpoint)?;
                        resolved.push((name, kind.profile(), result));
                    }
                }
            }
            resolved
        };
        for (name, profile, result) in raid_volumes {
            let result = result.map(|(segment, proof)| {
                hash.update(proof);
                segment
            });
            candidates
                .get_mut(name)
                .ok_or(BAD_FIELDS)?
                .bind(profile, result);
        }
        let snapshots = snapshot::Catalog::new(volumes);
        thin::bind(
            &mut readers,
            &mut candidates,
            volumes,
            &snapshots,
            &mut hash,
            &mut checkpoint,
        )?;
        snapshots.bind(
            &mut readers,
            &mut candidates,
            volumes,
            &mut hash,
            &mut checkpoint,
        )?;
        for (_, candidate) in candidates {
            if !candidate.visible {
                continue;
            }
            checkpoint()?;
            if let Some(mapped) = candidate.mapped {
                map.insert(candidate.info.id.clone(), mapped);
            }
            inventory.volumes.push(candidate.info);
        }
    }
    Ok(Group {
        readers,
        inventory,
        fingerprint: hash.finalize().into(),
        volumes: map,
    })
}

fn validate_allocations(allocations: &mut [Allocation]) -> Result<(), String> {
    allocations.sort_unstable_by(|(a, x), (b, y)| a.cmp(b).then(x.start.cmp(&y.start)));
    if allocations
        .windows(2)
        .any(|w| w[0].0 == w[1].0 && w[0].1.end > w[1].1.start)
    {
        return Err(BAD_MAP.into());
    }
    Ok(())
}

pub struct LogicalReader<R> {
    readers: Vec<R>,
    volume: Arc<Volume>,
    position: u64,
    scratch: Vec<u8>,
}

struct Window {
    member: usize,
    offset: u64,
    available: u64,
}

impl Window {
    fn read(&self, readers: &mut [impl Read + Seek], out: &mut [u8]) -> io::Result<()> {
        let reader = readers
            .get_mut(self.member)
            .ok_or_else(|| io::Error::other("LVM2 source member is absent"))?;
        reader.seek(SeekFrom::Start(self.offset))?;
        reader.read_exact(out)
    }
}

impl Segment {
    fn window(&self, position: u64) -> io::Result<Window> {
        let Mapping::Stripes(lanes) = &self.mapping else {
            return Err(io::Error::other("Expected a plain LVM2 component"));
        };
        let relative = position - self.logical.start;
        let chunk = relative / self.unit;
        let within = relative % self.unit;
        let width = lanes.len() as u64;
        let lane = &lanes[(chunk % width) as usize];
        Ok(Window {
            member: lane
                .member
                .ok_or_else(|| io::Error::other("LVM2 stripe member is absent"))?,
            offset: lane.offset + (chunk / width) * self.unit + within,
            available: (self.unit - within).min(self.logical.end - position),
        })
    }
}

impl Volume {
    fn segment_at(&self, position: u64) -> io::Result<&Segment> {
        let index = self.segments.partition_point(|s| s.logical.end <= position);
        self.segments
            .get(index)
            .filter(|segment| segment.logical.contains(&position))
            .ok_or_else(|| io::Error::other("LVM2 read outside logical volume"))
    }

    fn read_plain(
        &self,
        readers: &mut [impl Read + Seek],
        position: u64,
        out: &mut [u8],
    ) -> Result<(), String> {
        self.read_exact_at(readers, position, out, &mut Vec::new())
            .map_err(|e| e.to_string())
    }

    fn read_exact_at(
        &self,
        readers: &mut [impl Read + Seek],
        mut position: u64,
        mut out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> io::Result<()> {
        while !out.is_empty() {
            let amount = self.read_at_most(readers, position, out, scratch)?;
            out = &mut out[amount..];
            position += amount as u64;
        }
        Ok(())
    }

    fn read_at_most(
        &self,
        readers: &mut [impl Read + Seek],
        position: u64,
        out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> io::Result<usize> {
        let segment = self.segment_at(position)?;
        let length = (out.len() as u64).min(segment.logical.end - position);
        let relative = position - segment.logical.start;
        let amount = match &segment.mapping {
            Mapping::Stripes(_) => {
                let window = segment.window(position)?;
                let amount = length.min(window.available) as usize;
                window.read(readers, &mut out[..amount])?;
                amount
            }
            Mapping::Raid(array) => {
                array.read(readers, relative, &mut out[..length as usize], scratch)?
            }
            Mapping::Thin(image) => read_backing(
                readers,
                image.location(relative, segment.unit),
                &mut out[..length as usize],
                scratch,
            )?,
            Mapping::Snapshot(image) => read_backing(
                readers,
                image.location(relative, segment.unit),
                &mut out[..length as usize],
                scratch,
            )?,
        };
        Ok(amount)
    }
}

fn read_backing(
    readers: &mut [impl Read + Seek],
    (backing, offset, limit): (Option<&Volume>, u64, u64),
    out: &mut [u8],
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let amount = (out.len() as u64).min(limit) as usize;
    if let Some(backing) = backing {
        backing.read_at_most(readers, offset, &mut out[..amount], scratch)
    } else {
        out[..amount].fill(0);
        Ok(amount)
    }
}

impl<R: Seek> Seek for LogicalReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.volume.size) + i128::from(delta),
        };
        if !(0..=i128::from(self.volume.size)).contains(&next) {
            return Err(io::Error::other("LVM2 seek outside logical volume"));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

impl<R: Read + Seek> Read for LogicalReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.position == self.volume.size {
            return Ok(0);
        }
        let amount =
            self.volume
                .read_at_most(&mut self.readers, self.position, out, &mut self.scratch)?;
        self.position += amount as u64;
        Ok(amount)
    }
}

#[cfg(test)]
#[path = "lvm_tests.rs"]
mod tests;

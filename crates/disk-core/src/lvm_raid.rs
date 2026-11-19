//! LVM2 RAID component bindings and clean dm-raid membership checks.
use super::*;
#[path = "lvm_raid_parity.rs"]
mod parity;

const COMPONENTS: &str = "LVM2 RAID 成员映射缺失、重复或不受支持。";
const STATE: &str = "LVM2 RAID 成员未同步、事件代次不一致或元数据无效。";
const MISSING: &str = "LVM2 RAID 逻辑卷需要选齐物理卷。";
const INSUFFICIENT: &str = "LVM2 RAID 缺少的成员超出可重建范围。";
const BITMAP_OFFSET: u64 = 4096;
const BITMAP_HEADER: usize = 256;
const MAX_BITMAP: u64 = 256 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Stripe { metadata: bool },
    Mirror,
    Raid10(Raid10),
    Parity { level: u32, layout: u32 },
}

#[derive(Clone, Copy)]
pub(super) enum Raid10 {
    Near,
    Far,
    Offset,
}

const FAR_SETS: u32 = 1 << 18;

impl Kind {
    fn from_name(name: &str) -> Option<Self> {
        let (level, layout) = match name {
            "raid0" => return Some(Self::Stripe { metadata: false }),
            "raid0_meta" => return Some(Self::Stripe { metadata: true }),
            "raid1" => return Some(Self::Mirror),
            "raid10" | "raid10_near" => return Some(Self::Raid10(Raid10::Near)),
            "raid10_far" => return Some(Self::Raid10(Raid10::Far)),
            "raid10_offset" => return Some(Self::Raid10(Raid10::Offset)),
            // dm-raid implements the LVM raid4 target as RAID5 with parity first.
            "raid4" => (5, 4),
            "raid5_la" => (5, 0),
            "raid5_ra" => (5, 1),
            "raid5" | "raid5_ls" => (5, 2),
            "raid5_rs" => (5, 3),
            "raid5_n" => (5, 5),
            "raid6" | "raid6_zr" => (6, 8),
            "raid6_nr" => (6, 9),
            "raid6_nc" => (6, 10),
            "raid6_la_6" => (6, 16),
            "raid6_ra_6" => (6, 17),
            "raid6_ls_6" => (6, 18),
            "raid6_rs_6" => (6, 19),
            "raid6_n_6" => (6, 5),
            _ => return None,
        };
        Some(Self::Parity { level, layout })
    }

    pub(super) fn profile(self) -> &'static str {
        match self {
            Self::Stripe { .. } => "raid0",
            Self::Mirror => "raid1",
            Self::Raid10(_) => "raid10",
            Self::Parity {
                level: 5,
                layout: 4,
            } => "raid4",
            Self::Parity { level: 5, .. } => "raid5",
            Self::Parity { .. } => "raid6",
        }
    }
}

struct Geometry {
    kind: Kind,
    size: u64,
    unit: u64,
    region: Option<u64>,
    lanes: usize,
    member_bytes: u64,
    copies: usize,
    devices: usize,
    far_sets: bool,
}

impl Geometry {
    fn parse(kind: Kind, spec: &Record, size: u64, extent: u64) -> Result<Self, String> {
        let count_key = match kind {
            Kind::Stripe { .. } => "stripe_count",
            _ => "device_count",
        };
        let devices = usize::try_from(field(spec, count_key, Value::number)?)
            .ok()
            .filter(|n| (2..=MAX_MEMBERS).contains(n))
            .ok_or(COMPONENTS)?;
        let (copies, lanes, stored_copies) = match kind {
            Kind::Stripe { .. } => (1, devices, 1),
            Kind::Mirror => (devices, 1, 1),
            Kind::Raid10(_) => {
                let copies = usize::try_from(optional(spec, "data_copies")?.unwrap_or(2))
                    .ok()
                    .filter(|copies| (2..=devices).contains(copies))
                    .ok_or(COMPONENTS)?;
                (copies, devices, copies)
            }
            Kind::Parity { level: 5, .. } => (1, devices - 1, 1),
            Kind::Parity { .. } if devices >= 4 => (1, devices - 2, 1),
            Kind::Parity { .. } => return Err(COMPONENTS.into()),
        };
        let member_extents = (size / extent) * stored_copies as u64;
        if optional(spec, "data_offset")?.unwrap_or(0) != 0
            || member_extents % lanes as u64 != 0
            || spec.contains_key("reshape_count")
        {
            return Err(COMPONENTS.into());
        }
        let unit = if matches!(kind, Kind::Mirror) {
            if optional(spec, "stripe_size")?.unwrap_or(0) != 0 {
                return Err(COMPONENTS.into());
            }
            size
        } else {
            let unit = bytes(field(spec, "stripe_size", Value::number)?)?;
            if !unit.is_power_of_two() || extent % unit != 0 {
                return Err(COMPONENTS.into());
            }
            unit
        };
        let region = optional(spec, "region_size")?.map(bytes).transpose()?;
        if region.is_some_and(|size| !size.is_power_of_two()) {
            return Err(COMPONENTS.into());
        }
        if matches!(kind, Kind::Raid10(Raid10::Far | Raid10::Offset))
            && size / unit % devices as u64 != 0
        {
            return Err(COMPONENTS.into());
        }
        Ok(Self {
            kind,
            size,
            unit,
            region,
            lanes,
            member_bytes: member_extents / lanes as u64 * extent,
            copies,
            devices,
            far_sets: false,
        })
    }

    fn device_bytes(&self) -> u64 {
        self.member_bytes
    }

    fn metadata(&self) -> bool {
        !matches!(self.kind, Kind::Stripe { metadata: false })
    }

    fn kernel_layout(&self) -> (u32, u32, u64) {
        let sectors = self.unit / SECTOR;
        match self.kind {
            Kind::Stripe { .. } => (0, 0, sectors),
            Kind::Mirror => (1, 0, 0),
            Kind::Raid10(Raid10::Near) => (10, 256 + self.copies as u32, sectors),
            Kind::Raid10(Raid10::Far) => (10, 1 | (self.copies as u32) << 8, sectors),
            Kind::Raid10(Raid10::Offset) => (10, 1 | (self.copies as u32) << 8 | 1 << 16, sectors),
            Kind::Parity { level, layout } => (level, layout, sectors),
        }
    }

    fn lanes(&self, chunk: u64) -> (Range<usize>, u64) {
        let ordinal = (chunk % self.lanes as u64) as usize;
        let row = chunk / self.lanes as u64;
        let lane = match self.kind {
            Kind::Stripe { .. } | Kind::Mirror => ordinal * self.copies,
            Kind::Parity { level, layout } => {
                parity::Row::new(row, self.devices, level, layout).data_lane(ordinal)
            }
            Kind::Raid10(_) => return (0..self.copies, row),
        };
        (lane..lane + self.copies, row)
    }
    fn replica(&self, chunk: u64, ordinal: usize, row: u64) -> (usize, u64) {
        match self.kind {
            Kind::Raid10(Raid10::Near) => {
                let slot = chunk * self.copies as u64 + ordinal as u64;
                (
                    (slot % self.devices as u64) as usize,
                    slot / self.devices as u64,
                )
            }
            Kind::Raid10(layout) => {
                let first = (chunk % self.devices as u64) as usize;
                let (start, width) = if self.far_sets {
                    // Linux folds any remainder into the last full group.
                    let last = (self.devices / self.copies - 1) * self.copies;
                    let start = (first / self.copies * self.copies).min(last);
                    (
                        start,
                        if start == last {
                            self.devices - last
                        } else {
                            self.copies
                        },
                    )
                } else {
                    (0, self.devices)
                };
                let lane = start + (first - start + ordinal) % width;
                let row = match layout {
                    Raid10::Offset => row * self.copies as u64 + ordinal as u64,
                    _ => row + ordinal as u64 * (self.size / self.unit / self.devices as u64),
                };
                (lane, row)
            }
            _ => (ordinal, row),
        }
    }

    fn tolerates(&self, missing: u64) -> bool {
        match self.kind {
            Kind::Stripe { .. } => missing == 0,
            Kind::Mirror => missing.count_ones() < self.devices as u32,
            Kind::Raid10(_) => (0..(self.size / self.unit).min(self.devices as u64)).all(|chunk| {
                let (copies, row) = self.lanes(chunk);
                copies
                    .into_iter()
                    .any(|copy| missing & (1u64 << self.replica(chunk, copy, row).0) == 0)
            }),
            Kind::Parity { level, .. } => missing.count_ones() <= level - 4,
        }
    }
}

pub(super) struct Array {
    geometry: Geometry,
    components: Vec<Option<Arc<Volume>>>,
}

impl Array {
    pub(super) fn degraded(&self) -> bool {
        self.components.iter().any(Option::is_none)
    }

    pub(super) fn read(
        &self,
        readers: &mut [impl Read + Seek],
        relative: u64,
        out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> io::Result<usize> {
        let unit = self.geometry.unit;
        let within = relative % unit;
        let chunk = relative / unit;
        let (lanes, row) = self.geometry.lanes(chunk);
        let limit = (out.len() as u64).min(unit - within);
        let window = |component: &Arc<Volume>, row| {
            let position = row * unit + within;
            component.segment_at(position)?.window(position)
        };
        let target = lanes.start;
        if let Kind::Parity { level, layout } = self.geometry.kind {
            if self.components[target].is_none() {
                let windows = self
                    .components
                    .iter()
                    .map(|part| part.as_ref().map(|volume| window(volume, row)).transpose())
                    .collect::<io::Result<Vec<_>>>()?;
                let amount = windows
                    .iter()
                    .flatten()
                    .fold(limit.min(64 * 1024), |n, w| n.min(w.available))
                    as usize;
                parity::Row::new(row, self.geometry.devices, level, layout).rebuild(
                    readers,
                    &windows,
                    target,
                    &mut out[..amount],
                    scratch,
                )?;
                return Ok(amount);
            }
        }
        let mut windows = lanes.filter_map(|copy| {
            let (lane, row) = self.geometry.replica(chunk, copy, row);
            self.components[lane]
                .as_ref()
                .map(|volume| window(volume, row))
        });
        let first = windows
            .next()
            .ok_or_else(|| io::Error::other("LVM2 RAID member is absent"))??;
        let copies = windows.collect::<io::Result<Vec<_>>>()?;
        let limit = limit.min(first.available);
        let amount = if copies.is_empty() {
            limit
        } else {
            copies
                .iter()
                .fold(limit.min(64 * 1024), |n, w| n.min(w.available))
        } as usize;
        let out = &mut out[..amount];
        first.read(readers, out)?;
        for copy in copies {
            scratch.resize(amount, 0);
            copy.read(readers, scratch)?;
            if scratch.as_slice() != out {
                return Err(io::Error::other("LVM2 RAID 镜像副本内容不一致。"));
            }
        }
        Ok(amount)
    }
}

pub(super) struct Bindings<'a, 'n, R> {
    readers: &'a mut [R],
    candidates: &'a BTreeMap<&'n str, Candidate>,
    records: &'a Record,
    extent: u64,
    allow_degraded: bool,
}

fn optional(record: &Record, key: &str) -> Result<Option<u64>, String> {
    record
        .get(key)
        .map(|v| v.number().ok_or_else(|| COMPONENTS.into()))
        .transpose()
}

fn component_role<'a>(name: &'a str, owner: &str, role: &str) -> Result<&'a str, String> {
    name.strip_prefix(owner)
        .and_then(|name| name.strip_prefix(role))
        .filter(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| COMPONENTS.into())
}

impl<'a, 'n, R: Read + Seek> Bindings<'a, 'n, R> {
    pub(super) fn new(
        readers: &'a mut [R],
        candidates: &'a BTreeMap<&'n str, Candidate>,
        records: &'a Record,
        extent: u64,
        allow_degraded: bool,
    ) -> Self {
        Self {
            readers,
            candidates,
            records,
            extent,
            allow_degraded,
        }
    }

    fn record(&self, name: &str) -> Result<&'a Record, String> {
        field(self.records, name, Value::section)
    }

    pub(super) fn kind(&self, name: &str) -> Result<Option<Kind>, String> {
        let record = self.record(name)?;
        if field(record, "segment_count", Value::number)? != 1 {
            return Ok(None);
        }
        Ok(record
            .get("segment1")
            .and_then(Value::section)
            .and_then(|segment| segment.get("type"))
            .and_then(Value::text)
            .and_then(Kind::from_name))
    }

    fn component(&self, name: &str) -> Result<&Candidate, String> {
        let candidate = self
            .candidates
            .get(name)
            .filter(|entry| !entry.visible)
            .ok_or(COMPONENTS)?;
        if !supported_metadata_flags(self.record(name)?) || candidate.info.profile == "unsupported"
        {
            return Err(COMPONENTS.into());
        }
        Ok(candidate)
    }

    fn components(
        &self,
        owner: &str,
        geometry: &Geometry,
        spec: &Record,
    ) -> Result<Vec<Component>, String> {
        let has_meta = geometry.metadata();
        let list_key = if has_meta { "raids" } else { "raid0_lvs" };
        let names = field(spec, list_key, Value::list)?;
        let width = if has_meta { 2 } else { 1 };
        if names.len() != width * geometry.devices {
            return Err(COMPONENTS.into());
        }
        let mut components = Vec::with_capacity(geometry.devices);
        let mut used = HashSet::new();
        for entry in names.chunks_exact(width) {
            let data = entry[width - 1].text().ok_or(COMPONENTS)?;
            let role = component_role(data, owner, "_rimage_")?;
            let image = self.component(data)?;
            if !used.insert(role) || image.info.size != geometry.device_bytes() {
                return Err(COMPONENTS.into());
            }
            let metadata = if has_meta {
                let meta = entry[0].text().ok_or(COMPONENTS)?;
                if component_role(meta, owner, "_rmeta_")? != role {
                    return Err(COMPONENTS.into());
                }
                self.component(meta)?.mapped.clone()
            } else {
                None
            };
            let image = if !has_meta || metadata.is_some() {
                image.mapped.clone()
            } else {
                None
            };
            components.push(Component { image, metadata });
        }
        Ok(components)
    }

    pub(super) fn bind(
        &mut self,
        name: &str,
        size: u64,
        kind: Kind,
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Result<(Segment, [u8; 32]), String>, String> {
        let record = self.record(name)?;
        let spec = field(record, "segment1", Value::section)?;
        if !field(record, "status", Value::list)?
            .iter()
            .any(|item| item.text() == Some("READ"))
            || !supported_metadata_flags(record)
            || !supported_metadata_flags(spec)
        {
            return Ok(Err(STATE.into()));
        }
        let (mut geometry, components) = match Geometry::parse(kind, spec, size, self.extent)
            .and_then(|geometry| Ok((self.components(name, &geometry, spec)?, geometry)))
        {
            Ok((components, geometry)) => (geometry, components),
            Err(error) => return Ok(Err(error)),
        };
        let missing = components
            .iter()
            .enumerate()
            .fold(0u64, |mask, (index, component)| {
                mask | (u64::from(component.image.is_none()) << index)
            });
        if missing != 0 && !self.allow_degraded {
            return Ok(Err(MISSING.into()));
        }
        let mut digest = Sha256::new();
        digest.update(name.as_bytes());
        digest.update(missing.to_le_bytes());
        let mut epochs = None;
        for (index, component) in components.iter().enumerate() {
            let Some(volume) = component
                .metadata
                .as_deref()
                .filter(|_| component.image.is_some())
            else {
                continue;
            };
            checkpoint()?;
            match check_member(self.readers, volume, &geometry, index, missing, &mut digest) {
                Ok(state) if epochs.is_none_or(|expected| state == expected) => {
                    epochs = Some(state)
                }
                Ok(_) => return Ok(Err(STATE.into())),
                Err(error) => return Ok(Err(error)),
            }
        }
        geometry.far_sets = epochs.is_some_and(|(_, _, layout)| layout & FAR_SETS != 0);
        if !geometry.tolerates(missing) {
            return Ok(Err(INSUFFICIENT.into()));
        }
        Ok(Ok((
            Segment {
                logical: 0..size,
                unit: geometry.unit,
                mapping: Mapping::Raid(Array {
                    geometry,
                    components: components
                        .into_iter()
                        .map(|component| component.image)
                        .collect(),
                }),
            },
            digest.finalize().into(),
        )))
    }
}

struct Component {
    image: Option<Arc<Volume>>,
    metadata: Option<Arc<Volume>>,
}

fn check_member(
    readers: &mut [impl Read + Seek],
    volume: &Volume,
    geometry: &Geometry,
    index: usize,
    missing: u64,
    digest: &mut Sha256,
) -> Result<(u64, u64, u32), String> {
    let mut sb = [0; 512];
    volume.read_plain(readers, 0, &mut sb)?;
    let (level, layout, stripe) = geometry.kernel_layout();
    let actual_layout = u32le(&sb, 52);
    let allowed_sets = if matches!(geometry.kind, Kind::Raid10(Raid10::Far | Raid10::Offset)) {
        FAR_SETS
    } else {
        0
    };
    let epoch = u64le(&sb, 16);
    let failed = u64le(&sb, 24);
    if &sb[..4] != b"DmRd"
        || u32le(&sb, 4) != 1
        || u32le(&sb, 8) != geometry.devices as u32
        || u32le(&sb, 12) != index as u32
        || epoch == 0
        || failed & !missing != 0
        || u64le(&sb, 32) != u64::MAX
        || u64le(&sb, 40) != u64::MAX
        || u32le(&sb, 48) != level
        || actual_layout & !allowed_sets != layout
        || u64::from(u32le(&sb, 56)) != stripe
        || u32le(&sb, 60) != 0
        || u64le(&sb, 64) != u64::MAX
        || u32le(&sb, 72) != level
        || u32le(&sb, 76) != actual_layout
        || u64::from(u32le(&sb, 80)) != stripe
        || u32le(&sb, 84) != 0
        || u64le(&sb, 88) != geometry.size / SECTOR
        || u64le(&sb, 96) != 0
        || u64le(&sb, 104) != 0
        || u64le(&sb, 112) != geometry.device_bytes() / SECTOR
        || sb[120..148].iter().any(|&byte| byte != 0)
    {
        return Err(STATE.into());
    }
    digest.update(sb);
    if !matches!(geometry.kind, Kind::Stripe { .. }) {
        check_bitmap(readers, volume, geometry, epoch, digest)?;
    }
    Ok((epoch, failed, actual_layout))
}

fn check_bitmap(
    readers: &mut [impl Read + Seek],
    volume: &Volume,
    geometry: &Geometry,
    epoch: u64,
    digest: &mut Sha256,
) -> Result<(), String> {
    let mut header = [0; BITMAP_HEADER];
    volume.read_plain(readers, BITMAP_OFFSET, &mut header)?;
    let events = u64le(&header, 24);
    let state = u32le(&header, 48);
    let chunk = u64::from(u32le(&header, 52));
    let sync_size = if matches!(geometry.kind, Kind::Raid10(_)) {
        geometry.size
    } else {
        geometry.device_bytes()
    };
    if u32le(&header, 0) != 0x6d746962
        || u32le(&header, 4) != 4
        || !(epoch..=epoch.saturating_add(1)).contains(&events)
        || u64le(&header, 32) > events
        || u64le(&header, 40) != sync_size / SECTOR
        || state != 0
        || chunk < SECTOR
        || !chunk.is_power_of_two()
        || geometry.region.is_some_and(|n| n != chunk)
        || u32le(&header, 68) != 0
    {
        return Err(STATE.into());
    }
    let regions = sync_size.div_ceil(chunk);
    let bytes = regions.div_ceil(8);
    if bytes > MAX_BITMAP {
        return Err(STATE.into());
    }
    let mut bitmap = vec![0; bytes as usize];
    volume.read_plain(readers, BITMAP_OFFSET + BITMAP_HEADER as u64, &mut bitmap)?;
    // Linux can leave the bits beyond the final region set.
    let whole_bytes = (regions / 8) as usize;
    let tail_mask = (1u8 << (regions % 8)) - 1;
    if bitmap[..whole_bytes].iter().any(|&byte| byte != 0)
        || bitmap
            .get(whole_bytes)
            .is_some_and(|byte| byte & tail_mask != 0)
    {
        return Err(STATE.into());
    }
    digest.update(header);
    digest.update(bitmap);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_layouts_select_only_the_expected_data_columns() {
        let columns = |row, width, level, layout| {
            (0..width - (level - 4) as usize)
                .map(|column| parity::Row::new(row, width, level, layout).data_lane(column))
                .collect::<Vec<_>>()
        };
        assert_eq!(columns(2, 4, 5, 4), [1, 2, 3]);
        assert_eq!(columns(0, 4, 5, 2), [0, 1, 2]);
        assert_eq!(columns(1, 4, 5, 2), [3, 0, 1]);
        assert_eq!(columns(2, 4, 5, 2), [2, 3, 0]);
        assert_eq!(columns(0, 5, 6, 8), [2, 3, 4]);
        assert_eq!(columns(1, 5, 6, 8), [0, 3, 4]);
        assert_eq!(columns(4, 5, 6, 8), [1, 2, 3]);
        assert_eq!(columns(1, 5, 6, 9), [0, 1, 4]);
        assert_eq!(columns(1, 5, 6, 10), [4, 0, 1]);
        assert_eq!(columns(1, 5, 6, 18), [3, 0, 1]);
    }

    #[test]
    fn raid10_replica_rows_and_degraded_masks_follow_the_member_layout() {
        let geometry = |kind, devices, far_sets| {
            let record =
                text::parse(format!("device_count = {devices}\nstripe_size = 8\n\0").as_bytes())
                    .unwrap();
            let mut geometry =
                Geometry::parse(Kind::Raid10(kind), &record, 60 * 4096, 4096).unwrap();
            geometry.far_sets = far_sets;
            geometry
        };
        let slots = |g: &Geometry, chunk| {
            let (replicas, row) = g.lanes(chunk);
            replicas
                .map(|copy| g.replica(chunk, copy, row))
                .collect::<Vec<_>>()
        };
        let near = geometry(Raid10::Near, 5, false);
        assert_eq!(slots(&near, 2), [(4, 0), (0, 1)]);
        assert_eq!(slots(&near, 59), [(3, 23), (4, 23)]);
        assert!(!near.tolerates(0b10001));
        assert!(near.tolerates(0b10100));

        let far = geometry(Raid10::Far, 4, false);
        assert_eq!(slots(&far, 3), [(3, 0), (0, 15)]);
        assert_eq!(slots(&far, 59), [(3, 14), (0, 29)]);
        assert!(!far.tolerates(0b1001));
        assert!(far.tolerates(0b1010));

        let offset = geometry(Raid10::Offset, 5, true);
        assert_eq!(slots(&offset, 4), [(4, 0), (2, 1)]);
        assert_eq!(slots(&offset, 59), [(4, 22), (2, 23)]);
        assert!(!offset.tolerates(0b10100));
        assert!(offset.tolerates(0b10010));
    }
}

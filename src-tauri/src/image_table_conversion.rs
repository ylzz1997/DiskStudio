//! Table-only conversion of an immutable logical image into a new image.
//! Partition offsets, lengths and bytes never change.
use crate::{
    partitions::policy::{BASIC_DATA, LINUX_DATA, LINUX_SWAP},
    table_archive::{self, Archive, Extent, Kind},
    table_restore::{self, Entry},
    used_space::View,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read, Seek, SeekFrom},
};
use uuid::Uuid;

const LINUX_LVM: &str = "e6d6d379-f507-44c2-a23c-238f2a3df928";
const LINUX_RAID: &str = "a19d880f-05fc-4d3b-a006-743f0f84911e";
const EXTENDED: u8 = 0x0f;
const MIB: u64 = 1024 * 1024;

#[path = "image_mbr_placement.rs"]
mod mbr_placement;
use mbr_placement::PlacementChange;
#[path = "image_gpt_repair.rs"]
pub(crate) mod gpt_repair;
#[path = "image_table_recovery.rs"]
pub(crate) mod recovery;
#[path = "image_partition_registration.rs"]
pub(crate) mod registration;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Scheme {
    Mbr,
    Gpt,
}
impl Scheme {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Mbr => "MBR",
            Self::Gpt => "GPT",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PartitionChange {
    pub offset: u64,
    pub size: u64,
    pub old_type: String,
    pub new_type: String,
    pub logical: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub from: Scheme,
    pub to: Scheme,
    pub sector: u64,
    pub disk_id: String,
    pub partitions: Vec<PartitionChange>,
    pub table_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpt_repair: Option<gpt_repair::Changes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration: Option<registration::Preview>,
}

#[derive(Clone)]
struct Volume {
    offset: u64,
    size: u64,
    old_type: String,
    gpt_kind: &'static str,
    mbr_kind: u8,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Patch {
    pub offset: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}
pub(crate) struct Conversion {
    pub preview: Preview,
    retained: Vec<(u64, Vec<u8>)>,
    output: Archive,
    patches: Vec<Patch>,
}

fn label(kind: u8) -> String {
    format!("0x{kind:02X}")
}
fn extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
}
fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn basic_mbr_type(
    reader: &mut (impl Read + Seek),
    archive: &Archive,
    offset: u64,
    size: u64,
) -> Result<u8, String> {
    let boot = archive
        .extents
        .iter()
        .find(|e| e.kind == Kind::Boot && e.offset == offset)
        .ok_or("缺少数据分区的引导扇区。")?;
    if &boot.bytes[3..11] == b"NTFS    " {
        let volume = ntfs::Ntfs::new(&mut Cursor::new(&boot.bytes)).map_err(|e| e.to_string())?;
        if u64::from(volume.sector_size()) != archive.sector
            || volume.size() == 0
            || volume.size() > size
        {
            return Err("NTFS 引导扇区与分区范围或逻辑扇区大小不一致。".into());
        }
        return Ok(0x07);
    }
    let mut view = View::bounded(reader, offset, size, archive.size).map_err(|e| e.to_string())?;
    let bits = crate::fat_recovery::table_volume_kind(&mut view, &boot.bytes, archive.sector)
        .map_err(|e| format!("无法核对该数据分区的 NTFS、FAT 或 exFAT 格式。{e}"))?;
    match bits {
        12 => Ok(0x01),
        16 => Ok(0x0e),
        32 => Ok(0x0c),
        64 => Ok(0x07),
        _ => Err("无法选择对应的 MBR 数据类型。".into()),
    }
}

fn swap_signature(
    reader: &mut (impl Read + Seek),
    offset: u64,
    size: u64,
    disk: u64,
) -> Result<bool, String> {
    for page in [4096, 8192, 16384, 32768, 65536] {
        if page <= size {
            let sig = table_archive::read_at(reader, offset + page - 10, 10, disk)?;
            if sig == b"SWAPSPACE2" || sig == b"SWAP-SPACE" {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn gpt_volumes(reader: &mut (impl Read + Seek), archive: &Archive) -> Result<Vec<Volume>, String> {
    table_restore::archive_layout(archive)?
        .entries
        .into_iter()
        .map(|entry| {
            if entry.attributes != 0 {
                return Err("镜像 GPT 分区含有 MBR 无法保留的属性，已拒绝转换。".into());
            }
            let (gpt_kind, mbr_kind) = match entry.kind.as_str() {
                BASIC_DATA => (
                    BASIC_DATA,
                    basic_mbr_type(reader, archive, entry.offset, entry.size)?,
                ),
                LINUX_DATA => (LINUX_DATA, 0x83),
                LINUX_SWAP if swap_signature(reader, entry.offset, entry.size, archive.size)? => {
                    (LINUX_SWAP, 0x82)
                }
                LINUX_LVM => (LINUX_LVM, 0x8e),
                LINUX_RAID => (LINUX_RAID, 0xfd),
                _ => return Err(format!("该 GPT 分区类型无法安全转换为 MBR。{}", entry.kind)),
            };
            Ok(Volume {
                offset: entry.offset,
                size: entry.size,
                old_type: entry.kind,
                gpt_kind,
                mbr_kind,
            })
        })
        .collect()
}

fn mbr_volumes(reader: &mut (impl Read + Seek), archive: &Archive) -> Result<Vec<Volume>, String> {
    let mut volumes = Vec::new();
    for extent in &archive.extents {
        let slots = match extent.kind {
            Kind::Mbr => &extent.bytes[446..510],
            Kind::Ebr => &extent.bytes[446..462],
            _ => continue,
        };
        for entry in slots.chunks_exact(16) {
            if entry[0] != 0 {
                return Err("镜像含活动 MBR 分区，数据分区表转换不处理其引导配置。".into());
            }
            let mbr_kind = entry[4];
            if mbr_kind == 0 || extended(mbr_kind) {
                continue;
            }
            let offset = extent.offset + u64::from(u32le(&entry[8..12])) * archive.sector;
            let size = u64::from(u32le(&entry[12..16])) * archive.sector;
            let gpt_kind = match mbr_kind {
                0x01 | 0x04 | 0x06 | 0x07 | 0x0b | 0x0c | 0x0e => {
                    let expected = match mbr_kind {
                        0x04 | 0x06 => 0x0e,
                        0x0b => 0x0c,
                        other => other,
                    };
                    if basic_mbr_type(reader, archive, offset, size)? != expected {
                        return Err("MBR 类型代码与数据分区实际引导扇区不一致。".into());
                    }
                    BASIC_DATA
                }
                0x83 => LINUX_DATA,
                0x82 if swap_signature(reader, offset, size, archive.size)? => LINUX_SWAP,
                0x8e => LINUX_LVM,
                0xfd => LINUX_RAID,
                other => {
                    return Err(format!(
                        "该 MBR 分区类型无法安全转换为 GPT。{}",
                        label(other)
                    ))
                }
            };
            volumes.push(Volume {
                offset,
                size,
                old_type: label(mbr_kind),
                gpt_kind,
                mbr_kind,
            });
        }
    }
    Ok(volumes)
}

fn ranges_match(information: &table_archive::Info, volumes: &[Volume]) -> bool {
    let mut saved: Vec<_> = information
        .layout
        .iter()
        .map(|p| (p.offset, p.size))
        .collect();
    saved.sort_unstable();
    saved
        .into_iter()
        .eq(volumes.iter().map(|p| (p.offset, p.size)))
}

fn mbr_record(
    bytes: &mut [u8],
    slot: usize,
    kind: u8,
    lba: u64,
    length: u64,
) -> Result<(), String> {
    let start = u32::try_from(lba).map_err(|_| "MBR 起始逻辑扇区超出 32 位范围。")?;
    let count = u32::try_from(length).map_err(|_| "MBR 扇区数超出 32 位范围。")?;
    if start == 0 || count == 0 || start.checked_add(count - 1).is_none() {
        return Err("MBR 数据范围超出可表示的逻辑扇区。".into());
    }
    let entry = &mut bytes[446 + slot * 16..462 + slot * 16];
    entry[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
    entry[4] = kind;
    entry[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    entry[8..12].copy_from_slice(&start.to_le_bytes());
    entry[12..16].copy_from_slice(&count.to_le_bytes());
    Ok(())
}
fn empty_mbr(sector: u64) -> Vec<u8> {
    let mut bytes = vec![0; sector as usize];
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
    bytes
}
fn vacant_ebr(
    reader: &mut (impl Read + Seek),
    source: &Archive,
    lower: u64,
    data: u64,
) -> Result<u64, String> {
    let first = lower.max(data.saturating_sub(MIB));
    if first >= data {
        return Err("逻辑分区前没有可放置 EBR 的独立空闲扇区。".into());
    }
    zero_ebr(reader, source, first, data)?
        .ok_or_else(|| "逻辑分区前 1 MiB 范围内没有全零 EBR 扇区。".into())
}

fn zero_ebr(
    reader: &mut (impl Read + Seek),
    source: &Archive,
    lower: u64,
    data: u64,
) -> Result<Option<u64>, String> {
    let first = lower.max(data.saturating_sub(MIB));
    if first >= data {
        return Ok(None);
    }
    let gap = table_archive::read_at(reader, first, (data - first) as usize, source.size)?;
    Ok(gap
        .chunks_exact(source.sector as usize)
        .rposition(|block| block.iter().all(|&b| b == 0))
        .map(|zero| first + zero as u64 * source.sector))
}
fn new_mbr(
    reader: &mut (impl Read + Seek),
    source: &Archive,
    volumes: &[Volume],
    signature: u32,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<Archive, String> {
    let sector = source.sector;
    if source.size / sector > 1u64 << 32 || volumes.len() > 3 + disk_core::mbr::MAX_EBRS {
        return Err("镜像容量或分区数超出支持的 MBR 和 64 个 EBR 的范围。".into());
    }
    let mut mbr = empty_mbr(sector);
    mbr[440..444].copy_from_slice(&signature.to_le_bytes());
    let primary = if volumes.len() > 4 { 3 } else { volumes.len() };
    for (i, volume) in volumes[..primary].iter().enumerate() {
        mbr_record(
            &mut mbr,
            i,
            volume.mbr_kind,
            volume.offset / sector,
            volume.size / sector,
        )?;
    }
    let mut extents = vec![Extent {
        kind: Kind::Mbr,
        offset: 0,
        bytes: mbr,
    }];
    if primary < volumes.len() {
        let mut positions = Vec::new();
        let mut prior = volumes[primary - 1].offset + volumes[primary - 1].size;
        for volume in &volumes[primary..] {
            tick()?;
            positions.push(vacant_ebr(reader, source, prior, volume.offset)?);
            prior = volume.offset + volume.size;
        }
        let base = positions[0] / sector;
        let end = prior / sector;
        mbr_record(&mut extents[0].bytes, 3, EXTENDED, base, end - base)?;
        for (i, volume) in volumes[primary..].iter().enumerate() {
            let at = positions[i] / sector;
            let mut ebr = empty_mbr(sector);
            mbr_record(
                &mut ebr,
                0,
                volume.mbr_kind,
                volume.offset / sector - at,
                volume.size / sector,
            )?;
            if let Some(next) = positions.get(i + 1).map(|n| n / sector) {
                mbr_record(&mut ebr, 1, EXTENDED, next - base, end - next)?;
            }
            extents.push(Extent {
                kind: Kind::Ebr,
                offset: at * sector,
                bytes: ebr,
            });
        }
    }
    Ok(Archive {
        version: 1,
        size: source.size,
        sector,
        source: source.source.clone(),
        includes_boot: false,
        extents,
    })
}

impl Conversion {
    pub(crate) fn prepare(
        reader: &mut (impl Read + Seek),
        size: u64,
        sector: u64,
        source_key: String,
        to: Scheme,
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let source = table_archive::capture(reader, size, sector, source_key, true, &mut tick)?;
        let information = source.validate()?;
        let from = match information.scheme.as_str() {
            "GPT" => Scheme::Gpt,
            "MBR" => Scheme::Mbr,
            _ => return Err("请选择可校验的 MBR 或 GPT 整盘镜像。".into()),
        };
        if from == to {
            return Err("请选择与来源不同的目标分区表。".into());
        }
        let mut volumes = match from {
            Scheme::Mbr => mbr_volumes(reader, &source)?,
            Scheme::Gpt => gpt_volumes(reader, &source)?,
        };
        volumes.sort_by_key(|p| p.offset);
        if !ranges_match(&information, &volumes) {
            return Err("转换列表与已校验的原分区范围不一致。".into());
        }
        let id = Uuid::new_v4();
        let (output, disk_id) = match to {
            Scheme::Gpt => {
                let entries = volumes
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Entry {
                        slot: (i + 1) as u32,
                        guid: Uuid::new_v4().to_string(),
                        kind: v.gpt_kind.into(),
                        offset: v.offset,
                        size: v.size,
                        attributes: 0,
                        name: vec![],
                    })
                    .collect::<Vec<_>>();
                (
                    table_restore::new_gpt(size, sector, source.source.clone(), id, &entries)?,
                    id.to_string(),
                )
            }
            Scheme::Mbr => {
                let signature = u32le(&id.as_bytes()[..4]).max(1);
                (
                    new_mbr(reader, &source, &volumes, signature, &mut tick)?,
                    format!("{signature:08X}"),
                )
            }
        };
        let partitions = volumes
            .iter()
            .enumerate()
            .map(|(i, v)| PartitionChange {
                offset: v.offset,
                size: v.size,
                old_type: v.old_type.clone(),
                new_type: match to {
                    Scheme::Gpt => v.gpt_kind.into(),
                    Scheme::Mbr => label(v.mbr_kind),
                },
                logical: to == Scheme::Mbr && volumes.len() > 4 && i >= 3,
            })
            .collect();
        Self::finish_plan(
            reader,
            source,
            output,
            Preview {
                from,
                to,
                sector,
                disk_id,
                partitions,
                table_bytes: 0,
                placement: None,
                restored_from: None,
                gpt_repair: None,
                registration: None,
            },
            tick,
        )
    }

    fn finish_plan(
        reader: &mut (impl Read + Seek),
        source: Archive,
        output: Archive,
        mut preview: Preview,
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let sector = source.sector;
        let size = source.size;
        let rebuilt = output.validate()?;
        let mut ranges: Vec<_> = rebuilt.layout.iter().map(|p| (p.offset, p.size)).collect();
        ranges.sort_unstable();
        if rebuilt.scheme != preview.to.name()
            || !ranges
                .into_iter()
                .eq(preview.partitions.iter().map(|p| (p.offset, p.size)))
        {
            return Err("生成的分区表未通过范围回读校验。".into());
        }
        let mut patches = BTreeMap::new();
        for extent in source.extents.iter().filter(|e| e.kind != Kind::Boot) {
            for (i, bytes) in extent.bytes.chunks_exact(sector as usize).enumerate() {
                let offset = extent.offset + i as u64 * sector;
                let after = if preview.placement.is_some() && extent.kind == Kind::Ebr {
                    let mut retired = bytes.to_vec();
                    retired[446..512].fill(0);
                    retired
                } else {
                    vec![0; sector as usize]
                };
                patches.insert(
                    offset,
                    Patch {
                        offset,
                        before: bytes.to_vec(),
                        after,
                    },
                );
            }
        }
        for extent in &output.extents {
            for (i, bytes) in extent.bytes.chunks_exact(sector as usize).enumerate() {
                let offset = extent.offset + i as u64 * sector;
                if let Some(patch) = patches.get_mut(&offset) {
                    patch.after.copy_from_slice(bytes);
                } else {
                    tick()?;
                    let before = table_archive::read_at(reader, offset, sector as usize, size)?;
                    if before.iter().any(|&byte| byte != 0) {
                        return Err(format!(
                            "新分区表需要的空间含非零数据，已拒绝覆盖。偏移 {offset} B。"
                        ));
                    }
                    patches.insert(
                        offset,
                        Patch {
                            offset,
                            before,
                            after: bytes.to_vec(),
                        },
                    );
                }
            }
        }
        let patches = patches
            .into_values()
            .filter(|p| p.before != p.after)
            .collect::<Vec<_>>();
        if patches.iter().any(|p| {
            preview
                .partitions
                .iter()
                .any(|v| p.offset < v.offset + v.size && v.offset < p.offset + sector)
        }) {
            return Err("分区表变更与现有分区数据重叠。".into());
        }
        preview.table_bytes = patches.len() as u64 * sector;
        let conversion = Self {
            preview,
            retained: source
                .extents
                .into_iter()
                .map(|e| (e.offset, e.bytes))
                .collect(),
            output,
            patches,
        };
        conversion.verify_source(reader, tick)?;
        Ok(conversion)
    }

    fn replace_table(
        reader: &mut (impl Read + Seek),
        retained: Vec<(u64, Vec<u8>)>,
        output: Archive,
        mut preview: Preview,
    ) -> Result<Self, String> {
        let mut patches = Vec::new();
        for e in &output.extents {
            let before = table_archive::read_at(reader, e.offset, e.bytes.len(), output.size)?;
            if before != e.bytes {
                let sector = output.sector as usize;
                preview.table_bytes += output.sector
                    * before
                        .chunks_exact(sector)
                        .zip(e.bytes.chunks_exact(sector))
                        .filter(|(old, new)| old != new)
                        .count() as u64;
                patches.push(Patch {
                    offset: e.offset,
                    before,
                    after: e.bytes.clone(),
                });
            }
        }
        if patches.is_empty() {
            return Err("镜像分区表已经与计划一致。".into());
        }
        let conversion = Self {
            preview,
            retained,
            output,
            patches,
        };
        conversion.verify_source(reader, || Ok(()))?;
        Ok(conversion)
    }

    pub(crate) fn size(&self) -> u64 {
        self.output.size
    }
    pub(crate) fn verify_source(
        &self,
        reader: &mut (impl Read + Seek),
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        table_archive::match_ranges(
            reader,
            self.size(),
            self.retained
                .iter()
                .map(|(offset, bytes)| (*offset, bytes.as_slice())),
            &mut tick,
        )?;
        for patch in &self.patches {
            tick()?;
            if table_archive::read_at(reader, patch.offset, patch.before.len(), self.size())?
                != patch.before
            {
                return Err("分区表或其预留空间自预览起发生变化。".into());
            }
        }
        Ok(())
    }
    pub(crate) fn verify_output(
        &self,
        reader: &mut (impl Read + Seek),
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let observed = table_archive::capture(
            reader,
            self.size(),
            self.output.sector,
            self.output.source.clone(),
            false,
            &mut tick,
        )?;
        if observed != self.output {
            return Err("转换镜像回读的分区表与确认内容不一致。".into());
        }
        Ok(())
    }
    pub(crate) fn reader<'a, R: Read + Seek>(&'a self, source: R) -> io::Result<Overlay<'a, R>> {
        Overlay::new(source, &self.patches, self.size())
    }
}

pub(crate) struct Overlay<'a, R> {
    source: R,
    patches: &'a [Patch],
    offset: u64,
    size: u64,
}
impl<'a, R: Read + Seek> Overlay<'a, R> {
    pub(crate) fn new(mut source: R, patches: &'a [Patch], size: u64) -> io::Result<Self> {
        source.seek(SeekFrom::Start(0))?;
        Ok(Overlay {
            source,
            patches,
            offset: 0,
            size,
        })
    }
}
impl<R: Seek> Seek for Overlay<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let at = match position {
            SeekFrom::Start(at) => i128::from(at),
            SeekFrom::Current(by) => i128::from(self.offset) + i128::from(by),
            SeekFrom::End(by) => i128::from(self.size) + i128::from(by),
        };
        if !(0..=i128::from(self.size)).contains(&at) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Image overlay seek is out of range",
            ));
        }
        let at = at as u64;
        self.source.seek(SeekFrom::Start(at))?;
        self.offset = at;
        Ok(at)
    }
}
impl<R: Read> Read for Overlay<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = (self.size - self.offset).min(output.len() as u64) as usize;
        let read = self.source.read(&mut output[..remaining])?;
        let end = self.offset + read as u64;
        let i = self
            .patches
            .partition_point(|p| p.offset + p.before.len() as u64 <= self.offset);
        for p in self.patches[i..].iter().take_while(|p| p.offset < end) {
            let first = self.offset.max(p.offset);
            let last = end.min(p.offset + p.before.len() as u64);
            let a = (first - p.offset) as usize;
            let b = (last - p.offset) as usize;
            let target = &mut output[(first - self.offset) as usize..(last - self.offset) as usize];
            if target != &p.before[a..b] {
                return Err(io::Error::other("分区表或其预留空间自预览起发生变化。"));
            }
            target.copy_from_slice(&p.after[a..b]);
        }
        self.offset = end;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_convert::{Encoder, Format, Identity, Options};
    use disk_core::fat_format::{FatType, Plan};

    const SIZE: u64 = 12 * MIB;
    fn source_key() -> String {
        "b".repeat(64)
    }

    fn write_extents(image: &mut [u8], extents: &[Extent]) {
        for extent in extents {
            let offset = extent.offset as usize;
            image[offset..offset + extent.bytes.len()].copy_from_slice(&extent.bytes);
        }
    }
    fn gpt_image(sector: u64, partitions: usize, attrs: u64) -> Vec<u8> {
        let entries = (0..partitions)
            .map(|i| Entry {
                slot: (i + 1) as u32,
                guid: Uuid::new_v4().to_string(),
                offset: (i + 1) as u64 * MIB,
                size: MIB / 2,
                kind: LINUX_DATA.into(),
                attributes: if i == 0 { attrs } else { 0 },
                name: "Original label".encode_utf16().collect(),
            })
            .collect::<Vec<_>>();
        let archive =
            table_restore::new_gpt(SIZE, sector, source_key(), Uuid::new_v4(), &entries).unwrap();
        let mut image = vec![0; SIZE as usize];
        write_extents(&mut image, &archive.extents);
        for (i, entry) in entries.iter().enumerate() {
            let at = entry.offset as usize;
            image[at..at + entry.size as usize].fill(37 + i as u8);
        }
        // Source bytes outside old or new table sectors also survive.
        image[9 * MIB as usize] = 0xb3;
        image
    }
    fn prepare(image: &[u8], sector: u64, to: Scheme) -> Result<Conversion, String> {
        Conversion::prepare(
            &mut Cursor::new(image),
            image.len() as u64,
            sector,
            source_key(),
            to,
            || Ok(()),
        )
    }
    fn output(source: &[u8], plan: &Conversion) -> Vec<u8> {
        let mut result = Vec::new();
        plan.reader(Cursor::new(source))
            .unwrap()
            .read_to_end(&mut result)
            .unwrap();
        plan.verify_output(&mut Cursor::new(&result), || Ok(()))
            .unwrap();
        for partition in &plan.preview.partitions {
            let start = partition.offset as usize;
            let end = start + partition.size as usize;
            assert_eq!(&source[start..end], &result[start..end]);
        }
        result
    }

    fn placement(image: &[u8], sector: u64, start: u64) -> Result<Conversion, String> {
        Conversion::prepare_placement(
            &mut Cursor::new(image),
            image.len() as u64,
            sector,
            source_key(),
            (start, MIB / 2),
            || Ok(()),
        )
    }

    #[test]
    fn mbr_placement_uses_the_fourth_slot_and_frees_it_on_the_return_trip() {
        for sector in [512, 4096] {
            use mbr_placement::Form::{Logical, Primary};
            let gpt = gpt_image(sector, 4, 0);
            let mut original = output(&gpt, &prepare(&gpt, sector, Scheme::Mbr).unwrap());
            original[31] = 0x6a;
            let plan = placement(&original, sector, 2 * MIB).unwrap();
            let shift = plan.preview.placement.as_ref().unwrap();
            assert_eq!(
                (shift.offset, shift.from, shift.to),
                (2 * MIB, Primary, Logical)
            );
            assert_eq!(plan.preview.table_bytes, 2 * sector);
            let mut logical = output(&original, &plan);
            assert_ne!(&logical[440..444], &original[440..444]);
            assert_eq!(&logical[..440], &original[..440]);
            assert_eq!(&logical[446..462], &original[446..462]);
            assert_eq!(&logical[478..510], &original[478..510]);
            assert_eq!(logical[466], EXTENDED);
            let ebr = (2 * MIB - sector) as usize;
            logical[ebr + 31] = 0x73;
            let return_plan = placement(&logical, sector, 2 * MIB).unwrap();
            let shift = return_plan.preview.placement.as_ref().unwrap();
            assert_eq!((shift.from, shift.to), (Logical, Primary));
            let restored = output(&logical, &return_plan);
            assert_eq!(&restored[446..510], &original[446..510]);
            assert_eq!(restored[ebr + 31], 0x73);
            assert_eq!(&restored[ebr + 446..ebr + 512], &[0; 66]);
            assert_eq!(restored[9 * MIB as usize], 0xb3);
        }
    }

    #[test]
    fn mbr_placement_expands_and_shrinks_the_chain_without_moving_existing_ebrs() {
        let sector = 4096;
        let gpt = gpt_image(sector, 6, 0);
        let original = output(&gpt, &prepare(&gpt, sector, Scheme::Mbr).unwrap());
        let plan = placement(&original, sector, 3 * MIB).unwrap();
        let mbr = output(&original, &plan);
        assert_eq!(
            plan.output
                .extents
                .iter()
                .filter(|e| e.kind == Kind::Ebr)
                .map(|e| e.offset)
                .collect::<Vec<_>>(),
            (3..=6).map(|i| i * MIB - sector).collect::<Vec<_>>(),
        );
        assert!(placement(&original, sector, 6 * MIB)
            .err()
            .unwrap()
            .contains("空闲的主分区表项"));
        assert!(placement(&mbr, sector, 4 * MIB)
            .err()
            .unwrap()
            .contains("跨过其他主分区"));
        let outer = placement(&mbr, sector, 6 * MIB).unwrap();
        output(&mbr, &outer);
        let opposite_end = placement(&mbr, sector, 3 * MIB).unwrap();
        let final_mbr = output(&mbr, &opposite_end);
        assert_ne!(&final_mbr[440..444], &original[440..444]);
        assert_eq!(
            opposite_end
                .output
                .extents
                .iter()
                .filter(|e| e.kind == Kind::Ebr)
                .map(|e| e.offset)
                .collect::<Vec<_>>(),
            (4..=6).map(|i| i * MIB - sector).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn mbr_placement_rejects_unsafe_selection_and_rechecks_the_new_ebr_gap() {
        let sector = 512;
        let gpt = gpt_image(sector, 2, 0);
        let mut image = output(&gpt, &prepare(&gpt, sector, Scheme::Mbr).unwrap());
        assert!(placement(&gpt, sector, MIB).is_err());
        assert!(placement(&image, sector, MIB + sector).is_err());
        image[446] = 0x80;
        assert!(placement(&image, sector, 2 * MIB)
            .err()
            .unwrap()
            .contains("活动"));
        image[446] = 0;
        image[466] = 0xef;
        assert!(placement(&image, sector, 2 * MIB)
            .err()
            .unwrap()
            .contains("0xEF"));
        image[466] = 0x83;
        let start = (MIB + MIB / 2) as usize;
        let end = (2 * MIB) as usize;
        image[start..end].fill(0x41);
        assert!(placement(&image, sector, 2 * MIB)
            .err()
            .unwrap()
            .contains("全零 EBR"));
        let target = end - sector as usize;
        image[target..end].fill(0);
        let plan = placement(&image, sector, 2 * MIB).unwrap();
        image[target + 12] = 1;
        assert!(plan
            .verify_source(&mut Cursor::new(&image), || Ok(()))
            .is_err());
        assert!(plan
            .reader(Cursor::new(&image))
            .unwrap()
            .read_to_end(&mut Vec::new())
            .is_err());
    }

    #[test]
    fn image_table_backup_recovers_both_gpt_copies_and_a_damaged_mbr_chain() {
        use sha2::{Digest, Sha256};

        for sector in [512, 4096] {
            let gpt = gpt_image(sector, 6, 0);
            let mbr = output(&gpt, &prepare(&gpt, sector, Scheme::Mbr).unwrap());
            for (scheme, original) in [(Scheme::Gpt, &gpt), (Scheme::Mbr, &mbr)] {
                let backup =
                    recovery::backup(&mut Cursor::new(original), SIZE, sector, |_| Ok(())).unwrap();
                assert_eq!(backup.source, format!("{:x}", Sha256::digest(original)));
                let backup = Archive::decode(&backup.encode().unwrap()).unwrap();
                let mut damaged = original.clone();
                for extent in backup.extents.iter().filter(|e| e.kind != Kind::Boot) {
                    let start = extent.offset as usize;
                    damaged[start..start + extent.bytes.len()].fill(0xb3);
                }
                assert!(table_archive::capture(
                    &mut Cursor::new(&damaged),
                    SIZE,
                    sector,
                    source_key(),
                    false,
                    || Ok(()),
                )
                .is_err());
                recovery::verify_binding(&mut Cursor::new(&damaged), &backup, |_| Ok(())).unwrap();
                let plan = recovery::prepare(
                    &mut Cursor::new(&damaged),
                    SIZE,
                    sector,
                    backup,
                    "image-table.dspt".into(),
                )
                .unwrap();
                assert_eq!(plan.preview.from, scheme);
                assert_eq!(
                    plan.preview.restored_from.as_deref(),
                    Some("image-table.dspt")
                );
                assert_eq!(plan.preview.partitions.len(), 6);
                assert_eq!(output(&damaged, &plan), *original);
            }
        }
    }

    #[test]
    fn image_table_restore_rejects_legacy_archives_changed_data_and_stale_previews() {
        let sector = 4096;
        let original = gpt_image(sector, 2, 0);
        let backup =
            recovery::backup(&mut Cursor::new(&original), SIZE, sector, |_| Ok(())).unwrap();
        let restore = |source: &[u8], backup| {
            recovery::prepare(
                &mut Cursor::new(source),
                SIZE,
                sector,
                backup,
                "image-table.dspt".into(),
            )
        };
        assert!(restore(&original, backup.clone())
            .err()
            .unwrap()
            .contains("已经"));
        let mut damaged = original.clone();
        damaged[sector as usize] ^= 1;
        let mut legacy = backup.clone();
        legacy.version = 1;
        assert!(restore(&damaged, legacy).is_err());
        let plan = restore(&damaged, backup.clone()).unwrap();
        damaged[sector as usize + 6] ^= 1;
        assert!(plan
            .verify_source(&mut Cursor::new(&damaged), || Ok(()))
            .is_err());
        assert!(plan
            .reader(Cursor::new(&damaged))
            .unwrap()
            .read_to_end(&mut Vec::new())
            .is_err());
        let at = 9 * MIB as usize;
        damaged[at] ^= 1;
        assert!(
            recovery::verify_binding(&mut Cursor::new(&damaged), &backup, |_| Ok(()))
                .err()
                .unwrap()
                .contains("备份表扇区之外")
        );
        damaged[at] ^= 1;
        damaged[MIB as usize + 20] ^= 1;
        assert!(restore(&damaged, backup.clone()).is_err());
        assert!(
            recovery::backup(&mut Cursor::new(&original), SIZE, sector, |done| {
                if done >= MIB {
                    Err("cancelled".into())
                } else {
                    Ok(())
                }
            })
            .err()
            .unwrap()
            .contains("cancelled")
        );
        let mut geometry = backup;
        geometry.sector = 512;
        assert!(restore(&damaged, geometry).is_err());
    }

    #[test]
    fn round_trip_with_three_primary_and_three_logical_data_partitions() {
        for sector in [512, 4096] {
            let original = gpt_image(sector, 6, 0);
            let to_mbr = prepare(&original, sector, Scheme::Mbr).unwrap();
            assert_eq!(
                to_mbr
                    .preview
                    .partitions
                    .iter()
                    .filter(|p| p.logical)
                    .count(),
                3
            );
            let mbr = output(&original, &to_mbr);
            assert_eq!(mbr[9 * MIB as usize], 0xb3);
            assert_eq!(&mbr[sector as usize..sector as usize + 8], &[0; 8]);
            let tail = mbr.len() - sector as usize;
            assert_eq!(&mbr[tail..tail + 8], &[0; 8]);

            let to_gpt = prepare(&mbr, sector, Scheme::Gpt).unwrap();
            let gpt = output(&mbr, &to_gpt);
            assert_eq!(gpt[9 * MIB as usize], 0xb3);
            for sector in to_mbr.output.extents.iter().filter(|e| e.kind == Kind::Ebr) {
                let start = sector.offset as usize;
                assert!(gpt[start..start + sector.bytes.len()]
                    .iter()
                    .all(|&b| b == 0));
            }
            assert!(to_gpt
                .preview
                .partitions
                .iter()
                .all(|p| p.old_type == "0x83" && p.new_type == LINUX_DATA));
        }
    }

    #[test]
    fn recognizes_fat_from_its_bpb_and_verifies_the_normal_raw_encoder() {
        let sector = 512;
        let fat = Plan::new(FatType::Fat12, MIB, 512, 512, "CONVERT", 711, 0).unwrap();
        let mut volume = Cursor::new(vec![0u8; MIB as usize]);
        fat.format(&mut volume, |_, _| Ok(())).unwrap();
        let mut image = vec![0; SIZE as usize];
        let mut mbr = empty_mbr(sector);
        mbr_record(&mut mbr, 0, 0x01, MIB / sector, MIB / sector).unwrap();
        image[..512].copy_from_slice(&mbr);
        image[MIB as usize..2 * MIB as usize].copy_from_slice(&volume.into_inner());
        let conversion = prepare(&image, sector, Scheme::Gpt).unwrap();
        assert_eq!(conversion.preview.partitions[0].new_type, BASIC_DATA);
        let mut encoder = Encoder::new(
            Cursor::new(Vec::new()),
            Options {
                format: Format::Raw,
                size: SIZE,
                sector,
            },
            Identity::for_output("conversion.img".into()).unwrap(),
        )
        .unwrap();
        let expected = output(&image, &conversion);
        for verify in [false, true] {
            let mut reader = conversion.reader(Cursor::new(&image)).unwrap();
            for i in 0..SIZE.div_ceil(crate::image_convert::CHUNK as u64) {
                let offset = i * crate::image_convert::CHUNK as u64;
                let mut bytes =
                    vec![0; (SIZE - offset).min(crate::image_convert::CHUNK as u64) as usize];
                reader.read_exact(&mut bytes).unwrap();
                if verify {
                    encoder.verify_source(offset, &bytes).unwrap();
                } else {
                    encoder.push(offset, &bytes).unwrap();
                }
            }
        }
        let (mut result, _) = encoder.finish(|_| Ok(()), |_| Ok(())).unwrap();
        conversion.verify_output(&mut result, || Ok(())).unwrap();
        assert_eq!(result.into_inner(), expected);
        let back = prepare(&expected, sector, Scheme::Mbr).unwrap();
        assert_eq!(back.preview.partitions[0].new_type, "0x01");
    }

    #[test]
    fn refuses_active_protected_and_unavailable_table_space() {
        let sector = 512;
        let protected = gpt_image(sector, 1, 1u64 << 60);
        assert!(prepare(&protected, sector, Scheme::Mbr).is_err());

        let mut filled = gpt_image(sector, 5, 0);
        filled[3 * MIB as usize + MIB as usize / 2..4 * MIB as usize].fill(0x41);
        assert!(prepare(&filled, sector, Scheme::Mbr)
            .err()
            .unwrap()
            .contains("全零 EBR"));

        let mut image = vec![0; SIZE as usize];
        let mut mbr = empty_mbr(sector);
        mbr_record(&mut mbr, 0, 0x83, MIB / sector, MIB / sector).unwrap();
        mbr[446] = 0x80;
        image[..512].copy_from_slice(&mbr);
        assert!(prepare(&image, sector, Scheme::Gpt)
            .err()
            .unwrap()
            .contains("活动"));
        image[446] = 0;
        image[sector as usize + 27] = 1;
        assert!(prepare(&image, sector, Scheme::Gpt)
            .err()
            .unwrap()
            .contains("非零"));
        image[sector as usize + 27] = 0;
        let plan = prepare(&image, sector, Scheme::Gpt).unwrap();
        image[sector as usize + 27] = 1;
        assert!(plan
            .verify_source(&mut Cursor::new(&image), || Ok(()))
            .is_err());
        assert!(plan
            .reader(Cursor::new(&image))
            .unwrap()
            .read_to_end(&mut Vec::new())
            .is_err());
    }
}

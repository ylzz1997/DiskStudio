//! On-disk LDM structures, as documented by the Linux-NTFS project.
//! https://flatcap.github.io/linux-ntfs/ldm/technical/
use crate::{table_check::checked_gpt, MIB};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Seek, SeekFrom},
    ops::Range,
};

pub(super) type Guid = [u8; 16];
pub(super) type Id = u64;
const METADATA: Guid = [
    0xaa, 0xc8, 0x08, 0x58, 0x8f, 0x7e, 0xe0, 0x42, 0x85, 0xd2, 0xe1, 0xe9, 0x04, 0x34, 0xcf, 0xb3,
];
const DATA: Guid = [
    0xa0, 0x60, 0x9b, 0xaf, 0x31, 0x14, 0x62, 0x4f, 0xbc, 0x68, 0x33, 0x11, 0x71, 0x4a, 0x69, 0xad,
];
const INVALID: &str = "LDM 元数据无效、越界或副本不一致。";

pub(super) fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub(super) fn guid_string(guid: Guid) -> String {
    guid.iter()
        .enumerate()
        .fold(String::new(), |mut id, (i, b)| {
            use std::fmt::Write as _;
            if matches!(i, 4 | 6 | 8 | 10) {
                id.push('-');
            }
            write!(id, "{b:02x}").unwrap();
            id
        })
}

fn guid(bytes: &[u8]) -> Result<Guid, String> {
    let value = text(bytes);
    let value = value
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(&value);
    let value = value.as_bytes();
    if value.len() != 36 {
        return Err(INVALID.into());
    }
    let mut hex = [0; 32];
    let mut next = 0;
    for (i, &byte) in value.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return Err(INVALID.into());
            }
        } else {
            hex[next] = char::from(byte).to_digit(16).ok_or(INVALID)? as u8;
            next += 1;
        }
    }
    let result = std::array::from_fn(|i| hex[2 * i] * 16 + hex[2 * i + 1]);
    nonzero_guid(result)
}

fn nonzero_guid(bytes: Guid) -> Result<Guid, String> {
    (bytes != [0; 16])
        .then_some(bytes)
        .ok_or_else(|| INVALID.into())
}

fn range(start: u64, count: u64, sector: u64, limit: u64) -> Result<Range<u64>, String> {
    let start = start.checked_mul(sector).ok_or(INVALID)?;
    let size = count.checked_mul(sector).ok_or(INVALID)?;
    let end = start
        .checked_add(size)
        .filter(|&end| size != 0 && end <= limit)
        .ok_or(INVALID)?;
    Ok(start..end)
}

fn read(reader: &mut (impl Read + Seek), start: u64, bytes: &mut [u8]) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(start))
        .and_then(|_| reader.read_exact(bytes))
        .map_err(|error| format!("无法读取 LDM 元数据：{error}"))
}

fn checksum(block: &[u8], signature: &[u8; 8]) -> Result<(), String> {
    let sum: u32 = block[..8]
        .iter()
        .chain(&block[12..])
        .map(|&byte| u32::from(byte))
        .sum();
    if &block[..8] != signature || be32(block, 8) != sum {
        return Err(INVALID.into());
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
pub(super) struct Private {
    pub(super) disk: Guid,
    pub(super) group: Guid,
    pub(super) public: Range<u64>,
    pub(super) sector: u64,
    area: Range<u64>,
    version: u16,
    update: u64,
    layout_digest: [u8; 32],
    headers: [u64; 2],
    tocs: [u64; 2],
}

fn private(
    block: &[u8; 512],
    sector: u64,
    size: u64,
    layout_digest: [u8; 32],
) -> Result<Private, String> {
    checksum(block, b"PRIVHEAD")?;
    let version = be16(block, 14);
    if be16(block, 12) != 2
        || !matches!(version, 11 | 12)
        || u64::from(be32(block, 0x10f)) != sector
    {
        return Err("暂不支持此 LDM 私有头版本或扇区规格。".into());
    }
    let public = range(be64(block, 0x11b), be64(block, 0x123), sector, size)?;
    let area = range(be64(block, 0x12b), be64(block, 0x133), sector, size)?;
    if public.start == 0
        || area.start == 0
        || overlaps(&public, &area)
        || area.end - area.start > 4 * MIB
    {
        return Err(INVALID.into());
    }
    Ok(Private {
        disk: guid(&block[0x30..0x70])?,
        group: guid(&block[0xb0..0xf0])?,
        public,
        sector,
        area,
        version,
        update: be64(block, 0x18),
        layout_digest,
        headers: [be64(block, 0x20), be64(block, 0x28)],
        tocs: [be64(block, 0x13b), be64(block, 0x143)],
    })
}

fn one_range(entries: &[([u8; 16], Range<u64>)], kind: Guid) -> Result<Range<u64>, String> {
    let mut matching = entries.iter().filter(|(ty, _)| *ty == kind);
    let (_, found) = matching.next().ok_or("GPT 中缺少 LDM 数据或元数据分区。")?;
    if matching.next().is_some() {
        return Err("GPT 中存在多个 LDM 数据或元数据分区。".into());
    }
    Ok(found.clone())
}

fn read_private(reader: &mut (impl Read + Seek), sector: u64) -> Result<Private, String> {
    let size = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    if !matches!(sector, 512 | 4096) || !(4 * MIB..=1 << 44).contains(&size) || size % sector != 0 {
        return Err("LDM 成员容量或逻辑扇区大小无效。".into());
    }
    let mut mbr = [0; 512];
    read(reader, 0, &mut mbr)?;
    if mbr[510..] != [0x55, 0xaa] {
        return Err("LDM 成员缺少有效的 MBR 签名。".into());
    }
    let entries = &mbr[446..510];
    let mut block = [0; 512];
    if entries.chunks_exact(16).any(|entry| entry[4] == 0xee) {
        let mut used = entries
            .chunks_exact(16)
            .filter(|entry| entry.iter().any(|&byte| byte != 0));
        let pmbr = used.next().ok_or(INVALID)?;
        let length = u64::from(le32(pmbr, 12));
        if pmbr[0] != 0
            || pmbr[4] != 0xee
            || le32(pmbr, 8) != 1
            || (length != size / sector - 1 && length != u64::from(u32::MAX))
            || used.next().is_some()
        {
            return Err("LDM 成员的 GPT 保护性 MBR 不一致。".into());
        }
        let window = (MIB + 2 * sector) as usize;
        let mut head = vec![0; window];
        let mut tail = vec![0; window];
        read(reader, 0, &mut head)?;
        read(reader, size - window as u64, &mut tail)?;
        let gpt = checked_gpt(&head, &tail, size, sector)?;
        let metadata = one_range(&gpt.partitions, METADATA)?;
        let data = one_range(&gpt.partitions, DATA)?;
        read(reader, metadata.end - sector, &mut block)?;
        let header = private(&block, sector, size, gpt.digest)?;
        if header.area != metadata || header.public != data {
            return Err("LDM 私有头的范围与 GPT 条目不一致。".into());
        }
        return Ok(header);
    }

    read(reader, 6 * sector, &mut block)?;
    let mut digest = Sha256::new();
    digest.update(size.to_le_bytes());
    digest.update(sector.to_le_bytes());
    digest.update(mbr);
    let header = private(&block, sector, size, digest.finalize().into())?;
    if header.public.start < 7 * sector || header.area.start < 7 * sector {
        return Err(INVALID.into());
    }
    let mut has_ldm = false;
    for entry in entries.chunks_exact(16).filter(|entry| entry[4] != 0) {
        let extent = range(
            u64::from(le32(entry, 8)),
            u64::from(le32(entry, 12)),
            sector,
            size,
        )?;
        if entry[4] == 0x42 {
            has_ldm = true;
            if extent.start < header.public.start || extent.end > header.public.end {
                return Err(INVALID.into());
            }
        } else if overlaps(&extent, &header.public) || overlaps(&extent, &header.area) {
            return Err("LDM 范围与普通 MBR 分区重叠。".into());
        }
    }
    if !has_ldm {
        return Err("MBR 中缺少 0x42 动态磁盘条目。".into());
    }
    Ok(header)
}

fn config(block: &[u8; 512], header: &Private) -> Result<Range<u64>, String> {
    checksum(block, b"TOCBLOCK")?;
    if be64(block, 12) != header.update {
        return Err("LDM 目录与私有头的提交序列不一致。".into());
    }
    let mut config = None;
    let mut log = None;
    for at in [0x24, 0x46] {
        let target = match &block[at..at + 8] {
            b"config\0\0" => &mut config,
            b"log\0\0\0\0\0" => &mut log,
            _ => return Err(INVALID.into()),
        };
        if target.is_some() {
            return Err(INVALID.into());
        }
        *target = Some(range(
            be64(block, at + 10),
            be64(block, at + 18),
            header.sector,
            header.area.end - header.area.start,
        )?);
    }
    let config = config.ok_or(INVALID)?;
    let log = log.ok_or(INVALID)?;
    if overlaps(&config, &log) {
        return Err(INVALID.into());
    }
    let headers = header.headers.into_iter().chain(header.tocs);
    for lba in headers {
        let location = range(lba, 1, header.sector, header.area.end - header.area.start)?;
        if overlaps(&location, &config) || overlaps(&location, &log) {
            return Err(INVALID.into());
        }
    }
    Ok(config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    Linear,
    Stripe,
    Parity,
}

pub(super) struct Disk {
    pub(super) guid: Guid,
}

pub(super) struct Volume {
    pub(super) name: String,
    pub(super) id: Guid,
    pub(super) components: u64,
    pub(super) blocks: u64,
    pub(super) parity: bool,
    pub(super) clean: bool,
}

pub(super) struct Component {
    pub(super) volume: Id,
    pub(super) kind: Layout,
    pub(super) extents: u64,
    pub(super) chunk: u64,
    pub(super) columns: u64,
    pub(super) clean: bool,
}

#[derive(Clone)]
pub(super) struct Extent {
    pub(super) component: Id,
    pub(super) disk: Id,
    pub(super) start: u64,
    pub(super) position: u64,
    pub(super) blocks: u64,
    pub(super) column: u64,
    pub(super) clean: bool,
}

#[derive(Default)]
pub(super) struct Objects {
    group: Option<Id>,
    ids: HashSet<Id>,
    pub(super) disks: BTreeMap<Id, Disk>,
    pub(super) volumes: BTreeMap<Id, Volume>,
    pub(super) components: BTreeMap<Id, Component>,
    pub(super) extents: BTreeMap<Id, Extent>,
}

struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let bytes = self.0.get(..count).ok_or(INVALID)?;
        self.0 = &self.0[count..];
        Ok(bytes)
    }

    fn variable(&mut self) -> Result<&'a [u8], String> {
        let count = self.take(1)?[0] as usize;
        self.take(count)
    }

    fn number(&mut self) -> Result<u64, String> {
        let bytes = self.variable()?;
        if bytes.len() > 8 {
            return Err(INVALID.into());
        }
        Ok(bytes
            .iter()
            .fold(0, |value, byte| (value << 8) | u64::from(*byte)))
    }

    fn id(&mut self) -> Result<Id, String> {
        let id = self.number()?;
        (id != 0).then_some(id).ok_or_else(|| INVALID.into())
    }
}

impl Objects {
    fn record(&mut self, bytes: &[u8]) -> Result<(), String> {
        let length = be32(bytes, 4) as usize;
        if be16(bytes, 0) != 0 || length == 0 {
            return Err("LDM 记录未提交或长度无效。".into());
        }
        let end = 8usize.checked_add(length).ok_or(INVALID)?;
        let mut fields = Fields(bytes.get(8..end).ok_or(INVALID)?);
        let id = fields.id()?;
        let name = fields.variable()?;
        if !self.ids.insert(id) {
            return Err("LDM 对象 ID 重复。".into());
        }
        match bytes[3] {
            0x35 | 0x45 if self.group.is_none() => self.group = Some(id),
            0x34 | 0x44 => {
                let guid = if bytes[3] == 0x34 {
                    guid(fields.variable()?)?
                } else {
                    nonzero_guid(fields.take(16)?.try_into().unwrap())?
                };
                self.disks.insert(id, Disk { guid });
            }
            0x51 => {
                let kind = fields.variable()?;
                if !matches!(kind, b"gen" | b"raid5") {
                    return Err("暂不支持此 LDM 卷类型。".into());
                }
                fields.variable()?;
                let active = text(fields.take(14)?) == "ACTIVE";
                fields.take(1)?; // Read policy is independent of volume layout.
                fields.number()?; // Volume number.
                let flags = be32(fields.take(4)?, 0);
                let components = fields.id()?;
                fields.take(16)?; // Object update transaction IDs.
                let blocks = fields.id()?;
                fields.take(5)?; // Reserved bytes and the legacy filesystem hint.
                let volume_guid = nonzero_guid(fields.take(16)?.try_into().unwrap())?;
                self.volumes.insert(
                    id,
                    Volume {
                        name: text(name),
                        id: volume_guid,
                        components,
                        blocks,
                        parity: kind == b"raid5",
                        clean: active && flags & 0x0000_3504 == 0,
                    },
                );
            }
            0x32 => {
                let active = fields.variable()? == b"ACTIVE";
                let kind = match fields.take(1)?[0] {
                    1 => Layout::Stripe,
                    2 => Layout::Linear,
                    3 => Layout::Parity,
                    _ => return Err("暂不支持此 LDM 组件类型。".into()),
                };
                let flags = be32(fields.take(4)?, 0);
                let extents = fields.id()?;
                fields.take(16)?;
                let volume = fields.id()?;
                fields.number()?; // Log subdisk.
                let (chunk, columns) = if bytes[2] & 0x10 != 0 {
                    (fields.id()?, fields.id()?)
                } else {
                    (0, 0)
                };
                self.components.insert(
                    id,
                    Component {
                        volume,
                        kind,
                        extents,
                        chunk,
                        columns,
                        clean: active && flags & 0x0e == 0,
                    },
                );
            }
            0x33 => {
                let flags = be32(fields.take(4)?, 0);
                fields.take(8)?;
                let start = be64(fields.take(8)?, 0);
                let position = be64(fields.take(8)?, 0);
                let blocks = fields.id()?;
                let component = fields.id()?;
                let disk = fields.id()?;
                let column = if bytes[2] & 0x08 != 0 {
                    fields.number()?
                } else {
                    0
                };
                self.extents.insert(
                    id,
                    Extent {
                        component,
                        disk,
                        start,
                        position,
                        blocks,
                        column,
                        clean: flags & 0x38 == 0,
                    },
                );
            }
            _ => return Err("暂不支持此 LDM 记录及版本。".into()),
        }
        Ok(())
    }
}

pub(super) struct Database {
    pub(super) header: Private,
    pub(super) sequence: u64,
    pub(super) name: String,
    pub(super) digest: [u8; 32],
    pub(super) member_digest: [u8; 32],
    pub(super) objects: Objects,
}

impl Database {
    pub(super) fn read(reader: &mut (impl Read + Seek), sector: u64) -> Result<Self, String> {
        let header = read_private(reader, sector)?;
        let mut block = [0; 512];
        let slots: HashSet<_> = header.headers.into_iter().chain(header.tocs).collect();
        if slots.len() != 4
            || slots
                .iter()
                .any(|&lba| lba >= (header.area.end - header.area.start) / sector)
        {
            return Err(INVALID.into());
        }
        let size = reader
            .seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        for lba in header.headers {
            read(reader, header.area.start + lba * sector, &mut block)?;
            if private(&block, sector, size, header.layout_digest)? != header {
                return Err("LDM 私有头副本不一致。".into());
            }
        }
        let mut locations = Vec::with_capacity(2);
        for lba in header.tocs {
            read(reader, header.area.start + lba * sector, &mut block)?;
            locations.push(
                config(&block, &header).map_err(|error| format!("LDM TOCBLOCK {lba}：{error}"))?,
            );
        }
        let db = &locations[0];
        if *db != locations[1] {
            return Err("LDM 配置目录副本不一致。".into());
        }
        let mut raw = vec![0; (db.end - db.start) as usize];
        read(reader, header.area.start + db.start, &mut raw)?;
        Self::parse(header, &raw)
    }

    fn parse(header: Private, raw: &[u8]) -> Result<Self, String> {
        if raw.len() < 512 || &raw[..4] != b"VMDB" || be16(raw, 18) != 4 || be16(raw, 20) != 10 {
            return Err("LDM 配置区域缺少受支持的 VMDB 4.10 头。".into());
        }
        let sequence = be64(raw, 0x75);
        if be16(raw, 16) != 1
            || be64(raw, 0x7d) != sequence
            || guid(&raw[0x35..0x75])? != header.group
        {
            return Err("LDM 数据库未清洁提交，或磁盘组身份不匹配。".into());
        }
        let stride = be32(raw, 8) as usize;
        let start = be32(raw, 12) as usize;
        let last = be32(raw, 4) as usize;
        let end = last.checked_mul(stride).ok_or(INVALID)?;
        if !matches!(stride, 128 | 256 | 512)
            || start < 512
            || start % stride != 0
            || start >= end
            || end > raw.len()
        {
            return Err(INVALID.into());
        }
        let mut groups = BTreeMap::<u32, Vec<Option<&[u8]>>>::new();
        for (index, record) in raw[start..end].chunks_exact(stride).enumerate() {
            if &record[..4] != b"VBLK" {
                return Err("LDM 数据库缺少应有的 VBLK 记录。".into());
            }
            let count = be16(record, 14) as usize;
            if count == 0 {
                continue;
            }
            let id = be32(record, 8);
            let part = be16(record, 12) as usize;
            if count > 64
                || part >= count
                || id == 0
                || be32(record, 4) as usize != start / stride + index
            {
                return Err(INVALID.into());
            }
            let fragments = groups.entry(id).or_insert_with(|| vec![None; count]);
            if fragments.len() != count || fragments[part].replace(&record[16..]).is_some() {
                return Err("LDM VBLK 分片重复或总数不一致。".into());
            }
        }
        let mut objects = Objects::default();
        for (id, fragments) in groups {
            let mut bytes = Vec::with_capacity(fragments.len() * (stride - 16));
            for fragment in fragments {
                bytes.extend_from_slice(fragment.ok_or("LDM VBLK 记录缺少分片。")?);
            }
            objects.record(&bytes).map_err(|error| {
                format!("LDM VBLK 组 {id}，记录类型 {:#04x}：{error}", bytes[3])
            })?;
        }
        let counts = [
            objects.volumes.len(),
            objects.components.len(),
            objects.extents.len(),
            objects.disks.len(),
        ];
        if objects.group.is_none()
            || counts
                .into_iter()
                .zip([0x85, 0x89, 0x8d, 0x91])
                .any(|(count, at)| count != be32(raw, at) as usize)
            || be32(raw, 0x95) != 0
        {
            return Err("LDM 数据库的已提交记录数不一致或包含不支持的对象。".into());
        }
        let mut member = Sha256::new();
        member.update(header.layout_digest);
        member.update(header.area.start.to_le_bytes());
        member.update(header.area.end.to_le_bytes());
        member.update(header.update.to_le_bytes());
        Ok(Self {
            header,
            sequence,
            name: text(&raw[0x16..0x35]),
            digest: Sha256::digest(raw).into(),
            member_digest: member.finalize().into(),
            objects,
        })
    }
}

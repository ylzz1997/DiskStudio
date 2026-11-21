//! Offline Storage Spaces metadata validation over caller-supplied read-only streams.
//! The record framing is described by Kim's Storage Space Reconstructor research.
//! https://github.com/KimVegetable/StorageSpaceReconstructor
use crate::{table_check::checked_gpt, MIB};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    ops::Range,
};

#[path = "storage_spaces_cache.rs"]
mod cache;
#[path = "storage_spaces_layout.rs"]
mod layout;
pub use layout::{Group, SpaceReader};

pub type Guid = [u8; 16];
const MEMBER_TYPE: Guid = [
    0x8f, 0xaf, 0x5c, 0xe7, 0x80, 0xf6, 0xee, 0x4c, 0xaf, 0xa3, 0xb0, 0x01, 0xe5, 0x6e, 0xfc, 0x2d,
];
const GPT_EDGE: usize = 64 * 1024;
const SDBC_OFFSET: u64 = 4096;
const HEADER_SIZE: usize = 512;
const ENTRY_SIZE: usize = 64;
const FIRST_SLOT: u32 = 8;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_DATABASE_BYTES: usize = 8 * MIB as usize;
const INVALID: &str = "存储空间成员的元数据、记录或引用无效。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    pub id: u64,
    pub guid: Guid,
    pub name: String,
    pub description: String,
    group: u64,
    layout: Option<layout::SpaceLayout>,
}

/// An allocation record as stored on disk. Block indices have meaning only
/// after validating the virtual disk's layout and internal space references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub flags: u8,
    pub group: u64,
    pub span: u64,
    pub space: u64,
    pub virtual_block: u64,
    pub column: u64,
    pub copy: u64,
    pub member: u64,
    pub member_block: u64,
}

#[derive(Debug)]
pub struct Member {
    pub pool: Guid,
    pub name: String,
    pub member: Guid,
    pub partition: Range<u64>,
    pub layout_digest: [u8; 32],
    pub database_digest: [u8; 32],
    pub members: BTreeMap<u64, Guid>,
    pub spaces: BTreeMap<u64, Space>,
    pub allocation_owners: BTreeMap<u64, Guid>,
    pub allocations: Vec<Allocation>,
}

fn u16be(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(bytes))
        .map_err(|error| format!("无法读取存储空间成员：{error}"))
}

fn header_crc(header: &[u8; HEADER_SIZE]) -> bool {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&header[..12]);
    crc.update(&[0; 4]);
    crc.update(&header[16..]);
    crc.finalize() == u32be(header, 12)
}

fn headers(
    disk: &[u8; HEADER_SIZE],
    db: &[u8; HEADER_SIZE],
) -> Result<(Guid, Guid, usize), String> {
    if &disk[..8] != b"SPACEDB " || &db[..8] != b"SDBC    " || !header_crc(disk) || !header_crc(db)
    {
        return Err("存储空间元数据的签名或 CRC32 校验失败。".into());
    }
    if u32be(disk, 8) != 0x0003_0200 || u32be(db, 8) != 0x0001_0200 {
        return Err("此存储空间元数据版本暂不支持。".into());
    }
    let pool: Guid = disk[0x20..0x30].try_into().unwrap();
    let member: Guid = disk[0x30..0x40].try_into().unwrap();
    let length = (u32be(db, 0x28).checked_sub(FIRST_SLOT).ok_or(INVALID)? as usize)
        .checked_mul(ENTRY_SIZE)
        .filter(|length| *length != 0 && *length <= MAX_DATABASE_BYTES)
        .ok_or(INVALID)?;
    if pool == [0; 16]
        || member == [0; 16]
        || pool == member
        || db[0x10..0x20] != pool
        || u32be(db, 0x24) != ENTRY_SIZE as u32
    {
        return Err(INVALID.into());
    }
    Ok((pool, member, length))
}

struct Record<'a>(&'a [u8]);

impl<'a> Record<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], &'static str> {
        let (head, tail) = self.0.split_at_checked(length).ok_or(INVALID)?;
        self.0 = tail;
        Ok(head)
    }

    fn number(&mut self) -> Result<u64, &'static str> {
        let length = self.take(1)?[0] as usize;
        if length > 8 {
            return Err(INVALID);
        }
        Ok(self
            .take(length)?
            .iter()
            .fold(0, |n, &byte| (n << 8) | u64::from(byte)))
    }

    fn guid(&mut self) -> Result<Guid, &'static str> {
        let guid = self.take(16)?.try_into().unwrap();
        if guid == [0; 16] {
            return Err(INVALID);
        }
        Ok(guid)
    }

    fn text(&mut self) -> Result<String, &'static str> {
        let bytes = u16be(self.take(2)?, 0) as usize * 2;
        let words = self.take(bytes)?.chunks_exact(2).map(|w| u16be(w, 0));
        let mut text: String = char::decode_utf16(words)
            .collect::<Result<_, _>>()
            .map_err(|_| INVALID)?;
        if text.ends_with('\0') {
            text.pop();
        }
        if text.contains('\0') {
            return Err(INVALID);
        }
        Ok(text)
    }

    fn allocation(&mut self) -> Result<Allocation, &'static str> {
        self.number()?;
        let group = self.number()?;
        self.number()?;
        let flags = self.take(1)?[0];
        let span = self.number()?;
        let space = self.number()?;
        let virtual_block = self.number()?;
        let column = self.number()?;
        let copy = self.number()?;
        self.number()?;
        let member = self.number()?;
        let member_block = self.number()?;
        if !self.0.is_empty() {
            return Err(INVALID);
        }
        Ok(Allocation {
            flags,
            group,
            span,
            space,
            virtual_block,
            column,
            copy,
            member,
            member_block,
        })
    }
}

#[derive(Default)]
struct Objects {
    pool: Option<(Guid, String)>,
    members: BTreeMap<u64, Guid>,
    spaces: BTreeMap<u64, Space>,
    allocation_owners: BTreeMap<u64, Guid>,
    allocations: Vec<Allocation>,
}

impl Objects {
    fn add(&mut self, kind: u8, revision: u8, mut record: Record<'_>) -> Result<(), &'static str> {
        match (kind, revision) {
            (1, 0x10) => {
                record.number()?;
                record.number()?;
                let item = (record.guid()?, record.text()?);
                if self.pool.replace(item).is_some() {
                    return Err(INVALID);
                }
            }
            (2, 8) | (3, 0x11) | (6, 5) => {
                let id = record.number()?;
                let group = record.number()?;
                let guid = record.guid()?;
                if id == 0
                    || self.members.contains_key(&id)
                    || self.spaces.contains_key(&id)
                    || self.allocation_owners.contains_key(&id)
                {
                    return Err(INVALID);
                }
                if kind == 2 {
                    self.members.insert(id, guid);
                } else if kind == 3 {
                    let name = record.text()?;
                    let description = record.text()?;
                    let layout = layout::space(record).ok();
                    self.spaces.insert(
                        id,
                        Space {
                            id,
                            guid,
                            name,
                            description,
                            group,
                            layout,
                        },
                    );
                } else {
                    self.allocation_owners.insert(id, guid);
                }
            }
            (4, 6) => self.allocations.push(record.allocation()?),
            // Additional records are framed and included in the database digest.
            (5 | 7..=u8::MAX, _) => (),
            _ => return Err(INVALID),
        }
        Ok(())
    }
}

fn objects(mut table: &[u8]) -> Result<Objects, String> {
    let mut slot = FIRST_SLOT;
    let mut result = Objects::default();
    while !table.is_empty() {
        let block = &table[..ENTRY_SIZE];
        let count = u16be(block, 14) as usize;
        if count == 0 {
            if !block.iter().all(|&byte| byte == 0)
                && (&block[..4] != b"SDBB" || u32be(block, 4) != slot)
            {
                return Err(INVALID.into());
            }
            table = &table[ENTRY_SIZE..];
            slot += 1;
            continue;
        }
        let length = u32be(block, 20) as usize;
        let framed = length.checked_add(8).ok_or(INVALID)?;
        if length == 0 || length > MAX_RECORD_BYTES || count != framed.div_ceil(ENTRY_SIZE - 16) {
            return Err(INVALID.into());
        }
        let (fragments, remaining) = table.split_at_checked(count * ENTRY_SIZE).ok_or(INVALID)?;
        let mut payload = Vec::with_capacity(framed);
        for (index, fragment) in fragments.chunks_exact(ENTRY_SIZE).enumerate() {
            if &fragment[..4] != b"SDBB"
                || u32be(fragment, 4) != slot + index as u32
                || u32be(fragment, 8) != slot
                || u16be(fragment, 12) as usize != index
                || u16be(fragment, 14) as usize != count
            {
                return Err(INVALID.into());
            }
            let bytes = (framed - payload.len()).min(ENTRY_SIZE - 16);
            payload.extend_from_slice(&fragment[16..16 + bytes]);
        }
        result.add(payload[0], payload[1], Record(&payload[8..]))?;
        table = remaining;
        slot += count as u32;
    }
    let guids: BTreeSet<_> = result
        .members
        .values()
        .chain(result.spaces.values().map(|space| &space.guid))
        .chain(result.allocation_owners.values())
        .collect();
    if guids.len() != result.members.len() + result.spaces.len() + result.allocation_owners.len()
        || result.allocations.iter().any(|item| {
            !result.members.contains_key(&item.member)
                || !(result.spaces.contains_key(&item.space)
                    || result.allocation_owners.contains_key(&item.space))
        })
    {
        return Err(INVALID.into());
    }
    Ok(result)
}

/// Inspect a single member. This never interprets an allocation as a readable logical disk.
pub fn read_member<R: Read + Seek>(reader: &mut R, sector: u64) -> Result<Member, String> {
    let size = reader
        .seek(SeekFrom::End(0))
        .map_err(|err| err.to_string())?;
    if !matches!(sector, 512 | 4096) || size % sector != 0 || size < 2 * GPT_EDGE as u64 {
        return Err(INVALID.into());
    }
    let mut head = vec![0; GPT_EDGE];
    let mut tail = vec![0; GPT_EDGE];
    read_at(reader, 0, &mut head)?;
    read_at(reader, size - GPT_EDGE as u64, &mut tail)?;
    let layout = checked_gpt(&head, &tail, size, sector)?;
    let mut partitions = layout
        .partitions
        .into_iter()
        .filter(|(kind, _)| *kind == MEMBER_TYPE);
    let (_, partition) = partitions.next().ok_or("未找到存储空间的 GPT 成员分区。")?;
    let sdbc = partition.start.checked_add(SDBC_OFFSET).ok_or(INVALID)?;
    let first = sdbc
        .checked_add(HEADER_SIZE as u64)
        .filter(|end| *end <= partition.end)
        .ok_or(INVALID)?;
    if partitions.next().is_some() {
        return Err(INVALID.into());
    }
    let mut disk = [0; HEADER_SIZE];
    let mut db = [0; HEADER_SIZE];
    read_at(reader, partition.start, &mut disk)?;
    read_at(reader, sdbc, &mut db)?;
    let (pool, member, length) = headers(&disk, &db)?;
    if first
        .checked_add(length as u64)
        .is_none_or(|end| end > partition.end)
    {
        return Err(INVALID.into());
    }
    let mut records = vec![0; length];
    read_at(reader, first, &mut records)?;
    let mut observed = [0; HEADER_SIZE];
    read_at(reader, partition.start, &mut observed)?;
    if observed != disk {
        return Err("读取期间存储空间成员身份发生变化。".into());
    }
    read_at(reader, sdbc, &mut observed)?;
    if observed != db {
        return Err("读取期间存储空间数据库发生变化。".into());
    }
    let Objects {
        pool: pool_record,
        members,
        spaces,
        allocation_owners,
        allocations,
    } = objects(&records)?;
    let (pool_record, name) = pool_record.ok_or(INVALID)?;
    if pool != pool_record || !members.values().any(|id| *id == member) {
        return Err(INVALID.into());
    }
    let mut digest = Sha256::new();
    digest.update(db);
    digest.update(records);
    Ok(Member {
        pool,
        name,
        member,
        partition,
        layout_digest: layout.digest,
        database_digest: digest.finalize().into(),
        members,
        spaces,
        allocation_owners,
        allocations,
    })
}

/// Match independently read members to one database. Return declared member IDs
/// absent from this selection. Stale copies require a separate recovery policy.
pub fn missing_members(selection: &[Member]) -> Result<Vec<u64>, String> {
    let reference = selection.first().ok_or("请选择存储空间成员。")?;
    let mut present = BTreeSet::new();
    for member in selection {
        if member.pool != reference.pool
            || member.database_digest != reference.database_digest
            || member.members != reference.members
            || !present.insert(member.member)
        {
            return Err("存储空间成员重复、属于不同池或数据库副本不一致。".into());
        }
    }
    Ok(reference
        .members
        .iter()
        .filter_map(|(id, guid)| (!present.contains(guid)).then_some(*id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_rejects_fragment_count_and_position_mismatches() {
        let mut blocks = [0; 2 * ENTRY_SIZE];
        for (index, block) in blocks.chunks_exact_mut(ENTRY_SIZE).enumerate() {
            block[..4].copy_from_slice(b"SDBB");
            block[4..8].copy_from_slice(&(FIRST_SLOT + index as u32).to_be_bytes());
            block[8..12].copy_from_slice(&FIRST_SLOT.to_be_bytes());
            block[12..14].copy_from_slice(&(index as u16).to_be_bytes());
            block[14..16].copy_from_slice(&2u16.to_be_bytes());
        }
        blocks[16] = 5;
        blocks[20..24].copy_from_slice(&50u32.to_be_bytes());
        assert!(objects(&blocks).is_ok());
        assert!(objects(&blocks[..ENTRY_SIZE]).is_err());
        blocks[ENTRY_SIZE + 13] = 0;
        assert!(objects(&blocks).is_err());
    }
}

//! Validation for the witnessed standalone write-back cache format.
use super::Guid;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const SECTOR: usize = 4096;
const SLOT_START: usize = 2 * SECTOR;
const SLOTS: usize = 1024;
pub(super) const DIRECTORY_END: usize = SLOT_START + SLOTS * SECTOR;
const CHECKPOINT_BYTES: usize = 0x31000;
pub(super) const THIN_PAYLOAD: usize = 0x480000;
const THIN_SLOT_BYTES: u64 = 2 * 64 * 1024;
pub(super) const PENDING: &str =
    "存储空间缓存含待回放的记录或无法确认，请先通过 Windows 核对此空间。";

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

pub(super) fn record(header: &[u8], magic: &[u8; 8], guid: Guid, length: usize) -> bool {
    if header.len() < length
        || header.len() < 48
        || length < 48
        || &header[..8] != magic
        || le32(header, 24) != 1
        || le32(header, 28) as usize != length
        || header[40..48].iter().all(|&byte| byte == 0)
    {
        return false;
    }
    let mut wire = guid;
    wire[..4].reverse();
    wire[4..6].reverse();
    wire[6..8].reverse();
    if header[8..24] != wire {
        return false;
    }
    let mut crc = crc32fast::Hasher::new();
    crc.update(&header[..36]);
    crc.update(&[0; 4]);
    crc.update(&header[40..length]);
    crc.finalize() == le32(header, 36)
}

/// A supported clean copy has only empty slots and the observed control
/// records, which carry no cached user bytes. Other variants need replay.
pub(super) fn clean_digest(directory: &[u8], guid: Guid) -> Result<[u8; 32], &'static str> {
    if directory.len() != DIRECTORY_END
        || !record(directory, b"SPCACHE\0", guid, 96)
        || le32(directory, 32) != 0
        || directory[48..56] != (SLOT_START as u64).to_le_bytes()
        || le32(directory, 56) as usize != SECTOR
        || le32(directory, 60) as usize != SLOTS
        || directory[64..72] != (DIRECTORY_END as u64).to_le_bytes()
        || directory[96..SLOT_START].iter().any(|&byte| byte != 0)
    {
        return Err(PENDING);
    }
    for slot in directory[SLOT_START..].chunks_exact(SECTOR) {
        if slot.iter().all(|&byte| byte == 0) {
            continue;
        }
        if !record(slot, b"SPSLOT\0\0", guid, SECTOR)
            || le32(slot, 32) != 1
            || slot[48..56] != 1u64.to_le_bytes()
            || le32(slot, 56) != 8
            || le32(slot, 60) != 1
            || slot[64..].iter().any(|&byte| byte != 0)
        {
            return Err(PENDING);
        }
    }
    Ok(Sha256::digest(directory).into())
}

/// Recover the cache pages described by a complete first checkpoint. The
/// allocation stream supplies logical cache bytes in column order. A wrapped
/// log, alternate checkpoint, uncommitted suffix or unknown layout is rejected.
pub(super) fn thin_pages(
    metadata: &[u8],
    guid: Guid,
    space_size: u64,
    cache_size: u64,
) -> Result<BTreeMap<u64, u64>, &'static str> {
    if metadata.len() != THIN_PAYLOAD
        || !record(metadata, b"SPCACHE\0", guid, 96)
        || le32(metadata, 32) != 0
        || le64(metadata, 40) != 1
        || le64(metadata, 48) != SLOT_START as u64
        || le32(metadata, 56) as usize != SECTOR
        || le32(metadata, 60) as usize != SLOTS
        || le64(metadata, 64) != DIRECTORY_END as u64
        || le32(metadata, 72) as usize != CHECKPOINT_BYTES
        || le32(metadata, 76) != 2
        || le64(metadata, 80) != THIN_PAYLOAD as u64
        || u64::from(le32(metadata, 88)) != THIN_SLOT_BYTES
        || le32(metadata, 92) == 0
        || u64::from(le32(metadata, 92)) * THIN_SLOT_BYTES + THIN_PAYLOAD as u64 != cache_size
        || metadata[96..SLOT_START].iter().any(|&byte| byte != 0)
    {
        return Err(PENDING);
    }
    let slot_count = le32(metadata, 92);
    let mut log_tail = 0;
    let mut entries = BTreeMap::new();
    for (index, slot) in metadata[SLOT_START..DIRECTORY_END]
        .chunks_exact(SECTOR)
        .enumerate()
    {
        if slot.iter().all(|&byte| byte == 0) {
            continue;
        }
        let sequence = index as u64 + 1;
        if sequence != log_tail + 1
            || !record(slot, b"SPSLOT\0\0", guid, SECTOR)
            || le32(slot, 32) != 0
            || le64(slot, 40) != sequence
            || le64(slot, 48) != 1
            || slot[80..].iter().any(|&byte| byte != 0)
        {
            return Err(PENDING);
        }
        let raw: [u8; 24] = slot[56..80].try_into().unwrap();
        check_page_entry(&raw, slot_count, space_size)?;
        entries.insert(le64(&raw, 0), raw);
        log_tail = sequence;
    }
    let header = &metadata[DIRECTORY_END..DIRECTORY_END + SECTOR];
    let bytes = entries.len() * 24;
    let body_start = DIRECTORY_END + SECTOR;
    let body = &metadata[body_start..body_start + bytes];
    if entries.is_empty()
        || !record(header, b"SPCHECK\0", guid, 80)
        || le32(header, 32) != 0
        || le64(header, 40) != log_tail + 1
        || le32(header, 48) != crc32fast::hash(body)
        || le32(header, 52) as usize != SECTOR + bytes
        || le64(header, 56) != log_tail
        || le64(header, 64) != SECTOR as u64
        || le32(header, 72) as usize != bytes
        || le32(header, 76) as usize != entries.len()
        || header[80..].iter().any(|&byte| byte != 0)
        || metadata[body_start + bytes..].iter().any(|&byte| byte != 0)
    {
        return Err(PENDING);
    }
    let mut pages = BTreeMap::new();
    let mut slots = BTreeSet::new();
    for entry in body.chunks_exact(24) {
        let start = le64(entry, 0);
        let slot = le32(entry, 8);
        if entries.remove(&start).as_ref().map(|raw| &raw[..]) != Some(entry) || !slots.insert(slot)
        {
            return Err(PENDING);
        }
        let payload = THIN_PAYLOAD as u64 + u64::from(slot) * THIN_SLOT_BYTES;
        let bitmap = le32(entry, 16);
        for index in 0..THIN_SLOT_BYTES / SECTOR as u64 {
            if bitmap & (1 << index) != 0 {
                let offset = index * SECTOR as u64;
                pages.insert(start + offset, payload + offset);
            }
        }
    }
    Ok(pages)
}

fn check_page_entry(
    entry: &[u8; 24],
    slot_count: u32,
    space_size: u64,
) -> Result<(), &'static str> {
    let start = le64(entry, 0);
    if start % THIN_SLOT_BYTES != 0
        || start
            .checked_add(THIN_SLOT_BYTES)
            .is_none_or(|end| end > space_size)
        || le32(entry, 8) >= slot_count
        || !matches!(le32(entry, 12), 0x0004_0001 | 0x0004_0002)
        || le32(entry, 16) == 0
        || le32(entry, 20) != 0
    {
        return Err(PENDING);
    }
    Ok(())
}

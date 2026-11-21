//! Validation for the witnessed standalone write-back cache format.
use super::Guid;
use sha2::{Digest, Sha256};

pub(super) const SECTOR: usize = 4096;
const SLOT_START: usize = 2 * SECTOR;
const SLOTS: usize = 1024;
pub(super) const DIRECTORY_END: usize = SLOT_START + SLOTS * SECTOR;
pub(super) const PENDING: &str =
    "存储空间缓存含待回放的记录或无法确认，请先通过 Windows 核对此空间。";

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
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

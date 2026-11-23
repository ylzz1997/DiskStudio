use md5::{Digest, Md5};
use std::collections::HashSet;

const MAGIC: u64 = 0xab23_4cef_23dc_ea87;
const DIRTY_BITMAP: u64 = 0x2038_5fae_252c_b34a;
const MAX_CLUSTER: usize = 16 * 1024 * 1024;
const MALFORMED: &str = "Parallels HDS 变更位图扩展无效或超出读取上限。";

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

// The first eight frame bytes bind the header's sector count. Return only
// bitmap metadata addresses. Bitmap payloads never participate in guest reads.
pub(crate) fn reserved_sectors(frame: &[u8]) -> Result<Vec<u64>, &'static str> {
    let (geometry, cluster) = frame.split_at_checked(8).ok_or(MALFORMED)?;
    let sectors = le64(geometry, 0);
    if !(1..=1 << 34).contains(&sectors)
        || !(512..=MAX_CLUSTER).contains(&cluster.len())
        || cluster.len() % 512 != 0
        || le64(cluster, 0) != MAGIC
    {
        return Err(MALFORMED);
    }
    if Md5::digest(&cluster[24..])[..] != cluster[8..24] {
        return Err("Parallels HDS 格式扩展的 MD5 校验不匹配。");
    }

    let mut cursor = 24;
    let mut ids = HashSet::new();
    let mut reserved = Vec::new();
    while let Some(header) = cluster.get(cursor..cursor + 24) {
        cursor += 24;
        let magic = le64(header, 0);
        let length = le32(header, 16);
        if le64(header, 8) != 0 || le32(header, 20) != 0 {
            return Err(MALFORMED);
        }
        if magic == 0 {
            if length != 0 {
                return Err(MALFORMED);
            }
            return Ok(reserved);
        }
        if magic != DIRTY_BITMAP {
            return Err("Parallels HDS 含不支持的格式扩展类型。");
        }
        if length < 32 || length % 8 != 0 || ids.len() >= 1024 {
            return Err(MALFORMED);
        }
        let end = cursor.checked_add(length).ok_or(MALFORMED)?;
        let bitmap = cluster.get(cursor..end).ok_or(MALFORMED)?;
        cursor = end;
        let granularity = le32(bitmap, 24);
        let table = le32(bitmap, 28);
        if le64(bitmap, 0) != sectors
            || !granularity.is_power_of_two()
            || granularity >= 1 << 23
            || table as u64
                != sectors
                    .div_ceil(granularity as u64)
                    .div_ceil(cluster.len() as u64 * 8)
            || table != (length - 32) / 8
            || !ids.insert(&bitmap[8..24])
        {
            return Err(MALFORMED);
        }
        for entry in bitmap[32..].chunks_exact(8) {
            let sector = le64(entry, 0);
            if sector > 1 {
                if sector >= 1 << 35 || reserved.len() >= 128 * 1024 {
                    return Err(MALFORMED);
                }
                reserved.push(sector);
            }
        }
    }
    Err(MALFORMED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(frame: &mut [u8]) {
        let hash = Md5::digest(&frame[32..]);
        frame[16..32].copy_from_slice(&hash);
    }

    fn fixture() -> Vec<u8> {
        let mut frame = vec![0; 8 + 512];
        frame[..8].copy_from_slice(&8192u64.to_le_bytes());
        frame[8..16].copy_from_slice(&MAGIC.to_le_bytes());
        frame[32..40].copy_from_slice(&DIRTY_BITMAP.to_le_bytes());
        frame[48..52].copy_from_slice(&48u32.to_le_bytes());
        frame[56..64].copy_from_slice(&8192u64.to_le_bytes());
        frame[64..80].fill(0x23);
        frame[80..84].copy_from_slice(&1u32.to_le_bytes());
        frame[84..88].copy_from_slice(&2u32.to_le_bytes());
        frame[88..96].copy_from_slice(&1u64.to_le_bytes());
        frame[96..104].copy_from_slice(&4098u64.to_le_bytes());
        seal(&mut frame);
        frame
    }

    #[test]
    fn checks_bitmap_metadata_without_returning_sparse_sentinels() {
        assert_eq!(reserved_sectors(&fixture()), Ok(vec![4098]));
    }

    #[test]
    fn refuses_bad_digest_and_invalid_bitmap_geometry() {
        let base = fixture();
        for offset in [32, 40, 48, 52, 56, 80, 84, 120] {
            let mut frame = base.clone();
            frame[offset] ^= 1;
            assert!(reserved_sectors(&frame).is_err());
            seal(&mut frame);
            assert!(reserved_sectors(&frame).is_err());
        }
        assert!(reserved_sectors(&base[..520 - 1]).is_err());
    }
}

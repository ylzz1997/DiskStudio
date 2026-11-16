//! Bounded binary IPC frame: phase u8, job UUID [16], offset u64 LE, payload.
//! No paths, method names, devices or target identities in this transport.
use crate::image_convert::CHUNK;
pub fn decode(bytes: &[u8]) -> Result<(String, u64, bool, Vec<u8>), String> {
    if !(26..=25 + CHUNK).contains(&bytes.len()) || bytes[0] > 1 {
        return Err("镜像转换数据帧长度或阶段无效。".into());
    }
    let id = uuid::Uuid::from_slice(&bytes[1..17]).map_err(|e| e.to_string())?;
    if id.is_nil() {
        return Err("转换任务标识为空。".into());
    }
    let offset = u64::from_le_bytes(bytes[17..25].try_into().unwrap());
    if offset % CHUNK as u64 != 0 || offset >= 1u64 << 43 {
        return Err("转换数据帧偏移无效。".into());
    }
    Ok((id.to_string(), offset, bytes[0] == 1, bytes[25..].to_vec()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_keeps_identity_offset_phase_and_bytes_and_rejects_malformed_frames() {
        let id = uuid::Uuid::parse_str("12345678-1234-4321-8899-aabbccddeeff").unwrap();
        let mut frame = vec![0; 25 + 512];
        frame[0] = 1;
        frame[1..17].copy_from_slice(id.as_bytes());
        frame[17..25].copy_from_slice(&(7 * CHUNK as u64).to_le_bytes());
        frame[25] = 0xab;
        let (got, at, check, b) = decode(&frame).unwrap();
        assert_eq!(got, id.to_string());
        assert_eq!(at, 7 * CHUNK as u64);
        assert!(check);
        assert_eq!(b.len(), 512);
        assert_eq!(b[0], 0xab);
        for n in [0, 1, 24, 25, 26 + CHUNK] {
            assert!(decode(&vec![0; n]).is_err());
        }
        let mut bad = frame.clone();
        bad[0] = 2;
        assert!(decode(&bad).is_err());
        let mut bad = frame.clone();
        bad[1..17].fill(0);
        assert!(decode(&bad).is_err());
        for at in [1, 512, 1u64 << 43, u64::MAX] {
            let mut bad = frame.clone();
            bad[17..25].copy_from_slice(&at.to_le_bytes());
            assert!(decode(&bad).is_err());
        }
    }
}

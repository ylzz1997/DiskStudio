use super::model::{validate_entry, Value, MAX_VARIABLE_SIZE};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 8] = b"DSUEFI01";
const HEADER_SIZE: usize = 16;
const DIGEST_SIZE: usize = 32;
pub(super) const MAX_ARCHIVE_SIZE: usize = HEADER_SIZE + MAX_VARIABLE_SIZE + DIGEST_SIZE;

pub(super) fn encode(value: &Value) -> Result<Vec<u8>, String> {
    validate_entry(value)?;
    let mut bytes = Vec::with_capacity(HEADER_SIZE + value.bytes.len() + DIGEST_SIZE);
    bytes.extend(MAGIC);
    bytes.extend(value.attributes.to_le_bytes());
    bytes.extend((value.bytes.len() as u32).to_le_bytes());
    bytes.extend(&value.bytes);
    bytes.extend(Sha256::digest(&bytes));
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() < HEADER_SIZE + DIGEST_SIZE
        || bytes.len() > MAX_ARCHIVE_SIZE
        || &bytes[..MAGIC.len()] != MAGIC
    {
        return Err("DiskStudio UEFI 启动项备份的格式无效。".into());
    }
    let attributes = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let end = bytes.len() - DIGEST_SIZE;
    if size != end - HEADER_SIZE || Sha256::digest(&bytes[..end]).as_slice() != &bytes[end..] {
        return Err("UEFI 启动项备份的长度或 SHA-256 校验不匹配。".into());
    }
    let value = Value {
        attributes,
        bytes: bytes[HEADER_SIZE..end].to_vec(),
    };
    validate_entry(&value)?;
    Ok(value)
}

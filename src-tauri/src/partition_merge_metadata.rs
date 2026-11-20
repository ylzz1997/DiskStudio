//! Validate metadata shared by the NTFS merge and file-clone writers.
use super::{check, validate_component, Catalogue, Node, RecordId, RelativePath};
use ntfs::NtfsAttributeType;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
};

pub(crate) const OBJECT_ID: u32 = NtfsAttributeType::ObjectId as u32;
pub(crate) const REPARSE: u32 = NtfsAttributeType::ReparsePoint as u32;
pub(crate) const REPARSE_ATTRIBUTE: u32 = 0x400;
pub(crate) const ENCRYPTED_ATTRIBUTE: u32 = 0x4000;
pub(crate) const EFS: u32 = NtfsAttributeType::LoggedUtilityStream as u32;
pub(crate) const EFS_NAME: &[u8] = b"$\0E\0F\0S\0";
pub(crate) const EFS_LIMIT: u64 = 256 * 1024;
pub(crate) const REPARSE_LIMIT: u64 = 16 * 1024;
pub(crate) const WOF_TAG: u32 = 0x80000017;
pub(crate) const WOF_STREAM: &[u8] = b"W\0o\0f\0C\0o\0m\0p\0r\0e\0s\0s\0e\0d\0D\0a\0t\0a\0";
const EA_INFO: u32 = NtfsAttributeType::EAInformation as u32;
const EA: u32 = NtfsAttributeType::EA as u32;

/// Check the envelope recorded in NTFS's $EFS utility stream. The wrapped
/// keys and credential payloads remain opaque and are copied byte for byte.
pub(crate) fn validate_efs(data: &[u8]) -> Result<(), String> {
    const HEADER: usize = 76;
    const FIELD_HEADER: usize = 20;
    const INVALID: &str = "NTFS $EFS 的版本、凭据范围或密钥记录无效。";
    let word = |bytes: &[u8], offset| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
    };
    if !(85..=EFS_LIMIT as usize).contains(&data.len())
        || word(data, 0) != data.len()
        || word(data, 4) != 0
        || word(data, 8) != 2
        || word(data, 72) != 0
    {
        return Err(INVALID.into());
    }
    let ddf = word(data, 64);
    let drf = word(data, 68);
    if ddf == 0 || ddf == drf {
        return Err(INVALID.into());
    }
    for (offset, other) in [(ddf, drf), (drf, ddf)] {
        if offset == 0 {
            continue;
        }
        let end = if offset < other { other } else { data.len() };
        let array = data
            .get(offset..end)
            .filter(|bytes| offset >= HEADER && offset % 4 == 0 && bytes.len() >= 4 + FIELD_HEADER)
            .ok_or(INVALID)?;
        let count = word(array, 0);
        if count == 0 || count > (array.len() - 4) / FIELD_HEADER {
            return Err(INVALID.into());
        }
        let mut next = &array[4..];
        for _ in 0..count {
            if next.len() < FIELD_HEADER {
                return Err(INVALID.into());
            }
            let length = word(next, 0);
            let record = next
                .get(..length)
                .filter(|row| row.len() >= FIELD_HEADER)
                .ok_or(INVALID)?;
            next = &next[length..];
            let credential = word(record, 4);
            let key_size = word(record, 8);
            let key = word(record, 12);
            if credential < FIELD_HEADER
                || key < FIELD_HEADER
                || key_size == 0
                || key_size > record.len().saturating_sub(key)
            {
                return Err(INVALID.into());
            }
            let credential_limit = if credential < key {
                key
            } else if credential >= key + key_size {
                record.len()
            } else {
                return Err(INVALID.into());
            };
            let bytes = record
                .get(credential..credential_limit)
                .filter(|bytes| bytes.len() >= 28)
                .ok_or(INVALID)?;
            let length = word(bytes, 0);
            if length < 28 || length > bytes.len() || !matches!(word(bytes, 8), 1 | 3) {
                return Err(INVALID.into());
            }
        }
    }
    Ok(())
}

pub(crate) fn reparse_tag(data: &[u8], directory: bool) -> Result<u32, String> {
    const SYMLINK: u32 = 0xa000000c;
    const MOUNT_POINT: u32 = 0xa0000003;
    let invalid = "NTFS 重解析点的链接数据结构无效。";
    let tag = u32::from_le_bytes(data.get(..4).ok_or(invalid)?.try_into().unwrap());
    let paths_at = match tag {
        WOF_TAG if !directory => {
            wof_chunk_size(data)?;
            return Ok(tag);
        }
        SYMLINK => 20,
        MOUNT_POINT if directory => 16,
        _ => return Err("NTFS 重解析类型超出 Windows 链接和卷内 WOF 压缩范围。".into()),
    };
    if data.len() < paths_at
        || data.len() as u64 > REPARSE_LIMIT
        || usize::from(u16::from_le_bytes(data[4..6].try_into().unwrap())) != data.len() - 8
        || tag == SYMLINK && u32::from_le_bytes(data[16..20].try_into().unwrap()) > 1
    {
        return Err(invalid.into());
    }
    for (index, pair) in data[8..16].chunks_exact(4).enumerate() {
        let offset = usize::from(u16::from_le_bytes(pair[..2].try_into().unwrap()));
        let length = usize::from(u16::from_le_bytes(pair[2..].try_into().unwrap()));
        if offset % 2 != 0 || length % 2 != 0 || index == 0 && length == 0 {
            return Err(invalid.into());
        }
        let name = data
            .get(paths_at + offset..paths_at + offset + length)
            .ok_or(invalid)?;
        if name.chunks_exact(2).any(|unit| unit == [0, 0]) {
            return Err(invalid.into());
        }
    }
    Ok(tag)
}

pub(crate) fn wof_chunk_size(data: &[u8]) -> Result<u64, String> {
    let invalid = "NTFS WOF 压缩的提供程序、版本或算法不受支持。";
    if !matches!(data.len(), 24 | 28)
        || data[..4] != WOF_TAG.to_le_bytes()
        || data[4..8] != ((data.len() - 8) as u32).to_le_bytes()
        || data[8..12] != 1u32.to_le_bytes()
        || data[12..16] != 2u32.to_le_bytes()
        || data[16..20] != 1u32.to_le_bytes()
        || data.len() == 28 && data[24..28] != [0; 4]
    {
        return Err(invalid.into());
    }
    match u32::from_le_bytes(data[20..24].try_into().unwrap()) {
        0 => Ok(4096),
        1 => Ok(32768),
        2 => Ok(8192),
        3 => Ok(16384),
        _ => Err(invalid.into()),
    }
}

/// Check WOF's offset table in the same pass that hashes the stored stream.
/// The first chunk starts immediately after the table. The last ends at EOF.
pub(crate) struct WofOffsets {
    remaining: u64,
    width: usize,
    chunk: u64,
    last: u64,
    payload: u64,
    previous: u64,
    pending: [u8; 8],
    filled: usize,
}

impl WofOffsets {
    pub(crate) fn new(size: u64, stored: u64, chunk: u64) -> Result<Self, String> {
        let invalid = "NTFS WOF 压缩流的区块表长度无效。";
        let remaining = size.div_ceil(chunk).checked_sub(1).ok_or(invalid)?;
        let width = if size < 1 << 32 { 4 } else { 8 };
        let payload = stored
            .checked_sub(remaining * width as u64)
            .ok_or(invalid)?;
        if payload <= remaining || payload > size {
            return Err(invalid.into());
        }
        Ok(Self {
            remaining,
            width,
            chunk,
            last: (size - 1) % chunk + 1,
            payload,
            previous: 0,
            pending: [0; 8],
            filled: 0,
        })
    }

    pub(crate) fn consume(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while self.remaining != 0 && !data.is_empty() {
            let count = (self.width - self.filled).min(data.len());
            self.pending[self.filled..self.filled + count].copy_from_slice(&data[..count]);
            data = &data[count..];
            self.filled += count;
            if self.filled == self.width {
                let offset = u64::from_le_bytes(self.pending);
                if offset <= self.previous
                    || offset >= self.payload
                    || offset - self.previous > self.chunk
                {
                    return Err(std::io::Error::other("NTFS WOF 压缩流的区块偏移无效。"));
                }
                self.previous = offset;
                self.filled = 0;
                self.remaining -= 1;
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        if self.remaining != 0 || self.payload - self.previous > self.last {
            return Err("NTFS WOF 压缩流末端区块的范围无效。".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn wof_offsets_check_monotonic_bounds_and_partial_reads() {
    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    let data = [400u32.to_le_bytes(), 887u32.to_le_bytes()].concat();
    for piece in data.chunks(3) {
        offsets.consume(piece).unwrap();
    }
    offsets.finish().unwrap();

    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    offsets.consume(&400u32.to_le_bytes()).unwrap();
    assert!(offsets.consume(&399u32.to_le_bytes()).is_err());
    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    offsets.consume(&400u32.to_le_bytes()).unwrap();
    offsets.consume(&884u32.to_le_bytes()).unwrap();
    assert!(offsets.finish().is_err());

    let count = (1u64 << 32) / 32768 - 1;
    let mut wide = WofOffsets::new(1 << 32, count * 8 + 524288, 32768).unwrap();
    wide.consume(&400u64.to_le_bytes()).unwrap();
    assert_eq!(wide.remaining, count - 1);
    assert_eq!(wide.previous, 400);
}

pub(crate) fn short_name_links<'a>(
    catalogue: &'a Catalogue,
    cancel: &AtomicBool,
) -> Result<BTreeMap<&'a RelativePath, &'a [u16]>, String> {
    let mut aliases = BTreeMap::new();
    if catalogue.short_names.is_empty() {
        return Ok(aliases);
    }
    let mut paths = BTreeMap::<(&[Vec<u16>], RecordId), Option<&RelativePath>>::new();
    for (path, &id) in &catalogue.names {
        check(cancel)?;
        if let Some((_, parent)) = path.split_last() {
            paths
                .entry((parent, id))
                .and_modify(|name| *name = None)
                .or_insert(Some(path));
        }
    }
    for (path, &id) in &catalogue.short_names {
        check(cancel)?;
        let (alias, parent) = path.split_last().ok_or("NTFS 短名缺少父目录。")?;
        validate_component(alias)?;
        if alias.len() > 12 {
            return Err("NTFS 短名超过 Windows 8.3 格式。".into());
        }
        let Some(Some(name)) = paths.get(&(parent, id)) else {
            return Err("无法唯一确定 NTFS 短名对应的同目录硬链接。".into());
        };
        if aliases.insert(*name, alias.as_slice()).is_some() {
            return Err("同一 NTFS 文件链接对应多个短名。".into());
        }
    }
    Ok(aliases)
}

pub(crate) fn validate_short_name(alias: &[u16]) -> Result<(), String> {
    validate_component(alias)?;
    let mut parts = alias.split(|&unit| unit == u16::from(b'.'));
    if !parts
        .next()
        .is_some_and(|stem| (1..=8).contains(&stem.len()))
        || parts
            .next()
            .is_some_and(|extension| !(1..=3).contains(&extension.len()))
        || parts.next().is_some()
        || alias
            .iter()
            .any(|&unit| u8::try_from(unit).is_ok_and(|unit| b" ,;+=[]".contains(&unit)))
    {
        return Err("NTFS 短名不符合 8.3 格式。".into());
    }
    Ok(())
}

pub(crate) fn extended_attributes(node: &Node) -> Result<Option<(&[u8], &[u8])>, String> {
    match (node.metadata.get(&EA_INFO), node.metadata.get(&EA)) {
        (None, None) => Ok(None),
        (Some(info), Some(ea)) => Ok(Some((info, ea))),
        _ => Err("NTFS 扩展属性数据或长度信息缺失。".into()),
    }
}

pub(crate) fn visit_ea(
    info: &[u8],
    input: &[u8],
    mut visit: impl FnMut(&[u8], usize, bool) -> Result<(), String>,
) -> Result<(), String> {
    let invalid = "NTFS 扩展属性的记录、长度或名称无效。";
    if info.len() != 8
        || input.is_empty()
        || u32::from_le_bytes(info[4..8].try_into().unwrap()) as usize != input.len()
    {
        return Err(invalid.into());
    }
    let mut names = BTreeSet::new();
    let mut packed = 0;
    let mut required = 0;
    let mut at = 0;
    while at < input.len() {
        let remaining = &input[at..];
        if remaining.len() < 9 {
            return Err(invalid.into());
        }
        let flags = remaining[4];
        let name_len = remaining[5] as usize;
        let value_len = u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
        let len = 9 + name_len + value_len;
        let padded = len.next_multiple_of(4);
        let next = u32::from_le_bytes(remaining[..4].try_into().unwrap()) as usize;
        if !matches!(flags, 0 | 0x80)
            || name_len == 0
            || !(len..=padded).contains(&next)
                && !(next == 0 && (len..=padded).contains(&remaining.len()))
            || len > remaining.len()
            || remaining[8 + name_len] != 0
        {
            return Err(invalid.into());
        }
        let span = if next == 0 { remaining.len() } else { next };
        let name = &remaining[8..8 + name_len];
        if span > remaining.len()
            || span < remaining.len() && span != padded
            || name
                .iter()
                .any(|&ch| ch < 32 || b"\\/:*?\"<>|,+=[];".contains(&ch))
            || !names.insert(name.to_ascii_uppercase())
        {
            return Err(invalid.into());
        }
        visit(&remaining[4..len], padded, span == remaining.len())?;
        packed += len - 4;
        required += usize::from(flags == 0x80);
        at += span;
    }
    if packed != u16::from_le_bytes(info[..2].try_into().unwrap()) as usize
        || required != u16::from_le_bytes(info[2..4].try_into().unwrap()) as usize
    {
        return Err(invalid.into());
    }
    Ok(())
}

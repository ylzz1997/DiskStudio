//! Read the NTFS $Secure:$SDS witnesses for the security IDs used by a merge.
//! Relative descriptor offsets may differ between source and destination.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
};

const SDS_BUCKET: usize = 0x40000;
const SDS_HEADER: usize = 20;
const DESCRIPTOR_HEADER: usize = 20;
const MAX_DESCRIPTOR: usize = 128 * 1024;
const MAX_SDS: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Descriptor {
    raw: Vec<u8>,
    control: u16,
    manager: u8,
    components: [Option<std::ops::Range<usize>>; 4],
}

impl Descriptor {
    pub(super) fn read(raw: Vec<u8>) -> Result<Self, String> {
        if !(DESCRIPTOR_HEADER..=MAX_DESCRIPTOR).contains(&raw.len()) || raw[0] != 1 {
            return Err("NTFS 安全描述符的版本或大小无效。".into());
        }
        let control = u16::from_le_bytes([raw[2], raw[3]]);
        if control & 0x8000 == 0 {
            return Err("NTFS 安全描述符不是自相对格式。".into());
        }
        let mut components: [Option<std::ops::Range<usize>>; 4] = [None, None, None, None];
        for (index, component) in components.iter_mut().enumerate() {
            let offset = u32_at(&raw, 4 + index * 4) as usize;
            if offset == 0 {
                continue;
            }
            if offset < DESCRIPTOR_HEADER || offset % 4 != 0 || raw.len().saturating_sub(offset) < 8
            {
                return Err("NTFS 安全描述符的 SID 或 ACL 指针越界。".into());
            }
            let remaining = &raw[offset..];
            let len = if index < 2 {
                let count = usize::from(remaining[1]);
                if remaining[0] != 1 || count > 15 {
                    return Err("NTFS 安全描述符含无效 SID。".into());
                }
                8 + count * 4
            } else {
                let present = if index == 2 { 0x0010 } else { 0x0004 };
                if control & present == 0 {
                    return Err("ACL 偏移与安全描述符控制位不一致。".into());
                }
                acl_length(remaining)?
            };
            if len > remaining.len() {
                return Err("NTFS 安全描述符的组件被截断。".into());
            }
            *component = Some(offset..offset + len);
        }
        for (index, first) in components.iter().enumerate() {
            let Some(first) = first else {
                continue;
            };
            for (other_index, second) in components.iter().enumerate().skip(index + 1) {
                let Some(second) = second else {
                    continue;
                };
                if first.start < second.end
                    && second.start < first.end
                    && !(other_index == 1 && first == second)
                {
                    return Err("NTFS 安全描述符的组件相互重叠。".into());
                }
            }
        }
        Ok(Self {
            manager: if control & 0x4000 != 0 { raw[1] } else { 0 },
            raw,
            control,
            components,
        })
    }

    pub(super) fn agrees(&self, other: &Self) -> bool {
        self.control == other.control
            && self.manager == other.manager
            && self
                .components
                .iter()
                .zip(&other.components)
                .all(|(left, right)| {
                    left.as_ref().map(|span| &self.raw[span.clone()])
                        == right.as_ref().map(|span| &other.raw[span.clone()])
                })
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.raw
    }
}

fn acl_length(raw: &[u8]) -> Result<usize, String> {
    let length = usize::from(u16::from_le_bytes([raw[2], raw[3]]));
    let count = u16::from_le_bytes([raw[4], raw[5]]);
    if !matches!(raw[0], 2 | 4) || !(8..=raw.len()).contains(&length) || length % 4 != 0 {
        return Err("NTFS ACL 版本或大小无效。".into());
    }
    let mut at = 8;
    for _ in 0..count {
        if length - at < 4 {
            return Err("NTFS ACL 的访问规则头被截断。".into());
        }
        let span = usize::from(u16::from_le_bytes([raw[at + 2], raw[at + 3]]));
        if span < 4 || span % 4 != 0 || span > length - at {
            return Err("NTFS ACL 的访问规则越界。".into());
        }
        at += span;
    }
    Ok(length)
}

fn u32_at(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(raw[at..at + 4].try_into().unwrap())
}

fn descriptor_hash(raw: &[u8]) -> u32 {
    raw.chunks_exact(4).fold(0u32, |hash, word| {
        hash.rotate_left(3)
            .wrapping_add(u32::from_le_bytes(word.try_into().unwrap()))
    })
}

pub(super) fn read_sds(
    reader: &mut (impl Read + Seek),
    wanted: &BTreeSet<u32>,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<BTreeMap<u32, Descriptor>, String> {
    let total = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    if total > MAX_SDS {
        return Err("NTFS 安全描述符流超过分析容量。".into());
    }
    let mut found = BTreeMap::new();
    let mut original = vec![0; SDS_BUCKET];
    let mut mirror = vec![0; SDS_BUCKET];
    for base in (0..total).step_by(2 * SDS_BUCKET) {
        checkpoint()?;
        let mirror_base = base + SDS_BUCKET as u64;
        let mirror_bytes = total.saturating_sub(mirror_base).min(SDS_BUCKET as u64) as usize;
        if mirror_bytes == 0 {
            return Err("NTFS 安全描述符流缺少镜像范围。".into());
        }
        reader
            .seek(SeekFrom::Start(base))
            .and_then(|_| reader.read_exact(&mut original))
            .and_then(|_| reader.read_exact(&mut mirror[..mirror_bytes]))
            .map_err(|error| error.to_string())?;
        if original[mirror_bytes..].iter().any(|byte| *byte != 0) {
            return Err("NTFS 安全描述符镜像不覆盖完整主记录。".into());
        }
        let mut at = 0;
        while at < mirror_bytes {
            checkpoint()?;
            let remaining = &original[at..mirror_bytes];
            if remaining.len() < SDS_HEADER || remaining[..SDS_HEADER].iter().all(|byte| *byte == 0)
            {
                if remaining.iter().any(|byte| *byte != 0) || remaining != &mirror[at..mirror_bytes]
                {
                    return Err("NTFS 安全描述符流的尾部或镜像无效。".into());
                }
                break;
            }
            let raw_hash = u32_at(remaining, 0);
            let id = u32_at(remaining, 4);
            let absolute = u64::from_le_bytes(remaining[8..16].try_into().unwrap());
            let len = u32_at(remaining, 16) as usize;
            if id < 0x100
                || absolute != base + at as u64
                || len % 4 != 0
                || !(SDS_HEADER + DESCRIPTOR_HEADER..=SDS_HEADER + MAX_DESCRIPTOR).contains(&len)
                || len > remaining.len()
            {
                return Err("NTFS 安全描述符记录的标识或范围无效。".into());
            }
            let payload = &remaining[SDS_HEADER..len];
            if remaining[..len] != mirror[at..at + len] || raw_hash != descriptor_hash(payload) {
                return Err("NTFS 安全描述符的镜像或校验值不匹配。".into());
            }
            if wanted.contains(&id) {
                let descriptor = Descriptor::read(payload.to_vec())?;
                if found.insert(id, descriptor).is_some() {
                    return Err("NTFS 安全标识重复。".into());
                }
            }
            at += len.div_ceil(16) * 16;
        }
    }
    if found.len() != wanted.len() {
        return Err("文件引用了无法核对的 NTFS 安全标识。".into());
    }
    Ok(found)
}

#[cfg(test)]
#[path = "partition_merge_security_tests.rs"]
mod tests;

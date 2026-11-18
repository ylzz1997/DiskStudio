//! Enumerate NTFS name identities without converting names to filesystem paths.
//! Multiple names can refer to one MFT record. DOS names remain explicit aliases;
//! their pairing with long names is not guessed from the parent directory.
use ntfs::{
    structured_values::NtfsFileName, NtfsAttribute, NtfsAttributeType, NtfsFile, NtfsFileFlags,
    NtfsReadSeek,
};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
};

#[cfg(test)]
#[path = "ntfs_names_tests.rs"]
mod tests;

const MAX_ATTRIBUTES: usize = 4096;
const MAX_NAMES: usize = 2048;
const MAX_LIST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Name {
    pub parent: (u64, u16),
    pub namespace: u8,
    pub raw: Vec<u8>,
}
impl Name {
    pub fn validate(&self) -> Result<(), String> {
        if self.parent.0 >= 1 << 48
            || self.namespace > 3
            || self.raw.is_empty()
            || self.raw.len() > 510
            || self.raw.len() % 2 != 0
            || self.raw.chunks_exact(2).any(|b| b == [0, 0])
        {
            return Err("NTFS 保存名称身份无效".into());
        }
        Ok(())
    }
    pub fn display(&self) -> String {
        String::from_utf16_lossy(
            &self
                .raw
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
    }
    #[cfg(test)]
    pub fn fixture(name: &str) -> Self {
        Self {
            parent: (5, 1),
            namespace: 1,
            raw: name.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        }
    }
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The base reference is in the first 40 bytes, before sector fixups. Do not
/// interpret extension records as independent files during the MFT sweep.
pub fn base_reference<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<u64, String> {
    base_reference_with_flags(reader, file, file.flags().bits())
}

pub(crate) fn merge_base_reference<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
) -> Result<u64, String> {
    // The system and index bits also occur in active NTFS metadata records.
    let flags = file.raw_flags();
    if flags & !0x000f != 0 {
        return Err("NTFS 文件记录含未知标志".into());
    }
    base_reference_with_flags(reader, file, flags)
}

fn base_reference_with_flags<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    flags: u16,
) -> Result<u64, String> {
    let position = file.position().value().ok_or("文件记录位置不可用")?.get();
    reader.seek(SeekFrom::Start(position)).map_err(err)?;
    let mut header = [0; 40];
    reader.read_exact(&mut header).map_err(err)?;
    if &header[..4] != b"FILE"
        || u16::from_le_bytes([header[16], header[17]]) != file.sequence_number()
        || u16::from_le_bytes([header[22], header[23]]) != flags
    {
        return Err("文件记录头已变化".into());
    }
    Ok(u64::from_le_bytes(header[32..40].try_into().unwrap()))
}
fn read_name<R: Read + Seek>(
    reader: &mut R,
    attribute: &NtfsAttribute<'_, '_>,
) -> Result<Name, String> {
    if !attribute.is_resident()
        || attribute.name_length() != 0
        || !(68..=576).contains(&attribute.value_length())
    {
        return Err("不支持的文件名属性布局".into());
    }
    let name = attribute
        .structured_value::<_, NtfsFileName>(reader)
        .map_err(err)?;
    let raw = name.name().0.to_vec();
    if raw.is_empty()
        || raw.chunks_exact(2).any(|b| b == [0, 0])
        || attribute.value_length() != 66 + raw.len() as u64
    {
        return Err("文件名为空或包含 NUL".into());
    }
    let parent = name.parent_directory_reference();
    Ok(Name {
        parent: (parent.file_record_number(), parent.sequence_number()),
        namespace: name.namespace() as u8,
        raw,
    })
}
fn name_references(bytes: &[u8]) -> Result<Vec<(u64, u16, u16)>, String> {
    if bytes.len() > MAX_LIST_BYTES as usize {
        return Err("文件属性列表过大".into());
    }
    let mut at = 0;
    let mut count = 0;
    let mut references = Vec::new();
    let mut seen = HashSet::new();
    while at < bytes.len() {
        count += 1;
        let entry = &bytes[at..];
        if count > MAX_ATTRIBUTES || entry.len() < 26 {
            return Err("文件属性列表过长或被截断".into());
        }
        let size = u16::from_le_bytes([entry[4], entry[5]]) as usize;
        let name_size = usize::from(entry[6]) * 2;
        let name_at = usize::from(entry[7]);
        if size < 26
            || size % 8 != 0
            || size > entry.len()
            || (name_size > 0 && (name_at < 26 || name_at + name_size > size))
        {
            return Err("文件属性列表边界无效".into());
        }
        if u32::from_le_bytes(entry[..4].try_into().unwrap()) == 0x30 {
            if name_size != 0 || entry[8..16] != [0; 8] {
                return Err("文件名属性列表布局无效".into());
            }
            let reference = u64::from_le_bytes(entry[16..24].try_into().unwrap());
            let identity = (
                reference & 0x0000_ffff_ffff_ffff,
                (reference >> 48) as u16,
                u16::from_le_bytes([entry[24], entry[25]]),
            );
            if !seen.insert(identity) {
                return Err("文件名属性列表包含重复引用".into());
            }
            references.push(identity);
        }
        at += size;
    }
    Ok(references)
}
pub fn read<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<Vec<Name>, String> {
    read_with(reader, file, base_reference)
}

pub(crate) fn read_for_merge<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
) -> Result<Vec<Name>, String> {
    read_with(reader, file, merge_base_reference)
}

fn read_with<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    base_reference: impl Fn(&mut R, &NtfsFile<'_>) -> Result<u64, String>,
) -> Result<Vec<Name>, String> {
    if base_reference(reader, file)? != 0 {
        return Err("扩展记录不能作为独立文件".into());
    }
    let mut names = HashSet::new();
    let mut lists = 0;
    for (index, attribute) in file.attributes_raw().enumerate() {
        if index >= MAX_ATTRIBUTES {
            return Err("文件属性数量超过上限".into());
        }
        let attribute = attribute.map_err(err)?;
        match attribute.ty().map_err(err)? {
            NtfsAttributeType::FileName => {
                names.insert(read_name(reader, &attribute)?);
            }
            NtfsAttributeType::AttributeList => {
                lists += 1;
                if lists > 1 || attribute.value_length() > MAX_LIST_BYTES {
                    return Err("文件属性列表重复或过大".into());
                }
                let mut bytes = vec![0; attribute.value_length() as usize];
                attribute
                    .value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut bytes)
                    .map_err(err)?;
                for (record, sequence, instance) in name_references(&bytes)? {
                    let linked = file.ntfs().file(reader, record).map_err(err)?;
                    let expected_base = if record == file.file_record_number() {
                        0
                    } else {
                        file.file_record_number() | (u64::from(file.sequence_number()) << 48)
                    };
                    if linked.sequence_number() != sequence
                        || linked.is_directory() != file.is_directory()
                        || linked.flags().contains(NtfsFileFlags::IN_USE)
                            != file.flags().contains(NtfsFileFlags::IN_USE)
                        || base_reference(reader, &linked)? != expected_base
                    {
                        return Err("文件名扩展记录身份或所属文件不匹配".into());
                    }
                    let mut matched = None;
                    for (index, linked_attribute) in linked.attributes_raw().enumerate() {
                        if index >= MAX_ATTRIBUTES {
                            return Err("扩展记录属性数量超过上限".into());
                        }
                        let linked_attribute = linked_attribute.map_err(err)?;
                        if linked_attribute.ty().map_err(err)? == NtfsAttributeType::FileName
                            && linked_attribute.instance() == instance
                        {
                            if matched.is_some() {
                                return Err("扩展记录属性身份重复".into());
                            }
                            matched = Some(read_name(reader, &linked_attribute)?);
                        }
                    }
                    names.insert(matched.ok_or("扩展记录缺少所引用的文件名称")?);
                    if names.len() > MAX_NAMES {
                        return Err("文件名称数量超过上限".into());
                    }
                }
            }
            _ => {}
        }
        if names.len() > MAX_NAMES {
            return Err("文件名称数量超过上限".into());
        }
    }
    let mut names: Vec<_> = names.into_iter().collect();
    // Exact raw identity deduplicates base/list attribute enumeration; Unicode
    // replacement characters and case-folding never merge different names.
    names.sort_by(|a, b| {
        a.parent
            .cmp(&b.parent)
            .then(a.namespace.cmp(&b.namespace))
            .then(a.raw.cmp(&b.raw))
    });
    Ok(names)
}

pub fn directory<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<Name, String> {
    let names = read(reader, file)?;
    let long: Vec<_> = names.iter().filter(|n| n.namespace != 2).collect();
    let chosen = if long.is_empty() {
        names.iter().collect::<Vec<_>>()
    } else {
        long
    };
    let first = chosen.first().ok_or("目录名称属性缺失")?;
    if chosen
        .iter()
        .any(|n| n.raw != first.raw || n.parent != first.parent)
    {
        return Err("目录有多个不同名称，无法确认唯一目录链路".into());
    }
    Ok((*first).clone())
}

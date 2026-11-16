//! Named NTFS $DATA streams. Names remain exact UTF-16 bytes and never become
//! device paths or output ADS names. All reads use the existing bounded reader.
use ntfs::{NtfsAttributeType, NtfsFile, NtfsFileFlags, NtfsReadSeek};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Seek, SeekFrom, Write},
};

const MAX_ATTRIBUTES: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RUNS: usize = 262144;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub name: Vec<u8>,
    pub signature: [u8; 32],
}
impl Identity {
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name, false)
    }
    pub fn display(&self, file: &str) -> String {
        display(file, &self.name)
    }
}
pub fn display(file: &str, name: &[u8]) -> String {
    let text = String::from_utf16_lossy(
        &name
            .chunks_exact(2)
            .map(|b| u16le(b, 0))
            .collect::<Vec<_>>(),
    );
    format!("{file}:{text}")
}
pub fn validate_name(name: &[u8], empty: bool) -> Result<(), String> {
    if (!empty && name.is_empty())
        || name.len() > 510
        || name.len() % 2 != 0
        || name.chunks_exact(2).any(|b| b == [0, 0])
    {
        return Err("NTFS 数据流名称无效".into());
    }
    Ok(())
}
pub fn attribute_name(b: &[u8]) -> Result<&[u8], String> {
    if b.len() < 24 {
        return Err("数据属性头被截断".into());
    }
    let n = usize::from(b[9]) * 2;
    if n == 0 {
        return Ok(&[]);
    }
    let at = usize::from(u16le(b, 10));
    let minimum = if b[8] == 0 {
        24
    } else if b.len() >= 64 && u64le(b, 16) == 0 && u16le(b, 12) & 0x8001 != 0 {
        72
    } else {
        64
    };
    if at < minimum || at % 2 != 0 || n > b.len().saturating_sub(at) {
        return Err("数据流名称范围无效".into());
    }
    let name = &b[at..at + n];
    validate_name(name, false)?;
    Ok(name)
}
type Record = ((u64, u16, u16), Vec<u8>);
type Reference = (u64, u16, u16, u64, Vec<u8>);
fn references(b: &[u8]) -> Result<Vec<Reference>, String> {
    let mut at = 0;
    let mut count = 0;
    let mut refs = vec![];
    let mut seen = HashSet::new();
    while at < b.len() {
        count += 1;
        let e = &b[at..];
        if e.len() < 26 || count > MAX_ATTRIBUTES {
            return Err("数据流属性列表过长或被截断".into());
        }
        let size = usize::from(u16le(e, 4));
        let n = usize::from(e[6]) * 2;
        let start = usize::from(e[7]);
        if size < 26
            || size % 8 != 0
            || size > e.len()
            || (n > 0 && (start < 26 || n > size.saturating_sub(start)))
        {
            return Err("数据流属性列表边界无效".into());
        }
        if e[..4] == 0x80u32.to_le_bytes() && n > 0 {
            let name = e[start..start + n].to_vec();
            validate_name(&name, false)?;
            let id = u64le(e, 16);
            let r = (
                id & 0x0000_ffff_ffff_ffff,
                (id >> 48) as u16,
                u16le(e, 24),
                u64le(e, 8),
                name,
            );
            if !seen.insert((r.0, r.1, r.2)) {
                return Err("数据流属性列表重复引用".into());
            }
            refs.push(r);
        }
        at += size;
    }
    Ok(refs)
}
fn records<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<Vec<Record>, String> {
    if file.is_directory() || crate::ntfs_names::base_reference(reader, file)? != 0 {
        return Err("命名数据流读取限普通文件的基础记录".into());
    }
    let mut base = vec![];
    let mut base_ids = HashSet::new();
    let mut list = None;
    let mut total = 0;
    for (i, a) in file.attributes_raw().enumerate() {
        if i >= MAX_ATTRIBUTES {
            return Err("文件属性过多".into());
        }
        let a = a.map_err(err)?;
        match a.ty().map_err(err)? {
            NtfsAttributeType::Data if a.name_length() != 0 => {
                attribute_name(a.record_bytes())?;
                let id = (
                    file.file_record_number(),
                    file.sequence_number(),
                    a.instance(),
                );
                if !base_ids.insert(id) {
                    return Err("数据流属性身份重复".into());
                }
                total += a.record_bytes().len();
                if total > MAX_BYTES {
                    return Err("数据流映射过大".into());
                }
                base.push((id, a.record_bytes().to_vec()));
            }
            NtfsAttributeType::AttributeList => {
                if list.is_some() || a.flags().bits() != 0 || a.value_length() > 1024 * 1024 {
                    return Err("数据流属性列表不支持或过大".into());
                }
                let mut b = vec![0; a.value_length() as usize];
                a.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut b)
                    .map_err(err)?;
                list = Some(references(&b)?);
            }
            _ => {}
        }
    }
    let Some(list) = list else {
        return Ok(base);
    };
    let mut result = vec![];
    let mut listed_base = HashSet::new();
    total = 0;
    for (record, sequence, instance, low, name) in list {
        let linked = file.ntfs().file(reader, record).map_err(err)?;
        let owner = if record == file.file_record_number() {
            0
        } else {
            file.file_record_number() | (u64::from(file.sequence_number()) << 48)
        };
        if linked.sequence_number() != sequence
            || linked.is_directory()
            || linked.flags().contains(NtfsFileFlags::IN_USE)
                != file.flags().contains(NtfsFileFlags::IN_USE)
            || crate::ntfs_names::base_reference(reader, &linked)? != owner
        {
            return Err("数据流扩展记录身份已变化".into());
        }
        let mut matched = None;
        for (i, a) in linked.attributes_raw().enumerate() {
            if i >= MAX_ATTRIBUTES {
                return Err("扩展记录属性过多".into());
            }
            let a = a.map_err(err)?;
            if a.ty().map_err(err)? == NtfsAttributeType::Data && a.instance() == instance {
                let b = a.record_bytes();
                if matched.is_some()
                    || attribute_name(b)? != name
                    || (if a.is_resident() {
                        0
                    } else {
                        if b.len() < 64 {
                            return Err("数据流头被截断".into());
                        }
                        u64le(b, 16)
                    }) != low
                {
                    return Err("数据流属性引用不一致".into());
                }
                matched = Some(b.to_vec());
            }
        }
        let b = matched.ok_or("数据流扩展属性缺失")?;
        total += b.len();
        if total > MAX_BYTES {
            return Err("数据流映射过大".into());
        }
        let id = (record, sequence, instance);
        if record == file.file_record_number() {
            listed_base.insert(id);
        }
        result.push((id, b));
    }
    if listed_base != base_ids {
        return Err("数据流基础属性与列表不一致".into());
    }
    Ok(result)
}
pub struct Stream {
    pub identity: Identity,
    pub size: u64,
    pub reason: Option<String>,
}
pub fn enumerate<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
) -> Result<Vec<Stream>, String> {
    let mut groups: BTreeMap<Vec<u8>, Vec<Record>> = BTreeMap::new();
    for record in records(reader, file)? {
        groups
            .entry(attribute_name(&record.1)?.to_vec())
            .or_default()
            .push(record);
    }
    if groups.len() > 256 {
        return Err("单文件命名数据流超过 256 项上限".into());
    }
    if groups.is_empty() {
        return Ok(vec![]);
    }
    let reparse = crate::ntfs_compression::is_reparse(reader, file)?;
    let mut result = vec![];
    for (name, records) in groups {
        let first = records
            .iter()
            .find(|(_, b)| b[8] == 0 || b.len() >= 64 && u64le(b, 16) == 0)
            .ok_or("数据流缺少起始属性")?;
        let size = if first.1[8] == 0 {
            u32::from_le_bytes(first.1[16..20].try_into().unwrap()) as u64
        } else {
            u64le(&first.1, 48)
        };
        let signature = signature(file, &name, &records);
        let reason = if reparse {
            Some("重解析/WOF/云端文件的数据流暂不支持".into())
        } else {
            plan(reader, file, &name, &records).err()
        };
        result.push(Stream {
            identity: Identity { name, signature },
            size,
            reason,
        });
    }
    Ok(result)
}
fn signature(file: &NtfsFile<'_>, name: &[u8], records: &[Record]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(file.ntfs().serial_number().to_le_bytes());
    h.update(file.ntfs().size().to_le_bytes());
    h.update(file.ntfs().cluster_size().to_le_bytes());
    h.update(name);
    for ((record, sequence, instance), b) in records {
        h.update(record.to_le_bytes());
        h.update(sequence.to_le_bytes());
        h.update(instance.to_le_bytes());
        h.update((b.len() as u64).to_le_bytes());
        h.update(b);
    }
    h.finalize().into()
}
struct Run {
    start: u64,
    bytes: u64,
    physical: Option<u64>,
}
enum Content {
    Resident(Vec<u8>),
    Plain,
    Compressed(crate::ntfs_compression::Plan),
}
struct Plan {
    size: u64,
    initialized: u64,
    runs: Vec<Run>,
    content: Content,
}
fn plan<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    name: &[u8],
    records: &[Record],
) -> Result<Plan, String> {
    let cluster = u64::from(file.ntfs().cluster_size());
    let volume = file.ntfs().size();
    if !cluster.is_power_of_two()
        || !(512..=65536).contains(&cluster)
        || volume > reader.seek(SeekFrom::End(0)).map_err(err)?
    {
        return Err("数据流卷范围或簇大小无效".into());
    }
    let mut sorted = records.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|(_, b)| if b[8] == 0 { 0 } else { u64le(b, 16) });
    let b = &sorted[0].1;
    let flags = u16le(b, 12);
    if flags & !0x8001 != 0 {
        return Err("EFS 或未知压缩方法的数据流暂不支持".into());
    }
    if b[8] == 0 {
        let start = usize::from(u16le(b, 20));
        let size = u32::from_le_bytes(b[16..20].try_into().unwrap()) as usize;
        let name_end = usize::from(u16le(b, 10)) + name.len();
        if sorted.len() != 1
            || flags != 0
            || start < name_end
            || start > b.len()
            || size > b.len().saturating_sub(start)
        {
            return Err("驻留数据流边界无效".into());
        }
        return Ok(Plan {
            size: size as u64,
            initialized: size as u64,
            runs: vec![],
            content: Content::Resident(b[start..start + size].to_vec()),
        });
    }
    if b.len() < 64 || b[8] != 1 {
        return Err("非驻留数据流头无效".into());
    }
    let size = u64le(b, 48);
    let initialized = u64le(b, 56);
    let allocated = u64le(b, 40);
    // Windows also uses the standard 16-cluster unit for uncompressed sparse
    // streams. Its virtual allocation may include the final unit's padding.
    // This does not make those bytes LZNT1-compressed: only flag 0x0001 does.
    let unit = b[34];
    if b[35..40] != [0; 5]
        || (if flags & 1 != 0 {
            unit != 4
        } else if flags & 0x8000 != 0 {
            unit != 0 && unit != 4
        } else {
            unit != 0
        })
    {
        return Err("数据流的压缩/稀疏单元字段不支持或无效".into());
    }
    let stored = if flags & 0x8001 != 0 {
        Some(u64le(b, 64))
    } else {
        None
    };
    if size > 1 << 43 || initialized > size {
        return Err("数据流容量无效".into());
    }
    let mut runs = vec![];
    let mut vcn = 0u64;
    for (_, b) in sorted {
        if b.len() < 64 || b[8] != 1 || u16le(b, 12) != flags || u64le(b, 16) != vcn {
            return Err("数据流范围有空洞、重叠或属性冲突".into());
        }
        if b[34] != unit || b[35..40] != [0; 5] {
            return Err("数据流扩展范围的压缩/稀疏单元字段不一致".into());
        }
        let high = u64le(b, 24);
        let mut at = usize::from(u16le(b, 32));
        let name_end = usize::from(u16le(b, 10)) + name.len();
        if at < name_end || at >= b.len() || at % 8 != 0 || high == u64::MAX {
            return Err("数据流运行列表边界无效".into());
        }
        let mut lcn = 0i128;
        loop {
            let header = *b.get(at).ok_or("数据流运行列表缺少结束标记")?;
            at += 1;
            if header == 0 {
                break;
            }
            let length = usize::from(header & 15);
            let offset = usize::from(header >> 4);
            if !(1..=8).contains(&length)
                || offset > 8
                || length + offset > b.len().saturating_sub(at)
                || runs.len() >= MAX_RUNS
            {
                return Err("数据流运行列表无效或过长".into());
            }
            let mut len = [0; 8];
            len[..length].copy_from_slice(&b[at..at + length]);
            at += length;
            let count = u64::from_le_bytes(len);
            if count == 0
                || count > volume / cluster
                || vcn.checked_add(count).is_none_or(|v| v > volume / cluster)
            {
                return Err("数据流虚拟簇范围无效".into());
            }
            let physical = if offset == 0 {
                if flags & 0x8001 == 0 {
                    return Err("普通数据流含未声明稀疏范围".into());
                }
                None
            } else {
                let mut delta = [if b[at + offset - 1] & 128 != 0 {
                    255
                } else {
                    0
                }; 8];
                delta[..offset].copy_from_slice(&b[at..at + offset]);
                at += offset;
                lcn += i64::from_le_bytes(delta) as i128;
                if lcn <= 0 || lcn + count as i128 > (volume / cluster) as i128 {
                    return Err("数据流物理簇越界".into());
                }
                Some(lcn as u64 * cluster)
            };
            runs.push(Run {
                start: vcn * cluster,
                bytes: count * cluster,
                physical,
            });
            vcn += count;
        }
        if vcn != high + 1 {
            return Err("数据流运行列表范围不完整".into());
        }
    }
    let logical_clusters = size.div_ceil(cluster);
    let plain_coverage = if flags & 0x8000 != 0 && unit == 4 {
        logical_clusters.div_ceil(16) * 16
    } else {
        logical_clusters
    };
    if vcn < logical_clusters
        || flags & 1 == 0 && vcn != plain_coverage
        || allocated != vcn * cluster
    {
        return Err("数据流映射长度与容量不一致".into());
    }
    let mut physical = runs
        .iter()
        .filter_map(|r| r.physical.map(|p| (p, p + r.bytes)))
        .collect::<Vec<_>>();
    physical.sort_unstable();
    if physical.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err("数据流物理簇重叠".into());
    }
    if stored.is_some_and(|s| s != physical.iter().map(|(a, b)| b - a).sum::<u64>()) {
        return Err("数据流实际分配长度不一致".into());
    }
    let content = if flags & 1 != 0 {
        Content::Compressed(crate::ntfs_compression::Plan::read_named(
            reader, file, name,
        )?)
    } else {
        Content::Plain
    };
    Ok(Plan {
        size,
        initialized,
        runs,
        content,
    })
}
fn allocation<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    runs: &[Run],
    mut tick: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if runs.is_empty() {
        return Ok(());
    }
    let bitmap = file.ntfs().file(reader, 6).map_err(err)?;
    let item = bitmap
        .data(reader, "")
        .ok_or("NTFS 分配位图缺失")?
        .map_err(err)?;
    let attr = item.to_attribute().map_err(err)?;
    let cluster = u64::from(file.ntfs().cluster_size());
    if attr.flags().bits() != 0 || attr.value_length() < (file.ntfs().size() / cluster).div_ceil(8)
    {
        return Err("NTFS 分配位图无效".into());
    }
    let mut value = attr.value(reader).map_err(err)?;
    let expected = file.flags().contains(NtfsFileFlags::IN_USE);
    let mut page = [0u8; 4096];
    let mut cached = u64::MAX;
    let mut valid = 0;
    for r in runs {
        if let Some(physical) = r.physical {
            for c in physical / cluster..(physical + r.bytes) / cluster {
                let start = c / 8 / 4096 * 4096;
                if cached != start {
                    tick()?;
                    valid = (attr.value_length() - start).min(4096) as usize;
                    value.seek(reader, SeekFrom::Start(start)).map_err(err)?;
                    value.read_exact(reader, &mut page[..valid]).map_err(err)?;
                    cached = start;
                }
                let at = (c / 8 - cached) as usize;
                if at >= valid || (page[at] & (1 << (c % 8)) != 0) != expected {
                    return Err("数据流簇分配状态与文件记录不一致，可能已被重用".into());
                }
            }
        }
    }
    Ok(())
}
pub fn extract<R: Read + Seek, W: Write>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    identity: &Identity,
    out: &mut W,
    limit: u64,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    identity.validate()?;
    if crate::ntfs_compression::is_reparse(reader, file)? {
        return Err("重解析文件数据流暂不支持".into());
    }
    let records = records(reader, file)?
        .into_iter()
        .filter(|(_, b)| attribute_name(b).is_ok_and(|n| n == identity.name))
        .collect::<Vec<_>>();
    if records.is_empty() || signature(file, &identity.name, &records) != identity.signature {
        return Err("数据流内容属性、布局或卷身份已变化，请重新扫描".into());
    }
    let p = plan(reader, file, &identity.name, &records)?;
    allocation(reader, file, &p.runs, || tick(0))?;
    let take = p.size.min(limit);
    let mut done = 0;
    match &p.content {
        Content::Resident(b) => {
            tick(0)?;
            out.write_all(&b[..take as usize]).map_err(err)?;
            done = take;
        }
        Content::Compressed(c) => {
            done = c.extract(reader, out, limit, &mut tick)?;
        }
        Content::Plain => {
            let mut buffer = vec![0; 64 * 1024];
            for r in &p.runs {
                let end = (r.start + r.bytes).min(take);
                while done < end {
                    tick(done)?;
                    let n = (end - done).min(buffer.len() as u64) as usize;
                    buffer[..n].fill(0);
                    let valid = p.initialized.saturating_sub(done).min(n as u64) as usize;
                    if let Some(physical) = r.physical {
                        if valid > 0 {
                            reader
                                .seek(SeekFrom::Start(physical + done - r.start))
                                .map_err(err)?;
                            reader.read_exact(&mut buffer[..valid]).map_err(err)?;
                        }
                    }
                    out.write_all(&buffer[..n]).map_err(err)?;
                    done += n as u64;
                }
                if done == take {
                    break;
                }
            }
        }
    }
    if done != take {
        return Err("数据流读取长度不完整".into());
    }
    allocation(reader, file, &p.runs, || tick(done))?;
    let fresh_ntfs = ntfs::Ntfs::new(reader).map_err(err)?;
    let fresh = fresh_ntfs
        .file(reader, file.file_record_number())
        .map_err(err)?;
    let after = records_for_identity(reader, &fresh, identity)?;
    if fresh.sequence_number() != file.sequence_number()
        || fresh.flags() != file.flags()
        || signature(&fresh, &identity.name, &after) != identity.signature
    {
        return Err("读取期间数据流记录或卷身份已变化".into());
    }
    tick(done)?;
    Ok(done)
}
fn records_for_identity<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    identity: &Identity,
) -> Result<Vec<Record>, String> {
    Ok(records(reader, file)?
        .into_iter()
        .filter(|(_, b)| attribute_name(b).is_ok_and(|n| n == identity.name))
        .collect())
}
#[cfg(test)]
#[path = "ntfs_streams_tests.rs"]
mod tests;

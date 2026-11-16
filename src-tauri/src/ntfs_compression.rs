//! Bounded NTFS LZNT1 reads over an existing volume reader. No paths or writes
//! to the source. Compression units are 16 clusters, at most 64 KiB.
use ntfs::{NtfsAttributeType, NtfsFile, NtfsFileFlags, NtfsReadSeek};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
};

const MAX_ATTRIBUTES: usize = 4096;
const MAX_RUNS: usize = 262144;
const MAX_MAPPING_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIST_BYTES: u64 = 1024 * 1024;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[derive(Clone, Debug)]
struct Run {
    vcn: u64,
    clusters: u64,
    lcn: Option<u64>,
}
struct Segment {
    low: u64,
    high: u64,
    allocated: u64,
    size: u64,
    initialized: u64,
    stored: Option<u64>,
    runs: Vec<Run>,
}
pub struct Plan {
    pub signature: [u8; 32],
    size: u64,
    initialized: u64,
    cluster: u64,
    coverage: u64,
    runs: Vec<Run>,
}

// MS-XCA 2.5: one independently decoded chunk is at most 4096 bytes.
// Decode every supplied chunk before returning bytes; do not turn malformed
// backreferences or truncated chunks into zero-filled successful output.
fn lznt1(input: &[u8], maximum: usize) -> Result<Vec<u8>, String> {
    if maximum > 65536 || input.len() > 65536 + 32 {
        return Err("压缩单元超过上限".into());
    }
    let mut out = Vec::with_capacity(maximum);
    let mut at = 0;
    while at < input.len() {
        if input.len() - at < 2 {
            return Err("LZNT1 块头被截断".into());
        }
        let header = u16le(input, at);
        at += 2;
        if header == 0 {
            break;
        } // Remaining bytes are allocation padding.
        if header & 0x7000 != 0x3000 {
            return Err("LZNT1 块签名无效".into());
        }
        let end = at + usize::from(header & 0xfff) + 1;
        if end > input.len() {
            return Err("LZNT1 块内容被截断".into());
        }
        let start = out.len();
        if header & 0x8000 == 0 {
            if out.len() + end - at > maximum {
                return Err("LZNT1 输出越界".into());
            }
            out.extend_from_slice(&input[at..end]);
            at = end;
        } else {
            while at < end {
                let flags = input[at];
                at += 1;
                for bit in 0..8 {
                    if at == end {
                        break;
                    }
                    if flags & (1 << bit) == 0 {
                        if out.len() - start >= 4096 || out.len() == maximum {
                            return Err("LZNT1 输出越界".into());
                        }
                        out.push(input[at]);
                        at += 1;
                    } else {
                        if end - at < 2 {
                            return Err("LZNT1 引用被截断".into());
                        }
                        let token = usize::from(u16le(input, at));
                        at += 2;
                        let produced = out.len() - start;
                        let mut shift = 12;
                        let mut boundary = 16;
                        while produced > boundary && shift > 4 {
                            shift -= 1;
                            boundary <<= 1;
                        }
                        let distance = (token >> shift) + 1;
                        let length = (token & ((1 << shift) - 1)) + 3;
                        if distance > produced
                            || produced + length > 4096
                            || out.len() + length > maximum
                        {
                            return Err("LZNT1 引用或输出越界".into());
                        }
                        for _ in 0..length {
                            out.push(out[out.len() - distance]);
                        }
                    }
                }
            }
        }
        if out.len() == start {
            return Err("LZNT1 空块".into());
        }
        if out.len() - start < 4096 && at + 2 <= input.len() && u16le(input, at) != 0 {
            return Err("LZNT1 非末尾短块不支持".into());
        }
    }
    Ok(out)
}

fn parse_segment(bytes: &[u8], cluster: u64, volume: u64) -> Result<Segment, String> {
    if bytes.len() < 64
        || bytes[8] != 1
        || u16le(bytes, 12) & !0x8000 != 1
        || bytes[34] != 4
        || bytes[35..40] != [0; 5]
    {
        return Err("不是受支持的 NTFS LZNT1 非驻留属性".into());
    }
    let low = u64le(bytes, 16);
    let high = u64le(bytes, 24);
    let mapping = usize::from(u16le(bytes, 32));
    let minimum = if low == 0 { 72 } else { 64 };
    let name = crate::ntfs_streams::attribute_name(bytes)?;
    let name_end = if name.is_empty() {
        minimum
    } else {
        usize::from(u16le(bytes, 10)) + name.len()
    };
    if low > high
        || high == u64::MAX
        || mapping < minimum
        || mapping < name_end
        || mapping >= bytes.len()
        || mapping % 8 != 0
    {
        return Err("压缩属性范围或映射偏移无效".into());
    }
    let mut at = mapping;
    let mut runs = vec![];
    let mut vcn = low;
    let mut lcn = 0i128;
    loop {
        let header = *bytes.get(at).ok_or("压缩运行列表缺少结束标记")?;
        at += 1;
        if header == 0 {
            break;
        }
        let length_bytes = usize::from(header & 15);
        let offset_bytes = usize::from(header >> 4);
        if !(1..=8).contains(&length_bytes)
            || offset_bytes > 8
            || at + length_bytes + offset_bytes > bytes.len()
            || runs.len() >= MAX_RUNS
        {
            return Err("压缩运行列表边界或数量无效".into());
        }
        let mut raw = [0; 8];
        raw[..length_bytes].copy_from_slice(&bytes[at..at + length_bytes]);
        at += length_bytes;
        let count = u64::from_le_bytes(raw);
        if count == 0 || vcn.checked_add(count).is_none_or(|n| n > high + 1) {
            return Err("压缩运行列表虚拟簇越界".into());
        }
        let physical = if offset_bytes == 0 {
            None
        } else {
            let mut raw = [if bytes[at + offset_bytes - 1] & 128 != 0 {
                255
            } else {
                0
            }; 8];
            raw[..offset_bytes].copy_from_slice(&bytes[at..at + offset_bytes]);
            at += offset_bytes;
            lcn += i64::from_le_bytes(raw) as i128;
            if lcn < 0 || lcn + count as i128 > i128::from(volume / cluster) {
                return Err("压缩运行列表物理簇越界".into());
            }
            Some(lcn as u64)
        };
        runs.push(Run {
            vcn,
            clusters: count,
            lcn: physical,
        });
        vcn += count;
    }
    if vcn != high + 1 {
        return Err("压缩运行列表没有覆盖声明范围".into());
    }
    Ok(Segment {
        low,
        high,
        allocated: u64le(bytes, 40),
        size: u64le(bytes, 48),
        initialized: u64le(bytes, 56),
        stored: if low == 0 {
            Some(u64le(bytes, 64))
        } else {
            None
        },
        runs,
    })
}

// Match exact raw UTF-16 stream name; preserve record/sequence/instance/VCN.
fn data_references(bytes: &[u8], stream: &[u8]) -> Result<Vec<(u64, u16, u16, u64)>, String> {
    let mut at = 0;
    let mut refs = vec![];
    let mut seen = HashSet::new();
    let mut count = 0;
    while at < bytes.len() {
        count += 1;
        if bytes.len() - at < 26 || count > MAX_ATTRIBUTES {
            return Err("压缩属性列表被截断或过长".into());
        }
        let b = &bytes[at..];
        let len = usize::from(u16le(b, 4));
        let name = usize::from(b[6]) * 2;
        let name_at = usize::from(b[7]);
        if len < 26
            || len % 8 != 0
            || len > b.len()
            || (name > 0 && (name_at < 26 || name_at + name > len))
        {
            return Err("压缩属性列表边界无效".into());
        }
        if b[..4] == 0xc0u32.to_le_bytes() {
            return Err("重解析文件暂不支持".into());
        }
        if b[..4] == 0x80u32.to_le_bytes()
            && (if name == 0 {
                &[][..]
            } else {
                &b[name_at..name_at + name]
            }) == stream
        {
            let reference = u64le(b, 16);
            let id = (
                reference & 0x0000_ffff_ffff_ffff,
                (reference >> 48) as u16,
                u16le(b, 24),
                u64le(b, 8),
            );
            if !seen.insert(id) {
                return Err("压缩属性列表重复引用".into());
            }
            refs.push(id);
        }
        at += len;
    }
    Ok(refs)
}

pub fn is_reparse<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<bool, String> {
    for (i, a) in file.attributes_raw().enumerate() {
        if i >= MAX_ATTRIBUTES {
            return Err("文件属性过多".into());
        }
        let a = a.map_err(err)?;
        match a.ty().map_err(err)? {
            NtfsAttributeType::ReparsePoint => return Ok(true),
            NtfsAttributeType::AttributeList => {
                if a.value_length() > MAX_LIST_BYTES || a.flags().bits() != 0 {
                    return Err("属性列表无法核对".into());
                }
                let mut bytes = vec![0; a.value_length() as usize];
                a.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut bytes)
                    .map_err(err)?;
                // Reject listed reparse attributes even when located only in an
                // extension record and no longer reflected by standard flags.
                data_references(&bytes, &[])?;
            }
            NtfsAttributeType::StandardInformation => {
                let b = a.record_bytes();
                if b.len() < 24 || b[8] != 0 {
                    return Err("标准文件属性无效".into());
                }
                let at = usize::from(u16le(b, 20));
                let size = u32::from_le_bytes(b[16..20].try_into().unwrap()) as usize;
                if at < 24 || size < 36 || size > b.len().saturating_sub(at) {
                    return Err("标准文件属性被截断".into());
                }
                let flags = u32::from_le_bytes(b[at + 32..at + 36].try_into().unwrap());
                if flags & 0x400 != 0 {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

impl Plan {
    pub fn read<R: Read + Seek>(reader: &mut R, file: &NtfsFile<'_>) -> Result<Self, String> {
        Self::read_named(reader, file, &[])
    }
    pub fn read_named<R: Read + Seek>(
        reader: &mut R,
        file: &NtfsFile<'_>,
        stream: &[u8],
    ) -> Result<Self, String> {
        crate::ntfs_streams::validate_name(stream, true)?;
        let ntfs = file.ntfs();
        let cluster = u64::from(ntfs.cluster_size());
        if ntfs.size() > reader.seek(SeekFrom::End(0)).map_err(err)?
            || ntfs.file_record_size() > 256 * 1024
        {
            return Err("NTFS 卷或文件记录超出读取范围".into());
        }
        if !matches!(cluster, 512 | 1024 | 2048 | 4096)
            || is_reparse(reader, file)?
            || crate::ntfs_names::base_reference(reader, file)? != 0
        {
            return Err("压缩文件簇大小、重解析点或记录类型不支持".into());
        }
        let mut records = vec![];
        let mut base_ids = HashSet::new();
        let mut list = None;
        for (i, a) in file.attributes_raw().enumerate() {
            if i >= MAX_ATTRIBUTES {
                return Err("文件属性过多".into());
            }
            let a = a.map_err(err)?;
            match a.ty().map_err(err)? {
                NtfsAttributeType::Data
                    if crate::ntfs_streams::attribute_name(a.record_bytes())? == stream =>
                {
                    let id = (
                        file.file_record_number(),
                        file.sequence_number(),
                        a.instance(),
                    );
                    if !base_ids.insert(id) {
                        return Err("数据属性身份重复".into());
                    }
                    records.push((id, a.record_bytes().to_vec()));
                }
                NtfsAttributeType::AttributeList => {
                    if list.is_some() || a.value_length() > MAX_LIST_BYTES || a.flags().bits() != 0
                    {
                        return Err("压缩属性列表不支持或过大".into());
                    }
                    let mut b = vec![0; a.value_length() as usize];
                    a.value(reader)
                        .map_err(err)?
                        .read_exact(reader, &mut b)
                        .map_err(err)?;
                    list = Some(data_references(&b, stream)?);
                }
                _ => {}
            }
        }
        if let Some(refs) = list {
            records.clear();
            let mut listed_base = HashSet::new();
            let mut total = 0;
            for (record, sequence, instance, low) in refs {
                let linked = ntfs.file(reader, record).map_err(err)?;
                let owner = if record == file.file_record_number() {
                    0
                } else {
                    file.file_record_number() | u64::from(file.sequence_number()) << 48
                };
                if linked.sequence_number() != sequence
                    || linked.is_directory()
                    || linked.flags().contains(NtfsFileFlags::IN_USE)
                        != file.flags().contains(NtfsFileFlags::IN_USE)
                    || crate::ntfs_names::base_reference(reader, &linked)? != owner
                {
                    return Err("压缩扩展记录身份或所属文件不匹配".into());
                }
                let mut matched = None;
                for (i, a) in linked.attributes_raw().enumerate() {
                    if i >= MAX_ATTRIBUTES {
                        return Err("扩展属性过多".into());
                    }
                    let a = a.map_err(err)?;
                    if a.ty().map_err(err)? == NtfsAttributeType::Data
                        && crate::ntfs_streams::attribute_name(a.record_bytes())? == stream
                        && a.instance() == instance
                    {
                        if matched.is_some()
                            || a.record_bytes().len() < 64
                            || u64le(a.record_bytes(), 16) != low
                        {
                            return Err("压缩属性引用不一致".into());
                        }
                        matched = Some(a.record_bytes().to_vec());
                    }
                }
                let bytes = matched.ok_or("压缩扩展属性缺失")?;
                total += bytes.len();
                if total > MAX_MAPPING_BYTES {
                    return Err("压缩映射超过内存上限".into());
                }
                let id = (record, sequence, instance);
                if record == file.file_record_number() {
                    listed_base.insert(id);
                }
                records.push((id, bytes));
            }
            if listed_base != base_ids {
                return Err("基础压缩数据属性与列表不一致".into());
            }
        }
        if records.is_empty()
            || records.iter().map(|(_, b)| b.len()).sum::<usize>() > MAX_MAPPING_BYTES
        {
            return Err("压缩数据属性缺失或过大".into());
        }
        let mut segments = vec![];
        let mut hash = Sha256::new();
        hash.update(ntfs.serial_number().to_le_bytes());
        hash.update(ntfs.size().to_le_bytes());
        hash.update(cluster.to_le_bytes());
        for ((record, sequence, instance), bytes) in records {
            hash.update(record.to_le_bytes());
            hash.update(sequence.to_le_bytes());
            hash.update(instance.to_le_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
            segments.push(parse_segment(&bytes, cluster, ntfs.size())?);
        }
        segments.sort_by_key(|s| s.low);
        let base = &segments[0];
        let size = base.size;
        let initialized = base.initialized;
        let allocated = base.allocated;
        let stored = base.stored;
        if base.low != 0 || size == 0 || size > 1 << 43 || initialized > size {
            return Err("压缩文件大小或初始化范围无效".into());
        }
        let mut runs = vec![];
        let mut coverage = 0;
        for s in segments {
            if s.low != coverage || runs.len() + s.runs.len() > MAX_RUNS {
                return Err("压缩文件映射有空洞、重叠或过多运行".into());
            }
            coverage = s.high + 1;
            runs.extend(s.runs);
        }
        let needed = size.div_ceil(cluster);
        if coverage < needed
            || coverage > needed.div_ceil(16) * 16
            || allocated != coverage * cluster
        {
            return Err("压缩文件分配范围与容量不一致".into());
        }
        let actual = runs
            .iter()
            .filter(|r| r.lcn.is_some())
            .map(|r| r.clusters * cluster)
            .sum::<u64>();
        if stored != Some(actual) {
            return Err("压缩文件实际分配容量不一致".into());
        }
        let mut sparse = false;
        let mut physical = vec![];
        for r in &runs {
            if let Some(lcn) = r.lcn {
                if sparse && r.vcn % 16 != 0 {
                    return Err("压缩单元的稀疏尾之后出现数据".into());
                }
                physical.push((lcn, lcn + r.clusters));
                sparse = false;
            } else {
                sparse = true;
            }
        }
        physical.sort_unstable();
        if physical.windows(2).any(|w| w[0].1 > w[1].0) {
            return Err("压缩文件物理运行重叠".into());
        }
        Ok(Self {
            signature: hash.finalize().into(),
            size,
            initialized,
            cluster,
            coverage,
            runs,
        })
    }

    pub fn extract<R: Read + Seek, W: Write>(
        &self,
        reader: &mut R,
        out: &mut W,
        limit: u64,
        mut tick: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<u64, String> {
        let take = self.size.min(limit);
        let unit = 16 * self.cluster;
        let mut done = 0;
        let mut run_index = 0;
        while done < take {
            tick(done)?;
            let n = (take - done).min(unit) as usize;
            let valid = self.initialized.saturating_sub(done).min(unit) as usize;
            let mut decoded = if valid == 0 {
                vec![0; unit as usize]
            } else {
                let first = done / self.cluster;
                let end = (first + 16).min(self.coverage);
                let mut vcn = first;
                let mut packed = vec![];
                let mut has_hole = false;
                while vcn < end {
                    while self.runs[run_index].vcn + self.runs[run_index].clusters <= vcn {
                        run_index += 1;
                    }
                    let r = &self.runs[run_index];
                    let clusters = (r.vcn + r.clusters).min(end) - vcn;
                    if let Some(lcn) = r.lcn {
                        let at = (lcn + vcn - r.vcn) * self.cluster;
                        let len = (clusters * self.cluster) as usize;
                        let old = packed.len();
                        packed.resize(old + len, 0);
                        reader.seek(SeekFrom::Start(at)).map_err(err)?;
                        reader.read_exact(&mut packed[old..]).map_err(err)?;
                    } else {
                        has_hole = true;
                    }
                    vcn += clusters;
                }
                if packed.is_empty() {
                    vec![0; unit as usize]
                } else if has_hole {
                    lznt1(&packed, unit as usize)?
                } else {
                    packed
                }
            };
            if decoded.len() < valid {
                return Err("压缩单元解码长度小于已初始化内容".into());
            }
            decoded.resize(unit as usize, 0);
            decoded[valid..].fill(0);
            out.write_all(&decoded[..n]).map_err(err)?;
            done += n as u64;
        }
        tick(done)?;
        Ok(done)
    }
}

#[cfg(test)]
#[path = "ntfs_compression_tests.rs"]
mod tests;

//! FAT directory-entry-group checkpoints. Child of fat_recovery: all locations
//! are derived from checked directory bytes, never deserialized as locators.
use super::*;
use crate::recovery_tree::Catalog;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const MAX_DIRECTORY: u64 = 64 * 1024 * 1024;
const MAX_VISITED: u64 = 256 * 1024 * 1024;
type Entries = Vec<(u64, Vec<u8>)>;

struct Entry {
    name: String,
    first: u32,
    size: u64,
    valid: u64,
    contiguous: bool,
    deleted: bool,
    directory: bool,
    metadata: crate::file_metadata::Metadata,
}
fn decode(bits: u8, entries: &Entries, size: u64) -> Result<Entry, String> {
    if entries.is_empty() || entries.len() > 21 {
        return Err("保存目录项组长度无效".into());
    }
    let mut seen = HashSet::new();
    for (offset, bytes) in entries {
        if bytes.len() != 32
            || *offset % 32 != 0
            || offset.checked_add(32).is_none_or(|end| end > size)
            || !seen.insert(*offset)
        {
            return Err("保存目录项范围无效".into());
        }
    }
    let b = if bits == 64 {
        &entries[0].1
    } else {
        &entries.last().unwrap().1
    };
    let entry = if bits == 64 {
        if !matches!(b[0], 0x85 | 0x05)
            || !(2..=18).contains(&b[1])
            || entries.len() != b[1] as usize + 1
        {
            return Err("exFAT 文件项组不完整".into());
        }
        let deleted = b[0] == 5;
        let active = if deleted { 0 } else { 0x80 };
        if entries[1].1[0] != 0x40 | active
            || entries[2..].iter().any(|(_, x)| x[0] != 0x41 | active)
        {
            return Err("exFAT 次级目录项类型不匹配".into());
        }
        let sum = entries
            .iter()
            .flat_map(|(_, x)| x)
            .enumerate()
            .filter(|(i, _)| ![2, 3].contains(i))
            .fold(0u16, |sum, (i, x)| {
                sum.rotate_right(1)
                    .wrapping_add((if i % 32 == 0 { *x | 0x80 } else { *x }) as u16)
            });
        if sum != u16at(b, 2) {
            return Err("exFAT 文件项组校验失败".into());
        }
        let stream = &entries[1].1;
        let chars: Vec<_> = entries[2..]
            .iter()
            .flat_map(|(_, x)| (2..32).step_by(2).map(|n| u16at(x, n)))
            .collect();
        let n = stream[3] as usize;
        if n == 0 || n > 255 || n > chars.len() || stream[1] & !3 != 0 || stream[1] & 1 == 0 {
            return Err("exFAT 名称或数据流标志无效".into());
        }
        Entry {
            name: String::from_utf16_lossy(&chars[..n]),
            first: u32at(stream, 20),
            size: u64at(stream, 24),
            valid: u64at(stream, 8),
            contiguous: stream[1] & 2 != 0,
            deleted,
            directory: u16at(b, 4) & 0x10 != 0,
            metadata: crate::file_metadata::fat(b, true),
        }
    } else {
        if !matches!(bits, 12 | 16 | 32)
            || b[0] == 0
            || b[0] == b'.'
            || b[11] & 0xc8 != 0
            || b[11] == 15
            || entries[..entries.len() - 1]
                .iter()
                .any(|(_, x)| x[0] == 0 || x[11] != 15)
        {
            return Err("FAT 文件目录项无效".into());
        }
        let deleted = b[0] == 0xe5;
        let length = u32at(b, 28) as u64;
        Entry {
            name: fat_name(b, &entries[..entries.len() - 1], deleted),
            first: ((if bits == 32 { u16at(b, 20) as u32 } else { 0 }) << 16) | u16at(b, 26) as u32,
            size: length,
            valid: length,
            contiguous: deleted,
            deleted,
            directory: b[11] & 16 != 0,
            metadata: crate::file_metadata::fat(b, false),
        }
    };
    if entry.name.is_empty() || entry.name.len() > 1024 || entry.name.contains('\0') {
        return Err("目录项名称无效".into());
    }
    Ok(entry)
}
pub(super) fn extraction_metadata(
    bits: u8,
    location: &Location,
    size: u64,
    c: &Candidate,
) -> Result<crate::file_metadata::Metadata, String> {
    let entry = decode(bits, &location.entries, size)?;
    if entry.directory
        || entry.name != c.name
        || entry.size != c.size
        || entry.deleted != c.deleted
        || entry.first != location.first
        || entry.valid != location.valid
        || entry.contiguous != location.contiguous
    {
        return Err("提取记录与候选身份不一致。".into());
    }
    Ok(entry.metadata)
}
fn entry_id(bits: u8, entries: &Entries, first: u32) -> String {
    let offset = if bits == 64 {
        entries[0].0
    } else {
        entries.last().unwrap().0
    };
    format!("fat-{offset}-{first}")
}
fn directory_details(
    boot: [u8; 32],
    entries: &Entries,
    entry: &Entry,
) -> crate::file_metadata::Metadata {
    let mut metadata = entry.metadata.clone();
    metadata.directory_proof = Some(crate::directory_metadata::Proof::Fat(Location {
        boot,
        entries: entries.clone(),
        first: entry.first,
        contiguous: entry.contiguous,
        valid: entry.valid,
    }));
    metadata
}
pub(crate) fn directory_metadata(
    r: &mut (impl Read + Seek),
    location: &Location,
    name: &str,
) -> Result<crate::file_metadata::Metadata, String> {
    let fs = Fs::load(r)?;
    if fs.boot != location.boot {
        return Err("FAT 卷身份已变化。".into());
    }
    for (offset, bytes) in &location.entries {
        if at(r, *offset, bytes.len())? != *bytes {
            return Err("目录项已变化或被复用。".into());
        }
    }
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let entry = decode(fs.bits, &location.entries, size)?;
    if !entry.directory
        || entry.deleted
        || entry.name != name
        || entry.first != location.first
        || entry.valid != location.valid
        || entry.contiguous != location.contiguous
    {
        return Err("FAT 目录身份与凭据不符。".into());
    }
    if fs.bits == 64 && (entry.size == 0 || entry.size > MAX_DIRECTORY) {
        return Err("exFAT 目录范围无效。".into());
    }
    let clusters = fs.chain(
        r,
        entry.first,
        if fs.bits == 64 {
            Some(entry.size)
        } else {
            None
        },
        fs.bits == 64 && entry.contiguous,
        false,
    )?;
    if clusters.len() as u64 * fs.cluster > MAX_DIRECTORY {
        return Err("FAT 目录范围超过上限。".into());
    }
    Ok(entry.metadata)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Node {
    parent: Option<usize>,
    entries: Entries,
    first: u32,
    contiguous: bool,
    length: u64,
    digest: Option<[u8; 32]>,
    bytes: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    boot: Option<[u8; 32]>,
    bitmap: Option<[u8; 32]>,
    bits: u8,
    size: u64,
    clusters: u32,
    nodes: Vec<Node>,
    next_dir: usize,
    next_slot: usize,
    pub records: u64,
    pub count: usize,
    skipped: usize,
    pub sealed: bool,
}
impl Checkpoint {
    pub fn kind(&self) -> &'static str {
        if self.bits == 64 {
            "exfat"
        } else {
            "fat"
        }
    }
    fn node_id(&self, index: usize) -> String {
        if index == 0 {
            ROOT.into()
        } else {
            let n = &self.nodes[index];
            entry_id(self.bits, &n.entries, n.first)
        }
    }
    pub fn validate(&self, mode: ScanMode, size: u64) -> Result<(), String> {
        if mode == ScanMode::Signatures
            || !matches!(self.bits, 12 | 16 | 32 | 64)
            || self.boot.is_none()
            || self.bitmap.is_none()
            || self.size != size
            || size < 512
            || self.clusters == 0
            || self.nodes.is_empty()
            || self.nodes.len() > MAX_RESULTS
            || self.next_dir > self.nodes.len()
            || self.count > MAX_RESULTS
            || self.records > MAX_VISITED / 32
            || self.skipped as u64 > self.records
            || (!self.sealed && (self.next_dir == self.nodes.len() || self.count == MAX_RESULTS))
            || (self.sealed && self.next_dir != self.nodes.len() && self.count != MAX_RESULTS)
        {
            return Err("FAT/exFAT 续扫状态或计数无效".into());
        }
        let mut seen = HashSet::new();
        let mut depths = Vec::new();
        let mut bytes = 0u64;
        for (index, n) in self.nodes.iter().enumerate() {
            if !seen.insert(n.first)
                || n.first as u64 >= self.clusters as u64 + 2
                || (n.first < 2 && !(index == 0 && matches!(self.bits, 12 | 16) && n.first == 0))
                || n.bytes > MAX_DIRECTORY
                || n.bytes % 32 != 0
                || n.digest.is_some() != (n.bytes > 0)
                || (index < self.next_dir && n.digest.is_none())
                || (index > self.next_dir && n.digest.is_some())
            {
                return Err("保存目录队列或范围无效".into());
            }
            bytes = bytes.checked_add(n.bytes).ok_or("目录容量溢出")?;
            if bytes > MAX_VISITED {
                return Err("目录扫描超过 256 MiB 上限".into());
            }
            if index == 0 {
                if n.parent.is_some() || !n.entries.is_empty() || n.contiguous || n.length != 0 {
                    return Err("保存根目录无效".into());
                }
                depths.push(1);
            } else {
                let p = n
                    .parent
                    .filter(|p| *p < index && *p <= self.next_dir)
                    .ok_or("保存目录父节点无效")?;
                if self.nodes[p].digest.is_none() {
                    return Err("父目录尚未扫描".into());
                }
                let entry = decode(self.bits, &n.entries, size)?;
                if !entry.directory
                    || entry.deleted
                    || entry.first != n.first
                    || (self.bits == 64
                        && (entry.contiguous != n.contiguous || entry.size != n.length))
                    || (self.bits != 64 && (n.contiguous || n.length != 0))
                    || depths[p] >= MAX_DEPTH
                {
                    return Err("保存子目录与目录项不一致".into());
                }
                depths.push(depths[p] + 1);
            }
        }
        if self.records > bytes / 32 {
            return Err("保存目录记录计数越界".into());
        }
        if self.next_dir == self.nodes.len() {
            if self.next_slot != 0 {
                return Err("完成位置无效".into());
            }
        } else {
            let n = &self.nodes[self.next_dir];
            if self.next_slot as u64 > n.bytes / 32 || (self.next_slot > 0 && n.digest.is_none()) {
                return Err("目录续扫边界越界".into());
            }
        }
        Ok(())
    }
    fn tree(&self) -> Result<Catalog, String> {
        let mut tree = Catalog::default();
        for (i, n) in self.nodes.iter().enumerate().skip(1) {
            let e = decode(self.bits, &n.entries, self.size)?;
            let metadata = directory_details(self.boot.ok_or("FAT 卷凭据缺失")?, &n.entries, &e);
            tree.insert(Directory {
                id: self.node_id(i),
                parent: Some(self.node_id(n.parent.unwrap())),
                name: e.name,
                deleted: false,
                metadata,
            })?;
        }
        Ok(tree)
    }
    pub fn verify(
        &self,
        r: &mut (impl Read + Seek),
        mode: ScanMode,
        size: u64,
    ) -> Result<(), String> {
        self.verify_records(r, mode, size, None)
    }
    pub fn verify_records(
        &self,
        r: &mut (impl Read + Seek),
        mode: ScanMode,
        size: u64,
        records: Option<&[Record]>,
    ) -> Result<(), String> {
        self.validate(mode, size)?;
        let fs = Fs::load(r)?;
        if r.seek(SeekFrom::End(0)).map_err(e)? != size
            || Some(fs.boot) != self.boot
            || Some(Sha256::digest(&fs.bitmap).into()) != self.bitmap
            || fs.bits != self.bits
            || fs.count != self.clusters
            || fs.root != self.nodes[0].first
        {
            return Err("FAT/exFAT 引导、位图或几何布局已变化".into());
        }
        let mut children: HashMap<usize, Vec<&Entries>> = HashMap::new();
        for node in &self.nodes[1..] {
            children
                .entry(node.parent.unwrap())
                .or_default()
                .push(&node.entries);
        }
        if let Some(records) = records {
            // Keep this entry point safe even if a caller skipped portable-file
            // validation. No indexing or source reads from unchecked records.
            if records.len() != self.count {
                return Err("保存候选计数不一致".into());
            }
            for record in records {
                if record.directory >= self.nodes.len() || record.directory > self.next_dir {
                    return Err("保存候选父目录越界".into());
                }
                decode(self.bits, &record.entries, size)?;
                children
                    .entry(record.directory)
                    .or_default()
                    .push(&record.entries);
            }
        }
        let mut consumed = 0u64;
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(expected) = node.digest {
                let (bytes, offsets, digest) = read_directory(&fs, r, node)?;
                if digest != expected || bytes.len() as u64 != node.bytes {
                    return Err("已扫描目录内容或簇链已变化，请重新扫描".into());
                }
                let limit = if index == self.next_dir {
                    self.next_slot
                } else {
                    offsets.len()
                };
                let mut groups = HashMap::new();
                let mut end = 0;
                while end < limit {
                    let (step, _, finished) = group(self.bits, &bytes, &offsets, end);
                    if !finished {
                        groups.insert(offsets[end], (end, step));
                    }
                    end += step;
                    if finished {
                        break;
                    }
                }
                if index == self.next_dir && end != self.next_slot {
                    return Err("续扫位置位于目录项组中间或越过结束项".into());
                }
                consumed += end as u64;
                for entries in children.remove(&index).unwrap_or_default() {
                    let (start, step) = groups
                        .get(&entries[0].0)
                        .copied()
                        .ok_or("保存目录项不是已扫描父目录中的完整项组")?;
                    if entries.len() != step
                        || start + entries.len() > end
                        || entries.iter().enumerate().any(|(i, (at, b))| {
                            offsets[start + i] != *at
                                || bytes[(start + i) * 32..(start + i + 1) * 32] != *b
                        })
                    {
                        return Err("保存目录项超出已扫描前缀或与原目录不符".into());
                    }
                }
            }
        }
        if !children.is_empty() || consumed != self.records {
            return Err("候选引用未扫描目录或已扫描记录计数不符".into());
        }
        Ok(())
    }
}
fn read_directory(
    fs: &Fs,
    r: &mut (impl Read + Seek),
    n: &Node,
) -> Result<(Vec<u8>, Vec<u64>, [u8; 32]), String> {
    let bytes = fs.directory(r, n.first, n.contiguous, n.length)?;
    let mut offsets = fs.entry_offsets(r, n.first, n.contiguous, n.length)?;
    if bytes.is_empty() || bytes.len() % 32 != 0 || offsets.len() < bytes.len() / 32 {
        return Err("目录长度或位置映射无效".into());
    }
    offsets.truncate(bytes.len() / 32);
    let mut hash = Sha256::new();
    for p in &offsets {
        hash.update(p.to_le_bytes());
    }
    hash.update(&bytes);
    Ok((bytes, offsets, hash.finalize().into()))
}
// Consume one whole name/stream group, including invalid groups as one bounded unit.
fn group(bits: u8, bytes: &[u8], offsets: &[u64], at: usize) -> (usize, Entries, bool) {
    let slots = bytes.len() / 32;
    if bytes[at * 32] == 0 {
        return (1, vec![], true);
    }
    let mut end = at + 1;
    if bits == 64 && matches!(bytes[at * 32], 0x85 | 0x05) {
        let n = bytes[at * 32 + 1] as usize;
        if (2..=18).contains(&n) && at + n < slots {
            end = at + n + 1;
        }
    } else if bits != 64 {
        while end <= slots && bytes[(end - 1) * 32 + 11] == 15 && end - at <= 20 {
            if end == slots {
                break;
            }
            if bytes[end * 32] == 0 {
                return (end - at + 1, vec![], true);
            }
            end += 1;
        }
    }
    (
        end - at,
        (at..end)
            .map(|i| (offsets[i], bytes[i * 32..(i + 1) * 32].to_vec()))
            .collect(),
        false,
    )
}
pub fn continue_scan(
    r: &mut (impl Read + Seek),
    mode: ScanMode,
    cp: &mut Checkpoint,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory_found: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    if mode == ScanMode::Signatures {
        return Err("目录扫描模式无效".into());
    }
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    if cp.boot.is_some() {
        cp.verify(r, mode, size)?;
        if cp.sealed {
            return Err("目录扫描已经结束".into());
        }
    }
    let fs = Fs::load(r)?;
    if cp.boot.is_none() {
        *cp = Checkpoint {
            boot: Some(fs.boot),
            bitmap: Some(Sha256::digest(&fs.bitmap).into()),
            bits: fs.bits,
            size,
            clusters: fs.count,
            nodes: vec![Node {
                parent: None,
                entries: vec![],
                first: fs.root,
                contiguous: false,
                length: 0,
                digest: None,
                bytes: 0,
            }],
            ..Default::default()
        };
    }
    let mut seen: HashSet<_> = cp.nodes.iter().map(|n| n.first).collect();
    while cp.next_dir < cp.nodes.len() {
        tick(cp.records, 0)?;
        let node = cp.nodes[cp.next_dir].clone();
        let (bytes, offsets, digest) = read_directory(&fs, r, &node)?;
        if let Some(old) = node.digest {
            if old != digest {
                return Err("当前目录在扫描期间变化".into());
            }
        } else {
            if cp.nodes.iter().map(|n| n.bytes).sum::<u64>() + bytes.len() as u64 > MAX_VISITED {
                return Err("目录扫描超过 256 MiB 上限".into());
            }
            cp.nodes[cp.next_dir].digest = Some(digest);
            cp.nodes[cp.next_dir].bytes = bytes.len() as u64;
        }
        let parent = cp.node_id(cp.next_dir);
        let mut depth = 1;
        let mut at = cp.next_dir;
        while let Some(p) = cp.nodes[at].parent {
            depth += 1;
            at = p;
        }
        while cp.next_slot < offsets.len() {
            tick(cp.records, 0)?;
            let (step, entries, end) = group(fs.bits, &bytes, &offsets, cp.next_slot);
            if !end {
                match decode(fs.bits, &entries, size) {
                    Ok(entry) if entry.directory => {
                        if !entry.deleted
                            && depth < MAX_DEPTH
                            && cp.nodes.len() < MAX_RESULTS
                            && fs.offset(entry.first).is_ok()
                            && seen.insert(entry.first)
                        {
                            let id = entry_id(fs.bits, &entries, entry.first);
                            let metadata = directory_details(fs.boot, &entries, &entry);
                            directory_found(Directory {
                                id,
                                parent: Some(parent.clone()),
                                name: entry.name,
                                deleted: false,
                                metadata,
                            })?;
                            cp.nodes.push(Node {
                                parent: Some(cp.next_dir),
                                entries,
                                first: entry.first,
                                contiguous: fs.bits == 64 && entry.contiguous,
                                length: if fs.bits == 64 { entry.size } else { 0 },
                                digest: None,
                                bytes: 0,
                            });
                        } else {
                            cp.skipped += 1;
                        }
                    }
                    Ok(entry) if mode.includes(entry.deleted) => {
                        let support = if entry.valid > entry.size {
                            Err("有效数据长度越界".into())
                        } else if fs.bits == 64 && entry.deleted && !entry.contiguous {
                            Err("已删除的碎片簇链不可靠，暂不导出".into())
                        } else {
                            fs.chain(
                                r,
                                entry.first,
                                Some(entry.size),
                                entry.contiguous,
                                entry.deleted,
                            )
                            .map(|_| ())
                        };
                        found(make_candidate(
                            cp.count,
                            entry,
                            entries,
                            fs.boot,
                            fs.bits,
                            parent.clone(),
                            support,
                        ));
                        cp.count += 1;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        // Dot entries, volume labels, exFAT system entries and orphan
                        // LFN slots are intentionally ignored, matching normal enumeration.
                        if fs.bits == 64 && matches!(bytes[cp.next_slot * 32], 0x85 | 0x05) {
                            cp.skipped += 1;
                        }
                    }
                }
            }
            cp.next_slot += step;
            cp.records += step as u64;
            if cp.count == MAX_RESULTS {
                cp.sealed = true;
                return Ok(format!(
                    "达到 {MAX_RESULTS} 条结果上限；已跳过 {} 个无效目录项。",
                    cp.skipped
                ));
            }
            if end {
                break;
            }
        }
        cp.next_dir += 1;
        cp.next_slot = 0;
    }
    cp.sealed = true;
    tick(cp.records, cp.records)?;
    Ok(format!("扫描完成，找到 {} 个文件，跳过 {} 个无效或未支持的目录项。不遍历已删除、重复引用或超过 64 层的目录；已删除连续文件仅为候选，不能保证未覆盖或未被 TRIM。",cp.count,cp.skipped))
}
fn make_candidate(
    id: usize,
    entry: Entry,
    entries: Entries,
    boot: [u8; 32],
    bits: u8,
    parent: String,
    support: Result<(), String>,
) -> Candidate {
    let method = if bits == 64 {
        "exFAT".into()
    } else {
        format!("FAT{bits}")
    };
    candidate(
        id,
        entry.name,
        entry.size,
        entry.deleted,
        &method,
        entry.metadata,
        support,
        Location {
            boot,
            entries,
            first: entry.first,
            contiguous: entry.contiguous,
            valid: entry.valid,
        },
        parent,
    )
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    directory: usize,
    entries: Entries,
    supported: bool,
    note: String,
}
pub fn capture(
    cp: &Checkpoint,
    files: &[Candidate],
    tree: &Catalog,
) -> Result<Vec<Record>, String> {
    if cp.tree()?.snapshot() != tree.snapshot() {
        return Err("FAT 保存目录树与队列不一致".into());
    }
    let ids: HashMap<_, _> = (0..cp.nodes.len()).map(|i| (cp.node_id(i), i)).collect();
    files
        .iter()
        .map(|c| {
            let Locator::Fat(l) = &c.locator else {
                return Err("不是 FAT 候选".into());
            };
            if Some(l.boot) != cp.boot {
                return Err("候选引导身份不一致".into());
            }
            let directory = *ids
                .get(c.directory.as_deref().unwrap_or(""))
                .ok_or("候选父目录不在队列中")?;
            Ok(Record {
                directory,
                entries: l.entries.clone(),
                supported: c.supported,
                note: c.note.clone(),
            })
        })
        .collect()
}
pub fn restore(
    cp: &Checkpoint,
    mode: ScanMode,
    size: u64,
    records: &[Record],
) -> Result<(Vec<Candidate>, Catalog), String> {
    cp.validate(mode, size)?;
    if records.len() != cp.count || records.len() > MAX_RESULTS {
        return Err("FAT 进度候选计数不一致".into());
    }
    let mut tree = cp.tree()?;
    let mut files = Vec::with_capacity(records.len());
    let mut seen = HashSet::new();
    for (id, record) in records.iter().enumerate() {
        if record.directory > cp.next_dir
            || record.directory >= cp.nodes.len()
            || cp.nodes[record.directory].digest.is_none()
            || record.note.len() > 4096
            || record.note.contains('\0')
        {
            return Err("保存候选父目录或说明无效".into());
        }
        let entry = decode(cp.bits, &record.entries, size)?;
        if entry.directory
            || !mode.includes(entry.deleted)
            || !seen.insert(entry_id(cp.bits, &record.entries, entry.first))
            || (record.supported
                && (entry.valid > entry.size
                    || (cp.bits == 64 && entry.deleted && !entry.contiguous)))
        {
            return Err("保存候选类型、身份或支持状态无效".into());
        }
        let mut c = make_candidate(
            id,
            entry,
            record.entries.clone(),
            cp.boot.unwrap(),
            cp.bits,
            cp.node_id(record.directory),
            if record.supported {
                Ok(())
            } else {
                Err(record.note.clone())
            },
        );
        c.note = record.note.clone();
        if !c
            .note
            .ends_with("保存时的历史观察，提取前重新核对目录项和分配状态")
        {
            c.note
                .push_str("；保存时的历史观察，提取前重新核对目录项和分配状态");
        }
        tree.add_file(&c);
        files.push(c);
    }
    Ok((files, tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_checkpoint::{self as portable, Cursor as ProgressCursor, Progress};
    use crate::signature_checkpoint::Binding;
    use std::io::Cursor;

    fn source() -> crate::data_tasks::Selection {
        crate::data_tasks::Selection {
            target: crate::partitions::Target("a".repeat(64)),
            layout_key: "b".repeat(64),
            partition: Some(2),
        }
    }
    fn fixtures() -> Vec<Vec<u8>> {
        let mut fat16 = vec![0; 512 * 5000];
        fat16[11..13].copy_from_slice(&512u16.to_le_bytes());
        fat16[13] = 1;
        fat16[14] = 1;
        fat16[16] = 1;
        fat16[17] = 16;
        fat16[19..21].copy_from_slice(&5000u16.to_le_bytes());
        fat16[22] = 20;
        fat16[510..512].copy_from_slice(&[0x55, 0xaa]);
        fat16[512..518].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let small = super::super::tests::fixture();
        fat16[21 * 512..21 * 512 + 32].copy_from_slice(&small[1024..1056]);
        fat16[22 * 512..22 * 512 + 5].copy_from_slice(b"hello");
        let mut fat32 = vec![0; 512 * 66070];
        fat32[11..13].copy_from_slice(&512u16.to_le_bytes());
        fat32[13] = 1;
        fat32[14] = 32;
        fat32[16] = 1;
        fat32[32..36].copy_from_slice(&66070u32.to_le_bytes());
        fat32[36..40].copy_from_slice(&513u32.to_le_bytes());
        fat32[44] = 2;
        fat32[510..512].copy_from_slice(&[0x55, 0xaa]);
        for c in 0..4 {
            fat32[32 * 512 + c * 4..32 * 512 + c * 4 + 4]
                .copy_from_slice(&0x0fffffffu32.to_le_bytes());
        }
        fat32[545 * 512..545 * 512 + 32].copy_from_slice(&small[1024..1056]);
        fat32[545 * 512 + 26] = 3;
        fat32[546 * 512..546 * 512 + 5].copy_from_slice(b"hello");
        vec![small, fat16, fat32, super::super::tests::exfat_fixture()]
    }
    fn scan_to(bytes: &[u8], mode: ScanMode, stop: u64) -> (Checkpoint, Vec<Candidate>, Catalog) {
        let mut cp = Checkpoint::default();
        let mut files = vec![];
        let mut tree = Catalog::default();
        let result = continue_scan(
            &mut Cursor::new(bytes),
            mode,
            &mut cp,
            |n, _| {
                if n >= stop {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |c| files.push(c),
            |d| tree.insert(d),
        );
        if !cp.sealed {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
        for c in &files {
            tree.add_file(c);
        }
        (cp, files, tree)
    }
    fn roundtrip(
        bytes: &[u8],
        mode: ScanMode,
        cp: Checkpoint,
        files: &[Candidate],
        tree: &Catalog,
    ) -> Progress {
        let p = Progress::capture(
            source(),
            Binding::capture(&mut Cursor::new(bytes), bytes.len() as u64, 4096, 512).unwrap(),
            ProgressCursor::Fat { mode, cursor: cp },
            files,
            tree,
        )
        .unwrap();
        let p = portable::decode(&portable::encode(p).unwrap(), &source()).unwrap();
        p.verify_cursor(&mut Cursor::new(bytes), bytes.len() as u64)
            .unwrap();
        p
    }
    fn normalized(files: &[Candidate]) -> serde_json::Value {
        let mut v = serde_json::to_value(files).unwrap();
        for c in v.as_array_mut().unwrap() {
            c.as_object_mut().unwrap().remove("note");
        }
        v
    }
    #[test]
    fn all_fat_widths_and_exfat_resume_every_entry_boundary_in_all_modes() {
        for mut bytes in fixtures() {
            let fs = Fs::load(&mut Cursor::new(&bytes)).unwrap();
            let root = fs
                .root_fixed
                .map_or(fs.offset(fs.root).unwrap_or(0) as usize, |r| r.0 as usize);
            let first = root + if fs.bits == 64 { 32 } else { 0 };
            let span = if fs.bits == 64 { 96 } else { 32 };
            let mut deleted = bytes[first..first + span].to_vec();
            if fs.bits == 64 {
                for i in [0, 32, 64] {
                    deleted[i] &= 0x7f;
                }
            } else {
                deleted[0] = 0xe5;
            }
            bytes[first + span..first + 2 * span].copy_from_slice(&deleted);
            for mode in [ScanMode::Deleted, ScanMode::AllFiles, ScanMode::Existing] {
                let (full, expected, expected_tree) = scan_to(&bytes, mode, u64::MAX);
                for stop in 0..full.records {
                    let (cp, files, tree) = scan_to(&bytes, mode, stop);
                    let mut p = roundtrip(&bytes, mode, cp, &files, &tree);
                    let (mut files, mut tree) = p.validate(&source()).unwrap();
                    let ProgressCursor::Fat { cursor, .. } = &mut p.cursor else {
                        panic!()
                    };
                    continue_scan(
                        &mut Cursor::new(&bytes),
                        mode,
                        cursor,
                        |_, _| Ok(()),
                        |c| files.push(c),
                        |d| tree.insert(d),
                    )
                    .unwrap();
                    assert_eq!(normalized(&files), normalized(&expected));
                    assert_eq!(tree.snapshot(), expected_tree.snapshot());
                    cursor
                        .verify(&mut Cursor::new(&bytes), mode, bytes.len() as u64)
                        .unwrap();
                }
            }
        }
    }
    fn fat12_set(bytes: &mut [u8], c: usize, value: u16) {
        let p = 512 + c * 3 / 2;
        let old = u16at(bytes, p);
        let n = if c % 2 == 0 {
            (old & 0xf000) | (value & 0xfff)
        } else {
            (old & 0x000f) | (value << 4)
        };
        bytes[p..p + 2].copy_from_slice(&n.to_le_bytes());
    }
    fn nested_lfn() -> Vec<u8> {
        let mut b = super::super::tests::fixture();
        let short = b[1024..1056].to_vec();
        let mut long = vec![0xff; 32];
        long[0] = 0x41;
        long[11] = 15;
        long[12] = 0;
        long[26] = 0;
        long[27] = 0;
        long[13] = short[..11]
            .iter()
            .fold(0u8, |v, x| v.rotate_right(1).wrapping_add(*x));
        for (p, c) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
            .into_iter()
            .zip("Long name.txt".encode_utf16())
        {
            long[p..p + 2].copy_from_slice(&c.to_le_bytes());
        }
        b[1024..1056].copy_from_slice(&long);
        b[1056..1088].copy_from_slice(&short);
        b[1088..1099].copy_from_slice(b"FOLDER     ");
        b[1099] = 16;
        b[1114] = 3;
        b[1120..1131].copy_from_slice(b"EMPTY      ");
        b[1131] = 16;
        b[1146] = 4;
        fat12_set(&mut b, 3, 0xfff);
        fat12_set(&mut b, 4, 0xfff);
        b[2048..2080].copy_from_slice(&long);
        b[2080..2112].copy_from_slice(&short);
        b
    }
    #[test]
    fn long_names_nested_queue_and_empty_directories_survive_repeated_portable_resume() {
        let b = nested_lfn();
        let (full, expected, tree) = scan_to(&b, ScanMode::AllFiles, u64::MAX);
        assert_eq!(expected.len(), 2);
        assert!(expected.iter().all(|c| c.name == "Long name.txt"));
        assert!(tree.snapshot().iter().any(|d| d.name == "EMPTY"));
        for stop in 0..full.records {
            let (cp, files, tree) = scan_to(&b, ScanMode::AllFiles, stop);
            let mut p = roundtrip(&b, ScanMode::AllFiles, cp, &files, &tree);
            let (mut files, mut tree) = p.validate(&source()).unwrap();
            let ProgressCursor::Fat { cursor, .. } = &mut p.cursor else {
                panic!()
            };
            continue_scan(
                &mut Cursor::new(&b),
                ScanMode::AllFiles,
                cursor,
                |_, _| Ok(()),
                |c| files.push(c),
                |d| tree.insert(d),
            )
            .unwrap();
            for c in &files {
                tree.add_file(c);
            }
            let p = roundtrip(&b, ScanMode::AllFiles, cursor.clone(), &files, &tree);
            let (files, _) = p.validate(&source()).unwrap();
            assert_eq!(normalized(&files), normalized(&expected));
            for c in files {
                let mut out = vec![];
                crate::recovery::extract(&mut Cursor::new(&b), &c, &mut out, u64::MAX, |_| Ok(()))
                    .unwrap();
                assert_eq!(out, b"hello");
            }
        }
    }
    #[test]
    fn fat_directory_metadata_rechecks_entry_boot_and_allocation_after_resume() {
        let bytes = nested_lfn();
        let (cp, files, tree) = scan_to(&bytes, ScanMode::Existing, u64::MAX);
        let p = roundtrip(&bytes, ScanMode::Existing, cp, &files, &tree);
        let (_, restored) = p.validate(&source()).unwrap();
        for d in restored.snapshot().into_iter().filter(|d| d.id != ROOT) {
            let Some(crate::directory_metadata::Proof::Fat(location)) =
                d.metadata.directory_proof.as_ref()
            else {
                panic!()
            };
            let metadata = directory_metadata(&mut Cursor::new(&bytes), location, &d.name).unwrap();
            assert_eq!(metadata.attributes, d.metadata.attributes);
            assert!(metadata.windows_flags.is_some());
            for variant in 0..3 {
                let mut changed = bytes.clone();
                match variant {
                    0 => changed[39] ^= 1,
                    1 => changed[location.entries.last().unwrap().0 as usize] ^= 1,
                    _ => fat12_set(&mut changed, location.first as usize, 0),
                }
                assert!(directory_metadata(&mut Cursor::new(changed), location, &d.name).is_err());
            }
        }
    }
    #[test]
    fn source_directory_boot_bitmap_chain_and_split_group_changes_are_rejected() {
        let b = nested_lfn();
        let (cp, files, tree) = scan_to(&b, ScanMode::AllFiles, 2);
        let p = roundtrip(&b, ScanMode::AllFiles, cp.clone(), &files, &tree);
        for at in [0, 1024, 1056, 1100] {
            let mut bad = b.clone();
            bad[at] ^= 1;
            assert!(p
                .verify_cursor(&mut Cursor::new(bad), b.len() as u64)
                .is_err());
        }
        let mut bad = cp.clone();
        bad.next_slot = 1;
        bad.records = 1;
        assert!(bad
            .verify(&mut Cursor::new(&b), ScanMode::AllFiles, b.len() as u64)
            .is_err());
        let mut bad = cp;
        bad.records += 1;
        assert!(bad
            .verify(&mut Cursor::new(&b), ScanMode::AllFiles, b.len() as u64)
            .is_err());
        let b = super::super::tests::exfat_fixture();
        let (cp, files, tree) = scan_to(&b, ScanMode::AllFiles, 4);
        let p = roundtrip(&b, ScanMode::AllFiles, cp, &files, &tree);
        let mut bad = b.clone();
        bad[512 * 26] ^= 4;
        assert!(p
            .verify_cursor(&mut Cursor::new(bad), b.len() as u64)
            .is_err());
        let mut bad = b.clone();
        bad[24 * 512 + 8..24 * 512 + 12].copy_from_slice(&3u32.to_le_bytes());
        assert!(p
            .verify_cursor(&mut Cursor::new(bad), b.len() as u64)
            .is_err());
    }
    #[test]
    fn forged_records_cannot_supply_sizes_flags_paths_or_unscanned_locations() {
        let b = nested_lfn();
        let (cp, files, tree) = scan_to(&b, ScanMode::AllFiles, 2);
        let p = roundtrip(&b, ScanMode::AllFiles, cp, &files, &tree);
        for field in ["size", "contiguous", "devicePath", "locator"] {
            let mut v = serde_json::to_value(&p).unwrap();
            v["records"]["files"][0][field] = 1.into();
            assert!(serde_json::from_value::<Progress>(v).is_err());
        }
        for edit in 0..6 {
            let mut v = serde_json::to_value(&p).unwrap();
            match edit {
                0 => v["records"]["files"][0]["directory"] = 999.into(),
                1 => v["cursor"]["cursor"]["count"] = 0.into(),
                2 => v["cursor"]["cursor"]["nodes"][0]["parent"] = 0.into(),
                3 => v["records"]["files"][0]["entries"][0][0] = u64::MAX.into(),
                4 => v["records"]["files"][0]["entries"][1][1][28] = 99.into(),
                _ => {
                    v["records"]["files"][0]["entries"][0][0] = 2048.into();
                    v["records"]["files"][0]["entries"][1][0] = 2080.into();
                }
            }
            let bad: Progress = serde_json::from_value(v).unwrap();
            assert!(
                bad.validate(&source()).is_err()
                    || bad
                        .verify_cursor(&mut Cursor::new(&b), b.len() as u64)
                        .is_err()
            );
        }
    }
    #[test]
    fn loaded_candidate_rechecks_reused_directory_and_allocation_before_any_output() {
        for b in fixtures() {
            let (cp, files, tree) = scan_to(&b, ScanMode::AllFiles, u64::MAX);
            let p = roundtrip(&b, ScanMode::AllFiles, cp, &files, &tree);
            let (files, _) = p.validate(&source()).unwrap();
            let c = &files[0];
            let Locator::Fat(l) = &c.locator else {
                panic!()
            };
            let mut output = vec![];
            crate::recovery::extract(&mut Cursor::new(&b), c, &mut output, u64::MAX, |_| Ok(()))
                .unwrap();
            assert_eq!(output, b"hello");
            let mut bad = b.clone();
            bad[l.entries[0].0 as usize] ^= 1;
            output.clear();
            assert!(crate::recovery::extract(
                &mut Cursor::new(bad),
                c,
                &mut output,
                u64::MAX,
                |_| Ok(())
            )
            .is_err());
            assert!(output.is_empty());
            let fs = Fs::load(&mut Cursor::new(&b)).unwrap();
            let mut bad = b.clone();
            if fs.bits == 64 {
                bad[26 * 512] &= !4;
            } else {
                let pos = fs.fat as usize
                    + match fs.bits {
                        12 => l.first as usize * 3 / 2,
                        16 => l.first as usize * 2,
                        _ => l.first as usize * 4,
                    };
                bad[pos] = 0;
                bad[pos + 1] = 0;
            }
            assert!(crate::recovery::extract(
                &mut Cursor::new(bad),
                c,
                &mut output,
                u64::MAX,
                |_| Ok(())
            )
            .is_err());
            assert!(output.is_empty());
        }
    }
    #[test]
    fn entry_groups_cross_fragmented_directory_cluster_boundaries_atomically() {
        let mut all = fixtures();
        let mut fat = all.remove(2);
        let root = 545 * 512;
        let short = fat[root..root + 32].to_vec();
        let long = nested_lfn()[1024..1056].to_vec();
        fat[root..root + 512].fill(0);
        for i in 0..15 {
            fat[root + i * 32] = b'.';
        }
        fat[root + 480..root + 512].copy_from_slice(&long);
        let second = 548 * 512;
        fat[second..second + 32].copy_from_slice(&short);
        fat[32 * 512 + 8..32 * 512 + 12].copy_from_slice(&5u32.to_le_bytes());
        fat[32 * 512 + 20..32 * 512 + 24].copy_from_slice(&0x0fffffffu32.to_le_bytes());
        let mut ex = super::super::tests::exfat_fixture();
        let root = 25 * 512;
        let file = ex[root + 32..root + 128].to_vec();
        ex[root + 32..root + 512].fill(0);
        for i in 1..15 {
            ex[root + i * 32] = 1;
        }
        ex[root + 480..root + 512].copy_from_slice(&file[..32]);
        ex[28 * 512..28 * 512 + 64].copy_from_slice(&file[32..]);
        ex[24 * 512 + 8..24 * 512 + 12].copy_from_slice(&5u32.to_le_bytes());
        ex[24 * 512 + 20..24 * 512 + 24].copy_from_slice(&0xffffffffu32.to_le_bytes());
        ex[26 * 512] |= 8;
        for b in [fat, ex] {
            let (cp, expected, tree) = scan_to(&b, ScanMode::AllFiles, 16);
            assert_eq!(expected.len(), 1);
            assert!(cp.next_slot > 16);
            let mut p = roundtrip(&b, ScanMode::AllFiles, cp, &expected, &tree);
            let (mut files, mut tree) = p.validate(&source()).unwrap();
            let ProgressCursor::Fat { cursor, .. } = &mut p.cursor else {
                panic!()
            };
            continue_scan(
                &mut Cursor::new(&b),
                ScanMode::AllFiles,
                cursor,
                |_, _| Ok(()),
                |c| files.push(c),
                |d| tree.insert(d),
            )
            .unwrap();
            assert_eq!(files.len(), 1);
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&b), &files[0], &mut out, u64::MAX, |_| {
                Ok(())
            })
            .unwrap();
            assert_eq!(out, b"hello");
            cursor
                .verify(&mut Cursor::new(&b), ScanMode::AllFiles, b.len() as u64)
                .unwrap();
        }
    }
}

//! Portable Ext directory checkpoints. Stored names are filesystem-local bytes,
//! never device/output paths. Source membership is proven before publication.
use super::*;
use crate::recovery_tree::Catalog;
use serde::{Deserialize, Serialize};

const MAX_ENTRIES: u64 = 1_000_000;
const MAX_DIRECTORY: u64 = 64 * 1024 * 1024;
const MAX_VISITED: u64 = 256 * 1024 * 1024;

/// Share one already bounded reader with the owned Ext library adapter. No
/// duplicate device opens, unsafe lifetime extension, or arbitrary path input.
pub struct SharedReader<R>(Rc<RefCell<R>>);
impl<R> SharedReader<R> {
    pub fn new(reader: R) -> Self {
        Self(Rc::new(RefCell::new(reader)))
    }
}
impl<R> Clone for SharedReader<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<R: Read> Read for SharedReader<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(b)
    }
}
impl<R: Seek> Seek for SharedReader<R> {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.borrow_mut().seek(p)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Witness {
    name: Vec<u8>,
    inode: u32,
    record: Vec<u8>,
}
impl Witness {
    fn from_metadata(name: Vec<u8>, m: &Metadata) -> Self {
        Self {
            name,
            inode: m.inode_number(),
            record: m.inode_record().to_vec(),
        }
    }
    fn validate(&self, root: bool) -> Result<(), String> {
        if self.inode == 0
            || self.inode > 16_000_000
            || !matches!(self.record.len(), 128 | 256 | 512 | 1024 | 2048 | 4096)
            || (root && (!self.name.is_empty() || self.inode != 2))
            || (!root
                && (self.name.is_empty()
                    || self.name.len() > 255
                    || self.name.contains(&0)
                    || self.name.contains(&b'/')
                    || self.name == b"."
                    || self.name == b".."))
        {
            return Err("Ext 进度名称或 inode 记录无效".into());
        }
        if u32le(&self.record, 0x14) != 0 || u16le(&self.record, 0x1a) == 0 {
            return Err("Ext 进度引用已删除的 inode".into());
        }
        Ok(())
    }
    fn kind(&self) -> u16 {
        u16le(&self.record, 0) & 0xf000
    }
    fn size(&self) -> u64 {
        u32le(&self.record, 4) as u64 | ((u32le(&self.record, 0x6c) as u64) << 32)
    }
    fn unsupported(&self) -> bool {
        u32le(&self.record, 0x20) & (0x4 | 0x800 | 0x100000 | 0x200000 | 0x10000000) != 0
    }
    fn proof(&self, path: Vec<u8>) -> Proof {
        Proof {
            path,
            inode: self.inode,
            record: Sha256::digest(&self.record).into(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Node {
    parent: Option<usize>,
    entry: usize,
    witness: Witness,
    digest: Option<[u8; 32]>,
    entries: usize,
    bytes: u64,
}
impl Node {
    fn new(parent: Option<usize>, entry: usize, witness: Witness) -> Self {
        Self {
            parent,
            entry,
            witness,
            digest: None,
            entries: 0,
            bytes: 0,
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    fingerprint: Option<[u8; 32]>,
    size: u64,
    linux_ids: bool,
    nodes: Vec<Node>,
    next_dir: usize,
    next_entry: usize,
    pub entries: u64,
    pub count: usize,
    special: u64,
    unsupported: usize,
    skipped_directories: u64,
    pub sealed: bool,
}
impl Checkpoint {
    fn id(&self, i: usize) -> String {
        if i == 0 {
            ROOT.into()
        } else {
            format!("ext:{}", self.nodes[i].witness.inode)
        }
    }
    fn ancestors(&self) -> Result<Vec<Arc<Ancestor>>, String> {
        let mut result: Vec<Arc<Ancestor>> = Vec::with_capacity(self.nodes.len());
        for (i, node) in self.nodes.iter().enumerate() {
            node.witness.validate(i == 0)?;
            let (parent, mut path, depth) = if i == 0 {
                if node.parent.is_some() || node.entry != 0 {
                    return Err("Ext 根目录队列无效".into());
                }
                (None, b"/".to_vec(), 1)
            } else {
                let p = node.parent.filter(|p| *p < i).ok_or("Ext 父目录队列无效")?;
                let parent = result[p].clone();
                let mut path = parent.proof.path.clone();
                if path.last() != Some(&b'/') {
                    path.push(b'/');
                }
                let depth = parent.depth + 1;
                (Some(parent), path, depth)
            };
            path.extend_from_slice(&node.witness.name);
            if path.len() > 16384
                || depth > MAX_DEPTH
                || node.witness.kind() != 0x4000
                || node.witness.unsupported()
            {
                return Err("Ext 目录类型、特性、路径或深度不支持".into());
            }
            result.push(Arc::new(Ancestor {
                proof: node.witness.proof(path),
                parent,
                depth,
            }));
        }
        Ok(result)
    }
    fn tree(&self) -> Result<Catalog, String> {
        let mut tree = Catalog::default();
        let parents = self.ancestors()?;
        for (i, node) in self.nodes.iter().enumerate().skip(1) {
            let mut metadata = crate::file_metadata::ext(&node.witness.record, self.linux_ids);
            metadata.directory_proof =
                Some(crate::directory_metadata::Proof::Ext(DirectoryLocation {
                    filesystem: self.fingerprint.ok_or("Ext 卷凭据缺失")?,
                    node: parents[i].clone(),
                }));
            tree.insert(Directory {
                id: self.id(i),
                parent: Some(self.id(node.parent.ok_or("Ext 父目录缺失")?)),
                name: String::from_utf8_lossy(&node.witness.name).into_owned(),
                deleted: false,
                metadata,
            })?;
        }
        Ok(tree)
    }
    pub fn validate(&self, mode: ScanMode, size: u64) -> Result<(), String> {
        if !matches!(mode, ScanMode::AllFiles | ScanMode::Existing) {
            return Err("Ext 当前仅支持现存目录扫描；已删除文件及日志恢复尚未实现".into());
        }
        if self.fingerprint.is_none() {
            if self.size != 0
                || self.linux_ids
                || !self.nodes.is_empty()
                || self.next_dir != 0
                || self.next_entry != 0
                || self.entries != 0
                || self.count != 0
                || self.special != 0
                || self.unsupported != 0
                || self.skipped_directories != 0
                || self.sealed
            {
                return Err("Ext 空进度包含无效状态".into());
            }
            return Ok(());
        }
        if self.size != size
            || size < 2048
            || self.nodes.is_empty()
            || self.nodes.len() > MAX_RESULTS
            || self.next_dir > self.nodes.len()
            || self.entries > MAX_ENTRIES
            || self.count > MAX_RESULTS
            || self.unsupported > self.count
            || self.special > self.entries
            || self.skipped_directories > self.entries
        {
            return Err("Ext 进度范围或计数无效".into());
        }
        self.ancestors()?;
        let mut inodes = HashSet::new();
        let mut entries = 0u64;
        let mut bytes = 0u64;
        for (i, node) in self.nodes.iter().enumerate() {
            if !inodes.insert(node.witness.inode)
                || node.entries as u64 > MAX_ENTRIES
                || node.bytes > MAX_DIRECTORY
                || (node.digest.is_none() && (node.entries != 0 || node.bytes != 0))
                || (i < self.next_dir && node.digest.is_none())
                || (i > self.next_dir && node.digest.is_some())
            {
                return Err("Ext 目录观察状态无效".into());
            }
            bytes = bytes.checked_add(node.bytes).ok_or("Ext 观察大小溢出")?;
            if i < self.next_dir {
                entries += node.entries as u64;
            }
        }
        if let Some(node) = self.nodes.get(self.next_dir) {
            if self.next_entry >= node.entries && (self.next_entry != 0 || node.digest.is_some()) {
                return Err("Ext 续扫目录项边界无效".into());
            }
        } else if self.next_entry != 0 {
            return Err("Ext 已结束游标无效".into());
        }
        entries += self.next_entry as u64;
        if entries != self.entries
            || bytes > MAX_VISITED
            || self.sealed
                != (self.next_dir == self.nodes.len()
                    || self.entries == MAX_ENTRIES
                    || self.count == MAX_RESULTS)
        {
            return Err("Ext 进度计数或结束标记无效".into());
        }
        Ok(())
    }
    pub fn verify<R: Read + Seek + 'static>(
        &self,
        reader: R,
        mode: ScanMode,
        size: u64,
    ) -> Result<(), String> {
        self.verify_records(reader, mode, size, None)
    }
    pub fn verify_records<R: Read + Seek + 'static>(
        &self,
        reader: R,
        mode: ScanMode,
        size: u64,
        records: Option<&[Record]>,
    ) -> Result<(), String> {
        self.validate(mode, size)?;
        if let Some(records) = records {
            restore(self, mode, size, records)?;
        }
        let Some(fingerprint) = self.fingerprint else {
            return Ok(());
        };
        let mut session = Session::new(reader, Arc::new(AtomicBool::new(false)))?;
        if session.geometry.fingerprint != fingerprint
            || (session.geometry.creator_os == 0) != self.linux_ids
        {
            return Err("Ext 文件系统身份或几何已改变".into());
        }
        let parents = self.ancestors()?;
        let root = session.fs.symlink_metadata("/").map_err(err)?;
        session.allocated(&root)?;
        if Witness::from_metadata(vec![], &root) != self.nodes[0].witness {
            return Err("Ext 根目录已改变".into());
        }
        let mut expected_nodes = vec![(None, 0, self.nodes[0].witness.clone())];
        let mut count = 0usize;
        let mut special = 0;
        let mut unsupported = 0;
        let mut skipped = 0;
        let mut seen = HashSet::from([2]);
        for (i, node) in self.nodes.iter().enumerate() {
            let Some(digest) = node.digest else {
                continue;
            };
            let observed = observe(&mut session, &parents[i])?;
            if observed.digest != digest
                || observed.entries.len() != node.entries
                || observed.bytes != node.bytes
            {
                return Err("Ext 已访问目录的名称、inode 或顺序已改变".into());
            }
            let prefix = if i < self.next_dir {
                node.entries
            } else {
                self.next_entry
            };
            for (slot, entry) in observed.entries.iter().take(prefix).enumerate() {
                if entry.dot() {
                    continue;
                }
                match entry.witness.kind() {
                    0x4000 if entry.witness.unsupported() => skipped += 1,
                    0x4000 => {
                        if !seen.insert(entry.witness.inode) {
                            return Err("Ext 目录重复引用".into());
                        }
                        expected_nodes.push((Some(i), slot, entry.witness.clone()));
                    }
                    0x8000 => {
                        if let Some(records) = records {
                            let expected = Record {
                                directory: i,
                                entry: slot,
                                witness: entry.witness.clone(),
                                supported: entry.supported,
                            };
                            if records.get(count) != Some(&expected) {
                                return Err("Ext 候选不属于已扫描的原目录项".into());
                            }
                        }
                        count += 1;
                        unsupported += usize::from(!entry.supported);
                    }
                    _ => special += 1,
                }
            }
        }
        if count != self.count
            || unsupported != self.unsupported
            || special != self.special
            || skipped != self.skipped_directories
            || expected_nodes.len() != self.nodes.len()
            || expected_nodes
                .iter()
                .zip(&self.nodes)
                .any(|((p, e, w), n)| *p != n.parent || *e != n.entry || w != &n.witness)
        {
            return Err("Ext 目录队列或候选计数与实际已扫描范围不一致".into());
        }
        Ok(())
    }
}
struct Entry {
    witness: Witness,
    supported: bool,
}
impl Entry {
    fn dot(&self) -> bool {
        self.witness.name == b"." || self.witness.name == b".."
    }
}
struct Observed {
    entries: Vec<Entry>,
    digest: [u8; 32],
    bytes: u64,
}
fn observe<R: Read + Seek + 'static>(
    session: &mut Session<R>,
    parent: &Ancestor,
) -> Result<Observed, String> {
    session.prove(&parent.proof)?;
    let metadata = session
        .fs
        .symlink_metadata(parent.proof.path.as_slice())
        .map_err(err)?;
    if !metadata.is_dir() || metadata.len() > MAX_DIRECTORY {
        return Err("Ext 目录大小超过 64 MiB 上限".into());
    }
    let iter = session
        .fs
        .read_dir(parent.proof.path.as_slice())
        .map_err(err)?;
    let mut entries = Vec::new();
    let mut names = HashSet::new();
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    for entry in iter {
        let entry = entry.map_err(err)?;
        let name = entry.file_name().as_ref().to_vec();
        if !names.insert(name.clone()) {
            return Err("Ext 同一目录出现重复名称".into());
        }
        let metadata = entry.metadata().map_err(err)?;
        session.allocated(&metadata)?;
        let witness = Witness::from_metadata(name, &metadata);
        bytes += witness.name.len() as u64 + witness.record.len() as u64 + 12;
        if bytes > MAX_DIRECTORY || entries.len() as u64 >= MAX_ENTRIES {
            return Err("Ext 目录观察超过支持范围".into());
        }
        let dot = witness.name == b"." || witness.name == b"..";
        if !dot {
            witness.validate(false)?;
        }
        let supported = !dot
            && metadata.file_type().is_regular_file()
            && !witness.unsupported()
            && match session.fs.open(&entry.path()) {
                Ok(file) => file.metadata() == &metadata,
                Err(ext4_view::Ext4Error::Incompatible(_)) => false,
                Err(e) => return Err(err(e)),
            };
        hash.update((witness.name.len() as u32).to_le_bytes());
        hash.update(&witness.name);
        hash.update(witness.inode.to_le_bytes());
        hash.update((witness.record.len() as u32).to_le_bytes());
        hash.update(&witness.record);
        entries.push(Entry { witness, supported });
    }
    Ok(Observed {
        entries,
        digest: hash.finalize().into(),
        bytes,
    })
}

pub fn continue_scan<R: Read + Seek + 'static>(
    reader: R,
    mode: ScanMode,
    cp: &mut Checkpoint,
    cancel: Arc<AtomicBool>,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    let mut shared = SharedReader::new(reader);
    let size = shared.seek(SeekFrom::End(0)).map_err(err)?;
    cp.validate(mode, size)?;
    if cp.fingerprint.is_some() {
        cp.verify(shared.clone(), mode, size)?;
        if cp.sealed {
            return Err("Ext 扫描已经结束".into());
        }
    }
    let mut session = Session::new(shared, cancel.clone())?;
    if cp.fingerprint.is_none() {
        let root = session.fs.symlink_metadata("/").map_err(err)?;
        session.allocated(&root)?;
        *cp = Checkpoint {
            fingerprint: Some(session.geometry.fingerprint),
            size,
            linux_ids: session.geometry.creator_os == 0,
            nodes: vec![Node::new(None, 0, Witness::from_metadata(vec![], &root))],
            ..Default::default()
        };
    }
    let mut parents = cp.ancestors()?;
    let mut seen: HashSet<_> = cp.nodes.iter().map(|n| n.witness.inode).collect();
    let mut observed_bytes: u64 = cp.nodes.iter().map(|n| n.bytes).sum();
    while cp.next_dir < cp.nodes.len() {
        tick(cp.entries, 0)?;
        let i = cp.next_dir;
        let observed = observe(&mut session, &parents[i])?;
        let node = &cp.nodes[i];
        if node.digest.is_some_and(|d| d != observed.digest) {
            return Err("Ext 当前目录在扫描期间变化".into());
        }
        if node.digest.is_none() && observed_bytes + observed.bytes > MAX_VISITED {
            return Err("Ext 目录观察超过 256 MiB 上限".into());
        }
        // Observation and record publication commit together. Cancellation during
        // a directory read leaves the prior complete entry checkpoint intact.
        loop {
            tick(cp.entries, 0)?;
            if cancel.load(Ordering::Relaxed) {
                return Err("Ext 扫描已暂停或取消".into());
            }
            let node = &mut cp.nodes[i];
            if node.digest.is_none() {
                node.digest = Some(observed.digest);
                node.entries = observed.entries.len();
                node.bytes = observed.bytes;
                observed_bytes += observed.bytes;
            }
            let Some(entry) = observed.entries.get(cp.next_entry) else {
                cp.next_dir += 1;
                cp.next_entry = 0;
                break;
            };
            if !entry.dot() {
                match entry.witness.kind() {
                    0x4000 if entry.witness.unsupported() => cp.skipped_directories += 1,
                    0x4000 => {
                        if !seen.insert(entry.witness.inode)
                            || parents[i].depth >= MAX_DEPTH
                            || cp.nodes.len() >= MAX_RESULTS
                        {
                            return Err("Ext 目录循环、重复引用、过深或数量过多".into());
                        }
                        let id = format!("ext:{}", entry.witness.inode);
                        let mut path = parents[i].proof.path.clone();
                        if path.last() != Some(&b'/') {
                            path.push(b'/');
                        }
                        path.extend_from_slice(&entry.witness.name);
                        if path.len() > 16384 {
                            return Err("Ext 路径超过 16 KiB 上限".into());
                        }
                        parents.push(Arc::new(Ancestor {
                            proof: entry.witness.proof(path),
                            parent: Some(parents[i].clone()),
                            depth: parents[i].depth + 1,
                        }));
                        let mut metadata =
                            crate::file_metadata::ext(&entry.witness.record, cp.linux_ids);
                        metadata.directory_proof =
                            Some(crate::directory_metadata::Proof::Ext(DirectoryLocation {
                                filesystem: cp.fingerprint.ok_or("Ext 卷凭据缺失")?,
                                node: parents.last().ok_or("Ext 目录凭据缺失")?.clone(),
                            }));
                        directory(Directory {
                            id,
                            parent: Some(cp.id(i)),
                            name: String::from_utf8_lossy(&entry.witness.name).into_owned(),
                            deleted: false,
                            metadata,
                        })?;
                        cp.nodes
                            .push(Node::new(Some(i), cp.next_entry, entry.witness.clone()));
                    }
                    0x8000 => {
                        let record = Record {
                            directory: i,
                            entry: cp.next_entry,
                            witness: entry.witness.clone(),
                            supported: entry.supported,
                        };
                        found(candidate(cp, &parents, cp.count, &record, false)?);
                        cp.count += 1;
                        cp.unsupported += usize::from(!entry.supported);
                    }
                    _ => cp.special += 1,
                }
            }
            cp.next_entry += 1;
            cp.entries += 1;
            if cp.next_entry == observed.entries.len() {
                cp.next_dir += 1;
                cp.next_entry = 0;
            }
            cp.sealed = cp.next_dir == cp.nodes.len()
                || cp.count == MAX_RESULTS
                || cp.entries == MAX_ENTRIES;
            if cp.sealed || cp.next_dir != i {
                break;
            }
        }
        if cp.sealed {
            break;
        }
    }
    cp.sealed = true;
    tick(cp.entries, cp.entries)?;
    Ok(format!("Ext 扫描完成：{} 个现存文件，{} 个暂不支持提取，跳过 {} 个符号链接或特殊文件、{} 个不支持的目录。{}未执行删除恢复或日志重放。",cp.count,cp.unsupported,cp.special,cp.skipped_directories,if cp.count == MAX_RESULTS || cp.entries == MAX_ENTRIES { "已达到结果或目录项上限；" } else { "" }))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    directory: usize,
    entry: usize,
    witness: Witness,
    supported: bool,
}
fn candidate(
    cp: &Checkpoint,
    parents: &[Arc<Ancestor>],
    id: usize,
    r: &Record,
    historical: bool,
) -> Result<Candidate, String> {
    r.witness.validate(false)?;
    let parent = parents
        .get(r.directory)
        .ok_or("Ext 候选父目录缺失")?
        .clone();
    if r.witness.kind() != 0x8000 || (r.supported && r.witness.unsupported()) {
        return Err("Ext 候选文件类型或支持状态无效".into());
    }
    let mut path = parent.proof.path.clone();
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(&r.witness.name);
    if path.len() > 16384 {
        return Err("Ext 文件路径过长".into());
    }
    let mut note = if r.supported {
        "现存文件；支持稀疏洞读取，不重建 POSIX 权限、链接或扩展属性"
    } else {
        "此 Ext 文件的存储特性暂不支持提取"
    }
    .to_string();
    if historical {
        note.push_str("；保存时的历史观察，提取前重新核对 inode 和分配状态");
    }
    Ok(Candidate {
        id: id as u32,
        name: String::from_utf8_lossy(&r.witness.name).into_owned(),
        size: r.witness.size(),
        deleted: false,
        method: "Ext 目录".into(),
        supported: r.supported,
        note,
        directory: Some(cp.id(r.directory)),
        directory_note: if std::str::from_utf8(&r.witness.name).is_err() {
            "原名称含非 UTF-8 字节，界面使用替代字符；提取仍按原始字节定位".into()
        } else {
            String::new()
        },
        metadata: crate::file_metadata::ext(&r.witness.record, cp.linux_ids),
        locator: Locator::Ext(Location {
            filesystem: cp.fingerprint.ok_or("Ext 文件系统身份缺失")?,
            proof: r.witness.proof(path),
            parent,
            record: r.witness.record.clone(),
            entry: r.entry,
        }),
    })
}
pub fn capture(
    cp: &Checkpoint,
    files: &[Candidate],
    tree: &Catalog,
) -> Result<Vec<Record>, String> {
    if cp.tree()?.snapshot() != tree.snapshot() {
        return Err("Ext 保存目录树与队列不一致".into());
    }
    let ids: HashMap<_, _> = (0..cp.nodes.len()).map(|i| (cp.id(i), i)).collect();
    files
        .iter()
        .map(|c| {
            let Locator::Ext(l) = &c.locator else {
                return Err("不是 Ext 候选".into());
            };
            if Some(l.filesystem) != cp.fingerprint
                || Sha256::digest(&l.record).as_slice() != l.proof.record
            {
                return Err("Ext 候选身份不一致".into());
            }
            let directory = *ids
                .get(c.directory.as_deref().unwrap_or(""))
                .ok_or("Ext 候选父目录不在队列")?;
            let name = l
                .proof
                .path
                .rsplit(|b| *b == b'/')
                .next()
                .ok_or("Ext 文件名缺失")?
                .to_vec();
            Ok(Record {
                directory,
                entry: l.entry,
                witness: Witness {
                    name,
                    inode: l.proof.inode,
                    record: l.record.clone(),
                },
                supported: c.supported,
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
        return Err("Ext 候选计数不一致".into());
    }
    let parents = cp.ancestors()?;
    let mut tree = cp.tree()?;
    let mut files = Vec::with_capacity(records.len());
    let mut previous = None;
    for (id, r) in records.iter().enumerate() {
        let node = cp.nodes.get(r.directory).ok_or("Ext 候选父目录越界")?;
        let bound = if r.directory < cp.next_dir {
            node.entries
        } else if r.directory == cp.next_dir {
            cp.next_entry
        } else {
            0
        };
        let key = (r.directory, r.entry);
        if node.digest.is_none() || r.entry >= bound || previous.is_some_and(|p| p >= key) {
            return Err("Ext 候选不在已扫描目录前缀或重复/乱序".into());
        }
        let c = candidate(cp, &parents, id, r, true)?;
        tree.add_file(&c);
        files.push(c);
        previous = Some(key);
    }
    Ok((files, tree))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, inode_at, root_name_at};
    use super::*;
    use crate::recovery_checkpoint::{
        self as portable, Cursor as ProgressCursor, Progress, Records,
    };
    use crate::signature_checkpoint::Binding;
    use std::io::Cursor;

    fn source() -> crate::data_tasks::Selection {
        crate::data_tasks::Selection {
            target: crate::partitions::Target("a".repeat(64)),
            layout_key: "b".repeat(64),
            partition: Some(2),
        }
    }
    fn scan_to(
        bytes: Arc<[u8]>,
        mode: ScanMode,
        stop: u64,
    ) -> (Checkpoint, Vec<Candidate>, Catalog) {
        let mut cp = Checkpoint::default();
        let mut files = vec![];
        let mut tree = Catalog::default();
        let result = continue_scan(
            Cursor::new(bytes.clone()),
            mode,
            &mut cp,
            Arc::new(AtomicBool::new(false)),
            |done, _| {
                if done >= stop {
                    Err("test-pause".into())
                } else {
                    Ok(())
                }
            },
            |c| files.push(c),
            |d| tree.insert(d),
        );
        if stop == u64::MAX {
            result.unwrap();
            assert!(cp.sealed);
        } else {
            assert_eq!(result.unwrap_err(), "test-pause");
            assert!(!cp.sealed);
            assert_eq!(cp.entries, stop);
        }
        for c in &files {
            tree.add_file(c);
        }
        cp.verify(Cursor::new(bytes.clone()), mode, bytes.len() as u64)
            .unwrap();
        (cp, files, tree)
    }
    fn progress(
        bytes: Arc<[u8]>,
        mode: ScanMode,
        cp: Checkpoint,
        files: &[Candidate],
        tree: &Catalog,
    ) -> Progress {
        let binding = Binding::capture(
            &mut Cursor::new(bytes.clone()),
            bytes.len() as u64,
            4096,
            512,
        )
        .unwrap();
        let p = Progress::capture(
            source(),
            binding,
            ProgressCursor::Ext { mode, cursor: cp },
            files,
            tree,
        )
        .unwrap();
        let loaded = portable::decode(&portable::encode(p).unwrap(), &source()).unwrap();
        loaded
            .binding
            .verify(
                &mut Cursor::new(bytes.clone()),
                bytes.len() as u64,
                4096,
                512,
            )
            .unwrap();
        loaded
            .verify_owned(Cursor::new(bytes.clone()), bytes.len() as u64)
            .unwrap();
        loaded
    }
    fn normalized(files: &[Candidate]) -> serde_json::Value {
        let mut v = serde_json::to_value(files).unwrap();
        for c in v.as_array_mut().unwrap() {
            c.as_object_mut().unwrap().remove("note");
        }
        v
    }
    fn finish(bytes: Arc<[u8]>, mode: ScanMode, p: &mut Progress) -> (Vec<Candidate>, Catalog) {
        let (mut files, mut tree) = p.validate(&source()).unwrap();
        let mut added = vec![];
        let ProgressCursor::Ext { cursor, .. } = &mut p.cursor else {
            panic!()
        };
        continue_scan(
            Cursor::new(bytes.clone()),
            mode,
            cursor,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |c| added.push(c),
            |d| tree.insert(d),
        )
        .unwrap();
        for c in added {
            tree.add_file(&c);
            files.push(c);
        }
        cursor
            .verify_records(
                Cursor::new(bytes.clone()),
                mode,
                bytes.len() as u64,
                Some(&capture(cursor, &files, &tree).unwrap()),
            )
            .unwrap();
        (files, tree)
    }
    #[test]
    fn ext2_ext3_ext4_partial_portable_resume_matches_full_scan_and_known_contents() {
        for kind in 0..3 {
            let bytes = fixture(kind);
            for mode in [ScanMode::AllFiles, ScanMode::Existing] {
                let (base, expected, expected_tree) = scan_to(bytes.clone(), mode, u64::MAX);
                let mut stops = vec![
                    0,
                    1,
                    2,
                    base.nodes[0].entries as u64,
                    base.entries / 2,
                    base.entries - 1,
                ];
                stops.sort_unstable();
                stops.dedup();
                for stop in stops.into_iter().filter(|n| *n < base.entries) {
                    let (cp, files, tree) = scan_to(bytes.clone(), mode, stop);
                    let mut p = progress(bytes.clone(), mode, cp, &files, &tree);
                    let (files, tree) = finish(bytes.clone(), mode, &mut p);
                    assert_eq!(
                        normalized(&files),
                        normalized(&expected),
                        "kind={kind}, mode={mode:?}, stop={stop}"
                    );
                    assert_eq!(tree.snapshot(), expected_tree.snapshot());
                    assert_eq!(
                        serde_json::to_value(tree.folders(None, 0).unwrap()).unwrap(),
                        serde_json::to_value(expected_tree.folders(None, 0).unwrap()).unwrap()
                    );
                    let c = files
                        .iter()
                        .find(|c| c.name == if kind == 1 { "42" } else { "small_file" })
                        .unwrap();
                    let mut out = vec![];
                    crate::recovery::extract_owned(
                        Cursor::new(bytes.clone()),
                        c,
                        &mut out,
                        u64::MAX,
                        Arc::new(AtomicBool::new(false)),
                        |_| Ok(()),
                    )
                    .unwrap();
                    assert_eq!(
                        out,
                        if kind == 1 {
                            b"42".as_slice()
                        } else {
                            b"hello, world!".as_slice()
                        }
                    );
                }
            }
        }
    }
    #[test]
    fn repeated_checkpoints_preserve_aliases_raw_names_and_empty_directories() {
        let mut bytes = fixture(0).to_vec();
        let at = root_name_at(&bytes, b"small_file");
        bytes[at] = 0xff;
        let bytes: Arc<[u8]> = bytes.into();
        let mode = ScanMode::AllFiles;
        let (base, expected, expected_tree) = scan_to(bytes.clone(), mode, u64::MAX);
        let (cp, files, tree) = scan_to(bytes.clone(), mode, 0);
        let mut p = progress(bytes.clone(), mode, cp, &files, &tree);
        while !p.cursor.sealed() {
            let (mut files, mut tree) = p.validate(&source()).unwrap();
            let mut added = vec![];
            let stop = p.cursor.done() + 1;
            let ProgressCursor::Ext { cursor, .. } = &mut p.cursor else {
                panic!()
            };
            let result = continue_scan(
                Cursor::new(bytes.clone()),
                mode,
                cursor,
                Arc::new(AtomicBool::new(false)),
                |done, _| {
                    if done >= stop {
                        Err("pause".into())
                    } else {
                        Ok(())
                    }
                },
                |c| added.push(c),
                |d| tree.insert(d),
            );
            if let Err(e) = result {
                assert_eq!(e, "pause");
            }
            for c in added {
                tree.add_file(&c);
                files.push(c);
            }
            p = progress(bytes.clone(), mode, cursor.clone(), &files, &tree);
        }
        assert_eq!(p.cursor.done(), base.entries);
        let (files, tree) = p.validate(&source()).unwrap();
        assert_eq!(normalized(&files), normalized(&expected));
        assert_eq!(tree.snapshot(), expected_tree.snapshot());
        let c = files.iter().find(|c| c.name == "�mall_file").unwrap();
        let mut out = vec![];
        crate::recovery::extract_owned(
            Cursor::new(bytes),
            c,
            &mut out,
            u64::MAX,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(out, b"hello, world!");
        let bytes = fixture(2);
        let (cp, files, tree) = scan_to(bytes.clone(), mode, u64::MAX);
        let p = progress(bytes, mode, cp, &files, &tree);
        let (_, tree) = p.validate(&source()).unwrap();
        assert!(tree.snapshot().iter().any(|d| d.name == "empty_dir"));
    }
    #[test]
    fn source_changes_are_rejected_before_loaded_results_or_file_output() {
        let bytes = fixture(0);
        let mode = ScanMode::AllFiles;
        let (cp, files, tree) = scan_to(bytes.clone(), mode, u64::MAX);
        let p = progress(bytes.clone(), mode, cp, &files, &tree);
        let (loaded, _) = p.validate(&source()).unwrap();
        let c = loaded.iter().find(|c| c.name == "small_file").unwrap();
        let Locator::Ext(l) = &c.locator else {
            panic!()
        };
        for variant in 0..5 {
            let mut changed = bytes.to_vec();
            match variant {
                0 => changed[root_name_at(&bytes, b"small_file")] = b'X',
                1 => changed[inode_at(&bytes, l.proof.inode) + 0x64] ^= 1,
                2 => changed[inode_at(&bytes, 2) + 0x64] ^= 1,
                3 => changed[1024 + 0x68] ^= 1,
                _ => {
                    let bitmap = u32le(&bytes, 2052) as usize * 1024;
                    let bit = l.proof.inode - 1;
                    changed[bitmap + bit as usize / 8] &= !(1 << (bit % 8));
                }
            }
            let changed: Arc<[u8]> = changed.into();
            assert!(
                p.verify_owned(Cursor::new(changed.clone()), changed.len() as u64)
                    .is_err(),
                "variant={variant}"
            );
            let mut out = vec![];
            assert!(crate::recovery::extract_owned(
                Cursor::new(changed),
                c,
                &mut out,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(())
            )
            .is_err());
            assert!(out.is_empty());
        }
    }
    #[test]
    fn ext_directory_metadata_revalidates_inode_ancestors_and_volume_after_resume() {
        for kind in 0..3 {
            let bytes = fixture(kind);
            let (cp, files, tree) = scan_to(bytes.clone(), ScanMode::Existing, u64::MAX);
            let p = progress(bytes.clone(), ScanMode::Existing, cp, &files, &tree);
            let (_, restored) = p.validate(&source()).unwrap();
            let d = restored
                .snapshot()
                .into_iter()
                .find(|d| d.id != ROOT)
                .unwrap();
            let Some(crate::directory_metadata::Proof::Ext(location)) =
                d.metadata.directory_proof.as_ref()
            else {
                panic!()
            };
            let live = || Arc::new(AtomicBool::new(false));
            let metadata =
                directory_metadata(Cursor::new(bytes.clone()), location, &d.name, live()).unwrap();
            assert_eq!(metadata.permissions, d.metadata.permissions);
            assert!(metadata
                .modified
                .as_ref()
                .and_then(|s| s.instant_ns)
                .is_some());
            for offset in [
                1024 + 0x68,
                inode_at(&bytes, location.node.proof.inode) + 0x64,
                inode_at(&bytes, 2) + 0x64,
            ] {
                let mut changed = bytes.to_vec();
                changed[offset] ^= 1;
                assert!(
                    directory_metadata(Cursor::new(changed), location, &d.name, live()).is_err()
                );
            }
        }
    }
    #[test]
    fn forged_candidates_queue_counters_scope_and_unscanned_records_fail_closed() {
        let bytes = fixture(0);
        let mode = ScanMode::AllFiles;
        let (cp, files, tree) = scan_to(bytes.clone(), mode, u64::MAX);
        let p = progress(bytes.clone(), mode, cp, &files, &tree);
        for variant in 0..9 {
            let mut forged = p.clone();
            let ProgressCursor::Ext { cursor, .. } = &mut forged.cursor else {
                panic!()
            };
            let Records::Ext { files } = &mut forged.records else {
                panic!()
            };
            match variant {
                0 => files[0].witness.name = b"../escape".to_vec(),
                1 => files[0].witness.record[4] ^= 1,
                2 => files[0].witness.inode += 1,
                3 => files[0].entry = usize::MAX,
                4 => cursor.entries += 1,
                5 => cursor.nodes[0].parent = Some(0),
                6 => cursor.nodes[0].bytes = u64::MAX,
                7 => {
                    cursor.next_dir = 0;
                    cursor.next_entry = 0;
                    cursor.entries = 0;
                    cursor.sealed = false;
                }
                _ => files[0].supported = !files[0].supported,
            }
            assert!(
                forged.validate(&source()).is_err()
                    || forged
                        .verify_owned(Cursor::new(bytes.clone()), bytes.len() as u64)
                        .is_err(),
                "variant={variant}"
            );
        }
        let mut wrong = source();
        wrong.partition = Some(3);
        assert!(portable::decode(&portable::encode(p.clone()).unwrap(), &wrong).is_err());
        let mut value = serde_json::to_value(&p).unwrap();
        value["records"]["files"][0]["locator"] = serde_json::json!({"path":"arbitrary"});
        assert!(serde_json::from_value::<Progress>(value).is_err());
    }
    #[test]
    fn cancellation_after_candidate_commits_a_resumable_complete_record() {
        let bytes = fixture(0);
        let mode = ScanMode::AllFiles;
        let (_, expected, expected_tree) = scan_to(bytes.clone(), mode, u64::MAX);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cp = Checkpoint::default();
        let mut files = vec![];
        let mut tree = Catalog::default();
        assert!(continue_scan(
            Cursor::new(bytes.clone()),
            mode,
            &mut cp,
            cancel.clone(),
            |_, _| Ok(()),
            |c| {
                files.push(c);
                if files.len() == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            |d| tree.insert(d)
        )
        .is_err());
        assert_eq!(cp.count, 2);
        for c in &files {
            tree.add_file(c);
        }
        let mut p = progress(bytes.clone(), mode, cp, &files, &tree);
        let (files, tree) = finish(bytes, mode, &mut p);
        assert_eq!(normalized(&files), normalized(&expected));
        assert_eq!(tree.snapshot(), expected_tree.snapshot());
    }
}

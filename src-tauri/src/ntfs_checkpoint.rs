//! Persisted NTFS candidates. Never deserialize an internal extraction locator
//! or trust a saved name as an output path. Extraction rechecks actual records.
use crate::{
    file_metadata::Metadata,
    ntfs_names::Name,
    ntfs_scan::Checkpoint,
    recovery::{Candidate, Locator, ScanMode, MAX_RESULTS},
    recovery_tree::{Catalog, Directory, ROOT},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    record: u64,
    sequence: u16,
    link: Name,
    compression: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wof: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<crate::ntfs_streams::Identity>,
    size: u64,
    deleted: bool,
    supported: bool,
    note: String,
    directory: Option<String>,
    directory_note: String,
    metadata: Metadata,
}
impl Record {
    pub fn capture(c: &Candidate) -> Result<Self, String> {
        let Locator::Ntfs {
            record,
            sequence,
            link,
            compression,
            wof,
            stream,
        } = &c.locator
        else {
            return Err("不是 NTFS 候选".into());
        };
        Ok(Self {
            record: *record,
            sequence: *sequence,
            link: link.clone(),
            compression: *compression,
            wof: *wof,
            stream: stream.clone(),
            size: c.size,
            deleted: c.deleted,
            supported: c.supported,
            note: c.note.clone(),
            directory: c.directory.clone(),
            directory_note: c.directory_note.clone(),
            metadata: c.metadata.clone(),
        })
    }
    fn candidate(
        &self,
        id: u32,
        cursor: &Checkpoint,
        mode: ScanMode,
        tree: &Catalog,
    ) -> Result<Candidate, String> {
        self.link.validate()?;
        if let Some(stream) = &self.stream {
            stream.validate()?;
            if self.compression.is_some() {
                return Err("数据流保存身份冲突".into());
            }
        }
        if self.wof.is_some() && (self.compression.is_some() || self.stream.is_some()) {
            return Err("NTFS WOF 保存身份冲突。".into());
        }
        self.metadata.validate_snapshot()?;
        if self.record >= cursor.next
            || self.size > 1 << 43
            || !mode.includes(self.deleted)
            || self.note.len() > 4096
            || self.directory_note.len() > 4096
            || self.note.contains('\0')
            || self.directory_note.contains('\0')
        {
            return Err("NTFS 保存候选越界或模式不匹配".into());
        }
        if let Some(id) = &self.directory {
            let expected = if self.link.parent.0 == 5 {
                ROOT.into()
            } else {
                format!("ntfs-{}-{}", self.link.parent.0, self.link.parent.1)
            };
            if id != &expected {
                return Err("NTFS 保存候选与父目录身份不一致".into());
            }
            tree.path(id)?;
        }
        Ok(Candidate {
            id,
            name: self
                .stream
                .as_ref()
                .map_or_else(|| self.link.display(), |s| s.display(&self.link.display())),
            size: self.size,
            deleted: self.deleted,
            method: if self.stream.is_some() {
                "NTFS 命名数据流"
            } else if self.wof.is_some() {
                "NTFS WOF 压缩文件"
            } else {
                "NTFS 文件表"
            }
            .into(),
            supported: self.supported,
            note: if self
                .note
                .ends_with("保存时的历史观察，提取前重新核对记录和属性")
            {
                self.note.clone()
            } else {
                format!("{}；保存时的历史观察，提取前重新核对记录和属性", self.note)
            },
            directory: self.directory.clone(),
            directory_note: self.directory_note.clone(),
            metadata: self.metadata.clone(),
            locator: Locator::Ntfs {
                record: self.record,
                sequence: self.sequence,
                link: self.link.clone(),
                compression: self.compression,
                wof: self.wof,
                stream: self.stream.clone(),
            },
        })
    }
}
fn identity(id: &str, total: u64) -> bool {
    if id == ROOT {
        return true;
    }
    let Some(rest) = id.strip_prefix("ntfs-") else {
        return false;
    };
    let Some((a, b)) = rest.split_once('-') else {
        return false;
    };
    matches!((a.parse::<u64>(),b.parse::<u16>()),(Ok(n),Ok(s)) if n<total&&n!=5&&id==format!("ntfs-{n}-{s}"))
}
pub fn restore(
    records: &[Record],
    directories: &[Directory],
    cursor: &Checkpoint,
    mode: ScanMode,
) -> Result<(Vec<Candidate>, Catalog), String> {
    cursor.validate(mode)?;
    if records.len() != cursor.count
        || records.len() > MAX_RESULTS
        || directories.len() > MAX_RESULTS
    {
        return Err("NTFS 进度结果数量不一致".into());
    }
    let mut tree = Catalog::default();
    let mut nodes = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for d in directories {
        if !identity(&d.id, cursor.total) || nodes.insert(d.id.clone(), d.clone()).is_some() {
            return Err("NTFS 保存目录身份无效或重复".into());
        }
        if d.id == ROOT {
            if tree.snapshot()[0] != *d {
                return Err("NTFS 保存根目录无效".into());
            }
        } else {
            let parent = d.parent.as_ref().ok_or("保存目录缺少父节点")?;
            if !identity(parent, cursor.total) {
                return Err("保存父目录身份无效".into());
            }
            children
                .entry(parent.clone())
                .or_default()
                .push(d.id.clone());
        }
    }
    if !nodes.contains_key(ROOT) {
        return Err("NTFS 进度缺少根目录".into());
    }
    let mut visited = 1;
    let mut queue = VecDeque::from([ROOT.to_owned()]);
    while let Some(parent) = queue.pop_front() {
        for id in children.remove(&parent).unwrap_or_default() {
            let d = nodes.get(&id).ok_or("保存目录缺失")?.clone();
            tree.insert(d)?;
            visited += 1;
            queue.push_back(id);
        }
    }
    if visited != nodes.len() {
        return Err("NTFS 保存目录链缺失或循环".into());
    }
    let mut seen = HashSet::new();
    let mut files = Vec::with_capacity(records.len());
    for (id, r) in records.iter().enumerate() {
        if !seen.insert((
            r.record,
            r.sequence,
            r.link.clone(),
            r.stream.as_ref().map(|s| s.name.clone()),
        )) {
            return Err("NTFS 保存候选身份重复".into());
        }
        let c = r.candidate(id as u32, cursor, mode, &tree)?;
        tree.add_file(&c);
        files.push(c);
    }
    Ok((files, tree))
}

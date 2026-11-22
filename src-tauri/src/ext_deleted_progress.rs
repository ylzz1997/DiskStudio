//! Portable Ext4 deletion progress. Saved rows carry proofs, never disk extents.
use super::*;
use crate::recovery_tree::Catalog;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceIdentity {
    filesystem: [u8; 32],
    journal: [u8; 32],
    allocation: [u8; 32],
    limited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectoryRecord {
    inode: u32,
    name: Vec<u8>,
    record: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NameRecord {
    bytes: Vec<u8>,
    ancestors: Vec<DirectoryRecord>,
}

impl NameRecord {
    fn capture(name: &Named) -> Self {
        let mut next = Some(name.parent.as_ref());
        let mut ancestors = Vec::new();
        while let Some(node) = next {
            ancestors.push(DirectoryRecord {
                inode: node.proof.inode,
                name: node
                    .proof
                    .path
                    .rsplit(|b| *b == b'/')
                    .next()
                    .unwrap()
                    .to_vec(),
                record: node.proof.record,
            });
            next = node.parent.as_deref();
        }
        ancestors.reverse();
        Self {
            bytes: name.bytes.clone(),
            ancestors,
        }
    }

    fn restore(&self, filesystem: [u8; 32]) -> Result<(Named, Vec<Directory>), String> {
        let invalid = "Ext4 日志进度的原始名称或目录无效。";
        let name_valid = |name: &[u8]| {
            !name.is_empty()
                && name.len() <= 255
                && !name.contains(&0)
                && !name.contains(&b'/')
                && name != b"."
                && name != b".."
        };
        if !name_valid(&self.bytes) || self.ancestors.is_empty() || self.ancestors.len() > MAX_DEPTH
        {
            return Err(invalid.into());
        }
        let mut parent: Option<Arc<Ancestor>> = None;
        let mut path = vec![b'/'];
        let mut seen = HashSet::new();
        let mut directories = Vec::new();
        for (index, witness) in self.ancestors.iter().enumerate() {
            if witness.inode == 0
                || !seen.insert(witness.inode)
                || (index == 0 && (witness.inode != 2 || !witness.name.is_empty()))
                || (index != 0 && !name_valid(&witness.name))
            {
                return Err(invalid.into());
            }
            if index > 0 {
                if index > 1 {
                    path.push(b'/');
                }
                path.extend_from_slice(&witness.name);
            }
            let node = Arc::new(Ancestor {
                proof: Proof {
                    path: path.clone(),
                    inode: witness.inode,
                    record: witness.record,
                },
                parent: parent.clone(),
                depth: index + 1,
            });
            if let Some(parent) = &parent {
                let metadata = crate::file_metadata::Metadata {
                    directory_proof: Some(crate::directory_metadata::Proof::Ext(
                        DirectoryLocation {
                            filesystem,
                            node: node.clone(),
                        },
                    )),
                    ..Default::default()
                };
                directories.push(Directory {
                    id: directory_id(&node),
                    parent: Some(directory_id(parent)),
                    name: String::from_utf8_lossy(&witness.name).into_owned(),
                    deleted: false,
                    metadata,
                });
            }
            parent = Some(node);
        }
        Ok((
            Named {
                bytes: self.bytes.clone(),
                parent: parent.ok_or(invalid)?,
            },
            directories,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    location: Location,
    inode: Vec<u8>,
    name: Option<NameRecord>,
}

impl Record {
    fn size(&self) -> u64 {
        u64::from(u32le(&self.inode, 4)) | u64::from(u32le(&self.inode, 0x6c)) << 32
    }

    fn validate(&self, source: &SourceIdentity) -> Result<(), String> {
        if self.location.filesystem != source.filesystem
            || self.location.journal != source.journal
            || self.location.inode < 11
            || !matches!(self.inode.len(), 256 | 512 | 1024 | 2048 | 4096)
            || u16le(&self.inode, 0) & 0xf000 != 0x8000
            || u32le(&self.inode, 0x14) != 0
            || u16le(&self.inode, 0x1a) == 0
            || self.size() > MAX_FILE
        {
            return Err("Ext4 日志进度的候选记录无效。".into());
        }
        Ok(())
    }

    fn restore(&self, id: u32) -> Result<(Candidate, Vec<Directory>), String> {
        let (name, directories) = self
            .name
            .as_ref()
            .map(|name| name.restore(self.location.filesystem))
            .transpose()?
            .map_or_else(|| (None, Vec::new()), |(name, dirs)| (Some(name), dirs));
        if name.as_ref().map(Named::digest) != self.location.name {
            return Err("Ext4 日志进度的名称摘要不一致。".into());
        }
        let directory = name.as_ref().map(|name| directory_id(&name.parent));
        let name = witness_name(self.location.inode, u32le(&self.inode, 0x64), name.as_ref());
        Ok((Candidate {
            id,
            name,
            size: self.size(),
            deleted: true,
            method: "Ext4 日志".into(),
            note: "已核对删除前的 inode、删除记录和当前空闲块。当前读取的字节仍可能受覆盖或 TRIM 影响。".into(),
            supported: true,
            directory_note: if directory.is_some() { "目录由已提交的日志记录确认" } else { "日志中缺少可核对的原始文件名" }.into(),
            directory,
            metadata: crate::file_metadata::ext(&self.inode, true),
            locator: Locator::ExtDeleted(self.location.clone()),
        }, directories))
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Checkpoint {
    source: Option<SourceIdentity>,
    pub total: usize,
    records: Vec<Record>,
    pub sealed: bool,
}

impl Checkpoint {
    pub fn count(&self) -> usize {
        self.records.len()
    }

    pub fn started(&self) -> bool {
        self.source.is_some()
    }

    pub fn summary(&self) -> String {
        let locale = crate::native_locale::current();
        let mut message = locale.format(
            "Ext4 日志删除候选（{count}）。",
            &[("count", &self.count().to_string())],
        );
        if self.source.as_ref().is_some_and(|source| source.limited) {
            message.push(' ');
            message.push_str(locale.text("Ext4 候选内容达到 1 GiB 摘要检查上限。"));
        }
        message
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.source.is_none() {
            if self.total != 0 || self.sealed || !self.records.is_empty() {
                return Err("Ext4 日志空进度包含候选或结束标记。".into());
            }
            return Ok(());
        }
        if self.total > MAX_RESULTS
            || self.count() > self.total
            || self.sealed != (self.count() == self.total)
        {
            return Err("Ext4 日志进度计数或结束标记无效。".into());
        }
        let mut seen = HashSet::new();
        let mut bytes = 0;
        for record in &self.records {
            record.validate(self.source.as_ref().unwrap())?;
            bytes += record.size();
            if !seen.insert(record.location.inode) || bytes > MAX_CONTENT {
                return Err("Ext4 日志进度包含重复 inode 或数据范围过大。".into());
            }
        }
        Ok(())
    }

    pub fn restore(&self) -> Result<(Vec<Candidate>, Catalog), String> {
        let mut catalog = Catalog::default();
        let mut candidates = Vec::with_capacity(self.count());
        self.restore_into(&mut candidates, &mut catalog, 1)?;
        Ok((candidates, catalog))
    }

    pub fn restore_into(
        &self,
        candidates: &mut Vec<Candidate>,
        catalog: &mut Catalog,
        first_id: u32,
    ) -> Result<(), String> {
        self.validate()?;
        for (index, record) in self.records.iter().enumerate() {
            let (candidate, dirs) = record.restore(first_id + index as u32)?;
            for dir in dirs {
                catalog.insert_ext_journal(dir)?;
            }
            catalog.add_file(&candidate);
            candidates.push(candidate);
        }
        Ok(())
    }

    pub fn matches(&self, files: &[Candidate], catalog: &Catalog) -> Result<(), String> {
        let (restored, tree) = self.restore()?;
        if files.len() != restored.len()
            || tree.snapshot() != catalog.snapshot()
            || !same_results(files, &restored)
        {
            return Err("Ext4 日志进度与候选或目录不一致。".into());
        }
        Ok(())
    }

    pub fn verify<R: Read + Seek + 'static>(&self, reader: R) -> Result<(), String> {
        self.validate()?;
        if self.source.is_some() {
            let observed =
                Observed::read(reader, Arc::new(AtomicBool::new(false)), &mut || Ok(()))?;
            observed.verify(self)?;
        }
        Ok(())
    }
}

pub(crate) fn same_results(files: &[Candidate], restored: &[Candidate]) -> bool {
    files.len() == restored.len()
        && files.iter().zip(restored).all(|(actual, expected)| {
            matches!((&actual.locator, &expected.locator),
            (Locator::ExtDeleted(a), Locator::ExtDeleted(b)) if a == b)
                && actual.id == expected.id
                && actual.name == expected.name
                && actual.size == expected.size
                && actual.directory == expected.directory
        })
}

struct Observed<R> {
    history: History<R>,
    identity: SourceIdentity,
    proofs: Vec<Witness>,
    names: BTreeMap<u32, Named>,
}

impl<R: Read + Seek + 'static> Observed<R> {
    fn read(
        reader: R,
        cancel: Arc<AtomicBool>,
        tick: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut history = History::read(reader, cancel, &mut |_, _| tick())?;
        let mut allocation = Sha256::new();
        for range in &history.allocation.free {
            allocation.update(range.start.to_le_bytes());
            allocation.update(range.end.to_le_bytes());
        }
        let mut proofs = history.witnesses()?;
        reject_overlaps(&mut proofs);
        let mut bytes = 0;
        let count = proofs
            .iter()
            .take_while(|proof| {
                bytes += proof.size;
                bytes <= MAX_CONTENT
            })
            .count();
        let identity = SourceIdentity {
            filesystem: history.session.geometry.fingerprint,
            journal: history.journal.digest(),
            allocation: allocation.finalize().into(),
            limited: count < proofs.len(),
        };
        proofs.truncate(count);
        let names = history.names(&proofs)?;
        Ok(Self {
            history,
            identity,
            proofs,
            names,
        })
    }

    fn record(&self, proof: &Witness, content: [u8; 32]) -> Record {
        let name = self.names.get(&proof.inode);
        Record {
            location: self
                .history
                .location(proof, self.identity.journal, name, content),
            inode: proof.old.clone(),
            name: name.map(NameRecord::capture),
        }
    }

    fn verify(&self, checkpoint: &Checkpoint) -> Result<(), String> {
        if checkpoint.source.as_ref() != Some(&self.identity)
            || checkpoint.total != self.proofs.len()
            || checkpoint
                .records
                .iter()
                .zip(&self.proofs)
                .any(|(saved, proof)| self.record(proof, saved.location.content) != *saved)
        {
            return Err("Ext4 日志、分配或已扫描的删除凭据已改变，请重新扫描。".into());
        }
        Ok(())
    }
}

pub(crate) fn continue_scan<R: Read + Seek + 'static>(
    reader: R,
    checkpoint: &mut Checkpoint,
    cancel: Arc<AtomicBool>,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    checkpoint.validate()?;
    if checkpoint.sealed {
        return Err("Ext4 日志扫描已结束。".into());
    }
    let mut observed = Observed::read(reader, cancel.clone(), &mut || {
        tick(checkpoint.count() as u64, checkpoint.total as u64)
    })?;
    if checkpoint.source.is_some() {
        observed.verify(checkpoint)?;
    } else {
        checkpoint.source = Some(observed.identity.clone());
        checkpoint.total = observed.proofs.len();
        checkpoint.sealed = checkpoint.total == 0;
    }
    for index in checkpoint.count()..checkpoint.total {
        tick(index as u64, checkpoint.total as u64)?;
        let proof = &observed.proofs[index];
        let (content, _) = observed
            .history
            .content(proof, std::io::sink(), 0, &mut |_| {
                crate::used_space::stop(&cancel)
            })?;
        let record = observed.record(proof, content);
        // Standalone journal IDs and their flat export names start at one.
        let (candidate, dirs) = record.restore(1 + index as u32)?;
        for dir in dirs {
            directory(dir)?;
        }
        found(candidate);
        checkpoint.records.push(record);
        checkpoint.sealed = checkpoint.count() == checkpoint.total;
    }
    Ok(checkpoint.summary())
}

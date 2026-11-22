//! Versioned recovery progress; v1 signature files remain readable.
use crate::{
    data_tasks::Selection,
    ext_recovery::{deleted as ext_deleted, scan_progress as ext_scan},
    fat_recovery::scan_progress as fat_scan,
    ntfs_checkpoint, ntfs_scan,
    recovery::{Candidate, ScanMode},
    recovery_tree::{Catalog, Directory},
    signature_checkpoint::{self, Binding},
    signature_scan,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek};
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "camelCase", deny_unknown_fields)]
pub enum Cursor {
    Signatures {
        options: signature_scan::Options,
        cursor: signature_scan::Checkpoint,
    },
    Ntfs {
        mode: ScanMode,
        cursor: ntfs_scan::Checkpoint,
    },
    Fat {
        mode: ScanMode,
        cursor: fat_scan::Checkpoint,
    },
    Ext {
        mode: ScanMode,
        cursor: ext_scan::Checkpoint,
    },
    ExtJournal {
        cursor: ext_deleted::Checkpoint,
    },
    Combined {
        metadata: Option<Box<Cursor>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        journal: Option<ext_deleted::Checkpoint>,
        #[serde(rename = "signaturePhase")]
        signature_phase: bool,
        options: signature_scan::Options,
        #[serde(rename = "signatureCursor")]
        signature_cursor: signature_scan::Checkpoint,
    },
}
impl Cursor {
    pub fn requires_journal_lock(&self) -> bool {
        matches!(
            self,
            Self::ExtJournal { .. }
                | Self::Combined {
                    journal: Some(_),
                    ..
                }
        )
    }

    fn active_journal(&self) -> Option<&ext_deleted::Checkpoint> {
        match self {
            Self::Combined {
                metadata: Some(metadata),
                journal: Some(journal),
                signature_phase: false,
                ..
            } if metadata.sealed() => Some(journal),
            _ => None,
        }
    }

    pub fn validate(&self, size: u64) -> Result<(), String> {
        match self {
            Self::Signatures { options, cursor } => {
                let r = options.range(size)?;
                cursor.validate(r.end - r.start, options)
            }
            Self::Ntfs { mode, cursor } => cursor.validate(*mode),
            Self::Fat { mode, cursor } => cursor.validate(*mode, size),
            Self::Ext { mode, cursor } => cursor.validate(*mode, size),
            Self::ExtJournal { cursor } => cursor.validate(),
            Self::Combined {
                metadata,
                journal,
                signature_phase,
                options,
                signature_cursor,
            } => {
                if let Some(metadata) = metadata {
                    if !matches!(
                        metadata.as_ref(),
                        Self::Ntfs {
                            mode: ScanMode::AllFiles,
                            ..
                        } | Self::Fat {
                            mode: ScanMode::AllFiles,
                            ..
                        } | Self::Ext {
                            mode: ScanMode::AllFiles,
                            ..
                        }
                    ) || (*signature_phase && !metadata.sealed())
                    {
                        return Err("完整恢复的目录阶段无效。".into());
                    }
                    metadata.validate(size)?;
                } else if !signature_phase {
                    return Err("完整恢复缺少目录阶段。".into());
                } else if options.free_only {
                    return Err("无法确认文件系统，完整恢复的空闲簇范围不可用。".into());
                }
                if let Some(journal) = journal {
                    if !matches!(metadata.as_deref(), Some(Self::Ext { .. }))
                        || (journal.started() && !metadata.as_deref().is_some_and(Self::sealed))
                        || (*signature_phase && !journal.sealed)
                    {
                        return Err("完整恢复的 Ext4 日志阶段无效。".into());
                    }
                    journal.validate()?;
                }
                if !signature_phase && *signature_cursor != signature_scan::Checkpoint::default() {
                    return Err("签名阶段在目录阶段结束前已有进度。".into());
                }
                let range = options.range(size)?;
                signature_cursor.validate(range.end - range.start, options)
            }
        }
    }
    pub fn mode(&self) -> ScanMode {
        match self {
            Self::Signatures { .. } => ScanMode::Signatures,
            Self::Ntfs { mode, .. } => *mode,
            Self::Fat { mode, .. } => *mode,
            Self::Ext { mode, .. } => *mode,
            Self::ExtJournal { .. } => ScanMode::Deleted,
            Self::Combined { .. } => ScanMode::AllFiles,
        }
    }
    pub fn kind(&self) -> &'static str {
        if self.active_journal().is_some() {
            return "extJournal";
        }
        match self {
            Self::Signatures { .. } => "signatures",
            Self::Ntfs { .. } => "ntfs",
            Self::Fat { cursor, .. } => cursor.kind(),
            Self::Ext { .. } => "ext",
            Self::ExtJournal { .. } => "extJournal",
            Self::Combined {
                metadata: Some(metadata),
                signature_phase: false,
                ..
            } => metadata.kind(),
            Self::Combined { .. } => "signatures",
        }
    }
    pub fn options(&self) -> Option<signature_scan::Options> {
        match self {
            Self::Signatures { options, .. } | Self::Combined { options, .. } => {
                Some(options.clone())
            }
            _ => None,
        }
    }
    pub fn done(&self) -> u64 {
        if let Some(journal) = self.active_journal() {
            return journal.count() as u64;
        }
        match self {
            Self::Signatures { cursor, .. } => cursor.consumed,
            Self::Ntfs { cursor, .. } => cursor.next,
            Self::Fat { cursor, .. } => cursor.records,
            Self::Ext { cursor, .. } => cursor.entries,
            Self::ExtJournal { cursor } => cursor.count() as u64,
            Self::Combined {
                metadata: Some(metadata),
                signature_phase: false,
                ..
            } => metadata.done(),
            Self::Combined {
                signature_cursor, ..
            } => signature_cursor.consumed,
        }
    }
    pub fn total(&self, size: u64) -> Result<u64, String> {
        if let Some(journal) = self.active_journal() {
            return Ok(journal.total as u64);
        }
        match self {
            Self::Signatures { options, .. } => {
                let r = options.range(size)?;
                Ok(r.end - r.start)
            }
            Self::Ntfs { cursor, .. } => Ok(cursor.total),
            Self::Fat { cursor, .. } => Ok(if cursor.sealed { cursor.records } else { 0 }),
            Self::Ext { cursor, .. } => Ok(if cursor.sealed { cursor.entries } else { 0 }),
            Self::ExtJournal { cursor } => Ok(cursor.total as u64),
            Self::Combined {
                metadata: Some(metadata),
                signature_phase: false,
                ..
            } => metadata.total(size),
            Self::Combined { options, .. } => {
                let range = options.range(size)?;
                Ok(range.end - range.start)
            }
        }
    }
    pub fn sealed(&self) -> bool {
        match self {
            Self::Signatures { cursor, .. } => cursor.sealed,
            Self::Ntfs { cursor, .. } => cursor.sealed,
            Self::Fat { cursor, .. } => cursor.sealed,
            Self::Ext { cursor, .. } => cursor.sealed,
            Self::ExtJournal { cursor } => cursor.sealed,
            Self::Combined {
                signature_phase,
                signature_cursor,
                ..
            } => *signature_phase && signature_cursor.sealed,
        }
    }
    pub fn count(&self) -> usize {
        match self {
            Self::Signatures { cursor, .. } => cursor.count as usize,
            Self::Ntfs { cursor, .. } => cursor.count,
            Self::Fat { cursor, .. } => cursor.count,
            Self::Ext { cursor, .. } => cursor.count,
            Self::ExtJournal { cursor } => cursor.count(),
            Self::Combined {
                metadata,
                journal,
                signature_cursor,
                ..
            } => {
                metadata.as_deref().map_or(0, Self::count)
                    + journal.as_ref().map_or(0, ext_deleted::Checkpoint::count)
                    + signature_cursor.count as usize
            }
        }
    }
    pub fn verify(&self, reader: &mut (impl Read + Seek), size: u64) -> Result<(), String> {
        match self {
            Self::Signatures { options, cursor } => {
                let r = options.range(size)?;
                cursor.validate(r.end - r.start, options)?;
                cursor.verify_cursor(reader, options)
            }
            Self::Ntfs { mode, cursor } => cursor.verify(reader, *mode),
            Self::Fat { mode, cursor } => cursor.verify(reader, *mode, size),
            Self::Ext { .. } | Self::ExtJournal { .. } => {
                Err("Ext 校验必须使用已绑定的 owned reader".into())
            }
            Self::Combined { metadata, .. } => {
                if let Some(metadata) = metadata {
                    metadata.verify(reader, size)?;
                }
                self.verify_combined_signature(reader, size)
            }
        }
    }
    pub fn verify_owned<R: Read + Seek + 'static>(
        &self,
        mut reader: R,
        size: u64,
    ) -> Result<(), String> {
        match self {
            Self::Ext { mode, cursor } => cursor.verify(reader, *mode, size),
            Self::ExtJournal { cursor } => cursor.verify(reader),
            Self::Combined {
                metadata, journal, ..
            } => {
                let mut reader = ext_scan::SharedReader::new(reader);
                match metadata.as_deref() {
                    Some(Self::Ext { mode, cursor }) => {
                        cursor.verify(reader.clone(), *mode, size)?
                    }
                    Some(metadata) => metadata.verify(&mut reader, size)?,
                    None => {}
                }
                if let Some(journal) = journal {
                    journal.verify(reader.clone())?;
                }
                self.verify_combined_signature(&mut reader, size)
            }
            _ => self.verify(&mut reader, size),
        }
    }

    pub fn signature_progress(
        &self,
    ) -> Option<(&signature_scan::Options, &signature_scan::Checkpoint)> {
        match self {
            Self::Signatures { options, cursor }
            | Self::Combined {
                options,
                signature_cursor: cursor,
                signature_phase: true,
                ..
            } => Some((options, cursor)),
            _ => None,
        }
    }

    fn verify_combined_signature(
        &self,
        reader: &mut (impl Read + Seek),
        size: u64,
    ) -> Result<(), String> {
        self.validate(size)?;
        if let Some((options, cursor)) = self.signature_progress() {
            cursor.verify_cursor(reader, options)?;
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "camelCase", deny_unknown_fields)]
pub enum Records {
    Signatures {
        files: Vec<signature_scan::StoredCandidate>,
    },
    Ntfs {
        files: Vec<ntfs_checkpoint::Record>,
        directories: Vec<Directory>,
    },
    Fat {
        files: Vec<fat_scan::Record>,
    },
    Ext {
        files: Vec<ext_scan::Record>,
    },
    ExtJournal {},
    Combined {
        metadata: Option<Box<Records>>,
        signatures: Vec<signature_scan::StoredCandidate>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Progress {
    pub source: Selection,
    pub binding: Binding,
    pub cursor: Cursor,
    pub records: Records,
}
impl Progress {
    pub fn capture(
        source: Selection,
        binding: Binding,
        cursor: Cursor,
        files: &[Candidate],
        tree: &Catalog,
    ) -> Result<Self, String> {
        cursor.validate(binding.size)?;
        let records = Self::capture_records(&cursor, files, tree)?;
        let p = Self {
            source,
            binding,
            cursor,
            records,
        };
        let (restored, restored_catalog) = p.validate(&p.source)?;
        if let Cursor::Combined {
            metadata,
            journal: Some(journal),
            ..
        } = &p.cursor
        {
            let start = metadata.as_deref().map_or(0, Cursor::count);
            let deleted = start..start + journal.count();
            if restored_catalog.snapshot() != tree.snapshot()
                || !ext_deleted::same_results(&files[deleted.clone()], &restored[deleted])
            {
                return Err("完整恢复的 Ext4 日志进度与候选或目录不一致。".into());
            }
        }
        Ok(p)
    }

    fn capture_records(
        cursor: &Cursor,
        files: &[Candidate],
        tree: &Catalog,
    ) -> Result<Records, String> {
        Ok(match cursor {
            Cursor::Signatures { .. } => Records::Signatures {
                files: files
                    .iter()
                    .map(signature_scan::StoredCandidate::from_candidate)
                    .collect::<Result<_, _>>()?,
            },
            Cursor::Ntfs { .. } => Records::Ntfs {
                files: files
                    .iter()
                    .map(ntfs_checkpoint::Record::capture)
                    .collect::<Result<_, _>>()?,
                directories: tree.snapshot(),
            },
            Cursor::Fat { cursor, .. } => Records::Fat {
                files: fat_scan::capture(cursor, files, tree)?,
            },
            Cursor::Ext { cursor, .. } => Records::Ext {
                files: ext_scan::capture(cursor, files, tree)?,
            },
            Cursor::ExtJournal { cursor } => {
                cursor.matches(files, tree)?;
                Records::ExtJournal {}
            }
            Cursor::Combined {
                metadata, journal, ..
            } => {
                let directory_count = metadata.as_deref().map_or(0, Cursor::count);
                let count =
                    directory_count + journal.as_ref().map_or(0, ext_deleted::Checkpoint::count);
                if files.len() != cursor.count() {
                    return Err("完整恢复进度与候选数量不一致。".into());
                }
                let directory_files = &files[..directory_count];
                let metadata = metadata
                    .as_ref()
                    .map(|engine| {
                        if let (Some(_), Cursor::Ext { cursor, .. }) = (journal, engine.as_ref()) {
                            // Capture the directory stage against its own
                            // tree; the complete tree is checked after restore.
                            return ext_scan::capture(cursor, directory_files, &cursor.tree()?)
                                .map(|files| Box::new(Records::Ext { files }));
                        }
                        Self::capture_records(engine, directory_files, tree).map(Box::new)
                    })
                    .transpose()?;
                let signatures = files[count..]
                    .iter()
                    .enumerate()
                    .map(|(id, candidate)| {
                        if candidate.id != (count + id) as u32 {
                            return Err("完整恢复候选标识不连续。".into());
                        }
                        signature_scan::StoredCandidate::from_candidate(candidate)
                    })
                    .collect::<Result<_, _>>()?;
                Records::Combined {
                    metadata,
                    signatures,
                }
            }
        })
    }
    pub fn validate(&self, selected: &Selection) -> Result<(Vec<Candidate>, Catalog), String> {
        let hex = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if self.source.target != selected.target
            || self.source.layout_key != selected.layout_key
            || self.source.partition != selected.partition
            || !hex(&self.source.target.0)
            || !hex(&self.source.layout_key)
            || self.source.partition == Some(0)
        {
            return Err("恢复进度与所选磁盘、分区或布局不匹配。".into());
        }
        self.binding.validate()?;
        self.cursor.validate(self.binding.size)?;
        self.restore_records(&self.cursor, &self.records)
    }

    fn restore_records(
        &self,
        cursor: &Cursor,
        records: &Records,
    ) -> Result<(Vec<Candidate>, Catalog), String> {
        match (cursor, records) {
            (Cursor::Signatures { options, cursor }, Records::Signatures { files }) => {
                let legacy = signature_checkpoint::Progress {
                    source: self.source.clone(),
                    binding: self.binding.clone(),
                    options: options.clone(),
                    cursor: cursor.clone(),
                    records: files.clone(),
                };
                let files = legacy.validate(&self.source)?;
                let mut tree = Catalog::default();
                for c in &files {
                    tree.add_file(c);
                }
                Ok((files, tree))
            }
            (Cursor::Ntfs { mode, cursor }, Records::Ntfs { files, directories }) => {
                if self.source.partition.is_none() {
                    return Err("NTFS 进度必须绑定分区".into());
                }
                ntfs_checkpoint::restore(files, directories, cursor, *mode)
            }
            (Cursor::Fat { mode, cursor }, Records::Fat { files }) => {
                if self.source.partition.is_none() {
                    return Err("FAT/exFAT 进度必须绑定分区".into());
                }
                fat_scan::restore(cursor, *mode, self.binding.size, files)
            }
            (Cursor::Ext { mode, cursor }, Records::Ext { files }) => {
                if self.source.partition.is_none() {
                    return Err("Ext 进度必须绑定分区".into());
                }
                ext_scan::restore(cursor, *mode, self.binding.size, files)
            }
            (Cursor::ExtJournal { cursor }, Records::ExtJournal {}) => {
                if self.source.partition.is_none() {
                    return Err("Ext4 日志进度必须绑定分区。".into());
                }
                cursor.restore()
            }
            (
                Cursor::Combined {
                    metadata,
                    journal,
                    options,
                    signature_cursor,
                    ..
                },
                Records::Combined {
                    metadata: records,
                    signatures,
                },
            ) => {
                if self.source.partition.is_none() {
                    return Err("完整恢复进度必须绑定分区。".into());
                }
                let (mut files, mut tree) = match (metadata, records) {
                    (Some(metadata), Some(records)) => self.restore_records(metadata, records)?,
                    (None, None) => (Vec::new(), Catalog::default()),
                    _ => return Err("完整恢复的目录进度与候选不一致。".into()),
                };
                if let Some(journal) = journal {
                    let first_deleted = files.len() as u32;
                    journal.restore_into(&mut files, &mut tree, first_deleted)?;
                }
                let signatures = signature_checkpoint::Progress {
                    source: self.source.clone(),
                    binding: self.binding.clone(),
                    options: options.clone(),
                    cursor: signature_cursor.clone(),
                    records: signatures.clone(),
                }
                .validate(&self.source)?;
                let first_signature = files.len() as u32;
                for mut candidate in signatures {
                    candidate.id += first_signature;
                    tree.add_file(&candidate);
                    files.push(candidate);
                }
                Ok((files, tree))
            }
            _ => Err("恢复进度引擎与候选记录不一致".into()),
        }
    }
    pub fn verify_cursor(&self, reader: &mut (impl Read + Seek), size: u64) -> Result<(), String> {
        if let (Cursor::Fat { mode, cursor }, Records::Fat { files }) =
            (&self.cursor, &self.records)
        {
            cursor.verify_records(reader, *mode, size, Some(files))
        } else {
            self.cursor.verify(reader, size)
        }
    }
    pub fn verify_owned<R: Read + Seek + 'static>(
        &self,
        mut reader: R,
        size: u64,
    ) -> Result<(), String> {
        match (&self.cursor, &self.records) {
            (Cursor::Ext { mode, cursor }, Records::Ext { files }) => {
                cursor.verify_records(reader, *mode, size, Some(files))
            }
            (Cursor::ExtJournal { cursor }, Records::ExtJournal {}) => cursor.verify(reader),
            (
                Cursor::Combined {
                    metadata, journal, ..
                },
                Records::Combined {
                    metadata: records, ..
                },
            ) => {
                let mut reader = ext_scan::SharedReader::new(reader);
                match (metadata.as_deref(), records.as_deref()) {
                    (Some(Cursor::Fat { mode, cursor }), Some(Records::Fat { files })) => {
                        cursor.verify_records(&mut reader, *mode, size, Some(files))?
                    }
                    (Some(Cursor::Ext { mode, cursor }), Some(Records::Ext { files })) => {
                        cursor.verify_records(reader.clone(), *mode, size, Some(files))?
                    }
                    (Some(Cursor::Ntfs { mode, cursor }), Some(Records::Ntfs { .. })) => {
                        cursor.verify(&mut reader, *mode)?
                    }
                    (None, None) => {}
                    _ => return Err("完整恢复的目录进度无效。".into()),
                }
                if let Some(journal) = journal {
                    journal.verify(reader.clone())?;
                }
                self.cursor.verify_combined_signature(&mut reader, size)
            }
            _ => self.verify_cursor(&mut reader, size),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    version: u32,
    sha256: String,
    progress: Progress,
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn version(cursor: &Cursor) -> u32 {
    match cursor {
        Cursor::ExtJournal { .. } => 4,
        Cursor::Combined {
            journal: Some(_), ..
        } => 5,
        Cursor::Combined { .. } => 3,
        _ => 2,
    }
}
pub fn encode(progress: Progress) -> Result<Vec<u8>, String> {
    progress.validate(&progress.source)?;
    let digest = hash(&serde_json::to_vec(&progress).map_err(|e| e.to_string())?);
    let bytes = serde_json::to_vec(&Envelope {
        format: "DiskStudio recovery progress".into(),
        version: version(&progress.cursor),
        sha256: digest,
        progress,
    })
    .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > signature_checkpoint::MAX_FILE {
        return Err("恢复进度超过 64 MiB 上限".into());
    }
    Ok(bytes)
}
pub fn decode(bytes: &[u8], selected: &Selection) -> Result<Progress, String> {
    if bytes.len() as u64 > signature_checkpoint::MAX_FILE {
        return Err("恢复进度超过 64 MiB 上限".into());
    }
    #[derive(Deserialize)]
    struct Header {
        format: String,
        version: u32,
    }
    let h: Header = serde_json::from_slice(bytes).map_err(|_| "恢复进度格式无效")?;
    if h.version == 1 && h.format == "DiskStudio signature progress" {
        let p = signature_checkpoint::decode(bytes, selected)?;
        return Ok(Progress {
            source: p.source,
            binding: p.binding,
            cursor: Cursor::Signatures {
                options: p.options,
                cursor: p.cursor,
            },
            records: Records::Signatures { files: p.records },
        });
    }
    let e: Envelope = serde_json::from_slice(bytes).map_err(|_| "恢复进度文件格式无效")?;
    if e.version != version(&e.progress.cursor)
        || e.format != "DiskStudio recovery progress"
        || e.sha256 != hash(&serde_json::to_vec(&e.progress).map_err(|e| e.to_string())?)
    {
        return Err("恢复进度版本不支持或摘要不一致".into());
    }
    e.progress.validate(selected)?;
    Ok(e.progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as Reader;
    const IMAGE: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
    fn source() -> Selection {
        Selection {
            target: crate::partitions::Target("a".repeat(64)),
            layout_key: "b".repeat(64),
            partition: Some(2),
        }
    }
    fn fixture(mode: ScanMode, stop: u64) -> Progress {
        let mut cp = ntfs_scan::Checkpoint::default();
        let mut files = vec![];
        let mut tree = Catalog::default();
        let _ = ntfs_scan::continue_scan(
            &mut Reader::new(IMAGE),
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
        let binding =
            Binding::capture(&mut Reader::new(IMAGE), IMAGE.len() as u64, 4096, 512).unwrap();
        Progress::capture(
            source(),
            binding,
            Cursor::Ntfs { mode, cursor: cp },
            &files,
            &tree,
        )
        .unwrap()
    }
    fn normal(files: &[Candidate]) -> serde_json::Value {
        let mut v = serde_json::to_value(files).unwrap();
        for c in v.as_array_mut().unwrap() {
            c.as_object_mut().unwrap().remove("note");
        }
        v
    }

    fn ext_tree_image() -> Vec<u8> {
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../../tests/fixtures/ext-journal-tree-linux.raw.gz")[..],
        )
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .unwrap();
        assert_eq!(raw.len(), 32 * 1024 * 1024);
        raw
    }

    #[test]
    fn ext_journal_progress_round_trips_before_the_last_candidate() {
        use std::sync::{atomic::AtomicBool, Arc};

        let raw = ext_tree_image();
        let size = raw.len() as u64;
        let raw: Arc<[u8]> = raw.into();
        let reader = || Reader::new(raw.clone());
        let mut checkpoint = ext_deleted::Checkpoint::default();
        let mut files = Vec::new();
        let mut catalog = Catalog::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = ext_deleted::continue_scan(
            reader(),
            &mut checkpoint,
            cancel.clone(),
            |done, _| {
                if done == 2 {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |file| files.push(file),
            |dir| catalog.insert(dir),
        );
        assert_eq!(paused.unwrap_err(), "pause");
        assert_eq!(checkpoint.count(), 2);
        assert!(!checkpoint.sealed);
        let progress = Progress::capture(
            source(),
            Binding::capture(&mut reader(), size, 32 * 1024 * 1024, 4096).unwrap(),
            Cursor::ExtJournal { cursor: checkpoint },
            &files,
            &catalog,
        )
        .unwrap();
        let encoded = encode(progress).unwrap();
        let decoded = decode(&encoded, &source()).unwrap();
        assert_eq!(version(&decoded.cursor), 4);
        decoded.verify_owned(reader(), size).unwrap();
        let (mut restored, mut tree) = decoded.validate(&source()).unwrap();
        assert_eq!(normal(&files), normal(&restored));
        assert_eq!(tree.snapshot(), catalog.snapshot());
        let Cursor::ExtJournal {
            cursor: mut checkpoint,
        } = decoded.cursor
        else {
            panic!()
        };
        ext_deleted::continue_scan(
            reader(),
            &mut checkpoint,
            cancel,
            |_, _| Ok(()),
            |file| restored.push(file),
            |dir| tree.insert(dir),
        )
        .unwrap();
        assert_eq!(
            restored
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["removed.txt", "fragment.bin", "extent-tree.bin"]
        );
        assert!(checkpoint.sealed);
        assert_eq!(
            restored.iter().map(|file| file.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(restored[1].directory, restored[2].directory);
        assert_eq!(restored[2].size, 12_583_631);
        let mut changed = raw.to_vec();
        // The bytes after the 1024-byte JBD2 superblock are outside that
        // structure's CRC but still belong to the saved journal digest.
        let journal = changed
            .chunks_exact(4096)
            .position(|block| block[..8] == [0xc0, 0x3b, 0x39, 0x98, 0, 0, 0, 4])
            .unwrap();
        changed[journal * 4096 + 2048] ^= 1;
        assert!(checkpoint
            .verify(Reader::new(changed))
            .unwrap_err()
            .contains("删除凭据已改变"));
    }

    #[test]
    fn complete_ext4_progress_resumes_journal_then_scans_verified_free_space() {
        use crate::data_tasks::recovery_tasks::scan_combined_metadata;
        use std::sync::{atomic::AtomicBool, Arc};

        let mut image = ext_tree_image();
        let size = image.len() as u64;
        let mut incompatible = image.clone();
        let control = incompatible
            .chunks_exact(4096)
            .position(|block| block[..8] == [0xc0, 0x3b, 0x39, 0x98, 0, 0, 0, 4])
            .unwrap();
        incompatible[control * 4096 + 0xfc] ^= 1;
        assert!(!ext_deleted::supports(
            Reader::new(incompatible),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap());
        let last_block = image.len() - 4096;
        let sample = include_bytes!("../../tests/fixtures/ext-complete-sample.pdf");
        assert!(image[last_block..].iter().all(|&byte| byte == 0));
        image[last_block..last_block + sample.len()].copy_from_slice(sample);
        let source_bytes: Arc<[u8]> = image.into();
        let mut reader = ext_scan::SharedReader::new(Reader::new(source_bytes));
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(ext_deleted::may_probe(&mut reader).unwrap());
        assert!(ext_deleted::supports(reader.clone(), cancel.clone()).unwrap());
        let binding = Binding::capture(&mut reader, size, 32 * 1024 * 1024, 4096).unwrap();
        let mut metadata = Cursor::Ext {
            mode: ScanMode::AllFiles,
            cursor: Default::default(),
        };
        let mut journal = ext_deleted::Checkpoint::default();
        let options = signature_scan::Options {
            kinds: vec![signature_scan::Kind::Pdf],
            start: last_block as u64,
            length: Some(4096),
            sector_aligned: true,
            free_only: true,
        };
        let mut found = Vec::new();
        let mut tree = Catalog::default();
        let paused = scan_combined_metadata(
            &mut reader,
            size,
            &mut metadata,
            Some(&mut journal),
            &cancel,
            |phase, done, _| {
                if phase == "Ext4 日志扫描" && done == 2 {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |file| found.push(file),
            |node, _| tree.insert_ext_journal(node),
        );
        assert_eq!(paused.unwrap_err(), "pause");
        assert!(metadata.sealed());
        assert_eq!(metadata.count(), 1);
        assert_eq!(journal.count(), 2);
        for signature_phase in [true, false] {
            let attempt = Cursor::Combined {
                metadata: Some(Box::new(metadata.clone())),
                journal: Some(journal.clone()),
                signature_phase,
                options: options.clone(),
                signature_cursor: Default::default(),
            };
            assert_eq!(attempt.validate(size).is_ok(), !signature_phase);
        }
        let progress = Progress::capture(
            source(),
            binding.clone(),
            Cursor::Combined {
                metadata: Some(Box::new(metadata)),
                journal: Some(journal),
                signature_phase: false,
                options: options.clone(),
                signature_cursor: Default::default(),
            },
            &found,
            &tree,
        )
        .unwrap();
        assert_eq!(progress.cursor.kind(), "extJournal");
        let loaded = decode(&encode(progress).unwrap(), &source()).unwrap();
        assert_eq!(version(&loaded.cursor), 5);
        loaded.verify_owned(reader.clone(), size).unwrap();
        let (mut found, mut tree) = loaded.validate(&source()).unwrap();
        let Cursor::Combined {
            metadata: Some(mut metadata),
            journal: Some(mut journal),
            ..
        } = loaded.cursor
        else {
            panic!()
        };
        scan_combined_metadata(
            &mut reader,
            size,
            &mut metadata,
            Some(&mut journal),
            &cancel,
            |_, _, _| Ok(()),
            |file| found.push(file),
            |node, _| tree.insert_ext_journal(node),
        )
        .unwrap();
        assert!(journal.sealed);
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].name, "retained.txt");
        assert!(found[1..].iter().all(|file| file.deleted));
        assert_eq!(found[2].directory, found[3].directory);
        assert_eq!(
            tree.snapshot()
                .iter()
                .filter(|dir| dir.name == "nested")
                .count(),
            1
        );
        let free = crate::used_space::inspect(&mut reader, size, 4096, None, &cancel).unwrap();
        let mut signature_cursor = signature_scan::Checkpoint::default();
        let first_signature = found.len() as u32;
        signature_scan::continue_scan_scoped(
            &mut reader,
            size,
            &options,
            &mut signature_cursor,
            4096,
            Some(&free),
            |_, _| Ok(()),
            |mut file| {
                file.id += first_signature;
                tree.add_file(&file);
                found.push(file);
            },
        )
        .unwrap();
        let completed = Progress::capture(
            source(),
            binding,
            Cursor::Combined {
                metadata: Some(metadata),
                journal: Some(journal),
                signature_phase: true,
                options,
                signature_cursor,
            },
            &found,
            &tree,
        )
        .unwrap();
        let loaded = decode(&encode(completed).unwrap(), &source()).unwrap();
        let (files, restored_catalog) = loaded.validate(&source()).unwrap();
        assert_eq!(
            files.iter().map(|file| file.id).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(files[3].name, "extent-tree.bin");
        assert_eq!(files[4].size, sample.len() as u64);
        assert_eq!(normal(&files), normal(&found));
        assert_eq!(restored_catalog.snapshot(), tree.snapshot());
        loaded.verify_owned(reader.clone(), size).unwrap();
        signature_scan::verify_free_candidates(&free, &files).unwrap();
    }

    #[test]
    fn complete_recovery_restores_metadata_and_signature_records_in_one_progress() {
        let p = fixture(ScanMode::AllFiles, u64::MAX);
        assert!(p.cursor.sealed());
        let (mut files, tree) = p.validate(&p.source).unwrap();
        let metadata_count = files.len();
        let mut bytes = IMAGE.to_vec();
        bytes.resize(IMAGE.len() + 3 * 1024 * 1024, 0);
        let offset = IMAGE.len() + 512;
        let sample = b"%PDF-1.4\ncomplete recovery test\n%%EOF";
        bytes[offset..offset + sample.len()].copy_from_slice(sample);
        let binding =
            Binding::capture(&mut Reader::new(&bytes), bytes.len() as u64, 4096, 512).unwrap();
        let options = signature_scan::Options {
            kinds: vec![signature_scan::Kind::Pdf],
            start: IMAGE.len() as u64,
            ..Default::default()
        };
        let boundary = Progress::capture(
            source(),
            binding.clone(),
            Cursor::Combined {
                metadata: Some(Box::new(p.cursor.clone())),
                journal: None,
                signature_phase: false,
                options: options.clone(),
                signature_cursor: Default::default(),
            },
            &files,
            &tree,
        )
        .unwrap();
        let saved_boundary = encode(boundary).unwrap();
        let original_shape: serde_json::Value = serde_json::from_slice(&saved_boundary).unwrap();
        assert_eq!(original_shape["version"], 3);
        assert!(original_shape["progress"]["cursor"]
            .get("journal")
            .is_none());
        let boundary = decode(&saved_boundary, &source()).unwrap();
        assert!(!boundary.cursor.sealed());
        assert_eq!(boundary.cursor.kind(), "ntfs");
        assert_eq!(
            boundary.validate(&source()).unwrap().0.len(),
            metadata_count
        );
        let mut signature_cursor = signature_scan::Checkpoint::default();
        signature_scan::continue_scan(
            &mut Reader::new(&bytes),
            bytes.len() as u64,
            &options,
            &mut signature_cursor,
            |_, _| Ok(()),
            |mut candidate| {
                candidate.id += metadata_count as u32;
                files.push(candidate);
            },
        )
        .unwrap();
        let progress = Progress::capture(
            source(),
            binding,
            Cursor::Combined {
                metadata: Some(Box::new(p.cursor)),
                journal: None,
                signature_phase: true,
                options,
                signature_cursor,
            },
            &files,
            &tree,
        )
        .unwrap();
        let encoded = encode(progress).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap()["version"],
            3
        );
        let decoded = decode(&encoded, &source()).unwrap();
        decoded
            .verify_owned(Reader::new(bytes.clone()), bytes.len() as u64)
            .unwrap();
        assert!(decoded.cursor.sealed());
        assert_eq!(decoded.cursor.kind(), "signatures");
        let (restored, directory) = decoded.validate(&source()).unwrap();
        assert_eq!(restored.len(), metadata_count + 1);
        assert_eq!(
            restored.iter().map(|c| c.id).collect::<Vec<_>>(),
            (0..restored.len() as u32).collect::<Vec<_>>()
        );
        let folder = directory
            .folders(Some(crate::recovery_tree::SIGNATURES), 0)
            .unwrap();
        assert_eq!(folder.signature_files, 1);
        assert!(folder.path.is_empty());
        let filter = crate::recovery_tree::Filter {
            folder: Some(crate::recovery_tree::SIGNATURES.into()),
            ..Default::default()
        };
        assert_eq!(
            crate::recovery_tree::select_ids(&restored, &directory, "", &filter).unwrap(),
            vec![metadata_count as u32]
        );
        let mut extracted = Vec::new();
        crate::recovery::extract(
            &mut Reader::new(bytes),
            &restored[metadata_count],
            &mut extracted,
            256,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(extracted, sample);
        let mut wrong_source = source();
        wrong_source.partition = Some(9);
        assert!(decode(&encoded, &wrong_source).is_err());
    }

    #[test]
    fn complete_recovery_rejects_signature_results_before_metadata_has_finished() {
        let p = fixture(ScanMode::AllFiles, 24);
        assert!(!p.cursor.sealed());
        let (files, tree) = p.validate(&p.source).unwrap();
        let cursor = Cursor::Combined {
            metadata: Some(Box::new(p.cursor)),
            journal: None,
            signature_phase: true,
            options: Default::default(),
            signature_cursor: Default::default(),
        };
        assert!(Progress::capture(source(), p.binding, cursor, &files, &tree).is_err());
    }

    #[test]
    fn complete_recovery_accepts_only_signature_records_when_no_filesystem_is_identified() {
        let mut bytes = vec![0; 2 * 1024 * 1024];
        let sample = b"%PDF-1.4\nheader-only source\n%%EOF";
        bytes[512..512 + sample.len()].copy_from_slice(sample);
        let mut reader = Reader::new(&bytes);
        let options = signature_scan::Options {
            kinds: vec![signature_scan::Kind::Pdf],
            ..Default::default()
        };
        let binding = Binding::capture(&mut reader, bytes.len() as u64, 4096, 512).unwrap();
        let mut signatures = signature_scan::Checkpoint::default();
        let mut files = Vec::new();
        signature_scan::continue_scan(
            &mut reader,
            bytes.len() as u64,
            &options,
            &mut signatures,
            |_, _| Ok(()),
            |candidate| files.push(candidate),
        )
        .unwrap();
        let cursor = Cursor::Combined {
            metadata: None,
            journal: None,
            signature_phase: true,
            options,
            signature_cursor: signatures,
        };
        let progress =
            Progress::capture(source(), binding, cursor, &files, &Catalog::default()).unwrap();
        let decoded = decode(&encode(progress).unwrap(), &source()).unwrap();
        let (files, catalog) = decoded.validate(&source()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, 0);
        assert_eq!(catalog.snapshot().len(), 1);
        assert_eq!(catalog.folders(None, 0).unwrap().signature_files, 1);
        assert!(decoded.cursor.sealed());
    }
    #[test]
    fn ntfs_saved_catalogue_resumes_all_modes_and_retains_empty_directories() {
        for mode in [ScanMode::Existing, ScanMode::AllFiles, ScanMode::Deleted] {
            let full = fixture(mode, u64::MAX);
            let (expected, dirs) = full.validate(&full.source).unwrap();
            let total = full.cursor.total(IMAGE.len() as u64).unwrap();
            for stop in [16, 24, total / 2, total - 1] {
                let p = fixture(mode, stop);
                let mut p = decode(&encode(p).unwrap(), &source()).unwrap();
                let (mut files, mut tree) = p.validate(&source()).unwrap();
                p.binding
                    .verify(&mut Reader::new(IMAGE), IMAGE.len() as u64, 4096, 512)
                    .unwrap();
                p.cursor
                    .verify(&mut Reader::new(IMAGE), IMAGE.len() as u64)
                    .unwrap();
                let Cursor::Ntfs { mode, cursor } = &mut p.cursor else {
                    panic!()
                };
                assert_eq!(cursor.next, stop);
                ntfs_scan::continue_scan(
                    &mut Reader::new(IMAGE),
                    *mode,
                    cursor,
                    |_, _| Ok(()),
                    |c| files.push(c),
                    |d| tree.insert(d),
                )
                .unwrap();
                assert_eq!(normal(&files), normal(&expected));
                assert_eq!(tree.snapshot(), dirs.snapshot());
                assert_eq!(cursor.count, files.len());
            }
        }
    }
    #[test]
    fn loaded_ntfs_extracts_actual_content_and_rejects_reused_record_before_output() {
        let mut p = fixture(ScanMode::Existing, u64::MAX);
        for _ in 0..3 {
            let decoded = decode(&encode(p).unwrap(), &source()).unwrap();
            let (files, tree) = decoded.validate(&source()).unwrap();
            let c = files.iter().find(|c| c.name == "file-with-12345").unwrap();
            assert_eq!(c.note.matches("保存时的历史观察").count(), 1);
            let mut out = vec![];
            crate::recovery::extract(&mut Reader::new(IMAGE), c, &mut out, u64::MAX, |_| Ok(()))
                .unwrap();
            assert_eq!(out, b"12345");
            let crate::recovery::Locator::Ntfs { record, .. } = c.locator else {
                panic!()
            };
            let mut reader = Reader::new(IMAGE);
            let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
            let at = ntfs
                .file(&mut reader, record)
                .unwrap()
                .position()
                .value()
                .unwrap()
                .get() as usize;
            let mut changed = IMAGE.to_vec();
            changed[at + 16] ^= 1;
            out.clear();
            assert!(crate::recovery::extract(
                &mut Reader::new(changed),
                c,
                &mut out,
                u64::MAX,
                |_| Ok(())
            )
            .is_err());
            assert!(out.is_empty());
            p = Progress::capture(
                decoded.source,
                decoded.binding,
                decoded.cursor,
                &files,
                &tree,
            )
            .unwrap();
        }
    }
    fn reject(p: &Progress, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut v = serde_json::to_value(p).unwrap();
        edit(&mut v);
        if let Ok(bad) = serde_json::from_value::<Progress>(v) {
            // Recompute the envelope digest: structural checks must not rely on secrecy.
            let e = Envelope {
                format: "DiskStudio recovery progress".into(),
                version: 2,
                sha256: hash(&serde_json::to_vec(&bad).unwrap()),
                progress: bad,
            };
            assert!(decode(&serde_json::to_vec(&e).unwrap(), &source()).is_err());
        }
    }
    #[test]
    fn ntfs_progress_rejects_forged_candidates_names_metadata_and_directory_graphs() {
        let p = fixture(ScanMode::Existing, u64::MAX);
        reject(&p, |v| v["cursor"]["cursor"]["count"] = 0.into());
        reject(&p, |v| v["cursor"]["cursor"]["next"] = u64::MAX.into());
        reject(&p, |v| v["cursor"]["mode"] = "signatures".into());
        reject(&p, |v| v["source"]["partition"] = serde_json::Value::Null);
        reject(&p, |v| v["records"]["files"][0]["record"] = u64::MAX.into());
        reject(&p, |v| {
            v["records"]["files"][0]["link"]["raw"] = serde_json::json!([0, 0])
        });
        reject(&p, |v| {
            v["records"]["files"][0]["link"]["namespace"] = 4.into()
        });
        reject(&p, |v| {
            v["records"]["files"][0]["metadata"]["source"] = "a".repeat(81).into()
        });
        reject(&p, |v| {
            v["records"]["files"][0]["metadata"]["attributes"] = serde_json::json!(["\u{0000}"])
        });
        reject(&p, |v| {
            v["records"]["files"][0]["directory"] = "ntfs-15-1".into()
        });
        reject(&p, |v| {
            v["records"]["files"][1] = v["records"]["files"][0].clone()
        });
        reject(&p, |v| v["records"]["directories"] = serde_json::json!([]));
        reject(&p, |v| {
            let dirs = v["records"]["directories"].as_array_mut().unwrap();
            let i = dirs.iter().position(|d| d["id"] != "root").unwrap();
            dirs[i]["parent"] = dirs[i]["id"].clone();
        });
        reject(&p, |v| {
            let dirs = v["records"]["directories"].as_array_mut().unwrap();
            let i = dirs.iter().position(|d| d["id"] != "root").unwrap();
            dirs[i]["parent"] = "ntfs-15-1".into();
        });
        reject(&p, |v| {
            let dirs = v["records"]["directories"].as_array_mut().unwrap();
            dirs.push(dirs[0].clone());
        });
        let mut wrong = p.clone();
        wrong.records = Records::Signatures { files: vec![] };
        assert!(encode(wrong).is_err());
    }
    #[test]
    fn ntfs_progress_rejects_scope_corruption_and_unknown_input_fields() {
        let p = fixture(ScanMode::AllFiles, 24);
        let bytes = encode(p.clone()).unwrap();
        for change in 0..3 {
            let mut other = source();
            match change {
                0 => other.target.0 = "c".repeat(64),
                1 => other.layout_key = "d".repeat(64),
                _ => other.partition = Some(3),
            };
            assert!(decode(&bytes, &other).is_err());
        }
        let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["sha256"] = "0".repeat(64).into();
        assert!(decode(&serde_json::to_vec(&v).unwrap(), &source()).is_err());
        v = serde_json::from_slice(&bytes).unwrap();
        v["progress"]["devicePath"] = "arbitrary".into();
        assert!(decode(&serde_json::to_vec(&v).unwrap(), &source()).is_err());
    }
    #[test]
    fn legacy_signature_progress_loads_and_saves_in_version_two_without_losing_cursor() {
        let data = vec![0; 2 * 1024 * 1024];
        let options = signature_scan::Options::default();
        let mut cursor = signature_scan::Checkpoint::default();
        assert!(signature_scan::continue_scan(
            &mut Reader::new(&data),
            data.len() as u64,
            &options,
            &mut cursor,
            |n, _| if n > 0 { Err("pause".into()) } else { Ok(()) },
            |_| panic!()
        )
        .is_err());
        let old = signature_checkpoint::Progress {
            source: source(),
            options,
            binding: Binding::capture(&mut Reader::new(&data), data.len() as u64, 4096, 512)
                .unwrap(),
            cursor,
            records: vec![],
        };
        let p = decode(&signature_checkpoint::encode(old).unwrap(), &source()).unwrap();
        assert_eq!(p.cursor.kind(), "signatures");
        assert_eq!(p.cursor.done(), 1024 * 1024);
        let bytes = encode(p).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["version"],
            2
        );
        let p = decode(&bytes, &source()).unwrap();
        p.cursor
            .verify(&mut Reader::new(&data), data.len() as u64)
            .unwrap();
    }
}

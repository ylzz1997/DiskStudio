//! Versioned recovery progress; v1 signature files remain readable.
use crate::{
    data_tasks::Selection,
    ext_recovery::scan_progress as ext_scan,
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
}
impl Cursor {
    pub fn validate(&self, size: u64) -> Result<(), String> {
        match self {
            Self::Signatures { options, cursor } => {
                let r = options.range(size)?;
                cursor.validate(r.end - r.start, options)
            }
            Self::Ntfs { mode, cursor } => cursor.validate(*mode),
            Self::Fat { mode, cursor } => cursor.validate(*mode, size),
            Self::Ext { mode, cursor } => cursor.validate(*mode, size),
        }
    }
    pub fn mode(&self) -> ScanMode {
        match self {
            Self::Signatures { .. } => ScanMode::Signatures,
            Self::Ntfs { mode, .. } => *mode,
            Self::Fat { mode, .. } => *mode,
            Self::Ext { mode, .. } => *mode,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Signatures { .. } => "signatures",
            Self::Ntfs { .. } => "ntfs",
            Self::Fat { cursor, .. } => cursor.kind(),
            Self::Ext { .. } => "ext",
        }
    }
    pub fn options(&self) -> Option<signature_scan::Options> {
        match self {
            Self::Signatures { options, .. } => Some(options.clone()),
            _ => None,
        }
    }
    pub fn done(&self) -> u64 {
        match self {
            Self::Signatures { cursor, .. } => cursor.consumed,
            Self::Ntfs { cursor, .. } => cursor.next,
            Self::Fat { cursor, .. } => cursor.records,
            Self::Ext { cursor, .. } => cursor.entries,
        }
    }
    pub fn total(&self, size: u64) -> Result<u64, String> {
        match self {
            Self::Signatures { options, .. } => {
                let r = options.range(size)?;
                Ok(r.end - r.start)
            }
            Self::Ntfs { cursor, .. } => Ok(cursor.total),
            Self::Fat { cursor, .. } => Ok(if cursor.sealed { cursor.records } else { 0 }),
            Self::Ext { cursor, .. } => Ok(if cursor.sealed { cursor.entries } else { 0 }),
        }
    }
    pub fn sealed(&self) -> bool {
        match self {
            Self::Signatures { cursor, .. } => cursor.sealed,
            Self::Ntfs { cursor, .. } => cursor.sealed,
            Self::Fat { cursor, .. } => cursor.sealed,
            Self::Ext { cursor, .. } => cursor.sealed,
        }
    }
    pub fn count(&self) -> usize {
        match self {
            Self::Signatures { cursor, .. } => cursor.count as usize,
            Self::Ntfs { cursor, .. } => cursor.count,
            Self::Fat { cursor, .. } => cursor.count,
            Self::Ext { cursor, .. } => cursor.count,
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
            Self::Ext { .. } => Err("Ext 校验必须使用已绑定的 owned reader".into()),
        }
    }
    pub fn verify_owned<R: Read + Seek + 'static>(
        &self,
        mut reader: R,
        size: u64,
    ) -> Result<(), String> {
        match self {
            Self::Ext { mode, cursor } => cursor.verify(reader, *mode, size),
            _ => self.verify(&mut reader, size),
        }
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
        let records = match &cursor {
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
        };
        let p = Self {
            source,
            binding,
            cursor,
            records,
        };
        p.validate(&p.source)?;
        Ok(p)
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
        match (&self.cursor, &self.records) {
            (Cursor::Signatures { options, cursor }, Records::Signatures { files }) => {
                let legacy = signature_checkpoint::Progress {
                    source: self.source.clone(),
                    binding: self.binding.clone(),
                    options: options.clone(),
                    cursor: cursor.clone(),
                    records: files.clone(),
                };
                let files = legacy.validate(selected)?;
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
        if let (Cursor::Ext { mode, cursor }, Records::Ext { files }) =
            (&self.cursor, &self.records)
        {
            cursor.verify_records(reader, *mode, size, Some(files))
        } else {
            self.verify_cursor(&mut reader, size)
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
pub fn encode(progress: Progress) -> Result<Vec<u8>, String> {
    progress.validate(&progress.source)?;
    let digest = hash(&serde_json::to_vec(&progress).map_err(|e| e.to_string())?);
    let bytes = serde_json::to_vec(&Envelope {
        format: "DiskStudio recovery progress".into(),
        version: 2,
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
    if e.version != 2
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

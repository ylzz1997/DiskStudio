//! In-memory scan catalogue. Directory identifiers are metadata identities,
//! never output paths; browsing/filtering performs no filesystem/device I/O.
use crate::recovery::{Candidate, Locator, MAX_RESULTS};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[path = "recovery_query.rs"]
pub mod query;

pub const MAX_DEPTH: usize = 64;
pub const ROOT: &str = "root";
pub const UNKNOWN: &str = "unknown";
pub const SIGNATURES: &str = "signature-results";
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Directory {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub deleted: bool,
    #[serde(
        default,
        skip_serializing_if = "crate::file_metadata::Metadata::is_empty"
    )]
    pub metadata: crate::file_metadata::Metadata,
}
// Directory identity is independent of display metadata. A fresh scan can
// update timestamps without inventing a new parent or trusting saved values.
impl PartialEq for Directory {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.parent == other.parent
            && self.name == other.name
            && self.deleted == other.deleted
    }
}
impl Eq for Directory {}
#[derive(Clone)]
pub struct Catalog {
    nodes: HashMap<String, Directory>,
    counts: HashMap<String, usize>,
}
impl Default for Catalog {
    fn default() -> Self {
        Self {
            nodes: HashMap::from([(
                ROOT.into(),
                Directory {
                    id: ROOT.into(),
                    parent: None,
                    name: "根目录".into(),
                    deleted: false,
                    metadata: Default::default(),
                },
            )]),
            counts: HashMap::new(),
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderItem {
    #[serde(flatten)]
    directory: Directory,
    files: usize,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderPage {
    pub path: Vec<Directory>,
    pub items: Vec<FolderItem>,
    pub total: usize,
    pub offset: usize,
    pub unknown_files: usize,
    pub signature_files: usize,
    pub can_export_directory: bool,
}
impl Catalog {
    pub fn snapshot(&self) -> Vec<Directory> {
        let mut nodes: Vec<_> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        nodes
    }
    pub fn insert(&mut self, node: Directory) -> Result<(), String> {
        node.metadata.validate_snapshot()?;
        if let Some(old) = self.nodes.get_mut(&node.id) {
            return if old == &node {
                if node.metadata.directory_proof.is_some() {
                    old.metadata = node.metadata;
                }
                Ok(())
            } else {
                Err("同一目录身份出现冲突，无法确认父目录关系。".into())
            };
        }
        if node.id.is_empty()
            || node.id.len() > 128
            || node.id == UNKNOWN
            || node.id == SIGNATURES
            || node.name.is_empty()
            || node.name.len() > 1024
            || node.name.contains('\0')
            || self.nodes.len() >= MAX_RESULTS
            || node.parent.as_ref() == Some(&node.id)
        {
            return Err("目录元数据无效或超过数量上限。".into());
        }
        let parent = node.parent.as_deref().ok_or("目录缺少父节点。")?;
        if self.path(parent)?.len() >= MAX_DEPTH {
            return Err("目录层级超过 64 层。".into());
        }
        self.nodes.insert(node.id.clone(), node);
        Ok(())
    }
    pub(crate) fn insert_ext_journal(&mut self, node: Directory) -> Result<(), String> {
        // The directory scan may already have proved this same Ext inode and
        // captured its timestamps. The journal adds file evidence to it.
        if self.nodes.get(&node.id).is_some_and(|existing| {
            existing == &node && existing.metadata.directory_proof.is_some()
        }) {
            return Ok(());
        }
        self.insert(node)
    }
    pub fn path(&self, id: &str) -> Result<Vec<Directory>, String> {
        let mut at = Some(id);
        let mut path = Vec::new();
        while let Some(id) = at {
            let node = self.nodes.get(id).ok_or("目录不存在于当前扫描结果。")?;
            if path.len() >= MAX_DEPTH {
                return Err("目录链路循环或过深。".into());
            }
            path.push(node.clone());
            at = node.parent.as_deref();
        }
        path.reverse();
        Ok(path)
    }
    pub fn add_file(&mut self, candidate: &Candidate) {
        if matches!(candidate.locator, Locator::Signature { .. }) {
            *self.counts.entry(SIGNATURES.into()).or_default() += 1;
        }
        if let Some(id) = &candidate.directory {
            if let Ok(path) = self.path(id) {
                for dir in path {
                    *self.counts.entry(dir.id).or_default() += 1;
                }
                return;
            }
        }
        *self.counts.entry(UNKNOWN.into()).or_default() += 1;
    }
    fn contains(&self, child: Option<&str>, parent: &str, descendants: bool) -> bool {
        if parent == UNKNOWN {
            return child.is_none();
        }
        let mut at = child;
        for _ in 0..MAX_DEPTH {
            let Some(id) = at else {
                break;
            };
            if id == parent {
                return true;
            }
            if !descendants {
                break;
            }
            at = self.nodes.get(id).and_then(|d| d.parent.as_deref());
        }
        false
    }
    pub fn folders(&self, folder: Option<&str>, offset: usize) -> Result<FolderPage, String> {
        let id = folder.unwrap_or(ROOT);
        let path = if matches!(id, UNKNOWN | SIGNATURES) {
            vec![]
        } else {
            self.path(id)?
        };
        let mut items: Vec<_> = self
            .nodes
            .values()
            .filter(|n| n.parent.as_deref() == Some(id))
            .collect();
        items.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        let total = items.len();
        let offset = clamp_offset(offset, total);
        Ok(FolderPage {
            can_export_directory: false, // Filled by the scan owner with completion/mode context.
            path,
            total,
            offset,
            unknown_files: *self.counts.get(UNKNOWN).unwrap_or(&0),
            signature_files: *self.counts.get(SIGNATURES).unwrap_or(&0),
            items: items
                .into_iter()
                .skip(offset)
                .take(100)
                .map(|d| FolderItem {
                    directory: d.clone(),
                    files: *self.counts.get(&d.id).unwrap_or(&0),
                })
                .collect(),
        })
    }
}
pub fn clamp_offset(offset: usize, total: usize) -> usize {
    (offset / 100 * 100).min(total.saturating_sub(1) / 100 * 100)
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileState {
    #[default]
    All,
    Deleted,
    Existing,
    Unknown,
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileKind {
    #[default]
    All,
    Documents,
    Images,
    Videos,
    Audio,
    Archives,
    Other,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Filter {
    pub folder: Option<String>,
    pub descendants: bool,
    pub state: FileState,
    pub kind: FileKind,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub supported_only: bool,
    pub name_match: query::NameMatch,
    pub case_sensitive: bool,
    pub sort_by: query::SortBy,
    pub sort_direction: query::Direction,
    pub date_field: query::DateField,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
}
impl Filter {
    pub fn validate(&self, catalog: &Catalog) -> Result<(), String> {
        if let Some(folder) = &self.folder {
            if folder.len() > 128 {
                return Err("目录标识过长。".into());
            }
            if !matches!(folder.as_str(), UNKNOWN | SIGNATURES) {
                catalog.path(folder)?;
            }
        }
        if self.min_size.is_some_and(|n| n > 1 << 43)
            || self.max_size.is_some_and(|n| n > 1 << 43)
            || matches!((self.min_size,self.max_size), (Some(min),Some(max)) if min > max)
        {
            return Err("文件大小筛选范围无效。".into());
        }
        Ok(())
    }
    pub fn matches(&self, c: &Candidate, catalog: &Catalog) -> bool {
        let signature = matches!(c.locator, Locator::Signature { .. });
        let state = if signature {
            FileState::Unknown
        } else if c.deleted {
            FileState::Deleted
        } else {
            FileState::Existing
        };
        self.folder.as_ref().is_none_or(|f| {
            if f == SIGNATURES {
                signature
            } else {
                catalog.contains(c.directory.as_deref(), f, self.descendants)
            }
        }) && (self.state == FileState::All || self.state == state)
            && (self.kind == FileKind::All || self.kind == file_kind(&c.name))
            && self.min_size.is_none_or(|n| c.size >= n)
            && self.max_size.is_none_or(|n| c.size <= n)
            && (!self.supported_only || c.supported)
    }
}
pub fn select_ids(
    candidates: &[Candidate],
    catalog: &Catalog,
    query: &str,
    filter: &Filter,
) -> Result<Vec<u32>, String> {
    let query = query::Query::new(query, filter, catalog)?;
    let ids: Vec<_> = candidates
        .iter()
        .filter(|c| c.supported && query.matches(c, catalog))
        .take(10_001)
        .map(|c| c.id)
        .collect();
    if ids.len() > 10_000 {
        return Err("匹配文件超过 10000 个，请缩小筛选范围后选择。".into());
    }
    Ok(ids)
}
fn file_kind(name: &str) -> FileKind {
    match name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .as_deref()
    {
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico" | "cur"
            | "heic" | "svg" | "raw",
        ) => FileKind::Images,
        Some(
            "txt" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "csv" | "md" | "rtf"
            | "docm" | "xlsm" | "pptm" | "odt" | "ods" | "odp",
        ) => FileKind::Documents,
        Some("mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "mpeg") => FileKind::Videos,
        Some(
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "aif" | "aiff" | "aifc"
            | "mid" | "midi" | "dsf" | "dff",
        ) => FileKind::Audio,
        Some("zip" | "7z" | "rar" | "gz" | "bz2" | "xz" | "tar" | "zst") => FileKind::Archives,
        _ => FileKind::Other,
    }
}

/// Evidence from one NTFS directory record. Resolver keeps only validated
/// identities; missing/reused parents remain unknown, never guessed by name.
#[derive(Default)]
pub struct NtfsDirectory {
    pub sequence: u16,
    pub directory: bool,
    pub name: String,
    pub parent: (u64, u16),
    pub deleted: bool,
    pub metadata: crate::file_metadata::Metadata,
}
#[derive(Default)]
pub struct NtfsResolver {
    known: HashMap<(u64, u16), String>,
}
impl NtfsResolver {
    pub fn resolve(
        &mut self,
        reference: (u64, u16),
        mut read: impl FnMut(u64) -> Result<NtfsDirectory, String>,
        mut found: impl FnMut(Directory) -> Result<(), String>,
    ) -> Result<String, String> {
        let mut at = reference;
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        let mut parent;
        loop {
            if let Some(id) = self.known.get(&at) {
                parent = id.clone();
                break;
            }
            if !seen.insert(at) {
                return Err("父目录引用包含循环".into());
            }
            if pending.len() >= MAX_DEPTH {
                return Err("父目录链超过 64 层".into());
            }
            let d = read(at.0).map_err(|_| "父目录记录缺失或无法读取")?;
            if !d.directory || d.sequence != at.1 {
                return Err("父目录身份不匹配，记录可能已被复用".into());
            }
            if at.0 == 5 {
                if d.deleted {
                    return Err("根目录记录无效".into());
                }
                parent = ROOT.into();
                self.known.insert(at, parent.clone());
                break;
            }
            if d.name.is_empty() || d.name.len() > 1024 || d.name.contains('\0') {
                return Err("父目录名称无效".into());
            }
            let next = d.parent;
            pending.push((at, d));
            at = next;
        }
        for (reference, d) in pending.into_iter().rev() {
            if self.known.len() >= MAX_RESULTS {
                return Err("父目录索引达到数量上限".into());
            }
            let id = format!("ntfs-{}-{}", reference.0, reference.1);
            found(Directory {
                id: id.clone(),
                name: d.name,
                parent: Some(parent),
                deleted: d.deleted,
                metadata: d.metadata,
            })?;
            self.known.insert(reference, id.clone());
            parent = id;
        }
        Ok(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ntfs_depth_limit_counts_root_and_cached_ancestors() {
        fn metadata(record: u64) -> Result<NtfsDirectory, String> {
            Ok(NtfsDirectory {
                sequence: 1,
                directory: true,
                name: format!("directory-{record}"),
                parent: (record.saturating_sub(1).max(5), 1),
                deleted: false,
                metadata: Default::default(),
            })
        }
        let deepest = 5 + (MAX_DEPTH - 1) as u64;
        let mut resolver = NtfsResolver::default();
        let mut catalog = Catalog::default();
        let id = resolver
            .resolve((deepest, 1), metadata, |d| catalog.insert(d))
            .unwrap();
        assert_eq!(catalog.path(&id).unwrap().len(), MAX_DEPTH);
        assert!(resolver
            .resolve((deepest + 1, 1), metadata, |d| catalog.insert(d))
            .is_err());
        let mut fresh = NtfsResolver::default();
        let mut empty = Catalog::default();
        assert!(fresh
            .resolve((deepest + 1, 1), metadata, |d| empty.insert(d))
            .is_err());
        assert_eq!(empty.nodes.len(), 1);
    }
    fn dir(id: &str, parent: &str, name: &str) -> Directory {
        Directory {
            id: id.into(),
            parent: Some(parent.into()),
            name: name.into(),
            deleted: false,
            metadata: Default::default(),
        }
    }
    fn candidate(id: u32, directory: Option<&str>) -> Candidate {
        Candidate {
            metadata: crate::file_metadata::Metadata::default(),
            id,
            name: "文件.PDF".into(),
            size: 10,
            deleted: true,
            method: "exFAT 目录".into(),
            note: String::new(),
            supported: true,
            directory: directory.map(str::to_owned),
            directory_note: String::new(),
            locator: Locator::Ntfs {
                record: id as u64,
                sequence: 1,
                link: crate::ntfs_names::Name::fixture("文件.PDF"),
                compression: None,
                wof: None,
                stream: None,
            },
        }
    }
    #[test]
    fn identity_not_names_defines_tree_and_counts() {
        let mut catalog = Catalog::default();
        catalog.insert(dir("a", ROOT, "同名")).unwrap();
        catalog.insert(dir("b", ROOT, "同名")).unwrap();
        catalog.insert(dir("c", "a", "子目录")).unwrap();
        for item in [
            candidate(0, Some("a")),
            candidate(1, Some("c")),
            candidate(2, Some("b")),
            candidate(3, None),
        ] {
            catalog.add_file(&item);
        }
        let page = catalog.folders(None, 0).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(
            page.items.iter().map(|d| d.files).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(page.unknown_files, 1);
        assert!(catalog.contains(Some("c"), "a", true));
        assert!(!catalog.contains(Some("c"), "a", false));
        assert!(!catalog.contains(Some("b"), "a", true));
        assert_eq!(
            catalog
                .path("c")
                .unwrap()
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            [ROOT, "a", "c"]
        );
    }
    #[test]
    fn conflicting_missing_and_too_deep_directory_links_are_rejected() {
        let mut catalog = Catalog::default();
        catalog.insert(dir("a", ROOT, "A")).unwrap();
        assert!(catalog.insert(dir("a", ROOT, "B")).is_err());
        assert!(catalog.insert(dir("x", "missing", "X")).is_err());
        assert!(catalog.insert(dir("x", "x", "X")).is_err());
        let mut parent = "a".to_owned();
        for i in 2..MAX_DEPTH {
            let id = format!("d{i}");
            catalog.insert(dir(&id, &parent, "深目录")).unwrap();
            parent = id;
        }
        assert!(catalog.insert(dir("too-deep", &parent, "X")).is_err());
    }
    #[test]
    fn directories_are_paginated_without_losing_identity_or_overshooting() {
        let mut catalog = Catalog::default();
        for n in 0..201 {
            catalog
                .insert(dir(&format!("d{n:03}"), ROOT, "Same"))
                .unwrap();
        }
        assert_eq!(catalog.folders(None, 0).unwrap().items.len(), 100);
        assert_eq!(
            catalog.folders(None, 100).unwrap().items[0].directory.id,
            "d100"
        );
        let last = catalog.folders(None, usize::MAX).unwrap();
        assert_eq!(last.offset, 200);
        assert_eq!(last.items.len(), 1);
        assert_eq!(clamp_offset(500, 0), 0);
    }
    #[test]
    fn status_type_size_support_and_directory_filters_compose() {
        let mut catalog = Catalog::default();
        catalog.insert(dir("a", ROOT, "A")).unwrap();
        let file = candidate(0, Some("a"));
        let mut filter = Filter {
            folder: Some(ROOT.into()),
            descendants: true,
            state: FileState::Deleted,
            kind: FileKind::Documents,
            min_size: Some(10),
            max_size: Some(10),
            supported_only: true,
            ..Default::default()
        };
        assert!(filter.matches(&file, &catalog));
        filter.descendants = false;
        assert!(!filter.matches(&file, &catalog));
        filter.folder = Some("a".into());
        assert!(filter.matches(&file, &catalog));
        filter.state = FileState::Existing;
        assert!(!filter.matches(&file, &catalog));
        let mut signature = file.clone();
        signature.locator = Locator::Signature {
            content_hash: None,
            allocation_digest: None,
            offset: 0,
            header: vec![],
            footer: vec![],
        };
        signature.directory = None;
        filter.folder = Some(UNKNOWN.into());
        filter.state = FileState::Unknown;
        assert!(filter.matches(&signature, &catalog));
        assert!(!filter.matches(&file, &catalog));
        assert_eq!(file_kind("Photo.JPEG") == FileKind::Images, true);
        filter.min_size = Some(11);
        assert!(filter.validate(&catalog).is_err());
        filter.min_size = Some(u64::MAX);
        assert!(filter.validate(&catalog).is_err());
        assert!(serde_json::from_str::<Filter>(r#"{"path":"C:\\"}"#).is_err());
    }
    #[test]
    fn selection_spans_pages_but_never_silently_truncates() {
        let catalog = Catalog::default();
        let mut files: Vec<_> = (0..10_001).map(|i| candidate(i, Some(ROOT))).collect();
        assert!(select_ids(&files, &catalog, "", &Filter::default()).is_err());
        files[10000].supported = false;
        assert_eq!(
            select_ids(&files, &catalog, ".pdf", &Filter::default())
                .unwrap()
                .len(),
            10000
        );
        assert!(select_ids(&files, &catalog, &"x".repeat(201), &Filter::default()).is_err());
        assert!(
            select_ids(&files, &catalog, "not found", &Filter::default())
                .unwrap()
                .is_empty()
        );
    }
    fn record(record: u64, sequence: u16, parent: u64) -> NtfsDirectory {
        NtfsDirectory {
            sequence,
            directory: true,
            name: format!("folder-{record}"),
            parent: (parent, 1),
            deleted: false,
            metadata: Default::default(),
        }
    }
    #[test]
    fn ntfs_parent_sequence_and_missing_cycle_checks_do_not_invent_paths() {
        for mode in 0..5 {
            let mut resolver = NtfsResolver::default();
            let mut emitted = vec![];
            let result = resolver.resolve(
                (20, 1),
                |id| match mode {
                    0 => Err("missing".into()),
                    1 => Ok(record(id, 2, 5)),
                    2 => Ok(record(id, 1, 20)),
                    3 => Ok(NtfsDirectory {
                        directory: false,
                        ..record(id, 1, 5)
                    }),
                    _ => Ok(record(id, 1, id + 1)),
                },
                |d| {
                    emitted.push(d);
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(emitted.is_empty());
        }
    }
    #[test]
    fn ntfs_validated_ancestors_are_shared_without_merging_same_names() {
        let mut resolver = NtfsResolver::default();
        let mut catalog = Catalog::default();
        let id = resolver
            .resolve(
                (21, 1),
                |id| Ok(record(id, 1, if id == 21 { 20 } else { 5 })),
                |d| catalog.insert(d),
            )
            .unwrap();
        assert_eq!(id, "ntfs-21-1");
        assert_eq!(catalog.path(&id).unwrap().len(), 3);
        assert_eq!(
            resolver
                .resolve(
                    (21, 1),
                    |_| panic!("must use validated cache"),
                    |_| panic!("duplicate callback")
                )
                .unwrap(),
            id
        );
        assert!(resolver
            .resolve((21, 2), |id| Ok(record(id, 1, 5)), |_| Ok(()))
            .is_err());
    }
}

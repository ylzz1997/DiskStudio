//! Plan recovered file names and directory relationships using scan metadata only.
//! Source names never become paths. The executor also supports an in-memory sink.
use crate::{
    recovery::Candidate,
    recovery_tree::{Catalog, ROOT},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Layout {
    #[default]
    Tree,
    Flat,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub parent: Option<usize>,
    pub original_name: String,
    pub name: String,
    pub unknown: bool,
    #[serde(skip)]
    pub source: Option<crate::recovery_tree::Directory>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: u32,
    pub folder: usize,
    pub original_name: String,
    pub name: String,
    pub partial: String,
    #[serde(serialize_with = "decimal_size")]
    pub size: u64,
}
fn decimal_size<S: serde::Serializer>(size: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&size.to_string())
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSource {
    pub archive_name: String,
    pub point_id: String,
    pub point_label: String,
    pub partition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found_extent: Option<crate::surface_scan::Range>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSource {
    pub file_name: String,
    pub format: String,
    pub partition: String,
    pub offset: String,
    pub size: String,
    pub sector: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallels_snapshot: Option<uuid::Uuid>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLvmSource {
    pub volume_group: String,
    pub sequence: u64,
    pub logical_volume: String,
    pub logical_volume_id: String,
    pub members: Vec<ImageLvmMember>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLvmMember {
    pub files: Vec<String>,
    pub format: String,
    pub offset: String,
    pub size: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalLvmSource {
    pub volume_group: String,
    pub sequence: u64,
    pub logical_volume: String,
    pub logical_volume_id: String,
    pub members: Vec<PhysicalLvmMember>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalLvmMember {
    pub disk_number: u32,
    pub partition: Option<u32>,
    pub offset: String,
    pub size: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualRaidSource {
    pub config: disk_core::virtual_raid::Config,
    pub members: Vec<VirtualRaidMember>,
    pub partition: ImageSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_filesystem: Option<String>,
    pub checked_rows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md_metadata: Option<MdRaidSource>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MdRaidSource {
    pub version: &'static str,
    pub array_uuid: String,
    pub events: String,
    pub fingerprint: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualRaidMember {
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<VirtualRaidDevice>,
    pub offset: String,
    pub size: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualRaidDevice {
    pub disk_number: u32,
    pub partition: Option<u32>,
    pub extent_offset: String,
    pub extent_size: String,
    pub sector: u64,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub options: crate::export_metadata::Options,
    pub layout: Layout,
    pub folders: Vec<Folder>,
    pub entries: Vec<Entry>,
    pub renamed: usize,
    pub unknown_files: usize,
    pub total_bytes: u64,
    pub directory_selection: bool,
    pub skipped: Vec<Skipped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_source: Option<BackupSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_source: Option<ImageSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_lvm_source: Option<ImageLvmSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_lvm_source: Option<PhysicalLvmSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_raid_source: Option<VirtualRaidSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ntfs_reconstruction: Option<crate::recovery::ntfs_reconstruct::Proof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fat_reconstruction: Option<crate::fat_recovery::reconstruct::Proof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ext_reconstruction: Option<crate::ext_recovery::reconstruct::Proof>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lvm_degraded_raid: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub id: u32,
    pub name: String,
    pub directories: Vec<String>,
    pub reason: &'static str,
}
pub const REPORT: &str = "DiskStudio-export.json";
pub const REPORT_PARTIAL: &str = "DiskStudio-export.json.partial";
const COMPONENT_LIMIT: usize = 180;

fn truncate(s: &str, limit: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= limit
        })
        .collect()
}
fn extension(s: &str) -> (&str, &str) {
    if let Some((stem, ext)) = s.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() && ext.encode_utf16().count() <= 24 {
            return (stem, &s[stem.len()..]);
        }
    }
    (s, "")
}
pub(crate) fn reserved(s: &str) -> bool {
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.'])
        .to_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|tail| {
            matches!(
                tail,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}
fn component(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim_matches(['.', ' ']);
    let clean = if clean.is_empty() { "未命名" } else { clean };
    let clean = if reserved(clean) {
        format!("_{clean}")
    } else {
        clean.into()
    };
    let (stem, ext) = extension(&clean);
    format!(
        "{}{ext}",
        truncate(stem, COMPONENT_LIMIT - ext.encode_utf16().count())
    )
    .trim_end_matches(['.', ' '])
    .into()
}
fn unique(name: &str, tag: &str, used: &mut HashSet<String>) -> String {
    let clean = component(name);
    if used.insert(clean.to_uppercase()) {
        return clean;
    }
    let (stem, ext) = extension(&clean);
    // The set is bounded by the plan's 100000 folders and 10000 files.
    for i in 1..=120_001 {
        let suffix = format!(" ({tag}-{i})");
        let candidate = format!(
            "{}{suffix}{ext}",
            truncate(
                stem,
                COMPONENT_LIMIT - suffix.encode_utf16().count() - ext.encode_utf16().count()
            )
        );
        if used.insert(candidate.to_uppercase()) {
            return candidate;
        }
    }
    unreachable!("bounded export namespace exhausted")
}
pub fn plan(files: &[Candidate], catalog: &Catalog, layout: Layout) -> Result<Plan, String> {
    plan_with_directories(files, catalog, layout, &[], vec![])
}
pub fn select(
    candidates: &[Candidate],
    catalog: &Catalog,
    file_ids: &[u32],
    folder: Option<&str>,
    layout: Layout,
) -> Result<(Vec<Candidate>, Plan), String> {
    if let Some(folder) = folder {
        if !file_ids.is_empty() || layout != Layout::Tree {
            return Err("目录选择必须单独按原目录结构保存。".into());
        }
        return plan_directory(candidates, catalog, folder);
    }
    if file_ids.is_empty() || file_ids.len() > 10_000 {
        return Err("请选择 1–10000 个文件或一个已读取目录。".into());
    }
    let ids: HashSet<_> = file_ids.iter().copied().collect();
    if ids.len() != file_ids.len() {
        return Err("文件选择重复。".into());
    }
    let files: Vec<_> = candidates
        .iter()
        .filter(|c| ids.contains(&c.id))
        .cloned()
        .collect();
    if files.len() != ids.len() {
        return Err("选择包含不存在的文件。".into());
    }
    let export = plan(&files, catalog, layout)?;
    Ok((files, export))
}

/// Expand an opaque catalogue identity, never a path or a client-provided file
/// list. Includes every observed descendant directory, even one without files.
pub fn plan_directory(
    candidates: &[Candidate],
    catalog: &Catalog,
    id: &str,
) -> Result<(Vec<Candidate>, Plan), String> {
    if id.len() > 128 {
        return Err("目录标识过长。".into());
    }
    catalog.path(id)?;
    let directories: Vec<_> = catalog
        .snapshot()
        .into_iter()
        .filter(|d| {
            catalog
                .path(&d.id)
                .is_ok_and(|p| p.iter().any(|n| n.id == id))
        })
        .map(|d| d.id)
        .collect();
    if directories.len() > 10_000 {
        return Err("每次最多导出 10000 个已读取目录，请选择较小的目录。".into());
    }
    let ids: HashSet<_> = directories.iter().map(String::as_str).collect();
    let mut files = vec![];
    let mut skipped = vec![];
    for c in candidates
        .iter()
        .filter(|c| c.directory.as_deref().is_some_and(|d| ids.contains(d)))
    {
        if files.len() + skipped.len() >= 10_000 {
            return Err("目录内已读取文件超过 10000 个，请选择较小的目录。".into());
        }
        if c.supported {
            files.push(c.clone());
        } else {
            skipped.push(Skipped {
                id: c.id,
                name: c.name.clone(),
                directories: catalog
                    .path(c.directory.as_deref().unwrap())?
                    .into_iter()
                    .skip(1)
                    .map(|d| d.name)
                    .collect(),
                reason: "当前文件系统引擎不支持提取此文件",
            });
        }
    }
    let export = plan_with_directories(&files, catalog, Layout::Tree, &directories, skipped)?;
    Ok((files, export))
}
fn plan_with_directories(
    files: &[Candidate],
    catalog: &Catalog,
    layout: Layout,
    directories: &[String],
    skipped: Vec<Skipped>,
) -> Result<Plan, String> {
    if (files.is_empty() && directories.is_empty()) || files.len() > 10_000 {
        return Err("请选择 1–10000 个文件。".into());
    }
    let mut ids = HashSet::new();
    let mut selected: Vec<_> = files.iter().collect();
    selected.sort_by_key(|c| c.id);
    let mut total_bytes = 0u64;
    let mut needed = HashMap::new();
    for id in directories {
        for (depth, d) in catalog.path(id)?.into_iter().enumerate().skip(1) {
            needed.insert(d.id.clone(), (depth, d));
        }
    }
    let mut unknown_files = 0;
    for c in &selected {
        if !ids.insert(c.id) || !c.supported || c.name.len() > 4096 {
            return Err("文件选择重复、不可提取或名称过长。".into());
        }
        total_bytes = total_bytes.checked_add(c.size).ok_or("文件总容量溢出。")?;
        if layout == Layout::Tree {
            if let Some(id) = &c.directory {
                for (depth, d) in catalog.path(id)?.into_iter().enumerate().skip(1) {
                    needed.insert(d.id.clone(), (depth, d));
                }
            } else {
                unknown_files += 1;
            }
        }
    }
    let mut nodes: Vec<_> = needed.into_values().collect();
    nodes.sort_by(|(ad, a), (bd, b)| ad.cmp(bd).then(a.id.cmp(&b.id)));
    let mut result = Plan {
        options: Default::default(),
        layout,
        folders: vec![Folder {
            parent: None,
            original_name: "根目录".into(),
            name: String::new(),
            unknown: false,
            source: None,
        }],
        entries: vec![],
        renamed: 0,
        unknown_files,
        total_bytes,
        directory_selection: !directories.is_empty(),
        skipped,
        backup_source: None,
        image_source: None,
        image_lvm_source: None,
        physical_lvm_source: None,
        virtual_raid_source: None,
        ntfs_reconstruction: None,
        fat_reconstruction: None,
        ext_reconstruction: None,
        lvm_degraded_raid: false,
    };
    let mut map = HashMap::from([(ROOT.to_string(), 0)]);
    let mut temporary: HashMap<String, HashSet<String>> = HashMap::new();
    for c in &selected {
        let parent = if layout == Layout::Flat {
            ROOT
        } else {
            c.directory.as_deref().unwrap_or("unknown")
        };
        temporary
            .entry(parent.into())
            .or_default()
            .insert(format!("DiskStudio-file-{:06}.partial", c.id).to_uppercase());
    }
    let mut root_names = temporary.remove(ROOT).unwrap_or_default();
    root_names.extend([REPORT.to_uppercase(), REPORT_PARTIAL.to_uppercase()]);
    let mut names = vec![root_names];
    let unknown = if unknown_files > 0 {
        names[0].insert("目录未知".into());
        result.folders.push(Folder {
            parent: Some(0),
            original_name: "目录未知".into(),
            name: "目录未知".into(),
            unknown: true,
            source: None,
        });
        names.push(temporary.remove("unknown").unwrap_or_default());
        1
    } else {
        0
    };
    for (_, d) in nodes {
        let parent = *map
            .get(d.parent.as_deref().ok_or("目录父节点缺失")?)
            .ok_or("目录父节点未计划")?;
        let index = result.folders.len();
        let name = unique(&d.name, &format!("目录-{index}"), &mut names[parent]);
        result.renamed += usize::from(name != d.name);
        result.folders.push(Folder {
            parent: Some(parent),
            original_name: d.name.clone(),
            name,
            unknown: false,
            source: Some(d.clone()),
        });
        names.push(temporary.remove(&d.id).unwrap_or_default());
        map.insert(d.id, index);
    }
    for c in selected {
        let parent = if layout == Layout::Flat {
            0
        } else {
            c.directory
                .as_ref()
                .and_then(|id| map.get(id))
                .copied()
                .unwrap_or(unknown)
        };
        let desired = if layout == Layout::Flat {
            format!("{:06}_{}", c.id, c.name)
        } else {
            c.name.clone()
        };
        let name = unique(&desired, &format!("文件-{}", c.id), &mut names[parent]);
        result.renamed += usize::from(name != c.name);
        result.entries.push(Entry {
            id: c.id,
            folder: parent,
            original_name: c.name.clone(),
            name,
            partial: format!("DiskStudio-file-{:06}.partial", c.id),
            size: c.size,
        });
    }
    // Validate report size before asking for an output folder or creating files.
    if result.manifest(&[], false)?.len() > 12 * 1024 * 1024 {
        return Err("名称映射超过 12 MiB 上限，请减少本次选择。".into());
    }
    Ok(result)
}
impl Plan {
    pub fn chain(&self, folder: usize) -> Result<Vec<usize>, String> {
        let mut chain = vec![];
        let mut at = folder;
        while at != 0 {
            if chain.len() >= 64 {
                return Err("导出目录过深。".into());
            }
            let d = self.folders.get(at).ok_or("导出目录不存在")?;
            chain.push(at);
            let parent = d.parent.ok_or("导出目录链路不完整")?;
            if parent >= at {
                return Err("导出目录链路无效".into());
            }
            at = parent;
        }
        chain.reverse();
        Ok(chain)
    }
    pub fn description(&self, locale: &crate::native_locale::Locale) -> String {
        let layout = match self.layout {
            Layout::Tree => locale.format(
                "按已确认的原目录结构导出，创建 {folders} 个子目录；{unknown} 个目录未知的文件单独归组，{renamed} 个文件或目录名称因冲突或兼容性调整。",
                &[
                    ("folders", &(self.folders.len() - 1).to_string()),
                    ("unknown", &self.unknown_files.to_string()),
                    ("renamed", &self.renamed.to_string()),
                ],
            ),
            Layout::Flat => locale.text("所选文件保存在同一目录，以编号避免同名覆盖。").to_owned(),
        };
        let scope = if self.directory_selection {
            locale.format(
                "包含该目录下已读取的 {files} 个可提取文件及所有已读取子目录（包括空目录），不受当前筛选影响；跳过 {skipped} 个不支持的文件并记录在清单中。尚未读取到的内容不会导出，未承诺完整目录。",
                &[
                    ("files", &self.entries.len().to_string()),
                    ("skipped", &self.skipped.len().to_string()),
                ],
            )
        } else {
            locale
                .text("仅包含所选文件及其父目录，不含未选文件或独立空目录。")
                .to_owned()
        };
        let preserve = |enabled| locale.text(if enabled { "保留" } else { "不保留" });
        let options = locale.format(
            "并发任务 {workers} · 文件与目录时间 {timestamps} · 受支持属性 {attributes}。未知或无法保留的字段在结果清单中列明。",
            &[
                ("workers", &self.options.workers.to_string()),
                ("timestamps", preserve(self.options.timestamps)),
                ("attributes", preserve(self.options.attributes)),
            ],
        );
        [layout, scope, options].join(" ")
    }
    pub fn manifest(&self, outcomes: &[Outcome], completed: bool) -> Result<Vec<u8>, String> {
        self.manifest_with_folders(outcomes, &[], completed)
    }
    fn manifest_with_folders(
        &self,
        outcomes: &[Outcome],
        directories: &[FolderOutcome],
        completed: bool,
    ) -> Result<Vec<u8>, String> {
        struct Limited(Vec<u8>);
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len() + bytes.len() > 32 * 1024 * 1024 {
                    return Err(std::io::Error::other("导出清单超过 32 MiB 上限"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Report<'a> {
            version: u32,
            options: crate::export_metadata::Options,
            #[serde(skip_serializing_if = "Option::is_none")]
            backup_source: Option<&'a BackupSource>,
            #[serde(skip_serializing_if = "Option::is_none")]
            image_source: Option<&'a ImageSource>,
            #[serde(skip_serializing_if = "Option::is_none")]
            image_lvm_source: Option<&'a ImageLvmSource>,
            #[serde(skip_serializing_if = "Option::is_none")]
            physical_lvm_source: Option<&'a PhysicalLvmSource>,
            #[serde(skip_serializing_if = "Option::is_none")]
            virtual_raid_source: Option<&'a VirtualRaidSource>,
            #[serde(skip_serializing_if = "Option::is_none")]
            ntfs_reconstruction: Option<&'a crate::recovery::ntfs_reconstruct::Proof>,
            #[serde(skip_serializing_if = "Option::is_none")]
            fat_reconstruction: Option<&'a crate::fat_recovery::reconstruct::Proof>,
            #[serde(skip_serializing_if = "Option::is_none")]
            ext_reconstruction: Option<&'a crate::ext_recovery::reconstruct::Proof>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            lvm_degraded_raid: bool,
            completed: bool,
            saved_files: usize,
            failed_files: usize,
            unprocessed_files: usize,
            created_directories: usize,
            failed_directories: usize,
            unprocessed_directories: usize,
            directory_outcomes: &'a [FolderOutcome],
            skipped_files: &'a [Skipped],
            directory_selection: bool,
            layout: Layout,
            folders: &'a [Folder],
            entries: &'a [Entry],
            outcomes: &'a [Outcome],
            note: &'static str,
        }
        let mut bytes = Limited(vec![]);
        serde_json::to_writer(&mut bytes, &Report {
            version: 3, options:self.options, completed, layout:self.layout, backup_source: self.backup_source.as_ref(), image_source: self.image_source.as_ref(),
            image_lvm_source: self.image_lvm_source.as_ref(),
            physical_lvm_source: self.physical_lvm_source.as_ref(),
            virtual_raid_source: self.virtual_raid_source.as_ref(),
            ntfs_reconstruction: self.ntfs_reconstruction.as_ref(),
            fat_reconstruction: self.fat_reconstruction.as_ref(),
            ext_reconstruction: self.ext_reconstruction.as_ref(),
            lvm_degraded_raid: self.lvm_degraded_raid,
            saved_files: outcomes.iter().filter(|o| o.saved).count(),
            failed_files: outcomes.iter().filter(|o| !o.saved).count(),
            unprocessed_files: self.entries.len().saturating_sub(outcomes.len()),
            created_directories: directories.iter().filter(|d| d.created).count(),
            failed_directories: directories.iter().filter(|d| !d.created).count(),
            unprocessed_directories: self.folders.len().saturating_sub(1 + directories.len()),
            directory_outcomes: directories, skipped_files: &self.skipped, directory_selection: self.directory_selection,
            folders: &self.folders, entries: &self.entries, outcomes,
            note: "目录选择包含已读取的子目录与可提取文件，文件选择仅包含所选文件及父目录。受支持文件与目录的时间、属性按 options 尝试保存，逐字段记录实际结果；未知时区不推算。目录属性在子项写入后保存；不重建权限、所有者、链接、变化时间或扩展属性。文件内容成功必须通过完整 SHA-256 回读，不证明已删除原始内容完整。completed 表示本次执行结束，请同时检查失败、跳过、未处理项及元数据报告。",
        }).map_err(|e| e.to_string())?;
        Ok(bytes.0)
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub id: u32,
    pub saved: bool,
    pub error: Option<String>,
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<crate::export_metadata::Report>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderOutcome {
    pub folder: usize,
    pub created: bool,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<crate::export_metadata::Report>,
}
pub struct BatchOutcome {
    pub files: Vec<Outcome>,
    pub directories: Vec<FolderOutcome>,
}
pub trait Sink {
    type Output: Write;
    fn directory(&mut self, plan: &Plan, index: usize) -> Result<(), String>;
    fn create(&mut self, plan: &Plan, entry: &Entry) -> Result<Self::Output, String>;
    fn commit(
        &mut self,
        entry: &Entry,
        output: Self::Output,
        tick: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<String, String>;
    fn report(&mut self, bytes: &[u8], completed: bool) -> Result<(), String>;
    fn metadata_report(&self, _id: u32) -> Option<crate::export_metadata::Report> {
        None
    }
    fn preserve_directory(
        &mut self,
        plan: &Plan,
        _index: usize,
        metadata: &crate::file_metadata::Metadata,
    ) -> crate::export_metadata::Report {
        crate::export_metadata::unmodified(
            metadata,
            plan.options,
            Some("输出端不支持目录属性保留。".into()),
        )
    }
}
pub fn execute<S: Sink>(
    plan: &Plan,
    sink: &mut S,
    mut extract: impl FnMut(&Entry, &mut S::Output) -> Result<u64, String>,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Vec<Outcome>, String> {
    tick(0)?;
    sink.report(&plan.manifest(&[], false)?, false)?;
    let mut outcomes = vec![];
    let mut directories = vec![];
    for index in 1..plan.folders.len() {
        let result = tick(0).and_then(|_| sink.directory(plan, index));
        let failure = result.as_ref().err().cloned();
        directories.push(FolderOutcome {
            folder: index,
            created: result.is_ok(),
            error: result.err().map(|s| truncate(&s, 256)),
            metadata: None,
        });
        if let Some(e) = failure {
            let report = sink.report(
                &plan.manifest_with_folders(&outcomes, &directories, false)?,
                false,
            );
            return Err(format!(
                "创建目录未完成：{e}；已保留部分目录及清单。{}",
                report.err().unwrap_or_default()
            ));
        }
    }
    let mut done = 0;
    for entry in &plan.entries {
        if let Err(e) = tick(done) {
            let report = sink.report(
                &plan.manifest_with_folders(&outcomes, &directories, false)?,
                false,
            );
            return Err(format!(
                "{e}；已保留未完成导出及清单。{}",
                report.err().unwrap_or_default()
            ));
        }
        let result = (|| {
            let mut output = sink.create(plan, entry)?;
            let copied = extract(entry, &mut output)?;
            if copied != entry.size {
                return Err("提取字节数与候选容量不符。".into());
            }
            tick(done + entry.size)?;
            sink.commit(entry, output, &mut || tick(done + entry.size))
        })();
        outcomes.push(Outcome {
            id: entry.id,
            metadata: sink.metadata_report(entry.id),
            saved: result.is_ok(),
            sha256: result.as_ref().ok().cloned(),
            error: result.err().map(|s| truncate(&s, 256)),
        });
        done += entry.size;
    }
    if let Err(e) = tick(done) {
        let report = sink.report(
            &plan.manifest_with_folders(&outcomes, &directories, false)?,
            false,
        );
        return Err(format!(
            "{e}；导出未完成。{}",
            report.err().unwrap_or_default()
        ));
    }
    sink.report(
        &plan.manifest_with_folders(&outcomes, &directories, true)?,
        true,
    )?;
    Ok(outcomes)
}
#[path = "recovery_parallel.rs"]
mod parallel;
pub use parallel::{execute_parallel, execute_parallel_with_directories};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recovery::{self, Locator, ScanMode},
        recovery_tree::Directory,
    };
    use std::{
        cell::{Cell, RefCell},
        io::{Cursor, Read},
        rc::Rc,
        sync::{atomic::AtomicBool, Arc},
    };
    fn file(id: u32, name: &str, folder: Option<&str>) -> Candidate {
        Candidate {
            metadata: crate::file_metadata::Metadata::default(),
            id,
            name: name.into(),
            size: 5,
            deleted: true,
            method: "合成字节".into(),
            supported: true,
            note: String::new(),
            directory: folder.map(str::to_string),
            directory_note: String::new(),
            locator: Locator::Signature {
                content_hash: None,
                allocation_digest: None,
                offset: 0,
                header: b"hello".to_vec(),
                footer: b"hello".to_vec(),
            },
        }
    }
    fn dir(catalog: &mut Catalog, id: &str, parent: &str, name: &str) {
        catalog
            .insert(Directory {
                id: id.into(),
                parent: Some(parent.into()),
                name: name.into(),
                deleted: false,
                metadata: Default::default(),
            })
            .unwrap();
    }
    #[test]
    fn directory_selection_preserves_empty_descendants_and_reports_unsupported_files() {
        let mut catalog = Catalog::default();
        dir(&mut catalog, "parent", ROOT, "资料");
        dir(&mut catalog, "selected", "parent", "照片");
        dir(&mut catalog, "empty", "selected", "空目录");
        dir(&mut catalog, "nested-empty", "empty", "下级");
        dir(&mut catalog, "other", ROOT, "照片");
        let mut unsupported = file(2, "unsupported.bin", Some("empty"));
        unsupported.supported = false;
        let candidates = vec![
            file(1, "hello.txt", Some("selected")),
            unsupported,
            file(3, "outside", Some("other")),
            file(4, "unknown", None),
        ];
        let (files, p) =
            select(&candidates, &catalog, &[], Some("selected"), Layout::Tree).unwrap();
        assert_eq!(files.iter().map(|f| f.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(p.folders.len(), 5); // root, ancestor, selection, two descendants
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].id, 2);
        assert_eq!(p.skipped[0].directories, ["资料", "照片", "空目录"]);
        assert_eq!(path(&p, &p.entries[0]), "资料/照片/hello.txt");
        let english = crate::native_locale::Locale::new(crate::native_locale::Language::En);
        let description = p.description(&english);
        assert!(description.contains("with 4 subfolders"));
        assert!(description.contains("Exportable files 1"));
        assert!(description.contains("Skipped unsupported files 1"));
        assert!(description.contains("File and folder times Preserve"));
        assert!(select(&candidates, &catalog, &[1], Some("selected"), Layout::Tree).is_err());
        assert!(select(&candidates, &catalog, &[], Some("selected"), Layout::Flat).is_err());
        for bad in ["unknown", "missing", "../selected", "C:\\"] {
            assert!(select(&candidates, &catalog, &[], Some(bad), Layout::Tree).is_err());
        }
        assert!(select(&candidates, &catalog, &[1, 1], None, Layout::Tree).is_err());
        assert!(select(&candidates, &catalog, &[999], None, Layout::Tree).is_err());
        assert!(select(&candidates, &catalog, &[2], None, Layout::Tree).is_err());
        let (_, only_files) = select(&candidates, &catalog, &[1], None, Layout::Tree).unwrap();
        assert!(!only_files.directory_selection);
        assert_eq!(only_files.folders.len(), 3);
        let (_, empty_root) = plan_directory(&[], &Catalog::default(), ROOT).unwrap();
        assert_eq!(empty_root.folders.len(), 1);
        assert!(empty_root.entries.is_empty());
    }
    #[test]
    fn directory_expansion_enforces_limits_instead_of_silently_truncating() {
        let mut catalog = Catalog::default();
        let candidates: Vec<_> = (0..10_001).map(|id| file(id, "x", Some(ROOT))).collect();
        assert!(plan_directory(&candidates, &catalog, ROOT)
            .err()
            .unwrap()
            .contains("10000"));
        for n in 0..10_000 {
            dir(&mut catalog, &format!("d{n}"), ROOT, "目录");
        }
        assert!(plan_directory(&[], &catalog, ROOT)
            .err()
            .unwrap()
            .contains("10000"));
    }
    fn path(plan: &Plan, entry: &Entry) -> String {
        let mut names: Vec<_> = plan
            .chain(entry.folder)
            .unwrap()
            .iter()
            .map(|i| plan.folders[*i].name.clone())
            .collect();
        names.push(entry.name.clone());
        names.join("/")
    }
    #[test]
    fn preserves_selected_ancestry_separates_same_names_and_unknown_parent() {
        let mut catalog = Catalog::default();
        dir(&mut catalog, "a", ROOT, "资料");
        dir(&mut catalog, "b", ROOT, "资料");
        dir(&mut catalog, "c", "a", "图片");
        dir(&mut catalog, "empty", ROOT, "不应导出的空目录");
        dir(&mut catalog, "real-unknown", ROOT, "目录未知");
        let files = vec![
            file(4, "原名.txt", Some("b")),
            file(1, "原名.txt", Some("c")),
            file(2, "原名.txt", None),
            file(3, "原名.txt", Some("real-unknown")),
            file(5, "资料", Some(ROOT)),
        ];
        let p = plan(&files, &catalog, Layout::Tree).unwrap();
        assert_eq!(path(&p, &p.entries[0]), "资料/图片/原名.txt");
        assert_eq!(path(&p, &p.entries[1]), "目录未知/原名.txt");
        assert_ne!(p.entries[1].folder, p.entries[2].folder);
        assert_ne!(path(&p, &p.entries[0]), path(&p, &p.entries[3]));
        assert!(p.entries[4].name.starts_with("资料 (文件-"));
        assert_eq!(p.unknown_files, 1);
        assert!(!p.folders.iter().any(|d| d.name.contains("不应")));
        let mut reversed = files.clone();
        reversed.reverse();
        assert_eq!(
            p.manifest(&[], false).unwrap(),
            plan(&reversed, &catalog, Layout::Tree)
                .unwrap()
                .manifest(&[], false)
                .unwrap()
        );
    }
    #[test]
    fn windows_components_collisions_and_internal_names_never_escape_or_merge() {
        let mut catalog = Catalog::default();
        dir(&mut catalog, "temp", ROOT, "DiskStudio-file-000000.partial");
        dir(&mut catalog, "report", ROOT, REPORT);
        let long = format!("{}.txt", "🦀".repeat(150));
        let names = [
            "../..\\evil:stream",
            "CON.txt",
            "LPT¹.log",
            "conout$",
            "...",
            "a.txt",
            "A.TXT",
            "a?.txt",
            "a*.txt",
            REPORT,
            REPORT_PARTIAL,
            "DiskStudio-file-000011.partial",
            "x\0\n.txt",
            &long,
        ];
        let mut files: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(id, n)| file(id as u32, n, Some(ROOT)))
            .collect();
        files.extend([file(100, "x", Some("temp")), file(101, "y", Some("report"))]);
        let p = plan(&files, &catalog, Layout::Tree).unwrap();
        let mut unique = HashSet::new();
        for d in p.folders.iter().skip(1) {
            assert!(unique.insert((d.parent.unwrap(), d.name.to_uppercase())));
        }
        unique.insert((0, REPORT.to_uppercase()));
        unique.insert((0, REPORT_PARTIAL.to_uppercase()));
        for e in &p.entries {
            assert!(unique.insert((e.folder, e.name.to_uppercase())));
            assert!(unique.insert((e.folder, e.partial.to_uppercase())));
            assert!(e.name.encode_utf16().count() <= COMPONENT_LIMIT);
            assert!(!reserved(&e.name));
            assert!(!e
                .name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)));
            assert_eq!(std::path::Path::new(&e.name).components().count(), 1);
            assert!(!e.name.ends_with(['.', ' ']));
        }
        assert!(p
            .entries
            .iter()
            .find(|e| e.id == 13)
            .unwrap()
            .name
            .ends_with(".txt"));
        assert!(p.renamed >= 10);
    }
    #[test]
    fn flat_mode_validation_limits_and_exact_manifest_sizes() {
        let mut c = file(4, "text.txt", Some("missing"));
        assert!(plan(&[c.clone()], &Catalog::default(), Layout::Tree).is_err());
        let p = plan(&[c.clone()], &Catalog::default(), Layout::Flat).unwrap();
        assert_eq!(p.entries[0].name, "000004_text.txt");
        assert_eq!(p.folders.len(), 1);
        c.directory = Some(ROOT.into());
        c.size = u64::MAX;
        let p = plan(&[c.clone()], &Catalog::default(), Layout::Tree).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&p.manifest(&[], false).unwrap()).unwrap();
        assert_eq!(report["entries"][0]["size"], u64::MAX.to_string());
        assert!(report["entries"][0].get("locator").is_none());
        assert!(plan(&[c.clone(), c.clone()], &Catalog::default(), Layout::Tree).is_err());
        let mut second = c.clone();
        second.id = 5;
        assert!(plan(&[c, second], &Catalog::default(), Layout::Tree).is_err());
        assert!(plan(&[], &Catalog::default(), Layout::Tree).is_err());
        let mut bad = file(0, "bad", None);
        bad.supported = false;
        assert!(plan(&[bad], &Catalog::default(), Layout::Tree).is_err());
        let many: Vec<_> = (0..10001).map(|id| file(id, "same.txt", None)).collect();
        assert!(plan(&many, &Catalog::default(), Layout::Tree).is_err());
        let p = plan(&many[..10000], &Catalog::default(), Layout::Tree).unwrap();
        assert_eq!(
            p.entries
                .iter()
                .map(|e| e.name.to_uppercase())
                .collect::<HashSet<_>>()
                .len(),
            10000
        );
        let giant: Vec<_> = (0..4000)
            .map(|id| file(id, &"a".repeat(4096), None))
            .collect();
        assert!(plan(&giant, &Catalog::default(), Layout::Tree)
            .unwrap_err()
            .contains("12 MiB"));
    }
    #[test]
    fn backup_provenance_is_present_in_partial_and_final_reports_only_when_bound() {
        let mut p = plan(&[file(1, "a", None)], &Catalog::default(), Layout::Tree).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&p.manifest(&[], false).unwrap()).unwrap();
        assert!(report.get("backupSource").is_none());
        p.backup_source = Some(BackupSource {
            archive_name: "chain.dsbackup".into(),
            point_id: "old-point".into(),
            point_label: "最早完整备份".into(),
            partition: "分区 1".into(),
            found_extent: None,
        });
        for completed in [false, true] {
            let report: serde_json::Value =
                serde_json::from_slice(&p.manifest(&[], completed).unwrap()).unwrap();
            assert_eq!(
                report["backupSource"],
                serde_json::json!({
                    "archiveName":"chain.dsbackup", "pointId":"old-point",
                    "pointLabel":"最早完整备份", "partition":"分区 1"
                })
            );
            assert_eq!(report["completed"], completed);
        }
    }
    #[test]
    fn physical_lvm_export_records_every_selected_range_without_device_paths() {
        let mut plan = plan(&[file(1, "a", None)], &Catalog::default(), Layout::Tree).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&plan.manifest(&[], true).unwrap()).unwrap();
        assert!(report.get("physicalLvmSource").is_none());
        plan.physical_lvm_source = Some(PhysicalLvmSource {
            volume_group: "vg".into(),
            sequence: 3,
            logical_volume: "history".into(),
            logical_volume_id: "lv-id".into(),
            members: vec![
                PhysicalLvmMember {
                    disk_number: 1,
                    partition: Some(4),
                    offset: "8192".into(),
                    size: "4096".into(),
                },
                PhysicalLvmMember {
                    disk_number: 2,
                    partition: None,
                    offset: "0".into(),
                    size: "8192".into(),
                },
            ],
        });
        for completed in [false, true] {
            let report: serde_json::Value =
                serde_json::from_slice(&plan.manifest(&[], completed).unwrap()).unwrap();
            assert_eq!(
                report["physicalLvmSource"],
                serde_json::json!({
                    "volumeGroup": "vg", "sequence": 3, "logicalVolume": "history", "logicalVolumeId": "lv-id",
                    "members": [
                        {"diskNumber":1, "partition":4, "offset":"8192", "size":"4096"},
                        {"diskNumber":2, "partition":null, "offset":"0", "size":"8192"}
                    ]
                })
            );
        }
    }
    struct MemoryWriter {
        bytes: Rc<RefCell<Vec<u8>>>,
        fail_after: Option<usize>,
    }
    impl Write for MemoryWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let mut bytes = self.bytes.borrow_mut();
            let n = if let Some(limit) = self.fail_after {
                if bytes.len() >= limit {
                    return Err(std::io::Error::other("injected write failure"));
                }
                b.len().min(limit - bytes.len())
            } else {
                b.len()
            };
            bytes.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[derive(Default)]
    struct Memory {
        partials: HashMap<u32, Rc<RefCell<Vec<u8>>>>,
        saved: HashMap<u32, Vec<u8>>,
        report: Vec<u8>,
        completed: bool,
        fail_create: Option<u32>,
        fail_write: Option<u32>,
        fail_commit: Option<u32>,
        fail_report: bool,
        folders: Vec<usize>,
        fail_folder: Option<usize>,
    }
    impl Sink for Memory {
        type Output = MemoryWriter;
        fn directory(&mut self, _: &Plan, index: usize) -> Result<(), String> {
            if self.fail_folder == Some(index) {
                return Err("injected folder failure".into());
            }
            self.folders.push(index);
            Ok(())
        }
        fn create(&mut self, _: &Plan, e: &Entry) -> Result<Self::Output, String> {
            if self.fail_create == Some(e.id) {
                return Err("injected directory failure".into());
            }
            let bytes = Rc::new(RefCell::new(vec![]));
            assert!(self.partials.insert(e.id, bytes.clone()).is_none());
            Ok(MemoryWriter {
                bytes,
                fail_after: if self.fail_write == Some(e.id) {
                    Some(2)
                } else {
                    None
                },
            })
        }
        fn commit(
            &mut self,
            e: &Entry,
            w: MemoryWriter,
            tick: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<String, String> {
            tick()?;
            if self.fail_commit == Some(e.id) || self.saved.contains_key(&e.id) {
                return Err("injected finalize/no-overwrite failure".into());
            }
            self.saved.insert(e.id, w.bytes.borrow().clone());
            self.partials.remove(&e.id);
            use sha2::{Digest, Sha256};
            Ok(format!("{:x}", Sha256::digest(&*w.bytes.borrow())))
        }
        fn report(&mut self, b: &[u8], completed: bool) -> Result<(), String> {
            if self.fail_report {
                return Err("injected report failure".into());
            }
            self.report = b.to_vec();
            self.completed = completed;
            Ok(())
        }
    }
    fn copy_fixture(_: &Entry, w: &mut MemoryWriter) -> Result<u64, String> {
        w.write_all(b"hello").map_err(|e| e.to_string())?;
        Ok(5)
    }
    #[test]
    fn empty_directory_execution_reports_creation_failure_and_cancellation_precisely() {
        let mut catalog = Catalog::default();
        dir(&mut catalog, "parent", ROOT, "同名");
        dir(&mut catalog, "child", "parent", "空目录");
        dir(&mut catalog, "sibling", ROOT, "同名");
        let (_, p) = plan_directory(&[], &catalog, ROOT).unwrap();
        assert_eq!(p.renamed, 1);
        let mut m = Memory::default();
        let outcomes = execute(
            &p,
            &mut m,
            |_, _| panic!("empty directory must not extract content"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(outcomes.is_empty());
        assert_eq!(m.folders, vec![1, 2, 3]);
        let report: serde_json::Value = serde_json::from_slice(&m.report).unwrap();
        assert_eq!(report["createdDirectories"], 3);
        assert_eq!(report["unprocessedDirectories"], 0);
        assert_eq!(report["savedFiles"], 0);
        assert_eq!(report["completed"], true);
        let mut m = Memory {
            fail_folder: Some(2),
            ..Memory::default()
        };
        assert!(execute(&p, &mut m, |_, _| panic!(), |_| Ok(()))
            .unwrap_err()
            .contains("folder failure"));
        assert_eq!(m.folders, vec![1]);
        let report: serde_json::Value = serde_json::from_slice(&m.report).unwrap();
        assert_eq!(report["createdDirectories"], 1);
        assert_eq!(report["failedDirectories"], 1);
        assert_eq!(report["unprocessedDirectories"], 1);
        assert_eq!(report["completed"], false);
        let mut m = Memory::default();
        let mut ticks = 0;
        assert!(execute(
            &p,
            &mut m,
            |_, _| panic!(),
            |_| {
                ticks += 1;
                if ticks == 3 {
                    Err("cancel directories".into())
                } else {
                    Ok(())
                }
            }
        )
        .unwrap_err()
        .contains("cancel directories"));
        assert_eq!(m.folders, vec![1]);
        assert!(!m.completed);
    }
    #[test]
    fn executor_records_exact_bytes_and_initial_and_final_mapping_without_source_paths() {
        let files = [file(0, "你好.txt", Some(ROOT)), file(1, "hello.txt", None)];
        let p = plan(&files, &Catalog::default(), Layout::Tree).unwrap();
        let mut memory = Memory::default();
        let outcomes = execute(&p, &mut memory, copy_fixture, |_| Ok(())).unwrap();
        assert!(outcomes.iter().all(|o| o.saved));
        assert!(memory.completed);
        assert!(memory.partials.is_empty());
        assert_eq!(memory.saved[&0], b"hello");
        assert_eq!(memory.saved[&1], b"hello");
        let report: serde_json::Value = serde_json::from_slice(&memory.report).unwrap();
        assert_eq!(report["folders"][1]["unknown"], true);
        assert_eq!(report["outcomes"][0]["saved"], true);
        assert!(!String::from_utf8(memory.report)
            .unwrap()
            .contains("locator"));
    }
    #[test]
    fn executor_continues_individual_failures_preserves_partials_and_never_overwrites() {
        let files: Vec<_> = (0..6).map(|id| file(id, "file.txt", Some(ROOT))).collect();
        let p = plan(&files, &Catalog::default(), Layout::Tree).unwrap();
        let mut memory = Memory {
            fail_create: Some(0),
            fail_write: Some(1),
            fail_commit: Some(2),
            ..Memory::default()
        };
        memory.saved.insert(3, b"existing".to_vec());
        let outcomes = execute(
            &p,
            &mut memory,
            |e, w| {
                if e.id == 4 {
                    w.write_all(b"hi").unwrap();
                    Ok(2)
                } else {
                    copy_fixture(e, w)
                }
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(outcomes.iter().filter(|o| o.saved).count(), 1);
        assert_eq!(memory.saved[&3], b"existing");
        assert_eq!(memory.saved[&5], b"hello");
        assert_eq!(*memory.partials[&1].borrow(), b"he");
        assert_eq!(*memory.partials[&2].borrow(), b"hello");
        assert_eq!(*memory.partials[&4].borrow(), b"hi");
        assert!(memory.completed);
        let report: serde_json::Value = serde_json::from_slice(&memory.report).unwrap();
        assert_eq!(report["outcomes"][4]["saved"], false);
    }
    #[test]
    fn executor_cancellation_and_report_failure_never_claim_completion() {
        let p = plan(
            &[file(0, "a", None), file(1, "b", None)],
            &Catalog::default(),
            Layout::Tree,
        )
        .unwrap();
        let mut m = Memory::default();
        assert!(execute(&p, &mut m, |_, _| panic!(), |_| Err("cancel".into())).is_err());
        assert!(m.report.is_empty());
        assert!(m.partials.is_empty());
        let cancel = Cell::new(false);
        let error = execute(
            &p,
            &mut m,
            |e, w| {
                let n = copy_fixture(e, w)?;
                cancel.set(true);
                Ok(n)
            },
            |_| {
                if cancel.get() {
                    Err("cancel".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(error.contains("cancel"));
        assert!(m.saved.is_empty());
        assert!(!m.completed);
        assert_eq!(m.partials.len(), 1);
        let report: serde_json::Value = serde_json::from_slice(&m.report).unwrap();
        assert_eq!(report["completed"], false);
        let mut m = Memory {
            fail_report: true,
            ..Memory::default()
        };
        assert!(execute(&p, &mut m, |_, _| panic!(), |_| Ok(()))
            .unwrap_err()
            .contains("report failure"));
    }
    #[test]
    fn actual_ntfs_scan_and_extraction_feed_tree_executor_in_memory() {
        let image = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").as_slice();
        let mut files = vec![];
        let mut catalog = Catalog::default();
        recovery::scan_filesystem_owned(
            Cursor::new(image),
            ScanMode::AllFiles,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |c| files.push(c),
            |d| catalog.insert(d),
        )
        .unwrap();
        let c = files.iter().find(|c| c.name == "file-with-12345").unwrap();
        let p = plan(&[c.clone()], &catalog, Layout::Tree).unwrap();
        let mut memory = Memory::default();
        let outcomes = execute(
            &p,
            &mut memory,
            |_, w| {
                recovery::extract_owned(
                    Cursor::new(image),
                    c,
                    w,
                    u64::MAX,
                    Arc::new(AtomicBool::new(false)),
                    |_| Ok(()),
                )
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(outcomes[0].saved);
        assert_eq!(memory.saved[&c.id], b"12345");
        let mut check = Cursor::new(memory.saved[&c.id].clone());
        let mut bytes = vec![];
        check.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"12345");
    }
}

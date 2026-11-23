//! Read-only, explicitly configured RAID views over retained images or checked devices.
use super::*;
use crate::{
    image_browser::{Browser, Info, Kind, Options},
    lost_partitions, surface_scan,
};
use disk_core::{
    md_raid::Assembly,
    virtual_raid::{Config, Range},
};
#[path = "virtual_raid_search.rs"]
mod lost;
#[cfg(windows)]
use crate::data_windows::{self, Resolved, StableRead};
pub use lost::{browse_found, search, BrowseFoundRequest, Search, SearchRequest};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberRequest {
    file: Option<String>,
    range: Range,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRequest {
    id: String,
    #[serde(default)]
    replaces: Option<String>,
    config: Config,
    options: Options,
    #[serde(default)]
    open_without_table: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    md_fingerprint: Option<String>,
    members: Vec<MemberRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetectRequest {
    id: String,
    #[serde(default)]
    replaces: Option<String>,
    sector: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    version: &'static str,
    config: Config,
    members: Vec<MemberRequest>,
    fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalRequest {
    members: Vec<Selection>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputInfo {
    id: String,
    name: String,
    size: u64,
}

#[derive(Clone, Serialize)]
pub struct DraftInfo {
    id: String,
    files: Vec<InputInfo>,
}

#[derive(Serialize)]
pub struct Edit {
    sources: DraftInfo,
    request: OpenRequest,
}

#[cfg(windows)]
pub(super) struct Draft {
    info: DraftInfo,
    inputs: Inputs,
}

#[cfg(windows)]
impl Draft {
    fn lock(&self) -> Result<Vec<StableRead>, String> {
        match &self.inputs {
            Inputs::Images(files) => {
                files.validate()?;
                Ok(Vec::new())
            }
            Inputs::Physical(members) => members
                .iter()
                .map(|member| StableRead::lock(&member.selection))
                .collect(),
        }
    }

    fn inspect_md(&self, locks: &[StableRead], sector: u64) -> Result<Assembly, String> {
        let mut readers = match &self.inputs {
            Inputs::Images(files) => {
                files.validate()?;
                files.metadata_readers()?
            }
            Inputs::Physical(members) => {
                if locks.len() != members.len() {
                    return Err("RAID 物理成员的卷锁凭据不完整。".into());
                }
                members
                    .iter()
                    .zip(locks)
                    .map(|(member, lock)| {
                        if member.device.sector > sector {
                            return Err("逻辑扇区不能小于物理成员的逻辑扇区。".into());
                        }
                        let resolved = lock.resolved()?;
                        member.verify(&resolved)?;
                        let start = physical_range(
                            resolved.offset,
                            resolved.size,
                            member.device.sector,
                            Range { start: 0, size: resolved.size },
                        )?;
                        data_io::Region::for_metadata(
                            data_windows::device(&resolved, false)?,
                            start,
                            resolved.size,
                        )
                        .map_err(|e| e.to_string())
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
        };
        let result = disk_core::md_raid::inspect(&mut readers, sector)?;
        for lock in locks {
            lock.verify()?;
        }
        Ok(result)
    }

    fn request_members(&self, members: &[(Option<usize>, Range)]) -> Vec<MemberRequest> {
        members
            .iter()
            .map(|&(index, range)| MemberRequest {
                file: index.map(|index| self.info.files[index].id.clone()),
                range,
            })
            .collect()
    }
}

#[cfg(windows)]
enum Inputs {
    Images(Arc<crate::image_files::Group>),
    Physical(Vec<PhysicalMember>),
}

#[cfg(windows)]
struct PhysicalMember {
    selection: Selection,
    device: recovery_export::VirtualRaidDevice,
}

#[cfg(windows)]
impl PhysicalMember {
    fn identify(selection: Selection, resolved: &Resolved) -> Result<Self, String> {
        Ok(Self {
            device: Self::geometry(&selection, resolved)?,
            selection,
        })
    }

    fn geometry(
        selection: &Selection,
        resolved: &Resolved,
    ) -> Result<recovery_export::VirtualRaidDevice, String> {
        Ok(recovery_export::VirtualRaidDevice {
            disk_number: resolved.disk.number,
            partition: selection.partition,
            extent_offset: resolved.offset.to_string(),
            extent_size: resolved.size.to_string(),
            sector: resolved
                .disk
                .logical_sector_size
                .ok_or("缺少设备扇区大小")?,
        })
    }

    fn verify(&self, resolved: &Resolved) -> Result<(), String> {
        if Self::geometry(&self.selection, resolved)? != self.device {
            return Err("RAID 物理成员的设备编号或范围已变化，请重新选择。".into());
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(flatten)]
    pub info: Info,
    pub checked_rows: usize,
    pub missing_members: usize,
    pub md_version: Option<&'static str>,
    pub search: Option<Search>,
    pub scan_id: Option<String>,
    pub scan_mode: Option<ScanMode>,
}

pub(super) struct Saved {
    info: Info,
    array: Array,
    open_without_table: bool,
    checked_rows: usize,
    missing_members: usize,
}

struct Array {
    config: Config,
    md: Option<Assembly>,
    #[cfg(windows)]
    draft: Arc<Draft>,
    #[cfg(windows)]
    members: Vec<(Option<usize>, Range)>,
}

impl Array {
    fn physical(&self) -> bool {
        #[cfg(windows)]
        return matches!(self.draft.inputs, Inputs::Physical(_));
        #[cfg(not(windows))]
        false
    }

    fn validate(&self) -> Result<(), String> {
        #[cfg(windows)]
        match &self.draft.inputs {
            Inputs::Images(files) => files.validate()?,
            Inputs::Physical(members) => {
                for member in members {
                    member.verify(&data_windows::resolve(&member.selection, false, false)?)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn lock(&self) -> Result<Vec<StableRead>, String> {
        self.draft.lock()
    }

    #[cfg(windows)]
    fn reader(
        &self,
        locks: &[StableRead],
    ) -> Result<disk_core::virtual_raid::Reader<data_io::Region<File>>, String> {
        if let Some(expected) = &self.md {
            if &self.draft.inspect_md(locks, self.config.sector)? != expected {
                return Err("Linux md 成员元数据已变化，请重新识别阵列。".into());
            }
        }
        self.reader_verified(locks)
    }

    #[cfg(windows)]
    fn reader_verified(
        &self,
        locks: &[StableRead],
    ) -> Result<disk_core::virtual_raid::Reader<data_io::Region<File>>, String> {
        use disk_core::virtual_raid::Member;
        let members = match &self.draft.inputs {
            Inputs::Images(files) => {
                files.validate()?;
                let mut readers: Vec<_> = files.raw_readers()?.into_iter().map(Some).collect();
                self.members
                    .iter()
                    .map(|&(index, range)| {
                        let reader = index
                            .map(|index| readers[index].take().ok_or("RAID 成员不能重复。"))
                            .transpose()?;
                        Ok(Member { reader, range })
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            Inputs::Physical(devices) => {
                if locks.len() != devices.len() {
                    return Err("RAID 物理成员的卷锁凭据不完整。".into());
                }
                self.members
                    .iter()
                    .map(|&(index, range)| {
                        let Some(index) = index else {
                            return Ok(Member {
                                reader: None,
                                range,
                            });
                        };
                        let device = &devices[index];
                        if range.start.checked_rem(self.config.sector) != Some(0) {
                            return Err("RAID 成员、范围或条带参数无效。".into());
                        }
                        let resolved = locks[index].resolved()?;
                        device.verify(&resolved)?;
                        let start = physical_range(
                            resolved.offset,
                            resolved.size,
                            device.device.sector,
                            range,
                        )?;
                        let reader = data_io::Region::new(
                            data_windows::device(&resolved, false)?,
                            start,
                            range.size,
                        )
                        .map_err(|e| e.to_string())?;
                        Ok(Member {
                            reader: Some(reader),
                            range: Range { start: 0, ..range },
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
        };
        disk_core::virtual_raid::Reader::open(self.config, members)
    }
}

fn physical_range(base: u64, size: u64, sector: u64, range: Range) -> Result<u64, String> {
    if !matches!(sector, 512 | 4096)
        || base % sector != 0
        || range.start % sector != 0
        || range.size == 0
        || range.size % sector != 0
        || range
            .start
            .checked_add(range.size)
            .is_none_or(|end| end > size)
    {
        return Err("RAID 成员范围超出设备边界或未按设备扇区对齐。".into());
    }
    base.checked_add(range.start)
        .filter(|start| start.checked_add(range.size).is_some())
        .ok_or_else(|| "RAID 成员范围超出设备边界或未按设备扇区对齐。".into())
}

fn status_in(s: &State) -> Option<Status> {
    let saved = s.virtual_raid.as_ref()?;
    let scan = s.scan.as_ref().filter(|(_, source)| {
        matches!(source, ScanSource::VirtualRaid(source) if Arc::ptr_eq(&source.saved, saved))
    });
    Some(Status {
        info: saved.info.clone(),
        checked_rows: saved.checked_rows,
        missing_members: saved.missing_members,
        md_version: saved.array.md.as_ref().map(|md| md.version),
        search: s.raid_search.as_ref().map(Search::read_only),
        scan_id: scan.map(|(id, _)| id.clone()),
        scan_mode: scan.map(|_| s.scan_mode),
    })
}

pub fn status() -> Option<Status> {
    status_in(&state())
}

pub fn configuration(id: String) -> Result<Edit, String> {
    let s = state();
    idle(&s)?;
    configuration_in(&s, &id)
}

fn configuration_in(s: &State, id: &str) -> Result<Edit, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        let saved = find(s, id)?;
        let sources = saved.array.draft.info.clone();
        Ok(Edit {
            request: OpenRequest {
                id: sources.id.clone(),
                replaces: Some(saved.info.id.clone()),
                config: saved.array.config,
                options: saved.info.options,
                open_without_table: saved.open_without_table,
                md_fingerprint: saved.array.md.as_ref().map(|md| md.fingerprint.clone()),
                members: saved.array.draft.request_members(&saved.array.members),
            },
            sources,
        })
    }
}

fn clear(s: &mut State) {
    if matches!(s.scan, Some((_, ScanSource::VirtualRaid(_)))) {
        s.scan = None;
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name.clear();
        s.recovery_progress = None;
        s.pending = None;
    }
    s.virtual_raid = None;
    s.raid_search = None;
}

pub fn select() -> Result<Option<DraftInfo>, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 原生文件选择。".into());
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let Some(files) = crate::image_files::choose_raw_members("选择 RAID 成员镜像")?
        else {
            return Ok(None);
        };
        let info = files.info();
        Ok(Some(retain(
            DraftInfo {
                id: info.id,
                files: info
                    .files
                    .into_iter()
                    .map(|file| InputInfo {
                        id: file.id,
                        name: file.name,
                        size: file.size,
                    })
                    .collect(),
            },
            Inputs::Images(files),
        )))
    }
}

#[cfg(windows)]
fn retain(info: DraftInfo, inputs: Inputs) -> DraftInfo {
    state().raid_draft = Some(Arc::new(Draft {
        info: info.clone(),
        inputs,
    }));
    info
}

fn validate_physical(sources: &[Selection]) -> Result<(), String> {
    if !(1..=disk_core::virtual_raid::MAX_MEMBERS).contains(&sources.len()) {
        return Err("请选择 1–64 个 RAID 物理成员。".into());
    }
    for (index, source) in sources.iter().enumerate() {
        if sources[..index].iter().any(|other| {
            other.target == source.target
                && (other.layout_key != source.layout_key
                    || other.partition == source.partition
                    || other.partition.is_none()
                    || source.partition.is_none())
        }) {
            return Err("RAID 物理成员重复、布局版本不同或范围重叠。".into());
        }
    }
    Ok(())
}

pub fn select_physical(request: PhysicalRequest) -> Result<DraftInfo, String> {
    validate_physical(&request.members)?;
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let mut info = DraftInfo {
            id: uuid::Uuid::new_v4().to_string(),
            files: Vec::with_capacity(request.members.len()),
        };
        let mut members = Vec::with_capacity(request.members.len());
        for selection in request.members {
            let resolved = data_windows::resolve(&selection, true, false)?;
            let member = PhysicalMember::identify(selection, &resolved)?;
            info.files.push(InputInfo {
                id: uuid::Uuid::new_v4().to_string(),
                name: resolved.label,
                size: resolved.size,
            });
            members.push(member);
        }
        Ok(retain(info, Inputs::Physical(members)))
    }
}

pub fn open(request: OpenRequest) -> Result<Status, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        let draft = {
            let s = state();
            idle(&s)?;
            open_draft(&s, &request.id, request.replaces.as_deref())?
        };
        if request.config.sector != request.options.sector
            || !(2..=disk_core::virtual_raid::MAX_MEMBERS).contains(&request.members.len())
            || (request.open_without_table && request.options.kind != Kind::Disk)
        {
            return Err("RAID 成员、范围或条带参数无效。".into());
        }
        let mut selected = HashSet::new();
        let members = request
            .members
            .iter()
            .map(|member| {
                let index = member
                    .file
                    .as_ref()
                    .map(|id| {
                        let index = draft
                            .info
                            .files
                            .iter()
                            .position(|file| &file.id == id)
                            .ok_or("RAID 成员选择已失效。")?;
                        if !selected.insert(index) {
                            return Err("RAID 成员不能重复。");
                        }
                        Ok(index)
                    })
                    .transpose()?;
                Ok((index, member.range))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if selected.len() != draft.info.files.len() {
            return Err("请为每个选中的来源指定一个成员位置。".into());
        }
        let missing_members = members.iter().filter(|(input, _)| input.is_none()).count();
        let mut array = Array {
            config: request.config,
            md: None,
            draft,
            members,
        };
        let locks = array.lock()?;
        if let Some(fingerprint) = request.md_fingerprint {
            let metadata = array.draft.inspect_md(&locks, array.config.sector)?;
            if metadata.fingerprint != fingerprint
                || metadata.config != array.config
                || metadata.members != array.members
            {
                return Err("Linux md 成员元数据已变化，请重新识别阵列。".into());
            }
            array.md = Some(metadata);
        }
        let mut reader = array.reader_verified(&locks)?;
        let checked_rows = reader.check_parity_samples().map_err(|e| e.to_string())?;
        let name = array.config.layout.name().to_owned();
        let mut info = if request.open_without_table {
            let size = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
            request.options.validate(size)?;
            Info {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                size,
                options: request.options,
                partitions: Vec::new(),
                warning: "已打开整组逻辑扇区，可搜索丢失分区。".into(),
                verification_notes: Vec::new(),
                parallels_snapshot: None,
                format: String::new(),
                files: Vec::new(),
            }
        } else {
            if request.options.kind == Kind::Partition {
                check_filesystem(&mut reader)?;
            }
            Browser::open(reader, name, request.options, || Ok(()))?.info
        };
        info.format = "RAID".into();
        info.files = array
            .draft
            .info
            .files
            .iter()
            .map(|file| file.name.clone())
            .collect();
        for lock in &locks {
            lock.verify()?;
        }
        if !array.physical() {
            array.validate()?;
        }
        let mut s = state();
        install(
            &mut s,
            Saved {
                info,
                array,
                open_without_table: request.open_without_table,
                checked_rows,
                missing_members,
            },
        );
        status_in(&s).ok_or_else(|| "RAID 成员选择已失效。".into())
    }
}

#[cfg(windows)]
fn open_draft(s: &State, id: &str, replaces: Option<&str>) -> Result<Arc<Draft>, String> {
    let current = s.virtual_raid.as_ref();
    if replaces != current.map(|saved| saved.info.id.as_str()) {
        return Err("当前 RAID 阵列已变化，请重新查看参数。".into());
    }
    current
        .map(|saved| &saved.array.draft)
        .filter(|draft| draft.info.id == id)
        .or_else(|| {
            s.raid_draft
                .as_ref()
                .filter(|draft| draft.info.id == id)
        })
        .cloned()
        .ok_or_else(|| "RAID 成员选择已失效。".into())
}

pub fn detect_md(request: DetectRequest) -> Result<Detection, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        if !matches!(request.sector, 512 | 4096) {
            return Err("RAID 成员、范围或条带参数无效。".into());
        }
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        let draft = {
            let s = state();
            idle(&s)?;
            open_draft(&s, &request.id, request.replaces.as_deref())?
        };
        let locks = draft.lock()?;
        let found = draft.inspect_md(&locks, request.sector)?;
        Ok(Detection {
            version: found.version,
            members: draft.request_members(&found.members),
            config: found.config,
            fingerprint: found.fingerprint,
        })
    }
}

#[cfg(windows)]
fn install(s: &mut State, saved: Saved) {
    clear(s);
    s.raid_draft = None;
    s.virtual_raid = Some(Arc::new(saved));
}

#[cfg(windows)]
fn check_filesystem(mut reader: impl Read + Seek) -> Result<(), String> {
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(|e| e.to_string())?;
    if &boot[3..11] == b"NTFS    " || recovery::fat_boot(&boot) {
        return Ok(());
    }
    let mut magic = [0; 2];
    reader
        .seek(SeekFrom::Start(1080))
        .map_err(|e| e.to_string())?;
    reader.read_exact(&mut magic).map_err(|e| e.to_string())?;
    let size = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if magic == [0x53, 0xef] {
        return crate::ext_recovery::validate_existing(reader);
    }
    if disk_core::btrfs::has_magic(&mut reader, size)? {
        disk_core::btrfs::open(reader, || Ok(()))?;
        return Ok(());
    }
    Err("未找到可识别的 RAID 文件系统，请核对成员顺序和参数。".into())
}

fn find(s: &State, id: &str) -> Result<Arc<Saved>, String> {
    s.virtual_raid
        .as_ref()
        .filter(|saved| saved.info.id == id)
        .cloned()
        .ok_or_else(|| "RAID 成员选择已失效。".into())
}

pub fn close(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    find(&s, &id)?;
    clear(&mut s);
    Ok(())
}

pub fn discard(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    #[cfg(windows)]
    if s.raid_draft
        .as_ref()
        .is_some_and(|draft| draft.info.id == id)
    {
        s.raid_draft = None;
    }
    Ok(())
}

#[derive(Clone)]
enum Scope {
    Partition(u32),
    Found {
        range: surface_scan::Range,
        candidate: Arc<lost_partitions::Candidate>,
    },
}

#[derive(Clone)]
pub(crate) struct Source {
    saved: Arc<Saved>,
    scope: Scope,
}

impl Source {
    pub(super) fn physical(&self) -> bool {
        self.saved.array.physical()
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        self.saved.array.validate()
    }

    #[cfg(windows)]
    pub(super) fn lock(&self) -> Result<Vec<StableRead>, String> {
        self.saved.array.lock()
    }

    #[cfg(windows)]
    pub(super) fn reader(
        &self,
        locks: &[StableRead],
        cancel: &AtomicBool,
    ) -> Result<Box<dyn backup_archive::Source>, String> {
        let saved = &self.saved;
        let reader = saved.array.reader(locks)?;
        match &self.scope {
            Scope::Found { range, candidate } => Ok(Box::new(lost_partitions::candidate_reader(
                reader,
                saved.info.size,
                saved.info.options.sector,
                *range,
                candidate,
                cancel,
            )?)),
            Scope::Partition(id) if self.physical() && saved.info.options.kind == Kind::Disk => {
                let browser =
                    Browser::open(reader, saved.info.name.clone(), saved.info.options, || {
                        if cancel.load(Ordering::Relaxed) {
                            Err("任务已取消。".into())
                        } else {
                            Ok(())
                        }
                    })?;
                if browser.info.partitions != saved.info.partitions {
                    return Err("RAID 逻辑分区布局已变化，请重新组装。".into());
                }
                Ok(Box::new(browser.reader(*id)?))
            }
            Scope::Partition(id) => {
                let part = self.partition(*id)?;
                Ok(Box::new(
                    data_io::Region::new(reader, part.start, part.size)
                        .map_err(|e| e.to_string())?,
                ))
            }
        }
    }

    fn partition(&self, id: u32) -> Result<&crate::image_browser::Part, String> {
        self.saved
            .info
            .partitions
            .iter()
            .find(|part| part.id == id)
            .ok_or_else(|| "RAID 分区选择已失效。".into())
    }

    pub(super) fn found_btrfs(&self) -> bool {
        matches!(&self.scope, Scope::Found { candidate, .. } if candidate.filesystem == "Btrfs")
    }

    #[cfg(windows)]
    pub(super) fn reconstruction(&self) -> Option<&crate::recovery::view::Proof> {
        match &self.scope {
            Scope::Found { candidate, .. } => candidate.reconstruction(),
            Scope::Partition(_) => None,
        }
    }

    fn partition_info(&self) -> Result<(String, u64, u64), String> {
        match &self.scope {
            Scope::Partition(id) => {
                let part = self.partition(*id)?;
                Ok((part.name.clone(), part.start, part.size))
            }
            Scope::Found { range, candidate } => Ok((
                format!(
                    "{} · LBA {}",
                    candidate.filesystem,
                    candidate.offset / range.sector
                ),
                candidate.offset,
                candidate.size,
            )),
        }
    }

    pub(super) fn label(&self) -> Result<String, String> {
        Ok(format!(
            "{} / {}",
            self.saved.info.name,
            self.partition_info()?.0
        ))
    }

    #[cfg(windows)]
    pub(super) fn provenance(&self) -> Result<recovery_export::VirtualRaidSource, String> {
        let array = &self.saved.array;
        let (partition, offset, size) = self.partition_info()?;
        Ok(recovery_export::VirtualRaidSource {
            config: array.config,
            members: array
                .members
                .iter()
                .map(|(index, range)| {
                    let (file_name, device) = match (&array.draft.inputs, index) {
                        (Inputs::Images(_), Some(index)) => {
                            (Some(array.draft.info.files[*index].name.clone()), None)
                        }
                        (Inputs::Physical(devices), Some(index)) => {
                            (None, Some(devices[*index].device.clone()))
                        }
                        (_, None) => (None, None),
                    };
                    recovery_export::VirtualRaidMember {
                        file_name,
                        device,
                        offset: range.start.to_string(),
                        size: range.size.to_string(),
                    }
                })
                .collect(),
            partition: recovery_export::ImageSource {
                file_name: self.saved.info.name.clone(),
                format: self.saved.info.format.clone(),
                partition,
                offset: offset.to_string(),
                size: size.to_string(),
                sector: self.saved.info.options.sector,
                parallels_snapshot: None,
            },
            recovered_filesystem: match &self.scope {
                Scope::Found { candidate, .. } => Some(candidate.filesystem.clone()),
                Scope::Partition(_) => None,
            },
            checked_rows: self.saved.checked_rows,
            md_metadata: array.md.as_ref().map(|metadata| recovery_export::MdRaidSource {
                version: metadata.version,
                array_uuid: metadata.array_uuid.clone(),
                events: metadata.events.to_string(),
                fingerprint: metadata.fingerprint.clone(),
            }),
        })
    }

    pub(super) fn disks(&self) -> Vec<Target> {
        #[cfg(windows)]
        return match &self.saved.array.draft.inputs {
            Inputs::Images(files) => files.disks(),
            Inputs::Physical(members) => {
                let mut seen = std::collections::HashSet::new();
                members
                    .iter()
                    .map(|member| &member.selection.target)
                    .filter(|target| seen.insert(&target.0))
                    .cloned()
                    .collect()
            }
        };
        #[cfg(not(windows))]
        Vec::new()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScanRequest {
    id: String,
    partition: u32,
    mode: ScanMode,
}

pub fn scan(request: ScanRequest) -> Result<String, String> {
    browse(request.mode, |s| {
        let saved = find(s, &request.id)?;
        Ok(Source {
            saved,
            scope: Scope::Partition(request.partition),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_raid_members_exclude_overlap_and_require_device_alignment() {
        let source = |partition, key: &str| Selection {
            target: Target("disk".into()),
            layout_key: key.into(),
            partition,
        };
        let part = source(Some(4), "layout");
        assert!(validate_physical(&[part.clone(), source(None, "layout")]).is_err());
        assert!(validate_physical(&[part.clone(), source(Some(4), "layout")]).is_err());
        assert!(validate_physical(&[part.clone(), source(Some(5), "new-layout")]).is_err());
        assert!(validate_physical(&[part, source(Some(5), "layout")]).is_ok());
        let range = Range {
            start: 4096,
            size: 8192,
        };
        assert_eq!(physical_range(16384, 16384, 4096, range).unwrap(), 20480);
        assert!(physical_range(16384, 8192, 4096, range).is_err());
        assert!(physical_range(
            16384,
            16384,
            4096,
            Range {
                start: 512,
                ..range
            }
        )
        .is_err());
        assert!(
            serde_json::from_value::<PhysicalRequest>(serde_json::json!({
                "members": [{"target":"opaque","layoutKey":"bound","partition":4,"path":"invalid"}]
            }))
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn recompose_binds_the_current_array_and_commits_new_results_only_after_open() {
        let fixture = |id: &str, group: &str| Saved {
            info: Info {
                id: id.into(),
                name: "RAID 5".into(),
                size: 8 * disk_core::MIB,
                options: Options {
                    kind: Kind::Disk,
                    sector: 4096,
                },
                partitions: Vec::new(),
                warning: String::new(),
                verification_notes: Vec::new(),
                parallels_snapshot: None,
                format: "RAID".into(),
                files: vec!["a.img".into(), "b.img".into()],
            },
            array: Array {
                md: None,
                config: Config {
                    layout: disk_core::virtual_raid::Layout::Raid5FixedLast,
                    sector: 4096,
                    stripe: 65536,
                },
                draft: Arc::new(Draft {
                    info: DraftInfo {
                        id: group.into(),
                        files: ["a", "b"]
                            .into_iter()
                            .map(|name| InputInfo {
                                id: name.into(),
                                name: format!("{name}.img"),
                                size: 8 * disk_core::MIB,
                            })
                            .collect(),
                    },
                    inputs: Inputs::Physical(Vec::new()),
                }),
                members: vec![
                    (
                        Some(1),
                        Range {
                            start: 65536,
                            size: 4 * disk_core::MIB,
                        },
                    ),
                    (
                        None,
                        Range {
                            start: 0,
                            size: 4 * disk_core::MIB,
                        },
                    ),
                    (
                        Some(0),
                        Range {
                            start: 131072,
                            size: 4 * disk_core::MIB,
                        },
                    ),
                ],
            },
            open_without_table: true,
            checked_rows: 0,
            missing_members: 1,
        };
        let saved = Arc::new(fixture("current", "retained"));
        let mut state = State::default();
        state.virtual_raid = Some(saved.clone());
        state.scan = Some((
            "old-scan".into(),
            ScanSource::VirtualRaid(Source {
                saved: saved.clone(),
                scope: Scope::Partition(0),
            }),
        ));
        state.scan_name = "Old catalog".into();
        let mut edit = configuration_in(&state, "current").unwrap();
        assert_eq!(edit.sources.id, "retained");
        assert!(edit.request.open_without_table);
        assert_eq!(edit.request.config.stripe, 65536);
        assert_eq!(edit.request.members[0].file.as_deref(), Some("b"));
        assert!(edit.request.members[1].file.is_none());
        assert_eq!(edit.request.members[2].range.start, 131072);
        assert!(Arc::ptr_eq(
            &open_draft(&state, &edit.request.id, edit.request.replaces.as_deref()).unwrap(),
            &saved.array.draft
        ));
        edit.request.replaces = Some("stale".into());
        assert!(open_draft(&state, &edit.request.id, edit.request.replaces.as_deref()).is_err());
        assert!(Arc::ptr_eq(state.virtual_raid.as_ref().unwrap(), &saved));
        assert_eq!(
            status_in(&state).unwrap().scan_id.as_deref(),
            Some("old-scan")
        );

        let replacement = fixture("next", "new-group");
        edit.request.replaces = Some("current".into());
        edit.request.id = "new-group".into();
        state.raid_draft = Some(replacement.array.draft.clone());
        assert!(Arc::ptr_eq(
            &open_draft(&state, &edit.request.id, edit.request.replaces.as_deref()).unwrap(),
            &replacement.array.draft
        ));
        install(&mut state, replacement);
        assert!(state.raid_draft.is_none());
        assert!(state.scan_name.is_empty());
        let status = status_in(&state).unwrap();
        assert_eq!(status.info.id, "next");
        assert!(status.scan_id.is_none());
        assert!(open_draft(&state, &edit.request.id, edit.request.replaces.as_deref()).is_err());
    }
}

fn browse(
    mode: ScanMode,
    source: impl FnOnce(&State) -> Result<Source, String>,
) -> Result<String, String> {
    if mode == ScanMode::Signatures {
        return Err("镜像目录读取请选择现存文件、全部文件或删除候选。".into());
    }
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let source = {
        let s = state();
        idle(&s)?;
        source(&s)?
    };
    let label = source.label()?;
    scan_source_mode(
        ScanSource::VirtualRaid(source),
        None,
        label,
        "读取虚拟 RAID 目录",
        mode,
        guard,
    )
}

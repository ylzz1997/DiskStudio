//! Binds a manually configured virtual RAID to immutable native file handles.
use super::*;
use crate::image_browser::{Browser, Info, Options, ReaderFactory};
use disk_core::virtual_raid::{Config, Range};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberRequest {
    file: Option<String>,
    range: Range,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRequest {
    id: String,
    config: Config,
    options: Options,
    members: Vec<MemberRequest>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(flatten)]
    pub info: Info,
    pub checked_rows: usize,
    pub missing_members: usize,
    pub scan_id: Option<String>,
    pub scan_mode: Option<ScanMode>,
}

pub(super) struct Saved {
    browser: Browser,
    factory: Arc<Array>,
    checked_rows: usize,
    missing_members: usize,
}

struct Array {
    config: Config,
    #[cfg(windows)]
    files: Arc<crate::image_files::Group>,
    #[cfg(windows)]
    members: Vec<(Option<usize>, Range)>,
}

impl Array {
    fn validate(&self) -> Result<(), String> {
        #[cfg(windows)]
        self.files.validate()?;
        Ok(())
    }

    #[cfg(windows)]
    fn array_reader(
        &self,
    ) -> Result<disk_core::virtual_raid::Reader<data_io::Region<File>>, String> {
        let mut readers: Vec<_> = self.files.raw_readers()?.into_iter().map(Some).collect();
        let members = self
            .members
            .iter()
            .map(|&(index, range)| {
                let reader = index
                    .map(|index| readers[index].take().ok_or("RAID 成员镜像不能重复。"))
                    .transpose()?;
                Ok(disk_core::virtual_raid::Member { reader, range })
            })
            .collect::<Result<Vec<_>, String>>()?;
        disk_core::virtual_raid::Reader::open(self.config, members)
    }
}

impl ReaderFactory for Array {
    fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
        #[cfg(windows)]
        return Ok(Box::new(self.array_reader()?));
        #[cfg(not(windows))]
        Err("当前仅支持 Windows 原生文件选择。".into())
    }
}

fn status_in(s: &State) -> Option<Status> {
    let saved = s.virtual_raid.as_ref()?;
    let scan = s.scan.as_ref().filter(|(_, source)| {
        matches!(source, ScanSource::VirtualRaid(source) if Arc::ptr_eq(&source.saved, saved))
    });
    Some(Status {
        info: saved.browser.info.clone(),
        checked_rows: saved.checked_rows,
        missing_members: saved.missing_members,
        scan_id: scan.map(|(id, _)| id.clone()),
        scan_mode: scan.map(|_| s.scan_mode),
    })
}

pub fn status() -> Option<Status> {
    status_in(&state())
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
}

pub fn select() -> Result<Option<crate::image_files::Info>, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 原生文件选择。".into());
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let selected = crate::image_files::choose_raw_members("选择 RAID 成员镜像")?;
        let info = selected.as_ref().map(|files| files.info());
        if selected.is_some() {
            state().raid_draft = selected;
        }
        Ok(info)
    }
}

pub fn open(request: OpenRequest) -> Result<Status, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 原生文件选择。".into());
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        let files = {
            let s = state();
            idle(&s)?;
            s.raid_draft
                .as_ref()
                .filter(|files| files.id == request.id)
                .ok_or("RAID 镜像选择已失效。")?
                .clone()
        };
        let info = files.info();
        if request.config.sector != request.options.sector
            || !(2..=disk_core::virtual_raid::MAX_MEMBERS).contains(&request.members.len())
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
                        let index = info
                            .files
                            .iter()
                            .position(|file| &file.id == id)
                            .ok_or("RAID 镜像选择已失效。")?;
                        if !selected.insert(index) {
                            return Err("RAID 成员镜像不能重复。");
                        }
                        Ok(index)
                    })
                    .transpose()?;
                Ok((index, member.range))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if selected.len() != info.files.len() {
            return Err("请为每个选中的镜像指定一个成员位置。".into());
        }
        let missing_members = members.iter().filter(|(file, _)| file.is_none()).count();
        let factory = Arc::new(Array {
            config: request.config,
            files,
            members,
        });
        factory.validate()?;
        let checked_rows = factory
            .array_reader()?
            .check_parity_samples()
            .map_err(|e| e.to_string())?;
        let browser = Browser::from_factory(
            factory.clone(),
            request.config.layout.name().into(),
            request.options,
            "RAID".into(),
            info.files.into_iter().map(|file| file.name).collect(),
            String::new(),
            || Ok(()),
        )?;
        if request.options.kind == crate::image_browser::Kind::Partition {
            check_filesystem(browser.reader(0)?)?;
        }
        factory.validate()?;
        let mut s = state();
        clear(&mut s);
        s.raid_draft = None;
        s.virtual_raid = Some(Arc::new(Saved {
            browser,
            factory,
            checked_rows,
            missing_members,
        }));
        Ok(status_in(&s).unwrap())
    }
}

#[cfg(windows)]
fn check_filesystem(mut reader: impl Read + Seek) -> Result<(), String> {
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(|e| e.to_string())?;
    if &boot[3..11] == b"NTFS    " || recovery::fat_boot(&boot) {
        return Ok(());
    }
    let mut magic = [0; 2];
    reader.seek(SeekFrom::Start(1080)).map_err(|e| e.to_string())?;
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

pub fn close(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    if s.virtual_raid
        .as_ref()
        .is_none_or(|saved| saved.browser.info.id != id)
    {
        return Err("RAID 镜像选择已失效。".into());
    }
    clear(&mut s);
    Ok(())
}

pub fn discard(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let s = &mut *state();
    idle(s)?;
    #[cfg(windows)]
    if s.raid_draft.as_ref().is_some_and(|files| files.id == id) {
        s.raid_draft = None;
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Source {
    saved: Arc<Saved>,
    partition: u32,
}

impl Source {
    pub(super) fn validate(&self) -> Result<(), String> {
        self.saved.factory.validate()
    }
    pub(super) fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
        self.validate()?;
        Ok(Box::new(self.saved.browser.reader(self.partition)?))
    }
    pub(super) fn label(&self) -> Result<String, String> {
        self.saved.browser.label(self.partition)
    }
    #[cfg(windows)]
    pub(super) fn provenance(&self) -> Result<recovery_export::VirtualRaidSource, String> {
        let factory = &self.saved.factory;
        let files = factory.files.info().files;
        Ok(recovery_export::VirtualRaidSource {
            config: factory.config,
            members: factory
                .members
                .iter()
                .map(|(index, range)| recovery_export::VirtualRaidMember {
                    file_name: index.map(|index| files[index].name.clone()),
                    offset: range.start.to_string(),
                    size: range.size.to_string(),
                })
                .collect(),
            partition: self.saved.browser.provenance(self.partition)?,
            checked_rows: self.saved.checked_rows,
        })
    }
    pub(super) fn disks(&self) -> Vec<Target> {
        #[cfg(windows)]
        return self.saved.factory.files.disks();
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
    if request.mode == ScanMode::Signatures {
        return Err("镜像目录读取请选择现存文件、全部文件或删除候选。".into());
    }
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let source = {
        let s = state();
        idle(&s)?;
        let saved = s
            .virtual_raid
            .as_ref()
            .filter(|saved| saved.browser.info.id == request.id)
            .ok_or("RAID 镜像选择已失效。")?;
        Source {
            saved: saved.clone(),
            partition: request.partition,
        }
    };
    let label = source.label()?;
    // The worker rechecks the source after reading the directory.
    let reader = source.reader()?;
    scan_source_mode(
        ScanSource::VirtualRaid(source),
        Some(reader),
        label,
        "读取虚拟 RAID 目录",
        request.mode,
        guard,
    )
}

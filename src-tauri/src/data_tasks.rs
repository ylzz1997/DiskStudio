#[cfg(windows)]
use crate::data_windows as platform;
use crate::{
    backup_archive::{self, Chain},
    data_io,
    partitions::Target,
    recovery::{self, Candidate, ScanMode},
    recovery_export::{self, Layout},
    recovery_tree::{self, Catalog, Filter, FolderPage},
};
use serde::{Deserialize, Serialize};
#[path = "image_browse_tasks.rs"]
pub mod image_browse_tasks;
#[path = "recovery_tasks.rs"]
pub mod recovery_tasks;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub target: Target,
    pub layout_key: String,
    pub partition: Option<u32>,
}
#[derive(Clone)]
pub(crate) enum ScanSource {
    Disk(Selection),
    Lost {
        source: Selection,
        range: crate::surface_scan::Range,
        candidate: crate::lost_partitions::Candidate,
    },
    Backup {
        opened: OpenedBackup,
        partition: u32,
    },
    Image {
        opened: image_browse_tasks::Opened,
        partition: u32,
    },
}
impl From<Selection> for ScanSource {
    fn from(source: Selection) -> Self {
        Self::Disk(source)
    }
}
#[derive(Clone)]
pub(crate) struct OpenedBackup {
    browser: Arc<crate::backup_browser::Browser>,
    source_disks: Vec<Target>,
    #[cfg(windows)]
    source_bindings: Vec<platform::FileBinding>,
}
impl OpenedBackup {
    fn validate(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            platform::FileBinding::validate_all(&self.source_bindings)?;
        }
        Ok(())
    }
}
impl ScanSource {
    #[cfg(windows)]
    fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
        match self {
            Self::Lost {
                source,
                range,
                candidate,
            } => {
                let resolved = platform::resolve(source, false, false)?;
                let reader = crate::lost_partitions::candidate_reader(
                    platform::device(&resolved, false)?,
                    resolved.size,
                    resolved
                        .disk
                        .logical_sector_size
                        .ok_or("缺少设备扇区大小")?,
                    *range,
                    candidate,
                    &AtomicBool::new(false),
                )?;
                Ok(Box::new(reader))
            }
            Self::Disk(source) => Ok(Box::new(platform::reader(&platform::resolve(
                source, false, false,
            )?)?)),
            Self::Backup { opened, partition } => {
                opened.validate()?;
                Ok(Box::new(opened.browser.reader(*partition)?))
            }
            Self::Image { opened, partition } => {
                opened.validate()?;
                Ok(Box::new(opened.browser.reader(*partition)?))
            }
        }
    }
    fn forbidden_disks(&self) -> Vec<Target> {
        match self {
            Self::Disk(s) | Self::Lost { source: s, .. } => vec![s.target.clone()],
            Self::Backup { opened, .. } => opened.source_disks.clone(),
            Self::Image { opened, .. } => opened.source_disks.clone(),
        }
    }
    #[cfg(windows)]
    fn label(&self) -> Result<String, String> {
        match self {
            Self::Lost {
                source,
                range,
                candidate,
            } => {
                let label = platform::resolve(source, false, false)?.label;
                Ok(format!(
                    "{label} / 找到的 {} / 起始 LBA {}",
                    candidate.filesystem,
                    candidate.offset / range.sector
                ))
            }
            Self::Disk(s) => Ok(platform::resolve(s, false, false)?.label),
            Self::Backup { opened, partition } => {
                opened.validate()?;
                opened.browser.source_label(*partition)
            }
            Self::Image { opened, partition } => {
                opened.validate()?;
                opened.browser.label(*partition)
            }
        }
    }
    #[cfg(windows)]
    fn destination(&self, path: &std::path::Path, size: u64) -> Result<PathBuf, String> {
        if let Self::Backup { opened, .. } = self {
            opened.validate()?;
        }
        if let Self::Image { opened, .. } = self {
            opened.validate()?;
        }
        let sources = self.forbidden_disks();
        if sources.is_empty() {
            return Err("缺少来源文件所在磁盘身份".into());
        }
        let mut result = None;
        for source in sources {
            result = Some(platform::destination_folder(path, &source, size)?);
        }
        Ok(result.unwrap())
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub title: String,
    pub state: String,
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub found: usize,
    pub message: String,
    pub sha256: Option<String>,
}
struct State {
    recovery_progress: Option<recovery_tasks::Saved>,
    job: Option<Job>,
    cancel: Arc<AtomicBool>,
    candidates: Vec<Candidate>,
    directories: Catalog,
    scan: Option<(String, ScanSource)>,
    opened_backup: Option<OpenedBackup>,
    opened_image: Option<image_browse_tasks::Opened>,
    scan_name: String,
    scan_mode: ScanMode,
    scan_completion: crate::directory_report::Completion,
    pending: Option<Pending>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            recovery_progress: None,
            job: None,
            cancel: Arc::new(AtomicBool::new(false)),
            candidates: vec![],
            directories: Catalog::default(),
            scan: None,
            opened_backup: None,
            opened_image: None,
            scan_name: String::new(),
            scan_mode: ScanMode::AllFiles,
            scan_completion: crate::directory_report::Completion::Running,
            pending: None,
        }
    }
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
fn idle(s: &State) -> Result<(), String> {
    if s.job.as_ref().is_some_and(|j| j.state == "running") {
        Err("已有数据任务运行，请等待完成或取消。".into())
    } else {
        Ok(())
    }
}
pub fn status() -> Option<Job> {
    let mut s = state();
    if s.pending
        .as_ref()
        .is_some_and(|p| Instant::now() > p.expires)
    {
        s.pending = None;
    }
    s.job.clone()
}
pub fn discard_plan(token: String) {
    let mut s = state();
    if s.pending.as_ref().is_some_and(|p| p.plan.token == token) {
        s.pending = None;
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    id: String,
    source_name: String,
    lost: Option<LostScanStatus>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LostScanStatus {
    filesystem: String,
    size: u64,
    start_sector: u64,
}
// UI polling reads retained state only; never resolve a disk or open a device.
pub fn scan_status() -> Option<ScanStatus> {
    scan_status_in(&state())
}
fn scan_status_in(s: &State) -> Option<ScanStatus> {
    if matches!(s.scan_mode, ScanMode::Existing) {
        return None;
    }
    s.scan.as_ref().and_then(|(id, source)| {
        let lost = match source {
            ScanSource::Disk(_) => None,
            ScanSource::Lost {
                range, candidate, ..
            } => Some(LostScanStatus {
                filesystem: candidate.filesystem.clone(),
                size: candidate.size,
                start_sector: candidate.offset / range.sector,
            }),
            ScanSource::Backup { .. } | ScanSource::Image { .. } => return None,
        };
        Some(ScanStatus {
            id: id.clone(),
            source_name: s.scan_name.clone(),
            lost,
        })
    })
}
// This endpoint only matches retained memory state: it must never resolve or
// open the requested source. Returning to a page does not initiate device I/O.
fn matching_browse_id(s: &State, source: &Selection) -> Option<String> {
    if source.partition.is_none() || !matches!(s.scan_mode, ScanMode::Existing) {
        return None;
    }
    s.scan.as_ref().and_then(|(id, saved)| match saved {
        ScanSource::Disk(saved)
            if saved.target == source.target
                && saved.layout_key == source.layout_key
                && saved.partition == source.partition =>
        {
            Some(id.clone())
        }
        _ => None,
    })
}
pub fn browse_id(source: Selection) -> Option<String> {
    matching_browse_id(&state(), &source)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    #[serde(flatten)]
    info: crate::backup_browser::Info,
    scan_id: Option<String>,
    scan_partition: Option<u32>,
}
pub fn backup_status() -> Option<BackupStatus> {
    let s = state();
    let opened = s.opened_backup.as_ref()?;
    let scan_id = s.scan.as_ref().and_then(|(id, source)| match source {
        ScanSource::Backup { opened: scan, .. }
            if scan.browser.info.id == opened.browser.info.id =>
        {
            Some(id.clone())
        }
        _ => None,
    });
    Some(BackupStatus {
        info: opened.browser.info.clone(),
        scan_partition: s.scan.as_ref().and_then(|(_, source)| match source {
            ScanSource::Backup {
                opened: scan,
                partition,
            } if scan.browser.info.id == opened.browser.info.id => Some(*partition),
            _ => None,
        }),
        scan_id,
    })
}
pub fn close_backup(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    if s.opened_backup
        .as_ref()
        .is_none_or(|b| b.browser.info.id != id)
    {
        return Err("备份浏览会话已失效".into());
    }
    if s.scan.as_ref().is_some_and(
        |(_, source)| matches!(source,ScanSource::Backup{opened,..} if opened.browser.info.id==id),
    ) {
        s.scan = None;
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name.clear();
    }
    s.pending = None;
    s.opened_backup = None;
    Ok(())
}
pub fn open_backup() -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        Err("当前仅支持 Windows 原生文件选择".into())
    }
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let Some(path) = rfd::FileDialog::new()
            .set_title("打开备份以浏览文件")
            .add_filter("DiskStudio 备份", &["dsbackup"])
            .pick_file()
        else {
            return Ok(None);
        };
        let name = path
            .file_name()
            .ok_or("备份文件名无效")?
            .to_string_lossy()
            .into_owned();
        let mut source_disks = vec![];
        let mut source_bindings = vec![];
        let mut open = |path: PathBuf| -> Result<crate::backup_files::Input, String> {
            let input = crate::backup_files::Input::open(&path, None)?;
            source_bindings.push(input.binding.clone());
            if !source_disks.contains(&input.disk) {
                source_disks.push(input.disk.clone());
            }
            Ok(input)
        };
        enum Input {
            V2(Chain),
            V1(File),
        }
        let input = open(path)?;
        let input = match &input.magic {
            b"DSBK0002" | b"DSBK0003" | b"DSBKENC1" => {
                let Some(archive) = input.archive(|| Ok(()))? else {
                    return Ok(None);
                };
                let mut chain = Chain::new(archive);
                while let Some(parent) = chain.missing_parent() {
                    if chain.depth() >= backup_archive::MAX_CHAIN {
                        return Err("备份链超过 16 层".into());
                    }
                    let Some(path) = rfd::FileDialog::new()
                        .set_title(format!("选择依赖的基础备份 · {parent}"))
                        .add_filter("DiskStudio 备份", &["dsbackup"])
                        .pick_file()
                    else {
                        return Ok(None);
                    };
                    let Some(archive) = open(path)?.archive(|| Ok(()))? else {
                        return Ok(None);
                    };
                    chain.add_parent(archive)?;
                }
                Input::V2(chain)
            }
            b"DSBK0001" => {
                let mut file = input.file;
                read_header(&mut file)?;
                Input::V1(file)
            }
            _ => {
                return Err(
                    "仅支持 DiskStudio 备份及其加密文件，不支持 DiskGenius 专有备份格式".into(),
                )
            }
        };
        let (id, cancel) = start("打开备份")?;
        state().pending = None;
        let work_id = id.clone();
        drop(guard);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = crate::OPERATION
                    .try_lock()
                    .map_err(|_| "另一项磁盘操作正在运行")?;
                let tick = |n, total| progress(&work_id, &cancel, "校验备份", n, total);
                let browser = match input {
                    Input::V2(chain) => {
                        crate::backup_browser::Browser::from_chain(chain, name, tick)?
                    }
                    Input::V1(file) => {
                        crate::backup_browser::Browser::from_legacy(file, name, tick)?
                    }
                };
                let opened = OpenedBackup {
                    browser: Arc::new(browser),
                    source_disks,
                    source_bindings,
                };
                opened.validate()?;
                let message = "备份校验通过。请选择备份中的分区并读取目录；没有还原或挂载到磁盘。";
                commit_backup(&mut state(), &work_id, None, opened, message)?;
                Ok((message.into(), None))
            }))
            .unwrap_or_else(|_| Err("备份读取异常退出；没有写入备份或磁盘".into()));
            finish(&work_id, result);
        });
        Ok(Some(id))
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupScanRequest {
    pub id: String,
    pub partition: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupPointRequest {
    pub id: String,
    pub point: String,
}
pub fn select_backup_point(request: BackupPointRequest) -> Result<String, String> {
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut opened = {
        let s = state();
        idle(&s)?;
        let opened = s
            .opened_backup
            .as_ref()
            .filter(|b| b.browser.info.id == request.id)
            .ok_or("备份浏览会话已失效")?
            .clone();
        if !opened
            .browser
            .info
            .points
            .iter()
            .any(|p| p.id == request.point)
        {
            return Err("备份还原点不存在".into());
        }
        opened
    };
    let (id, cancel) = start("切换备份还原点")?;
    state().pending = None;
    let work_id = id.clone();
    drop(guard);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = crate::OPERATION
                .try_lock()
                .map_err(|_| "另一项磁盘操作正在运行")?;
            opened.validate()?;
            let browser = opened.browser.select_point(&request.point, |n, total| {
                progress(&work_id, &cancel, "校验还原点", n, total)
            })?;
            opened.validate()?;
            // Replace only after complete verification. A failed/cancelled switch
            // leaves both the previously verified view and its directory intact.
            opened.browser = Arc::new(browser);
            let message = "还原点校验通过。请重新读取此还原点中的目录。";
            commit_backup(&mut state(), &work_id, Some(&request.id), opened, message)?;
            Ok((message.into(), None))
        }))
        .unwrap_or_else(|_| Err("还原点读取异常退出，保留原还原点".into()));
        finish(&work_id, result);
    });
    Ok(id)
}
// Both publication and the terminal job state share cancel()'s state lock.
// Cancellation either wins and preserves the old view, or observes completion.
fn commit_backup(
    s: &mut State,
    job_id: &str,
    expected_session: Option<&str>,
    opened: OpenedBackup,
    message: &str,
) -> Result<(), String> {
    if s.job
        .as_ref()
        .is_none_or(|j| j.id != job_id || j.state != "running")
    {
        return Err("备份任务已失效".into());
    }
    if expected_session.is_some_and(|id| {
        s.opened_backup
            .as_ref()
            .is_none_or(|b| b.browser.info.id != id)
    }) {
        return Err("备份浏览会话已失效".into());
    }
    if s.cancel.load(Ordering::Relaxed) {
        return Err("操作已取消，保留原备份还原点和目录".into());
    }
    if s.scan
        .as_ref()
        .is_some_and(|(_, source)| matches!(source, ScanSource::Backup { .. }))
    {
        s.scan = None;
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name.clear();
    }
    s.opened_backup = Some(opened);
    s.pending = None;
    let job = s.job.as_mut().unwrap();
    job.state = "completed".into();
    job.phase = "完成".into();
    job.message = message.into();
    job.sha256 = None;
    Ok(())
}
pub fn scan_backup(request: BackupScanRequest) -> Result<String, String> {
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let source = {
        let s = state();
        idle(&s)?;
        let opened = s
            .opened_backup
            .as_ref()
            .filter(|b| b.browser.info.id == request.id)
            .ok_or("备份浏览会话已失效")?
            .clone();
        if !opened
            .browser
            .info
            .partitions
            .iter()
            .any(|p| p.id == request.partition)
        {
            return Err("备份分区选择无效".into());
        }
        ScanSource::Backup {
            opened,
            partition: request.partition,
        }
    };
    let ScanSource::Backup { opened, partition } = &source else {
        unreachable!()
    };
    opened.validate()?;
    let reader = opened.browser.reader(*partition)?;
    let label = format!("备份：{}", opened.browser.source_label(*partition)?);
    scan_source(source, Box::new(reader), label, "读取备份目录", guard)
}
pub fn scan_lost(request: crate::lost_partition_tasks::BrowseRequest) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("当前仅支持 Windows 设备读取".into())
    }
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行")?;
        idle(&state())?;
        let (source, range, candidate) = crate::lost_partition_tasks::selected(request)?;
        let source = ScanSource::Lost {
            source,
            range,
            candidate,
        };
        let label = source.label()?;
        let reader = source.reader()?;
        scan_source(source, reader, label, "读取找到的分区目录", guard)
    }
}
fn scan_source(
    source: ScanSource,
    reader: Box<dyn backup_archive::Source>,
    label: String,
    title: &str,
    guard: std::sync::MutexGuard<'_, ()>,
) -> Result<String, String> {
    scan_source_mode(source, reader, label, title, ScanMode::AllFiles, guard)
}
fn scan_source_mode(
    source: ScanSource,
    reader: Box<dyn backup_archive::Source>,
    label: String,
    title: &str,
    mode: ScanMode,
    guard: std::sync::MutexGuard<'_, ()>,
) -> Result<String, String> {
    let (id, cancel) = start(title)?;
    let image_bridge = match &source {
        ScanSource::Image { opened, .. } => Some(opened.bridge.clone()),
        _ => None,
    };
    {
        let mut s = state();
        s.scan = Some((id.clone(), source));
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name = label;
        s.scan_mode = mode;
        s.scan_completion = crate::directory_report::Completion::Running;
        s.pending = None;
    }
    let work_id = id.clone();
    drop(guard);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = crate::OPERATION
                .try_lock()
                .map_err(|_| "另一项磁盘操作正在运行")?;
            let message = recovery::scan_filesystem_owned(
                reader,
                mode,
                cancel.clone(),
                |done, total| progress(&work_id, &cancel, "读取目录", done, total),
                |c| {
                    let mut s = state();
                    s.directories.add_file(&c);
                    s.candidates.push(c);
                    let count = s.candidates.len();
                    if let Some(j) = &mut s.job {
                        j.found = count;
                    }
                },
                |d| state().directories.insert(d),
            )?;
            if let Some(bridge) = image_bridge {
                bridge.healthy()?;
            }
            Ok((message, None))
        }))
        .unwrap_or_else(|_| Err("目录读取异常退出；没有修改数据源".into()));
        finish(&work_id, result);
    });
    Ok(id)
}
pub fn cancel(id: String) -> Result<(), String> {
    cancel_in(&state(), &id)?;
    crate::image_bridge::cancel_all();
    Ok(())
}
fn cancel_in(s: &State, id: &str) -> Result<(), String> {
    if s.job
        .as_ref()
        .is_none_or(|j| j.id != id || j.state != "running")
    {
        return Err("任务已结束。".into());
    }
    s.cancel.store(true, Ordering::Relaxed);
    Ok(())
}
pub(crate) fn start(title: &str) -> Result<(String, Arc<AtomicBool>), String> {
    let mut s = state();
    idle(&s)?;
    let id = uuid::Uuid::new_v4().to_string();
    s.cancel = Arc::new(AtomicBool::new(false));
    s.job = Some(Job {
        id: id.clone(),
        title: title.into(),
        state: "running".into(),
        phase: "准备".into(),
        done: 0,
        total: 0,
        found: 0,
        message: String::new(),
        sha256: None,
    });
    Ok((id, s.cancel.clone()))
}
fn progress(
    id: &str,
    cancel: &AtomicBool,
    phase: &str,
    done: u64,
    total: u64,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("任务已取消。已写入的数据不会回滚；部分输出保留为 .partial，覆盖目标保持脱机，请勿当作完整副本使用。".into());
    }
    record_progress(id, phase, done, total);
    Ok(())
}
pub(crate) fn record_progress(id: &str, phase: &str, done: u64, total: u64) {
    if let Some(j) = &mut state().job {
        if j.id == id {
            j.phase = phase.into();
            j.done = done;
            j.total = total;
        }
    }
}
pub(crate) fn finish(id: &str, result: Result<(String, Option<String>), String>) {
    let mut s = state();
    let canceled = s.cancel.load(Ordering::Relaxed);
    let paused = canceled
        && s.recovery_progress.as_ref().is_some_and(|p| {
            p.id == id && p.pause_requested && p.cursor.as_ref().is_some_and(|c| !c.sealed())
        });
    if s.scan.as_ref().is_some_and(|(scan_id, _)| scan_id == id) {
        s.scan_completion = if result.is_ok() {
            crate::directory_report::Completion::Ended
        } else if paused {
            crate::directory_report::Completion::Paused
        } else if canceled {
            crate::directory_report::Completion::Canceled
        } else {
            crate::directory_report::Completion::Failed
        };
    }
    if let Some(j) = &mut s.job {
        if j.id == id {
            match result {
                Ok((message, hash)) => {
                    j.state = "completed".into();
                    j.message = message;
                    j.sha256 = hash;
                    j.phase = "完成".into();
                }
                Err(e) => {
                    j.state = if paused {
                        "paused"
                    } else if canceled {
                        "canceled"
                    } else {
                        "failed"
                    }
                    .into();
                    j.message = if paused {
                        "恢复扫描已暂停，可保存进度或继续。未完成的数据块或文件记录将在续扫时重新检查。"
                            .into()
                    } else {
                        e
                    };
                    j.phase = if paused { "已暂停" } else { "已停止" }.into();
                }
            }
        }
    }
}

pub fn scan(
    source: Selection,
    mode: ScanMode,
    signatures: Option<crate::signature_scan::Options>,
) -> Result<String, String> {
    recovery_tasks::start_scan(source, mode, signatures)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Results {
    pub scan_id: String,
    pub source: String,
    pub total: usize,
    pub scanned_total: usize,
    pub offset: usize,
    pub paths: std::collections::HashMap<String, Vec<String>>,
    pub items: Vec<Candidate>,
}
pub fn filtered_results(
    scan_id: String,
    offset: usize,
    query: String,
    filter: Filter,
) -> Result<Results, String> {
    let s = state();
    if s.scan.as_ref().is_none_or(|(id, _)| id != &scan_id) {
        return Err("扫描结果已失效。".into());
    }
    let matched = recovery_tree::query::ordered(&s.candidates, &s.directories, &query, &filter)?;
    let total = matched.len();
    let offset = recovery_tree::clamp_offset(offset, total);
    let items: Vec<_> = matched
        .into_iter()
        .skip(offset)
        .take(100)
        .cloned()
        .collect();
    let mut paths = std::collections::HashMap::new();
    for c in &items {
        if let Some(id) = &c.directory {
            if !paths.contains_key(id) {
                paths.insert(
                    id.clone(),
                    s.directories
                        .path(id)?
                        .into_iter()
                        .map(|d| d.name)
                        .collect(),
                );
            }
        }
    }
    Ok(Results {
        scan_id,
        source: s.scan_name.clone(),
        total,
        scanned_total: s.candidates.len(),
        offset,
        paths,
        items,
    })
}
pub fn folders(
    scan_id: String,
    folder: Option<String>,
    offset: usize,
) -> Result<FolderPage, String> {
    let s = state();
    if s.scan.as_ref().is_none_or(|(id, _)| id != &scan_id) {
        return Err("扫描结果已失效。".into());
    }
    let mut page = s.directories.folders(folder.as_deref(), offset)?;
    page.can_export_directory = directory_export_available(&s, folder.as_deref());
    Ok(page)
}
fn directory_export_available(s: &State, folder: Option<&str>) -> bool {
    let Some(folder) = folder else {
        return false;
    };
    if s.scan.is_none()
        || s.scan_mode == ScanMode::Signatures
        || idle(s).is_err()
        || s.directories.path(folder).is_err()
    {
        return false;
    }
    // ROOT is a virtual UI node even before any successful filesystem read.
    // A failed/paused empty scan must not turn it into a claimed source folder.
    folder != recovery_tree::ROOT
        || matches!(
            s.scan_completion,
            crate::directory_report::Completion::Ended
        )
        || s.candidates.iter().any(|c| c.directory.is_some())
        || s.directories.snapshot().len() > 1
}
pub fn select_results(scan_id: String, query: String, filter: Filter) -> Result<Vec<u32>, String> {
    let s = state();
    if s.scan.as_ref().is_none_or(|(id, _)| id != &scan_id) {
        return Err("扫描结果已失效。".into());
    }
    recovery_tree::select_ids(&s.candidates, &s.directories, &query, &filter)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectoryReportRequest {
    scan_id: String,
}
// Takes only an opaque scan identity. The snapshot deliberately omits hardware
// labels, source paths, disk/layout identities and content extraction locators.
fn directory_report_snapshot(
    s: &State,
    scan_id: &str,
) -> Result<
    (
        Catalog,
        Vec<Candidate>,
        ScanMode,
        crate::directory_report::Completion,
    ),
    String,
> {
    idle(s)?;
    if s.scan.as_ref().is_none_or(|(id, _)| id != scan_id) {
        return Err("扫描结果已失效。".into());
    }
    Ok((
        s.directories.clone(),
        s.candidates.clone(),
        s.scan_mode,
        s.scan_completion,
    ))
}
pub fn export_directory_report(request: DirectoryReportRequest) -> Result<Option<String>, String> {
    #[cfg(windows)]
    use platform::err;
    let (catalog, files, mode, completion, source) = {
        let s = state();
        let (catalog, files, mode, completion) = directory_report_snapshot(&s, &request.scan_id)?;
        (
            catalog,
            files,
            mode,
            completion,
            s.scan.as_ref().unwrap().1.clone(),
        )
    };
    let html = crate::directory_report::render(&catalog, &files, mode, completion)?;
    #[cfg(not(windows))]
    {
        let _ = (html, source);
        Err("当前仅支持 Windows 保存目录清单。".into())
    }
    #[cfg(windows)]
    {
        let Some(path) = rfd::FileDialog::new()
            .set_title("保存目录清单到另一块磁盘（请使用新文件名）")
            .add_filter("HTML 目录清单", &["html"])
            .set_file_name("DiskStudio-directory.html")
            .save_file()
        else {
            return Ok(None);
        };
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("html"))
        {
            return Err("目录清单需要使用 .html 扩展名。".into());
        }
        let parent =
            source.destination(path.parent().ok_or("输出目录无效。")?, html.len() as u64)?;
        let name = path.file_name().ok_or("文件名无效。")?;
        if name.to_string_lossy().contains(':') {
            return Err("不支持数据流文件名。".into());
        }
        let output = parent.join(name);
        if output.try_exists().map_err(err)? {
            return Err("文件已存在，请使用新文件名；未覆盖原文件。".into());
        }
        let partial = parent.join(format!(
            "DiskStudio-report-{}.partial",
            uuid::Uuid::new_v4()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(err)?;
        let result = (|| {
            file.write_all(html.as_bytes()).map_err(err)?;
            file.sync_all().map_err(err)?;
            drop(file);
            rename_new(&partial, &output)
        })();
        result.map_err(|e| {
            format!(
                "目录清单未保存完成：{e}。临时文件保留于 {}",
                partial.display()
            )
        })?;
        Ok(Some(output.to_string_lossy().into_owned()))
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub name: String,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}
pub fn preview(scan_id: String, file_id: u32) -> Result<Preview, String> {
    #[cfg(not(windows))]
    {
        let _ = (scan_id, file_id);
        Err("仅 Windows 支持。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成。")?;
        let (source, c) = {
            let s = state();
            idle(&s)?;
            let (id, source) = s.scan.as_ref().ok_or("尚无扫描结果")?;
            if *id != scan_id {
                return Err("扫描结果已失效。".into());
            }
            (
                source.clone(),
                s.candidates
                    .iter()
                    .find(|c| c.id == file_id)
                    .ok_or("文件不存在")?
                    .clone(),
            )
        };
        let reader = source.reader()?;
        let extension = c.name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let limit = if ["png", "jpg", "jpeg", "gif", "bmp", "webp"].contains(&extension.as_str()) {
            8 * 1024 * 1024
        } else {
            64 * 1024
        };
        let mut bytes = vec![];
        recovery::extract_owned(
            reader,
            &c,
            &mut bytes,
            limit,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )?;
        Ok(Preview {
            name: c.name,
            bytes,
            truncated: c.size > limit,
        })
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Request {
    Backup {
        source: Selection,
        #[serde(default)]
        options: BackupOptions,
    },
    Clone {
        source: Selection,
        destination: Selection,
        #[serde(default)]
        mode: crate::used_space::Mode,
    },
    Restore {
        destination: Selection,
    },
    Recover {
        scan_id: String,
        file_ids: Vec<u32>,
        #[serde(default)]
        folder: Option<String>,
        #[serde(default)]
        layout: Layout,
        #[serde(default)]
        options: crate::export_metadata::Options,
    },
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupOptions {
    #[serde(default)]
    pub compression: bool,
    #[serde(default)]
    pub compression_level: backup_archive::Compression,
    #[serde(default)]
    pub encryption: bool,
    #[serde(default)]
    pub incremental: bool,
    #[serde(default)]
    pub hot: bool,
    #[serde(default)]
    pub mode: crate::used_space::Mode,
}
impl BackupOptions {
    fn compression(&self) -> backup_archive::Compression {
        if self.compression {
            self.compression_level
        } else {
            backup_archive::Compression::None
        }
    }
}
fn validate_backup_options(options: &BackupOptions) -> Result<(), String> {
    if options.compression && options.compression_level == backup_archive::Compression::None {
        return Err("压缩开关与压缩档位不一致。".into());
    }
    if options.hot && options.mode == crate::used_space::Mode::UsedSpace {
        return Err("按已用空间备份需要锁定源卷；VSS 热备请使用逐扇区方式。".into());
    }
    Ok(())
}
fn backup_output_bound(payload: u64, size: u64, encrypted: bool) -> Result<u64, String> {
    let plain = payload
        .checked_add(HEADER_SIZE)
        .and_then(|n| {
            n.checked_add(
                size.div_ceil(backup_archive::CHUNK as u64)
                    .checked_mul(56)?,
            )
        })
        .ok_or("备份输出容量超出范围")?;
    if encrypted {
        crate::backup_encryption::storage_bound(plain).map_err(|e| e.to_string())
    } else {
        Ok(plain)
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub token: String,
    pub title: String,
    pub source: String,
    pub destination: String,
    pub bytes: u64,
    pub warning: String,
    pub destructive: bool,
}
pub(crate) enum Work {
    ArchiveBackup {
        source: Selection,
        path: PathBuf,
        options: BackupOptions,
        parent: Option<Chain>,
    },
    ArchiveRestore {
        destination: Selection,
        chain: Chain,
    },
    #[allow(dead_code)] // Version 1 fixture writer retained for compatibility checks.
    Backup { source: Selection, path: PathBuf },
    Clone {
        source: Selection,
        destination: Selection,
        mode: crate::used_space::Mode,
    },
    Restore {
        destination: Selection,
        file: File,
        header: BackupHeader,
        #[cfg(windows)]
        binding: platform::FileBinding,
    },
    Recover {
        source: ScanSource,
        files: Vec<Candidate>,
        folder: PathBuf,
        export: recovery_export::Plan,
    },
}
struct Pending {
    plan: Plan,
    work: Work,
    expires: Instant,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupHeader {
    pub(crate) version: u32,
    pub(crate) kind: String,
    pub(crate) size: u64,
    pub(crate) sector: u64,
    pub(crate) sha256: String,
}
const HEADER_SIZE: u64 = 4096;
pub(crate) fn read_header(file: &mut (impl Read + Seek)) -> Result<BackupHeader, String> {
    let len = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let mut buf = vec![0; HEADER_SIZE as usize];
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.read_exact(&mut buf).map_err(|e| e.to_string())?;
    decode_header(&buf, len)
}
fn decode_header(buf: &[u8], len: u64) -> Result<BackupHeader, String> {
    if buf.len() != HEADER_SIZE as usize || &buf[..8] != b"DSBK0001" {
        return Err("不是 DiskStudio 备份文件。".into());
    }
    let n = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
    if n > 4084 {
        return Err("备份头长度异常。".into());
    }
    let h: BackupHeader = serde_json::from_slice(&buf[12..12 + n]).map_err(|e| e.to_string())?;
    if h.version != 1
        || !matches!(h.kind.as_str(), "disk" | "partition")
        || !matches!(h.sector, 512 | 4096)
        || h.size == 0
        || h.size % h.sector != 0
        || h.size.checked_add(HEADER_SIZE) != Some(len)
        || h.sha256.len() != 64
        || !h.sha256.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("备份信息无效或文件不完整。".into());
    }
    Ok(h)
}
fn write_header(file: &mut File, h: &BackupHeader) -> Result<(), String> {
    let json = serde_json::to_vec(h).map_err(|e| e.to_string())?;
    if json.len() > 4084 {
        return Err("备份头过大".into());
    }
    let mut buf = vec![0; HEADER_SIZE as usize];
    buf[..8].copy_from_slice(b"DSBK0001");
    buf[8..12].copy_from_slice(&(json.len() as u32).to_le_bytes());
    buf[12..12 + json.len()].copy_from_slice(&json);
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.write_all(&buf).map_err(|e| e.to_string())
}

pub fn prepare(request: Request) -> Result<Option<Plan>, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘任务正在运行。")?;
        {
            let mut s = state();
            idle(&s)?;
            s.pending = None;
        }
        let (title, source_label, destination_label, bytes, warning, destructive, work) =
            match request {
                Request::Backup { source, options } => {
                    validate_backup_options(&options)?;
                    let r = platform::resolve(&source, true, false)?;
                    if options.hot
                        && (source.partition.is_none()
                            || !r.ntfs_volume
                            || r.volumes.len() != 1
                            || r.disk.is_offline != Some(false))
                    {
                        return Err(
                            "VSS 热备需要一个在线 NTFS 数据分区。整盘及运行中的系统分区暂不支持。"
                                .into(),
                        );
                    }
                    let parent = if options.incremental {
                        let Some(path) = rfd::FileDialog::new()
                            .set_title("选择上一次备份（完整或增量）")
                            .add_filter("DiskStudio 备份", &["dsbackup"])
                            .pick_file()
                        else {
                            return Ok(None);
                        };
                        let input = crate::backup_files::Input::open(&path, Some(&source.target))?;
                        let Some(chain) = choose_chain(input, &source.target)? else {
                            return Ok(None);
                        };
                        if chain.header().size != r.size
                            || chain.header().sector != r.disk.logical_sector_size.unwrap()
                            || (chain.header().kind == "partition") != source.partition.is_some()
                            || chain.depth() >= backup_archive::MAX_CHAIN
                        {
                            return Err("基础备份与所选范围不匹配，或已达到 16 层上限。".into());
                        }
                        Some(chain)
                    } else {
                        None
                    };
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("保存磁盘/分区备份")
                        .add_filter("DiskStudio 备份", &["dsbackup"])
                        .set_file_name("DiskStudio-backup.dsbackup")
                        .save_file()
                    else {
                        return Ok(None);
                    };
                    if path.exists() {
                        return Err("为避免覆盖，请选择新的备份文件名。".into());
                    }
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or("备份文件名无效")?;
                    if name != data_io::safe_name(name)
                        || !name.to_ascii_lowercase().ends_with(".dsbackup")
                    {
                        return Err("请选择不含特殊路径字符、以 .dsbackup 结尾的新文件名（最多 120 字符）。".into());
                    }
                    let folder = platform::destination_folder(
                        path.parent().ok_or("输出目录无效")?,
                        &source.target,
                        if options.mode == crate::used_space::Mode::UsedSpace {
                            0
                        } else {
                            backup_output_bound(r.size, r.size, options.encryption)?
                        },
                    )?;
                    let path = folder.join(path.file_name().ok_or("文件名无效")?);
                    let warning = format!(
                        "{}{}{}{}{} 完成后重新读取并校验全部逻辑内容。",
                        if options.hot {
                            "VSS 文件系统一致性快照，源卷可继续使用；不包含数据库等应用的事务一致性保证。"
                        } else {
                            "任务期间锁定源卷。"
                        },
                        if options.incremental {
                            "只保存相对上次备份变化的数据块；还原必须保留完整依赖链。"
                        } else {
                            if options.mode == crate::used_space::Mode::UsedSpace {
                                "创建完整的已用空间备份。"
                            } else {
                                "完整备份包含空闲空间。"
                            }
                        },
                        if options.compression {
                            "启用并行压缩。"
                        } else {
                            "不压缩。"
                        },
                        if options.mode == crate::used_space::Mode::UsedSpace {
                            "已确认空闲的区域按零保存，不保留其已删除内容；未知文件系统完整保留。"
                        } else {
                            ""
                        },
                        if options.encryption {
                            "使用密码加密；打开及还原时需要密码，程序不保存密码。"
                        } else {
                            ""
                        }
                    );
                    (
                        "创建镜像备份",
                        r.label,
                        path.display().to_string(),
                        r.size,
                        warning,
                        false,
                        Work::ArchiveBackup {
                            source,
                            path,
                            options,
                            parent,
                        },
                    )
                }
                Request::Clone {
                    source,
                    destination,
                    mode,
                } => {
                    validate_pair(&source, &destination)?;
                    let src = platform::resolve(&source, true, false)?;
                    let dst = platform::resolve(&destination, true, true)?;
                    if src.size != dst.size
                        || src.disk.logical_sector_size != dst.disk.logical_sector_size
                    {
                        return Err("保持布局的克隆要求源与目标容量、逻辑扇区大小完全一致。".into());
                    }
                    (
                        if mode == crate::used_space::Mode::UsedSpace {
                            "按已用空间克隆"
                        } else {
                            "逐扇区克隆"
                        },
                        src.label,
                        dst.label,
                        src.size,
                        format!(
                            "{}{}",
                            overwrite_warning(),
                            if mode == crate::used_space::Mode::UsedSpace {
                                "只跳过确认空闲的区域；目标空闲区不会被清除，未知文件系统完整复制。保持分区布局，不调整容量。"
                            } else {
                                ""
                            }
                        ),
                        true,
                        Work::Clone {
                            source,
                            destination,
                            mode,
                        },
                    )
                }
                Request::Restore { destination } => {
                    let dst = platform::resolve(&destination, true, true)?;
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("选择 DiskStudio 备份")
                        .add_filter("DiskStudio 备份", &["dsbackup"])
                        .pick_file()
                    else {
                        return Ok(None);
                    };
                    let input = crate::backup_files::Input::open(&path, Some(&destination.target))?;
                    if input.chunked() {
                        let Some(chain) = choose_chain(input, &destination.target)? else {
                            return Ok(None);
                        };
                        let h = chain.header();
                        if h.size != dst.size
                            || Some(h.sector) != dst.disk.logical_sector_size
                            || (h.kind == "partition") != destination.partition.is_some()
                        {
                            return Err("备份类型、容量或逻辑扇区与目标不匹配。".into());
                        }
                        (
                            "从备份还原",
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                            dst.label,
                            h.size,
                            format!(
                                "写入前验证完整备份链、每个数据块及最终 SHA-256。{}",
                                overwrite_warning()
                            ),
                            true,
                            Work::ArchiveRestore { destination, chain },
                        )
                    } else {
                        let mut file = input.file;
                        let header = read_header(&mut file)?;
                        if header.size != dst.size
                            || Some(header.sector) != dst.disk.logical_sector_size
                            || (header.kind == "partition") != destination.partition.is_some()
                        {
                            return Err(
                                "备份类型、容量或逻辑扇区与目标不匹配。请选择同容量的磁盘/分区。"
                                    .into(),
                            );
                        }
                        (
                            "从备份还原",
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                            dst.label,
                            header.size,
                            format!("写入前先验证备份 SHA-256。{}", overwrite_warning()),
                            true,
                            Work::Restore {
                                destination,
                                file,
                                header,
                                binding: input.binding,
                            },
                        )
                    }
                }
                Request::Recover {
                    scan_id,
                    file_ids,
                    folder: selected_folder,
                    layout,
                    options,
                } => {
                    let (source, files, mut export, browsing) = {
                        let s = state();
                        let (id, source) = s.scan.as_ref().ok_or("尚无扫描结果")?;
                        if *id != scan_id {
                            return Err("扫描结果已失效，请重新选择。".into());
                        }
                        if selected_folder.is_some()
                            && !directory_export_available(&s, selected_folder.as_deref())
                        {
                            return Err(
                                "当前扫描尚未确认此目录；文件签名结果不能用于恢复原目录。".into()
                            );
                        }
                        let (files, export) = recovery_export::select(
                            &s.candidates,
                            &s.directories,
                            &file_ids,
                            selected_folder.as_deref(),
                            layout,
                        )?;
                        (
                            source.clone(),
                            files,
                            export,
                            matches!(s.scan_mode, ScanMode::Existing),
                        )
                    };
                    let source_label = source.label()?;
                    export.options = options.validate()?;
                    if let ScanSource::Backup { opened, partition } = &source {
                        export.backup_source = Some(opened.browser.provenance(*partition)?);
                    }
                    if let ScanSource::Image { opened, partition } = &source {
                        export.image_source = Some(opened.browser.provenance(*partition)?);
                    }
                    let size = export.total_bytes;
                    let Some(folder) = rfd::FileDialog::new()
                        .set_title(if browsing {
                            "复制到另一块磁盘的目录"
                        } else {
                            "恢复到另一块磁盘的目录"
                        })
                        .pick_folder()
                    else {
                        return Ok(None);
                    };
                    let folder = source.destination(&folder, size)?;
                    let warning=format!("{} 输出到新建目录，并保存文件名映射与成功/失败清单；不覆盖已有文件。每个文件在发布前完整回读并核对 SHA-256；不重建权限、链接及扩展属性；文件与目录时间、受支持属性按复制选项保存并记录结果。{}",export.description(), if matches!(source, ScanSource::Image { .. }) { "源镜像以禁止共享写入的文件句柄保持打开，不挂载、不修改源。已删除内容可能已被覆盖，导出成功不代表原内容完整。" } else if browsing { "源卷未锁定，在线文件可能变化；这不是时间点快照，请核对副本内容。" } else { "已删除内容可能被覆盖或被 TRIM 清除，导出成功不代表原内容完整。" });
                    (
                        if export.directory_selection {
                            if browsing {
                                "复制目录"
                            } else {
                                "恢复目录"
                            }
                        } else if browsing {
                            "复制所选文件"
                        } else {
                            "恢复所选文件"
                        },
                        source_label,
                        folder.display().to_string(),
                        size,
                        warning,
                        false,
                        Work::Recover {
                            source,
                            files,
                            folder,
                            export,
                        },
                    )
                }
            };
        let plan = Plan {
            token: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            source: source_label,
            destination: destination_label,
            bytes,
            warning,
            destructive,
        };
        state().pending = Some(Pending {
            plan: plan.clone(),
            work,
            expires: Instant::now() + Duration::from_secs(300),
        });
        Ok(Some(plan))
    }
}
fn overwrite_warning() -> String {
    "目标范围原有的文件系统及数据将被替换，不能撤销。执行前锁定卷并将整个目标磁盘脱机；完成、取消或失败后都保持脱机，避免重复 GPT 标识自动挂载。需在确认副本后由你决定何时联机。".into()
}
fn validate_pair(a: &Selection, b: &Selection) -> Result<(), String> {
    if a.target == b.target {
        return Err("源和目标必须是不同的物理磁盘。".into());
    }
    if a.partition.is_some() != b.partition.is_some() {
        return Err("整盘只能克隆到整盘，分区只能克隆到分区。".into());
    }
    Ok(())
}
pub fn execute(token: String) -> Result<String, String> {
    let pending = {
        let mut s = state();
        idle(&s)?;
        let p = s.pending.take().ok_or("确认已失效，请重新预览。")?;
        if p.plan.token != token || Instant::now() > p.expires {
            return Err("确认已过期或不匹配。".into());
        }
        p
    };
    let (id, cancel) = start(&pending.plan.title)?;
    let work_id = id.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = crate::OPERATION
                .try_lock()
                .map_err(|_| "另一项磁盘操作正在执行。")?;
            run(&work_id, &cancel, pending.work)
        })).unwrap_or_else(|_|Err("数据任务异常退出。目标可能已部分写入，请勿当作完整副本使用；重新启动应用后检查状态。".into()));
        finish(&work_id, result);
    });
    Ok(id)
}
#[cfg(windows)]
fn choose_chain(
    input: crate::backup_files::Input,
    other_disk: &Target,
) -> Result<Option<Chain>, String> {
    let Some(archive) = input.archive(|| Ok(()))? else {
        return Ok(None);
    };
    let mut chain = Chain::new(archive);
    while let Some(id) = chain.missing_parent() {
        if chain.depth() >= backup_archive::MAX_CHAIN {
            return Err("备份链超过 16 层".into());
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title(format!("选择依赖的基础备份 · {id}"))
            .add_filter("DiskStudio 备份", &["dsbackup"])
            .pick_file()
        else {
            return Ok(None);
        };
        let input = crate::backup_files::Input::open(&path, Some(other_disk))?;
        let Some(archive) = input.archive(|| Ok(()))? else {
            return Ok(None);
        };
        chain.add_parent(archive)?;
    }
    Ok(Some(chain))
}
#[cfg(not(windows))]
fn run(_: &str, _: &Arc<AtomicBool>, _: Work) -> Result<(String, Option<String>), String> {
    Err("仅支持 Windows".into())
}
#[cfg(windows)]
pub(crate) fn run(
    id: &str,
    cancel: &Arc<AtomicBool>,
    work: Work,
) -> Result<(String, Option<String>), String> {
    let err = |e: std::io::Error| e.to_string();
    match work {
        Work::ArchiveBackup {
            source,
            path,
            options,
            mut parent,
        } => {
            validate_backup_options(&options)?;
            let r = platform::resolve(&source, true, false)?;
            platform::destination_folder(
                path.parent().ok_or("输出目录无效")?,
                &source.target,
                if options.mode == crate::used_space::Mode::UsedSpace {
                    0
                } else {
                    backup_output_bound(r.size, r.size, options.encryption)?
                },
            )?;
            if options.incremental != parent.is_some() {
                return Err("增量备份依赖缺失".into());
            }
            if let Some(p) = &mut parent {
                p.verify(|n| progress(id, cancel, "验证基础备份", n, r.size))?;
            }
            let password = if options.encryption {
                let password = crate::backup_secrets::ask(
                    crate::backup_secrets::Purpose::Create,
                    &path.file_name().ok_or("文件名无效")?.to_string_lossy(),
                    || progress(id, cancel, "等待设置备份密码", 0, r.size),
                )?;
                if password.is_none() {
                    cancel.store(true, Ordering::SeqCst);
                    return Err("已取消加密备份，未锁定源卷或创建输出。".into());
                }
                password
            } else {
                None
            };
            progress(id, cancel, "准备备份", 0, r.size)?;
            let locks = if options.hot {
                None
            } else {
                Some(platform::lock(&r)?)
            };
            let fresh = if let Some(locks) = &locks {
                platform::resolve_locked(&source, false, &r, locks)
                    .map_err(|e| format!("备份源卷锁复核失败：{e}"))?
            } else {
                platform::resolve(&source, true, false)?
            };
            if let Some(parent) = &parent {
                parent.validate_sources()?;
            }
            let snapshot = if options.hot {
                if source.partition.is_none() {
                    return Err("热备仅支持 NTFS 数据分区".into());
                }
                Some(crate::snapshot::create(&fresh, || {
                    progress(id, cancel, "创建 VSS 快照", 0, r.size)
                })?)
            } else {
                None
            };
            let mut reader: Box<dyn backup_archive::Source> = if let Some(s) = &snapshot {
                Box::new(s.reader()?)
            } else if options.mode == crate::used_space::Mode::UsedSpace {
                Box::new(exact_reader(&fresh)?)
            } else {
                Box::new(platform::reader(&fresh)?)
            };
            let allocation = if options.mode == crate::used_space::Mode::UsedSpace {
                progress(id, cancel, "检查文件系统分配信息", 0, r.size)?;
                let plan = crate::used_space::inspect(
                    &mut reader,
                    r.size,
                    r.disk.logical_sector_size.unwrap(),
                    source
                        .partition
                        .is_none()
                        .then_some(fresh.partition_ranges.as_slice()),
                    cancel,
                )?;
                plan.proof.verify(&mut reader, cancel)?;
                Some(plan)
            } else {
                None
            };
            if let Some(locks) = &locks {
                platform::resolve_locked(&source, false, &fresh, locks)?;
            } else {
                platform::resolve(&source, true, false)?;
            }
            let payload = match &allocation {
                Some(plan) => plan.backup_payload_bound(backup_archive::CHUNK as u64)?,
                None => r.size,
            };
            let required = backup_output_bound(payload, r.size, options.encryption)?;
            reader.seek(SeekFrom::Start(0)).map_err(err)?;
            let (mut file, publication) =
                crate::backup_files::create(&path, &source.target, required, password.as_ref())?;
            drop(password);
            let header = {
                let mut content: Box<dyn Read + '_> = if let Some(plan) = &allocation {
                    Box::new(plan.masked(&mut reader))
                } else {
                    Box::new(&mut reader)
                };
                backup_archive::create_with_settings(
                    &mut content,
                    &mut file,
                    if source.partition.is_some() {
                        "partition"
                    } else {
                        "disk"
                    },
                    r.size,
                    r.disk.logical_sector_size.unwrap(),
                    options.compression(),
                    parent.as_ref(),
                    allocation
                        .as_ref()
                        .map(|p| backup_archive::AllocationPolicy {
                            skipped_bytes: p.skipped,
                            map_sha256: p.digest.clone(),
                        }),
                    |n| {
                        progress(
                            id,
                            cancel,
                            if options.compression {
                                "并行压缩备份"
                            } else {
                                "备份"
                            },
                            n,
                            r.size,
                        )
                    },
                )?
            };
            if let Some(plan) = &allocation {
                plan.proof.verify(&mut reader, cancel)?;
            }
            if let Some(locks) = &locks {
                platform::resolve_locked(&source, false, &fresh, locks)?;
            } else {
                platform::resolve(&source, true, false)?;
            }
            drop(reader);
            drop(snapshot);
            drop(locks);
            publication.validate()?;
            let binding = publication.binding();
            let archive = file.archive()?.bind_source(move || {
                platform::FileBinding::validate_all(std::slice::from_ref(&binding))
            })?;
            let mut chain = if let Some(p) = parent {
                p.prepend(archive)?
            } else {
                Chain::new(archive)
            };
            chain.verify(|n| progress(id, cancel, "校验备份链", n, r.size))?;
            drop(chain);
            progress(id, cancel, "保存已验证备份", r.size, r.size)?;
            publication.publish()?;
            Ok((
                format!(
                    "备份完成，全部逻辑内容已通过 SHA-256 校验。{}{}{}\n{}",
                    if options.encryption { "文件已加密。" } else { "" },
                    if options.incremental {
                        "还原时请保留全部基础备份。"
                    } else {
                        ""
                    },
                    allocation.as_ref().map(|p|format!("按已用空间备份：{} B 确认空闲区域按零保存，不包含这些区域的已删除内容。{}",p.skipped,allocation_notes(p))).unwrap_or_default(),
                    path.display()
                ),
                Some(header.sha256),
            ))
        }
        Work::ArchiveRestore {
            destination,
            mut chain,
        } => {
            let h = chain.header().clone();
            chain.verify(|n| progress(id, cancel, "验证备份链", n, h.size))?;
            let dst = platform::resolve(&destination, true, true)?;
            if dst.size != h.size
                || dst.disk.logical_sector_size != Some(h.sector)
                || (h.kind == "partition") != destination.partition.is_some()
            {
                return Err("目标范围已变化".into());
            }
            let _locks = platform::lock(&dst)?;
            let dst = platform::resolve_locked(&destination, true, &dst, &_locks)
                .map_err(|e| format!("还原目标卷锁复核失败：{e}"))?;
            chain.validate_sources()?;
            let target = platform::device(&dst, true)?;
            write_device(
                id,
                cancel,
                &mut chain.reader(),
                target,
                &dst,
                Some(&h.sha256),
            )
        }
        Work::Backup { source, path } => {
            let r = platform::resolve(&source, true, false)?;
            platform::destination_folder(
                path.parent().unwrap(),
                &source.target,
                r.size + HEADER_SIZE,
            )?;
            let locks = platform::lock(&r)?;
            let fresh = platform::resolve_locked(&source, false, &r, &locks)?;
            let mut reader = platform::reader(&fresh)?;
            progress(id, cancel, "准备备份", 0, r.size)?;
            let partial = PathBuf::from(format!("{}.partial", path.display()));
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&partial)
                .map_err(err)?;
            file.write_all(&vec![0; HEADER_SIZE as usize])
                .map_err(err)?;
            let hash = data_io::copy_exact(&mut reader, &mut file, r.size, |n| {
                progress(id, cancel, "备份", n, r.size)
            })?;
            let h = BackupHeader {
                version: 1,
                kind: if source.partition.is_some() {
                    "partition"
                } else {
                    "disk"
                }
                .into(),
                size: r.size,
                sector: r.disk.logical_sector_size.unwrap(),
                sha256: hash.clone(),
            };
            write_header(&mut file, &h)?;
            file.sync_all().map_err(err)?;
            platform::resolve_locked(&source, false, &fresh, &locks)?;
            drop(locks);
            file.seek(SeekFrom::Start(HEADER_SIZE)).map_err(err)?;
            let readback = data_io::hash_exact(&mut file, r.size, |n| {
                progress(id, cancel, "校验备份", n, r.size)
            })?;
            if hash != readback {
                return Err("备份写后校验失败，保留 .partial 文件。".into());
            }
            drop(file);
            rename_new(&partial, &path)?;
            Ok((
                format!("备份完成，SHA-256 写后校验一致。\n{}", path.display()),
                Some(hash),
            ))
        }
        Work::Clone {
            source,
            destination,
            mode,
        } => {
            validate_pair(&source, &destination)?;
            let src = platform::resolve(&source, true, false)?;
            let dst = platform::resolve(&destination, true, true)?;
            if src.size != dst.size || src.disk.logical_sector_size != dst.disk.logical_sector_size
            {
                return Err("源或目标容量已变化".into());
            }
            let _src_locks = platform::lock(&src)?;
            let _dst_locks = platform::lock(&dst)?;
            let src = platform::resolve_locked(&source, false, &src, &_src_locks)?;
            let dst = platform::resolve_locked(&destination, true, &dst, &_dst_locks)?;
            if mode == crate::used_space::Mode::UsedSpace {
                let mut reader = exact_reader(&src)?;
                progress(id, cancel, "检查文件系统分配信息", 0, src.size)?;
                let plan = crate::used_space::inspect(
                    &mut reader,
                    src.size,
                    src.disk.logical_sector_size.unwrap(),
                    source
                        .partition
                        .is_none()
                        .then_some(src.partition_ranges.as_slice()),
                    cancel,
                )?;
                plan.proof.verify(&mut reader, cancel)?;
                platform::resolve_locked(&source, false, &src, &_src_locks)?;
                let dst = platform::resolve_locked(&destination, true, &dst, &_dst_locks)?;
                let target = platform::device(&dst, true)?;
                return write_allocated(id, cancel, &mut reader, target, &dst, &plan);
            }
            let mut reader = platform::reader(&src)?;
            let target = platform::device(&dst, true)?;
            write_device(id, cancel, &mut reader, target, &dst, None)
        }
        Work::Restore {
            destination,
            mut file,
            header,
            binding,
        } => {
            platform::FileBinding::validate_all(std::slice::from_ref(&binding))?;
            let current = read_header(&mut file)?;
            if current != header {
                return Err("备份头已变化".into());
            }
            file.seek(SeekFrom::Start(HEADER_SIZE)).map_err(err)?;
            let actual = data_io::hash_exact(&mut file, header.size, |n| {
                progress(id, cancel, "验证备份", n, header.size)
            })?;
            if actual != header.sha256 {
                return Err("备份 SHA-256 不匹配，未修改目标磁盘。".into());
            }
            let dst = platform::resolve(&destination, true, true)?;
            if dst.size != header.size || dst.disk.logical_sector_size != Some(header.sector) {
                return Err("目标容量或扇区大小已变化".into());
            }
            let _locks = platform::lock(&dst)?;
            let dst = platform::resolve_locked(&destination, true, &dst, &_locks)
                .map_err(|e| format!("旧版备份还原目标卷锁复核失败：{e}"))?;
            platform::FileBinding::validate_all(std::slice::from_ref(&binding))?;
            let target = platform::device(&dst, true)?;
            file.seek(SeekFrom::Start(HEADER_SIZE)).map_err(err)?;
            write_device(id, cancel, &mut file, target, &dst, Some(&header.sha256))
        }
        Work::Recover {
            source,
            files,
            folder,
            export,
        } => {
            let total = export.total_bytes;
            source.label()?; // Revalidate retained source identity/layout even for directory-only exports.
            progress(id, cancel, "恢复文件", 0, total)?;
            let root = source.destination(&folder, total.saturating_add(32 * 1024 * 1024))?;
            let (_, expected) = platform::folder_location(&root)?;
            let forbidden = source.forbidden_disks();
            let mut output = crate::recovery_output::Destination::new_checked(&root, |parent| {
                let actual = platform::file_location(parent)?;
                if actual.target != expected || forbidden.contains(&actual.target) {
                    return Err("输出目录实际所在磁盘已变化或与源盘相同，未创建输出".into());
                }
                Ok(())
            })?;
            let files: std::collections::HashMap<_, _> = files.iter().map(|c| (c.id, c)).collect();
            let phase = format!("提取文件 · 并发 {}", export.options.workers);
            let batch = recovery_export::execute_parallel_with_directories(
                &export,
                &mut output,
                |entry, out, tick| {
                    let c = files.get(&entry.id).ok_or("导出文件身份缺失")?;
                    if c.size != entry.size {
                        return Err("导出容量与计划不一致".into());
                    }
                    let reader = source.reader()?;
                    let extracted = recovery::extract_owned_with_metadata(
                        reader,
                        c,
                        out,
                        u64::MAX,
                        cancel.clone(),
                        tick,
                    )?;
                    out.extracted_metadata(extracted.metadata);
                    Ok(extracted.bytes)
                },
                |directory| {
                    crate::directory_metadata::read(source.reader()?, directory, cancel.clone())
                },
                |done| progress(id, cancel, &phase, done, total),
            )
            .map_err(|e| format!("{e}\n输出目录：{}", output.display.display()))?;
            let directory_warnings = batch
                .directories
                .iter()
                .filter(|o| o.metadata.as_ref().is_some_and(|m| m.warnings() > 0))
                .count();
            let outcomes = batch.files;
            let recovered = outcomes.iter().filter(|o| o.saved).count();
            let failed = outcomes.len() - recovered;
            let metadata_warnings = outcomes
                .iter()
                .filter(|o| o.metadata.as_ref().is_some_and(|m| m.warnings() > 0))
                .count();
            let errors: Vec<_> = outcomes
                .iter()
                .filter_map(|o| o.error.as_ref().map(|e| format!("文件 {}：{e}", o.id)))
                .take(10)
                .collect();
            let message = format!(
                "已导出 {recovered} 个文件，失败 {failed} 个，跳过 {} 个；创建 {} 个子目录。成功文件均已通过完整 SHA-256 回读校验；{metadata_warnings} 个文件、{directory_warnings} 个目录有时间或属性保留提示，详见结果清单。不代表已删除的原始内容完整。结果见 DiskStudio-export.json。\n{}\n{}",
                export.skipped.len(), export.folders.len().saturating_sub(1), output.display.display(),
                errors.join("\n")
            );
            if failed > 0 || !export.skipped.is_empty() {
                Err(message)
            } else {
                Ok((message, None))
            }
        }
    }
}
#[cfg(windows)]
fn exact_reader(source: &platform::Resolved) -> Result<crate::used_space::View<File>, String> {
    crate::used_space::View::bounded(
        platform::device(source, false)?,
        source.offset,
        source.size,
        source.disk.size,
    )
    .map_err(|e| e.to_string())
}
#[cfg(windows)]
fn allocation_notes(plan: &crate::used_space::Plan) -> String {
    plan.volumes
        .iter()
        .filter(|v| !v.note.is_empty())
        .take(16)
        .map(|v| format!(" {}：{}", v.filesystem, v.note))
        .collect()
}
#[cfg(windows)]
fn write_allocated(
    id: &str,
    cancel: &Arc<AtomicBool>,
    source: &mut (impl Read + Seek + Send),
    target: File,
    dst: &platform::Resolved,
    plan: &crate::used_space::Plan,
) -> Result<(String, Option<String>), String> {
    if plan.size != dst.size || Some(plan.sector) != dst.disk.logical_sector_size {
        return Err("已用空间复制的目标几何不匹配".into());
    }
    progress(id, cancel, "锁定目标", 0, plan.copied())?;
    platform::offline(&target)?;
    let result = (|| {
        let mut target =
            crate::used_space::View::bounded(target, dst.offset, dst.size, dst.disk.size)
                .map_err(|e| e.to_string())?;
        let hash = plan.copy_verified(
            source,
            &mut target,
            cancel.clone(),
            |t| t.inner_mut().sync_all().map_err(|e| e.to_string()),
            |phase, n, total| {
                progress(
                    id,
                    cancel,
                    match phase {
                        crate::used_space::Phase::Copying => "复制已用范围",
                        crate::used_space::Phase::Verifying => "回读校验已复制范围",
                    },
                    n,
                    total,
                )
            },
        )?;
        plan.proof.verify(source, cancel)?;
        Ok((format!("按已用空间克隆完成，复制 {} B，跳过 {} B。已复制范围及分配计划通过 SHA-256 完整回读校验；目标空闲区未清除。目标磁盘保持脱机，未修改分区容量或重复 GPT 标识。{}",plan.copied(),plan.skipped,allocation_notes(plan)),Some(hash)))
    })();
    result.map_err(|e: String| {
        format!("{e}。目标磁盘保持脱机，可能仅完成部分覆盖；未自动重试或回滚。")
    })
}
#[cfg(windows)]
fn write_device(
    id: &str,
    cancel: &AtomicBool,
    reader: &mut (impl Read + Send),
    mut target: File,
    dst: &platform::Resolved,
    expected: Option<&str>,
) -> Result<(String, Option<String>), String> {
    progress(id, cancel, "锁定目标", 0, dst.size)?;
    platform::offline(&target)?;
    let result = (|| {
        target
            .seek(SeekFrom::Start(dst.offset))
            .map_err(|e| e.to_string())?;
        let hash = data_io::copy_exact(reader, &mut target, dst.size, |n| {
            progress(id, cancel, "写入目标", n, dst.size)
        })?;
        target.sync_all().map_err(|e| e.to_string())?;
        if expected.is_some_and(|h| h != hash) {
            return Err("写入流的校验值与备份不一致".into());
        }
        target
            .seek(SeekFrom::Start(dst.offset))
            .map_err(|e| e.to_string())?;
        let readback = data_io::hash_exact(&mut target, dst.size, |n| {
            progress(id, cancel, "校验目标", n, dst.size)
        })?;
        if hash != readback {
            return Err("写后 SHA-256 校验失败".into());
        }
        Ok(("复制完成，目标全部字节已通过 SHA-256 校验。目标磁盘保持脱机；未自动更改 GPT 标识或扩展分区。".into(),Some(hash)))
    })();
    result.map_err(|e: String| {
        format!("{e}。目标磁盘保持脱机，可能只完成部分覆盖，请勿当作完整副本使用。")
    })
}
#[cfg(windows)]
pub(crate) fn rename_new(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 0) }
        == 0
    {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup_archive::Archive;
    fn memory_backup() -> OpenedBackup {
        let mut archive = std::io::Cursor::new(Vec::new());
        backup_archive::create(
            &mut &vec![0u8; 512][..],
            &mut archive,
            "partition",
            512,
            512,
            true,
            None,
            |_| Ok(()),
        )
        .unwrap();
        archive.set_position(0);
        let browser = crate::backup_browser::Browser::from_chain(
            Chain::new(Archive::from_reader(archive).unwrap()),
            "memory.dsbackup".into(),
            |_, _| Ok(()),
        )
        .unwrap();
        OpenedBackup {
            browser: Arc::new(browser),
            source_disks: vec![],
            #[cfg(windows)]
            source_bindings: vec![],
        }
    }
    #[test]
    fn backup_publication_and_cancellation_have_one_commit_boundary() {
        let old = memory_backup();
        let new = memory_backup();
        let old_id = old.browser.info.id.clone();
        let new_id = new.browser.info.id.clone();
        let mut s = State::default();
        s.opened_backup = Some(old.clone());
        s.scan = Some((
            "old-results".into(),
            ScanSource::Backup {
                opened: old,
                partition: 0,
            },
        ));
        s.scan_name = "old results".into();
        s.job = Some(Job {
            id: "job".into(),
            title: "switch".into(),
            state: "running".into(),
            phase: "verify".into(),
            done: 512,
            total: 512,
            found: 0,
            message: String::new(),
            sha256: None,
        });
        assert!(commit_backup(&mut s, "stale-job", None, new.clone(), "done").is_err());
        assert!(commit_backup(&mut s, "job", Some("stale-session"), new.clone(), "done").is_err());
        cancel_in(&s, "job").unwrap();
        assert!(commit_backup(&mut s, "job", Some(&old_id), new.clone(), "done").is_err());
        assert_eq!(s.opened_backup.as_ref().unwrap().browser.info.id, old_id);
        assert_eq!(s.scan.as_ref().unwrap().0, "old-results");
        assert_eq!(s.scan_name, "old results");
        // Independent commit-first ordering: late cancellation must be rejected.
        s.cancel = Arc::new(AtomicBool::new(false));
        commit_backup(&mut s, "job", Some(&old_id), new, "done").unwrap();
        assert_eq!(s.opened_backup.as_ref().unwrap().browser.info.id, new_id);
        assert!(s.scan.is_none());
        assert!(s.scan_name.is_empty());
        assert_eq!(s.job.as_ref().unwrap().state, "completed");
        assert!(cancel_in(&s, "job").is_err());
        assert!(!s.cancel.load(Ordering::Relaxed));
    }
    #[test]
    fn partition_browser_matches_only_retained_existing_source_without_io() {
        let mut s = State::default();
        let source = selection("a", Some(2));
        assert_eq!(matching_browse_id(&s, &source), None);
        s.scan = Some(("files".into(), source.clone().into()));
        assert_eq!(matching_browse_id(&s, &source), None);
        s.scan_mode = ScanMode::Existing;
        assert_eq!(matching_browse_id(&s, &source).as_deref(), Some("files"));
        for other in [
            selection("b", Some(2)),
            selection("a", Some(1)),
            selection("a", None),
            Selection {
                layout_key: "changed".into(),
                ..source.clone()
            },
        ] {
            assert_eq!(matching_browse_id(&s, &other), None);
        }
        let html = crate::directory_report::render(
            &s.directories,
            &s.candidates,
            s.scan_mode,
            crate::directory_report::Completion::Ended,
        )
        .unwrap();
        assert!(html.contains("浏览现存文件"));
        s.scan_mode = ScanMode::Deleted;
        assert_eq!(matching_browse_id(&s, &source), None);
        let mut json = serde_json::json!({"target":"a".repeat(64),"layoutKey":"x","partition":2});
        json["path"] = "C:\\never-open".into();
        assert!(serde_json::from_value::<Selection>(json).is_err());
    }
    fn selection(id: &str, part: Option<u32>) -> Selection {
        Selection {
            target: Target(id.repeat(64)),
            layout_key: "layout".into(),
            partition: part,
        }
    }
    #[test]
    fn scan_status_uses_retained_candidate_context_without_device_access() {
        let mut s = State::default();
        assert!(scan_status_in(&s).is_none());
        let bytes = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let range = crate::surface_scan::Range {
            offset: 0,
            size: bytes.len() as u64,
            sector: 512,
        };
        let mut candidate = crate::lost_partitions::probe(
            &mut std::io::Cursor::new(bytes),
            range,
            0,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        candidate.offset = 32768 * 512;
        s.scan_name = "retained disk / NTFS / LBA 32768".into();
        s.scan = Some((
            "candidate-scan".into(),
            ScanSource::Lost {
                source: selection("a", None),
                range,
                candidate,
            },
        ));
        let json = serde_json::to_value(scan_status_in(&s).unwrap()).unwrap();
        assert_eq!(json["id"], "candidate-scan");
        assert_eq!(json["sourceName"], s.scan_name);
        assert_eq!(json["lost"]["filesystem"], "NTFS");
        assert_eq!(json["lost"]["startSector"], 32768);
        assert!(json["lost"]["size"].as_u64().unwrap() > 0);
        assert!(!json.to_string().contains("fingerprint"));
        s.scan = Some(("regular".into(), selection("b", Some(1)).into()));
        assert!(scan_status_in(&s).unwrap().lost.is_none());
        s.scan_mode = ScanMode::Existing;
        assert!(scan_status_in(&s).is_none());
    }
    #[test]
    fn directory_report_request_is_snapshot_only_and_rejects_paths() {
        assert!(serde_json::from_value::<DirectoryReportRequest>(
            serde_json::json!({"scanId":"id"})
        )
        .is_ok());
        assert!(serde_json::from_value::<DirectoryReportRequest>(
            serde_json::json!({"scanId":"id", "path":"C:\\report.html"})
        )
        .is_err());
        let mut s = State::default();
        assert!(directory_report_snapshot(&s, "id").is_err());
        s.scan = Some(("id".into(), selection("a", Some(1)).into()));
        s.scan_name = "private hardware label must not appear".into();
        s.scan_completion = crate::directory_report::Completion::Canceled;
        assert!(directory_report_snapshot(&s, "stale").is_err());
        let (catalog, files, mode, completion) = directory_report_snapshot(&s, "id").unwrap();
        let html = crate::directory_report::render(&catalog, &files, mode, completion).unwrap();
        assert!(!html.contains(&s.scan_name));
        s.job = Some(Job {
            id: "other".into(),
            title: String::new(),
            state: "running".into(),
            phase: String::new(),
            done: 0,
            total: 0,
            found: 0,
            message: String::new(),
            sha256: None,
        });
        assert!(directory_report_snapshot(&s, "id").is_err());
    }
    #[test]
    fn cloning_rejects_same_disk_and_mixed_scope() {
        assert!(validate_pair(&selection("a", Some(1)), &selection("a", Some(2))).is_err());
        assert!(validate_pair(&selection("a", None), &selection("b", Some(1))).is_err());
        assert!(validate_pair(&selection("a", None), &selection("b", None)).is_ok());
    }
    #[test]
    fn allocation_mode_is_typed_and_hot_backups_require_the_full_sector_policy() {
        use crate::used_space::Mode;
        let options: BackupOptions =
            serde_json::from_str(r#"{"mode":"usedSpace","compression":true}"#).unwrap();
        assert_eq!(options.mode, Mode::UsedSpace);
        assert!(validate_backup_options(&options).is_ok());
        assert!(validate_backup_options(&BackupOptions {
            hot: true,
            ..options
        })
        .is_err());
        assert!(serde_json::from_str::<BackupOptions>(r#"{"mode":"guessFree"}"#).is_err());
        assert!(
            serde_json::from_str::<BackupOptions>(r#"{"mode":"usedSpace","path":"C:\\disk"}"#)
                .is_err()
        );
        let old: BackupOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(old.mode, Mode::AllSectors);
        assert!(!old.encryption);
        assert_eq!(old.compression(), backup_archive::Compression::None);
        let value = serde_json::json!({"type":"clone","source":selection("a",Some(1)),"destination":selection("b",Some(1)),"mode":"usedSpace"});
        assert!(matches!(
            serde_json::from_value::<Request>(value).unwrap(),
            Request::Clone {
                mode: Mode::UsedSpace,
                ..
            }
        ));
    }
    #[test]
    fn protected_backup_options_and_capacity_are_bounded_without_paths_or_passwords() {
        let options: BackupOptions = serde_json::from_str(
            r#"{"compression":true,"compressionLevel":"high","encryption":true}"#,
        )
        .unwrap();
        assert!(validate_backup_options(&options).is_ok());
        assert_eq!(options.compression(), backup_archive::Compression::High);
        assert!(options.encryption);
        assert!(validate_backup_options(&BackupOptions {
            compression_level: backup_archive::Compression::None,
            ..options
        })
        .is_err());
        for extra in ["password", "path", "key"] {
            assert!(serde_json::from_value::<BackupOptions>(
                serde_json::json!({extra:"not accepted"})
            )
            .is_err());
        }
        let plain = backup_output_bound(512, 512, false).unwrap();
        assert_eq!(plain, 512 + HEADER_SIZE + 56);
        assert_eq!(
            backup_output_bound(512, 512, true).unwrap(),
            crate::backup_encryption::storage_bound(plain).unwrap()
        );
        assert!(backup_output_bound(u64::MAX, u64::MAX, false).is_err());
        assert!(backup_output_bound(9 * 1024u64.pow(4), 512, true).is_err());
    }
    #[test]
    fn data_selection_rejects_device_paths() {
        assert!(serde_json::from_value::<Selection>(
            serde_json::json!({"target":"\\\\.\\PhysicalDrive0","layoutKey":"x","partition":null})
        )
        .is_err());
    }
    #[test]
    fn backup_header_rejects_truncation_bad_hash_and_wrong_sector() {
        let encode = |size: u64, sector: u64, hash: &str| {
            let json=serde_json::to_vec(&serde_json::json!({"version":1,"kind":"partition","size":size,"sector":sector,"sha256":hash})).unwrap();
            let mut b = vec![0; 4096];
            b[..8].copy_from_slice(b"DSBK0001");
            b[8..12].copy_from_slice(&(json.len() as u32).to_le_bytes());
            b[12..12 + json.len()].copy_from_slice(&json);
            b
        };
        let good = encode(512, 512, &"a".repeat(64));
        assert!(decode_header(&good, 4608).is_ok());
        assert!(decode_header(&good, 4607).is_err());
        assert!(decode_header(&good[..4000], 4608).is_err());
        assert!(decode_header(&encode(512, 1024, &"a".repeat(64)), 4608).is_err());
        assert!(decode_header(&encode(512, 512, &"z".repeat(64)), 4608).is_err());
    }
    #[test]
    fn directory_export_rejects_signature_and_unconfirmed_virtual_root_without_io() {
        use crate::directory_report::Completion;
        let mut s = State::default();
        assert!(!directory_export_available(&s, Some("root")));
        s.scan = Some(("scan".into(), selection("a", Some(5)).into()));
        s.scan_completion = Completion::Failed;
        assert!(!directory_export_available(&s, Some("root")));
        s.scan_completion = Completion::Ended;
        assert!(directory_export_available(&s, Some("root")));
        s.scan_mode = ScanMode::Signatures;
        assert!(!directory_export_available(&s, Some("root")));
        s.scan_mode = ScanMode::Existing;
        s.scan_completion = Completion::Paused;
        s.directories
            .insert(crate::recovery_tree::Directory {
                id: "observed".into(),
                parent: Some("root".into()),
                name: "已读取目录".into(),
                deleted: false,
                metadata: Default::default(),
            })
            .unwrap();
        assert!(directory_export_available(&s, Some("observed")));
        assert!(directory_export_available(&s, Some("root")));
        assert!(!directory_export_available(&s, Some("unknown")));
        assert!(!directory_export_available(&s, None));
    }
    #[test]
    fn recovery_request_accepts_typed_ids_not_paths() {
        let default = serde_json::from_value::<Request>(
            serde_json::json!({"type":"recover","scanId":"id","fileIds":[1,2]}),
        )
        .unwrap();
        assert!(matches!(
            default,
            Request::Recover {
                layout: Layout::Tree,
                ..
            }
        ));
        assert!(serde_json::from_value::<Request>(
            serde_json::json!({"type":"recover","scanId":"id","fileIds":[1],"layout":"flat"})
        )
        .is_ok());
        assert!(serde_json::from_value::<Request>(
            serde_json::json!({"type":"recover","scanId":"id","fileIds":[1],"layout":"C:\\"})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(
            serde_json::json!({"type":"recover","scanId":"id","fileIds":[1],"path":"C:\\"})
        )
        .is_err());
    }
    #[test]
    fn data_confirmation_expiry_discard_and_cancellation_are_enforced() {
        let pending = |token: &str, expires| Pending {
            plan: Plan {
                token: token.into(),
                title: "test".into(),
                source: "test".into(),
                destination: "test".into(),
                bytes: 512,
                warning: String::new(),
                destructive: false,
            },
            work: Work::Backup {
                source: selection("a", None),
                path: PathBuf::from("never-opened-test-file"),
            },
            expires,
        };
        state().pending = Some(pending("expired", Instant::now() - Duration::from_secs(1)));
        assert!(execute("expired".into()).is_err());
        assert!(state().pending.is_none());
        state().pending = Some(pending("once", Instant::now() + Duration::from_secs(5)));
        assert!(execute("wrong".into()).is_err());
        assert!(state().pending.is_none());
        state().pending = Some(pending("discard", Instant::now() + Duration::from_secs(5)));
        discard_plan("discard".into());
        assert!(state().pending.is_none());
        let (id, flag) = start("synthetic cancellation").unwrap();
        cancel(id.clone()).unwrap();
        assert!(progress(&id, &flag, "copy", 0, 512).is_err());
        finish(&id, Err("canceled".into()));
        assert_eq!(status().unwrap().state, "canceled");
        *state() = State::default();
    }
}

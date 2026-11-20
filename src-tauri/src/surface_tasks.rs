use crate::{
    data_tasks::{self, Selection},
    partitions::Target,
    surface_scan::{self, Options, Report},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard, OnceLock,
};

const MAX_RETAINED: usize = 64;
const MAX_ACTIVE: usize = 8;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub options: Options,
    #[serde(default)]
    pub refine_failures: bool,
    #[serde(default)]
    pub read_timeout_seconds: Option<u16>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub id: String,
    pub target: String,
    pub source: String,
    pub started_at: u64,
    pub report: Report,
    pub partition: Option<u32>,
    pub options: Options,
    pub loaded: bool,
    pub resume_count: u32,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Overview {
    target: String,
    state: String,
    checked_bytes: u64,
    size: u64,
    pausing: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    scans: Vec<Overview>,
    selected: Option<Snapshot>,
    repair_available: bool,
}
struct Active {
    number: u32,
    pause: Arc<AtomicBool>,
}
struct Saved {
    source: Selection,
    snapshot: Snapshot,
    active: Option<Active>,
    repair_used: bool,
}
impl Saved {
    fn can_repair(&self) -> bool {
        self.active.is_none()
            && !self.repair_used
            && !self.snapshot.loaded
            && self.snapshot.report.state == "completed"
            && self
                .snapshot
                .report
                .refinement
                .as_ref()
                .is_some_and(|refinement| {
                    refinement.ranges.iter().any(|range| {
                        range.resolution == surface_scan::FailureResolution::SectorReadFailed
                    })
                })
    }
}
#[derive(Default)]
struct Scans(Vec<Saved>);
impl Scans {
    fn result(&self, id: &str) -> Option<&Saved> {
        self.0.iter().find(|s| s.snapshot.id == id)
    }

    fn stopped(&self, id: &str) -> Result<(Selection, Snapshot), String> {
        let saved = self
            .result(id)
            .filter(|s| s.active.is_none())
            .ok_or("检测进度不存在或仍在运行。")?;
        Ok((saved.source.clone(), saved.snapshot.clone()))
    }

    fn worker(&mut self, id: &str) -> Option<&mut Saved> {
        self.0
            .iter_mut()
            .find(|s| s.snapshot.id == id && s.active.is_some())
    }

    fn pause(&mut self, id: &str) -> Result<(), String> {
        let active = self
            .worker(id)
            .and_then(|saved| saved.active.as_ref())
            .ok_or("检测已结束。")?;
        active.pause.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn can_start(&self, numbers: &[(u32, &Target)]) -> Result<(), String> {
        let mut count = numbers.len();
        for entry in &self.0 {
            if let Some(active) = &entry.active {
                if numbers.iter().any(|(number, target)| {
                    *number == active.number || **target == entry.source.target
                }) {
                    return Err("同一块磁盘已有检测正在运行。".into());
                }
                count += 1;
            }
        }
        if count > MAX_ACTIVE {
            return Err("最多同时检测 8 块磁盘。".into());
        }
        Ok(())
    }

    fn insert(&mut self, saved: Saved) {
        self.0.retain(|s| s.source.target != saved.source.target);
        self.0.push(saved);
        if self.0.len() > MAX_RETAINED {
            // Insertion is limited to eight active jobs. The oldest stopped
            // entry can always be replaced without touching an active reader.
            if let Some(oldest) = self.0.iter().position(|s| s.active.is_none()) {
                self.0.remove(oldest);
            }
        }
    }
}
fn state() -> MutexGuard<'static, Scans> {
    static STATE: OnceLock<Mutex<Scans>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(Scans::default()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}
pub fn status(target: &Target) -> Status {
    let scans = state();
    let selected = scans.0.iter().find(|s| &s.source.target == target);
    Status {
        scans: scans
            .0
            .iter()
            .map(|saved| Overview {
                target: saved.snapshot.target.clone(),
                state: saved.snapshot.report.state.clone(),
                checked_bytes: saved.snapshot.report.checked_bytes,
                size: saved.snapshot.report.size,
                pausing: saved
                    .active
                    .as_ref()
                    .is_some_and(|active| active.pause.load(Ordering::Relaxed)),
            })
            .collect(),
        selected: selected.map(|s| s.snapshot.clone()),
        repair_available: selected.is_some_and(Saved::can_repair),
    }
}

#[cfg(windows)]
pub(crate) fn repair_source(id: &str) -> Result<(Selection, Snapshot), String> {
    let scans = state();
    if !scans.result(id).is_some_and(Saved::can_repair) {
        return Err("请先完成本次坏道检测并细查读取失败的扇区。".into());
    }
    scans.stopped(id)
}

#[cfg(windows)]
pub(crate) fn mark_repair_used(id: &str) {
    if let Some(saved) = state().0.iter_mut().find(|s| s.snapshot.id == id) {
        saved.repair_used = true;
    }
}
pub fn start(requests: Vec<Request>) -> Result<(), String> {
    launch(requests, None)
}
fn validate_requests(requests: &[Request]) -> Result<(), String> {
    if requests.is_empty() || requests.len() > MAX_ACTIVE {
        return Err("请选择 1 到 8 块磁盘进行检测。".into());
    }
    for (index, request) in requests.iter().enumerate() {
        if requests[..index]
            .iter()
            .any(|other| request.source.target == other.source.target)
        {
            return Err("每块磁盘只能添加一次。".into());
        }
        surface_scan::validate_read_timeout(request.read_timeout_seconds)?;
    }
    Ok(())
}
fn launch(requests: Vec<Request>, resume: Option<Snapshot>) -> Result<(), String> {
    validate_requests(&requests)?;
    #[cfg(not(windows))]
    {
        let _ = resume;
        Err("坏道检测当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        use crate::data_windows as platform;
        let permit = || {
            crate::OPERATION
                .try_surface_scan()
                .map_err(|_| "另一项磁盘操作正在运行。")
        };
        let _batch = permit()?;
        data_tasks::ensure_idle()?;

        // Validate every requested scope before publishing or reading any job.
        let mut prepared: Vec<(Request, Saved)> = Vec::with_capacity(requests.len());
        for request in requests {
            let resolved = platform::resolve(&request.source, false, false)?;
            let number = resolved.disk.number;
            if prepared.iter().any(|(_, saved)| {
                saved
                    .active
                    .as_ref()
                    .is_some_and(|active| active.number == number)
            }) {
                return Err("每块磁盘只能添加一次。".into());
            }
            let range = request.options.range(
                resolved.offset,
                resolved.size,
                resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?,
            )?;
            let id = uuid::Uuid::new_v4().to_string();
            let mut snapshot = if let Some(previous) = &resume {
                if previous.target != request.source.target.0
                    || previous.report.range() != range
                    || previous.options != request.options
                    || previous.report.refinement.is_some() != request.refine_failures
                    || previous.report.checked_bytes == range.size
                    || previous.report.state == "running"
                    || previous.resume_count >= 1024
                {
                    return Err("原检测范围已变化或检测已完成，不能续扫。".into());
                }
                previous.report.validate()?;
                let mut snapshot = previous.clone();
                snapshot.id = id;
                snapshot.source = resolved.label;
                snapshot.resume_count += 1;
                snapshot
            } else {
                Snapshot {
                    id,
                    target: request.source.target.0.clone(),
                    source: resolved.label,
                    started_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    report: Report::new(range, request.refine_failures),
                    partition: request.source.partition,
                    options: request.options,
                    loaded: false,
                    resume_count: 0,
                }
            };
            snapshot.report.state = "running".into();
            snapshot.report.message.clear();
            snapshot.report.read_timeout_seconds = request.read_timeout_seconds;
            snapshot.report.timed_out_read = None;
            let saved = Saved {
                source: request.source.clone(),
                snapshot,
                active: Some(Active {
                    number,
                    pause: Arc::new(AtomicBool::new(false)),
                }),
                repair_used: false,
            };
            prepared.push((request, saved));
        }

        let mut scans = state();
        if let Some(previous) = &resume {
            if scans.result(&previous.id).is_none() {
                return Err("检测进度已被替换，请重新选择。".into());
            }
        }
        let numbers = prepared
            .iter()
            .map(|(_, saved)| (saved.active.as_ref().unwrap().number, &saved.source.target))
            .collect::<Vec<_>>();
        scans.can_start(&numbers)?;
        let mut ready = Vec::with_capacity(prepared.len());
        for (request, saved) in prepared {
            let id = saved.snapshot.id.clone();
            let active = saved.active.as_ref().unwrap();
            let number = active.number;
            let pause = active.pause.clone();
            let report = saved.snapshot.report.clone();
            let reader_permit = permit()?;
            std::thread::Builder::new()
                .spawn(move || {
                    let _permit = reader_permit;
                    // Registration is committed only after every worker exists.
                    // The startup lock prevents any early physical reads.
                    let registered = state().worker(&id).is_some();
                    if registered {
                        run(id, request, number, report, pause);
                    }
                })
                .map_err(|err| format!("无法启动检测线程：{err}"))?;
            ready.push(saved);
        }
        for saved in ready {
            scans.insert(saved);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn run(id: String, request: Request, number: u32, initial: Report, pause: Arc<AtomicBool>) {
    use crate::data_windows as platform;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<Report, String> {
            if pause.load(Ordering::Relaxed) {
                let mut report = initial;
                report.state = "canceled".into();
                return Ok(report);
            }
            let resolved = platform::resolve(&request.source, false, false)?;
            let range = request.options.range(
                resolved.offset,
                resolved.size,
                resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?,
            )?;
            if resolved.disk.number != number || initial.range() != range {
                return Err("检测范围已变化，请重新选择。".into());
            }
            let (mut reader, limit) =
                platform::surface_scan_device(&resolved, request.read_timeout_seconds)?;
            let mut report =
                surface_scan::continue_scan(&mut reader, initial, limit, &pause, |report| {
                    if let Some(saved) = state().worker(&id) {
                        saved.snapshot.report = report.clone();
                        saved.snapshot.report.state = "running".into();
                        saved.snapshot.report.message.clear();
                    }
                })?;
            drop(reader);
            if report.state == "completed" {
                if let Err(err) = platform::resolve(&request.source, false, false) {
                    report.state = "failed".into();
                    report.message =
                        format!("检测结束时设备或布局核对失败：{err}。结果仅对应已读取范围。");
                }
            }
            Ok(report)
        },
    ))
    .unwrap_or_else(|_| Err("检测线程异常结束；源磁盘未写入。".into()));
    if let Some(saved) = state().worker(&id) {
        let report = &mut saved.snapshot.report;
        match result {
            Ok(final_report) => *report = final_report,
            Err(err) => {
                report.state = "failed".into();
                report.message = err;
            }
        }
        if pause.load(Ordering::Relaxed) && report.state == "canceled" {
            report.state = "paused".into();
            report.message = "检测已暂停，可以保存进度或从未完成位置继续。".into();
        }
        saved.active = None;
    }
}

pub fn pause(id: String) -> Result<(), String> {
    state().pause(&id)
}
pub fn resume(id: String, read_timeout_seconds: Option<u16>) -> Result<(), String> {
    let (source, snapshot) = state().stopped(&id)?;
    launch(
        vec![Request {
            source,
            options: snapshot.options,
            refine_failures: snapshot.report.refinement.is_some(),
            read_timeout_seconds,
        }],
        Some(snapshot),
    )
}
pub fn save_progress(id: String) -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("仅 Windows 支持保存进度。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        data_tasks::ensure_idle()?;
        let (source, snapshot) = state().stopped(&id)?;
        let bytes = crate::surface_checkpoint::encode(crate::surface_checkpoint::Progress {
            source: source.clone(),
            options: snapshot.options,
            snapshot,
        })?;
        save_file(
            &source,
            &bytes,
            "保存检测进度到另一块磁盘",
            "dsscan",
            "DiskStudio-progress.dsscan",
        )
    }
}
pub fn load_progress(target: Target) -> Result<Option<Snapshot>, String> {
    #[cfg(not(windows))]
    {
        let _ = target;
        Err("仅 Windows 支持加载进度。".into())
    }
    #[cfg(windows)]
    {
        use std::{
            fs::OpenOptions,
            io::Read,
            os::windows::fs::{MetadataExt, OpenOptionsExt},
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        data_tasks::ensure_idle()?;
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("加载当前磁盘的检测进度"))
            .add_filter(
                crate::native_locale::text("DiskStudio 检测进度"),
                &["dsscan"],
            )
            .pick_file()
        else {
            return Ok(None);
        };
        let normal = path.to_string_lossy();
        let b = normal.as_bytes();
        if b.len() < 4
            || !b[0].is_ascii_alphabetic()
            || &b[1..3] != b":\\"
            || normal[2..].contains(':')
            || !path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("dsscan"))
        {
            return Err("请选择本机普通文件路径下的 .dsscan 进度文件。".into());
        }
        let canonical = path.canonicalize().map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&canonical)
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.len() > crate::surface_checkpoint::MAX_FILE
        {
            return Err("进度必须为不超过 2 MiB 的普通文件。".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(crate::surface_checkpoint::MAX_FILE + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let mut progress = crate::surface_checkpoint::decode(&bytes, &target)?;
        progress.snapshot.id = uuid::Uuid::new_v4().to_string();
        progress.snapshot.loaded = true;
        let snapshot = progress.snapshot.clone();
        state().insert(Saved {
            source: progress.source,
            snapshot: progress.snapshot,
            active: None,
            repair_used: false,
        });
        Ok(Some(snapshot))
    }
}
pub fn export(
    id: String,
    format: crate::surface_report::ReportFormat,
) -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = (id, format);
        Err("当前仅 Windows 支持保存检测报告。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前任务完成。")?;
        data_tasks::ensure_idle()?;
        let (source, snapshot) = state().stopped(&id)?;
        let bytes =
            crate::surface_report::encode(format, &snapshot, &crate::native_locale::current())?;
        let (extension, name) = format.file_name();
        save_file(
            &source,
            &bytes,
            "保存检测报告到另一块磁盘（使用新文件名）",
            extension,
            name,
        )
    }
}
#[cfg(windows)]
fn save_file(
    source: &Selection,
    bytes: &[u8],
    title: &str,
    extension: &str,
    name: &str,
) -> Result<Option<String>, String> {
    use crate::data_windows as platform;
    use std::{fs::OpenOptions, io::Write};
    let Some(path) = rfd::FileDialog::new()
        .set_title(crate::native_locale::text(title))
        .add_filter(
            crate::native_locale::text("DiskStudio 检测文件"),
            &[extension],
        )
        .set_file_name(name)
        .save_file()
    else {
        return Ok(None);
    };
    if !path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(extension))
    {
        return Err(format!("文件需要使用 .{extension} 扩展名。"));
    }
    let (folder, destination) = platform::destination_location(
        path.parent().ok_or("输出目录无效")?,
        std::slice::from_ref(&source.target),
        bytes.len() as u64,
    )?;
    let name = path.file_name().ok_or("输出文件名无效")?;
    if name.to_string_lossy().contains(':') {
        return Err("不支持数据流文件名。".into());
    }
    let output = folder.join(name);
    if output.try_exists().map_err(platform::err)? {
        return Err("文件已存在，请使用新文件名。".into());
    }
    let partial = folder.join(format!(
        "DiskStudio-surface-{}.partial",
        uuid::Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(platform::err)?;
    let output_binding = platform::FileBinding::new(&file, &destination)?;
    platform::FileBinding::validate_all(&[output_binding])?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("报告未写完：{e}；保留 {}", partial.display()))?;
    drop(file);
    data_tasks::rename_new(&partial, &output)?;
    Ok(Some(
        output
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: &str) -> Request {
        Request {
            source: Selection {
                target: Target(target.into()),
                layout_key: "layout".into(),
                partition: None,
            },
            options: Options {
                start_sector: 0,
                sector_count: 4096,
            },
            refine_failures: false,
            read_timeout_seconds: None,
        }
    }

    fn saved(id: &str, target: &str, number: u32) -> Saved {
        let request = request(target);
        let range = request.options.range(0, 2097152, 512).unwrap();
        Saved {
            source: request.source,
            snapshot: Snapshot {
                id: id.into(),
                target: target.into(),
                source: target.into(),
                started_at: 0,
                report: Report::new(range, false),
                partition: None,
                options: request.options,
                loaded: false,
                resume_count: 0,
            },
            active: Some(Active {
                number,
                pause: Arc::new(AtomicBool::new(false)),
            }),
            repair_used: false,
        }
    }

    #[test]
    fn duplicate_targets_rejected_even_when_they_select_different_partitions() {
        let mut second = request("disk-a");
        second.source.partition = Some(2);
        assert!(validate_requests(&[request("disk-a"), second]).is_err());
        assert!(validate_requests(&[request("disk-a"), request("disk-b")]).is_ok());
    }

    #[test]
    fn pausing_and_replacing_one_disk_leaves_other_reader_intact() {
        let mut scans = Scans::default();
        scans.insert(saved("first", "disk-a", 1));
        scans.insert(saved("second", "disk-b", 2));
        assert!(scans.can_start(&[(2, &Target("disk-c".into()))]).is_err());
        assert!(scans.can_start(&[(3, &Target("disk-a".into()))]).is_err());
        assert!(scans.can_start(&[(3, &Target("disk-c".into()))]).is_ok());
        assert!(scans.stopped("first").is_err());
        scans.pause("first").unwrap();
        for (id, expected) in [("first", true), ("second", false)] {
            let active = scans.result(id).unwrap().active.as_ref().unwrap();
            assert_eq!(active.pause.load(Ordering::Relaxed), expected);
        }
        let first = scans.worker("first").unwrap();
        first.active = None;
        first.snapshot.report.state = "paused".into();
        assert!(scans.stopped("first").is_ok());
        scans.insert(saved("resumed", "disk-a", 1));
        assert!(scans.pause("first").is_err());
        assert!(scans.worker("second").is_some());
        assert!(scans.worker("resumed").is_some());
    }
}

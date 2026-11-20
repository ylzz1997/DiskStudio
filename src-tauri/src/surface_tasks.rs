use crate::{
    data_tasks::{self, Selection},
    surface_scan::{self, Options, Report},
};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

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
#[derive(Clone)]
struct Saved {
    source: Selection,
    snapshot: Snapshot,
    pause_requested: bool,
}
fn state() -> std::sync::MutexGuard<'static, Option<Saved>> {
    static STATE: OnceLock<Mutex<Option<Saved>>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn status() -> Option<Snapshot> {
    state().as_ref().map(|s| s.snapshot.clone())
}
pub fn start(request: Request) -> Result<String, String> {
    launch(request, None)
}
fn launch(request: Request, resume: Option<Snapshot>) -> Result<String, String> {
    surface_scan::validate_read_timeout(request.read_timeout_seconds)?;
    #[cfg(not(windows))]
    {
        let _ = (request, resume);
        Err("坏道检测当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        use crate::data_windows as platform;
        // Reserving the global job prevents parallel jobs and window closure.
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let resolved = platform::resolve(&request.source, false, false)?;
        let range = request.options.range(
            resolved.offset,
            resolved.size,
            resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?,
        )?;
        let previous = if let Some(snapshot) = resume {
            if state()
                .as_ref()
                .is_none_or(|saved| saved.snapshot.id != snapshot.id)
            {
                return Err("检测进度已被替换，请重新选择。".into());
            }
            if snapshot.report.range() != range
                || snapshot.options != request.options
                || snapshot.report.refinement.is_some() != request.refine_failures
                || snapshot.report.checked_bytes == range.size
                || snapshot.report.state == "running"
                || snapshot.resume_count >= 1024
            {
                return Err("原检测范围已变化或检测已完成，不能续扫。".into());
            }
            snapshot.report.validate()?;
            Some(snapshot)
        } else {
            None
        };
        let (id, cancel) = data_tasks::start("坏道检测")?;
        let mut snapshot = if let Some(mut previous) = previous {
            previous.id = id.clone();
            previous.source = resolved.label;
            previous.resume_count += 1;
            previous
        } else {
            Snapshot {
                id: id.clone(),
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
        let initial_report = snapshot.report.clone();
        *state() = Some(Saved {
            source: request.source.clone(),
            snapshot,
            pause_requested: false,
        });
        drop(guard);
        let work_id = id.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<Report, String> {
                    let _guard = crate::OPERATION
                        .try_lock()
                        .map_err(|_| "另一项磁盘操作正在运行。")?;
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err("检测已取消，尚未读取设备。".into());
                    }
                    let resolved = platform::resolve(&request.source, false, false)?;
                    let range = request.options.range(
                        resolved.offset,
                        resolved.size,
                        resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?,
                    )?;
                    if initial_report.range() != range {
                        return Err("检测范围已变化，请重新选择。".into());
                    }
                    let (mut reader, limit) =
                        platform::surface_scan_device(&resolved, request.read_timeout_seconds)?;
                    let mut report = surface_scan::continue_scan(
                        &mut reader,
                        initial_report,
                        limit,
                        &cancel,
                        |report| {
                            data_tasks::record_progress(
                                &work_id,
                                "逐块读取",
                                report.checked_bytes,
                                report.size,
                            );
                            if let Some(saved) = &mut *state() {
                                saved.snapshot.report = report.clone();
                                saved.snapshot.report.state = "running".into();
                                saved.snapshot.report.message.clear();
                            }
                        },
                    )?;
                    // Same selection/layout must still resolve. A changed layout cannot
                    // be presented as a successful verification of the new target.
                    if report.state == "completed" {
                        if let Err(e) = platform::resolve(&request.source, false, false) {
                            report.state = "failed".into();
                            report.message = format!(
                                "检测结束时设备或布局核对失败：{e}。结果仅对应已读取范围。"
                            );
                        }
                    }
                    Ok(report)
                },
            ))
            .unwrap_or_else(|_| Err("检测线程异常结束；源磁盘未写入。".into()));
            let mut saved = state();
            let pause_requested = saved.as_ref().unwrap().pause_requested;
            let report = &mut saved.as_mut().unwrap().snapshot.report;
            match result {
                Ok(final_report) => *report = final_report,
                Err(e) => {
                    report.state = if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        "canceled"
                    } else {
                        "failed"
                    }
                    .into();
                    report.message = e;
                }
            }
            if pause_requested && report.state == "canceled" {
                report.state = "paused".into();
                report.message = "检测已暂停，可以保存进度或从未完成位置继续。".into();
            }
            let outcome = if report.state == "completed" {
                Ok((report.message.clone(), None))
            } else {
                Err(report.message.clone())
            };
            data_tasks::finish(&work_id, outcome);
            drop(saved);
        });
        Ok(id)
    }
}
fn stopped(id: &str) -> Result<Saved, String> {
    if data_tasks::status().is_some_and(|j| j.state == "running") {
        return Err("请先暂停当前检测或等待任务结束。".into());
    }
    state()
        .as_ref()
        .filter(|s| s.snapshot.id == id && s.snapshot.report.state != "running")
        .cloned()
        .ok_or("检测进度不存在或仍在运行。".into())
}
pub fn pause(id: String) -> Result<(), String> {
    let mut state = state();
    let saved = state
        .as_mut()
        .filter(|s| s.snapshot.id == id && s.snapshot.report.state == "running")
        .ok_or("检测已结束。")?;
    data_tasks::cancel(id)?;
    saved.pause_requested = true;
    Ok(())
}
pub fn resume(id: String, read_timeout_seconds: Option<u16>) -> Result<String, String> {
    let saved = stopped(&id)?;
    launch(
        Request {
            source: saved.source,
            options: saved.snapshot.options,
            refine_failures: saved.snapshot.report.refinement.is_some(),
            read_timeout_seconds,
        },
        Some(saved.snapshot),
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
        let saved = stopped(&id)?;
        let bytes = crate::surface_checkpoint::encode(crate::surface_checkpoint::Progress {
            source: saved.source.clone(),
            options: saved.snapshot.options,
            snapshot: saved.snapshot,
        })?;
        save_file(
            &saved.source,
            &bytes,
            "保存检测进度到另一块磁盘",
            "dsscan",
            "DiskStudio-progress.dsscan",
        )
    }
}
pub fn load_progress(target: crate::partitions::Target) -> Result<Option<Snapshot>, String> {
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
        if data_tasks::status().is_some_and(|j| j.state == "running") {
            return Err("请先暂停当前检测。".into());
        }
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
        if data_tasks::status().is_some_and(|j| j.state == "running") {
            return Err("另一项任务已经开始，未加载进度。".into());
        }
        progress.snapshot.id = uuid::Uuid::new_v4().to_string();
        progress.snapshot.loaded = true;
        let snapshot = progress.snapshot.clone();
        *state() = Some(Saved {
            source: progress.source,
            snapshot: progress.snapshot,
            pause_requested: false,
        });
        Ok(Some(snapshot))
    }
}
pub fn export(id: String) -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("当前仅 Windows 支持保存检测报告。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前任务完成。")?;
        if data_tasks::status().is_some_and(|j| j.state == "running") {
            return Err("请先结束检测再保存报告。".into());
        }
        let (source, snapshot) = {
            let s = state();
            let saved = s
                .as_ref()
                .filter(|s| s.snapshot.id == id)
                .ok_or("检测结果已失效。")?;
            if saved.snapshot.report.state == "running" {
                return Err("检测仍在进行。".into());
            }
            (saved.source.clone(), saved.snapshot.clone())
        };
        let limitations = if snapshot.report.refinement.is_some() {
            "耗时分布和失败块统计基于初读，平均速度包含细查时间。细查在初读失败的块内读取更小的范围，记录单扇区读取失败及预算用尽时尚未定位的范围。结果反映该次读取，不能判定介质健康。未锁定在线卷，内容可能变化。"
        } else {
            "每次读取最多 1 MiB；读取失败标记整个请求块，不代表块内每个扇区损坏。耗时包含设备和虚拟化缓存影响，不是健康诊断。未读取范围没有结论。未锁定在线卷，内容可能变化。"
        };
        let report = serde_json::json!({
            "format": "DiskStudio surface report",
            "version": 1,
            "limitations": limitations,
            "result": snapshot,
        });
        let bytes = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
        save_file(
            &source,
            &bytes,
            "保存检测报告到另一块磁盘（使用新文件名）",
            "json",
            "DiskStudio-surface.json",
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
    let folder = platform::destination_folder(
        path.parent().ok_or("输出目录无效")?,
        &source.target,
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

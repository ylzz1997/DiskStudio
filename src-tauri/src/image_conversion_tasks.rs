//! Native save-dialog capabilities for image-file conversion. No device paths,
//! WMI, mounts or storage operations. Incoming data is bounded and sequential.
use crate::image_convert::{Options, Verified};
use serde::Serialize;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub id: String,
    pub options: Options,
    pub state: String,
    pub message: String,
    pub received: u64,
    pub source_verified: u64,
    pub verified: u64,
    pub output_name: String,
    pub partial_name: String,
    pub result: Option<Verified>,
    pub creation: Option<crate::image_creation::Summary>,
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::image_convert::{Encoder, Identity, CHUNK};
    use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
    use std::{
        fs::File,
        os::windows::io::AsRawHandle,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows_sys::Win32::Storage::FileSystem::*;
    static STATE: Mutex<State> = Mutex::new(State {
        active: None,
        report: None,
        opening: false,
    });
    struct State {
        active: Option<Arc<Job>>,
        report: Option<Report>,
        opening: bool,
    }
    struct Job {
        id: String,
        generated: bool,
        cancel: AtomicBool,
        session: Mutex<Session>,
    }
    struct Session {
        encoder: Option<Encoder<File>>,
        dir: Dir,
        name: String,
    }
    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }
    fn state() -> std::sync::MutexGuard<'static, State> {
        STATE.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn active(id: &str) -> Result<Arc<Job>, String> {
        state()
            .active
            .as_ref()
            .filter(|j| j.id == id)
            .cloned()
            .ok_or("转换任务已结束或标识失效。".into())
    }
    fn stopped(job: &Job) -> Result<(), String> {
        if job.cancel.load(Ordering::Relaxed) {
            Err("已取消；未完成的 partial 文件保留供检查或删除。".into())
        } else {
            Ok(())
        }
    }
    fn update(id: &str, f: impl FnOnce(&mut Report)) {
        if let Some(r) = state().report.as_mut().filter(|r| r.id == id) {
            f(r);
        }
    }
    fn terminal(job: &Job, message: String) {
        let mut s = state();
        if let Some(r) = s.report.as_mut().filter(|r| r.id == job.id) {
            r.state = if job.cancel.load(Ordering::Relaxed) {
                "canceled"
            } else {
                "failed"
            }
            .into();
            r.message = message;
        }
        if s.active.as_ref().is_some_and(|j| j.id == job.id) {
            s.active = None;
        }
    }
    fn plain_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && name.encode_utf16().count() <= 220
            && !name.ends_with(['.', ' '])
            && !name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
            && !crate::recovery_export::reserved(name)
    }
    pub fn begin(options: Options) -> Result<Option<Report>, String> {
        begin_with(options, None)
    }
    fn begin_with(
        options: Options,
        creation: Option<crate::image_creation::Summary>,
    ) -> Result<Option<Report>, String> {
        options.validate()?;
        {
            let mut s = state();
            if s.opening || s.active.is_some() {
                return Err("已有镜像创建或转换任务。".into());
            }
            s.opening = true;
        }
        let result = (|| {
            let extension = options.format.extension();
            let Some(path) = rfd::FileDialog::new()
                .set_title(crate::native_locale::text(if creation.is_some() {
                    "创建虚拟磁盘（新文件）"
                } else {
                    "保存转换后的镜像（新文件）"
                }))
                .add_filter(crate::native_locale::text("虚拟磁盘镜像"), &[extension])
                .set_file_name(format!(
                    "{}.{extension}",
                    if creation.is_some() {
                        "disk"
                    } else {
                        "converted"
                    }
                ))
                .save_file()
            else {
                return Ok(None);
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| plain_name(n))
                .ok_or("输出名称无效。")?
                .to_owned();
            if !name.to_lowercase().ends_with(&format!(".{extension}")) {
                return Err(format!("输出扩展名须为 .{extension}。"));
            }
            let parent = path.parent().ok_or("输出目录无效。")?;
            let dir = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
            let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
            if unsafe { GetFileInformationByHandle(dir.as_raw_handle(), info.as_mut_ptr()) } == 0 {
                return Err(err(std::io::Error::last_os_error()));
            }
            let info = unsafe { info.assume_init() };
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err("输出目录类型无效。".into());
            }
            match dir.symlink_metadata(&name) {
                Ok(_) => return Err("目标已存在；不会覆盖已有文件。".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(err(e)),
            }
            let id = uuid::Uuid::new_v4().to_string();
            let partial = format!("diskstudio-{id}.partial");
            let mut o = OpenOptions::new();
            o.read(true)
                .write(true)
                .create_new(true)
                .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ);
            let file = dir.open_with(&partial, &o).map_err(err)?.into_std();
            if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
                || !file.metadata().map_err(err)?.is_file()
            {
                return Err("输出必须为普通新文件。".into());
            }
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(err)?
                .as_secs()
                .checked_sub(946684800)
                .ok_or("系统时间无效")?;
            let identity = Identity {
                created: *uuid::Uuid::new_v4().as_bytes(),
                modified: *uuid::Uuid::new_v4().as_bytes(),
                vhd_timestamp: u32::try_from(seconds).map_err(err)?,
                file_name: name.clone(),
            };
            let encoder = Encoder::new(file, options.clone(), identity)?;
            let report = Report {
                id: id.clone(),
                options,
                state: "receiving".into(),
                message: if creation.is_some() {
                    "正在生成新镜像逻辑内容。"
                } else {
                    "正在接收源镜像逻辑内容。"
                }
                .into(),
                received: 0,
                source_verified: 0,
                verified: 0,
                output_name: name.clone(),
                partial_name: partial,
                result: None,
                creation: creation.clone(),
            };
            let job = Arc::new(Job {
                id,
                generated: creation.is_some(),
                cancel: AtomicBool::new(false),
                session: Mutex::new(Session {
                    encoder: Some(encoder),
                    dir,
                    name,
                }),
            });
            let mut s = state();
            s.report = Some(report.clone());
            s.active = Some(job);
            Ok(Some(report))
        })();
        state().opening = false;
        result
    }
    pub fn push(id: String, offset: u64, bytes: Vec<u8>) -> Result<u64, String> {
        let job = active(&id)?;
        if job.generated {
            return Err("新建镜像不接受外部数据块。".into());
        }
        let mut session = job
            .session
            .try_lock()
            .map_err(|_| "转换请求重叠，请等待上一块完成。")?;
        let result: Result<u64, String> = (|| {
            stopped(&job)?;
            if bytes.is_empty() || bytes.len() > CHUNK {
                return Err("转换块大小超限。".into());
            }
            let e = session.encoder.as_mut().ok_or("转换不再接收数据。")?;
            e.push(offset, &bytes)?;
            Ok(e.received())
        })();
        match result {
            Ok(n) => {
                update(&id, |r| r.received = n);
                Ok(n)
            }
            Err(e) => {
                session.encoder.take();
                terminal(&job, e.clone());
                Err(e)
            }
        }
    }
    pub fn finish(id: String) -> Result<Report, String> {
        finish_as(id, false)
    }
    fn finish_as(id: String, generated: bool) -> Result<Report, String> {
        let job = active(&id)?;
        if job.generated != generated {
            return Err("镜像任务类型不符。".into());
        }
        let mut session = job.session.try_lock().map_err(|_| "仍有转换请求运行。")?;
        let result: Result<Report, String> = (|| {
            stopped(&job)?;
            let encoder = session.encoder.take().ok_or("转换不再接收数据。")?;
            update(&id, |r| {
                r.state = "verifying".into();
                r.message = "正在完整回读输出并核对 SHA-256。".into();
            });
            let (file, verified) = encoder.finish(
                |f| f.sync_all().map_err(err),
                |done| {
                    stopped(&job)?;
                    update(&id, |r| r.verified = done);
                    Ok(())
                },
            )?;
            // Serialize cancellation against the final no-replace rename. A
            // completed rename is never reported as canceled afterwards.
            let mut s = state();
            stopped(&job)?;
            crate::recovery_output::finish(&file, &session.dir, &session.name)?;
            let r = s
                .report
                .as_mut()
                .filter(|r| r.id == id)
                .ok_or("转换结果状态丢失。")?;
            r.state = "completed".into();
            r.message = if generated {
                "创建完成，完整逻辑内容与容器元数据已回读校验。"
            } else {
                "转换完成，完整逻辑内容与块映射已回读校验；源镜像未写入。"
            }
            .into();
            r.result = Some(verified);
            let report = r.clone();
            s.active = None;
            Ok(report)
        })();
        if let Err(e) = &result {
            session.encoder.take();
            terminal(&job, e.clone());
        }
        result
    }
    pub fn verify_source(id: String, offset: u64, bytes: Vec<u8>) -> Result<u64, String> {
        let job = active(&id)?;
        if job.generated {
            return Err("新建镜像不接受外部复核数据。".into());
        }
        let mut session = job.session.try_lock().map_err(|_| "仍有转换请求运行。")?;
        let result: Result<u64, String> = (|| {
            stopped(&job)?;
            session
                .encoder
                .as_mut()
                .ok_or("转换不再接收数据。")?
                .verify_source(offset, &bytes)
        })();
        match result {
            Ok(n) => {
                update(&id, |r| {
                    r.state = "checkingSource".into();
                    r.source_verified = n;
                    r.message = "正在重新读取源镜像并核对完整内容。".into();
                });
                Ok(n)
            }
            Err(e) => {
                session.encoder.take();
                terminal(&job, e.clone());
                Err(e)
            }
        }
    }
    pub fn cancel(id: String) -> Result<(), String> {
        let job = {
            let s = state();
            let job = s
                .active
                .as_ref()
                .filter(|j| j.id == id)
                .cloned()
                .ok_or("转换任务已结束。")?;
            job.cancel.store(true, Ordering::Relaxed);
            job
        };
        if let Ok(mut session) = job.session.try_lock() {
            session.encoder.take();
            terminal(&job, "已取消；partial 文件保留，未发布成功镜像。".into());
        }
        Ok(())
    }
    pub fn status() -> Option<Report> {
        state().report.clone()
    }
    pub fn create(options: crate::image_creation::Options) -> Result<Option<Report>, String> {
        let generator =
            crate::image_creation::Generator::new(options.clone(), uuid::Uuid::new_v4())?;
        let Some(report) = begin_with(options.output, Some(generator.summary()))? else {
            return Ok(None);
        };
        let job = active(&report.id)?;
        let running = Arc::clone(&job);
        let spawn = std::thread::Builder::new()
            .name("image-creation".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    {
                        let mut session =
                            running.session.lock().map_err(|_| "镜像生成状态不可用。")?;
                        stopped(&running)?;
                        let encoder = session.encoder.as_mut().ok_or("镜像创建已结束。")?;
                        generator.feed(encoder, |stage, done| {
                            stopped(&running)?;
                            update(&running.id, |r| match stage {
                                crate::image_creation::Stage::Generating => r.received = done,
                                crate::image_creation::Stage::Checking => {
                                    r.state = "checkingSource".into();
                                    r.message = "正在重新生成并复核新镜像内容。".into();
                                    r.source_verified = done;
                                }
                            });
                            Ok(())
                        })?;
                    }
                    finish_as(running.id.clone(), true).map(|_| ())
                }))
                .unwrap_or_else(|_| Err("镜像创建异常停止；未完成文件保留。".into()));
                if let Err(e) = result {
                    running
                        .session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .encoder
                        .take();
                    terminal(&running, e);
                }
            });
        if let Err(e) = spawn {
            job.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .encoder
                .take();
            terminal(&job, err(&e));
            return Err(err(e));
        }
        Ok(Some(report))
    }
}
#[cfg(windows)]
pub use platform::*;
#[cfg(not(windows))]
pub fn begin(_: Options) -> Result<Option<Report>, String> {
    Err("镜像转换当前在 Windows 桌面应用中提供。".into())
}
#[cfg(not(windows))]
pub fn push(_: String, _: u64, _: Vec<u8>) -> Result<u64, String> {
    Err("需要 Windows 桌面应用。".into())
}
#[cfg(not(windows))]
pub fn verify_source(_: String, _: u64, _: Vec<u8>) -> Result<u64, String> {
    Err("需要 Windows 桌面应用。".into())
}
#[cfg(not(windows))]
pub fn finish(_: String) -> Result<Report, String> {
    Err("需要 Windows 桌面应用。".into())
}
#[cfg(not(windows))]
pub fn cancel(_: String) -> Result<(), String> {
    Err("需要 Windows 桌面应用。".into())
}
#[cfg(not(windows))]
pub fn status() -> Option<Report> {
    None
}
#[cfg(not(windows))]
pub fn create(_: crate::image_creation::Options) -> Result<Option<Report>, String> {
    Err("镜像创建当前在 Windows 桌面应用中提供。".into())
}

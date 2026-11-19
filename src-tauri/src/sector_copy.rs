//! Explicit, verified copies of a bounded sector range to a new ordinary file.
use crate::{
    data_tasks::Selection,
    surface_scan::{Direction, Options},
};
use serde::{Deserialize, Serialize};

#[path = "sector_copy_range.rs"]
mod range_copy;
use range_copy::{Errors, Failures};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub options: Options,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    errors: Errors,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    version: u32,
    start_lba: u64,
    sector_size: u64,
    sector_count: u64,
    direction: Direction,
    errors: Errors,
    output_name: String,
    sha256: String,
    elapsed_ms: u64,
    failures: Failures,
}

pub fn report(id: String) -> Option<Report> {
    #[cfg(windows)]
    {
        platform::report(&id)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        None
    }
}

pub fn save_report(id: String) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        platform::save_report(&id)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("扇区导出当前仅支持 Windows。".into())
    }
}

#[cfg(any(windows, test))]
pub(crate) fn copy_verified<R, W>(
    source: &mut R,
    output: &mut W,
    size: u64,
    sync: impl FnOnce(&W) -> std::io::Result<()>,
    mut progress: impl FnMut(&str, u64) -> Result<(), String>,
) -> Result<String, String>
where
    R: std::io::Read + Send,
    W: std::io::Read + std::io::Write + std::io::Seek + Send,
{
    use crate::data_io;
    use std::io::SeekFrom;

    let digest = data_io::copy_exact(source, output, size, |done| progress("复制扇区", done))?;
    sync(output).map_err(|e| e.to_string())?;
    if output.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != size {
        return Err("输出文件长度与确认范围不一致。".into());
    }
    output.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let read_back = data_io::hash_exact(output, size, |done| progress("回读输出文件", done))?;
    if read_back != digest {
        return Err("导出文件回读 SHA-256 不一致。".into());
    }
    Ok(digest)
}

pub fn start(request: Request) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        platform::start(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("扇区导出当前仅支持 Windows。".into())
    }
}

#[cfg(windows)]
pub(crate) mod platform {
    use super::*;
    use crate::{
        data_io::Region,
        data_tasks,
        data_windows::{self, Resolved},
        native_locale,
        surface_scan::Range,
    };
    use std::{
        fs::File,
        io::Cursor,
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
        time::Instant,
    };

    #[derive(Clone)]
    struct ResultRecord {
        id: String,
        report: Report,
        source: crate::partitions::Target,
    }
    static RESULT: Mutex<Option<ResultRecord>> = Mutex::new(None);

    pub(super) fn report(id: &str) -> Option<Report> {
        RESULT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|entry| entry.id == id)
            .map(|entry| entry.report.clone())
    }

    pub(super) fn save_report(id: &str) -> Result<Option<String>, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let result = RESULT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|entry| entry.id == id)
            .cloned()
            .ok_or("扇区复制报告已被替换。")?;
        let Some(path) = rfd::FileDialog::new()
            .set_title(native_locale::text("保存扇区复制报告"))
            .add_filter("JSON", &["json"])
            .set_file_name(format!(
                "DiskStudio-LBA-{}-{}-report.json",
                result.report.start_lba, result.report.sector_count
            ))
            .save_file()
        else {
            return Ok(None);
        };
        let bytes = serde_json::to_vec_pretty(&result.report).map_err(|e| e.to_string())?;
        let size = bytes.len() as u64;
        let (mut file, publication) =
            crate::backup_files::create_sector_report(&path, &result.source, size)?;
        copy_verified(
            &mut Cursor::new(bytes),
            &mut file,
            size,
            File::sync_all,
            |_, _| Ok(()),
        )?;
        publication.publish()?;
        Ok(Some(path.to_string_lossy().into_owned()))
    }

    fn range(request: &Request, source: &Resolved) -> Result<Range, String> {
        if source.disk.is_read_only.is_none() {
            return Err("设备状态不完整或为复杂存储，已拒绝访问。".into());
        }
        request.options.range(
            source.offset,
            source.size,
            source.disk.logical_sector_size.ok_or("扇区大小缺失")?,
        )
    }

    pub(super) fn start(request: Request) -> Result<Option<String>, String> {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let before = data_windows::resolve(&request.source, true, false)?;
        let planned = range(&request, &before)?;
        request.errors.prepare(planned.sector)?;
        let Some(path) = rfd::FileDialog::new()
            .set_title(native_locale::text("导出扇区范围到另一块磁盘"))
            .add_filter(native_locale::text("扇区数据"), &["bin"])
            .set_file_name(format!(
                "DiskStudio-LBA-{}-{}.bin",
                planned.offset / planned.sector,
                planned.size / planned.sector
            ))
            .save_file()
        else {
            return Ok(None);
        };
        let (id, cancel) = data_tasks::start("复制扇区")?;
        data_tasks::record_progress(&id, "准备", 0, planned.size);
        drop(guard);
        let work_id = id.clone();
        if let Err(error) = std::thread::Builder::new().spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = crate::OPERATION
                    .try_lock()
                    .map_err(|_| "另一项磁盘操作正在运行。")?;
                run(&work_id, &cancel, request, &before, &path)
            }))
            .unwrap_or_else(|_| Err("扇区任务异常结束；没有写入源设备。".into()));
            if let Err(error) = result {
                data_tasks::finish(&work_id, Err(error));
            }
        }) {
            let message = error.to_string();
            data_tasks::finish(&id, Err(message.clone()));
            return Err(message);
        }
        Ok(Some(id))
    }

    pub(crate) fn run(
        id: &str,
        cancel: &AtomicBool,
        request: Request,
        before: &Resolved,
        path: &Path,
    ) -> Result<(), String> {
        let started = Instant::now();
        let fresh = data_windows::resolve(&request.source, true, false)?;
        if !before.same_data_binding(&fresh) {
            return Err("磁盘布局已变化，请刷新并重新选择。".into());
        }
        let bounds = range(&request, &fresh)?;
        data_windows::destination_folder(
            path.parent().ok_or("输出目录无效")?,
            &request.source.target,
            bounds.size,
        )?;
        let progress = |phase: &str, done: u64| {
            if cancel.load(Ordering::Relaxed) {
                return Err("任务已取消。".into());
            }
            data_tasks::record_progress(id, phase, done, bounds.size);
            Ok::<_, String>(())
        };
        progress("准备", 0)?;
        let locks = data_windows::state_locks(&fresh)?;
        let locked = data_windows::resolve_locked(&request.source, false, &fresh, &locks)?;
        let reader = data_windows::device(&locked, false)?;
        let policy = request.errors.prepare(bounds.sector)?;
        progress("复制扇区", 0)?;
        let (mut file, publication) =
            crate::backup_files::create_sector_output(path, &request.source.target, bounds.size)?;
        let filename = path.file_name().ok_or("输出文件名无效")?.to_string_lossy();
        (|| {
            let (digest, failures) = if request.direction == Direction::Forward
                && matches!(policy, range_copy::Policy::Stop)
            {
                let mut region =
                    Region::new(reader, bounds.offset, bounds.size).map_err(|e| e.to_string())?;
                (
                    copy_verified(
                        &mut region,
                        &mut file,
                        bounds.size,
                        File::sync_all,
                        progress,
                    )?,
                    Failures::default(),
                )
            } else {
                range_copy::copy(
                    reader,
                    &mut file,
                    bounds,
                    request.direction,
                    policy,
                    File::sync_all,
                    progress,
                )?
            };
            data_windows::resolve_locked(&request.source, false, &locked, &locks)?;
            drop(locks);
            let key = if failures.filled_sectors == 0 {
                "已导出并校验 SHA-256：{path}"
            } else {
                "已导出含填充的文件并校验 SHA-256：{path}"
            };
            let message = native_locale::current().format(key, &[("path", &filename)]);
            let result = ResultRecord {
                id: id.into(),
                source: request.source.target,
                report: Report {
                    version: 1,
                    start_lba: bounds.offset / bounds.sector,
                    sector_size: bounds.sector,
                    sector_count: bounds.size / bounds.sector,
                    direction: request.direction,
                    errors: request.errors,
                    output_name: filename.to_string(),
                    sha256: digest.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    failures,
                },
            };
            data_tasks::publish_verified_file(id, message, digest, || {
                publication.publish()?;
                *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                Ok(())
            })
        })()
        .map_err(|error: String| {
            native_locale::current().format(
                "{error} 未完成文件保留为 {name}。",
                &[("error", &error), ("name", &format!("{filename}.partial"))],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn exact_range_is_copied_and_read_back_without_adjacent_bytes() {
        let mut source = Cursor::new([vec![1; 4096], vec![2; 4096], vec![3; 4096]].concat());
        source.set_position(4096);
        let mut output = Cursor::new(Vec::new());
        let mut phases = Vec::new();
        let digest = copy_verified(
            &mut source,
            &mut output,
            4096,
            |_| Ok(()),
            |phase, done| {
                phases.push((phase.to_owned(), done));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(source.position(), 8192);
        assert_eq!(output.into_inner(), vec![2; 4096]);
        assert_eq!(digest.len(), 64);
        assert_eq!(phases.last(), Some(&("回读输出文件".into(), 4096)));
    }

    #[test]
    fn rejects_short_source_extra_destination_bytes_and_readback_cancellation() {
        let mut source = Cursor::new(vec![7; 512]);
        let mut output = Cursor::new(vec![0; 513]);
        assert!(copy_verified(&mut source, &mut output, 512, |_| Ok(()), |_, _| Ok(())).is_err());
        source.set_position(0);
        let mut output = Cursor::new(Vec::new());
        assert!(copy_verified(&mut source, &mut output, 1024, |_| Ok(()), |_, _| Ok(())).is_err());
        source.set_position(0);
        let result = copy_verified(
            &mut source,
            &mut output,
            512,
            |_| Ok(()),
            |phase, _| {
                if phase == "回读输出文件" {
                    Err("canceled".into())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result.unwrap_err(), "canceled");
    }
}

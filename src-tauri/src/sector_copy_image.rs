//! Copy a verified logical range from a retained read-only image session.
use super::{platform::*, range_copy, Direction, Errors, ImageRequest, Options, Report};
use crate::{
    backup_archive::Source,
    backup_files, data_io,
    data_tasks::{self, image_browse_tasks, Selection},
    data_windows, native_locale,
    partitions::{
        platform::{maintenance, TableReadContext},
        Target,
    },
    surface_scan::Range,
    table_archive::Archive,
    used_space::View,
};
use std::{
    fs::File,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

struct ImageSource {
    opened: image_browse_tasks::Opened,
    range: Range,
    direction: Direction,
    label: String,
}

impl ImageSource {
    fn open(request: ImageRequest) -> Result<Self, String> {
        let opened = image_browse_tasks::opened(&request.id)?;
        opened.validate()?;
        let browser = &opened.browser;
        let (base, size, label) = if let Some(partition) = request.partition {
            let part = browser.part(partition)?;
            (part.start, part.size, browser.label(partition)?)
        } else {
            (0, browser.info.size, browser.info.name.clone())
        };
        let range = request
            .options
            .range(base, size, browser.info.options.sector)?;
        Ok(Self {
            opened,
            range,
            direction: request.direction,
            label,
        })
    }

    fn validate(&self) -> Result<(), String> {
        image_browse_tasks::opened(&self.opened.browser.info.id)?;
        self.opened.validate()
    }

    fn reader(&self) -> Result<Box<dyn Source>, String> {
        self.opened.logical_reader()
    }

    fn result(
        self,
        id: &str,
        output_name: String,
        destination: Option<(Target, Range)>,
        digest: String,
        started: Instant,
    ) -> ResultRecord {
        let (target, destination_start_lba) = match destination {
            Some((target, range)) => (Some(target), Some(range.offset / range.sector)),
            None => (None, None),
        };
        ResultRecord {
            id: id.into(),
            sources: self.opened.source_disks,
            destination: target,
            report: Report {
                version: 3,
                start_lba: self.range.offset / self.range.sector,
                sector_size: self.range.sector,
                sector_count: self.range.size / self.range.sector,
                direction: self.direction,
                errors: Errors::Stop,
                output_name,
                destination_start_lba,
                sha256: digest,
                elapsed_ms: started.elapsed().as_millis() as u64,
                failures: range_copy::Failures::default(),
            },
        }
    }
}

fn progress<'a>(
    id: &'a str,
    cancel: &'a AtomicBool,
    size: u64,
) -> impl Fn(&str, u64) -> Result<(), String> + 'a {
    move |phase, done| {
        if cancel.load(Ordering::Relaxed) {
            return Err("任务已取消。".into());
        }
        data_tasks::record_progress(id, phase, done, size);
        Ok(())
    }
}

pub(super) fn start_file(request: ImageRequest) -> Result<Option<String>, String> {
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    data_tasks::ensure_idle()?;
    let source = ImageSource::open(request)?;
    let Some(path) = save_file_dialog(source.range) else {
        return Ok(None);
    };
    launch_file_job(guard, 0, source.range.size, move |id, cancel| {
        copy_file(id, cancel, source, &path)
    })
}

fn copy_file(
    id: &str,
    cancel: &AtomicBool,
    source: ImageSource,
    path: &Path,
) -> Result<(), String> {
    let started = Instant::now();
    let progress = progress(id, cancel, source.range.size);
    progress("准备", 0)?;
    source.validate()?;
    let reader = source.reader()?;
    let filename = path.file_name().ok_or("输出文件名无效")?.to_string_lossy();
    let (mut file, publication) = backup_files::create_sector_output_excluding(
        path,
        &source.opened.source_disks,
        source.range.size,
    )?;
    let locale = native_locale::current();
    (|| {
        let (digest, _) = range_copy::copy_resumable(
            reader,
            &mut file,
            source.range,
            source.direction,
            range_copy::Policy::Stop,
            range_copy::Output::File,
            |file| file.sync_all(),
            None,
            progress,
        )?;
        source.validate()?;
        let message = locale.format("已导出并校验 SHA-256：{path}", &[("path", &filename)]);
        let result = source.result(id, filename.to_string(), None, digest.clone(), started);
        data_tasks::complete_verified(id, message, digest, || {
            publication.publish()?;
            *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            Ok(())
        })
    })()
    .map_err(|error: String| {
        locale.format(
            "{error} 未完成文件保留为 {name}。",
            &[
                ("error", locale.text(&error)),
                ("name", &format!("{filename}.partial")),
            ],
        )
    })
}

pub(crate) struct DeviceCopy {
    source: ImageSource,
    destination: Selection,
    range: Range,
    label: String,
    fingerprint: String,
    table: Option<Archive>,
}

pub(crate) fn prepare_device(
    request: ImageRequest,
    mut destination: Selection,
    start_sector: u64,
) -> Result<(DeviceCopy, String, String, u64), String> {
    let source = ImageSource::open(request)?;
    if source.opened.source_disks.contains(&destination.target) {
        return Err("源和目标必须是不同的物理磁盘。".into());
    }
    let partition = destination.partition.take();
    let target = maintenance::write_context(&destination, None)?;
    let (base, size, label) = destination_extent(&target, partition, source.range.sector)?;
    let range = Options {
        start_sector,
        sector_count: source.range.size / source.range.sector,
    }
    .range(base, size, source.range.sector)?;
    maintenance::protect_partitions(&target, range)?;
    let mut file = data_windows::device(&target.resolved, false)?;
    let table = target_table(&target, &mut file, &destination.target, range)?;
    let source_label = range_label(&source.label, source.range);
    let target_label = range_label(&label, range);
    let bytes = source.range.size;
    Ok((
        DeviceCopy {
            source,
            destination,
            range,
            label,
            fingerprint: target.fingerprint,
            table,
        },
        source_label,
        target_label,
        bytes,
    ))
}

impl DeviceCopy {
    fn verify_table(&self, target: &TableReadContext, file: &mut File) -> Result<(), String> {
        if target_table(target, file, &self.destination.target, self.range)? != self.table {
            return Err("GPT 与预览不同".into());
        }
        Ok(())
    }

    pub(crate) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let started = Instant::now();
        let locale = native_locale::current();
        let progress = progress(id, cancel, self.source.range.size);
        progress("准备", 0)?;
        self.source.validate()?;
        let source_range = self.source.range;
        // Check the entire image extent before locking or offlining the target.
        let mut input = View::bounded(
            self.source.reader()?,
            source_range.offset,
            source_range.size,
            self.source.opened.browser.info.size,
        )
        .map_err(|e| e.to_string())?;
        let expected = data_io::hash_exact(&mut input, source_range.size, |done| {
            progress("校验镜像源", done)
        })?;
        drop(input);
        let destination = maintenance::write_context(&self.destination, None)?;
        if destination.fingerprint != self.fingerprint {
            return Err("身份、布局或保护与预览不同".into());
        }
        maintenance::protect_partitions(&destination, self.range)?;
        progress("锁定目标", 0)?;
        let locks = data_windows::lock(&destination.resolved)?;
        let target = maintenance::write_context(&self.destination, Some((&destination, &locks)))?;
        self.source.validate()?;
        let reader = self.source.reader()?;
        let mut file = data_windows::exclusive_device(&target.resolved)?;
        self.verify_table(&target, &mut file)?;
        progress("锁定目标", 0)?;
        data_windows::offline(&file)
            .map_err(|error| format!("磁盘可能已脱机，尚未发送数据写入：{error}"))?;
        let partial_error = |error: String| {
            locale.format(
                "目标可能已部分覆盖，请核对脱机状态和写入范围。{error}",
                &[("error", locale.text(&error))],
            )
        };
        let digest = (|| {
            self.verify_table(&target, &mut file)?;
            data_windows::confirm_writable_offline(&file)?;
            let mut output = View::bounded(
                &mut file,
                self.range.offset,
                self.range.size,
                target.resolved.size,
            )
            .map_err(|e| e.to_string())?;
            let (digest, _) = range_copy::copy_resumable(
                reader,
                &mut output,
                source_range,
                self.source.direction,
                range_copy::Policy::Stop,
                range_copy::Output::Device,
                |output| output.inner_mut().sync_all(),
                None,
                progress,
            )?;
            if digest != expected {
                return Err("镜像内容与写入前校验不一致。".into());
            }
            self.verify_table(&target, &mut file)?;
            data_windows::confirm_writable_offline(&file)?;
            self.source.validate()?;
            Ok(digest)
        })()
        .map_err(partial_error)?;
        let message = locale
            .text("扇区复制完成，已回读校验目标范围。目标磁盘保持脱机。")
            .to_owned();
        let result = self.source.result(
            id,
            self.label,
            Some((self.destination.target, self.range)),
            digest.clone(),
            started,
        );
        data_tasks::complete_verified(id, message.clone(), digest.clone(), || {
            *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            Ok(())
        })
        .map_err(partial_error)?;
        Ok((message, Some(digest)))
    }
}

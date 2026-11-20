//! Explicit, verified copies of a bounded sector range.
use crate::{
    data_tasks::Selection,
    surface_scan::{Direction, Options},
};
use serde::{Deserialize, Serialize};
use std::io::Read;

#[path = "sector_copy_range.rs"]
mod range_copy;
use range_copy::{Errors, Failures};

#[cfg(windows)]
#[path = "sector_copy_checkpoint.rs"]
mod checkpoint;

#[cfg(windows)]
#[path = "sector_copy_image.rs"]
pub(crate) mod image_platform;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub options: Options,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    errors: Errors,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageRequest {
    id: String,
    partition: Option<u32>,
    options: Options,
    #[serde(default)]
    direction: Direction,
}

pub fn start_image(request: ImageRequest) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        image_platform::start_file(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("扇区导出当前仅支持 Windows。".into())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    version: u32,
    start_lba: u64,
    sector_size: u64,
    sector_count: u64,
    direction: Direction,
    errors: Errors,
    output_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_start_lba: Option<u64>,
    sha256: String,
    elapsed_ms: u64,
    failures: Failures,
}

fn read_legacy_report(mut input: impl Read) -> Result<Report, String> {
    const LIMIT: u64 = 512 * 1024;
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("扇区复制报告超过大小限制。".into());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let report: Report =
        serde_json::from_slice(bytes).map_err(|_| "扇区复制 JSON 报告格式无效。")?;
    if !matches!(
        (report.version, report.destination_start_lba),
        (1, None) | (2, Some(_))
    ) || report.sha256.len() != 64
        || !report
            .sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("不支持此扇区复制报告的版本、目标或摘要。".into());
    }
    Ok(report)
}

fn legacy_source_request(
    source: Selection,
    report: &Report,
    offset: u64,
    capacity: u64,
    sector: u64,
) -> Result<Request, String> {
    if report.sector_size != sector || !matches!(sector, 512 | 4096) || offset % sector != 0 {
        return Err("报告与所选来源的逻辑扇区大小不一致。".into());
    }
    let options = Options {
        start_sector: report
            .start_lba
            .checked_sub(offset / sector)
            .ok_or("报告中的源扇区超出所选范围。")?,
        sector_count: report.sector_count,
    };
    let range = options.range(offset, capacity, sector)?;
    range_copy::filled_spans(range, report)?;
    if report.failures.filled_sectors == report.sector_count {
        return Err("旧报告没有可核对来源的未填充扇区。".into());
    }
    Ok(Request {
        source,
        options,
        direction: report.direction,
        errors: report.errors.for_filled_reread()?,
    })
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumedFile {
    id: String,
    options: Options,
    direction: Direction,
    errors: Errors,
}

pub fn resume_file(source: Selection, reread_filled: bool) -> Result<Option<ResumedFile>, String> {
    #[cfg(windows)]
    {
        platform::resume_file(source, reread_filled)
    }
    #[cfg(not(windows))]
    {
        let _ = (source, reread_filled);
        Err("扇区导出当前仅支持 Windows。".into())
    }
}

pub fn reread_file_report(source: Selection) -> Result<Option<ResumedFile>, String> {
    #[cfg(windows)]
    {
        platform::reread_file_report(source)
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        Err("扇区导出当前仅支持 Windows。".into())
    }
}

#[cfg(windows)]
pub(crate) mod platform {
    use super::*;
    use crate::{
        backup_files, data_tasks,
        data_windows::{self, Resolved},
        native_locale,
        partitions::{
            platform::{maintenance, TableReadContext},
            Target,
        },
        surface_scan::Range,
        table_archive::Archive,
        used_space::View,
    };
    use sha2::{Digest, Sha256};
    use std::{
        fs::File,
        io::{Cursor, Seek},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
        time::Instant,
    };

    #[derive(Clone)]
    pub(super) struct ResultRecord {
        pub(super) id: String,
        pub(super) report: Report,
        pub(super) sources: Vec<Target>,
        pub(super) destination: Option<Target>,
    }
    pub(super) static RESULT: Mutex<Option<ResultRecord>> = Mutex::new(None);

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
        let (mut file, publication) = crate::backup_files::create_sector_report(
            &path,
            &result.sources,
            result.destination.as_ref(),
            size,
        )?;
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

    pub(crate) struct DeviceCopy {
        request: Request,
        before_source: Resolved,
        source_range: Range,
        destination: Selection,
        destination_scope: Selection,
        start_sector: u64,
        destination_fingerprint: String,
        destination_range: Range,
        destination_label: String,
        table: Option<Archive>,
        checkpoint: DeviceCheckpoint,
        legacy: Option<Report>,
    }

    enum DeviceCheckpoint {
        None,
        New(checkpoint::Location),
        Resume(checkpoint::Opened),
    }

    struct ActiveCheckpoint {
        journal: checkpoint::Journal,
        plan: checkpoint::Plan,
        state: Option<range_copy::ResumeState>,
    }

    impl ActiveCheckpoint {
        fn reread(
            &mut self,
            reader: &mut (impl Read + Seek),
            output: &mut (impl Read + Seek),
            previous: &Report,
            mut progress: impl FnMut(&str, u64) -> Result<(), String>,
        ) -> Result<(), String> {
            let mut state = range_copy::start_filled_reread(
                output,
                self.plan.range,
                previous,
                self.state.as_ref(),
                &mut progress,
            )?;
            if self.state.is_none() {
                state.corroborate_source(
                    reader,
                    output,
                    self.plan.range,
                    previous.direction,
                    progress,
                )?;
            }
            self.journal.commit(&self.plan, &state)?;
            self.state = Some(state);
            Ok(())
        }
    }

    type PreparedDevice = (DeviceCopy, String, String, u64);

    pub(super) fn range_label(label: &str, range: Range) -> String {
        let first = range.offset / range.sector;
        let last = first + range.size / range.sector - 1;
        let bounds = native_locale::current().format(
            "绝对 LBA {first}–{last} · {size} B",
            &[
                ("first", &first.to_string()),
                ("last", &last.to_string()),
                ("size", &range.size.to_string()),
            ],
        );
        format!("{label} · {bounds}")
    }

    pub(super) fn target_table(
        ctx: &TableReadContext,
        file: &mut File,
        identity: &Target,
        range: Range,
    ) -> Result<Option<Archive>, String> {
        let table = maintenance::capture_write_table(ctx, file, identity, range, true)?;
        if let Some(archive) = &table {
            let layout = crate::table_restore::archive_layout(archive)?;
            let first = range.offset / range.sector;
            let last = first + range.size / range.sector - 1;
            if first < layout.first || last > layout.last {
                return Err("目标范围超出 GPT 可用扇区。".into());
            }
        }
        Ok(table)
    }

    pub(super) fn destination_extent(
        target: &TableReadContext,
        partition: Option<u32>,
        sector: u64,
    ) -> Result<(u64, u64, String), String> {
        if target.resolved.disk.logical_sector_size != Some(sector) {
            return Err("源和目标的逻辑扇区大小必须一致。".into());
        }
        let mut label = target.resolved.label.clone();
        let Some(number) = partition else {
            return Ok((0, target.resolved.size, label));
        };
        let part = target
            .parts
            .iter()
            .find(|part| part.partition_number == number)
            .ok_or("目标分区不存在")?;
        let name = native_locale::current()
            .format("分区 {partition}", &[("partition", &number.to_string())]);
        label.push_str(&format!(" / {name}"));
        Ok((part.offset, part.size, label))
    }

    pub(crate) fn prepare_device(
        request: Request,
        destination: Selection,
        start_sector: u64,
        keep_checkpoint: bool,
    ) -> Result<Option<PreparedDevice>, String> {
        let before = data_windows::resolve(&request.source, true, false)?;
        let prepared = device_plan(
            request,
            before,
            destination,
            start_sector,
            DeviceCheckpoint::None,
            None,
        )?;
        choose_device_checkpoint(prepared, keep_checkpoint)
    }

    pub(crate) fn prepare_report_device(
        source: Selection,
        destination: Selection,
        keep_checkpoint: bool,
    ) -> Result<Option<PreparedDevice>, String> {
        let Some(report) = pick_legacy_report(&source.target)? else {
            return Ok(None);
        };
        if report.version != 2 {
            return Err("请选择写入磁盘的扇区复制报告。".into());
        }
        let before = data_windows::resolve(&source, true, false)?;
        let request = legacy_request(source, &report, &before)?;
        let prepared = device_plan(
            request,
            before,
            destination,
            0,
            DeviceCheckpoint::None,
            Some(report),
        )?;
        choose_device_checkpoint(prepared, keep_checkpoint)
    }

    fn choose_device_checkpoint(
        mut prepared: PreparedDevice,
        keep_checkpoint: bool,
    ) -> Result<Option<PreparedDevice>, String> {
        if keep_checkpoint {
            let work = &mut prepared.0;
            let Some(path) = rfd::FileDialog::new()
                .set_title(native_locale::text("在源盘和目标盘以外保存扇区复制断点"))
                .add_filter("DiskStudio", &["dscopy"])
                .set_file_name("DiskStudio-sector-copy.dscopy")
                .save_file()
            else {
                return Ok(None);
            };
            let location = checkpoint::Location::bind(
                &path,
                &work.request.source.target,
                Some(&work.destination.target),
            )?;
            location.ensure_unused()?;
            work.checkpoint = DeviceCheckpoint::New(location);
        }
        Ok(Some(prepared))
    }

    fn select_checkpoint(source: &Selection) -> Result<Option<checkpoint::Opened>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(native_locale::text("打开扇区复制断点"))
            .add_filter("DiskStudio", &["dscopy"])
            .pick_file()
        else {
            return Ok(None);
        };
        let opened = checkpoint::Journal::open(&path, &source.target)?;
        if opened.plan.request.source.layout_key != source.layout_key
            || opened.plan.request.source.partition != source.partition
        {
            return Err("请选择断点记录中的源盘及分区范围。".into());
        }
        Ok(Some(opened))
    }

    pub(crate) fn prepare_device_resume(
        source: Selection,
        reread_filled: bool,
    ) -> Result<Option<PreparedDevice>, String> {
        let Some(opened) = select_checkpoint(&source)? else {
            return Ok(None);
        };
        let device = opened
            .plan
            .device
            .as_ref()
            .ok_or("请从文件导出模式打开此断点。")?;
        let mut request = opened.plan.request.clone();
        if reread_filled {
            let report = opened
                .verified
                .as_ref()
                .ok_or("请选择已完成的扇区复制断点。")?;
            if report.version != 2
                || report.destination_start_lba.is_none()
                || report.destination_start_lba
                    != device.range.offset.checked_div(device.range.sector)
            {
                return Err("复制记录中的目标范围不一致。".into());
            }
            range_copy::filled_spans(opened.plan.range, report)?;
            request.errors = report.errors.for_filled_reread()?;
        } else if opened.verified.is_some() {
            return Err("此磁盘复制断点已完成。".into());
        }
        let before = data_windows::resolve(&request.source, true, false)?;
        device_plan(
            request,
            before,
            device.selection.clone(),
            device.start_sector,
            DeviceCheckpoint::Resume(opened),
            None,
        )
        .map(Some)
    }

    fn device_plan(
        request: Request,
        source: Resolved,
        destination_scope: Selection,
        start_sector: u64,
        checkpoint: DeviceCheckpoint,
        legacy: Option<Report>,
    ) -> Result<PreparedDevice, String> {
        let mut destination = destination_scope.clone();
        if request.source.target == destination.target {
            return Err("源和目标必须是不同的物理磁盘。".into());
        }
        let source_range = range(&request, &source)?;
        request.errors.prepare(source_range.sector)?;
        let partition = destination.partition.take();
        let target = maintenance::write_context(&destination, None)?;
        if source.disk.number == target.resolved.disk.number {
            return Err("源和目标必须是不同的物理磁盘。".into());
        }
        let (base, size, destination_label) =
            destination_extent(&target, partition, source_range.sector)?;
        let start_sector = match legacy
            .as_ref()
            .and_then(|report| report.destination_start_lba)
        {
            Some(absolute) => absolute
                .checked_sub(base / source_range.sector)
                .ok_or("复制记录中的目标范围不一致。")?,
            None => start_sector,
        };
        let destination_range = Options {
            start_sector,
            sector_count: request.options.sector_count,
        }
        .range(base, size, source_range.sector)?;
        if matches!(&checkpoint, DeviceCheckpoint::Resume(_)) {
            protect_offline_target(&target, destination_range)?;
        } else {
            maintenance::protect_partitions(&target, destination_range)?;
        }
        let mut file = data_windows::device(&target.resolved, false)?;
        let table = target_table(&target, &mut file, &destination.target, destination_range)?;
        if let DeviceCheckpoint::Resume(opened) = &checkpoint {
            let device = opened.plan.device.as_ref().ok_or("断点缺少目标磁盘。")?;
            if opened.plan.source_capacity != source.disk.size
                || opened.plan.range != source_range
                || device.capacity != target.resolved.size
                || device.range != destination_range
                || device.table_digest != table_digest(&table)?
            {
                return Err("扇区复制断点中的源盘、目标盘或 GPT 已变化。".into());
            }
            data_windows::confirm_writable_offline(&file)?;
        }
        let source_label = range_label(&source.label, source_range);
        let target_label = range_label(&destination_label, destination_range);
        Ok((
            DeviceCopy {
                request,
                before_source: source,
                source_range,
                destination,
                destination_scope,
                start_sector,
                destination_fingerprint: target.fingerprint,
                destination_range,
                destination_label,
                table,
                checkpoint,
                legacy,
            },
            source_label,
            target_label,
            source_range.size,
        ))
    }

    fn table_digest(table: &Option<Archive>) -> Result<Option<[u8; 32]>, String> {
        table
            .as_ref()
            .map(|archive| Ok(Sha256::digest(archive.encode()?).into()))
            .transpose()
    }

    /// A previously authorized GPT must still match both native and raw
    /// evidence. With that proof, only flags Windows can omit offline are
    /// accepted as unavailable. Explicit protection always rejects a write.
    fn protect_offline_target(ctx: &TableReadContext, range: Range) -> Result<(), String> {
        let target = &ctx.resolved;
        if target.disk.is_offline != Some(true) || !target.volumes.is_empty() {
            return Err("磁盘续作要求原目标盘保持脱机且没有已关联的卷。".into());
        }
        let end = range.offset + range.size;
        for part in ctx
            .parts
            .iter()
            .filter(|part| part.offset < end && range.offset < part.offset + part.size)
        {
            if !offline_data_partition(part) {
                return Err("断点内的目标分区受保护或状态已变化。".into());
            }
        }
        Ok(())
    }

    fn offline_data_partition(part: &crate::partitions::policy::Partition) -> bool {
        use crate::partitions::policy::{guid, BASIC_DATA};
        part.gpt_type.as_deref().map(guid).as_deref() == Some(BASIC_DATA)
            && part.is_boot == Some(false)
            && part.is_system == Some(false)
            && part.is_offline != Some(false)
            && part.drive_letter.as_deref().is_none_or(str::is_empty)
            && ![part.is_hidden, part.is_read_only, part.is_shadow_copy].contains(&Some(true))
    }

    #[cfg(test)]
    #[test]
    fn offline_resume_allows_only_missing_offline_attributes_on_a_normal_partition() {
        use crate::partitions::policy::{Partition, BASIC_DATA};
        let ordinary = Partition {
            gpt_type: Some(BASIC_DATA.into()),
            is_system: Some(false),
            is_boot: Some(false),
            is_offline: Some(true),
            ..Default::default()
        };
        assert!(offline_data_partition(&ordinary));
        for changed in [
            Partition {
                is_system: None,
                ..ordinary.clone()
            },
            Partition {
                is_boot: Some(true),
                ..ordinary.clone()
            },
            Partition {
                is_offline: Some(false),
                ..ordinary.clone()
            },
            Partition {
                drive_letter: Some("F".into()),
                ..ordinary.clone()
            },
            Partition {
                is_read_only: Some(true),
                ..ordinary.clone()
            },
            Partition {
                is_hidden: Some(true),
                ..ordinary.clone()
            },
            Partition {
                is_shadow_copy: Some(true),
                ..ordinary.clone()
            },
            Partition {
                gpt_type: None,
                ..ordinary.clone()
            },
        ] {
            assert!(!offline_data_partition(&changed));
        }
    }

    impl DeviceCopy {
        pub(crate) fn execute(
            self,
            id: &str,
            cancel: &AtomicBool,
        ) -> Result<(String, Option<String>), String> {
            let started = Instant::now();
            let locale = native_locale::current();
            let progress = |phase: &str, done: u64| {
                if cancel.load(Ordering::Relaxed) {
                    return Err("任务已取消。".to_owned());
                }
                data_tasks::record_progress(id, phase, done, self.source_range.size);
                Ok(())
            };
            progress("准备", 0)?;
            let source = data_windows::resolve(&self.request.source, true, false)?;
            let destination = maintenance::write_context(&self.destination, None)?;
            if !self.before_source.same_data_binding(&source)
                || range(&self.request, &source)? != self.source_range
                || source.disk.number == destination.resolved.disk.number
                || destination.fingerprint != self.destination_fingerprint
            {
                return Err("身份、布局或保护与预览不同".into());
            }
            let resuming = matches!(&self.checkpoint, DeviceCheckpoint::Resume(_));
            let checkpointed = !matches!(&self.checkpoint, DeviceCheckpoint::None);
            if resuming {
                protect_offline_target(&destination, self.destination_range)?;
            } else {
                maintenance::protect_partitions(&destination, self.destination_range)?;
            }
            progress("锁定目标", 0)?;
            let source_locks = data_windows::state_locks(&source)?;
            let destination_locks = data_windows::lock(&destination.resolved)?;
            let source =
                data_windows::resolve_locked(&self.request.source, false, &source, &source_locks)?;
            let target = maintenance::write_context(
                &self.destination,
                Some((&destination, &destination_locks)),
            )?;
            let mut reader = data_windows::device(&source, false)?;
            let mut file = data_windows::exclusive_device(&target.resolved)?;
            let confirm_table = |file: &mut File| {
                if target_table(
                    &target,
                    file,
                    &self.destination.target,
                    self.destination_range,
                )? != self.table
                {
                    return Err("GPT 与预览不同".to_owned());
                }
                Ok(())
            };
            confirm_table(&mut file)?;
            let policy = self.request.errors.prepare(self.source_range.sector)?;
            let mut imported_state = if let Some(report) = &self.legacy {
                let mut view = View::bounded(
                    &mut file,
                    self.destination_range.offset,
                    self.destination_range.size,
                    target.resolved.size,
                )
                .map_err(|e| e.to_string())?;
                let mut state = range_copy::start_filled_reread(
                    &mut view,
                    self.source_range,
                    report,
                    None,
                    progress,
                )?;
                state.corroborate_source(
                    &mut reader,
                    &mut view,
                    self.source_range,
                    report.direction,
                    progress,
                )?;
                Some(state)
            } else {
                None
            };
            progress("锁定目标", 0)?;
            let (mut continuation, previous) = match self.checkpoint {
                DeviceCheckpoint::None => (None, None),
                DeviceCheckpoint::New(location) => {
                    let plan = checkpoint::Plan {
                        request: self.request.clone(),
                        range: self.source_range,
                        source_capacity: source.disk.size,
                        output_id: None,
                        output_disk: location.disk.clone(),
                        device: Some(checkpoint::DevicePlan {
                            selection: self.destination_scope,
                            start_sector: self.start_sector,
                            range: self.destination_range,
                            capacity: target.resolved.size,
                            table_digest: table_digest(&self.table)?,
                        }),
                    };
                    (
                        Some(ActiveCheckpoint {
                            journal: location.create()?,
                            plan,
                            state: None,
                        }),
                        None,
                    )
                }
                DeviceCheckpoint::Resume(opened) => {
                    protect_offline_target(&target, self.destination_range)?;
                    opened.journal.validate()?;
                    let mut plan = opened.plan;
                    plan.request = self.request.clone();
                    (
                        Some(ActiveCheckpoint {
                            journal: opened.journal,
                            plan,
                            state: Some(opened.state),
                        }),
                        opened.verified,
                    )
                }
            };
            if !resuming {
                let result = if checkpointed {
                    data_windows::persist_offline(&file)
                } else {
                    data_windows::offline(&file)
                };
                result.map_err(|e| format!("磁盘可能已脱机，尚未发送数据写入：{e}"))?;
            }
            let partial_error = |error: String| {
                locale.format(
                    "目标可能已部分覆盖，请核对脱机状态和写入范围。{error}",
                    &[("error", locale.text(&error))],
                )
            };
            let (digest, failures) = (|| {
                confirm_table(&mut file)?;
                data_windows::confirm_writable_offline(&file)?;
                let mut view = View::bounded(
                    &mut file,
                    self.destination_range.offset,
                    self.destination_range.size,
                    target.resolved.size,
                )
                .map_err(|e| e.to_string())?;
                if let Some(previous) = &previous {
                    continuation
                        .as_mut()
                        .ok_or("缺少已验证的复制断点。")?
                        .reread(&mut reader, &mut view, previous, progress)?;
                }
                if let (Some(active), Some(state)) = (&mut continuation, &imported_state) {
                    active.journal.commit(&active.plan, state)?;
                }
                let state = imported_state
                    .take()
                    .or_else(|| continuation.as_mut().and_then(|active| active.state.take()));
                let use_state = state.is_some() || checkpointed;
                let mut commit = |state: &range_copy::ResumeState| {
                    if let Some(active) = &mut continuation {
                        active.journal.commit(&active.plan, state)?;
                    }
                    Ok(())
                };
                let output = range_copy::copy_resumable(
                    reader,
                    &mut view,
                    self.source_range,
                    self.request.direction,
                    policy,
                    range_copy::Output::Device,
                    |view| view.inner_mut().sync_all(),
                    use_state.then_some(range_copy::Checkpoint {
                        state,
                        commit: &mut commit,
                    }),
                    progress,
                )?;
                confirm_table(&mut file)?;
                data_windows::confirm_writable_offline(&file)?;
                data_windows::resolve_locked(&self.request.source, false, &source, &source_locks)?;
                Ok::<_, String>(output)
            })()
            .map_err(partial_error)?;
            let report = Report {
                version: 2,
                start_lba: self.source_range.offset / self.source_range.sector,
                sector_size: self.source_range.sector,
                sector_count: self.request.options.sector_count,
                direction: self.request.direction,
                errors: self.request.errors,
                output_name: self.destination_label,
                destination_start_lba: Some(
                    self.destination_range.offset / self.destination_range.sector,
                ),
                sha256: digest.clone(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                failures,
            };
            let message = locale
                .text("扇区复制完成，已回读校验目标范围。目标磁盘保持脱机。")
                .to_owned();
            let result = ResultRecord {
                id: id.into(),
                sources: vec![self.request.source.target],
                destination: Some(self.destination.target),
                report,
            };
            data_tasks::complete_verified(id, message.clone(), digest.clone(), || {
                if let Some(active) = &mut continuation {
                    active.journal.finish(&active.plan, &result.report)?;
                }
                *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                Ok(())
            })
            .map_err(partial_error)?;
            Ok((message, Some(digest)))
        }
    }

    pub(super) fn start(request: Request) -> Result<Option<String>, String> {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let before = data_windows::resolve(&request.source, true, false)?;
        let planned = range(&request, &before)?;
        request.errors.prepare(planned.sector)?;
        let Some(path) = save_file_dialog(planned) else {
            return Ok(None);
        };
        launch_file(guard, request, before, planned, path, FileMode::New)
    }

    pub(super) fn save_file_dialog(planned: Range) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(native_locale::text("导出扇区范围到另一块磁盘"))
            .add_filter(native_locale::text("扇区数据"), &["bin"])
            .set_file_name(format!(
                "DiskStudio-LBA-{}-{}.bin",
                planned.offset / planned.sector,
                planned.size / planned.sector
            ))
            .save_file()
    }

    enum FileMode {
        New,
        Resume(ActiveCheckpoint),
        Reread {
            report: Report,
            original: backup_files::Input,
            previous: Option<range_copy::ResumeState>,
        },
    }

    fn pick_legacy_report(source: &Target) -> Result<Option<Report>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(native_locale::text("打开扇区复制 JSON 报告"))
            .add_filter("JSON", &["json"])
            .pick_file()
        else {
            return Ok(None);
        };
        let input = backup_files::Input::open(&path, Some(source))?;
        Ok(Some(read_legacy_report(input.file)?))
    }

    fn legacy_request(
        source: Selection,
        report: &Report,
        disk: &Resolved,
    ) -> Result<Request, String> {
        legacy_source_request(
            source,
            report,
            disk.offset,
            disk.size,
            disk.disk.logical_sector_size.ok_or("扇区大小缺失")?,
        )
    }

    fn pick_reread_output(planned: Range) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(native_locale::text("保存复读后的新扇区文件"))
            .add_filter(native_locale::text("扇区数据"), &["bin"])
            .set_file_name(format!(
                "DiskStudio-LBA-{}-reread.bin",
                planned.offset / planned.sector
            ))
            .save_file()
    }

    pub(super) fn reread_file_report(source: Selection) -> Result<Option<ResumedFile>, String> {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let Some(report) = pick_legacy_report(&source.target)? else {
            return Ok(None);
        };
        if report.version != 1 {
            return Err("请选择文件导出的扇区复制报告。".into());
        }
        let before = data_windows::resolve(&source, true, false)?;
        let request = legacy_request(source, &report, &before)?;
        let planned = range(&request, &before)?;
        request.errors.prepare(planned.sector)?;
        let Some(original_path) = rfd::FileDialog::new()
            .set_title(native_locale::text("为旧报告选择原扇区导出文件"))
            .add_filter(native_locale::text("扇区数据"), &["bin"])
            .pick_file()
        else {
            return Ok(None);
        };
        let original = backup_files::Input::open(&original_path, Some(&request.source.target))?;
        if original.file.metadata().map_err(data_windows::err)?.len() != planned.size {
            return Err("原副本长度与复制记录不一致。".into());
        }
        let Some(path) = pick_reread_output(planned) else {
            return Ok(None);
        };
        launch_resumed_file(
            guard,
            request,
            before,
            planned,
            path,
            FileMode::Reread {
                original,
                report,
                previous: None,
            },
        )
    }

    pub(super) fn resume_file(
        source: Selection,
        reread_filled: bool,
    ) -> Result<Option<ResumedFile>, String> {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let Some(checkpoint::Opened {
            journal,
            plan,
            state,
            verified,
        }) = select_checkpoint(&source)?
        else {
            return Ok(None);
        };
        if plan.device.is_some() {
            return Err("请从目标磁盘模式打开此断点。".into());
        }
        let mut request = plan.request.clone();
        let before = data_windows::resolve(&request.source, true, false)?;
        let planned = range(&request, &before)?;
        if before.disk.size != plan.source_capacity || planned != plan.range {
            return Err("断点中的源盘容量或复制范围已变化。".into());
        }
        request.errors.prepare(planned.sector)?;
        let original_path = journal.output_path();
        let (path, mode) = if reread_filled {
            let report = verified.ok_or("请选择已完成的扇区复制断点。")?;
            if report.version != 1 || report.destination_start_lba.is_some() {
                return Err("复制记录中的目标范围不一致。".into());
            }
            range_copy::filled_spans(planned, &report)?;
            request.errors = report.errors.for_filled_reread()?;
            let original = backup_files::Input::open(&original_path, Some(&request.source.target))?;
            if original.disk != plan.output_disk
                || Some(backup_files::sector_file_identity(&original.file)?) != plan.output_id
                || original.file.metadata().map_err(data_windows::err)?.len() != planned.size
            {
                return Err("复制断点与原导出文件不一致。".into());
            }
            let Some(path) = pick_reread_output(planned) else {
                return Ok(None);
            };
            (
                path,
                FileMode::Reread {
                    original,
                    report,
                    previous: Some(state),
                },
            )
        } else {
            (
                original_path,
                FileMode::Resume(ActiveCheckpoint {
                    journal,
                    plan,
                    state: Some(state),
                }),
            )
        };
        launch_resumed_file(guard, request, before, planned, path, mode)
    }

    fn launch_resumed_file(
        guard: std::sync::MutexGuard<'_, ()>,
        request: Request,
        before: Resolved,
        planned: Range,
        path: PathBuf,
        mode: FileMode,
    ) -> Result<Option<ResumedFile>, String> {
        let resumed = launch_file(guard, request.clone(), before, planned, path, mode)?;
        Ok(resumed.map(|id| ResumedFile {
            id,
            options: request.options,
            direction: request.direction,
            errors: request.errors,
        }))
    }

    fn launch_file(
        guard: std::sync::MutexGuard<'_, ()>,
        request: Request,
        before: Resolved,
        planned: Range,
        path: PathBuf,
        mode: FileMode,
    ) -> Result<Option<String>, String> {
        let done = match &mode {
            FileMode::Resume(active) => active
                .state
                .as_ref()
                .map_or(0, range_copy::ResumeState::done),
            _ => 0,
        };
        launch_file_job(guard, done, planned.size, move |id, cancel| {
            run(id, cancel, request, &before, &path, mode)
        })
    }

    pub(super) fn launch_file_job(
        guard: std::sync::MutexGuard<'_, ()>,
        done: u64,
        size: u64,
        run: impl FnOnce(&str, &AtomicBool) -> Result<(), String> + Send + 'static,
    ) -> Result<Option<String>, String> {
        let (id, cancel) = data_tasks::start("复制扇区")?;
        data_tasks::record_progress(&id, "准备", done, size);
        drop(guard);
        let work_id = id.clone();
        if let Err(error) = std::thread::Builder::new().spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = crate::OPERATION
                    .try_lock()
                    .map_err(|_| "另一项磁盘操作正在运行。")?;
                run(&work_id, &cancel)
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

    fn run(
        id: &str,
        cancel: &AtomicBool,
        request: Request,
        before: &Resolved,
        path: &Path,
        mode: FileMode,
    ) -> Result<(), String> {
        let started = Instant::now();
        let fresh = data_windows::resolve(&request.source, true, false)?;
        if !before.same_data_binding(&fresh) {
            return Err("磁盘布局已变化，请刷新并重新选择。".into());
        }
        let bounds = range(&request, &fresh)?;
        if !matches!(&mode, FileMode::Resume(_)) {
            data_windows::destination_folder(
                path.parent().ok_or("输出目录无效")?,
                &request.source.target,
                bounds.size,
            )?;
        }
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
        let mut reader = data_windows::device(&locked, false)?;
        let policy = request.errors.prepare(bounds.sector)?;
        progress("复制扇区", 0)?;
        let (mut file, publication, mut active, reread) = match mode {
            FileMode::Resume(active) => {
                let plan = &active.plan;
                let (file, publication) =
                    crate::backup_files::reopen_sector_output(path, &request.source.target)?;
                if Some(publication.file_identity()?) != plan.output_id
                    || publication.binding().target() != &plan.output_disk
                    || active.journal.disk != plan.output_disk
                {
                    return Err("断点记录与未完成的输出文件不一致。".into());
                }
                (file, publication, active, None)
            }
            mode => {
                let (file, publication) = crate::backup_files::create_sector_output(
                    path,
                    &request.source.target,
                    bounds.size,
                )?;
                let journal = checkpoint::Journal::create_for_file(path, &request.source.target)?;
                let plan = checkpoint::Plan {
                    request: request.clone(),
                    range: bounds,
                    source_capacity: locked.disk.size,
                    output_id: Some(publication.file_identity()?),
                    output_disk: publication.binding().target().clone(),
                    device: None,
                };
                if journal.disk != plan.output_disk {
                    return Err("断点记录与未完成的输出文件不一致。".into());
                }
                let reread = match mode {
                    FileMode::Reread {
                        original,
                        report,
                        previous,
                    } => Some((original, report, previous)),
                    _ => None,
                };
                (
                    file,
                    publication,
                    ActiveCheckpoint {
                        journal,
                        plan,
                        state: None,
                    },
                    reread,
                )
            }
        };
        let filename = path.file_name().ok_or("输出文件名无效")?.to_string_lossy();
        (|| {
            if let Some((mut original, report, previous)) = reread {
                data_windows::FileBinding::validate_all(std::slice::from_ref(&original.binding))?;
                let digest = copy_verified(
                    &mut original.file,
                    &mut file,
                    bounds.size,
                    File::sync_all,
                    |_, done| progress("复制原扇区文件", done),
                )?;
                if digest != report.sha256 {
                    return Err("原副本的 SHA-256 与复制记录不一致。".into());
                }
                active.state = previous;
                active.reread(&mut reader, &mut file, &report, progress)?;
            }
            let state = active.state.take();
            let mut commit =
                |state: &range_copy::ResumeState| active.journal.commit(&active.plan, state);
            let (digest, failures) = range_copy::copy_resumable(
                reader,
                &mut file,
                bounds,
                request.direction,
                policy,
                range_copy::Output::File,
                |file| file.sync_all(),
                Some(range_copy::Checkpoint {
                    state,
                    commit: &mut commit,
                }),
                progress,
            )?;
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
                sources: vec![request.source.target],
                destination: None,
                report: Report {
                    version: 1,
                    start_lba: bounds.offset / bounds.sector,
                    sector_size: bounds.sector,
                    sector_count: bounds.size / bounds.sector,
                    direction: request.direction,
                    errors: request.errors,
                    output_name: filename.to_string(),
                    destination_start_lba: None,
                    sha256: digest.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    failures,
                },
            };
            data_tasks::complete_verified(id, message, digest, || {
                active.journal.finish(&active.plan, &result.report)?;
                publication.publish()?;
                *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                Ok(())
            })
        })()
        .map_err(|error: String| {
            let locale = native_locale::current();
            let message = if active.journal.has_state() {
                "{error} 未完成文件保留为 {name}。选择 {checkpoint} 续作。"
            } else {
                "{error} 未完成文件保留为 {name}。"
            };
            locale.format(
                message,
                &[
                    ("error", locale.text(&error)),
                    ("name", &format!("{filename}.partial")),
                    ("checkpoint", &format!("{filename}.dscopy")),
                ],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn standalone_reports_bind_exact_scope_and_reject_unknown_versions_and_fields() {
        let mut json = serde_json::json!({
            "version": 1, "startLba": 43, "sectorSize": 512, "sectorCount": 8,
            "direction": "backward", "errors": {"kind": "fill", "skipSectors": 2, "marker": "LOSS"},
            "outputName": "copy.bin", "sha256": "9".repeat(64), "elapsedMs": 1,
            "failures": {
                "filledSectors": 3, "unreadableSectors": 1, "omitted": 0,
                "ranges": [{"failedLba": 45, "firstFilledLba": 43, "filledSectors": 3, "windowsCode": 1117}]
            }
        });
        let parse = |value: &serde_json::Value| read_legacy_report(value.to_string().as_bytes());
        let source = Selection {
            target: crate::partitions::Target("1".repeat(64)),
            layout_key: "layout".into(),
            partition: Some(3),
        };
        let report = parse(&json).unwrap();
        let request =
            legacy_source_request(source.clone(), &report, 40 * 512, 12 * 512, 512).unwrap();
        assert_eq!(request.options.start_sector, 3);
        assert_eq!(request.options.sector_count, 8);
        assert!(legacy_source_request(source.clone(), &report, 44 * 512, 12 * 512, 512).is_err());
        assert!(legacy_source_request(source.clone(), &report, 40 * 512, 10 * 512, 512).is_err());
        assert!(legacy_source_request(source, &report, 0, 4096 * 100, 4096).is_err());
        json["version"] = 2.into();
        assert!(parse(&json).is_err());
        json["destinationStartLba"] = 70.into();
        assert_eq!(parse(&json).unwrap().destination_start_lba, Some(70));
        json["path"] = "ignored.bin".into();
        assert!(parse(&json).is_err());
        assert!(read_legacy_report(std::io::repeat(b' ').take(512 * 1024 + 1)).is_err());
    }

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

//! Explicit single-confirmation Windows GPT space reallocation.
use super::*;
use crate::partition_move_task::{self, JournalLocation};
use crate::partitions::merge;
use crate::partitions::reallocation::{self as model, PartitionRef, Step};
use crate::{data_tasks::Selection, data_windows, extfs, table_archive, table_restore};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const CONFIRM_FOR: Duration = Duration::from_secs(300);

static PENDING_BATCH: Mutex<Option<PendingBatch>> = Mutex::new(None);
static RUNNING_BATCH: Mutex<Option<RunningBatch>> = Mutex::new(None);

struct RunningBatch {
    token: String,
    cancel: Arc<AtomicBool>,
}

impl RunningBatch {
    fn request_stop(&self, token: &str) -> bool {
        if token != self.token {
            return false;
        }
        self.cancel.store(true, Ordering::Relaxed);
        true
    }
}

pub(crate) fn stop(token: &str) -> bool {
    let running = RUNNING_BATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    running
        .as_ref()
        .is_some_and(|batch| batch.request_stop(token))
}

struct PendingBatch {
    request: model::Request,
    preview: model::Preview,
    snapshot: Snapshot,
    journals: VecDeque<JournalLocation>,
    deadline: Instant,
}

pub(super) struct Snapshot {
    disk: Disk,
    parts: Vec<Partition>,
    volumes: Vec<Option<Volume>>,
    pub archive: table_archive::Archive,
    pub layout: table_restore::Layout,
    native_protected: Vec<String>,
}

impl Snapshot {
    fn read(session: &Session, target: &Target) -> Result<Self, String> {
        let disk = session.disk(target)?;
        if disk.partition_style != Some(2) {
            return Err("空间重新分配要求已初始化的 GPT 磁盘。".into());
        }
        let parts = policy::layout(disk, &session.partitions)?;
        let snapshot = volume_snapshot::read(session, disk)?;
        let volumes = parts
            .iter()
            .map(|part| snapshot.volume(disk, part))
            .collect::<Result<Vec<_>, _>>()?;
        let mut volume_keys = HashSet::new();
        for path in volumes
            .iter()
            .flatten()
            .filter_map(|volume| volume.volume_path.as_deref())
        {
            if !volume_keys.insert(volume_manage::volume_key(path)?) {
                return Err("同一卷标识对应多个分区。".into());
            }
        }
        let source = source(target, disk, &parts, None);
        let context = table_inspect_context(&source)?;
        if &context.resolved.disk != disk || !context.parts.iter().eq(parts.iter().copied()) {
            return Err("读取卷关联后磁盘或分区信息已经改变。".into());
        }
        let rows: Vec<TableDiskGuid> = session
            .con
            .raw_query("SELECT __PATH,Number,PartitionStyle,Guid FROM MSFT_Disk")
            .map_err(err)?;
        let guid = unique_table_guid(&rows, disk)?;
        let sector = disk.logical_sector_size.ok_or("缺少逻辑扇区大小。")?;
        let mut file = data_windows::device(&context.resolved, false)?;
        let archive = table_archive::capture(
            &mut file,
            disk.size,
            sector,
            target.0.clone(),
            false,
            || Ok(()),
        )?;
        let layout = table_restore::archive_layout(&archive)?;
        let (native, mut native_protected, _) = extfs::restore_native(&file, Some(&parts), sector)?;
        if guid != layout.guid || !layout.agrees(&native) {
            return Err("Windows 存储提供程序、原生布局和主备 GPT 无法互相核对。".into());
        }
        archive.matches(&mut file, || Ok(()))?;
        native_protected.sort_unstable();
        if Session::open()?.revision(target)? != session.revision(target)? {
            return Err("原生检查期间布局或磁盘保护信息已经改变。".into());
        }
        Ok(Self {
            disk: disk.clone(),
            parts: context.parts,
            volumes,
            archive,
            layout,
            native_protected,
        })
    }

    fn original(&self, reference: &PartitionRef) -> Result<(&Partition, &Volume), String> {
        let index = self
            .parts
            .iter()
            .position(|part| {
                part.guid.as_deref().map(policy::guid).as_ref() == Some(&reference.guid)
            })
            .ok_or("原始布局中缺少所选 GPT 分区。")?;
        let part = &self.parts[index];
        if !policy::formattable(part) || self.native_protected.contains(&reference.guid) {
            return Err("原生分区或 Windows 卷受保护，已拒绝本批操作。".into());
        }
        let volume = self.volumes[index]
            .as_ref()
            .ok_or("所选分区缺少唯一的 Windows 卷。")?;
        volume_manage::volume_key(volume.volume_path.as_deref().ok_or("卷标识缺失。")?)?;
        Ok((part, volume))
    }

    fn exact_layout_and_volumes(&self, expected: &Self) -> Result<(), String> {
        if self.disk != expected.disk
            || self.parts != expected.parts
            || self.volumes != expected.volumes
            || self.native_protected != expected.native_protected
            || self.archive != expected.archive
        {
            return Err("磁盘、主备 GPT、保护信息或卷关联已经变化。".into());
        }
        Ok(())
    }

    fn part_refs(&self) -> Vec<&Partition> {
        self.parts.iter().collect()
    }

    fn new_entry(
        &self,
        entry: &crate::partitions::quick::Entry,
    ) -> Result<(&Partition, &Volume), String> {
        self.parts
            .iter()
            .zip(&self.volumes)
            .find(|(part, _)| {
                (part.offset, part.size) == (entry.offset, entry.size) && policy::formattable(part)
            })
            .and_then(|(part, volume)| volume.as_ref().map(|volume| (part, volume)))
            .ok_or_else(|| "缺少符合确认范围的新分区或普通 Windows 卷。".into())
    }

    fn verify_transition(&self, step: &Step, next: &Self) -> Result<(), String> {
        step.verify_geometry(&self.layout, &next.layout)?;
        let is_mbr = |extent: &&table_archive::Extent| extent.kind == table_archive::Kind::Mbr;
        let mbr_matches = self.archive.extents.iter().filter(is_mbr).eq(next
            .archive
            .extents
            .iter()
            .filter(is_mbr));
        if !mbr_matches || matches!(step, Step::Format { .. }) && self.archive != next.archive {
            return Err("步骤后 GPT 之外的保护扇区或格式化步骤的布局已改变。".into());
        }
        let mut expected_disk = self.disk.clone();
        expected_disk.number_of_partitions = Some(next.parts.len() as u32);
        if expected_disk != next.disk || self.native_protected != next.native_protected {
            return Err("步骤后的磁盘或原生保护状态与确认不同。".into());
        }
        if let Step::Create { entry } = step {
            let (_, volume) = next.new_entry(entry)?;
            volume_manage::volume_key(volume.volume_path.as_deref().ok_or("新卷标识缺失。")?)?;
        }
        for (part, volume) in self.parts.iter().zip(&self.volumes) {
            if matches!(step, Step::CopyAndRemove { absorb, .. } if reported_guid(part, absorb)) {
                continue;
            }
            let Some((current, current_volume)) = next
                .parts
                .iter()
                .zip(&next.volumes)
                .find(|(current, _)| current.guid == part.guid)
            else {
                return Err("步骤后的原有 GPT 分区已消失。".into());
            };
            let mut expected = part.clone();
            // WMI's path and number are runtime selectors. Both snapshots have
            // already bound the full local provider objects in both directions.
            expected.partition_number = current.partition_number;
            expected.path.clone_from(&current.path);
            let mut expected_volume = volume.clone();
            match step {
                Step::Resize {
                    partition,
                    new_size,
                } if reported_guid(part, partition) => {
                    expected.size = *new_size;
                    if let (Some(old), Some(new)) = (&mut expected_volume, current_volume) {
                        old.size = new.size;
                    }
                }
                Step::Move {
                    partition,
                    new_offset,
                } if reported_guid(part, partition) => {
                    expected.offset = *new_offset;
                }
                Step::Format { entry }
                    if (part.offset, part.size) == (entry.offset, entry.size) =>
                {
                    let format = entry.format.as_ref().ok_or("缺少格式化设置。")?;
                    let old = expected_volume.as_mut().ok_or("缺少格式化前卷关联。")?;
                    let new = current_volume.as_ref().ok_or("缺少格式化后卷关联。")?;
                    if !new.file_system.as_deref().is_some_and(|fs| {
                        fs.eq_ignore_ascii_case(format.file_system.windows_name())
                    }) || new.file_system_label.as_deref() != Some(format.label.as_str())
                    {
                        return Err("格式化后卷的文件系统或卷标不符合确认。".into());
                    }
                    old.file_system.clone_from(&new.file_system);
                    old.file_system_label.clone_from(&new.file_system_label);
                    old.size = new.size;
                }
                _ => {}
            }
            if &expected != current || !same_volume(&expected_volume, current_volume) {
                return Err("步骤后分区保护信息、盘符或卷关联与确认变化不同。".into());
            }
        }
        Ok(())
    }
}

fn reported_guid(part: &Partition, reference: &PartitionRef) -> bool {
    part.guid.as_deref().map(policy::guid).as_ref() == Some(&reference.guid)
}

fn same_volume(before: &Option<Volume>, after: &Option<Volume>) -> bool {
    match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) if before == after => true,
        (Some(before), Some(after)) => {
            let before_key = before
                .volume_path
                .as_deref()
                .and_then(|path| volume_manage::volume_key(path).ok());
            let after_key = after
                .volume_path
                .as_deref()
                .and_then(|path| volume_manage::volume_key(path).ok());
            if before_key.is_none() || before_key != after_key || after.object_id.is_empty() {
                return false;
            }
            let mut expected = before.clone();
            expected.path.clone_from(&after.path);
            expected.object_id.clone_from(&after.object_id);
            expected.volume_path.clone_from(&after.volume_path);
            &expected == after
        }
        _ => false,
    }
}

fn preflight(request: &model::Request) -> Result<(Snapshot, Vec<Step>), String> {
    let session = Session::open()?;
    if session.revision(&request.target)? != request.revision {
        return Err("布局已经变化，请刷新后重新预览。".into());
    }
    let snapshot = Snapshot::read(&session, &request.target)?;
    let steps = model::plan(
        &snapshot.disk,
        &snapshot.parts,
        &snapshot.layout,
        &request.operation,
    )?;
    for (index, step) in steps.iter().enumerate() {
        match step {
            Step::Resize {
                partition,
                new_size,
            } => {
                let (part, volume) = snapshot.original(partition)?;
                validate_ntfs(&session, part, volume)?;
                if index == 0 || *new_size < partition.size {
                    let bound = Request {
                        target: request.target.clone(),
                        revision: request.revision.clone(),
                        action: Action::Resize {
                            partition: part.partition_number,
                            size: *new_size,
                        },
                    };
                    session.bind(&bound)?;
                }
            }
            Step::Move { partition, .. } => {
                let (part, volume) = snapshot.original(partition)?;
                session.protect_paging(part, volume)?;
                if !volume
                    .file_system
                    .as_deref()
                    .is_some_and(crate::partition_move::supports_filesystem)
                {
                    return Err("至少一个需要搬移的分区使用不支持的文件系统。".into());
                }
            }
            Step::CopyAndRemove { keep, absorb } => {
                for reference in [keep, absorb] {
                    let (part, volume) = snapshot.original(reference)?;
                    validate_ntfs(&session, part, volume)?;
                }
                let part = absorb.resolve(&snapshot.part_refs())?;
                session.bind(&Request {
                    target: request.target.clone(),
                    revision: request.revision.clone(),
                    action: Action::Delete {
                        partition: part.partition_number,
                    },
                })?;
            }
            Step::Create { .. } | Step::Format { .. } | Step::Offline => {}
        }
    }
    Ok((snapshot, steps))
}

fn validate_ntfs(session: &Session, part: &Partition, volume: &Volume) -> Result<(), String> {
    if volume.file_system.as_deref() != Some("NTFS") {
        return Err("当前多步骤缩放和合并要求从 Windows 唯一识别的普通 NTFS 卷开始。".into());
    }
    session.protect_paging(part, volume)
}

fn consume(
    pending: &mut Option<PendingBatch>,
    token: &str,
    now: Instant,
) -> Result<PendingBatch, String> {
    let pending = pending
        .take()
        .ok_or("空间重新分配确认已失效，请重新预览。")?;
    if token != pending.preview.confirmation.token || now >= pending.deadline {
        return Err("空间重新分配确认不匹配或已过期。".into());
    }
    Ok(pending)
}

pub(crate) fn discard(token: &str) {
    let mut pending = PENDING_BATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if pending
        .as_ref()
        .is_some_and(|pending| pending.preview.confirmation.token == token)
    {
        *pending = None;
    }
}

pub(crate) fn prepare(request: model::Request) -> Result<Option<model::Preview>, String> {
    let _operation = OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    *PENDING_BATCH
        .lock()
        .map_err(|_| "空间重新分配预览状态不可用。")? = None;
    let title = match request.operation {
        model::Operation::Split { .. } => "拆分分区",
        model::Operation::Allocate { .. } => "分配未分配空间",
        model::Operation::Transfer { .. } => "在分区间重新分配空间",
        model::Operation::Merge { .. } => "合并分区",
    };
    let (snapshot, mut steps) = preflight(&request)?;
    let move_count = steps
        .iter()
        .filter(|step| matches!(step, Step::Move { .. }))
        .count();
    let journals = if move_count == 0 {
        Vec::new()
    } else {
        let session = Session::open()?;
        device_state::access_identity(&session, session.disk(&request.target)?, true)?;
        let Some(journals) = JournalLocation::select_for_batch(&request.target, move_count)? else {
            return Ok(None);
        };
        Snapshot::read(&Session::open()?, &request.target)?.exact_layout_and_volumes(&snapshot)?;
        journals
    };
    let warning = if move_count == 0 {
        "每一步写后核对通过才会继续。缩放结果取决于 Windows 的实际允许范围。失败会停止剩余步骤，请检查已完成的部分。"
    } else {
        steps.push(Step::Offline);
        "搬移时整块磁盘将脱机。每段数据和 GPT 核对通过后临时联机以完成后续步骤，整批完成后磁盘保持脱机。中断时请保留另一块磁盘上的断点文件。"
    };
    let confirmation = uuid::Uuid::new_v4();
    let merge_folder = matches!(request.operation, model::Operation::Merge { .. })
        .then(|| merge::output::CopiedDirectory::name(confirmation));
    let warning = if let Some(folder) = &merge_folder {
        format!(
            "被吸收卷的文件会复制到保留卷根目录下的 {folder}。保留卷当前可用空间需容纳这些文件。持锁复查源卷和目标副本后才会发出删除请求。中断时，已复制的文件会留在该目录中。{warning}"
        )
    } else {
        warning.to_owned()
    };
    let preview = model::Preview {
        confirmation: Plan {
            token: confirmation.to_string(),
            title: title.to_owned(),
            target_name: format!(
                "磁盘 {} · {}",
                snapshot.disk.number,
                snapshot
                    .disk
                    .friendly_name
                    .as_deref()
                    .unwrap_or("未提供型号")
            ),
            serial: snapshot
                .disk
                .serial_number
                .as_deref()
                .unwrap_or("")
                .to_owned(),
            partition: None,
            before_size: None,
            after_size: None,
            warning,
            expires_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(err)?
                .as_millis() as u64
                + CONFIRM_FOR.as_millis() as u64,
        },
        steps,
        journals: journals
            .iter()
            .map(|journal| journal.name().to_owned())
            .collect(),
        merge_folder,
    };
    *PENDING_BATCH
        .lock()
        .map_err(|_| "空间重新分配预览状态不可用。")? = Some(PendingBatch {
        request,
        preview: preview.clone(),
        snapshot,
        journals: journals.into(),
        deadline: Instant::now() + CONFIRM_FOR,
    });
    Ok(Some(preview))
}

pub(crate) fn execute(
    token: &str,
    mut progress: impl FnMut(crate::partitions::quick::Progress),
) -> Result<Outcome, String> {
    use crate::partitions::quick::{Phase, Progress};
    let _operation = OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let pending = {
        let mut pending = PENDING_BATCH
            .lock()
            .map_err(|_| "空间重新分配确认状态不可用。")?;
        consume(&mut pending, token, Instant::now())?
    };
    let PendingBatch {
        request,
        preview,
        snapshot,
        mut journals,
        ..
    } = pending;
    let confirmation = uuid::Uuid::parse_str(&preview.confirmation.token).map_err(err)?;
    progress(Progress {
        phase: Phase::Checking,
        completed: 0,
        total: preview.steps.len(),
        current: None,
    });
    let (mut verified, _) = preflight(&request)?;
    verified.exact_layout_and_volumes(&snapshot)?;
    for journal in &journals {
        journal.ensure_unused()?;
    }
    let (id, cancel) = crate::data_tasks::start(&preview.confirmation.title)?;
    *RUNNING_BATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(RunningBatch {
        token: preview.confirmation.token.clone(),
        cancel: Arc::clone(&cancel),
    });
    let mut reopened = false;
    let mut requested_final_offline = false;
    let mut merge_folder = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        quick::run_steps(
            &preview.steps,
            Step::title,
            |step| {
                if !matches!(step, Step::Offline) {
                    check_cancel(&cancel)?;
                }
                let session = Session::open()?;
                let current = Snapshot::read(&session, &request.target)?;
                current.exact_layout_and_volumes(&verified)?;
                let moved = match step {
                    Step::Offline => {
                        requested_final_offline = true;
                        return offline(&request.target, &current);
                    }
                    Step::Move { .. } => {
                        let journal = journals.pop_front().ok_or("缺少本步骤的移动断点位置。")?;
                        let archive =
                            move_one(step, &current, &request.target, journal, &id, &cancel)?;
                        reopened = true;
                        Some(archive)
                    }
                    Step::CopyAndRemove { keep, absorb } => {
                        let files = stage_merge(
                            &current,
                            &request.target,
                            keep,
                            absorb,
                            confirmation,
                            &cancel,
                        )?;
                        files.remove_source(&current, &request.target, keep, absorb, &cancel)?;
                        merge_folder = Some(merge::output::CopiedDirectory::name(confirmation));
                        None
                    }
                    _ => {
                        storage_step(step, &current, &request.target, session, &cancel)?;
                        None
                    }
                };
                let after = Snapshot::read(&Session::open()?, &request.target)?;
                if let Some(moved) = moved {
                    if moved != after.archive {
                        return Err("联机后 GPT 与已完成移动的记录不同。".into());
                    }
                }
                current.verify_transition(step, &after)?;
                verified = after;
                Ok(())
            },
            progress,
        )
    }))
    .map_err(|_| "空间重新分配异常结束。请检查实际布局和已保存的断点。".to_owned());
    let mut result = result;
    if let Ok(outcome) = &mut result {
        if let Some(folder) = merge_folder {
            outcome
                .message
                .push_str(&format!("\n已合并的文件位于保留卷根目录下的 {folder}。"));
        }
        if reopened && !requested_final_offline && !outcome.verified {
            match settle_after_stop(&request.target, &verified) {
                Ok(()) => outcome.message.push_str("\n磁盘已确认脱机。"),
                Err(error) => outcome
                    .message
                    .push_str(&format!("\n未能确认磁盘脱机：{error}")),
            }
        }
    }
    *RUNNING_BATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    crate::data_tasks::finish(
        &id,
        match &result {
            Ok(result) if result.verified => Ok((result.message.clone(), None)),
            Ok(result) => Err(result.message.clone()),
            Err(error) => Err(error.clone()),
        },
    );
    result
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("空间重新分配已停止。".into())
    } else {
        Ok(())
    }
}

fn offline(target: &Target, expected: &Snapshot) -> Result<(), String> {
    device_state::offline_after_verified_batch(
        source(target, &expected.disk, &expected.part_refs(), None),
        &expected.disk,
        &expected.archive,
    )
}

fn settle_after_stop(target: &Target, expected: &Snapshot) -> Result<(), String> {
    let session = Session::open()?;
    let disk = policy::target_disk(&session.disks, target)?;
    policy::corroborate(disk, &session.legacy)?;
    if disk.is_offline == Some(true) {
        let parts = policy::layout(disk, &session.partitions)?;
        let context = table_inspect_context(&source(target, disk, &parts, None))?;
        return data_windows::verify_offline_device(&context.resolved);
    }
    let current = Snapshot::read(&session, target)?;
    current.exact_layout_and_volumes(expected)?;
    offline(target, expected)
}

fn storage_step(
    step: &Step,
    current: &Snapshot,
    target: &Target,
    session: Session,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let action = match step {
        Step::Resize {
            partition,
            new_size,
        } => {
            let part = partition.resolve(&current.part_refs())?;
            let (_, volume) = current.original(partition)?;
            validate_ntfs(&session, part, volume)?;
            Action::Resize {
                partition: part.partition_number,
                size: *new_size,
            }
        }
        Step::Create { entry } => Action::Create {
            offset: entry.offset,
            size: entry.size,
            kind: PartitionKind::Windows,
        },
        Step::Format { entry } => {
            let (part, _) = current.new_entry(entry)?;
            let format = entry.format.as_ref().ok_or("缺少已确认的格式化设置。")?;
            Action::Format {
                partition: part.partition_number,
                label: format.label.clone(),
                file_system: format.file_system,
                cluster_size: format.cluster_size,
                options: format.options,
            }
        }
        Step::Move { .. } | Step::CopyAndRemove { .. } | Step::Offline => {
            return Err("此步骤需要专用数据核验。".into())
        }
    };
    let request = Request {
        target: target.clone(),
        revision: session.revision(target)?,
        action,
    };
    let (binding, volume) = session.bind(&request)?;
    check_cancel(cancel)?;
    let result = execute_bound(request, session, binding, volume, None)?;
    if result.verified {
        Ok(())
    } else {
        Err(result.message)
    }
}

fn source(target: &Target, disk: &Disk, parts: &[&Partition], partition: Option<u32>) -> Selection {
    Selection {
        target: target.clone(),
        layout_key: crate::disk_identity::layout_key(
            disk.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        ),
        partition,
    }
}

// Only a completed locked readback can construct this permission to remove.
struct VerifiedMerge {
    copied: merge::output::CopiedDirectory,
    absorb: LockedMergeVolume,
    keep: LockedMergeVolume,
}

struct LockedMergeVolume {
    selection: Selection,
    resolved: data_windows::Resolved,
    locks: data_windows::Locks,
}

impl LockedMergeVolume {
    fn revalidate(&self) -> Result<(), String> {
        data_windows::resolve_locked(&self.selection, false, &self.resolved, &self.locks)?;
        Ok(())
    }
}

impl VerifiedMerge {
    fn remove_source(
        self,
        expected: &Snapshot,
        target: &Target,
        keep: &PartitionRef,
        absorb: &PartitionRef,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let folder = String::from_utf16_lossy(&self.copied.name);
        let result = (|| {
            let session = Session::open()?;
            let disk = session.disk(target)?;
            let parts = policy::layout(disk, &session.partitions)?;
            let (kept, keep_volume) = expected.original(keep)?;
            let (removed, absorb_volume) = expected.original(absorb)?;
            if keep.resolve(&parts)? != kept
                || absorb.resolve(&parts)? != removed
                || self.keep.selection.partition != Some(kept.partition_number)
                || self.absorb.selection.partition != Some(removed.partition_number)
            {
                return Err("提交前两个确认分区的身份已变化。".to_owned());
            }
            let keep_volume =
                volume_during_validation(session.associated_volume(kept)?, Some(keep_volume))?
                    .ok_or("提交前缺少保留卷关联。")?;
            validate_ntfs(&session, kept, &keep_volume)?;
            let request = Request {
                target: target.clone(),
                revision: session.revision(target)?,
                action: Action::Delete {
                    partition: removed.partition_number,
                },
            };
            // The narrow retention path is backed by both verified native
            // locks below. A changed or ambiguous association still aborts.
            let (binding, volume) = session.bind_inner(&request, Some(absorb_volume))?;
            self.keep.resolved.verify_partition_snapshot(disk, &parts)?;
            self.absorb
                .resolved
                .verify_partition_snapshot(disk, &parts)?;
            self.keep.revalidate()?;
            self.absorb.revalidate()?;
            let mut handle = data_windows::device(&self.absorb.resolved, false)?;
            expected
                .archive
                .matches(&mut handle, || check_cancel(cancel))?;
            drop(handle);
            check_cancel(cancel)?;
            let outcome = execute_bound(request, session, binding, volume, None)?;
            if !outcome.verified {
                return Err(outcome.message);
            }
            Ok(())
        })();
        result.map_err(|error| format!("已核对的副本保留在目标卷的 {folder}。{error}"))
    }
}

fn stage_merge(
    current: &Snapshot,
    target: &Target,
    keep: &PartitionRef,
    absorb: &PartitionRef,
    directory_id: uuid::Uuid,
    cancel: &AtomicBool,
) -> Result<VerifiedMerge, String> {
    let session = Session::open()?;
    let parts = current.part_refs();
    let resolve_volume = |reference| {
        let (part, volume) = current.original(reference)?;
        validate_ntfs(&session, part, volume)?;
        let volume_path =
            data_windows::volume_root(volume.volume_path.as_deref().ok_or("缺少卷标识。")?)?;
        let selection = source(target, &current.disk, &parts, Some(part.partition_number));
        let resolved = data_windows::resolve(&selection, true, false)?;
        resolved.verify_partition_snapshot(&current.disk, &parts)?;
        if resolved.volumes.as_slice() != [volume_path]
            || !resolved.ntfs_volume
            || resolved.disk.is_offline != Some(false)
            || resolved.disk.is_read_only != Some(false)
            || (resolved.offset, resolved.size) != (part.offset, part.size)
        {
            return Err("合并所选 NTFS 卷、物理范围或磁盘状态已经变化。".to_owned());
        }
        Ok((part, selection, resolved))
    };
    let (keep_part, keep_selection, keep_resolved) = resolve_volume(keep)?;
    let (_, absorb_selection, absorb_resolved) = resolve_volume(absorb)?;
    if keep_resolved.volumes == absorb_resolved.volumes
        || keep_resolved.offset == absorb_resolved.offset
    {
        return Err("合并要求两个已确认的独立卷。".into());
    }
    let lock = |selection, resolved| {
        check_cancel(cancel)?;
        let locks = data_windows::lock(&resolved)?;
        let resolved = data_windows::resolve_locked(&selection, false, &resolved, &locks)?;
        Ok::<_, String>(LockedMergeVolume {
            selection,
            resolved,
            locks,
        })
    };
    let absorb = lock(absorb_selection, absorb_resolved)?;
    let verify_layout = || {
        let mut disk = data_windows::device(&absorb.resolved, false)?;
        current.archive.matches(&mut disk, || check_cancel(cancel))
    };
    verify_layout()?;
    let mut source_reader = data_windows::reader(&absorb.resolved)?;
    let initial = merge::catalogue(&mut source_reader, None, cancel)?;
    absorb.revalidate()?;
    verify_layout()?;
    let copied = merge::output::copy(
        &keep_selection,
        &current.disk,
        keep_part,
        &keep_resolved.volumes[0],
        directory_id,
        &initial,
        &mut source_reader,
        cancel,
    )?;
    // Open new readers after the copy and target lock so reads cannot be served
    // from the older read cache. The initial source lock remains held.
    let keep = (|| {
        let keep = lock(keep_selection, keep_resolved)?;
        absorb.revalidate()?;
        verify_layout()?;
        let mut source_reader = data_windows::reader(&absorb.resolved)?;
        initial.same_source(&merge::catalogue(&mut source_reader, None, cancel)?)?;
        let mut target_reader = data_windows::reader(&keep.resolved)?;
        copied.verify(&initial, &mut target_reader, cancel)?;
        absorb.revalidate()?;
        keep.revalidate()?;
        verify_layout()?;
        Ok::<_, String>(keep)
    })()
    .map_err(|error| {
        format!(
            "目标卷目录 {} 已保留，源卷尚未删除。{error}",
            merge::output::CopiedDirectory::name(directory_id)
        )
    })?;
    Ok(VerifiedMerge {
        copied,
        absorb,
        keep,
    })
}

fn move_one(
    step: &Step,
    current: &Snapshot,
    target: &Target,
    journal: JournalLocation,
    id: &str,
    cancel: &AtomicBool,
) -> Result<table_archive::Archive, String> {
    let Step::Move {
        partition,
        new_offset,
    } = step
    else {
        return Err("缺少已确认的移动步骤。".into());
    };
    let parts = current.part_refs();
    let part = partition.resolve(&parts)?;
    let selection = source(target, &current.disk, &parts, Some(part.partition_number));
    let context = sector_context(&selection, None)?;
    let expected_volumes =
        current
            .parts
            .iter()
            .zip(&current.volumes)
            .filter_map(|(part, volume)| {
                volume
                    .as_ref()
                    .map(|volume| (part.partition_number, volume))
            });
    if context.resolved.disk != current.disk
        || context.parts != current.parts
        || !context
            .volumes
            .iter()
            .map(|(number, volume)| (*number, volume))
            .eq(expected_volumes)
    {
        return Err("移动前磁盘、分区或卷关联与上一核验状态不同。".into());
    }
    let journal_name = journal.name().to_owned();
    let archive = partition_move_task::move_for_batch(
        partition_move_task::Request {
            source: selection,
            destination: *new_offset,
        },
        &context,
        &current.archive,
        journal,
        id,
        cancel,
    )?;
    let session = Session::open()?;
    let offline_disk = policy::target_disk(&session.disks, target)?;
    let offline_parts = policy::layout(offline_disk, &session.partitions)?;
    let layout = table_restore::archive_layout(&archive)?;
    step.verify_geometry(&current.layout, &layout)?;
    check_cancel(cancel)
        .map_err(|error| format!("{error} 已完成的移动保持脱机。断点文件为 {journal_name}。"))?;
    device_state::online_after_verified_move(
        source(target, offline_disk, &offline_parts, None),
        &current.disk,
        &archive,
        || check_cancel(cancel),
    )
    .map_err(|error| format!("分区已移动并验证，联机核验未完成。请保留 {journal_name}。{error}"))?;
    Ok(archive)
}

#[cfg(test)]
#[path = "partition_reallocation_windows_tests.rs"]
mod tests;

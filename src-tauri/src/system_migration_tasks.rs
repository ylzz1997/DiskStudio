//! Execute a confirmed cold migration against verified Windows disks.
use super::{progress, validate_pair, Selection, OVERWRITE_WARNING};
use crate::{
    data_io::{self, Region},
    data_windows as device,
    native_locale::{self, Locale},
    partitions::platform::{maintenance, TableReadContext},
    surface_scan::Range,
    system_migration::{Options, PartitionSelection, PartitionSize, Plan, Preview},
    table_archive::Archive,
};
use disk_core::MIB;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_UPDATE_PROPERTIES,
};

#[path = "system_migration_resize.rs"]
mod volume_resize;

pub(super) const TITLE: &str = "离线迁移 Windows";

#[derive(PartialEq, Eq)]
struct TargetState {
    archive: Option<Archive>,
    guard: [u8; 32],
}

pub(crate) const MAX_DESTINATIONS: usize = 8;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Destination {
    pub(crate) selection: Selection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) partition_sizes: Vec<PartitionSize>,
    #[serde(default, skip_serializing_if = "PartitionSelection::is_all")]
    pub(crate) partition_selection: PartitionSelection,
}

impl Destination {
    pub(crate) fn options(&self) -> Options {
        Options {
            sizes: self.partition_sizes.clone(),
            selection: self.partition_selection.clone(),
        }
    }
}

pub(crate) fn validate_destinations<'a>(
    source: &Selection,
    destinations: impl ExactSizeIterator<Item = &'a Destination>,
) -> Result<(), String> {
    if source.partition.is_some() || !(1..=MAX_DESTINATIONS).contains(&destinations.len()) {
        return Err("请选择一个完整源盘和 1 到 8 个目标盘。".into());
    }
    let mut identities = BTreeSet::new();
    for destination in destinations {
        validate_pair(source, &destination.selection)?;
        destination.options().validate()?;
        if !identities.insert(&destination.selection.target.0) {
            return Err("系统迁移的目标必须是互不相同的物理磁盘。".into());
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundDestination {
    pub(crate) destination: Destination,
    pub(crate) layout: String,
    pub(crate) guard: [u8; 32],
    pub(crate) bytes: u64,
}

impl BoundDestination {
    pub(crate) fn capture(
        destination: Destination,
        from: &device::Resolved,
    ) -> Result<(Self, device::Resolved), String> {
        let (to, guard) = Migration::handoff_target(&destination.selection, from)?;
        let layout = device::handoff_layout_fingerprint(&destination.selection, &to, false)?;
        let bytes = to.size;
        Ok((
            Self {
                destination,
                layout,
                guard,
                bytes,
            },
            to,
        ))
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetPreview {
    destination: String,
    pub(crate) layout: Preview,
}

struct TargetPlan {
    destination: Destination,
    fingerprint: String,
    number: u32,
    before: TargetState,
    size: u64,
    label: String,
    layout: String,
    plan: Plan,
}

pub(crate) struct Migration {
    source: Selection,
    source_before: device::Resolved,
    winpe_source: bool,
    sector: u64,
    targets: Vec<TargetPlan>,
}

fn resolve_source(selection: &Selection, winpe_source: bool) -> Result<device::Resolved, String> {
    let source = device::resolve(selection, true, false)?;
    if !matches!(source.disk.partition_style, Some(1 | 2))
        || (!winpe_source && source.disk.is_offline != Some(true))
        || source.disk.is_read_only.is_none()
    {
        return Err("请选择处于脱机状态的完整 Windows 系统源盘。".into());
    }
    Ok(source)
}

fn read_plan(
    selection: &Selection,
    source: &device::Resolved,
    file: &File,
    size: u64,
    sector: u64,
    options: &Options,
) -> Result<Plan, String> {
    device::verify_read_state(source, file)?;
    let mut reader = Region::for_file_view(file, 0, source.size).map_err(device::err)?;
    let mut ranges = source.partition_ranges.clone();
    ranges.sort_by_key(|range| range.start);
    let plan = Plan::prepare_layout(
        &mut reader,
        source.size,
        size,
        sector,
        Some(&ranges),
        options,
        |layout| source.migration_partition_numbers(selection, layout),
    )?;
    if let Some((gpt, boot)) = plan.gpt_identity()? {
        source.verify_boot_loader_layout(file, &gpt, boot)?;
    } else {
        verify_mbr_signature(file, plan.mbr_signature().ok_or("缺少来源 MBR 标识。")?)?;
    }
    Ok(plan)
}

fn verify_mbr_signature(file: &File, signature: [u8; 4]) -> Result<(), String> {
    let mut bytes = [0; 65536];
    let n = device::control(file, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut bytes)?;
    if n < 48 || bytes[..4] != [0; 4] || signature == [0; 4] || bytes[8..12] != signature {
        return Err("Windows 原生 MBR 标识与原始分区表不同。".into());
    }
    Ok(())
}

fn empty_target(
    context: &TableReadContext,
    file: &mut File,
    selection: &Selection,
    sector: u64,
) -> Result<TargetState, String> {
    let size = context.resolved.size;
    if !context.parts.is_empty()
        || !(4 * MIB..=1 << 43).contains(&size)
        || !matches!(sector, 512 | 4096)
        || size % sector != 0
    {
        return Err("系统迁移需要无分区的可写 RAW 或 GPT 目标磁盘。".into());
    }
    let archive = maintenance::capture_write_table(
        context,
        file,
        &selection.target,
        Range {
            offset: 0,
            size,
            sector,
        },
        false,
    )?;
    if let Some(table) = &archive {
        if !crate::table_restore::archive_layout(table)?
            .entries
            .is_empty()
        {
            return Err("系统迁移需要无分区的可写 RAW 或 GPT 目标磁盘。".into());
        }
    }
    let mut block = vec![0; MIB as usize];
    let mut hash = Sha256::new();
    for offset in [0, size - MIB] {
        let mut view = Region::for_metadata(&*file, offset, MIB).map_err(device::err)?;
        view.read_exact(&mut block).map_err(device::err)?;
        hash.update(&block);
    }
    Ok(TargetState {
        archive,
        guard: hash.finalize().into(),
    })
}

impl Migration {
    pub(crate) fn handoff_target(
        destination: &Selection,
        source: &device::Resolved,
    ) -> Result<(device::Resolved, [u8; 32]), String> {
        let (target, state) = capture_target(destination, source)?;
        Ok((target.resolved, state.guard))
    }

    pub(crate) fn resume(
        id: &str,
        cancel: &AtomicBool,
        source: Selection,
        source_layout: &str,
        targets: Vec<BoundDestination>,
    ) -> Result<(String, Option<String>), String> {
        let destinations = targets
            .iter()
            .map(|target| target.destination.clone())
            .collect();
        let locale = native_locale::current();
        let (job, _, _, _) = Self::prepare_source(source, destinations, &locale, true, cancel)?;
        if device::handoff_layout_fingerprint(&job.source, &job.source_before, false)?
            != source_layout
        {
            return Err(locale
                .text("Windows 迁移来源或目标与重启前确认时不同。")
                .into());
        }
        for (actual, expected) in job.targets.iter().zip(targets) {
            if actual.size != expected.bytes
                || actual.layout != expected.layout
                || actual.before.guard != expected.guard
            {
                return Err(on_target(
                    &locale,
                    &actual.label,
                    "Windows 迁移目标布局已变化。",
                ));
            }
        }
        job.execute(id, cancel)
    }

    pub(super) fn prepare(
        source: Selection,
        targets: Vec<Destination>,
        locale: &Locale,
    ) -> Result<(Self, String, String, String), String> {
        Self::prepare_source(source, targets, locale, false, &AtomicBool::new(false))
    }

    fn prepare_source(
        source: Selection,
        targets: Vec<Destination>,
        locale: &Locale,
        winpe_source: bool,
        cancel: &AtomicBool,
    ) -> Result<(Self, String, String, String), String> {
        validate_destinations(&source, targets.iter())?;
        let source_before = resolve_source(&source, winpe_source)?;
        let sector = source_before
            .disk
            .logical_sector_size
            .ok_or("源盘逻辑扇区大小未知。")?;
        let locks = device::state_locks(&source_before)?;
        let source_before = device::resolve_locked(&source, false, &source_before, &locks)?;
        let source_file = device::exclusive_reader(&source_before)?;
        let mut numbers = BTreeSet::from([source_before.disk.number]);
        let mut plans = Vec::with_capacity(targets.len());
        for destination in targets {
            let (target, before) = capture_target(&destination.selection, &source_before)?;
            if !numbers.insert(target.resolved.disk.number) {
                return Err("系统迁移的目标必须是互不相同的物理磁盘。".into());
            }
            let label = target.resolved.label.clone();
            let size = target.resolved.size;
            let plan = read_plan(
                &source,
                &source_before,
                &source_file,
                size,
                sector,
                &destination.options(),
            )
            .map_err(|error| on_target(locale, &label, &error))?;
            plans.push(TargetPlan {
                layout: device::handoff_layout_fingerprint(
                    &destination.selection,
                    &target.resolved,
                    false,
                )?,
                destination,
                fingerprint: target.fingerprint,
                number: target.resolved.disk.number,
                before,
                size,
                label,
                plan,
            });
        }
        trial_resizes(&plans, &source_file, sector, cancel, locale)?;
        let source_label = source_before.label.clone();
        let target_labels = plans
            .iter()
            .map(|target| target.label.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        let warning = locale.sentences(&[
            OVERWRITE_WARNING,
            if plans.len() == 1 {
                "保留原磁盘和分区标识以维持 Windows 启动引用。首次启动前请断开源盘。"
            } else {
                "各副本保留源盘的磁盘和分区标识。启动任一副本前，请断开源盘和其余副本。"
            },
            if plans.iter().any(|target| target.plan.adds_efi_fallback()) {
                "在副本的 EFI 系统分区补齐 Windows x64 回退入口。"
            } else {
                ""
            },
            if plans
                .iter()
                .any(|target| target.plan.volume_resizes().next().is_some())
            {
                "首次启动时，Windows 会检查调整过容量的 NTFS 卷。"
            } else {
                ""
            },
        ]);
        Ok((
            Self {
                source,
                source_before,
                winpe_source,
                sector,
                targets: plans,
            },
            source_label,
            target_labels,
            warning,
        ))
    }

    pub(super) fn size(&self) -> u64 {
        self.targets.iter().map(|target| target.size).sum()
    }

    pub(super) fn target_count(&self) -> usize {
        self.targets.len()
    }

    pub(super) fn preview(&self) -> Vec<TargetPreview> {
        self.targets
            .iter()
            .map(|target| TargetPreview {
                destination: target.label.clone(),
                layout: target.plan.preview(),
            })
            .collect()
    }

    pub(super) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let total = self.size();
        let tick = |phase: &str, done| progress(id, cancel, phase, done, total);
        let locale = native_locale::current();
        tick("核对 Windows 迁移来源", 0)?;
        let source = resolve_source(&self.source, self.winpe_source)?;
        if !source.same_data_binding(&self.source_before) {
            return Err("源设备、布局或卷关联与确认时不同。".into());
        }
        let source_locks = device::state_locks(&source)?;
        let source = device::resolve_locked(&self.source, false, &source, &source_locks)?;
        let source_file = device::exclusive_reader(&source)?;
        for target in &self.targets {
            if read_plan(
                &self.source,
                &source,
                &source_file,
                target.size,
                self.sector,
                &target.destination.options(),
            )
            .map_err(|error| on_target(&locale, &target.label, &error))?
                != target.plan
            {
                return Err(on_target(
                    &locale,
                    &target.label,
                    "Windows 启动链或系统迁移布局与预览不同。",
                ));
            }
        }
        let engine = trial_resizes(&self.targets, &source_file, self.sector, cancel, &locale)?;
        let verify_source = || -> Result<(), String> {
            device::verify_read_state(&source, &source_file)?;
            device::resolve_locked(&self.source, false, &source, &source_locks)?;
            Ok(())
        };
        let mut opened = Vec::with_capacity(self.targets.len());
        for target in &self.targets {
            tick("锁定目标", 0)?;
            opened.push(
                target
                    .open(source.disk.number, self.sector)
                    .map_err(|error| on_target(&locale, &target.label, &error))?,
            );
        }
        verify_source()?;
        let mut status: Vec<_> = self.targets.iter().map(|_| Outcome::Unchanged).collect();
        let result = (|| {
            for (index, (target, opened)) in self.targets.iter().zip(&mut opened).enumerate() {
                tick("锁定目标", 0)?;
                status[index] = Outcome::OfflineUncertain;
                device::persist_offline(&opened.file)
                    .and_then(|()| device::confirm_writable_offline(&opened.file))
                    .map_err(|error| on_target(&locale, &target.label, &error))?;
                status[index] = Outcome::Offline;
            }
            // Confirm the entire set before the first write. Keep every exclusive
            // handle and lock alive across copying and verification.
            for (target, opened) in self.targets.iter().zip(&mut opened) {
                device::confirm_writable_offline(&opened.file)
                    .and_then(|()| {
                        target.verify_blank(&opened.context, &mut opened.file, self.sector)
                    })
                    .map_err(|error| on_target(&locale, &target.label, &error))?;
            }
            verify_source()?;
            let open_source = || Region::new(&source_file, 0, source.size).map_err(device::err);
            let digests = {
                let mut streams = Vec::with_capacity(opened.len());
                for (target, opened) in self.targets.iter().zip(&mut opened) {
                    opened.file.seek(SeekFrom::Start(0)).map_err(device::err)?;
                    streams.push((
                        target.plan.reader(open_source()?).map_err(device::err)?,
                        &mut opened.file,
                        target.size,
                    ));
                }
                data_io::copy_rounds(&mut streams, |index, done, writing| {
                    tick("写入目标", done)?;
                    if writing {
                        status[index] = Outcome::Writing;
                    }
                    Ok(())
                })
                .map_err(|(index, error)| on_target(&locale, &self.targets[index].label, &error))?
            };
            for (target, opened) in self.targets.iter().zip(&opened) {
                opened
                    .file
                    .sync_all()
                    .map_err(|error| on_target(&locale, &target.label, &device::err(error)))?;
            }
            let mut verified = 0;
            for (index, (target, opened)) in self.targets.iter().zip(&mut opened).enumerate() {
                let target_tick = |phase: &str, done| {
                    let label = format!("{} · {}", locale.text(phase), target.label);
                    tick(&label, verified + done)
                };
                let digest = target
                    .verify_copy(
                        &mut opened.file,
                        &source_file,
                        open_source,
                        self.sector,
                        &digests[index],
                        engine.as_ref(),
                        cancel,
                        target_tick,
                    )
                    .map_err(|error| on_target(&locale, &target.label, &error))?;
                status[index] = Outcome::Verified(digest);
                verified += target.size;
            }
            verify_source()?;
            tick("核对迁移后的 Windows 启动链", total)?;
            Ok::<(), String>(())
        })();
        let report = self
            .targets
            .iter()
            .zip(&status)
            .map(|(target, outcome)| match outcome {
                Outcome::Unchanged => on_target(&locale, &target.label, "未写入"),
                Outcome::OfflineUncertain => {
                    on_target(&locale, &target.label, "未写入，脱机状态待核对")
                }
                Outcome::Offline => on_target(&locale, &target.label, "已脱机，未写入"),
                Outcome::Writing => on_target(
                    &locale,
                    &target.label,
                    "已开始写入，尚未完成校验，请勿用作副本",
                ),
                Outcome::Verified(hash) => format!("{} · SHA-256 {hash}", target.label),
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) = result {
            return Err(format!("{}\n{report}", locale.text(&error)));
        }
        if let [Outcome::Verified(hash)] = status.as_slice() {
            return Ok((locale.text("Windows 迁移完成。目标全盘通过 SHA-256 回读，启动链已核对，目标盘仍脱机。首次启动前请断开源盘。").into(), Some(hash.clone())));
        }
        let message = locale.text("Windows 迁移完成。各目标盘均通过全盘回读和启动链校验，保持脱机。启动任一副本前，请断开源盘和其余副本。");
        Ok((format!("{message}\n{report}"), None))
    }
}

fn on_target(locale: &Locale, label: &str, error: &str) -> String {
    format!("{label} · {}", locale.text(error))
}

fn capture_target(
    destination: &Selection,
    source: &device::Resolved,
) -> Result<(TableReadContext, TargetState), String> {
    let target = maintenance::write_context(destination, None)?;
    let sector = source
        .disk
        .logical_sector_size
        .ok_or("源盘逻辑扇区大小未知。")?;
    if target.resolved.disk.number == source.disk.number
        || target.resolved.disk.logical_sector_size != Some(sector)
        || (source.disk.partition_style == Some(1)
            && (sector != 512 || target.resolved.size < source.size))
    {
        return Err("系统迁移需要不同的物理磁盘及兼容的容量与逻辑扇区大小。".into());
    }
    let mut file = device::device(&target.resolved, false)?;
    let state = empty_target(&target, &mut file, destination, sector)?;
    Ok((target, state))
}

fn trial_resizes(
    plans: &[TargetPlan],
    source: &File,
    sector: u64,
    cancel: &AtomicBool,
    locale: &Locale,
) -> Result<Option<crate::extfs::Engine>, String> {
    if plans
        .iter()
        .all(|target| target.plan.volume_resizes().next().is_none())
    {
        return Ok(None);
    }
    let engine = crate::ntfs_file_clone::Plan::engine()?;
    for target in plans {
        for change in target.plan.volume_resizes() {
            volume_resize::resize(&engine, source, None, change, sector, cancel)
                .map_err(|error| on_target(locale, &target.label, &error))?;
        }
    }
    Ok(Some(engine))
}

struct OpenedTarget {
    file: File,
    context: TableReadContext,
    _locks: device::Locks,
}

enum Outcome {
    Unchanged,
    OfflineUncertain,
    Offline,
    Writing,
    Verified(String),
}

impl TargetPlan {
    fn open(&self, source_number: u32, sector: u64) -> Result<OpenedTarget, String> {
        let target = maintenance::write_context(&self.destination.selection, None)?;
        if target.fingerprint != self.fingerprint
            || target.resolved.disk.number != self.number
            || target.resolved.disk.number == source_number
        {
            return Err("目标盘身份、布局或保护与预览不同。".into());
        }
        let locks = device::lock(&target.resolved)?;
        let context =
            maintenance::write_context(&self.destination.selection, Some((&target, &locks)))?;
        let mut file = device::exclusive_device(&context.resolved)?;
        self.verify_blank(&context, &mut file, sector)?;
        Ok(OpenedTarget {
            file,
            context,
            _locks: locks,
        })
    }

    fn verify_blank(
        &self,
        target: &TableReadContext,
        file: &mut File,
        sector: u64,
    ) -> Result<(), String> {
        if empty_target(target, file, &self.destination.selection, sector)? != self.before {
            return Err("预览后目标内容已变化。".into());
        }
        Ok(())
    }

    fn verify_copy<'a>(
        &self,
        output: &mut File,
        source: &File,
        open: impl Fn() -> Result<Region<&'a File>, String>,
        sector: u64,
        digest: &str,
        engine: Option<&crate::extfs::Engine>,
        cancel: &AtomicBool,
        tick: impl Fn(&str, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let source_check = data_io::hash_exact(
            &mut self.plan.reader(open()?).map_err(device::err)?,
            self.size,
            |done| tick("重新读取 Windows 迁移来源", done),
        )?;
        output.seek(SeekFrom::Start(0)).map_err(device::err)?;
        let readback = data_io::hash_exact(output, self.size, |done| tick("校验目标", done))?;
        if source_check != digest || readback != digest {
            return Err("迁移来源的两次读取和目标 SHA-256 回读结果不一致。".into());
        }
        let final_digest =
            if let Some(engine) = engine.filter(|_| self.plan.volume_resizes().next().is_some()) {
                let mut writes = BTreeMap::new();
                for change in self.plan.volume_resizes() {
                    tick("正在调整迁移目标 NTFS 容量", change.new_offset)?;
                    writes.extend(volume_resize::resize(
                        engine,
                        source,
                        Some((output, self.size)),
                        change,
                        sector,
                        cancel,
                    )?);
                }
                volume_resize::verify_copy(
                    self.plan.reader(open()?).map_err(device::err)?,
                    output,
                    self.size,
                    writes,
                    |done| tick("校验目标", done),
                )?
            } else {
                digest.into()
            };
        tick("核对迁移后的 Windows 启动链", self.size)?;
        device::control(output, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])?;
        self.plan.verify_output(
            &mut Region::for_file_view(&*output, 0, self.size).map_err(device::err)?,
        )?;
        if let Some(signature) = self.plan.mbr_signature() {
            verify_mbr_signature(output, signature)?;
        } else {
            self.plan
                .verify_native_gpt(&crate::extfs::restore_native(output, None, sector)?.0)?;
        }
        device::confirm_writable_offline(output)?;
        Ok(final_digest)
    }
}

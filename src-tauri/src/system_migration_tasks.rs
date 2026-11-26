//! Execute confirmed Windows migration plans against verified destination disks.
use super::{progress, validate_pair, Selection, OVERWRITE_WARNING};
use crate::{
    backup_archive::Source,
    boot_inspection::virtualization::SystemHive,
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

#[path = "system_migration_hot.rs"]
mod hot;
#[path = "system_migration_resize.rs"]
mod volume_resize;
pub(super) use hot::{HotBackup, HotMigration, HotPreview, HOT_TITLE};

pub(super) const TITLE: &str = "离线迁移 Windows";

#[derive(PartialEq, Eq)]
struct TargetState {
    archive: Option<Archive>,
    guard: [u8; 32],
    replaced: Vec<ReplacedPartition>,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplacedPartition {
    number: u32,
    offset: u64,
    bytes: u64,
}

pub(crate) struct InspectedTarget {
    pub(crate) resolved: device::Resolved,
    pub(crate) guard: [u8; 32],
    pub(crate) replaced: Vec<ReplacedPartition>,
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
    ) -> Result<(Self, InspectedTarget), String> {
        let to = Migration::handoff_target(&destination.selection, from)?;
        let layout =
            device::handoff_layout_fingerprint(&destination.selection, &to.resolved, false)?;
        let bytes = to.resolved.size;
        Ok((
            Self {
                destination,
                layout,
                guard: to.guard,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replaced: Vec<ReplacedPartition>,
}

struct TargetPlan<P = Plan> {
    destination: Destination,
    fingerprint: String,
    number: u32,
    before: TargetState,
    size: u64,
    label: String,
    layout: String,
    plan: P,
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
    mut reader: impl Source,
    target_size: u64,
    options: &Options,
    system_hive: SystemHive,
) -> Result<Plan, String> {
    let sector = source.disk.logical_sector_size.ok_or("源盘逻辑扇区大小未知。")?;
    let mut ranges = source.partition_ranges.clone();
    ranges.sort_by_key(|range| range.start);
    let plan = Plan::prepare_layout_with_hive(
        &mut reader,
        source.size,
        target_size,
        sector,
        Some(&ranges),
        options,
        system_hive,
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

fn inspect_target(
    context: &TableReadContext,
    file: &mut File,
    selection: &Selection,
    sector: u64,
) -> Result<TargetState, String> {
    let size = context.resolved.size;
    if !(4 * MIB..=1 << 43).contains(&size)
        || !matches!(sector, 512 | 4096)
        || size % sector != 0
    {
        return Err("系统迁移目标的容量或扇区大小不受支持。".into());
    }
    let range = Range { offset: 0, size, sector };
    maintenance::protect_partitions(context, range)?;
    if context.resolved.disk.is_offline == Some(false)
        && context.parts.iter().any(|part| !context.has_volume(part.partition_number))
    {
        return Err("系统迁移无法核对目标数据分区的卷关联。".into());
    }
    let archive = maintenance::capture_write_table(
        context,
        file,
        &selection.target,
        range,
        false,
    )?;
    let replaced = context
        .parts
        .iter()
        .map(|part| ReplacedPartition {
            number: part.partition_number,
            offset: part.offset,
            bytes: part.size,
        })
        .collect::<Vec<_>>();
    let occupied_table = if replaced.is_empty() {
        None
    } else {
        let archive = archive.as_ref().ok_or("系统迁移缺少目标 GPT 分区表。")?;
        let parts = context.parts.iter().collect::<Vec<_>>();
        let (native, service, _) = crate::extfs::restore_native(file, Some(&parts), sector)?;
        if !service.is_empty() || !crate::table_restore::archive_layout(archive)?.agrees(&native) {
            return Err("系统迁移目标含有原生服务分区或分区表已变化。".into());
        }
        Some(archive)
    };
    let guard = target_guard(&*file, size, occupied_table)?;
    Ok(TargetState { archive, guard, replaced })
}

fn target_guard(
    mut file: impl Read + Seek,
    size: u64,
    occupied_table: Option<&Archive>,
) -> Result<[u8; 32], String> {
    let mut block = vec![0; MIB as usize];
    let mut hash = Sha256::new();
    for offset in [0, size - MIB] {
        let mut view = Region::for_metadata(&mut file, offset, MIB).map_err(device::err)?;
        view.read_exact(&mut block).map_err(device::err)?;
        hash.update(&block);
    }
    // Older handoff records cover empty disks with the edge hash alone.
    if let Some(table) = occupied_table {
        hash.update(b"DiskStudio occupied migration destination GPT\0");
        for extent in &table.extents {
            hash.update(extent.offset.to_le_bytes());
            hash.update((extent.bytes.len() as u64).to_le_bytes());
            hash.update(&extent.bytes);
        }
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
#[test]
fn destination_guard_binds_occupied_table_outside_disk_edges() {
    use std::io::Cursor;

    let image = vec![0u8; 4 * MIB as usize];
    let empty = target_guard(Cursor::new(&image), image.len() as u64, None).unwrap();
    assert_eq!(
        empty.as_slice(),
        Sha256::digest(&image[..2 * MIB as usize]).as_slice(),
    );
    let mut table = Archive {
        version: 2,
        size: image.len() as u64,
        sector: 4096,
        source: "0".repeat(64),
        includes_boot: false,
        extents: vec![crate::table_archive::Extent {
            kind: crate::table_archive::Kind::PrimaryEntries,
            offset: 2 * MIB,
            bytes: vec![0; 4096],
        }],
    };
    let occupied = target_guard(Cursor::new(&image), table.size, Some(&table)).unwrap();
    assert_ne!(empty, occupied);
    table.extents[0].bytes[0] = 1;
    assert_ne!(
        occupied,
        target_guard(Cursor::new(&image), table.size, Some(&table)).unwrap(),
    );
}

impl Migration {
    pub(crate) fn handoff_target(
        destination: &Selection,
        source: &device::Resolved,
    ) -> Result<InspectedTarget, String> {
        let (target, state) = capture_target(destination, source)?;
        Ok(InspectedTarget {
            resolved: target.resolved,
            guard: state.guard,
            replaced: state.replaced,
        })
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
        let targets = bind_targets(targets, &source_before)?;
        let open_source =
            || Region::for_file_view(&source_file, 0, source_before.size).map_err(device::err);
        let mut plans = Vec::with_capacity(targets.len());
        device::verify_read_state(&source_before, &source_file)?;
        for target in targets {
            let plan = read_plan(
                &source,
                &source_before,
                &source_file,
                open_source()?,
                target.size,
                &target.destination.options(),
                SystemHive::Clean,
            )
            .map_err(|error| on_target(locale, &target.label, &error))?;
            plans.push(target.with_plan(plan));
        }
        trial_resizes(
            &plans,
            open_source,
            source_before.size,
            sector,
            cancel,
            locale,
        )?;
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

    pub(super) fn preview(&self) -> Vec<TargetPreview> {
        self.targets
            .iter()
            .map(|target| TargetPreview {
                destination: target.label.clone(),
                layout: target.plan.preview(),
                replaced: target.before.replaced.clone(),
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
        let open_source = || Region::new(&source_file, 0, source.size).map_err(device::err);
        let verify_source = || -> Result<(), String> {
            device::verify_read_state(&source, &source_file)?;
            device::resolve_locked(&self.source, false, &source, &source_locks)?;
            Ok(())
        };
        verify_source()?;
        for target in &self.targets {
            let reader =
                Region::for_file_view(&source_file, 0, source.size).map_err(device::err)?;
            if read_plan(
                &self.source,
                &source,
                &source_file,
                reader,
                target.size,
                &target.destination.options(),
                SystemHive::Clean,
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
        execute_targets(
            &self.targets,
            source.disk.number,
            source.size,
            self.sector,
            open_source,
            verify_source,
            cancel,
            &tick,
            &locale,
        )
    }
}

fn execute_targets<R: Source>(
    targets: &[TargetPlan],
    source_number: u32,
    source_bytes: u64,
    sector: u64,
    open_source: impl Fn() -> Result<R, String>,
    verify_source: impl Fn() -> Result<(), String>,
    cancel: &AtomicBool,
    tick: &impl Fn(&str, u64) -> Result<(), String>,
    locale: &Locale,
) -> Result<(String, Option<String>), String> {
    let total = targets.iter().map(|target| target.size).sum();
    let engine = trial_resizes(targets, &open_source, source_bytes, sector, cancel, locale)?;
    let mut opened = Vec::with_capacity(targets.len());
    for target in targets {
        tick("锁定目标", 0)?;
        opened.push(
            target
                .open(source_number, sector)
                .map_err(|error| on_target(&locale, &target.label, &error))?,
        );
    }
    verify_source()?;
    let mut status: Vec<_> = targets.iter().map(|_| Outcome::Unchanged).collect();
    let result = (|| {
        for (index, (target, opened)) in targets.iter().zip(&mut opened).enumerate() {
            tick("锁定目标", 0)?;
            status[index] = Outcome::OfflineUncertain;
            device::persist_offline(&opened.file)
                .and_then(|()| device::confirm_writable_offline(&opened.file))
                .map_err(|error| on_target(&locale, &target.label, &error))?;
            status[index] = Outcome::Offline;
        }
        // Confirm the entire set before the first write. Keep every exclusive
        // handle and lock alive across copying and verification.
        for (target, opened) in targets.iter().zip(&mut opened) {
            device::confirm_writable_offline(&opened.file)
                .and_then(|()| target.verify_before(&opened.context, &mut opened.file, sector))
                .map_err(|error| on_target(&locale, &target.label, &error))?;
        }
        verify_source()?;
        let digests = {
            let mut streams = Vec::with_capacity(opened.len());
            for (target, opened) in targets.iter().zip(&mut opened) {
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
            .map_err(|(index, error)| on_target(locale, &targets[index].label, &error))?
        };
        for (target, opened) in targets.iter().zip(&opened) {
            opened
                .file
                .sync_all()
                .map_err(|error| on_target(&locale, &target.label, &device::err(error)))?;
        }
        let mut verified = 0;
        for (index, (target, opened)) in targets.iter().zip(&mut opened).enumerate() {
            let target_tick = |phase: &str, done| {
                let label = format!("{} · {}", locale.text(phase), target.label);
                tick(&label, verified + done)
            };
            let digest = target
                .verify_copy(
                    &mut opened.file,
                    &open_source,
                    source_bytes,
                    sector,
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
    let report = targets
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

fn on_target(locale: &Locale, label: &str, error: &str) -> String {
    format!("{label} · {}", locale.text(error))
}

fn bind_targets(
    destinations: Vec<Destination>,
    source: &device::Resolved,
) -> Result<Vec<TargetPlan<()>>, String> {
    let mut numbers = BTreeSet::from([source.disk.number]);
    let mut plans = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let (target, before) = capture_target(&destination.selection, source)?;
        if !numbers.insert(target.resolved.disk.number) {
            return Err("系统迁移的目标必须是互不相同的物理磁盘。".into());
        }
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
            size: target.resolved.size,
            label: target.resolved.label,
            plan: (),
        });
    }
    Ok(plans)
}

impl TargetPlan<()> {
    fn with_plan(self, plan: Plan) -> TargetPlan {
        TargetPlan {
            destination: self.destination,
            fingerprint: self.fingerprint,
            number: self.number,
            before: self.before,
            size: self.size,
            label: self.label,
            layout: self.layout,
            plan,
        }
    }
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
    let state = inspect_target(&target, &mut file, destination, sector)?;
    Ok((target, state))
}

fn trial_resizes<R: Source>(
    plans: &[TargetPlan],
    open_source: impl Fn() -> Result<R, String>,
    source_bytes: u64,
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
            volume_resize::resize(
                &engine,
                open_source()?,
                source_bytes,
                None,
                change,
                sector,
                cancel,
            )
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

impl<P> TargetPlan<P> {
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
        self.verify_before(&context, &mut file, sector)?;
        Ok(OpenedTarget {
            file,
            context,
            _locks: locks,
        })
    }

    fn verify_before(
        &self,
        target: &TableReadContext,
        file: &mut File,
        sector: u64,
    ) -> Result<(), String> {
        if inspect_target(target, file, &self.destination.selection, sector)? != self.before {
            return Err("预览后目标内容已变化。".into());
        }
        Ok(())
    }
}

impl TargetPlan {
    fn verify_copy<R: Source>(
        &self,
        output: &mut File,
        open: impl Fn() -> Result<R, String>,
        source_bytes: u64,
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
        if readback != digest {
            return Err("迁移目标的全盘 SHA-256 与写入数据不一致。".into());
        }
        if source_check != digest {
            return Err("同一 Windows 迁移来源的两次 SHA-256 不一致。".into());
        }
        let final_digest =
            if let Some(engine) = engine.filter(|_| self.plan.volume_resizes().next().is_some()) {
                let mut writes = BTreeMap::new();
                for change in self.plan.volume_resizes() {
                    tick("正在调整迁移目标 NTFS 容量", change.new_offset)?;
                    writes.extend(volume_resize::resize(
                        engine,
                        open()?,
                        source_bytes,
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

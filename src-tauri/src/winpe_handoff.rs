//! Portable, typed authorization for one offline data task. No drive letters or
//! reusable operation tokens survive a reboot.
use crate::{
    backup_archive::{Chain, Compression, MAX_CHAIN},
    backup_files::{PortableInput, PortableOutput},
    data_tasks::{
        self, system_migration_tasks::{validate_destinations, BoundDestination, Destination, Migration, ReplacedPartition},
        BackupHeader, BackupOptions, HandoffBinding,
        Selection, Work,
    },
    data_windows as device,
    partitions::Target,
    system_migration::{validate_sizes, Options as MigrationOptions, PartitionSelection, PartitionSize},
    used_space::Mode,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const BACKUP_TITLE: &str = "在 WinPE 执行完整扇区备份";
const RESTORE_TITLE: &str = "在 WinPE 从备份还原";
const CLONE_TITLE: &str = "在 WinPE 克隆";
const MIGRATE_TITLE: &str = "在 WinPE 迁移 Windows";
pub(super) const MAX_RECORD: usize = 4096;
const MAX_PAYLOAD: usize = 24 * 1024;
const RECORD_MAGIC: &[u8] = b"DSWT2";
const LIFETIME: u64 = 2 * 60 * 60;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Request {
    Backup { source: Selection },
    Restore { destination: Selection },
    Clone { source: Selection, destination: Selection, mode: Mode },
    Migrate {
        source: Selection,
        destination: Selection,
        #[serde(default, rename = "partitionSizes")]
        partition_sizes: Vec<PartitionSize>,
        #[serde(default, rename = "partitionSelection")]
        partition_selection: PartitionSelection,
    },
    MigrateBatch { source: Selection, destinations: Vec<Destination> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub(crate) source: String,
    pub(crate) destination: String,
    pub(crate) bytes: u64,
    pub(crate) destructive: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    partition_sizes: Vec<PartitionSize>,
    #[serde(skip_serializing_if = "PartitionSelection::is_all")]
    partition_selection: PartitionSelection,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    migration_destinations: Vec<MigrationDestinationPreview>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationDestinationPreview {
    destination: String,
    bytes: u64,
    partition_sizes: Vec<PartitionSize>,
    partition_selection: PartitionSelection,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replaced: Vec<ReplacedPartition>,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "details",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum Task {
    Backup(Backup),
    Restore(Restore),
    Clone(CloneJob),
    Migrate(MigrationJob),
    MigrateBatch(MigrationBatch),
}

impl Task {
    pub(crate) fn revalidate(&self) -> Result<(), String> {
        match self {
            Self::Backup(backup) => backup.revalidate(false),
            Self::Restore(restore) => restore.revalidate(),
            Self::Clone(copy) => copy.revalidate(),
            Self::Migrate(migration) => migration.revalidate(),
            Self::MigrateBatch(migration) => migration.revalidate(),
        }
    }

    pub(crate) fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        if cancel.load(Ordering::Relaxed) {
            return Err("任务已取消。".into());
        }
        match self {
            Self::Backup(backup) => backup.run(cancel),
            Self::Restore(restore) => restore.run(cancel),
            Self::Clone(copy) => copy.run(cancel),
            Self::Migrate(migration) => migration.run(cancel),
            Self::MigrateBatch(migration) => migration.run(cancel),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskPair {
    source: Selection,
    destination: Selection,
    source_layout: String,
    destination_layout: String,
    bytes: u64,
    sector: u64,
}

fn copy_source(source: &Selection, destination: &Selection, boot: &Target) -> Result<device::Resolved, String> {
    data_tasks::validate_pair(source, destination)?;
    if &destination.target == boot {
        return Err("WinPE 启动介质与写入目标必须位于不同磁盘。".into());
    }
    device::resolve(source, false, false)
}

impl DiskPair {
    fn prepare(
        source: Selection,
        destination: Selection,
        from: device::Resolved,
        to: device::Resolved,
        cloning: bool,
    ) -> Result<(Self, Preview), String> {
        let sector = from.disk.logical_sector_size.ok_or("源盘逻辑扇区大小未知。")?;
        if from.disk.number == to.disk.number
            || to.disk.logical_sector_size != Some(sector)
            || (cloning && from.size != to.size)
        {
            return Err("来源与目标必须是不同的磁盘，且所选范围的几何必须兼容。".into());
        }
        let source_layout = device::handoff_layout_fingerprint(&source, &from, false)?;
        let destination_layout = device::handoff_layout_fingerprint(&destination, &to, cloning)?;
        let preview = Preview {
            source: from.label,
            destination: to.label,
            bytes: to.size,
            destructive: true,
            partition_sizes: Vec::new(),
            partition_selection: PartitionSelection::All,
            migration_destinations: Vec::new(),
        };
        Ok((Self { source, destination, source_layout, destination_layout, bytes: to.size, sector }, preview))
    }

    fn verify(&self, from: &device::Resolved, to: &device::Resolved, cloning: bool) -> Result<(), String> {
        if from.disk.number == to.disk.number
            || from.disk.logical_sector_size != Some(self.sector)
            || to.disk.logical_sector_size != Some(self.sector)
            || to.size != self.bytes
            || (cloning && from.size != self.bytes)
            || device::handoff_layout_fingerprint(&self.source, from, false)? != self.source_layout
            || device::handoff_layout_fingerprint(&self.destination, to, cloning)? != self.destination_layout
        {
            return Err("来源或目标与重启前确认时不同。".into());
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloneJob {
    pair: DiskPair,
    mode: Mode,
}

impl CloneJob {
    pub(crate) fn prepare(source: Selection, destination: Selection, mode: Mode, boot: &Target) -> Result<(Self, Preview), String> {
        let from = copy_source(&source, &destination, boot)?;
        let to = device::resolve(&destination, false, false)?;
        let (pair, preview) = DiskPair::prepare(source, destination, from, to, true)?;
        Ok((Self { pair, mode }, preview))
    }

    fn revalidate(&self) -> Result<(), String> {
        let from = device::resolve(&self.pair.source, false, false)?;
        let to = device::resolve(&self.pair.destination, false, false)?;
        self.pair.verify(&from, &to, true)
    }

    fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let DiskPair { source, destination, source_layout, destination_layout, .. } = self.pair;
        let id = data_tasks::start_with_cancel(CLONE_TITLE, Arc::clone(&cancel))?;
        finish(&id, data_tasks::run_with_handoff(
            &id, &cancel, Work::Clone { source, destination, mode: self.mode },
            Some(HandoffBinding::CloneLayouts { source: source_layout, destination: destination_layout }),
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MigrationJob {
    pair: DiskPair,
    target_guard: [u8; 32],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    partition_sizes: Vec<PartitionSize>,
    #[serde(default, skip_serializing_if = "PartitionSelection::is_all")]
    partition_selection: PartitionSelection,
}

impl MigrationJob {
    pub(crate) fn prepare(source: Selection, destination: Selection, boot: &Target, options: MigrationOptions) -> Result<(Self, Preview), String> {
        if source.partition.is_some() {
            return Err("系统迁移需要选择两块整盘。".into());
        }
        let from = copy_source(&source, &destination, boot)?;
        if !matches!(from.disk.partition_style, Some(1 | 2)) {
            return Err("请选择完整的 GPT 或 MBR Windows 系统源盘。".into());
        }
        options.validate()?;
        if (!options.sizes.is_empty() || !options.selection.is_all()) && from.disk.partition_style != Some(2) {
            return Err("选择迁移分区与调整 NTFS 布局需要 GPT/UEFI。".into());
        }
        let to = Migration::handoff_target(&destination, &from)?;
        let (pair, mut preview) = DiskPair::prepare(source, destination, from, to.resolved, false)?;
        preview.partition_sizes = options.sizes.clone();
        preview.partition_selection = options.selection.clone();
        if !to.replaced.is_empty() {
            preview.migration_destinations.push(MigrationDestinationPreview {
                destination: preview.destination.clone(),
                bytes: preview.bytes,
                partition_sizes: options.sizes.clone(),
                partition_selection: options.selection.clone(),
                replaced: to.replaced,
            });
        }
        Ok((Self {
            pair, target_guard: to.guard, partition_sizes: options.sizes, partition_selection: options.selection,
        }, preview))
    }

    fn revalidate(&self) -> Result<(), String> {
        let from = device::resolve(&self.pair.source, false, false)?;
        let to = Migration::handoff_target(&self.pair.destination, &from)?;
        self.pair.verify(&from, &to.resolved, false)?;
        if to.guard != self.target_guard {
            return Err("Windows 迁移目标内容已变化。".into());
        }
        Ok(())
    }

    fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let DiskPair { source, destination, source_layout, destination_layout, bytes, .. } = self.pair;
        let id = data_tasks::start_with_cancel(MIGRATE_TITLE, Arc::clone(&cancel))?;
        let target = BoundDestination {
            destination: Destination {
                selection: destination,
                partition_sizes: self.partition_sizes,
                partition_selection: self.partition_selection,
            },
            layout: destination_layout,
            guard: self.target_guard,
            bytes,
        };
        finish(&id, Migration::resume(
            &id, &cancel, source, &source_layout, vec![target],
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MigrationBatch {
    source: Selection,
    source_layout: String,
    boot: Selection,
    sector: u64,
    targets: Vec<BoundDestination>,
}

impl MigrationBatch {
    pub(crate) fn prepare(source: Selection, destinations: Vec<Destination>, boot: &Selection) -> Result<(Self, Preview), String> {
        validate_destinations(&source, destinations.iter())?;
        let from = device::resolve(&source, false, false)?;
        if !matches!(from.disk.partition_style, Some(1 | 2)) {
            return Err("请选择完整的 GPT 或 MBR Windows 系统源盘。".into());
        }
        let boot_number = device::resolve(boot, false, false)?.disk.number;
        let mut numbers = BTreeSet::from([from.disk.number]);
        let mut targets = Vec::with_capacity(destinations.len());
        let mut previews = Vec::with_capacity(destinations.len());
        for destination in destinations {
            if destination.selection.target == boot.target {
                return Err("WinPE 启动介质与写入目标必须位于不同磁盘。".into());
            }
            if from.disk.partition_style != Some(2)
                && (!destination.partition_sizes.is_empty() || !destination.partition_selection.is_all())
            {
                return Err("选择迁移分区与调整 NTFS 布局需要 GPT/UEFI。".into());
            }
            let (target, to) = BoundDestination::capture(destination, &from)?;
            if to.resolved.disk.number == boot_number {
                return Err("WinPE 启动介质与写入目标必须位于不同磁盘。".into());
            }
            if !numbers.insert(to.resolved.disk.number) {
                return Err("系统迁移的目标必须是互不相同的物理磁盘。".into());
            }
            previews.push(MigrationDestinationPreview {
                destination: to.resolved.label,
                bytes: target.bytes,
                partition_sizes: target.destination.partition_sizes.clone(),
                partition_selection: target.destination.partition_selection.clone(),
                replaced: to.replaced,
            });
            targets.push(target);
        }
        let source_layout = device::handoff_layout_fingerprint(&source, &from, false)?;
        let sector = from.disk.logical_sector_size.ok_or("源盘逻辑扇区大小未知。")?;
        let preview = Preview {
            source: from.label,
            destination: previews.iter().map(|target| target.destination.as_str()).collect::<Vec<_>>().join(" · "),
            bytes: targets.iter().map(|target| target.bytes).sum(),
            destructive: true,
            partition_sizes: Vec::new(),
            partition_selection: PartitionSelection::All,
            migration_destinations: previews,
        };
        Ok((Self { source, source_layout, boot: boot.clone(), sector, targets }, preview))
    }

    fn revalidate(&self) -> Result<(), String> {
        let from = device::resolve(&self.source, false, false)?;
        let boot_number = device::resolve(&self.boot, false, false)?.disk.number;
        if device::handoff_layout_fingerprint(&self.source, &from, false)? != self.source_layout
            || from.disk.logical_sector_size != Some(self.sector)
        {
            return Err("Windows 迁移来源或目标与重启前确认时不同。".into());
        }
        let mut numbers = BTreeSet::from([from.disk.number]);
        for target in &self.targets {
            let (actual, to) = BoundDestination::capture(target.destination.clone(), &from)?;
            if to.resolved.disk.number == boot_number || !numbers.insert(to.resolved.disk.number)
                || actual.bytes != target.bytes || actual.guard != target.guard || actual.layout != target.layout
            {
                return Err("Windows 迁移目标布局已变化。".into());
            }
        }
        Ok(())
    }

    fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let id = data_tasks::start_with_cancel(MIGRATE_TITLE, Arc::clone(&cancel))?;
        finish(&id, Migration::resume(&id, &cancel, self.source, &self.source_layout, self.targets))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    source: Selection,
    offset: u64,
    bytes: u64,
    sector: u64,
    output: PortableOutput,
}

impl Backup {
    pub(crate) fn prepare(
        source: Selection,
        boot: &Target,
        path: &Path,
    ) -> Result<(Self, Preview), String> {
        if &source.target == boot {
            return Err("WinPE 启动介质必须与离线备份源位于不同磁盘。".into());
        }
        let resolved = device::resolve(&source, false, false)?;
        let sector = resolved
            .disk
            .logical_sector_size
            .ok_or("备份源扇区大小缺失。")?;
        let required = data_tasks::backup_output_bound(resolved.size, resolved.size, false)?;
        let output = PortableOutput::prepare(path, &source.target, required)?;
        let preview = Preview {
            source: resolved.label,
            destination: path.display().to_string(),
            bytes: resolved.size,
            destructive: false,
            partition_sizes: Vec::new(),
            partition_selection: PartitionSelection::All,
            migration_destinations: Vec::new(),
        };
        Ok((
            Self {
                source,
                offset: resolved.offset,
                bytes: resolved.size,
                sector,
                output,
            },
            preview,
        ))
    }

    pub(crate) fn revalidate(&self, stable: bool) -> Result<(), String> {
        let fresh = device::resolve(&self.source, stable, false)?;
        if fresh.offset != self.offset
            || fresh.size != self.bytes
            || fresh.disk.logical_sector_size != Some(self.sector)
        {
            return Err("离线备份的源范围与重启前确认时不同。".into());
        }
        self.output.open(&self.source.target)?;
        Ok(())
    }

    fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        self.revalidate(true)?;
        let (path, _, _) = self.output.open(&self.source.target)?;
        let id = data_tasks::start_with_cancel(BACKUP_TITLE, Arc::clone(&cancel))?;
        let result = data_tasks::run_with_handoff(
            &id,
            &cancel,
            Work::ArchiveBackup {
                source: self.source,
                path,
                options: BackupOptions {
                    compression: true,
                    compression_level: Compression::default(),
                    ..Default::default()
                },
                parent: None,
            },
            Some(HandoffBinding::BackupOutput(self.output)),
        );
        finish(&id, result)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Restore {
    destination: Selection,
    layout: String,
    bytes: u64,
    sector: u64,
    layers: Vec<PortableInput>,
    proof: ArchiveProof,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "format", rename_all = "camelCase", deny_unknown_fields)]
enum ArchiveProof {
    Legacy { header: BackupHeader },
    Chunked { manifest: String },
}

impl Restore {
    pub(crate) fn prepare(
        destination: Selection,
        boot: &Target,
        path: &Path,
        mut choose_parent: impl FnMut(&str) -> Option<PathBuf>,
    ) -> Result<Option<(Self, Preview)>, String> {
        if &destination.target == boot {
            return Err("WinPE 启动介质必须与离线还原目标位于不同磁盘。".into());
        }
        let target = device::resolve(&destination, false, false)?;
        let layout = device::restore_layout_fingerprint(&destination, &target)?;
        let sector = target
            .disk
            .logical_sector_size
            .ok_or("备份源扇区大小缺失。")?;
        let (first, mut input) = PortableInput::prepare(path, &destination.target)?;
        let mut layers = vec![first];
        let (proof, kind, bytes, logical_sector) = if input.chunked() {
            let Some(archive) = input.archive(|| Ok(()))? else {
                return Ok(None);
            };
            let mut chain = Chain::new(archive);
            while let Some(id) = chain.missing_parent() {
                if chain.depth() >= MAX_CHAIN {
                    return Err("备份链超过 16 层".into());
                }
                let Some(parent) = choose_parent(id) else {
                    return Ok(None);
                };
                let (locator, input) = PortableInput::prepare(&parent, &destination.target)?;
                let Some(archive) = input.archive(|| Ok(()))? else {
                    return Ok(None);
                };
                chain.add_parent(archive)?;
                layers.push(locator);
            }
            let header = chain.header();
            (
                ArchiveProof::Chunked {
                    manifest: chain.manifest().to_owned(),
                },
                header.kind.clone(),
                header.size,
                header.sector,
            )
        } else {
            let header = data_tasks::read_header(&mut input.file)?;
            (
                ArchiveProof::Legacy {
                    header: header.clone(),
                },
                header.kind,
                header.size,
                header.sector,
            )
        };
        if bytes != target.size
            || logical_sector != sector
            || (kind == "partition") != destination.partition.is_some()
        {
            return Err("备份类型、容量或逻辑扇区与目标不匹配。".into());
        }
        let preview = Preview {
            source: path.display().to_string(),
            destination: target.label,
            bytes,
            destructive: true,
            partition_sizes: Vec::new(),
            partition_selection: PartitionSelection::All,
            migration_destinations: Vec::new(),
        };
        Ok(Some((
            Self {
                destination,
                layout,
                bytes,
                sector,
                layers,
                proof,
            },
            preview,
        )))
    }

    fn revalidate(&self) -> Result<(), String> {
        let target = device::resolve(&self.destination, false, false)?;
        if device::restore_layout_fingerprint(&self.destination, &target)? != self.layout {
            return Err("WinPE 还原目标的布局与确认时不同。".into());
        }
        for file in &self.layers {
            file.open(&self.destination.target)?;
        }
        Ok(())
    }

    fn prepare_work(self, cancel: &AtomicBool) -> Result<(Work, String), String> {
        self.revalidate()?;
        let ready = || {
            if cancel.load(Ordering::Relaxed) {
                Err("任务已取消。".into())
            } else {
                Ok(())
            }
        };
        let mut inputs = self
            .layers
            .iter()
            .map(|layer| layer.open(&self.destination.target));
        let mut first = inputs.next().ok_or("WinPE 续接记录无效。")??;
        let work = match self.proof {
            ArchiveProof::Legacy { header } => {
                if data_tasks::read_header(&mut first.file)? != header || inputs.next().is_some() {
                    return Err("WinPE 备份文件与确认时不同。".into());
                }
                Work::Restore {
                    destination: self.destination,
                    file: first.file,
                    header,
                    binding: first.binding,
                }
            }
            ArchiveProof::Chunked { manifest } => {
                let first = first.archive(ready)?.ok_or("任务已取消。")?;
                if first.manifest != manifest {
                    return Err("WinPE 备份文件与确认时不同。".into());
                }
                let mut chain = Chain::new(first);
                for input in inputs {
                    ready()?;
                    let archive = input?.archive(ready)?.ok_or("任务已取消。")?;
                    chain.add_parent(archive)?;
                }
                chain.complete()?;
                Work::ArchiveRestore {
                    destination: self.destination,
                    chain,
                }
            }
        };
        Ok((work, self.layout))
    }

    fn run(self, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let id = data_tasks::start_with_cancel(RESTORE_TITLE, Arc::clone(&cancel))?;
        let result = self.prepare_work(&cancel).and_then(|(work, layout)| {
            data_tasks::run_with_handoff(
                &id,
                &cancel,
                work,
                Some(HandoffBinding::RestoreTarget(layout)),
            )
        });
        finish(&id, result)
    }
}

fn finish(id: &str, result: Result<(String, Option<String>), String>) -> Result<String, String> {
    let message = result
        .as_ref()
        .map(|(text, _)| text.clone())
        .map_err(Clone::clone);
    data_tasks::finish(id, result);
    message
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Armed {
    version: u8,
    nonce: uuid::Uuid,
    pub(crate) entry: u16,
    issued: Clock,
    entry_sha256: [u8; 32],
    app_sha256: String,
    pub(crate) task: Task,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Clock {
    utc: u64,
    local: u64,
}

fn now() -> Result<Clock, String> {
    use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(device::err)?;
    let mut wall = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut wall) };
    let local = time::Date::from_calendar_date(
        i32::from(wall.wYear),
        time::Month::try_from(wall.wMonth as u8).map_err(device::err)?,
        wall.wDay as u8,
    )
    .and_then(|day| day.with_hms(wall.wHour as u8, wall.wMinute as u8, wall.wSecond as u8))
    .map_err(device::err)?
    .assume_utc()
    .unix_timestamp()
    .try_into()
    .map_err(device::err)?;
    Ok(Clock { utc, local })
}

impl Armed {
    pub(crate) fn new(
        entry: u16,
        entry_bytes: &[u8],
        app_sha256: String,
        task: Task,
    ) -> Result<Self, String> {
        Ok(Self {
            version: 2,
            nonce: uuid::Uuid::new_v4(),
            entry,
            issued: now()?,
            entry_sha256: Sha256::digest(entry_bytes).into(),
            app_sha256,
            task,
        })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        let payload = serde_json::to_vec(self).map_err(device::err)?;
        if payload.len() > MAX_PAYLOAD {
            return Err("WinPE 续接任务超过固件存储限制。".into());
        }
        let mut bytes = RECORD_MAGIC.to_vec();
        bytes.extend(miniz_oxide::deflate::compress_to_vec_zlib(&payload, 1));
        if bytes.len() > MAX_RECORD {
            return Err("WinPE 续接任务超过固件存储限制。".into());
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_RECORD {
            return Err("WinPE 续接任务超过固件存储限制。".into());
        }
        let data = bytes
            .strip_prefix(RECORD_MAGIC)
            .ok_or("WinPE 续接记录无效。")?;
        let decoded = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, MAX_PAYLOAD)
            .map_err(|_| "WinPE 续接记录无效。")?;
        let record: Self = serde_json::from_slice(&decoded).map_err(|_| "WinPE 续接记录无效。")?;
        if record.version != 2
            || record.nonce.is_nil()
            || Target::try_from(record.app_sha256.clone()).is_err()
        {
            return Err("WinPE 续接记录无效。".into());
        }
        let geometry = |source: &Selection, bytes: u64, sector: u64| {
            Target::try_from(source.layout_key.clone()).is_ok()
                && source.partition != Some(0)
                && matches!(sector, 512 | 4096)
                && bytes > 0
                && bytes % sector == 0
        };
        let pair = |job: &DiskPair| {
            geometry(&job.source, job.bytes, job.sector)
                && geometry(&job.destination, job.bytes, job.sector)
                && data_tasks::validate_pair(&job.source, &job.destination).is_ok()
                && Target::try_from(job.source_layout.clone()).is_ok()
                && Target::try_from(job.destination_layout.clone()).is_ok()
        };
        let valid = match &record.task {
            Task::Backup(job) => {
                geometry(&job.source, job.bytes, job.sector) && job.offset % job.sector == 0
            }
            Task::Restore(job) => {
                geometry(&job.destination, job.bytes, job.sector)
                    && (1..=MAX_CHAIN).contains(&job.layers.len())
                    && Target::try_from(job.layout.clone()).is_ok()
            }
            Task::Clone(job) => pair(&job.pair),
            Task::Migrate(job) => pair(&job.pair) && job.pair.source.partition.is_none()
                && validate_sizes(&job.partition_sizes).is_ok() && job.partition_selection.validate().is_ok(),
            Task::MigrateBatch(job) => {
                validate_destinations(&job.source, job.targets.iter().map(|target| &target.destination)).is_ok()
                    && Target::try_from(job.source_layout.clone()).is_ok()
                    && job.boot.partition.is_some_and(|partition| partition != 0)
                    && job.targets.iter().all(|target| {
                        let selection = &target.destination.selection;
                        selection.target != job.boot.target
                            && (4 * disk_core::MIB..=1 << 43).contains(&target.bytes)
                            && geometry(&job.boot, target.bytes, job.sector)
                            && geometry(&job.source, target.bytes, job.sector)
                            && geometry(selection, target.bytes, job.sector)
                            && Target::try_from(target.layout.clone()).is_ok()
                    })
            }
        };
        if !valid {
            return Err("WinPE 续接记录无效。".into());
        }
        Ok(record)
    }

    pub(crate) fn verify(&self, entry_bytes: &[u8], executable: &Path) -> Result<(), String> {
        if !valid_period(&self.issued, &now()?) {
            return Err("WinPE 续接任务已过期。".into());
        }
        if <[u8; 32]>::from(Sha256::digest(entry_bytes)) != self.entry_sha256 {
            return Err("WinPE 续接任务的固件启动项已变化。".into());
        }
        let mut hash = Sha256::new();
        const MAX_APP: u64 = 512 * 1024 * 1024;
        let mut file = File::open(executable)
            .map_err(device::err)?
            .take(MAX_APP + 1);
        let bytes = io::copy(&mut file, &mut hash).map_err(device::err)?;
        if bytes == 0 || bytes > MAX_APP || format!("{:x}", hash.finalize()) != self.app_sha256 {
            return Err("WinPE 续接任务的 DiskStudio 程序摘要不同。".into());
        }
        Ok(())
    }
}

fn valid_period(issued: &Clock, current: &Clock) -> bool {
    let recent =
        |before: u64, after: u64| (before..=before.saturating_add(LIFETIME)).contains(&after);
    // WinPE can interpret a local-time hardware clock with a different zone.
    // UTC remains authoritative when the hardware itself keeps UTC.
    recent(issued.utc, current.utc) || recent(issued.local, current.local)
}

#[test]
fn offline_backup_expiration_rejects_a_clock_before_issue_and_later_reboots() {
    let clock = |value| Clock {
        utc: value,
        local: value + 28800,
    };
    assert!(!valid_period(&clock(135), &clock(134)));
    assert!(valid_period(&clock(135), &clock(135)));
    assert!(valid_period(&clock(135), &clock(135 + LIFETIME)));
    assert!(!valid_period(&clock(135), &clock(136 + LIFETIME)));
    assert!(valid_period(
        &clock(135),
        &Clock {
            utc: 28936,
            local: 28936
        }
    ));
    assert!(valid_period(
        &clock(135),
        &Clock {
            utc: 136,
            local: 136
        }
    ));
}

#[test]
fn portable_backup_rejects_bad_geometry_and_untyped_payloads() {
    let mut record = serde_json::json!({
        "version": 2, "nonce": "dad63fb3-bb4e-4b84-b3ce-3be0d11fe790", "entry": 3,
        "issued": {"utc": 2000, "local": 2000}, "entry_sha256": vec![0u8; 32], "app_sha256": "a".repeat(64),
        "task": {"kind": "backup", "details": {
            "source": {"target": "b".repeat(64), "layoutKey": "c".repeat(64), "partition": 1},
            "offset": 1048576, "bytes": 32768, "sector": 4096,
            "output": {
                "selection": {"target": "d".repeat(64), "layoutKey": "e".repeat(64), "partition": 2},
                "directory": ["backups"], "identity": {"volume": 1, "high": 0, "low": 2},
                "name": "source.dsbackup",
            },
        }},
    });
    let bytes = |value: &serde_json::Value| {
        let mut record = RECORD_MAGIC.to_vec();
        record.extend(miniz_oxide::deflate::compress_to_vec_zlib(
            &serde_json::to_vec(value).unwrap(),
            1,
        ));
        record
    };
    assert!(Armed::decode(&bytes(&record)).is_ok());
    assert!(Armed::decode(&vec![0; MAX_RECORD + 1]).is_err());
    record["task"]["details"]["bytes"] = 4097.into();
    assert!(Armed::decode(&bytes(&record)).is_err());
    record["task"]["details"]["bytes"] = 32768.into();
    record["task"]["details"]["command"] = "unexpected".into();
    assert!(Armed::decode(&bytes(&record)).is_err());
}

#[test]
fn copy_handoff_rejects_same_disk_and_invalid_migration_scopes() {
    let mut value = serde_json::json!({
        "version": 2, "nonce": "dad63fb3-bb4e-4b84-b3ce-3be0d11fe790", "entry": 3,
        "issued": {"utc": 2000, "local": 2000}, "entry_sha256": vec![0u8; 32], "app_sha256": "a".repeat(64),
        "task": {"kind": "clone", "details": {"mode": "usedSpace", "pair": {
            "source": {"target": "b".repeat(64), "layoutKey": "c".repeat(64), "partition": 1},
            "destination": {"target": "d".repeat(64), "layoutKey": "e".repeat(64), "partition": 2},
            "source_layout": "f".repeat(64), "destination_layout": "a".repeat(64),
            "bytes": 128 * 1024 * 1024, "sector": 4096
        }}}
    });
    let decode = |value: &serde_json::Value| {
        let mut record = RECORD_MAGIC.to_vec();
        record.extend(miniz_oxide::deflate::compress_to_vec_zlib(
            &serde_json::to_vec(value).unwrap(), 1,
        ));
        Armed::decode(&record)
    };
    assert!(decode(&value).is_ok());
    value["task"]["details"]["pair"]["destination"]["target"] = "b".repeat(64).into();
    assert!(decode(&value).is_err());
    value["task"]["details"]["pair"]["destination"]["target"] = "d".repeat(64).into();
    value["task"]["details"]["pair"]["destination"]["partition"] = serde_json::Value::Null;
    assert!(decode(&value).is_err());
    value["task"]["kind"] = "migrate".into();
    let details = value["task"]["details"].as_object_mut().unwrap();
    details.remove("mode");
    details.insert("target_guard".into(), serde_json::json!(vec![0u8; 32]));
    value["task"]["details"]["pair"]["destination"]["partition"] = 2.into();
    assert!(decode(&value).is_err());
    value["task"]["details"]["pair"]["source"]["partition"] = serde_json::Value::Null;
    value["task"]["details"]["pair"]["destination"]["partition"] = serde_json::Value::Null;
    assert!(decode(&value).is_ok());
    value["task"]["details"]["partition_selection"] =
        serde_json::json!({"mode": "selected", "partitions": [5]});
    let decoded = decode(&value).unwrap();
    assert!(matches!(&decoded.task, Task::Migrate(job) if
        matches!(&job.partition_selection, PartitionSelection::Selected { partitions } if partitions == &[5])));
    let encoded = decoded.encode().unwrap();
    assert!(matches!(&Armed::decode(&encoded).unwrap().task, Task::Migrate(job) if
        matches!(&job.partition_selection, PartitionSelection::Selected { partitions } if partitions == &[5])));
    value["task"]["details"]["partition_selection"]["partitions"] = serde_json::json!([5, 5]);
    assert!(decode(&value).is_err());
}

#[test]
fn migration_batch_record_binds_each_target_scope_and_rejects_conflicts() {
    let selection = |id: &str, partition| Selection {
        target: Target(id.repeat(64)),
        layout_key: "f".repeat(64),
        partition,
    };
    let target = |id, bytes, partition_selection| BoundDestination {
        destination: Destination { selection: selection(id, None), partition_sizes: Vec::new(), partition_selection },
        layout: id.repeat(64),
        guard: [bytes as u8; 32],
        bytes: bytes * disk_core::MIB,
    };
    let batch = MigrationBatch {
        source: selection("a", None),
        boot: selection("b", Some(7)),
        source_layout: "1".repeat(64),
        sector: 4096,
        targets: vec![target("c", 32, PartitionSelection::Selected { partitions: vec![5] }),
            target("d", 48, PartitionSelection::All)],
    };
    let mut armed = Armed::new(1, b"entry", "2".repeat(64), Task::MigrateBatch(batch)).unwrap();
    let decoded = Armed::decode(&armed.encode().unwrap()).unwrap();
    let Task::MigrateBatch(decoded) = decoded.task else { unreachable!() };
    assert_eq!(decoded.targets[0].bytes, 32 * disk_core::MIB);
    assert_eq!(decoded.targets[0].guard, [32; 32]);
    assert!(matches!(&decoded.targets[0].destination.partition_selection,
        PartitionSelection::Selected { partitions } if partitions == &[5]));
    assert_eq!(decoded.targets[1].bytes, 48 * disk_core::MIB);
    assert!(decoded.targets[1].destination.partition_selection.is_all());
    for invalid in [selection("a", None), selection("b", None), selection("c", None), selection("d", Some(2))] {
        let Task::MigrateBatch(batch) = &mut armed.task else { unreachable!() };
        batch.targets[1].destination.selection = invalid;
        assert!(Armed::decode(&armed.encode().unwrap()).is_err());
    }
}

#[test]
fn firmware_record_rejects_a_compressed_payload_above_the_decode_limit() {
    let mut record = RECORD_MAGIC.to_vec();
    record.extend(miniz_oxide::deflate::compress_to_vec_zlib(
        &vec![0; MAX_PAYLOAD + 1],
        1,
    ));
    assert!(record.len() < MAX_RECORD);
    assert!(Armed::decode(&record).is_err());
}

//! Clone an ordinary GPT data disk by rebuilding its live filesystems.
use super::{capture_source, device, file_clone, ntfs, policy, progress, validate_pair, Selection};
use crate::{
    data_io::{self, Region},
    extfs::Engine,
    native_locale::{self, Locale},
    partitions::platform::{
        maintenance, table_inspect_context, TableInspectContext, TableReadContext,
    },
    surface_scan::Range,
    table_archive::{self, Archive, Kind},
    table_restore::{self, Entry},
    used_space::View,
};
use disk_core::{btrfs, MIB};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};
use uuid::Uuid;
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_UPDATE_PROPERTIES;

const MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
pub(crate) const TITLE: &str = "整盘按文件克隆";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PartitionSize {
    partition: u32,
    bytes: u64,
}

struct TargetState {
    fingerprint: String,
    number: u32,
    table: Option<Archive>,
    guards: [u8; 32],
}

struct Partition {
    source: Range,
    destination: Range,
    filesystem: Files,
}

enum Files {
    Fat { fingerprint: [u8; 32], serial: u32 },
    Ntfs(Box<ntfs::Plan>),
    Btrfs { resize: String, allocation: String },
    Raw,
}

enum Prepared<'a> {
    Fat(file_clone::Plan),
    Ntfs(&'a ntfs::Plan),
    Btrfs(Box<btrfs::ResizePlan>, &'a str),
    Raw,
}

#[derive(PartialEq, Eq)]
enum Strategy {
    Fat(String),
    Ntfs,
    Btrfs,
    Raw,
    Reserved,
}

struct Volume {
    number: u32,
    entry: Entry,
    size: u64,
    strategy: Strategy,
}

pub(crate) struct Clone {
    source: Selection,
    source_before: device::Resolved,
    source_table: Archive,
    source_protected: Vec<String>,
    destination: Selection,
    target: TargetState,
    new_table: Archive,
    partitions: Vec<Partition>,
}

fn source_context(
    selection: &Selection,
    resolved: &device::Resolved,
) -> Result<TableInspectContext, String> {
    let context = table_inspect_context(selection)?;
    resolved.verify_partition_snapshot(
        &context.resolved.disk,
        &context.parts.iter().collect::<Vec<_>>(),
    )?;
    if resolved.disk.partition_style != Some(2)
        || resolved.disk.is_offline != Some(false)
        || !matches!(resolved.disk.bus_type, Some(1 | 3 | 7 | 11 | 17))
    {
        return Err("整盘按文件克隆需要联机的普通 GPT 数据盘。".into());
    }
    Ok(context)
}

fn empty_target(
    context: &TableReadContext,
    file: &mut File,
    selection: &Selection,
    table: &Archive,
) -> Result<Option<Archive>, String> {
    if !context.parts.is_empty()
        || context.resolved.size != table.size
        || context.resolved.disk.logical_sector_size != Some(table.sector)
    {
        return Err("整盘按文件克隆的目标必须是逻辑扇区大小相同的空 RAW 或 GPT 磁盘。".into());
    }
    let existing = maintenance::capture_write_table(
        context,
        file,
        &selection.target,
        Range {
            offset: 0,
            size: table.size,
            sector: table.sector,
        },
        false,
    )?;
    if let Some(old) = &existing {
        let matching_extents = old.extents.len() == table.extents.len()
            && old.extents.iter().zip(&table.extents).all(|(a, b)| {
                a.kind == b.kind && a.offset == b.offset && a.bytes.len() == b.bytes.len()
            });
        if !matching_extents || !table_restore::archive_layout(old)?.entries.is_empty() {
            return Err("空 GPT 目标的分区表几何不受支持。".into());
        }
    }
    Ok(existing)
}

fn target_guards(file: &File, size: u64) -> Result<[u8; 32], String> {
    let mut bytes = vec![0u8; MIB as usize];
    let mut hash = Sha256::new();
    for offset in [0, size - MIB] {
        let mut reader = Region::for_metadata(file, offset, MIB).map_err(device::err)?;
        reader.read_exact(&mut bytes).map_err(device::err)?;
        hash.update(&bytes);
    }
    Ok(hash.finalize().into())
}

fn packed_entry(
    old: &Entry,
    size: u64,
    sector: u64,
    end: &mut u64,
    ids: &mut BTreeSet<Uuid>,
    fixed: &[Range],
) -> Result<Entry, String> {
    if size == 0 || size % sector != 0 {
        return Err("目标分区容量必须是逻辑扇区大小的正整数倍。".into());
    }
    let align = |start: u64| {
        start
            .max(MIB)
            .div_ceil(MIB)
            .checked_mul(MIB)
            .ok_or("目标起始偏移溢出。")
    };
    let mut offset = align(*end)?;
    for range in fixed {
        let last = offset.checked_add(size).ok_or("目标分区结束偏移溢出。")?;
        if last <= range.offset {
            break;
        }
        let limit = range.offset + range.size;
        if offset < limit {
            offset = align(limit)?;
        }
    }
    *end = offset.checked_add(size).ok_or("目标分区结束偏移溢出。")?;
    new_entry(old, offset, size, ids)
}

fn new_entry(
    old: &Entry,
    offset: u64,
    size: u64,
    ids: &mut BTreeSet<Uuid>,
) -> Result<Entry, String> {
    let id = Uuid::new_v4();
    if !ids.insert(id) {
        return Err("新 GPT 标识重复，请重新预览。".into());
    }
    Ok(Entry {
        guid: id.to_string(),
        offset,
        size,
        ..old.clone()
    })
}

fn classify_volumes(
    entries: Vec<Entry>,
    context: &TableInspectContext,
    source: &device::Resolved,
    protected: &[String],
    mut overrides: BTreeMap<u32, u64>,
) -> Result<Vec<Volume>, String> {
    let mut volumes = Vec::with_capacity(entries.len());
    let mut reserved = false;
    let parts = context.parts.iter().collect::<Vec<_>>();
    for entry in entries {
        let part = parts
            .iter()
            .find(|part| part.guid.as_deref().map(policy::guid).as_deref() == Some(&entry.guid))
            .ok_or("GPT 条目缺少唯一的 Windows 分区号。")?;
        let number = part.partition_number;
        let requested = overrides.remove(&number);
        let size = requested.unwrap_or(entry.size);
        let associated = source.has_partition_volume(number);
        if entry.attributes != 0 {
            return Err("源盘含有受保护、未知或不支持按文件克隆的 GPT 条目。".into());
        }
        let strategy = match entry.kind.as_str() {
            MSR if !reserved && requested.is_none() && !associated => {
                reserved = true;
                Strategy::Reserved
            }
            _ if protected.contains(&entry.guid) => {
                return Err("源 GPT 分区具有原生保护属性。".into());
            }
            policy::BASIC_DATA if policy::formattable(part) && associated => {
                match source
                    .partition_filesystem(number)
                    .unwrap_or("")
                    .to_ascii_uppercase()
                {
                    label if label == "NTFS" => Strategy::Ntfs,
                    label
                        if matches!(
                            label.as_str(),
                            "FAT" | "FAT12" | "FAT16" | "FAT32" | "EXFAT"
                        ) =>
                    {
                        Strategy::Fat(label)
                    }
                    _ => Strategy::Raw,
                }
            }
            policy::LINUX_DATA
                if !associated && policy::linux_clone_candidate(&source.disk, &parts, part) =>
            {
                if size == entry.size {
                    Strategy::Raw
                } else {
                    Strategy::Btrfs
                }
            }
            _ => return Err("源盘含有受保护、未知或不支持按文件克隆的 GPT 条目。".into()),
        };
        if strategy == Strategy::Raw && size != entry.size {
            return Err("只能按扇区复制的分区必须保留原容量。".into());
        }
        volumes.push(Volume {
            number,
            entry,
            size,
            strategy,
        });
    }
    if volumes.iter().all(|v| v.strategy == Strategy::Reserved) || !overrides.is_empty() {
        return Err("至少需要一个数据分区。自定义容量须对应源盘的数据分区。".into());
    }
    volumes.sort_by_key(|volume| volume.entry.offset);
    Ok(volumes)
}

fn range(entry: &Entry, sector: u64) -> Range {
    Range {
        offset: entry.offset,
        size: entry.size,
        sector,
    }
}

fn region(file: &File, range: Range) -> Result<Region<&File>, String> {
    Region::for_metadata(file, range.offset, range.size).map_err(device::err)
}

fn gaps(
    table: &Archive,
    partitions: impl IntoIterator<Item = Range>,
) -> Result<Vec<Range>, String> {
    let mut occupied = table
        .extents
        .iter()
        .map(|extent| (extent.offset, extent.bytes.len() as u64))
        .chain(
            partitions
                .into_iter()
                .map(|range| (range.offset, range.size)),
        )
        .collect::<Vec<_>>();
    occupied.sort_unstable();
    let mut gaps = Vec::new();
    let mut end = 0;
    for (offset, size) in occupied.into_iter().chain([(table.size, 0)]) {
        if offset < end || offset > table.size || size > table.size - offset {
            return Err("整盘克隆的有效载荷或 GPT 范围重叠、越界。".into());
        }
        if offset > end {
            gaps.push(Range {
                offset: end,
                size: offset - end,
                sector: table.sector,
            });
        }
        end = offset + size;
    }
    Ok(gaps)
}

impl Clone {
    pub(crate) fn prepare(
        source: Selection,
        destination: Selection,
        sizes: Vec<PartitionSize>,
        locale: &Locale,
    ) -> Result<(Self, String, String, u64, String), String> {
        validate_pair(&source, &destination)?;
        if source.partition.is_some() || sizes.len() > 128 {
            return Err("整盘按文件克隆需要选择两块整盘。".into());
        }
        let before = device::resolve(&source, true, false)?;
        let sector = before.disk.logical_sector_size.ok_or("扇区大小缺失")?;
        let source_ctx = source_context(&source, &before)?;
        let target_ctx = maintenance::write_context(&destination, None)?;
        if before.disk.number == target_ctx.resolved.disk.number {
            return Err("源和目标必须是不同的物理磁盘。".into());
        }
        let size = target_ctx.resolved.size;
        if !target_ctx.parts.is_empty()
            || target_ctx.resolved.disk.logical_sector_size != Some(sector)
        {
            return Err("整盘按文件克隆的目标必须是逻辑扇区大小相同的空 RAW 或 GPT 磁盘。".into());
        }
        let mut overrides = BTreeMap::new();
        for size in sizes {
            if overrides.insert(size.partition, size.bytes).is_some() {
                return Err("目标分区容量被指定了多次。".into());
            }
        }
        let source_file = device::device(&before, false)?;
        let (source_table, source_protected) = capture_source(&source_ctx, &source_file, &source)?;
        let layout = table_restore::archive_layout(&source_table)?;
        let disk_guid = Uuid::new_v4();
        let mut known_ids = layout
            .entries
            .iter()
            .map(|entry| Uuid::parse_str(&entry.guid))
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(device::err)?;
        known_ids.insert(Uuid::parse_str(&layout.guid).map_err(device::err)?);
        if !known_ids.insert(disk_guid) {
            return Err("新 GPT 标识重复，请重新预览。".into());
        }
        let volumes = classify_volumes(
            layout.entries,
            &source_ctx,
            &before,
            &source_protected,
            overrides,
        )?;
        let has_raw = volumes
            .iter()
            .any(|volume| volume.strategy == Strategy::Raw);
        let keep_start = |strategy: &Strategy| {
            *strategy == Strategy::Raw || has_raw && *strategy == Strategy::Reserved
        };
        let fixed = volumes
            .iter()
            .filter(|volume| keep_start(&volume.strategy))
            .map(|volume| range(&volume.entry, sector))
            .collect::<Vec<_>>();
        let mut end = 0;
        let mut entries = Vec::with_capacity(volumes.len());
        let mut jobs = Vec::new();
        let mut details = Vec::new();
        let mut has_msr = false;
        for Volume {
            number,
            entry,
            size,
            strategy,
        } in volumes
        {
            let target = if keep_start(&strategy) {
                new_entry(&entry, entry.offset, entry.size, &mut known_ids)?
            } else {
                packed_entry(&entry, size, sector, &mut end, &mut known_ids, &fixed)?
            };
            if strategy == Strategy::Reserved {
                has_msr = true;
            } else {
                jobs.push((
                    number,
                    range(&entry, sector),
                    range(&target, sector),
                    strategy,
                ));
            }
            entries.push(target);
        }
        let new_table = table_restore::new_gpt(
            size,
            sector,
            destination.target.0.clone(),
            disk_guid,
            &entries,
        )?;
        let mut target_file = device::device(&target_ctx.resolved, false)?;
        let table = empty_target(&target_ctx, &mut target_file, &destination, &new_table)?;
        if let Some(old) = &table {
            let old_id =
                Uuid::parse_str(&table_restore::archive_layout(old)?.guid).map_err(device::err)?;
            if entries.iter().any(|entry| entry.guid == old_id.to_string()) || old_id == disk_guid {
                return Err("新 GPT 标识重复，请重新预览。".into());
            }
        }
        let target = TargetState {
            fingerprint: target_ctx.fingerprint,
            number: target_ctx.resolved.disk.number,
            table,
            guards: target_guards(&target_file, size)?,
        };
        let cancel = AtomicBool::new(false);
        let mut partitions = Vec::with_capacity(jobs.len());
        let mut preserves_filesystem_identity = has_raw;
        for (number, from, to, strategy) in jobs {
            let serial = (Uuid::new_v4().as_u128() as u64).max(1);
            let (filesystem, format) = match strategy {
                Strategy::Ntfs => {
                    let mut reader = region(&source_file, from)?;
                    if !ntfs::is_ntfs(&mut reader)? {
                        return Err("源分区的文件系统与 Windows 卷元数据不一致。".into());
                    }
                    (
                        Files::Ntfs(Box::new(ntfs::Plan::prepare(
                            &mut reader,
                            to,
                            serial,
                            &cancel,
                        )?)),
                        "NTFS".into(),
                    )
                }
                Strategy::Fat(expected) => {
                    let mut reader = region(&source_file, from)?;
                    let serial = (serial as u32).max(1);
                    let plan = file_clone::prepare(
                        &mut reader,
                        sector,
                        to.size,
                        to.offset,
                        serial,
                        &cancel,
                    )?;
                    let actual = plan.source_format.to_ascii_uppercase();
                    if expected != actual
                        && !(expected == "FAT" && matches!(actual.as_str(), "FAT12" | "FAT16"))
                    {
                        return Err("源分区的文件系统与 Windows 卷元数据不一致。".into());
                    }
                    (
                        Files::Fat {
                            fingerprint: plan.fingerprint(),
                            serial,
                        },
                        plan.destination_format.into(),
                    )
                }
                Strategy::Btrfs => {
                    let mut reader =
                        Region::new(&source_file, from.offset, from.size).map_err(device::err)?;
                    let plan = btrfs::plan_resize(&mut reader, to.size)?;
                    let (_, allocation) = super::btrfs_view(reader, &plan, to, &cancel)?;
                    preserves_filesystem_identity = true;
                    (
                        Files::Btrfs {
                            resize: plan.proof(),
                            allocation: allocation.digest,
                        },
                        locale.format(
                            "Btrfs · 搬移 {bytes} B 条带",
                            &[("bytes", &plan.relocation_bytes().to_string())],
                        ),
                    )
                }
                Strategy::Raw => (
                    Files::Raw,
                    locale.text("逐扇区复制，保留分区起点和容量").to_string(),
                ),
                Strategy::Reserved => unreachable!(),
            };
            details.push(locale.format(
                "分区 {partition} → {filesystem} · {bytes} B",
                &[
                    ("partition", &number.to_string()),
                    ("filesystem", &format),
                    ("bytes", &to.size.to_string()),
                ],
            ));
            partitions.push(Partition {
                source: from,
                destination: to,
                filesystem,
            });
        }
        let summary = details.join(" / ");
        let warning = locale.sentences(&[
            "覆盖整个空目标磁盘，按预览方式复制各数据分区，生成新的 GPT 磁盘与分区标识。未使用范围清零，完成或中止后保持脱机。",
            &summary,
            if has_msr { "重新创建 Microsoft 保留分区并清零其数据范围。" } else { "" },
            if preserves_filesystem_identity { "调整容量的 Btrfs 与逐扇区复制的分区保留卷标识。使用副本时请将源盘保持脱机。" } else { "" },
        ]);
        let source_label = before.label.clone();
        let target_label = target_ctx.resolved.label;
        Ok((
            Self {
                source,
                source_before: before,
                source_table,
                source_protected,
                destination,
                target,
                new_table,
                partitions,
            },
            source_label,
            target_label,
            size,
            warning,
        ))
    }

    pub(crate) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let size = self.new_table.size;
        let sector = self.new_table.sector;
        progress(id, cancel, "锁定源卷", 0, size)?;
        let before = device::resolve(&self.source, true, false)?;
        if !before.same_data_binding(&self.source_before)
            || before.disk.number == self.target.number
        {
            return Err("源盘身份、布局或卷关联与预览不同。".into());
        }
        let source_locks = device::state_locks(&before)?;
        let source = device::resolve_locked(&self.source, false, &before, &source_locks)?;
        let ctx = source_context(&self.source, &source)?;
        let file = device::device(&source, false)?;
        let validate_source = || -> Result<(), String> {
            device::resolve_locked(&self.source, false, &before, &source_locks)?;
            let (table, protected) = capture_source(&ctx, &file, &self.source)?;
            if table != self.source_table || protected != self.source_protected {
                return Err("源盘 GPT 在克隆期间发生变化。".into());
            }
            Ok(())
        };
        validate_source()?;
        let mut prepared = Vec::with_capacity(self.partitions.len());
        let mut engine = None;
        progress(id, cancel, "检查文件系统分配信息", 0, size)?;
        for part in &self.partitions {
            let to = part.destination;
            prepared.push(match &part.filesystem {
                Files::Fat {
                    serial,
                    fingerprint,
                } => {
                    let mut reader = region(&file, part.source)?;
                    let plan = file_clone::prepare(
                        &mut reader,
                        sector,
                        to.size,
                        to.offset,
                        *serial,
                        cancel,
                    )?;
                    if plan.fingerprint() != *fingerprint {
                        return Err("源文件系统分配计划已变化，请重新预览。".into());
                    }
                    Prepared::Fat(plan)
                }
                Files::Ntfs(plan) => {
                    let mut reader = region(&file, part.source)?;
                    plan.revalidate(&mut reader, cancel)?;
                    if engine.is_none() {
                        engine = Some(ntfs::Plan::engine()?);
                    }
                    Prepared::Ntfs(plan)
                }
                Files::Btrfs { resize, allocation } => {
                    let mut reader = Region::new(&file, part.source.offset, part.source.size)
                        .map_err(device::err)?;
                    let plan = btrfs::plan_resize(&mut reader, to.size)?;
                    let (_, map) = super::btrfs_view(reader, &plan, to, cancel)?;
                    if plan.proof() != *resize || map.digest != *allocation {
                        return Err("源文件系统分配计划已变化，请重新预览。".into());
                    }
                    Prepared::Btrfs(Box::new(plan), allocation)
                }
                Files::Raw => Prepared::Raw,
            });
        }
        let gaps = gaps(
            &self.new_table,
            self.partitions.iter().map(|part| part.destination),
        )?;
        progress(id, cancel, "锁定目标", 0, size)?;
        let target = maintenance::write_context(&self.destination, None)?;
        if target.fingerprint != self.target.fingerprint
            || target.resolved.disk.number != self.target.number
        {
            return Err("目标盘身份、布局或保护与预览不同。".into());
        }
        let target_locks = device::lock(&target.resolved)?;
        let target = maintenance::write_context(&self.destination, Some((&target, &target_locks)))?;
        let validate_target = |output: &mut File| -> Result<(), String> {
            if empty_target(&target, output, &self.destination, &self.new_table)?
                != self.target.table
            {
                return Err("目标 GPT 与预览不同。".into());
            }
            Ok(())
        };
        validate_source()?;
        let mut output = device::exclusive_device(&target.resolved)?;
        validate_target(&mut output)?;
        progress(id, cancel, "锁定目标", 0, size)?;
        device::persist_offline(&output)
            .map_err(|error| format!("磁盘可能已脱机，尚未发送数据写入：{error}"))?;
        let result = (|| -> Result<String, String> {
            device::confirm_writable_offline(&output)?;
            validate_target(&mut output)?;
            if target_guards(&output, size)? != self.target.guards {
                return Err("预览后目标内容已变化。".into());
            }
            let mut verified = Vec::with_capacity(prepared.len());
            for (part, plan) in self.partitions.iter().zip(prepared) {
                let digest = copy_partition(
                    &file,
                    &mut output,
                    size,
                    part,
                    plan,
                    engine.as_ref(),
                    id,
                    cancel,
                )?;
                verified.push((part.destination, digest));
            }
            let zero = vec![0; MIB as usize];
            let mut cleared = 0;
            let clear_size = gaps.iter().map(|gap| gap.size).sum();
            for gap in &gaps {
                let mut view =
                    View::bounded(&mut output, gap.offset, gap.size, size).map_err(device::err)?;
                let mut left = gap.size;
                while left > 0 {
                    progress(id, cancel, "清零目标未使用空间", cleared, clear_size)?;
                    let len = left.min(MIB);
                    if view.write(&zero[..len as usize]).map_err(device::err)? != len as usize {
                        return Err("目标清零写入长度不足。".into());
                    }
                    cleared += len;
                    left -= len;
                }
            }
            output.sync_all().map_err(device::err)?;
            validate_source()?;
            validate_target(&mut output)?;
            device::confirm_writable_offline(&output)?;
            progress(id, cancel, "写入新 GPT", 0, size)?;
            publish_table(&mut output, &self.new_table, |file| file.sync_all())?;
            device::control(&output, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])?;
            let raw = table_archive::capture(
                &mut output,
                size,
                sector,
                self.destination.target.0.clone(),
                false,
                || progress(id, cancel, "校验目标", 0, size),
            )?;
            let native = crate::extfs::restore_native(&output, None, sector)?.0;
            if raw != self.new_table || !table_restore::archive_layout(&raw)?.agrees(&native) {
                return Err("写后原始 GPT 或 Windows 原生布局与预览不一致。".into());
            }
            let sha = verify_disk(&mut output, &raw, &gaps, &verified, |bytes| {
                progress(id, cancel, "校验目标", bytes, size)
            })?;
            device::confirm_writable_offline(&output)?;
            validate_source()?;
            Ok(sha)
        })();
        let locale = native_locale::current();
        let digest = result.map_err(|error| {
            locale.format(
                "目标磁盘保持脱机，可能仅完成部分覆盖；未自动重试或回滚。{error}",
                &[("error", locale.text(&error))],
            )
        })?;
        Ok((
            locale
                .text(
                    "整盘按文件克隆完成。新 GPT、各数据卷及整盘 SHA-256 已核对，目标磁盘保持脱机。",
                )
                .into(),
            Some(digest),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_partition(
    file: &File,
    output: &mut File,
    size: u64,
    part: &Partition,
    plan: Prepared<'_>,
    engine: Option<&Engine>,
    id: &str,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let to = part.destination;
    let mut target = View::bounded(output, to.offset, to.size, size).map_err(device::err)?;
    let digest = match plan {
        Prepared::Fat(plan) => {
            let mut input = plan.reader(region(file, part.source)?);
            let digest = write_partition(&mut input, &mut target, to, id, cancel)?;
            plan.verify(&mut region(target.inner_mut(), to)?)?;
            digest
        }
        Prepared::Ntfs(plan) => {
            let digest = plan.write(
                engine.ok_or("未加载 NTFS 克隆工具。")?,
                &mut region(file, part.source)?,
                &mut target,
                |view| view.inner_mut().sync_all(),
                cancel,
                |stage, now, total| progress(id, cancel, stage, now, total),
            )?;
            plan.verify(&mut region(target.inner_mut(), to)?, cancel)?;
            digest
        }
        Prepared::Btrfs(plan, expected_allocation) => {
            let source =
                Region::new(file, part.source.offset, part.source.size).map_err(device::err)?;
            let (view, allocation) = super::btrfs_view(source, &plan, to, cancel)?;
            if allocation.digest != expected_allocation {
                return Err("源文件系统分配计划已变化，请重新预览。".into());
            }
            let identity = view.identity();
            let digest =
                write_partition(&mut allocation.masked(view), &mut target, to, id, cancel)?;
            super::verify_btrfs_copy(&mut region(target.inner_mut(), to)?, identity, cancel)?;
            digest
        }
        Prepared::Raw => {
            let mut reader =
                Region::new(file, part.source.offset, part.source.size).map_err(device::err)?;
            write_partition(&mut reader, &mut target, to, id, cancel)?
        }
    };
    Ok(digest)
}

fn write_partition(
    reader: &mut (impl Read + Send),
    output: &mut View<&mut File>,
    to: Range,
    id: &str,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let digest = data_io::copy_exact(reader, output, to.size, |n| {
        progress(id, cancel, "写入目标", n, to.size)
    })?;
    let file = output.inner_mut();
    file.sync_all().map_err(device::err)?;
    let mut readback = Region::new(&**file, to.offset, to.size).map_err(device::err)?;
    if data_io::hash_exact(&mut readback, to.size, |n| {
        progress(id, cancel, "校验目标", n, to.size)
    })? != digest
    {
        return Err("目标分区完整 SHA-256 回读不一致。".into());
    }
    Ok(digest)
}

fn publish_table<R: Read + Write + Seek>(
    file: &mut R,
    table: &Archive,
    mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
) -> Result<(), String> {
    for kind in [
        Kind::BackupEntries,
        Kind::BackupHeader,
        Kind::PrimaryEntries,
        Kind::PrimaryHeader,
        Kind::Mbr,
    ] {
        let extent = table
            .extents
            .iter()
            .find(|e| e.kind == kind)
            .ok_or("新 GPT 范围缺失。")?;
        file.seek(SeekFrom::Start(extent.offset))
            .map_err(device::err)?;
        if file.write(&extent.bytes).map_err(device::err)? != extent.bytes.len() {
            return Err("新 GPT 写入长度不足。".into());
        }
        sync(file).map_err(device::err)?;
        let mut check = vec![0; extent.bytes.len()];
        file.seek(SeekFrom::Start(extent.offset))
            .map_err(device::err)?;
        file.read_exact(&mut check).map_err(device::err)?;
        if check != extent.bytes {
            return Err("新 GPT 范围回读不一致。".into());
        }
    }
    Ok(())
}

fn verify_disk(
    file: &mut (impl Read + Seek),
    table: &Archive,
    gaps: &[Range],
    partitions: &[(Range, String)],
    mut checkpoint: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    enum Content<'a> {
        Zero,
        Digest(&'a str),
        Table(&'a [u8]),
    }
    let mut extents = gaps
        .iter()
        .map(|r| (*r, Content::Zero))
        .chain(
            partitions
                .iter()
                .map(|(r, digest)| (*r, Content::Digest(digest))),
        )
        .chain(table.extents.iter().map(|e| {
            (
                Range {
                    offset: e.offset,
                    size: e.bytes.len() as u64,
                    sector: table.sector,
                },
                Content::Table(&e.bytes),
            )
        }))
        .collect::<Vec<_>>();
    extents.sort_unstable_by_key(|(range, _)| range.offset);
    let mut disk = Sha256::new();
    let mut read = 0;
    let mut buffer = vec![0; MIB as usize];
    for (range, content) in extents {
        if range.offset != read || range.size > table.size - read {
            return Err("整盘回读范围不连续。".into());
        }
        file.seek(SeekFrom::Start(range.offset))
            .map_err(device::err)?;
        let mut part = Sha256::new();
        let mut pos = 0;
        while pos < range.size {
            checkpoint(read)?;
            let len = (range.size - pos).min(MIB) as usize;
            let bytes = &mut buffer[..len];
            file.read_exact(bytes).map_err(device::err)?;
            match content {
                Content::Zero if bytes.iter().any(|&b| b != 0) => {
                    return Err("目标清零范围回读不一致。".into())
                }
                Content::Table(expected)
                    if bytes != &expected[pos as usize..pos as usize + len] =>
                {
                    return Err("目标 GPT 回读不一致。".into())
                }
                Content::Digest(_) => part.update(&*bytes),
                _ => {}
            }
            disk.update(&*bytes);
            pos += len as u64;
            read += len as u64;
        }
        if let Content::Digest(expected) = content {
            if format!("{:x}", part.finalize()) != expected {
                return Err("目标分区完整 SHA-256 回读不一致。".into());
            }
        }
    }
    if read != table.size {
        return Err("整盘回读范围不完整。".into());
    }
    Ok(format!("{:x}", disk.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source_entry(slot: u32) -> Entry {
        Entry {
            slot,
            guid: Uuid::from_u128(u128::from(slot)).to_string(),
            kind: policy::BASIC_DATA.into(),
            offset: u64::from(slot) * MIB,
            size: 2 * MIB,
            attributes: 0,
            name: "数据卷"
                .encode_utf16()
                .chain(std::iter::repeat(0))
                .take(36)
                .collect(),
        }
    }

    #[test]
    fn packs_a_windows_reserved_partition_after_the_gpt_array() {
        let sector = 4096;
        let disk = Uuid::from_u128(100);
        let mut ids = BTreeSet::from([disk, Uuid::from_u128(1), Uuid::from_u128(2)]);
        let mut end = 0;
        let source = Entry {
            kind: MSR.into(),
            offset: 6 * sector,
            size: 16 * MIB - 6 * sector,
            ..source_entry(1)
        };
        let reserved = packed_entry(&source, source.size, sector, &mut end, &mut ids, &[]).unwrap();
        let data =
            packed_entry(&source_entry(2), 48 * MIB, sector, &mut end, &mut ids, &[]).unwrap();
        assert_eq!(reserved.offset, MIB);
        assert_eq!(data.offset, 17 * MIB);
        let table = table_restore::new_gpt(
            128 * MIB,
            sector,
            "a".repeat(64),
            disk,
            &[reserved.clone(), data],
        )
        .unwrap();
        let checked = table_restore::archive_layout(&table).unwrap();
        assert_eq!(checked.entries[0], reserved);
        assert_eq!(checked.entries[0].size, source.size);
    }

    #[test]
    fn packs_rebuildable_partitions_around_fixed_sector_copies() {
        for sector in [512, 4096] {
            let raw = Entry {
                offset: 20 * MIB,
                size: 8 * MIB,
                kind: policy::LINUX_DATA.into(),
                ..source_entry(3)
            };
            let first_usable = 2 * sector + 128 * 128;
            let reserved = Range {
                offset: first_usable,
                size: 16 * MIB - first_usable,
                sector,
            };
            let fixed = [reserved, range(&raw, sector)];
            let mut end = 0;
            let mut ids = BTreeSet::new();
            let preserved = new_entry(&raw, raw.offset, raw.size, &mut ids).unwrap();
            let first = packed_entry(
                &source_entry(2),
                4 * MIB,
                sector,
                &mut end,
                &mut ids,
                &fixed,
            )
            .unwrap();
            let next = packed_entry(
                &source_entry(4),
                6 * MIB,
                sector,
                &mut end,
                &mut ids,
                &fixed,
            )
            .unwrap();
            assert_eq!((first.offset, next.offset), (16 * MIB, 28 * MIB));
            assert_eq!(range(&preserved, sector).offset, raw.offset);
            assert_eq!(preserved.size, raw.size);
            assert_ne!(preserved.guid, raw.guid);
        }
    }

    #[test]
    fn reparse_both_gpt_sector_sizes_and_verify_the_entire_disk() {
        for sector in [512, 4096] {
            let mut end = 0;
            let disk_guid = Uuid::from_u128(100);
            let mut ids = BTreeSet::from([disk_guid, Uuid::from_u128(1), Uuid::from_u128(3)]);
            let first = packed_entry(
                &source_entry(1),
                2 * MIB + sector,
                sector,
                &mut end,
                &mut ids,
                &[],
            )
            .unwrap();
            let second =
                packed_entry(&source_entry(3), MIB, sector, &mut end, &mut ids, &[]).unwrap();
            assert_eq!(first.offset, MIB);
            assert_eq!(second.offset, 4 * MIB);
            let table = table_restore::new_gpt(
                8 * MIB,
                sector,
                "a".repeat(64),
                disk_guid,
                &[first.clone(), second.clone()],
            )
            .unwrap();
            let parts = [range(&first, sector), range(&second, sector)];
            let gaps = super::gaps(&table, parts).unwrap();
            let mut media = Cursor::new(vec![0; table.size as usize]);
            let mut proofs = Vec::new();
            for (part, fill) in parts.into_iter().zip([0x59, 0xac]) {
                let contents =
                    &mut media.get_mut()[part.offset as usize..(part.offset + part.size) as usize];
                contents.fill(fill);
                proofs.push((part, format!("{:x}", Sha256::digest(contents))));
            }
            let mut flushed = Vec::new();
            publish_table(&mut media, &table, |f| {
                flushed.push(f.position());
                Ok(())
            })
            .unwrap();
            assert_eq!(
                flushed,
                [
                    table.size - sector,
                    table.size,
                    sector * 2 + 16384,
                    sector * 2,
                    sector
                ]
            );
            let raw = table_archive::capture(
                &mut media,
                table.size,
                sector,
                table.source.clone(),
                false,
                || Ok(()),
            )
            .unwrap();
            assert_eq!(raw, table);
            assert_eq!(
                table_restore::archive_layout(&raw).unwrap().entries,
                [first, second]
            );
            let sha = verify_disk(&mut media, &raw, &gaps, &proofs, |_| Ok(())).unwrap();
            assert_eq!(sha, format!("{:x}", Sha256::digest(media.get_ref())));
            for corrupt in [0, gaps[0].offset, parts[0].offset] {
                media.get_mut()[corrupt as usize] ^= 1;
                assert!(verify_disk(&mut media, &raw, &gaps, &proofs, |_| Ok(())).is_err());
                media.get_mut()[corrupt as usize] ^= 1;
            }
            assert!(verify_disk(&mut media, &raw, &gaps, &proofs[1..], |_| Ok(())).is_err());
            assert!(
                verify_disk(&mut media, &raw, &gaps, &proofs, |_| Err("canceled".into())).is_err()
            );
        }
    }

    #[test]
    fn rejects_overlapping_oversized_and_duplicate_gpt_entries() {
        let mut first = source_entry(1);
        let mut second = source_entry(2);
        let disk_guid = Uuid::from_u128(100);
        let make = |entries: &[Entry]| {
            table_restore::new_gpt(8 * MIB, 4096, "a".repeat(64), disk_guid, entries)
        };
        assert!(make(&[first.clone(), second.clone()]).is_err());
        second.offset = 6 * MIB;
        assert!(make(&[first.clone(), second.clone()]).is_err());
        second.offset = 4 * MIB;
        assert!(make(&[first.clone(), second.clone()]).is_ok());
        second.guid = first.guid.clone();
        assert!(make(&[first.clone(), second]).is_err());
        first.guid = disk_guid.to_string();
        assert!(make(&[first]).is_err());
    }
}

//! Prepare an adjusted-size or file-based clone of a confirmed partition.
use super::{progress, validate_pair, Selection, PARTITION_OVERWRITE_WARNING};
use crate::{
    data_io::{self, Region},
    data_windows as device,
    fat_recovery::resize::{clone, file_clone, Plan},
    native_locale::Locale,
    ntfs_file_clone as ntfs,
    partitions::{
        platform::{maintenance, table_inspect_context, TableInspectContext, TableReadContext},
        policy,
    },
    surface_scan::{Options, Range},
    table_archive::{self, Archive},
    table_restore,
    used_space::{self, View},
};
use disk_core::btrfs;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{atomic::AtomicBool, Arc},
};

#[path = "disk_file_clone_windows.rs"]
pub(super) mod disk;

struct Source {
    selection: Selection,
    before: device::Resolved,
    context_key: String,
    table: Archive,
}

pub(crate) struct Clone {
    source: Source,
    target: PartitionTarget,
    plan: ClonePlan,
}

enum ClonePlan {
    Resize([u8; 32]),
    BtrfsResize { resize: String, allocation: String },
    LinuxExact { proof: String, mode: used_space::Mode },
    Files(FilePlan),
}

pub(super) enum FilePlan {
    Fat { fingerprint: [u8; 32], serial: u32 },
    Ntfs(Box<ntfs::Plan>),
}

pub(super) fn file_serial() -> u64 {
    (uuid::Uuid::new_v4().as_u128() as u64).max(1)
}

fn check_plan(before: [u8; 32], after: [u8; 32]) -> Result<(), String> {
    if before == after {
        Ok(())
    } else {
        Err("源文件系统分配计划已变化，请重新预览。".into())
    }
}

pub(super) enum Method {
    Resize,
    LinuxExact(used_space::Mode),
    Files { serial: u64 },
}

impl Method {
    pub(super) fn files() -> Self {
        Self::Files {
            serial: file_serial(),
        }
    }

    pub(super) fn title(&self) -> &'static str {
        match self {
            Self::Resize => "调整容量克隆",
            Self::LinuxExact(used_space::Mode::AllSectors) => "逐扇区克隆",
            Self::LinuxExact(used_space::Mode::UsedSpace) => "按已用空间克隆",
            Self::Files { .. } => "按文件克隆",
        }
    }
}

pub(super) struct PartitionTarget {
    disk: Selection,
    key: String,
    pub(super) range: Range,
    number: u32,
    partition: u32,
    kind: PartitionType,
    table: Archive,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartitionType {
    Windows,
    Linux,
}

fn whole(selection: &Selection) -> Selection {
    Selection {
        partition: None,
        ..selection.clone()
    }
}

fn source_context(
    selection: &Selection,
    source: &device::Resolved,
    allow_linux: bool,
) -> Result<(TableInspectContext, PartitionType), String> {
    let context = table_inspect_context(&whole(selection))?;
    let parts = context.parts.iter().collect::<Vec<_>>();
    source.verify_partition_snapshot(&context.resolved.disk, &parts)?;
    let part = parts
        .iter()
        .find(|p| Some(p.partition_number) == selection.partition)
        .ok_or("所选源分区已不存在。")?;
    if source.disk.partition_style != Some(2)
        || part.offset != source.offset
        || part.size != source.size
    {
        return Err("克隆需要选择两个普通 GPT 数据分区。".into());
    }
    let kind = match part.gpt_type.as_deref().map(policy::guid).as_deref() {
        Some(policy::BASIC_DATA)
            if part.is_shadow_copy != Some(true) && part.is_hidden != Some(true) =>
        {
            PartitionType::Windows
        }
        Some(policy::LINUX_DATA)
            if allow_linux
                && source.volumes.is_empty()
                && policy::linux_clone_candidate(&source.disk, &parts, part) =>
        {
            PartitionType::Linux
        }
        _ => return Err("克隆需要选择两个普通 GPT 数据分区。".into()),
    };
    Ok((context, kind))
}

fn capture_source(
    context: &TableInspectContext,
    mut file: &File,
    selection: &Selection,
) -> Result<(Archive, Vec<String>), String> {
    let sector = context
        .resolved
        .disk
        .logical_sector_size
        .ok_or("扇区大小缺失")?;
    let table = table_archive::capture(
        &mut file,
        context.resolved.size,
        sector,
        selection.target.0.clone(),
        false,
        || Ok(()),
    )?;
    let parts = context.parts.iter().collect::<Vec<_>>();
    let (native, protected, _) = crate::extfs::restore_native(file, Some(&parts), sector)?;
    if !table_restore::archive_layout(&table)?.agrees(&native) {
        return Err("源盘原始 GPT 与 Windows 原生布局不一致。".into());
    }
    Ok((table, protected))
}

fn capture_destination(
    ctx: &TableReadContext,
    file: &mut File,
    selection: &Selection,
    range: Range,
    partition: u32,
    kind: PartitionType,
) -> Result<Archive, String> {
    let table = maintenance::capture_write_table(ctx, file, &selection.target, range, true)?
        .ok_or("请选择普通 GPT 数据分区作为目标。")?;
    let layout = table_restore::archive_layout(&table)?;
    let first = range.offset / range.sector;
    if first < layout.first || first + range.size / range.sector - 1 > layout.last {
        return Err("目标范围超出 GPT 可用扇区。".into());
    }
    if kind == PartitionType::Linux {
        check_linux_native(file, &ctx.resolved, &ctx.parts, partition, range)?;
    }
    Ok(table)
}

fn check_linux_native(
    file: &File,
    resolved: &device::Resolved,
    parts: &[policy::Partition],
    partition: u32,
    range: Range,
) -> Result<(), String> {
    // An offline disk can report NULL partition-level hidden/offline flags.
    // Confirm the physical offline state as well as the raw GPT attributes.
    if resolved.disk.is_offline == Some(true) {
        device::verify_offline_handle(resolved, file)?;
    }
    let (layout, protected, _) =
        crate::extfs::restore_native(file, Some(&parts.iter().collect::<Vec<_>>()), range.sector)?;
    if !layout.entries.iter().any(|entry| {
        entry.slot == partition
            && entry.kind == policy::LINUX_DATA
            && entry.offset == range.offset
            && entry.size == range.size
            && entry.attributes == 0
            && !protected.contains(&entry.guid)
    }) {
        return Err("需要无保护属性且无 Windows 卷关联的普通 GPT Linux 数据分区。".into());
    }
    Ok(())
}

fn protect_destination(
    dst: &TableReadContext,
    partition: u32,
    range: Range,
    kind: PartitionType,
) -> Result<(), String> {
    if kind == PartitionType::Windows {
        return maintenance::protect_partitions(dst, range);
    }
    let disk = &dst.resolved.disk;
    let parts = dst.parts.iter().collect::<Vec<_>>();
    if disk.partition_style != Some(2)
        || dst.has_volume(partition)
        || !parts.iter().any(|part| {
            part.partition_number == partition
                && part.offset == range.offset
                && part.size == range.size
                && policy::linux_clone_candidate(disk, &parts, part)
        })
    {
        return Err("需要无保护属性且无 Windows 卷关联的普通 GPT Linux 数据分区。".into());
    }
    Ok(())
}

fn input<'a>(
    file: &'a File,
    source: &device::Resolved,
    kind: PartitionType,
) -> Result<Region<&'a File>, String> {
    match kind {
        PartitionType::Windows => Region::for_metadata(file, source.offset, source.size),
        PartitionType::Linux => Region::new(file, source.offset, source.size),
    }
    .map_err(device::err)
}

impl PartitionTarget {
    pub(super) fn prepare(
        destination: &Selection,
        sector: u64,
        locale: &Locale,
    ) -> Result<(Self, String), String> {
        Self::prepare_kind(destination, sector, locale, PartitionType::Windows)
    }

    pub(super) fn prepare_linux(
        destination: &Selection,
        sector: u64,
        locale: &Locale,
    ) -> Result<(Self, String), String> {
        Self::prepare_kind(destination, sector, locale, PartitionType::Linux)
    }

    fn prepare_kind(
        destination: &Selection,
        sector: u64,
        locale: &Locale,
        kind: PartitionType,
    ) -> Result<(Self, String), String> {
        let number = destination
            .partition
            .ok_or("请选择普通 GPT 数据分区作为目标。")?;
        let disk = whole(destination);
        let dst = maintenance::write_context(&disk, None)?;
        if dst.resolved.disk.logical_sector_size != Some(sector) {
            return Err("源和目标的逻辑扇区大小必须一致。".into());
        }
        let part = dst
            .parts
            .iter()
            .find(|part| part.partition_number == number)
            .ok_or("目标分区不存在")?;
        let range = Options {
            start_sector: 0,
            sector_count: part.size / sector,
        }
        .range(part.offset, part.size, sector)?;
        protect_destination(&dst, number, range, kind)?;
        let mut target = device::device(&dst.resolved, false)?;
        let table = capture_destination(&dst, &mut target, &disk, range, number, kind)?;
        let destination_label = format!(
            "{} / {}",
            dst.resolved.label,
            locale.format("分区 {partition}", &[("partition", &number.to_string())])
        );
        Ok((
            Self {
                disk,
                key: dst.fingerprint,
                range,
                number: dst.resolved.disk.number,
                partition: number,
                kind,
                table,
            },
            destination_label,
        ))
    }

    pub(super) fn write(
        &self,
        id: &str,
        cancel: &AtomicBool,
        stream: &mut (impl Read + Send),
        expected: Option<&str>,
        verify_filesystem: impl FnOnce(&mut Region<&File>) -> Result<(), String>,
        validate_source: impl Fn() -> Result<(), String>,
    ) -> Result<String, String> {
        let size = self.range.size;
        self.overwrite(
            id,
            cancel,
            |output| {
                let copied = data_io::copy_exact(stream, output, size, |n| {
                    progress(id, cancel, "写入目标", n, size)
                })?;
                output.inner_mut().sync_all().map_err(device::err)?;
                if expected.is_some_and(|digest| digest != copied) {
                    return Err("写入流的校验值与备份不一致".into());
                }
                output.seek(SeekFrom::Start(0)).map_err(device::err)?;
                let actual = data_io::hash_exact(output, size, |n| {
                    progress(id, cancel, "校验目标", n, size)
                })?;
                if copied != actual {
                    return Err("写后 SHA-256 校验失败".into());
                }
                Ok(actual)
            },
            verify_filesystem,
            validate_source,
        )
    }

    fn overwrite(
        &self,
        id: &str,
        cancel: &AtomicBool,
        write: impl FnOnce(&mut View<&mut File>) -> Result<String, String>,
        verify_filesystem: impl FnOnce(&mut Region<&File>) -> Result<(), String>,
        validate_source: impl Fn() -> Result<(), String>,
    ) -> Result<String, String> {
        let locale = crate::native_locale::current();
        let range = self.range;
        progress(id, cancel, "锁定目标", 0, range.size)?;
        let dst = maintenance::write_context(&self.disk, None)?;
        if dst.fingerprint != self.key || dst.resolved.disk.number != self.number {
            return Err("目标盘身份、布局或保护与预览不同。".into());
        }
        protect_destination(&dst, self.partition, range, self.kind)?;
        let locks = device::lock(&dst.resolved)?;
        let dst = maintenance::write_context(&self.disk, Some((&dst, &locks)))?;
        if self.kind == PartitionType::Linux {
            protect_destination(&dst, self.partition, range, self.kind)?;
        }
        validate_source()?;
        let mut target = device::exclusive_device(&dst.resolved)?;
        let verify_table = |target: &mut File| {
            if capture_destination(&dst, target, &self.disk, range, self.partition, self.kind)?
                != self.table
            {
                return Err("GPT 与预览不同".into());
            }
            Ok::<(), String>(())
        };
        verify_table(&mut target)?;
        progress(id, cancel, "锁定目标", 0, range.size)?;
        device::persist_offline(&target)
            .map_err(|error| format!("磁盘可能已脱机，尚未发送数据写入：{error}"))?;
        let result = (|| {
            device::confirm_writable_offline(&target)?;
            verify_table(&mut target)?;
            let mut output =
                View::bounded(&mut target, range.offset, range.size, dst.resolved.size)
                    .map_err(device::err)?;
            let actual = write(&mut output)?;
            let mut check = Region::for_metadata(&**output.inner_mut(), range.offset, range.size)
                .map_err(device::err)?;
            verify_filesystem(&mut check)?;
            device::confirm_writable_offline(output.inner_mut())?;
            verify_table(output.inner_mut())?;
            validate_source()?;
            Ok(actual)
        })();
        result.map_err(|error: String| {
            locale.format(
                "目标磁盘保持脱机，可能仅完成部分覆盖；未自动重试或回滚。{error}",
                &[("error", locale.text(&error))],
            )
        })
    }
}

pub(super) fn resized_plan(reader: &mut (impl Read + Seek), range: Range) -> Result<Plan, String> {
    clone::prepare(reader, range.sector, range.size, range.offset)
}

pub(super) fn linux_copy_plan(
    reader: &mut (impl Read + Seek),
    range: Range,
    cancel: &AtomicBool,
    ext_only: bool,
) -> Result<used_space::Plan, String> {
    let plan = used_space::inspect(reader, range.size, range.sector, None, cancel)?;
    let [volume] = plan.volumes.as_slice() else {
        return Err("需要可核对的 Ext2/3/4 或单设备 Btrfs 文件系统。".into());
    };
    match volume.filesystem.as_str() {
        "Ext2" | "Ext3" | "Ext4" => {
            let mut log_block_size = [0; 4];
            reader.seek(SeekFrom::Start(1024 + 0x18)).map_err(device::err)?;
            reader.read_exact(&mut log_block_size).map_err(device::err)?;
            let block_size = 1024u64.checked_shl(u32::from_le_bytes(log_block_size));
            if block_size.is_none_or(|bytes| bytes < range.sector) {
                return Err("Ext 块大小不能小于目标逻辑扇区。".into());
            }
        }
        "Btrfs" if !ext_only => {
            let mut fs = btrfs::open(&mut *reader, || used_space::stop(cancel))?;
            fs.verify_view(|| used_space::stop(cancel))?;
        }
        _ => return Err("需要可核对的 Ext2/3/4 或单设备 Btrfs 文件系统。".into()),
    }
    plan.proof.verify(reader, cancel)?;
    Ok(plan)
}

pub(super) fn resized_view<'a, R: Read + Seek>(
    plan: &'a Plan,
    reader: R,
    range: Range,
    cancel: &AtomicBool,
) -> Result<(clone::Reader<'a, R>, used_space::Plan), String> {
    let mut view = clone::Reader::new(plan, reader)?;
    let allocation = used_space::inspect(&mut view, range.size, range.sector, None, cancel)?;
    allocation.proof.verify(&mut view, cancel)?;
    let mut stream = allocation.masked(&mut view);
    plan.verify(&mut stream)?;
    view.seek(SeekFrom::Start(0)).map_err(device::err)?;
    Ok((view, allocation))
}

pub(super) fn resize_detail(locale: &Locale, plan: &Plan) -> String {
    locale.format(
        "{fs} · {old} B → {new} B · 预计搬移 {count} 个簇。按目标容量重建分配，保留活动文件和目录。确认空闲区写零，完整回读并核对 SHA-256。目标 GPT 分区表保持原样。",
        &[
            ("fs", &plan.filesystem),
            ("old", &plan.old_size.to_string()),
            ("new", &plan.new_size.to_string()),
            ("count", &plan.moved_clusters().to_string()),
        ],
    )
}

impl FilePlan {
    pub(super) fn prepare(
        reader: &mut (impl Read + Seek),
        range: Range,
        serial: u64,
        cancel: &AtomicBool,
        locale: &Locale,
    ) -> Result<(Self, String), String> {
        if ntfs::is_ntfs(reader)? {
            let plan = ntfs::Plan::prepare(reader, range, serial, cancel)?;
            let detail = locale.format(
                "NTFS → NTFS · {files} 个文件名，{directories} 个子目录，{bytes} B 数据流。重建 DISKCLONE 卷，保留文件数据、硬链接、时间、权限、8.3 短名和扩展属性。写后检查完整目录与 SHA-256，目标 GPT 保持原样。",
                &[("files", &plan.files.to_string()), ("directories", &plan.directories.to_string()),
                    ("bytes", &plan.bytes.to_string())],
            );
            return Ok((Self::Ntfs(Box::new(plan)), detail));
        }

        let serial = (serial as u32).max(1);
        let plan = file_clone::prepare(reader, range.sector, range.size, range.offset, serial, cancel)?;
        let detail = locale.format(
            "{from} → {to} · {files} 个文件，{directories} 个子目录。目标将生成 DISKCLONE 卷，连续分配活动文件，保留文件名、时间和属性，其余区域写零。目标 GPT 保持原样。",
            &[("from", plan.source_format), ("to", plan.destination_format),
                ("files", &plan.files.to_string()), ("directories", &plan.directories.to_string())],
        );
        Ok((Self::Fat { fingerprint: plan.fingerprint(), serial }, detail))
    }

    pub(super) fn write(
        &self,
        target: &PartitionTarget,
        id: &str,
        cancel: &AtomicBool,
        mut reader: impl Read + Seek + Send,
        validate_source: impl Fn() -> Result<(), String>,
    ) -> Result<String, String> {
        let range = target.range;
        match self {
            Self::Fat { fingerprint, serial } => {
                let plan = file_clone::prepare(
                    &mut reader, range.sector, range.size, range.offset, *serial, cancel,
                )?;
                check_plan(*fingerprint, plan.fingerprint())?;
                target.write(
                    id, cancel, &mut plan.reader(reader), None,
                    |check| plan.verify(check), validate_source,
                )
            }
            Self::Ntfs(plan) => {
                plan.revalidate(&mut reader, cancel)?;
                let engine = ntfs::Plan::engine()?;
                target.overwrite(
                    id, cancel,
                    |output| {
                        plan.write(
                            &engine, &mut reader, output,
                            |view| view.inner_mut().sync_all(), cancel,
                            |stage, now, total| progress(id, cancel, stage, now, total),
                        )
                    },
                    |check| plan.verify(check, cancel), validate_source,
                )
            }
        }
    }
}

pub(super) fn btrfs_view<R: Read + Seek>(
    reader: R,
    plan: &btrfs::ResizePlan,
    range: Range,
    cancel: &AtomicBool,
) -> Result<(btrfs::ProjectedResize<R>, used_space::Plan), String> {
    let mut view = btrfs::project_resize(reader, plan, || used_space::stop(cancel))?;
    let allocation = used_space::inspect(&mut view, range.size, range.sector, None, cancel)?;
    if allocation.volumes.len() != 1 || allocation.volumes[0].filesystem != "Btrfs" {
        return Err("缩放后的 Btrfs 分配信息无法核对。".into());
    }
    allocation.proof.verify(&mut view, cancel)?;
    view.seek(SeekFrom::Start(0)).map_err(device::err)?;
    Ok((view, allocation))
}

pub(super) fn verify_btrfs_copy(
    reader: &mut (impl Read + Seek),
    identity: [u8; 32],
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut volume = btrfs::open(reader, || used_space::stop(cancel))?;
    if volume.identity() != identity {
        return Err("目标 Btrfs 提交与复制计划不同。".into());
    }
    volume.verify_view(|| used_space::stop(cancel))
}

impl Clone {
    pub(super) fn prepare(
        source: Selection,
        destination: Selection,
        method: Method,
        locale: &Locale,
    ) -> Result<(Self, String, String, u64, String), String> {
        validate_pair(&source, &destination)?;
        if source.partition.is_none() {
            return Err("克隆需要选择两个普通 GPT 数据分区。".into());
        }
        let before = device::resolve(&source, true, false)?;
        let sector = before.disk.logical_sector_size.ok_or("扇区大小缺失")?;
        let (source_ctx, kind) = source_context(
            &source,
            &before,
            matches!(method, Method::Resize | Method::LinuxExact(_)),
        )?;
        if matches!(method, Method::LinuxExact(_)) && kind != PartitionType::Linux {
            return Err("需要无保护属性且无 Windows 卷关联的普通 GPT Linux 数据分区。".into());
        }
        let (target, destination_label) =
            PartitionTarget::prepare_kind(&destination, sector, locale, kind)?;
        if before.disk.number == target.number {
            return Err("源和目标必须是不同的物理磁盘。".into());
        }
        let range = target.range;
        if matches!(method, Method::Resize) && range.size == before.size {
            return Err("选择不同容量的目标分区，或改用同容量克隆方式。".into());
        }
        if matches!(method, Method::LinuxExact(_)) && range.size != before.size {
            return Err("保持布局的克隆要求源与目标容量、逻辑扇区大小完全一致。".into());
        }
        let file = device::device(&before, false)?;
        let (table, _) = capture_source(&source_ctx, &file, &source)?;
        if kind == PartitionType::Linux {
            check_linux_native(
                &file,
                &before,
                &source_ctx.parts,
                source.partition.ok_or("所选源分区已不存在。")?,
                Range {
                    offset: before.offset,
                    size: before.size,
                    sector,
                },
            )?;
        }
        let mut reader = input(&file, &before, kind)?;
        let cancel = AtomicBool::new(false);
        let (plan, detail) = match method {
            Method::LinuxExact(mode) => {
                let allocation = linux_copy_plan(&mut reader, range, &cancel, false)?;
                let filesystem = locale.format(
                    "文件系统为 {filesystem}。",
                    &[("filesystem", &allocation.volumes[0].filesystem)],
                );
                let used_space = if mode == used_space::Mode::UsedSpace {
                    locale.format(
                        "预计复制 {copied} B，跳过 {skipped} B。",
                        &[
                            ("copied", &allocation.copied().to_string()),
                            ("skipped", &allocation.skipped.to_string()),
                        ],
                    )
                } else {
                    String::new()
                };
                let detail = locale.sentences(&[
                    &filesystem,
                    &used_space,
                    "副本保留原文件系统 UUID，请与原卷分别挂载。",
                    if mode == used_space::Mode::UsedSpace {
                        "只跳过确认空闲的区域。目标空闲区保持原样，已复制范围完整回读并核对 SHA-256。目标 GPT 保持原样。"
                    } else {
                        "逐扇区复制所选分区。完整回读并核对 SHA-256，目标 GPT 保持原样。"
                    },
                ]);
                (ClonePlan::LinuxExact { proof: allocation.digest, mode }, detail)
            }
            Method::Resize if kind == PartitionType::Linux => {
                let filesystem = btrfs::plan_resize(&mut reader, range.size)?;
                let (_, allocation) = btrfs_view(reader, &filesystem, range, &cancel)?;
                let detail = locale.format(
                    "Btrfs · {old} B → {new} B · 预计搬移 {moved} B 的条带。保留卷 UUID、文件和子卷，确认空闲区写零。源卷与副本具有相同 UUID，请分别挂载。完整回读 SHA-256，目标 GPT 保持原样。",
                    &[
                        ("old", &filesystem.before().to_string()),
                        ("new", &range.size.to_string()),
                        ("moved", &filesystem.relocation_bytes().to_string()),
                    ],
                );
                (
                    ClonePlan::BtrfsResize {
                        resize: filesystem.proof(),
                        allocation: allocation.digest,
                    },
                    detail,
                )
            }
            Method::Resize => {
                let filesystem = resized_plan(&mut reader, range)?;
                resized_view(&filesystem, reader, range, &cancel)?;
                (
                    ClonePlan::Resize(filesystem.fingerprint()),
                    resize_detail(locale, &filesystem),
                )
            }
            Method::Files { serial } => {
                let (plan, detail) = FilePlan::prepare(&mut reader, range, serial, &cancel, locale)?;
                (ClonePlan::Files(plan), detail)
            }
        };
        let detail = locale.sentences(&[&detail, PARTITION_OVERWRITE_WARNING]);
        let source_label = before.label.clone();
        Ok((
            Self {
                source: Source {
                    selection: source,
                    before,
                    context_key: source_ctx.fingerprint,
                    table,
                },
                target,
                plan,
            },
            source_label,
            destination_label,
            range.size,
            detail,
        ))
    }

    pub(super) fn execute(
        self,
        id: &str,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(String, Option<String>), String> {
        let range = self.target.range;
        progress(id, cancel, "检查文件系统分配信息", 0, range.size)?;
        let source = device::resolve(&self.source.selection, true, false)?;
        if !source.same_data_binding(&self.source.before)
            || source.disk.number == self.target.number
        {
            return Err("源盘身份、布局或卷关联与预览不同。".into());
        }
        let source_locks = device::state_locks(&source)?;
        let source = device::resolve_locked(&self.source.selection, false, &source, &source_locks)?;
        let (source_ctx, kind) = source_context(
            &self.source.selection,
            &source,
            self.target.kind == PartitionType::Linux,
        )?;
        if kind != self.target.kind {
            return Err("源盘 GPT 或保护状态与预览不同。".into());
        }
        let file = device::device(&source, false)?;
        let validate_source = || {
            device::resolve_locked(&self.source.selection, false, &source, &source_locks)?;
            if capture_source(&source_ctx, &file, &self.source.selection)?.0 != self.source.table {
                return Err("源盘 GPT 在克隆期间发生变化。".into());
            }
            if kind == PartitionType::Linux {
                check_linux_native(
                    &file,
                    &source,
                    &source_ctx.parts,
                    self.source
                        .selection
                        .partition
                        .ok_or("所选源分区已不存在。")?,
                    Range {
                        offset: source.offset,
                        size: source.size,
                        sector: range.sector,
                    },
                )?;
            }
            Ok(())
        };
        if source_ctx.fingerprint != self.source.context_key {
            return Err("源盘 GPT 或保护状态与预览不同。".into());
        }
        validate_source()?;
        let mut reader = input(&file, &source, kind)?;
        let files_message = "按文件克隆完成，目标文件系统及完整 SHA-256 已核对。目标磁盘保持脱机。";
        let resized_message =
            "调整容量克隆完成，目标文件系统及完整 SHA-256 已核对。目标磁盘保持脱机。";
        let (digest, message) = match self.plan {
            ClonePlan::LinuxExact { proof, mode } => {
                let plan = linux_copy_plan(&mut reader, range, cancel, false)?;
                if plan.digest != proof {
                    return Err("源文件系统分配计划已变化，请重新预览。".into());
                }
                let verify_copy = |check: &mut Region<&File>| {
                    let actual = linux_copy_plan(check, range, cancel, false)?;
                    if actual.digest == proof {
                        Ok(())
                    } else {
                        Err("目标文件系统分配信息与源不一致。".into())
                    }
                };
                let validate_source = || {
                    validate_source()?;
                    plan.proof.verify(&mut input(&file, &source, kind)?, cancel)
                };
                if mode == used_space::Mode::UsedSpace {
                    let digest = self.target.overwrite(
                        id,
                        cancel,
                        |output| {
                            plan.copy_verified(
                                &mut reader,
                                output,
                                cancel.clone(),
                                |t| t.inner_mut().sync_all().map_err(device::err),
                                |phase, n, total| super::allocated_progress(id, cancel, phase, n, total),
                            )
                        },
                        verify_copy,
                        validate_source,
                    )?;
                    return Ok((super::allocated_summary(&plan, &crate::native_locale::current()), Some(digest)));
                }
                reader.seek(SeekFrom::Start(0)).map_err(device::err)?;
                let digest = self.target.write(id, cancel, &mut reader, None, verify_copy, validate_source)?;
                (digest, "逐扇区克隆完成，完整读回 SHA-256 一致。目标磁盘保持脱机。")
            }
            ClonePlan::Resize(fingerprint) => {
                let plan = resized_plan(&mut reader, range)?;
                check_plan(fingerprint, plan.fingerprint())?;
                let (view, allocation) = resized_view(&plan, reader, range, cancel)?;
                let digest = self.target.write(
                    id,
                    cancel,
                    &mut allocation.masked(view),
                    None,
                    |check| plan.verify(check),
                    validate_source,
                )?;
                (digest, resized_message)
            }
            ClonePlan::BtrfsResize { resize, allocation } => {
                let plan = btrfs::plan_resize(&mut reader, range.size)?;
                let (view, target_allocation) = btrfs_view(reader, &plan, range, cancel)?;
                if plan.proof() != resize || target_allocation.digest != allocation {
                    return Err("源文件系统分配计划已变化，请重新预览。".into());
                }
                let identity = view.identity();
                let digest = self.target.write(
                    id,
                    cancel,
                    &mut target_allocation.masked(view),
                    None,
                    |check| verify_btrfs_copy(check, identity, cancel),
                    validate_source,
                )?;
                (digest, resized_message)
            }
            ClonePlan::Files(plan) => (
                plan.write(&self.target, id, cancel, reader, validate_source)?,
                files_message,
            ),
        };
        Ok((
            crate::native_locale::current().text(message).into(),
            Some(digest),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::MIB;
    use sha2::{Digest, Sha256};
    use std::{io::Cursor, os::windows::fs::MetadataExt};

    #[test]
    fn linux_copy_rechecks_ext_allocation_in_both_sector_geometries() {
        let cancel = AtomicBool::new(false);
        for (name, source) in [
            ("Ext2", &include_bytes!("../../tests/fixtures/test_disk_ext2.bin.zst")[..]),
            ("Ext3", &include_bytes!("../../tests/fixtures/test_disk_ext3.bin.zst")[..]),
            ("Ext4", &include_bytes!("../../tests/fixtures/test_disk1.bin.zst")[..]),
        ] {
            let bytes = zstd::stream::decode_all(source).unwrap();
            for sector in [512, 4096] {
                let range = Range { offset: MIB, size: bytes.len() as u64, sector };
                let mut input = Cursor::new(&bytes);
                let proof = linux_copy_plan(&mut input, range, &cancel, true);
                let block_size = 1024u64 << u32::from_le_bytes(bytes[1048..1052].try_into().unwrap());
                if block_size < sector {
                    assert_eq!(proof.err().as_deref(), Some("Ext 块大小不能小于目标逻辑扇区。"));
                    continue;
                }
                let proof = proof.unwrap();
                assert_eq!(proof.volumes[0].filesystem, name);
                assert!(proof.skipped > 0);
                let mut normalized = proof.masked(Cursor::new(&bytes));
                let restored = linux_copy_plan(&mut normalized, range, &cancel, true).unwrap();
                assert_eq!(restored.digest, proof.digest);
            }
            let mut dirty = bytes;
            dirty[1024 + 0x3a..1024 + 0x3c].fill(0);
            let range = Range { offset: MIB, size: dirty.len() as u64, sector: 4096 };
            assert!(linux_copy_plan(&mut Cursor::new(dirty), range, &cancel, true).is_err());
        }
    }

    fn files(stream: &mut (impl Read + Seek)) -> ([u8; 32], usize) {
        let mut volume = btrfs::open(stream, || Ok(())).unwrap();
        let [tree] = volume.inventory().subvolumes.as_slice() else {
            panic!("Expected the fixture's file tree");
        };
        let mut entries = volume
            .read_directory(tree.id, tree.root_directory, || Ok(()))
            .unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 51);
        let mut hash = Sha256::new();
        let mut total = 0;
        for item in entries {
            assert_eq!(item.file_kind, 1);
            let mut data = Vec::new();
            volume
                .read_file(5, item.target.object_id, 384 * MIB, &mut data, || Ok(()))
                .unwrap();
            hash.update((item.name.len() as u64).to_le_bytes());
            hash.update(item.name);
            hash.update((data.len() as u64).to_le_bytes());
            hash.update(&data);
            total += data.len();
        }
        (hash.finalize().into(), total)
    }

    #[test]
    #[ignore = "requires the 384 MiB ordinary upstream Btrfs relocation image"]
    fn btrfs_clone_projection_relocates_stripes_and_preserves_real_files() {
        let path = std::env::var_os("DISKSTUDIO_TEST_BTRFS_RELOCATION_IMAGE").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file() && metadata.file_attributes() & 0x400 == 0);
        assert_eq!(metadata.len(), 384 * MIB);
        let mut source = File::open(&path).unwrap();
        let initial = data_io::hash_exact(&mut source, 384 * MIB, |_| Ok(())).unwrap();
        let contents = files(&mut source);
        assert_eq!(contents.1, 27_430_912);
        let cancel = AtomicBool::new(false);
        let same_range = Range { offset: MIB, size: 384 * MIB, sector: 4096 };
        let exact = linux_copy_plan(&mut source, same_range, &cancel, false).unwrap();
        assert_eq!(exact.volumes[0].filesystem, "Btrfs");
        assert!(exact.skipped > 0);
        let mut normalized = exact.masked(&mut source);
        assert_eq!(linux_copy_plan(&mut normalized, same_range, &cancel, false).unwrap().digest, exact.digest);
        assert_eq!(files(&mut normalized), contents);
        for (size, moved) in [(192 * MIB, 80 * MIB), (448 * MIB, 0)] {
            let plan = btrfs::plan_resize(&mut source, size).unwrap();
            assert_eq!(plan.relocation_bytes(), moved);
            let range = Range {
                offset: MIB,
                size,
                sector: 4096,
            };
            let (view, allocation) = btrfs_view(&mut source, &plan, range, &cancel).unwrap();
            assert!(allocation.skipped > 0);
            let identity = view.identity();
            let mut target = Cursor::new(Vec::with_capacity(size as usize));
            let expected =
                data_io::copy_exact(&mut allocation.masked(view), &mut target, size, |_| Ok(()))
                    .unwrap();
            assert_eq!(target.get_ref().len() as u64, size);
            assert_eq!(expected, format!("{:x}", Sha256::digest(target.get_ref())));
            assert_eq!(
                btrfs::open(&mut target, || Ok(())).unwrap().identity(),
                identity
            );
            assert_eq!(files(&mut target), contents);
        }
        source.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            data_io::hash_exact(&mut source, 384 * MIB, |_| Ok(())).unwrap(),
            initial
        );
    }
}

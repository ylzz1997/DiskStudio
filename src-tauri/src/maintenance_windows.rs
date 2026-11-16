//! Windows adapter for the maintenance batch. No inventory-triggered device I/O.
use super::*;
use crate::{
    data_tasks, data_windows as device, maintenance_io as engine, maintenance_tasks as model,
    surface_scan::Range, table_archive,
};
use model::{Operation, Preview, Report, Request, Ticket};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
    Storage::FileSystem::*,
    System::Ioctl::*,
};
struct Proof {
    request: Request,
    preview: Preview,
    fingerprint: String,
    table: Option<Vec<u8>>,
    whole_wipe: bool,
}
static PENDING: Mutex<Option<Ticket<Proof>>> = Mutex::new(None);
fn pending() -> std::sync::MutexGuard<'static, Option<Ticket<Proof>>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn discard(token: &str) {
    let mut p = pending();
    if p.as_ref().is_some_and(|p| p.token == token) {
        *p = None;
    }
}
fn canceled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("维护任务已取消。".into())
    } else {
        Ok(())
    }
}
fn random(bytes: &mut [u8]) -> Result<(), String> {
    let n = u32::try_from(bytes.len()).map_err(|_| "随机数据请求过大")?;
    // SAFETY: system RNG receives the valid writable slice and no provider handle.
    let result = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            n,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if result < 0 {
        Err(format!("系统随机数据生成失败：0x{:08x}", result as u32))
    } else {
        Ok(())
    }
}
fn whole(request: &Request) -> data_tasks::Selection {
    let mut s = request.source.clone();
    s.partition = None;
    s
}
fn range(request: &Request, ctx: &TableReadContext) -> Result<Range, String> {
    let (offset, size) = if let Some(n) = request.source.partition {
        let p = ctx
            .parts
            .iter()
            .find(|p| p.partition_number == n)
            .ok_or("目标分区不存在")?;
        (p.offset, p.size)
    } else {
        (0, ctx.resolved.size)
    };
    request.range.range(
        offset,
        size,
        ctx.resolved
            .disk
            .logical_sector_size
            .ok_or("扇区大小未知")?,
    )
}
fn overlap(a: u64, n: u64, b: u64, m: u64) -> bool {
    a < b + m && b < a + n
}
fn partition_problem(p: &Partition, d: &Disk) -> Option<String> {
    let reason = if p.gpt_type.as_deref().map(policy::guid).as_deref() != Some(policy::BASIC_DATA) {
        "不是普通 Windows 数据分区".to_string()
    } else if let Some((name, state)) = [
        ("启动", p.is_boot),
        ("系统", p.is_system),
        ("隐藏", p.is_hidden),
        ("只读", p.is_read_only),
        ("卷影副本", p.is_shadow_copy),
    ]
    .into_iter()
    .find(|(_, state)| *state != Some(false))
    {
        format!(
            "{name}属性{}",
            if state.is_none() {
                "未知"
            } else {
                "已启用"
            }
        )
    } else if !(p.is_offline == Some(false)
        || d.is_offline == Some(true) && p.is_offline == Some(true))
    {
        "分区脱机状态未知或与磁盘不一致".into()
    } else {
        return None;
    };
    Some(format!(
        "分区 {}：{reason}，不满足维护写入条件。",
        p.partition_number
    ))
}
#[cfg(test)]
fn writable_partition(p: &Partition, d: &Disk) -> bool {
    partition_problem(p, d).is_none()
}
fn context(
    request: &Request,
    old: Option<(&TableReadContext, &device::Locks)>,
) -> Result<TableReadContext, String> {
    let ctx = disk_read_context(&whole(request), old, true)?;
    let d = &ctx.resolved.disk;
    if d.is_read_only != Some(false)
        || !matches!(d.bus_type, Some(1 | 3 | 7 | 11 | 17))
        || !matches!(d.partition_style, Some(0 | 2))
        || ctx.parts.iter().any(|p| {
            matches!(
                p.gpt_type.as_deref().map(policy::guid).as_deref(),
                Some(
                    "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3"
                        | "af9b60a0-1431-4f62-bc68-3311714a69ad"
                        | "e75caf8f-f680-4cee-afa3-b001e56efc2d"
                )
            )
        })
    {
        return Err("维护写入需要可写、身份明确的普通 RAW/GPT 磁盘。".into());
    }
    let session = Session::open()?;
    let current = policy::target_disk(&session.disks, &request.source.target)?;
    if digest(current)? != digest(d)?
        || digest(&policy::layout(current, &session.partitions)?)? != digest(&ctx.parts)?
    {
        return Err("身份核对期间磁盘或布局变化".into());
    }
    device_state::access_identity(&session, current, true)?;
    let r = range(request, &ctx)?;
    match &request.operation {
        Operation::Wipe { options } => options.validate(r)?,
        Operation::WipeFree { options } => {
            let n = request
                .source
                .partition
                .ok_or("清除空闲空间需要选择一个完整数据分区。")?;
            let p = ctx
                .parts
                .iter()
                .find(|p| p.partition_number == n)
                .ok_or("分区不存在")?;
            if r.offset != p.offset || r.size != p.size {
                return Err("清除空闲空间不接受自定义扇区范围。".into());
            }
            options.validate(r)?;
        }
        Operation::Benchmark { options } => options.validate(r)?,
        Operation::Trim => {
            let n = request
                .source
                .partition
                .ok_or("TRIM 需要选择一个 NTFS 数据分区。")?;
            let p = ctx
                .parts
                .iter()
                .find(|p| p.partition_number == n)
                .ok_or("分区不存在")?;
            let v = ctx
                .volumes
                .iter()
                .find(|(i, _)| *i == n)
                .map(|(_, v)| v)
                .ok_or("卷关联缺失")?;
            if d.is_offline != Some(false)
                || !policy::formattable(p)
                || v.file_system.as_deref() != Some("NTFS")
                || r.offset != p.offset
                || r.size != p.size
            {
                return Err(
                    "TRIM 仅适用于联机的完整普通 NTFS 数据分区，不接受自定义扇区范围。".into(),
                );
            }
        }
    }
    for p in ctx
        .parts
        .iter()
        .filter(|p| overlap(r.offset, r.size, p.offset, p.size))
    {
        // No Linux NULL/hidden exception: that exception is for deletion only.
        if let Some(reason) = partition_problem(p, d) {
            return Err(reason);
        }
    }
    Ok(ctx)
}
fn capture(
    ctx: &TableReadContext,
    file: &mut File,
    request: &Request,
) -> Result<Option<Vec<u8>>, String> {
    let mut attributes = [0u8; 16];
    if device::control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attributes)? != 16
        || u32::from_le_bytes(attributes[..4].try_into().unwrap()) != 16
        || u64::from_le_bytes(attributes[8..16].try_into().unwrap()) & !1 != 0
    {
        return Err("原生磁盘只读或具有未知保护属性".into());
    }
    if ctx.resolved.disk.partition_style == Some(0) {
        if !ctx.parts.is_empty() {
            return Err("RAW 磁盘存在分区元数据".into());
        }
        let mut native = vec![0u8; 65536];
        let n = device::control(file, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut native)?;
        if n < 48
            || u32::from_le_bytes(native[..4].try_into().unwrap()) != 2
            || u32::from_le_bytes(native[4..8].try_into().unwrap()) != 0
        {
            return Err("原生设备并非无分区 RAW 磁盘".into());
        }
        return Ok(None);
    }
    let sector = ctx
        .resolved
        .disk
        .logical_sector_size
        .ok_or("扇区大小缺失")?;
    let archive = table_archive::capture(
        file,
        ctx.resolved.size,
        sector,
        request.source.target.0.clone(),
        false,
        || Ok(()),
    )?;
    let native =
        crate::extfs::restore_native(file, Some(&ctx.parts.iter().collect::<Vec<_>>()), sector)?.0;
    let layout = crate::table_restore::archive_layout(&archive)?;
    if !layout.agrees(&native) {
        return Err("原始 GPT 与 Windows 原生布局不一致。".into());
    }
    let r = range(request, ctx)?;
    for entry in layout
        .entries
        .iter()
        .filter(|e| overlap(r.offset, r.size, e.offset, e.size))
    {
        if entry.attributes & !(1u64 << 63) != 0 {
            return Err("所选范围内有受保护或未知的原生 GPT 属性。".into());
        }
    }
    let full = matches!(request.operation, Operation::Wipe { .. })
        && r.offset == 0
        && r.size == ctx.resolved.size;
    if request.operation.destructive()
        && !full
        && archive
            .extents
            .iter()
            .any(|e| overlap(r.offset, r.size, e.offset, e.bytes.len() as u64))
    {
        return Err("局部清除/写测速不能覆盖分区表；整盘清除须明确选择完整磁盘。".into());
    }
    Ok(Some(archive.encode()?))
}
fn trim_capability(file: &File) -> Result<(), String> {
    let mut query = [0u8; 12];
    query[..4].copy_from_slice(&8u32.to_le_bytes()); // StorageDeviceTrimProperty
    let mut descriptor = [0u8; 16];
    let n = device::control(file, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut descriptor)?;
    if n < 12 || u32::from_le_bytes(descriptor[4..8].try_into().unwrap()) < 12 || descriptor[8] != 1
    {
        return Err("设备未确认支持 TRIM，不能提交该操作。".into());
    }
    Ok(())
}
fn reserve<T>(
    title: &str,
    f: impl FnOnce(&str, &AtomicBool) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let (id, cancel) = data_tasks::start(title)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&id, &cancel)))
        .unwrap_or_else(|_| Err("维护任务异常；可能已部分写入或脱机，不会重试或回滚。".into()));
    if data_tasks::status().is_some_and(|j| j.id == id && j.state == "running") {
        data_tasks::finish(
            &id,
            result
                .as_ref()
                .map(|_| ("维护任务结束".into(), None))
                .map_err(Clone::clone),
        );
    }
    result
}
pub fn prepare(request: Request) -> Result<Preview, String> {
    reserve("预览磁盘维护", |id, cancel| {
        *pending() = None;
        canceled(cancel)?;
        data_tasks::record_progress(id, "核对设备身份、布局与卷关联", 0, 0);
        let (r, label, fingerprint, table, whole_wipe) = if matches!(&request.operation,Operation::Benchmark{options} if !options.write)
        {
            let resolved = device::resolve(&request.source, false, false)?;
            let r = request.range.range(
                resolved.offset,
                resolved.size,
                resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?,
            )?;
            if let Operation::Benchmark { options } = &request.operation {
                options.validate(r)?;
            }
            let _file = device::surface_device(&resolved)?;
            let fingerprint = digest(&resolved.disk)?;
            (r, resolved.label, fingerprint, None, false)
        } else {
            let ctx = context(&request, None)?;
            canceled(cancel)?;
            data_tasks::record_progress(id, "核对原生分区表与保护属性", 0, 0);
            let r = range(&request, &ctx)?;
            let mut file = device::device(&ctx.resolved, false)?;
            let table = capture(&ctx, &mut file, &request)?;
            if matches!(request.operation, Operation::Trim) {
                trim_capability(&file)?;
            }
            canceled(cancel)?;
            data_tasks::record_progress(id, "复核预览期间的设备与卷状态", 0, 0);
            let again = context(&request, None)?;
            if again.fingerprint != ctx.fingerprint {
                return Err("预览期间身份或保护变化".into());
            }
            let full = matches!(request.operation, Operation::Wipe { .. })
                && r.offset == 0
                && r.size == ctx.resolved.size;
            let label = format!(
                "{}{}",
                ctx.resolved.label,
                request
                    .source
                    .partition
                    .map(|p| format!(" · 分区 {p}"))
                    .unwrap_or_default()
            );
            (r, label, ctx.fingerprint, table, full)
        };
        canceled(cancel)?;
        let warning=match &request.operation {
            Operation::Wipe{..}=>"将永久覆盖指定范围。若覆盖文件系统或分区表，现有文件/分区会损坏。成功、取消或失败均保持整盘脱机；不自动重试、回滚或重新联机。每轮完整 SHA-256 回读；不能证明 SSD 重映射区、隐藏区域或历史物理页已清除。",
            Operation::WipeFree{..}=>"锁定后检查 NTFS、FAT12/16/32 或 exFAT 的分配信息，仅覆盖确认空闲的簇，不处理文件尾部余量、未知格式或 Ext。已删除内容将无法从已覆盖区域恢复。每轮完整回读，并比对全部保留区域的写前/写后 SHA-256。整盘保持脱机；取消或失败不重试、不回滚。逻辑覆写不能证明 SSD 历史物理页已擦除。",
            Operation::Benchmark{options} if options.write=>"这是破坏性的原始写入测速，所选范围内的数据可能损坏。整盘将脱机并保持脱机，每次写入刷新并回读 SHA-256；不会恢复原数据。QD1，无 Windows 缓存；写入耗时包括刷新，回读单独计数，设备/虚拟化缓存仍可能影响结果。",
            Operation::Benchmark{..}=>"仅读取选定范围。QD1，无 Windows 缓存；设备/虚拟化缓存、其他工作负载会影响结果，在线内容可能变化。请求次数决定读取量，可重复覆盖测量范围。",
            Operation::Trim=>"由 Windows 对所选 NTFS 卷执行 ReTrim，可能使已删除数据无法恢复。只传 ReTrim，不执行碎片整理。不保证 SSD 物理擦除或清零；提交到 Windows 后无法中途取消，提供者失败不自动重试。",
        }.into();
        let preview = Preview {
            token: uuid::Uuid::new_v4().to_string(),
            title: request.operation.title().into(),
            target: label,
            offset: r.offset,
            size: r.size,
            sector_size: r.sector,
            destructive: request.operation.destructive(),
            warning,
        };
        *pending() = Some(Ticket::new(
            preview.token.clone(),
            Proof {
                request,
                preview: preview.clone(),
                fingerprint,
                table,
                whole_wipe,
            },
        ));
        Ok(preview)
    })
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct TrimParams {
    re_trim: bool,
}
fn phase(report: &mut Report, message: &str) {
    report.phase = message.into();
    model::publish(report);
    data_tasks::record_progress(&report.id, message, 0, 0);
}
pub fn execute(token: String) -> Result<Report, String> {
    reserve("执行磁盘维护", |id, cancel| {
        let p = model::consume(&mut pending(), &token, Instant::now())?;
        let mut report = Report {
            id: id.into(),
            source: p.request.source.clone(),
            title: p.preview.title.clone(),
            target: p.preview.target.clone(),
            operation: p.request.operation.clone(),
            offset: p.preview.offset,
            size: p.preview.size,
            state: "running".into(),
            phase: "重新核对身份".into(),
            message: String::new(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            wipe: None,
            free_space: None,
            benchmark: None,
            disk_left_offline: false,
        };
        model::publish(&report);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_inner(&p, &mut report, cancel)
        }))
        .unwrap_or_else(|_| Err("维护引擎异常，可能存在部分写入。".into()));
        report.state = if result.is_ok() {
            "completed"
        } else if cancel.load(Ordering::Relaxed) {
            "canceled"
        } else {
            "failed"
        }
        .into();
        report.phase = if result.is_ok() {
            "完成"
        } else {
            "已停止"
        }
        .into();
        report.message = match &result {
            Ok(message) => message.clone(),
            Err(e) => format!(
                "{e} 不会自动重试或回滚。{}",
                if report.disk_left_offline {
                    "磁盘保持脱机；部分写入不可视为全部完成。"
                } else {
                    "请核对实际设备状态。"
                }
            ),
        };
        model::publish(&report);
        // Return the retained report on ordinary errors as well, preserving partial counts.
        data_tasks::finish(
            id,
            result
                .as_ref()
                .map(|m| (m.clone(), None))
                .map_err(Clone::clone),
        );
        Ok(report)
    })
}
fn execute_inner(p: &Proof, report: &mut Report, cancel: &AtomicBool) -> Result<String, String> {
    canceled(cancel)?;
    let request = &p.request;
    let expected = Range {
        offset: p.preview.offset,
        size: p.preview.size,
        sector: p.preview.sector_size,
    };
    if let Operation::Benchmark { options } = &request.operation {
        if !options.write {
            let resolved = device::resolve(&request.source, false, false)?;
            if digest(&resolved.disk)? != p.fingerprint
                || request
                    .range
                    .range(resolved.offset, resolved.size, expected.sector)?
                    != expected
            {
                return Err("身份或范围与预览不一致".into());
            }
            let mut file = device::surface_device(&resolved)?;
            let mut stats = engine::BenchmarkProgress::default();
            let result = engine::benchmark(
                &mut file,
                expected,
                options,
                cancel,
                &mut stats,
                random,
                |f| f.sync_all(),
                |s| {
                    report.benchmark = Some(s.clone());
                    report.phase = "无缓存读取".into();
                    model::publish(report);
                    data_tasks::record_progress(
                        &report.id,
                        &report.phase,
                        u64::from(s.operations),
                        u64::from(options.operations),
                    );
                },
            );
            report.benchmark = Some(stats);
            result?;
            if digest(&device::resolve(&request.source, false, false)?.disk)? != p.fingerprint {
                return Err("测速结束时设备状态变化，结果仅对应已读请求。".into());
            }
            return Ok(
                "读取测速完成。结果为 QD1 请求耗时统计，设备和虚拟化缓存仍会影响数值。".into(),
            );
        }
    }
    let ctx = context(request, None)?;
    if ctx.fingerprint != p.fingerprint || range(request, &ctx)? != expected {
        return Err("身份、布局或保护与预览不同".into());
    }
    canceled(cancel)?;
    phase(report, "锁定磁盘上的卷并核对保留句柄");
    let locks = device::lock(&ctx.resolved)?;
    phase(report, "复核锁定后的设备、布局与卷关联");
    let fresh = context(request, Some((&ctx, &locks)))?;
    device::verify_locks(
        &locks,
        &fresh.resolved,
        &fresh.parts.iter().collect::<Vec<_>>(),
    )?;
    canceled(cancel)?;
    if matches!(request.operation, Operation::Trim) {
        phase(report, "核对 TRIM 支持与原生分区表");
        let mut file = device::device(&fresh.resolved, false)?;
        if capture(&fresh, &mut file, request)? != p.table {
            return Err("GPT 与预览不同".into());
        }
        trim_capability(&file)?;
        drop(locks); // Windows Optimize owns its filesystem handles.
        phase(report, "提交前复核 NTFS 卷身份与保护状态");
        let final_ctx = context(request, None)?;
        if final_ctx.fingerprint != p.fingerprint {
            return Err("提交前卷身份或保护变化".into());
        }
        let n = request.source.partition.ok_or("没有目标分区")?;
        let v = final_ctx
            .volumes
            .iter()
            .find(|(i, _)| *i == n)
            .map(|(_, v)| v)
            .ok_or("目标卷未就绪")?;
        let session = Session::open()?;
        let current = policy::target_disk(&session.disks, &request.source.target)?;
        policy::corroborate(current, &session.legacy)?;
        if digest(current)? != digest(&final_ctx.resolved.disk)?
            || digest(&policy::layout(current, &session.partitions)?)? != digest(&final_ctx.parts)?
        {
            return Err("TRIM 提交前设备状态变化".into());
        }
        let part = session.partition(&request.source.target, n)?;
        if session.volume(part)? != *v {
            return Err("提交前卷关联变化".into());
        }
        canceled(cancel)?;
        phase(report, "Windows 正在执行 ReTrim（不能中途取消）");
        call::<Volume>(&session, &v.path, "Optimize", TrimParams { re_trim: true })?;
        phase(report, "核对 ReTrim 后的身份与完整分区表");
        let after = context(request, None)?;
        if after.fingerprint != p.fingerprint || capture(&after, &mut file, request)? != p.table {
            return Err("Windows 返回成功，但布局/身份写后核验失败".into());
        }
        return Ok("Windows ReTrim 返回成功，设备身份及完整 GPT 未变。这不证明介质已物理擦除或读取一定为零。".into());
    }
    // Table capture uses bounded ordinary buffers; never feed them into the
    // unbuffered benchmark handle, whose data buffers require 4096 alignment.
    phase(report, "取得写入句柄并核对原生分区表");
    let mut proof_file = device::device(&fresh.resolved, false)?;
    let allocation = if matches!(request.operation, Operation::WipeFree { .. }) {
        phase(report, "检查锁定分区的文件系统分配信息");
        let mut view = crate::used_space::View::bounded(
            &mut proof_file,
            expected.offset,
            expected.size,
            fresh.resolved.size,
        )
        .map_err(err)?;
        let plan =
            crate::used_space::inspect(&mut view, expected.size, expected.sector, None, cancel)?;
        report.free_space = Some(crate::free_space_wipe::summary(&plan)?);
        model::publish(report);
        Some(plan)
    } else {
        None
    };
    let mut file = if matches!(request.operation, Operation::Benchmark { .. }) {
        device::benchmark_device(&fresh.resolved, true)?
    } else {
        device::exclusive_device(&fresh.resolved)?
    };
    if capture(&fresh, &mut proof_file, request)? != p.table {
        return Err("锁定后 GPT 与预览不同".into());
    }
    canceled(cancel)?;
    phase(report, "将目标磁盘脱机并再次核对布局");
    device::offline(&file).map_err(|e| format!("磁盘可能已脱机，尚未发送数据写入：{e}"))?;
    report.disk_left_offline = true;
    model::publish(report);
    if capture(&fresh, &mut proof_file, request)? != p.table {
        return Err("脱机后布局变化，未写入".into());
    }
    match &request.operation {
        Operation::WipeFree { options } => {
            let plan = allocation.as_ref().ok_or("缺少已验证的空闲空间计划")?;
            let mut view = crate::used_space::View::bounded(
                &mut file,
                expected.offset,
                expected.size,
                fresh.resolved.size,
            )
            .map_err(err)?;
            let mut stats = engine::WipeProgress::default();
            let result = crate::free_space_wipe::execute(
                &mut view,
                plan,
                options,
                cancel,
                &mut stats,
                random,
                |v| v.inner_mut().sync_all(),
                |event| {
                    let (done, total) = match event {
                        crate::free_space_wipe::Progress::Preserve { after, done, total } => {
                            report.phase = if after {
                                "复核全部保留区域 SHA-256"
                            } else {
                                "记录全部保留区域 SHA-256"
                            }
                            .into();
                            (done, total)
                        }
                        crate::free_space_wipe::Progress::Wipe(s) => {
                            report.wipe = Some(s.clone());
                            report.phase = format!("空闲空间 · 第 {} 轮 · {}", s.pass, s.phase);
                            (
                                s.written_bytes + s.readback_bytes,
                                plan.skipped * u64::from(options.passes) * 2,
                            )
                        }
                    };
                    model::publish(report);
                    data_tasks::record_progress(&report.id, &report.phase, done, total);
                },
            );
            report.wipe = Some(stats);
            report.free_space = Some(result?);
        }
        Operation::Wipe { options } => {
            let mut progress = engine::WipeProgress::default();
            let result = engine::wipe(
                &mut file,
                expected,
                options,
                cancel,
                &mut progress,
                random,
                |f| f.sync_all(),
                |s| {
                    report.wipe = Some(s.clone());
                    report.phase = format!("第 {} 轮 · {}", s.pass, s.phase);
                    model::publish(report);
                    data_tasks::record_progress(
                        &report.id,
                        &report.phase,
                        s.written_bytes + s.readback_bytes,
                        expected.size * u64::from(options.passes) * 2,
                    );
                },
            );
            report.wipe = Some(progress);
            result?;
        }
        Operation::Benchmark { options } => {
            let mut stats = engine::BenchmarkProgress::default();
            let result = engine::benchmark(
                &mut file,
                expected,
                options,
                cancel,
                &mut stats,
                random,
                |f| f.sync_all(),
                |s| {
                    report.benchmark = Some(s.clone());
                    report.phase = "写入、刷新与回读".into();
                    model::publish(report);
                    data_tasks::record_progress(
                        &report.id,
                        &report.phase,
                        u64::from(s.operations),
                        u64::from(options.operations),
                    );
                },
            );
            report.benchmark = Some(stats);
            result?;
        }
        Operation::Trim => unreachable!(),
    }
    phase(report, "核对写入后的完整分区表与脱机状态");
    if !p.whole_wipe && capture(&fresh, &mut proof_file, request)? != p.table {
        return Err("数据写入已完成，但完整 GPT 核验不一致".into());
    }
    let mut attrs = [0u8; 16];
    if device::control(&file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attrs)? < 16
        || u64::from_le_bytes(attrs[8..16].try_into().unwrap()) & 1 == 0
    {
        return Err("写后不能确认磁盘保持脱机".into());
    }
    Ok(
        "所选操作完成，全部要求的 SHA-256 回读通过。磁盘保持脱机；未自动恢复原内容或重新联机。"
            .into(),
    )
}
pub fn export(id: &str) -> Result<Option<String>, String> {
    reserve("保存磁盘维护报告", |_, cancel| {
        let report = model::status()
            .filter(|r| r.id == id && r.state != "running")
            .ok_or("报告尚未完成或已被替换")?;
        let Some(path) = rfd::FileDialog::new()
            .set_title("保存维护报告到另一块磁盘（新文件名）")
            .set_file_name("DiskStudio-maintenance.json")
            .add_filter("JSON 报告", &["json"])
            .save_file()
        else {
            return Ok(None);
        };
        let parent = path.parent().ok_or("缺少输出目录")?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("文件名无效")?;
        if name.is_empty()
            || name.ends_with(['.', ' '])
            || name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
            || crate::recovery_export::reserved(name)
        {
            return Err("报告文件名无效".into());
        }
        let dir = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())
            .map_err(err)?;
        let location = device::file_location(&dir)?;
        if location.target == report.source.target {
            return Err("报告必须保存到另一块物理磁盘".into());
        }
        let pinned = dir.try_clone().map_err(err)?.into_std_file();
        let binding = device::FileBinding::new(&pinned, &location.target)?;
        device::FileBinding::validate_all(std::slice::from_ref(&binding))?;
        canceled(cancel)?;
        let bytes=serde_json::to_vec_pretty(&serde_json::json!({"format":"DiskStudio maintenance report","version":1,"result":report})).map_err(err)?;
        use cap_std::fs::OpenOptionsExt;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ);
        let mut file = dir.open_with(name, &options).map_err(err)?.into_std();
        let output = device::FileBinding::new(&file, &location.target)?;
        let bindings = [binding, output];
        device::FileBinding::validate_all(&bindings)?;
        file.write_all(&bytes).map_err(err)?;
        file.sync_all().map_err(err)?;
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut back = vec![0; bytes.len()];
        file.read_exact(&mut back).map_err(err)?;
        if Sha256::digest(&back) != Sha256::digest(&bytes) {
            return Err("报告回读失败，文件不能当作成功结果".into());
        }
        device::FileBinding::validate_all(&bindings)?;
        Ok(Some(path.display().to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trim_serialization_cannot_enable_other_optimizations() {
        assert_eq!(
            serde_json::to_value(TrimParams { re_trim: true }).unwrap(),
            serde_json::json!({"ReTrim":true})
        );
    }
    #[test]
    fn maintenance_write_protection_rejects_every_unknown_and_protected_partition_flag() {
        let mut d = Disk {
            is_offline: Some(false),
            ..Default::default()
        };
        let p = Partition {
            gpt_type: Some(policy::BASIC_DATA.into()),
            is_boot: Some(false),
            is_system: Some(false),
            is_hidden: Some(false),
            is_read_only: Some(false),
            is_shadow_copy: Some(false),
            is_offline: Some(false),
            ..Default::default()
        };
        assert!(writable_partition(&p, &d));
        for i in 0..6 {
            for value in [None, Some(true)] {
                let mut bad = p.clone();
                *match i {
                    0 => &mut bad.is_boot,
                    1 => &mut bad.is_system,
                    2 => &mut bad.is_hidden,
                    3 => &mut bad.is_read_only,
                    4 => &mut bad.is_shadow_copy,
                    _ => &mut bad.is_offline,
                } = value;
                assert!(!writable_partition(&bad, &d));
            }
        }
        for kind in [
            policy::LINUX_DATA,
            policy::LINUX_SWAP,
            "unknown",
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
            "e3c9e316-0b5c-4db8-817d-f92df00215ae",
        ] {
            let mut bad = p.clone();
            bad.gpt_type = Some(kind.into());
            assert!(!writable_partition(&bad, &d));
        }
        let mut off = p;
        off.is_offline = Some(true);
        d.is_offline = Some(true);
        assert!(writable_partition(&off, &d));
    }
    #[test]
    fn adjacent_ranges_are_not_overlapping() {
        assert!(!overlap(4096, 4096, 0, 4096));
        assert!(!overlap(4096, 4096, 8192, 4096));
        assert!(overlap(4096, 4096, 8191, 1));
        assert!(overlap(4096, 4096, 0, 4097));
    }
}

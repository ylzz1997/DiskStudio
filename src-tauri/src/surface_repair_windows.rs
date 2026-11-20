use super::*;
use crate::{
    data_tasks::{self, Selection},
    data_windows as device,
    maintenance_tasks::{self, Ticket},
    partitions::platform::{maintenance, TableReadContext},
    surface_tasks::{self, Snapshot},
    table_archive::Archive,
    table_restore::Layout,
};
use std::{fs::File, sync::Mutex, time::Instant};

type Table = Option<(Archive, Layout)>;

struct Plan {
    scan_id: String,
    geometry: Geometry,
    fingerprint: String,
    table: Table,
    blocks: Vec<(Group, BlockProof)>,
}

static PENDING: Mutex<Option<Ticket<Plan>>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Ticket<Plan>>> {
    PENDING.lock().unwrap_or_else(|err| err.into_inner())
}

pub fn discard(token: String) {
    let mut state = pending();
    if state.as_ref().is_some_and(|ticket| ticket.token == token) {
        *state = None;
    }
}

fn context(
    source: &Selection,
    snapshot: &Snapshot,
) -> Result<(Selection, TableReadContext), String> {
    let selected = device::resolve(source, false, false)?;
    if snapshot.options.range(
        selected.offset,
        selected.size,
        selected.disk.logical_sector_size.ok_or("扇区大小缺失")?,
    )? != snapshot.report.range()
    {
        return Err("检测范围已变化，请重新检测。".into());
    }
    let whole = Selection {
        partition: None,
        ..source.clone()
    };
    let ctx = maintenance::write_context(&whole, None)?;
    if selected.disk != ctx.resolved.disk {
        return Err("检测范围与写入目标的磁盘状态不一致。".into());
    }
    Ok((whole, ctx))
}

fn geometry(ctx: &TableReadContext, file: &File) -> Result<Geometry, String> {
    let (physical, offset) = device::physical_alignment(&ctx.resolved, file)?;
    Ok(Geometry {
        logical: ctx
            .resolved
            .disk
            .logical_sector_size
            .ok_or("扇区大小缺失")?,
        physical,
        offset,
    })
}

fn check_range(ctx: &TableReadContext, table: &Table, range: Range) -> Result<(), String> {
    maintenance::protect_partitions(ctx, range)?;
    if let Some((archive, layout)) = table {
        maintenance::protect_write_range(archive, layout, range, true)?;
    }
    Ok(())
}

pub fn prepare(scan_id: String) -> Result<Preview, String> {
    maintenance::reserve("预览坏道修复", |id, cancel| {
        *pending() = None;
        let (source, snapshot) = surface_tasks::repair_source(&scan_id)?;
        let (whole, ctx) = context(&source, &snapshot)?;
        canceled(cancel)?;
        let mut proof_file = device::device(&ctx.resolved, false)?;
        let geometry = geometry(&ctx, &proof_file)?;
        let table = maintenance::write_table(&ctx, &mut proof_file, &whole.target)?;
        let (candidates, mut excluded_sectors) = groups(&snapshot.report, geometry)?;
        let (mut reader, _) =
            device::surface_scan_device(&ctx.resolved, snapshot.report.read_timeout_seconds)?;
        let total = candidates.len() as u64;
        let mut blocks = Vec::new();
        let mut now_readable_sectors = 0;
        for (index, group) in candidates.into_iter().enumerate() {
            canceled(cancel)?;
            data_tasks::record_progress(id, "复读候选扇区", index as u64, total);
            let count = group.candidate_count();
            if check_range(&ctx, &table, group.range).is_err() {
                excluded_sectors += count;
                continue;
            }
            let Some((proof, _)) = observe(&mut reader, &group, cancel)? else {
                excluded_sectors += count;
                continue;
            };
            now_readable_sectors += count - proof.failure_count();
            if proof.failure_count() > 0 {
                blocks.push((group, proof));
            }
        }
        canceled(cancel)?;
        if blocks.is_empty() {
            return Err("复读后没有符合范围和保护条件的扇区可修复。".into());
        }
        if maintenance::write_context(&whole, None)?.fingerprint != ctx.fingerprint
            || maintenance::write_table(&ctx, &mut proof_file, &whole.target)? != table
        {
            return Err("预览期间磁盘、分区或卷信息发生变化。".into());
        }
        let preview = Preview {
            token: uuid::Uuid::new_v4().to_string(),
            target: ctx.resolved.label,
            logical_sector_size: geometry.logical,
            physical_sector_size: geometry.physical,
            excluded_sectors,
            now_readable_sectors,
            blocks: blocks.iter().map(|(_, proof)| proof.clone()).collect(),
        };
        *pending() = Some(Ticket::new(
            preview.token.clone(),
            Plan {
                scan_id,
                geometry,
                fingerprint: ctx.fingerprint,
                table,
                blocks,
            },
        ));
        Ok(preview)
    })
}

pub fn execute(token: String) -> Result<Outcome, String> {
    maintenance::reserve("写入修复坏道", |id, cancel| {
        let plan = maintenance_tasks::consume(&mut pending(), &token, Instant::now())?;
        let mut outcome = Outcome {
            state: "running",
            planned_blocks: plan.blocks.len(),
            attempted_blocks: 0,
            verified_blocks: Vec::new(),
            disk_left_offline: None,
            message: String::new(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_plan(&plan, &mut outcome, id, cancel)
        }))
        .unwrap_or_else(|_| Err("修复意外停止，请核对磁盘状态及已尝试的范围。".into()));
        outcome.state = match &result {
            Ok(()) => "completed",
            Err(_) if cancel.load(Ordering::Relaxed) => "canceled",
            Err(_) => "failed",
        };
        outcome.message = match &result {
            Ok(()) => "已完成写入及 SHA-256 回读核对。磁盘保持脱机。".into(),
            Err(message) => message.clone(),
        };
        data_tasks::finish(id, result.map(|_| (outcome.message.clone(), None)));
        Ok(outcome)
    })
}

fn execute_plan(
    plan: &Plan,
    outcome: &mut Outcome,
    id: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (source, snapshot) = surface_tasks::repair_source(&plan.scan_id)?;
    let (whole, ctx) = context(&source, &snapshot)?;
    if ctx.fingerprint != plan.fingerprint {
        return Err("身份、布局或保护状态与预览不一致。".into());
    }
    canceled(cancel)?;
    let locks = device::lock(&ctx.resolved)?;
    let fresh = maintenance::write_context(&whole, Some((&ctx, &locks)))?;
    device::verify_locks(
        &locks,
        &fresh.resolved,
        &fresh.parts.iter().collect::<Vec<_>>(),
    )?;
    let mut proof_file = device::device(&fresh.resolved, false)?;
    let mut writer = device::benchmark_device(&fresh.resolved, true)?;
    if geometry(&fresh, &writer)? != plan.geometry
        || maintenance::write_table(&fresh, &mut proof_file, &whole.target)? != plan.table
    {
        return Err("设备对齐或原生分区表与预览不一致。".into());
    }
    for (group, _) in &plan.blocks {
        check_range(&fresh, &plan.table, group.range)?;
    }
    let (mut reader, _) =
        device::surface_scan_device(&fresh.resolved, snapshot.report.read_timeout_seconds)?;
    // Recheck every block while locked before taking the disk offline.
    for (group, expected) in &plan.blocks {
        canceled(cancel)?;
        recheck(&mut reader, group, expected, cancel)?;
    }
    canceled(cancel)?;
    surface_tasks::mark_repair_used(&plan.scan_id);
    let offline_result = device::offline(&writer);
    outcome.disk_left_offline = Some(device::confirm_writable_offline(&writer).is_ok());
    offline_result?;
    device::confirm_writable_offline(&writer)?;
    let table_matches = |file: &mut File| -> Result<(), String> {
        if maintenance::write_table(&fresh, file, &whole.target)? != plan.table {
            return Err("原生分区表或保护状态与确认记录不一致。".into());
        }
        Ok(())
    };
    table_matches(&mut proof_file)?;
    let work = (|| {
        let total = plan.blocks.len() as u64;
        for (group, expected) in &plan.blocks {
            canceled(cancel)?;
            data_tasks::record_progress(
                id,
                "写入并回读",
                outcome.verified_blocks.len() as u64,
                total,
            );
            let image = recheck(&mut reader, group, expected, cancel)?;
            device::confirm_writable_offline(&writer)?;
            write_verified(
                &mut writer,
                &mut reader,
                group,
                expected,
                &image,
                |file| file.sync_all(),
                cancel,
                &mut outcome.attempted_blocks,
            )?;
            outcome.verified_blocks.push(expected.clone());
        }
        Ok::<(), String>(())
    })();
    let disk_state = device::confirm_writable_offline(&writer);
    outcome.disk_left_offline = Some(disk_state.is_ok());
    let table_state = table_matches(&mut proof_file);
    match (work, disk_state.and(table_state)) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(work), Err(proof)) => Err(format!("{work} 写后核验失败：{proof}")),
    }
}

fn recheck(
    reader: &mut impl ScanSource,
    group: &Group,
    expected: &BlockProof,
    cancel: &AtomicBool,
) -> Result<Aligned, String> {
    match observe(reader, group, cancel)? {
        Some((proof, image)) if proof == *expected => Ok(image),
        _ => Err(format!(
            "LBA {} 的读取失败状态或相邻数据已变化。",
            expected.start_lba
        )),
    }
}

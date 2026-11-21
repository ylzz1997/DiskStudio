use crate::{
    data_tasks::Selection,
    table_diagnostics::{self, Snapshot},
};
use disk_core::table_check::{CheckState, TableReport};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub id: String,
    pub target: String,
    pub report: TableReport,
    pub retained_bytes: u64,
    pub sha256: String,
    pub repair_available: bool,
    pub repair_note: String,
}
#[derive(Clone)]
pub(crate) struct Saved {
    pub source: Selection,
    pub snapshot: Snapshot,
    pub report: Report,
    pub fingerprint: String,
}
static SAVED: Mutex<Option<Saved>> = Mutex::new(None);
fn state() -> std::sync::MutexGuard<'static, Option<Saved>> {
    SAVED.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn discard(id: String) {
    let mut s = state();
    if s.as_ref().is_some_and(|s| s.report.id == id) {
        *s = None;
    }
}
pub(crate) fn saved(id: &str) -> Result<Saved, String> {
    state()
        .as_ref()
        .filter(|s| s.report.id == id)
        .cloned()
        .ok_or("分区表检查已失效，请重新检查。".into())
}
pub fn inspect(request: Request) -> Result<Report, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("设备分区表检查当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        use crate::{data_windows as device, partitions::platform};
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        *state() = None;
        let (id, cancel) = crate::data_tasks::start("检查分区表")?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<Report, String> {
                let check = || {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        Err("已取消分区表检查。".into())
                    } else {
                        Ok(())
                    }
                };
                let ctx = platform::table_inspect_context(&request.source)?;
                check()?;
                let mut file = device::device(&ctx.resolved, false)?;
                let sector = ctx
                    .resolved
                    .disk
                    .logical_sector_size
                    .ok_or("逻辑扇区未知")?;
                let snapshot =
                    table_diagnostics::inspect(&mut file, ctx.resolved.size, sector, &check)?;
                let mut report = snapshot.report.clone();
                report.filename = ctx.resolved.label.clone();
                let expected = match ctx.resolved.disk.partition_style {
                    Some(1) => "MBR",
                    Some(2) => "GPT",
                    _ => "RAW",
                };
                let mut corroborated = false;
                if report.scheme != expected {
                    table_diagnostics::add(
                        &mut report,
                        "windows-layout",
                        "Windows 布局核对",
                        CheckState::Failed,
                        format!(
                            "Windows 报告 {expected}，读取到的结构为 {}，不允许修复。",
                            snapshot.report.scheme
                        ),
                    );
                } else if expected == "GPT" {
                    let actual = crate::extfs::restore_native(
                        &file,
                        Some(&ctx.parts.iter().collect::<Vec<_>>()),
                        sector,
                    );
                    match actual.and_then(|n| snapshot.layout().map(|l| l.agrees(&n.0))) {
                        Ok(true) => {
                            corroborated = true;
                            table_diagnostics::add(
                                &mut report,
                                "windows-layout",
                                "Windows 布局核对",
                                CheckState::Passed,
                                "有效 GPT 的持久字段与原生设备句柄及完整 Windows 分区清单一致。"
                                    .into(),
                            );
                        }
                        Ok(false) => table_diagnostics::add(
                            &mut report,
                            "windows-layout",
                            "Windows 布局核对",
                            CheckState::Failed,
                            "有效 GPT 与 Windows 原生布局不一致，不允许修复。".into(),
                        ),
                        Err(e) => table_diagnostics::add(
                            &mut report,
                            "windows-layout",
                            "Windows 布局核对",
                            CheckState::Unchecked,
                            e,
                        ),
                    }
                } else if let Some(ranges) = &snapshot.mbr_ranges {
                    let mut windows: Vec<_> =
                        ctx.parts.iter().map(|p| (p.offset, p.size)).collect();
                    windows.sort_unstable();
                    let (state, detail) = if *ranges == windows && snapshot.mbr_container == ctx.mbr_container {
                        (
                            CheckState::Passed,
                            "MBR 与 EBR 的数据分区范围和 Windows 分区清单一致。",
                        )
                    } else {
                        (
                            CheckState::Failed,
                            "MBR 或 EBR 的数据分区范围与 Windows 分区清单不一致。",
                        )
                    };
                    table_diagnostics::add(
                        &mut report,
                        "windows-layout",
                        "Windows 布局核对",
                        state,
                        detail.into(),
                    );
                }
                if platform::table_inspect_context(&request.source)?.fingerprint != ctx.fingerprint
                {
                    return Err("检查期间 Windows 磁盘或分区状态变化。".into());
                }
                snapshot.matches(&mut file, &check)?;
                let d = &ctx.resolved.disk;
                let repair = crate::disk_identity::key(
                    d.serial_number.as_deref(),
                    None,
                    d.friendly_name.as_deref(),
                    d.size,
                    d.logical_sector_size,
                    d.bus_type,
                )
                .ok_or("缺少可核验的磁盘身份。".into())
                .and_then(|identity| snapshot.repair(&identity));
                let (repair_available, repair_note) = match repair {
                    Ok(Some(p))
                        if corroborated
                            && ctx.resolved.disk.is_boot == Some(false)
                            && ctx.resolved.disk.is_system == Some(false)
                            && ctx.resolved.disk.is_read_only == Some(false) =>
                    {
                        (true, p.description)
                    }
                    Ok(Some(_)) => (
                        false,
                        "当前磁盘受保护或无法与 Windows 布局核对，不能生成修复操作。".into(),
                    ),
                    Ok(None) => (false, "主备 GPT 完整且一致，无需修复。".into()),
                    Err(e) => (false, e),
                };
                let result = Report {
                    id: uuid::Uuid::new_v4().to_string(),
                    target: ctx.resolved.label,
                    report,
                    retained_bytes: snapshot.retained_bytes(),
                    sha256: snapshot.digest(),
                    repair_available,
                    repair_note,
                };
                *state() = Some(Saved {
                    source: request.source,
                    snapshot,
                    report: result.clone(),
                    fingerprint: ctx.fingerprint,
                });
                Ok(result)
            },
        ))
        .unwrap_or_else(|_| Err("分区表检查异常结束；未写入设备。".into()));
        crate::data_tasks::finish(
            &id,
            result
                .as_ref()
                .map(|_| ("分区表检查完成".into(), None))
                .map_err(Clone::clone),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_ipc_accepts_only_selection_and_repair_accepts_only_check_id() {
        let good = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":null}});
        assert!(serde_json::from_value::<Request>(good.clone()).is_ok());
        for name in ["path", "device", "method", "bytes", "repair"] {
            let mut bad = good.clone();
            bad[name] = serde_json::json!("x");
            assert!(serde_json::from_value::<Request>(bad).is_err());
            let bad = serde_json::json!({"checkId":"known",name:"x"});
            assert!(
                serde_json::from_value::<crate::table_restore_task::RepairRequest>(bad).is_err()
            );
        }
        assert!(
            serde_json::from_value::<crate::table_restore_task::RepairRequest>(
                serde_json::json!({"checkId":"known"})
            )
            .is_ok()
        );
        assert!(saved("unissued").is_err());
    }
}

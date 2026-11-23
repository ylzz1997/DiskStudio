//! Confirm an exact offline-disk BCD patch and save the original before writing.
use super::Selection;
use crate::boot_inspection::repair::{Change, Summary};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    source: Selection,
    change: Change,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    token: Uuid,
    summary: Summary,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    backup_path: String,
    summary: Summary,
}

#[cfg(windows)]
pub(crate) use windows::{apply, discard, preview};

#[cfg(not(windows))]
pub(crate) fn preview(_: Request) -> Result<Preview, String> {
    Err("该操作需要 Windows。".into())
}

#[cfg(not(windows))]
pub(crate) fn apply(_: Uuid) -> Result<Option<Report>, String> {
    Err("该操作需要 Windows。".into())
}

#[cfg(not(windows))]
pub(crate) fn discard(_: Uuid) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_ipc_only_accepts_identifiers_and_expected_digest() {
        let request = serde_json::json!({
            "source":{"target":"a".repeat(64),"layoutKey":"layout","partition":null},
            "change":{
                "bootPartition":Uuid::from_u128(1),
                "storeSha256":"b".repeat(64),
                "entry":Uuid::from_u128(2),
                "windowsPartition":Uuid::from_u128(3),
                "makeDefault":true
            }
        });
        assert!(serde_json::from_value::<Request>(request.clone()).is_ok());
        for field in ["path", "offset", "replacement", "method"] {
            let mut unexpected = request.clone();
            unexpected["change"][field] = serde_json::json!("arbitrary value");
            assert!(
                serde_json::from_value::<Request>(unexpected).is_err(),
                "{field}"
            );
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{
        backup_files,
        boot_inspection::repair::{self, Geometry, Plan},
        data_tasks::{finish, idle, record_progress, start, state},
        data_windows::{self as platform, Resolved},
        sector_edit,
        used_space::View,
    };
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex, MutexGuard,
        },
        time::{Duration, Instant},
    };

    struct Pending {
        request: Request,
        before: Resolved,
        fingerprint: [u8; 32],
        token: Uuid,
        expires: Instant,
    }

    static PREVIEW: Mutex<Option<Pending>> = Mutex::new(None);

    fn pending() -> MutexGuard<'static, Option<Pending>> {
        PREVIEW.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn discard(token: Uuid) {
        let mut slot = pending();
        if slot.as_ref().is_some_and(|preview| preview.token == token) {
            slot.take();
        }
    }

    fn take(token: Uuid) -> Result<Pending, String> {
        let mut slot = pending();
        let preview = slot
            .take_if(|preview| preview.token == token)
            .filter(|preview| preview.expires > Instant::now())
            .ok_or("BCD 修复预览已失效，请重新预览。")?;
        Ok(preview)
    }

    fn resolve(selection: &Selection) -> Result<Resolved, String> {
        if selection.partition.is_some() {
            return Err("请选择经核对的脱机 GPT 整盘。".into());
        }
        let bound = platform::resolve(selection, true, true)?;
        if bound.disk.is_offline != Some(true) || bound.disk.partition_style != Some(2) {
            return Err("请选择经核对的脱机 GPT 整盘。".into());
        }
        Ok(bound)
    }

    fn plan(file: &File, bound: &Resolved, change: &Change) -> Result<Plan, String> {
        platform::verify_offline_handle(bound, file)?;
        let (physical, alignment) = platform::physical_alignment(bound, file)?;
        let geometry = Geometry {
            size: bound.size,
            sector: bound.disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?,
            physical,
            alignment,
        };
        let view = View::bounded(file, 0, bound.size, bound.disk.size).map_err(platform::err)?;
        let plan = repair::plan(view, geometry, &bound.partition_ranges, change)?;
        bound.verify_boot_repair_layout(file, &plan.layout, change.boot_partition)?;
        Ok(plan)
    }

    pub(crate) fn preview(request: Request) -> Result<Preview, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        pending().take();
        let initial = resolve(&request.source)?;
        let locks = platform::lock(&initial)?;
        let bound = platform::resolve_locked(&request.source, true, &initial, &locks)?;
        let plan = plan(&platform::device(&bound, false)?, &bound, &request.change)?;
        platform::resolve_locked(&request.source, true, &bound, &locks)?;
        let token = Uuid::new_v4();
        *pending() = Some(Pending {
            request,
            before: bound,
            fingerprint: plan.fingerprint,
            token,
            expires: Instant::now() + Duration::from_secs(300),
        });
        Ok(Preview {
            token,
            summary: plan.summary,
        })
    }

    fn save_backup(
        path: &Path,
        selection: &Selection,
        bound: &Resolved,
        plan: &Plan,
    ) -> Result<File, String> {
        let (mut file, publication) = backup_files::create_bcd_backup(
            path,
            &selection.target,
            bound.disk.number,
            plan.original.len() as u64,
        )?;
        file.write_all(&plan.original)
            .and_then(|_| file.sync_all())
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .map_err(platform::err)?;
        let mut readback = vec![0; plan.original.len()];
        file.read_exact(&mut readback).map_err(platform::err)?;
        if file.metadata().map_err(platform::err)?.len() != readback.len() as u64
            || sector_edit::hash(&readback) != plan.summary.before_sha256
        {
            return Err("原始 BCD 备份未通过 SHA-256 回读校验。".into());
        }
        publication.publish()?;
        Ok(file)
    }

    fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消 BCD 修复。".into());
        }
        Ok(())
    }

    fn execute(pending: Pending, id: &str, cancel: &AtomicBool) -> Result<Option<Report>, String> {
        let Pending {
            request: Request { source, change },
            before,
            fingerprint,
            ..
        } = pending;
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("将原始 BCD 备份到另一块磁盘"))
            .add_filter("BCD", &["bcd"])
            .set_file_name("BCD-before-repair.bcd")
            .save_file()
        else {
            return Ok(None);
        };
        check_cancel(cancel)?;
        let current = resolve(&source)?;
        if !before.same_data_binding(&current) {
            return Err("磁盘布局或保护状态已变化，请重新预览。".into());
        }
        let locks = platform::lock(&current)?;
        let bound = platform::resolve_locked(&source, true, &current, &locks)?;

        // A read-only pass verifies the preview and provides the original BCD.
        let initial = plan(&platform::device(&bound, false)?, &bound, &change)?;
        if initial.fingerprint != fingerprint {
            return Err("BCD 或目标内容已变化，请重新检查并预览。".into());
        }
        check_cancel(cancel)?;
        record_progress(id, "将原始 BCD 备份到另一块磁盘", 0, 0);
        // Keep the verified backup handle until the transaction has finished.
        let _backup = save_backup(&path, &source, &bound, &initial)?;
        let backup_path = path.display().to_string();

        let mut started = false;
        let result = (|| {
            check_cancel(cancel)?;
            let bound = platform::resolve_locked(&source, true, &bound, &locks)?;
            let file = platform::exclusive_device(&bound)?;
            let patch = plan(&file, &bound, &change)?;
            if patch.fingerprint != fingerprint {
                return Err("BCD 或目标内容已变化，请重新检查并预览。".into());
            }
            let mut writer =
                View::bounded(file, 0, bound.size, bound.disk.size).map_err(platform::err)?;
            let result = patch.write(
                &mut writer,
                || {
                    platform::resolve_locked(&source, true, &bound, &locks)?;
                    check_cancel(cancel)?;
                    record_progress(id, "正在写入并回读校验 BCD…", 0, 0);
                    started = true;
                    Ok(())
                },
                |view| view.inner_mut().sync_all(),
            );
            let verified_state = platform::resolve_locked(&source, true, &bound, &locks);
            result?;
            verified_state?.verify_boot_repair_layout(
                writer.inner_mut(),
                &patch.layout,
                change.boot_partition,
            )?;
            Ok(patch.summary)
        })();
        let summary = result.map_err(|error: String| {
            let locale = crate::native_locale::current();
            locale.format(
                if started {
                    "{error} 修复可能只完成了一部分。原始 BCD 已备份至 {path}。"
                } else {
                    "{error} 原始 BCD 已备份至 {path}。"
                },
                &[("error", locale.text(&error)), ("path", &backup_path)],
            )
        })?;
        Ok(Some(Report {
            backup_path,
            summary,
        }))
    }

    pub(crate) fn apply(token: Uuid) -> Result<Option<Report>, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let pending = take(token)?;
        let (id, cancel) = start("修复 BCD 引用")?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(pending, &id, &cancel)
        }))
        .unwrap_or_else(|_| Err("BCD 修复异常结束，请检查磁盘和选定的备份文件。".into()));
        finish(
            &id,
            result
                .as_ref()
                .map(|report| {
                    report.as_ref().map_or_else(
                        || ("已取消 BCD 修复。".into(), None),
                        |report| {
                            (
                                "BCD 修复完成，已通过 SHA-256 回读校验。".into(),
                                Some(report.summary.after_sha256.clone()),
                            )
                        },
                    )
                })
                .map_err(Clone::clone),
        );
        result
    }
}

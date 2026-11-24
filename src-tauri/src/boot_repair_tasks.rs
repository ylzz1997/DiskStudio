//! Confirm offline Windows boot repairs and verify backups before writing.
use super::Selection;
use crate::boot_inspection::repair::{files, Change, Summary as BcdSummary};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    source: Selection,
    change: Change,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FileRequest {
    source: Selection,
    change: files::Change,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Summary {
    Bcd(BcdSummary),
    Files(files::Summary),
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
pub(crate) use windows::{apply, discard, preview, preview_files};

#[cfg(not(windows))]
pub(crate) fn preview(_: Request) -> Result<Preview, String> {
    Err("该操作需要 Windows。".into())
}

#[cfg(not(windows))]
pub(crate) fn preview_files(_: FileRequest) -> Result<Preview, String> {
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

    #[test]
    fn file_repair_accepts_partition_ids_and_a_fallback_choice() {
        let request = serde_json::json!({
            "source":{"target":"a".repeat(64),"layoutKey":"layout","partition":null},
            "change":{
                "bootPartition":Uuid::from_u128(1),
                "windowsPartition":Uuid::from_u128(2),
                "rebuildFallback":true
            }
        });
        assert!(serde_json::from_value::<FileRequest>(request.clone()).is_ok());
        let mut invalid = request;
        invalid["change"]["sourceFile"] = serde_json::json!("another.efi");
        assert!(serde_json::from_value::<FileRequest>(invalid).is_err());
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{
        backup_files,
        boot_inspection::repair::{self, Geometry},
        data_tasks::{finish, idle, record_progress, start, state},
        data_windows::{self as platform, Resolved},
        sector_edit,
        used_space::View,
    };
    use std::{
        fs::File,
        io::{Cursor, Read, Seek, SeekFrom, Write},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex, MutexGuard,
        },
        time::{Duration, Instant},
    };

    struct Pending {
        source: Selection,
        choice: Choice,
        before: Resolved,
        fingerprint: [u8; 32],
        token: Uuid,
        expires: Instant,
    }

    enum Choice {
        Bcd(Change),
        Files(files::Change),
    }

    impl Choice {
        fn boot_partition(&self) -> Uuid {
            match self {
                Self::Bcd(change) => change.boot_partition,
                Self::Files(change) => change.boot_partition,
            }
        }

        fn title(&self) -> &'static str {
            match self {
                Self::Bcd(_) => "修复 BCD 引用",
                Self::Files(_) => "恢复 Windows EFI 启动文件",
            }
        }

        fn backup_title(&self) -> &'static str {
            match self {
                Self::Bcd(_) => "将原始 BCD 备份到另一块磁盘",
                Self::Files(_) => "将原始 EFI 文件备份到另一块磁盘",
            }
        }
    }

    enum Plan {
        Bcd(repair::Plan),
        Files(files::Plan),
    }

    impl Plan {
        fn layout(&self) -> &disk_core::table_check::GptIdentity {
            match self {
                Self::Bcd(plan) => &plan.layout,
                Self::Files(plan) => &plan.layout,
            }
        }

        fn fingerprint(&self) -> [u8; 32] {
            match self {
                Self::Bcd(plan) => plan.fingerprint,
                Self::Files(plan) => plan.fingerprint,
            }
        }

        fn into_summary(self) -> Summary {
            match self {
                Self::Bcd(plan) => Summary::Bcd(plan.summary),
                Self::Files(plan) => Summary::Files(plan.summary),
            }
        }

        fn backup(&self) -> Result<std::borrow::Cow<'_, [u8]>, String> {
            match self {
                Self::Bcd(plan) => Ok(std::borrow::Cow::Borrowed(&plan.original)),
                Self::Files(plan) => {
                    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored);
                    zip.start_file("manifest.json", options)
                        .map_err(platform::err)?;
                    let mut manifest = serde_json::json!({
                        "version": 1,
                        "files": &plan.summary.files,
                    });
                    if let Some(bcd) = &plan.summary.new_bcd {
                        manifest["newBcd"] = serde_json::to_value(bcd).map_err(platform::err)?;
                    }
                    if let Some(bcd) = &plan.summary.recovered_bcd {
                        manifest["recoveredBcd"] = serde_json::to_value(bcd).map_err(platform::err)?;
                    }
                    serde_json::to_writer(&mut zip, &manifest).map_err(platform::err)?;
                    for file in &plan.originals {
                        if let Some(bytes) = &file.original {
                            let path = file.target.path().join("/");
                            zip.start_file(path, options).map_err(platform::err)?;
                            zip.write_all(bytes).map_err(platform::err)?;
                        }
                    }
                    for file in &plan.retired {
                        zip.start_file(file.target.path().join("/"), options)
                            .map_err(platform::err)?;
                        zip.write_all(&file.original).map_err(platform::err)?;
                    }
                    Ok(std::borrow::Cow::Owned(
                        zip.finish().map_err(platform::err)?.into_inner(),
                    ))
                }
            }
        }

        fn write(
            &self,
            writer: &mut View<File>,
            before_write: impl FnOnce() -> Result<(), String>,
        ) -> Result<(), String> {
            let sync = |view: &mut View<File>| view.inner_mut().sync_all();
            match self {
                Self::Bcd(plan) => plan.write(writer, before_write, sync),
                Self::Files(plan) => plan.write(writer, before_write, sync),
            }
        }
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

    fn plan(file: &File, bound: &Resolved, choice: &Choice) -> Result<Plan, String> {
        platform::verify_offline_handle(bound, file)?;
        let (physical, alignment) = platform::physical_alignment(bound, file)?;
        let geometry = Geometry {
            size: bound.size,
            sector: bound.disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?,
            physical,
            alignment,
        };
        let view = View::bounded(file, 0, bound.size, bound.disk.size).map_err(platform::err)?;
        let plan = match choice {
            Choice::Bcd(change) => Plan::Bcd(repair::plan(
                view,
                geometry,
                &bound.partition_ranges,
                change,
            )?),
            Choice::Files(change) => Plan::Files(files::plan(
                view,
                geometry,
                &bound.partition_ranges,
                change,
            )?),
        };
        bound.verify_boot_repair_layout(file, plan.layout(), choice.boot_partition())?;
        Ok(plan)
    }

    pub(crate) fn preview(request: Request) -> Result<Preview, String> {
        confirm_preview(request.source, Choice::Bcd(request.change))
    }

    pub(crate) fn preview_files(request: FileRequest) -> Result<Preview, String> {
        confirm_preview(request.source, Choice::Files(request.change))
    }

    fn confirm_preview(source: Selection, choice: Choice) -> Result<Preview, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        pending().take();
        let initial = resolve(&source)?;
        let locks = platform::lock(&initial)?;
        let bound = platform::resolve_locked(&source, true, &initial, &locks)?;
        let plan = plan(&platform::device(&bound, false)?, &bound, &choice)?;
        platform::resolve_locked(&source, true, &bound, &locks)?;
        let token = Uuid::new_v4();
        *pending() = Some(Pending {
            source,
            choice,
            before: bound,
            fingerprint: plan.fingerprint(),
            token,
            expires: Instant::now() + Duration::from_secs(300),
        });
        Ok(Preview {
            token,
            summary: plan.into_summary(),
        })
    }

    fn save_backup(
        path: &Path,
        selection: &Selection,
        bound: &Resolved,
        plan: &Plan,
    ) -> Result<File, String> {
        let bytes = plan.backup()?;
        let (mut file, publication) = backup_files::create_boot_repair_backup(
            path,
            &selection.target,
            bound.disk.number,
            bytes.len() as u64,
            matches!(plan, Plan::Files(_)),
        )?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .map_err(platform::err)?;
        let mut readback = vec![0; bytes.len()];
        file.read_exact(&mut readback).map_err(platform::err)?;
        if file.metadata().map_err(platform::err)?.len() != readback.len() as u64
            || sector_edit::hash(&readback) != sector_edit::hash(&bytes)
        {
            return Err("启动修复备份未通过 SHA-256 回读校验。".into());
        }
        publication.publish()?;
        Ok(file)
    }

    fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消启动修复。".into());
        }
        Ok(())
    }

    fn execute(pending: Pending, id: &str, cancel: &AtomicBool) -> Result<Option<Report>, String> {
        let Pending {
            source,
            choice,
            before,
            fingerprint,
            ..
        } = pending;
        let (name, ext) = match choice {
            Choice::Bcd(_) => ("BCD-before-repair.bcd", "bcd"),
            Choice::Files(_) => ("EFI-files-before-repair.zip", "zip"),
        };
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(choice.backup_title()))
            .add_filter(ext.to_ascii_uppercase(), &[ext])
            .set_file_name(name)
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

        // A read-only pass verifies the preview and provides the original files.
        let initial = plan(&platform::device(&bound, false)?, &bound, &choice)?;
        if initial.fingerprint() != fingerprint {
            return Err("启动文件或目标内容已变化，请重新检查并预览。".into());
        }
        check_cancel(cancel)?;
        record_progress(id, choice.backup_title(), 0, 0);
        // Keep the verified backup handle until the transaction has finished.
        let _backup = save_backup(&path, &source, &bound, &initial)?;
        let backup_path = path.display().to_string();

        let mut started = false;
        let result = (|| {
            check_cancel(cancel)?;
            let bound = platform::resolve_locked(&source, true, &bound, &locks)?;
            let file = platform::exclusive_device(&bound)?;
            let patch = plan(&file, &bound, &choice)?;
            if patch.fingerprint() != fingerprint {
                return Err("启动文件或目标内容已变化，请重新检查并预览。".into());
            }
            let mut writer =
                View::bounded(file, 0, bound.size, bound.disk.size).map_err(platform::err)?;
            let result = patch.write(&mut writer, || {
                platform::resolve_locked(&source, true, &bound, &locks)?;
                check_cancel(cancel)?;
                record_progress(
                    id,
                    if ext == "bcd" {
                        "正在写入并回读校验 BCD…"
                    } else {
                        "正在写入并回读校验 EFI 文件…"
                    },
                    0,
                    0,
                );
                started = true;
                Ok(())
            });
            let verified_state = platform::resolve_locked(&source, true, &bound, &locks);
            result?;
            verified_state?.verify_boot_repair_layout(
                writer.inner_mut(),
                patch.layout(),
                choice.boot_partition(),
            )?;
            Ok(patch.into_summary())
        })();
        let summary = result.map_err(|error: String| {
            let locale = crate::native_locale::current();
            locale.format(
                match (ext, started) {
                    ("bcd", true) => "{error} 修复可能只完成了一部分。原始 BCD 已备份至 {path}。",
                    ("bcd", false) => "{error} 原始 BCD 已备份至 {path}。",
                    (_, true) => "{error} 修复可能只完成了一部分。EFI 文件备份位于 {path}。",
                    (_, false) => "{error} EFI 文件备份位于 {path}。",
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
        let (id, cancel) = start(pending.choice.title())?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(pending, &id, &cancel)
        }))
        .unwrap_or_else(|_| Err("启动修复异常结束，请检查磁盘和选定的备份文件。".into()));
        finish(
            &id,
            result
                .as_ref()
                .map(|report| {
                    report.as_ref().map_or_else(
                        || ("已取消启动修复。".into(), None),
                        |report| match &report.summary {
                            Summary::Bcd(summary) => (
                                "BCD 修复完成，已通过 SHA-256 回读校验。".into(),
                                Some(summary.after_sha256.clone()),
                            ),
                            Summary::Files(_) => {
                                ("EFI 启动文件已恢复并通过 SHA-256 回读校验。".into(), None)
                            }
                        },
                    )
                })
                .map_err(Clone::clone),
        );
        result
    }
}

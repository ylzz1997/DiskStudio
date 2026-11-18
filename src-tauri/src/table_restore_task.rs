//! Explicit GPT restore. No device access until a typed user request.
use crate::{data_tasks::Selection, table_restore};
use serde::{Deserialize, Serialize};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub includes_boot: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairRequest {
    pub check_id: String,
}
pub fn prepare_recovery(
    request: crate::lost_partition_tasks::RecoverRequest,
) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        windows::job("预览丢失分区恢复", |cancel| {
            windows::prepare_recovery(request, cancel)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区记录恢复当前仅支持 Windows。".into())
    }
}
pub fn prepare_repair(request: RepairRequest) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        windows::job("预览 GPT 修复", |cancel| {
            windows::prepare_repair(request, cancel)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("GPT 修复当前仅支持 Windows。".into())
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub repair: bool,
    pub recovery: bool,
    pub token: Option<String>,
    pub target: String,
    pub name: String,
    pub file_sha256: String,
    pub partitions: usize,
    pub changed_bytes: u64,
    pub write_bytes: u64,
    pub ranges: usize,
    pub includes_boot: bool,
    pub changes: Vec<table_restore::Change>,
    pub warnings: Vec<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub message: String,
    pub verification: table_restore::Outcome,
}
pub fn prepare(request: Request) -> Result<Option<Preview>, String> {
    #[cfg(windows)]
    {
        windows::job("预览分区表恢复", |cancel| {
            windows::prepare(request, cancel)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区表恢复当前仅支持 Windows。".into())
    }
}
pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(windows)]
    {
        windows::job("恢复分区表", |cancel| windows::execute(token, cancel))
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("分区表恢复当前仅支持 Windows。".into())
    }
}
pub fn discard(token: String) {
    #[cfg(windows)]
    {
        windows::discard(token);
    }
    #[cfg(not(windows))]
    {
        let _ = token;
    }
}
#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{data_windows as device, partitions::platform, table_archive};
    use cap_std::fs::{Dir, OpenOptions as CapOptions, OpenOptionsExt};
    use sha2::{Digest, Sha256};
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Seek, SeekFrom},
        os::windows::{
            fs::{MetadataExt, OpenOptionsExt as _},
            io::AsRawHandle,
        },
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{Storage::FileSystem::*, System::Ioctl::*};
    struct Input {
        file: Option<File>,
        _dir: Option<Dir>,
        repair: bool,
        recovered: Vec<crate::lost_partition_restore::Evidence>,
        bytes: Vec<u8>,
        archive: table_archive::Archive,
        name: String,
    }
    struct Pending {
        preview: Preview,
        request: Request,
        input: Input,
        plan: table_restore::Plan,
        fingerprint: String,
        native: Vec<String>,
        expires: Instant,
    }
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
        PENDING.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn discard(token: String) {
        let mut p = pending();
        if p.as_ref()
            .is_some_and(|p| p.preview.token.as_deref() == Some(&token))
        {
            *p = None;
        }
    }
    fn take(token: &str) -> Result<Pending, String> {
        take_from(&mut pending(), token, Instant::now())
    }
    fn take_from(
        state: &mut Option<Pending>,
        token: &str,
        now: Instant,
    ) -> Result<Pending, String> {
        let p = state.take().ok_or("恢复确认已失效，请重新预览。")?;
        if p.preview.token.as_deref() != Some(token) || now >= p.expires {
            return Err("恢复确认已过期或不匹配。".into());
        }
        Ok(p)
    }
    pub fn job<T>(
        title: &str,
        work: impl FnOnce(&AtomicBool) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let (id, cancel) = crate::data_tasks::start(title)?;
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work(&cancel))).unwrap_or_else(|_|Err("分区表任务异常结束；若已确认恢复，磁盘可能脱机或已有部分写入。不会自动重试或回滚。".into()));
        crate::data_tasks::finish(
            &id,
            result
                .as_ref()
                .map(|_| ("分区表任务完成".into(), None))
                .map_err(Clone::clone),
        );
        result
    }
    fn check(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            Err("已取消分区表任务。".into())
        } else {
            Ok(())
        }
    }
    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    // The immutable file handle, not a frontend path or drive letter, determines
    // its actual volume. This query reads extent metadata, not volume contents.
    fn input_on_other_disk(file: &File, destination: u32) -> Result<(), String> {
        let mut path = vec![0u16; 32768];
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                VOLUME_NAME_GUID,
            )
        } as usize;
        if n == 0 || n >= path.len() {
            return Err("无法确认备份文件所在卷。".into());
        }
        let path = String::from_utf16(&path[..n]).map_err(|_| "备份卷路径无效")?;
        let prefix = "\\\\?\\Volume{";
        let end = path.find("}\\").ok_or("备份不在本机普通卷上")?;
        if !path.starts_with(prefix) || uuid::Uuid::parse_str(&path[prefix.len()..end]).is_err() {
            return Err("备份卷 GUID 无效。".into());
        }
        let volume = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path[..end + 1])
            .map_err(device::err)?;
        let mut extents = [0u8; 32];
        let n = device::control(
            &volume,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            &[],
            &mut extents,
        )?;
        if n < 32
            || u32::from_le_bytes(extents[..4].try_into().unwrap()) != 1
            || u32::from_le_bytes(extents[8..12].try_into().unwrap()) == destination
        {
            return Err("分区表备份文件必须位于另一块普通物理磁盘。".into());
        }
        Ok(())
    }
    fn choose(source: &Selection, disk: u32, cancel: &AtomicBool) -> Result<Option<Input>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("选择另一块磁盘上的分区表备份"))
            .add_filter(
                crate::native_locale::text("DiskStudio 分区表备份"),
                &["dspt"],
            )
            .pick_file()
        else {
            return Ok(None);
        };
        check(cancel)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("备份文件名无效")?
            .to_string();
        if name.len() > 180
            || name.contains([':', '/', '\\'])
            || !name.to_ascii_lowercase().ends_with(".dspt")
        {
            return Err("请选择普通 .dspt 文件。".into());
        }
        let folder =
            device::destination_folder(path.parent().ok_or("备份目录无效")?, &source.target, 0)?;
        let dir =
            Dir::open_ambient_dir(folder, cap_std::ambient_authority()).map_err(device::err)?;
        let mut options = CapOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = dir
            .open_with(&name, &options)
            .map_err(device::err)?
            .into_std();
        let m = file.metadata().map_err(device::err)?;
        if !m.is_file()
            || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || m.len() > table_archive::MAX_FILE as u64
        {
            return Err("备份必须是大小受限的普通文件。".into());
        }
        input_on_other_disk(&file, disk)?;
        let mut bytes = vec![0; m.len() as usize];
        file.read_exact(&mut bytes).map_err(device::err)?;
        let archive = table_archive::Archive::decode(&bytes)?;
        Ok(Some(Input {
            file: Some(file),
            _dir: Some(dir),
            repair: false,
            recovered: vec![],
            bytes,
            archive,
            name,
        }))
    }
    fn verify_input(input: &mut Input, destination: u32) -> Result<(), String> {
        let Some(file) = &mut input.file else {
            if (!input.repair && input.recovered.is_empty())
                || input.archive.encode()? != input.bytes
            {
                return Err("修复计划的内存摘要不一致。".into());
            }
            return Ok(());
        };
        input_on_other_disk(file, destination)?;
        if file.metadata().map_err(device::err)?.len() != input.bytes.len() as u64 {
            return Err("备份长度与预览不同。".into());
        }
        file.seek(SeekFrom::Start(0)).map_err(device::err)?;
        let mut bytes = vec![0; input.bytes.len()];
        file.read_exact(&mut bytes).map_err(device::err)?;
        if hash(&bytes) != hash(&input.bytes) {
            return Err("备份摘要与预览不同。".into());
        }
        table_archive::Archive::decode(&bytes)?;
        Ok(())
    }
    pub fn prepare(request: Request, cancel: &AtomicBool) -> Result<Option<Preview>, String> {
        *pending() = None;
        let (initial, _, _) = platform::table_restore_context(&request.source, None)?;
        let Some(input) = choose(&request.source, initial.resolved.disk.number, cancel)? else {
            return Ok(None);
        };
        let (ctx, guid, protected) = platform::table_restore_context(&request.source, None)?;
        if ctx.fingerprint != initial.fingerprint {
            return Err("选择备份期间磁盘状态已变化。".into());
        }
        build(request, ctx, guid, protected, input, None, cancel).map(Some)
    }
    pub fn prepare_repair(request: RepairRequest, cancel: &AtomicBool) -> Result<Preview, String> {
        *pending() = None;
        let saved = crate::table_inspect_task::saved(&request.check_id)?;
        if !saved.report.repair_available {
            return Err("该检查结果没有可执行的 GPT 修复，请重新检查。".into());
        }
        if platform::table_inspect_context(&saved.source)?.fingerprint != saved.fingerprint {
            return Err("Windows 状态与检查结果不同，请重新检查。".into());
        }
        let (ctx, guid, protected) = platform::table_restore_context(&saved.source, None)?;
        let d = &ctx.resolved.disk;
        let identity = crate::disk_identity::key(
            d.serial_number.as_deref(),
            None,
            d.friendly_name.as_deref(),
            d.size,
            d.logical_sector_size,
            d.bus_type,
        )
        .ok_or("磁盘身份不可核验")?;
        let repair = saved
            .snapshot
            .repair(&identity)?
            .ok_or("GPT 已完整，无需修复")?;
        let bytes = repair.archive.encode()?;
        let input = Input {
            file: None,
            _dir: None,
            repair: true,
            recovered: vec![],
            bytes,
            archive: repair.archive,
            name: repair.description,
        };
        let request = Request {
            source: saved.source,
            includes_boot: false,
        };
        build(
            request,
            ctx,
            guid,
            protected,
            input,
            Some(&saved.snapshot),
            cancel,
        )
    }
    pub fn prepare_recovery(
        request: crate::lost_partition_tasks::RecoverRequest,
        cancel: &AtomicBool,
    ) -> Result<Preview, String> {
        *pending() = None;
        let (source, range, candidates) = crate::lost_partition_tasks::selected_many(request)?;
        let (ctx, guid, protected) = platform::table_restore_context(&source, None)?;
        let d = &ctx.resolved.disk;
        let sector = d.logical_sector_size.ok_or("缺少逻辑扇区")?;
        if range.sector != sector
            || range
                .offset
                .checked_add(range.size)
                .is_none_or(|end| end > d.size)
        {
            return Err("候选搜索范围或设备几何变化。".into());
        }
        let identity = crate::disk_identity::key(
            d.serial_number.as_deref(),
            None,
            d.friendly_name.as_deref(),
            d.size,
            Some(sector),
            d.bus_type,
        )
        .ok_or("磁盘身份不可核验")?;
        let mut file = device::device(&ctx.resolved, false)?;
        let current =
            table_archive::capture(&mut file, d.size, sector, identity, false, || check(cancel))?;
        let mut recovered = vec![];
        for candidate in candidates {
            let extent =
                crate::lost_partition_restore::extent(&mut file, range, &candidate, cancel)?;
            recovered.push(crate::lost_partition_restore::Evidence {
                range,
                candidate,
                extent,
            });
        }
        let archive = crate::lost_partition_restore::add_entries(
            &current,
            &recovered
                .iter()
                .map(|e| e.extent.clone())
                .collect::<Vec<_>>(),
        )?;
        let input = Input {
            file: None,
            _dir: None,
            repair: false,
            bytes: archive.encode()?,
            archive,
            name: format!("恢复 {} 个找到的分区记录", recovered.len()),
            recovered,
        };
        build(
            Request {
                source,
                includes_boot: false,
            },
            ctx,
            guid,
            protected,
            input,
            None,
            cancel,
        )
    }
    fn build(
        request: Request,
        ctx: platform::TableReadContext,
        guid: String,
        mut protected: Vec<String>,
        input: Input,
        snapshot: Option<&crate::table_diagnostics::Snapshot>,
        cancel: &AtomicBool,
    ) -> Result<Preview, String> {
        let disk = &ctx.resolved.disk;
        let sector = disk.logical_sector_size.ok_or("逻辑扇区未知")?;
        let mut file = device::device(&ctx.resolved, false)?;
        crate::lost_partition_restore::verify(&mut file, &input.recovered, cancel)?;
        if let Some(snapshot) = snapshot {
            snapshot.matches(&mut file, || check(cancel))?;
        }
        let (native, services, fingerprint) = crate::extfs::restore_native(
            &file,
            Some(&ctx.parts.iter().collect::<Vec<_>>()),
            sector,
        )?;
        if native.guid != guid {
            return Err("Windows 原生与 WMI 磁盘 GUID 不一致。".into());
        }
        protected.extend(services);
        let identity = crate::disk_identity::key(
            disk.serial_number.as_deref(),
            None,
            disk.friendly_name.as_deref(),
            disk.size,
            Some(sector),
            disk.bus_type,
        )
        .ok_or("不能确认备份源身份")?;
        let mut plan = table_restore::prepare(
            &mut file,
            &input.archive,
            &identity,
            disk.size,
            sector,
            &native,
            &protected,
            request.includes_boot,
            || check(cancel),
        )?;
        if input.repair && !plan.changes.is_empty() {
            return Err("GPT 修复不得改变分区条目。".into());
        }
        if !input.recovered.is_empty() {
            if plan.changes.len() != input.recovered.len()
                || plan
                    .changes
                    .iter()
                    .any(|c| c.before.is_some() || c.after.is_none())
            {
                return Err("候选恢复只能新增记录，不能替换现有分区。".into());
            }
            plan.warnings.push("恢复使用文件系统所需范围，可能不含原分区末尾空白；原分区 GUID、名称和属性无法从文件系统推回，将生成新的普通数据分区记录。不会格式化、修复文件系统或自动分配盘符。".into());
        }
        if platform::table_restore_context(&request.source, None)?
            .0
            .fingerprint
            != ctx.fingerprint
        {
            return Err("预览期间磁盘状态已变化。".into());
        }
        check(cancel)?;
        let preview = Preview {
            repair: input.repair,
            recovery: !input.recovered.is_empty(),
            token: (plan.ranges() > 0).then(|| uuid::Uuid::new_v4().to_string()),
            target: ctx.resolved.label,
            name: input.name.clone(),
            file_sha256: hash(&input.bytes),
            partitions: plan.layout.entries.len(),
            changed_bytes: plan.changed_bytes,
            write_bytes: plan.write_bytes,
            ranges: plan.ranges(),
            includes_boot: request.includes_boot,
            changes: plan.changes.clone(),
            warnings: plan.warnings.clone(),
        };
        if preview.token.is_some() {
            *pending() = Some(Pending {
                preview: preview.clone(),
                request,
                input,
                plan,
                fingerprint: ctx.fingerprint,
                native: fingerprint,
                expires: Instant::now() + Duration::from_secs(300),
            });
        }
        Ok(preview)
    }
    pub fn execute(token: String, cancel: &AtomicBool) -> Result<Outcome, String> {
        let mut pending = take(&token)?;
        check(cancel)?;
        let (ctx, guid, _) = platform::table_restore_context(&pending.request.source, None)?;
        if ctx.fingerprint != pending.fingerprint || guid != pending.plan.layout.guid {
            return Err("目标状态与预览不同，请重新预览。".into());
        }
        verify_input(&mut pending.input, ctx.resolved.disk.number)?;
        let locks = device::lock(&ctx.resolved)?;
        let (fresh, guid, _) =
            platform::table_restore_context(&pending.request.source, Some((&ctx, &locks)))?;
        if fresh.fingerprint != pending.fingerprint || guid != pending.plan.layout.guid {
            return Err("锁定期间目标状态改变。".into());
        }
        let parts = fresh.parts.iter().collect::<Vec<_>>();
        device::verify_locks(&locks, &ctx.resolved, &parts)?;
        check(cancel)?;
        let mut file = device::exclusive_device(&fresh.resolved)?;
        let sector = fresh
            .resolved
            .disk
            .logical_sector_size
            .ok_or("逻辑扇区未知")?;
        if crate::extfs::restore_native(&file, Some(&parts), sector)?.2 != pending.native {
            return Err("原生布局与预览不同。".into());
        }
        pending.plan.matches_before(&mut file, || check(cancel))?;
        crate::lost_partition_restore::verify(&mut file, &pending.input.recovered, cancel)?;
        device::offline(&file).map_err(|e| format!("{e}；磁盘可能已脱机，未执行分区表写入。"))?;
        let result = (|| -> Result<table_restore::Outcome, String> {
            if crate::extfs::restore_native(&file, Some(&parts), sector)?.2 != pending.native {
                return Err("脱机后原生布局变化。".into());
            }
            crate::lost_partition_restore::verify(&mut file, &pending.input.recovered, cancel)?;
            let result = pending.plan.execute(
                &mut file,
                |f| f.sync_all().map_err(device::err),
                || check(cancel),
            )?;
            // One explicit cache invalidation, never a retry or disk activation.
            device::control(&file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])?;
            pending.plan.verify_after(&mut file)?;
            let after = crate::extfs::restore_native(&file, None, sector)?.0;
            if !after.agrees(&pending.plan.layout) {
                return Err("写后 Windows 原生布局与备份不一致。".into());
            }
            let mut attrs = [0u8; 16];
            if device::control(&file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attrs)? < 16
                || u64::from_le_bytes(attrs[8..16].try_into().unwrap()) & 1 == 0
            {
                return Err("写后不能确认磁盘仍脱机。".into());
            }
            Ok(result)
        })()
        .map_err(|e| {
            format!("{e} 磁盘保持脱机，可能已有部分恢复；请核对实际状态，不会自动重试或回滚。")
        })?;
        Ok(Outcome {
            message: format!(
                "{}，通过逐范围 SHA-256 回读和 Windows 原生布局校验。磁盘保持脱机。",
                if !pending.input.recovered.is_empty() {
                    "找到的分区记录已恢复"
                } else if pending.input.repair {
                    "GPT 已修复"
                } else {
                    "分区表已恢复"
                }
            ),
            verification: result,
        })
    }
    #[cfg(test)]
    #[test]
    fn restore_ticket_consumed_on_success_mismatch_expiry_and_file_is_immutable() {
        use std::{fs, io::Cursor};
        let root =
            std::env::temp_dir().join(format!("DiskStudio-table-ticket-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let fixture = crate::table_archive::tests::gpt(512);
        let archive = table_archive::capture(
            &mut Cursor::new(&fixture),
            fixture.len() as u64,
            512,
            "a".repeat(64),
            false,
            || Ok(()),
        )
        .unwrap();
        let bytes = archive.encode().unwrap();
        fs::write(root.join("table.dspt"), &bytes).unwrap();
        let now = Instant::now();
        let make = |expires| {
            let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
            let mut options = CapOptions::new();
            options.read(true).share_mode(FILE_SHARE_READ);
            let file = dir.open_with("table.dspt", &options).unwrap().into_std();
            let native = table_restore::archive_layout(&archive).unwrap();
            let plan = table_restore::prepare(
                &mut Cursor::new(&fixture),
                &archive,
                &"a".repeat(64),
                fixture.len() as u64,
                512,
                &native,
                &[],
                false,
                || Ok(()),
            )
            .unwrap();
            let preview = Preview {
                repair: false,
                recovery: false,
                token: Some("once".into()),
                target: "synthetic".into(),
                name: "table.dspt".into(),
                file_sha256: hash(&bytes),
                partitions: 1,
                changed_bytes: 0,
                write_bytes: 0,
                ranges: 0,
                includes_boot: false,
                changes: vec![],
                warnings: vec![],
            };
            Pending{preview,request:serde_json::from_value(serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":null},"includesBoot":false})).unwrap(),input:Input{file:Some(file),_dir:Some(dir),repair:false,recovered:vec![],bytes:bytes.clone(),archive:archive.clone(),name:"table.dspt".into()},plan,fingerprint:"synthetic".into(),native:vec![],expires}
        };
        for (token, expires, ok) in [
            ("once", now + Duration::from_secs(30), true),
            ("wrong", now + Duration::from_secs(30), false),
            ("once", now, false),
        ] {
            let mut state = Some(make(expires));
            assert!(OpenOptions::new()
                .write(true)
                .open(root.join("table.dspt"))
                .is_err());
            assert!(fs::rename(root.join("table.dspt"), root.join("other.dspt")).is_err());
            let outcome = take_from(&mut state, token, now);
            assert_eq!(outcome.is_ok(), ok);
            assert!(state.is_none());
            assert!(take_from(&mut state, "once", now).is_err());
            drop(outcome);
        }
        fs::remove_file(root.join("table.dspt")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_ipc_rejects_paths_commands_and_unknown_fields() {
        let good = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"known","partition":null},"includesBoot":false});
        assert!(serde_json::from_value::<Request>(good.clone()).is_ok());
        for key in ["path", "device", "method", "token"] {
            let mut bad = good.clone();
            bad[key] = serde_json::json!("x");
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
    }
}

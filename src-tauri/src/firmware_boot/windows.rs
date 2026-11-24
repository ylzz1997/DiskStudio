use super::{
    archive, catalog, loader,
    model::{
        new_load_option, Plan, Snapshot, Value, Variable, BOOT_VARIABLE_ATTRIBUTES,
        MAX_VARIABLE_SIZE,
    },
    secure_boot, BackupRequest, BackupResult, Change, Preview, Report, Request, WinPePreview,
    WinPeRequest, WinPeRestart,
};
use crate::maintenance_tasks::{consume, Ticket};
use crate::winpe_handoff::{self, Armed, Backup, CloneJob, MigrationBatch, MigrationJob, Restore, Task};
use crate::{backup_files, data_windows, native_locale};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::Instant;
use windows_sys::core::w;
use windows_sys::Win32::{
    Foundation::{
        GetLastError, ERROR_ENVVAR_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA,
        ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, WAIT_ABANDONED, WAIT_OBJECT_0,
    },
    Security::{
        AdjustTokenPrivileges, ImpersonateSelf, LookupPrivilegeValueW, RevertToSelf,
        SecurityImpersonation, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_PRIVILEGES,
    },
    System::{
        Shutdown::{
            InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_APPLICATION,
        },
        SystemInformation::{FirmwareTypeUefi, FirmwareTypeUnknown, GetFirmwareType},
        Threading::{
            CreateMutexW, GetCurrentThread, OpenThreadToken, ReleaseMutex, WaitForSingleObject,
        },
        WindowsProgramming::{
            GetFirmwareEnvironmentVariableExW, SetFirmwareEnvironmentVariableExW,
        },
    },
};

const UEFI_GLOBAL: *const u16 = w!("{8be4df61-93ca-11d2-aa0d-00e098032b8c}");
const UEFI_IMAGE_SECURITY: *const u16 = w!("{d719b2cb-3d3a-4596-a3bc-dad00e67656f}");
const HANDOFF_VENDOR: *const u16 = w!("{dad63fb3-bb4e-4b84-b3ce-3be0d11fe790}");
const HANDOFF_NAME: &str = "DiskStudioWinPeTask";
const PRIVILEGE: &str = "当前进程没有读取或修改 UEFI 固件变量所需的 Windows 权限。";
static I_O: Mutex<()> = Mutex::new(());
static PENDING: Mutex<Option<Ticket<Pending>>> = Mutex::new(None);
static WINPE_PENDING: Mutex<Option<Ticket<WinPePending>>> = Mutex::new(None);

struct Pending {
    plan: Plan,
    source: Option<(loader::Candidate, loader::Witness)>,
    clears_handoff: bool,
}

struct WinPePending {
    plan: Plan,
    source: crate::data_tasks::Selection,
    media: loader::WinPeWitness,
    executable: PathBuf,
    entry: u16,
    task: Option<Task>,
}

fn with_firmware<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    // Run on a dedicated thread so an unexpected failure to revert cannot
    // leave a reused Tauri worker with the firmware privilege enabled.
    std::thread::Builder::new()
        .name("uefi-firmware".into())
        .spawn(move || {
            let _serial = I_O.lock().map_err(|_| "固件启动项操作已中断。")?;
            let mut kind = FirmwareTypeUnknown;
            if unsafe { GetFirmwareType(&mut kind) } == 0 {
                return Err(format!("GetFirmwareType ({})", unsafe { GetLastError() }));
            }
            if kind != FirmwareTypeUefi {
                return Err("固件启动项管理需要 Windows UEFI。".into());
            }
            if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
                return Err(PRIVILEGE.into());
            }
            let result = enable_assigned_privilege(w!("SeSystemEnvironmentPrivilege"), PRIVILEGE)
                .and_then(|()| operation());
            // The thread exits immediately even if this call fails.
            unsafe { RevertToSelf() };
            result
        })
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|_| "固件启动项操作已中断。".to_string())?
}

fn enable_assigned_privilege(name: *const u16, missing: &str) -> Result<(), String> {
    let mut handle = null_mut();
    let mut luid = Default::default();
    if unsafe { LookupPrivilegeValueW(null(), name, &mut luid) } == 0
        || unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_ADJUST_PRIVILEGES, 1, &mut handle) }
            == 0
    {
        return Err(missing.into());
    }
    let _owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let adjusted =
        unsafe { AdjustTokenPrivileges(handle, 0, &privileges, 0, null_mut(), null_mut()) };
    let error = unsafe { GetLastError() };
    if adjusted == 0 || error == ERROR_NOT_ALL_ASSIGNED {
        return Err(missing.into());
    }
    if error != ERROR_SUCCESS {
        return Err(format!("AdjustTokenPrivileges ({error})"));
    }
    Ok(())
}

fn read(key: Variable) -> Result<Option<Value>, String> {
    let name = key.name();
    read_named(&name, UEFI_GLOBAL, MAX_VARIABLE_SIZE)
        .map_err(|error| format!("GetFirmwareEnvironmentVariableExW {name} ({error})"))
}

fn read_named(name: &str, namespace: *const u16, limit: usize) -> Result<Option<Value>, u32> {
    let name: Vec<_> = name.encode_utf16().chain([0]).collect();
    let mut bytes = vec![0; limit.min(MAX_VARIABLE_SIZE)];
    loop {
        let mut attributes = 0;
        let size = unsafe {
            GetFirmwareEnvironmentVariableExW(
                name.as_ptr(),
                namespace,
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                &mut attributes,
            )
        } as usize;
        if size != 0 {
            if size > bytes.len() {
                return Err(ERROR_INVALID_DATA);
            }
            bytes.truncate(size);
            return Ok(Some(Value { attributes, bytes }));
        }
        match unsafe { GetLastError() } {
            ERROR_ENVVAR_NOT_FOUND => return Ok(None),
            ERROR_INSUFFICIENT_BUFFER if bytes.len() < limit => {
                bytes.resize((bytes.len() * 2).min(limit), 0);
            }
            error => return Err(error),
        }
    }
}

fn write(key: Variable, value: Option<&Value>) -> Result<(), String> {
    if matches!(key, Variable::Current | Variable::IndicationsSupported)
        || (value.is_none() && !matches!(key, Variable::Next | Variable::Entry(_)))
    {
        return Err("不能修改该 UEFI 固件变量。".into());
    }
    write_named(&key.name(), UEFI_GLOBAL, value)
}

fn write_named(name: &str, namespace: *const u16, value: Option<&Value>) -> Result<(), String> {
    let wide_name: Vec<_> = name.encode_utf16().chain([0]).collect();
    let (data, len, attributes) = value.map_or((null(), 0, BOOT_VARIABLE_ATTRIBUTES), |value| {
        (
            value.bytes.as_ptr().cast(),
            value.bytes.len() as u32,
            value.attributes,
        )
    });
    if unsafe {
        SetFirmwareEnvironmentVariableExW(wide_name.as_ptr(), namespace, data, len, attributes)
    } == 0
    {
        let error = unsafe { GetLastError() };
        return Err(format!(
            "SetFirmwareEnvironmentVariableExW {name} ({error})"
        ));
    }
    Ok(())
}

fn read_handoff() -> Result<Option<Value>, String> {
    read_named(HANDOFF_NAME, HANDOFF_VENDOR, winpe_handoff::MAX_RECORD)
        .map_err(|error| format!("GetFirmwareEnvironmentVariableExW {HANDOFF_NAME} ({error})"))
}

fn update_handoff(value: Option<&Value>) -> Result<(), String> {
    write_named(HANDOFF_NAME, HANDOFF_VENDOR, value)?;
    if read_handoff()?.as_ref() != value {
        return Err("WinPE 续接记录回读不一致。".into());
    }
    Ok(())
}

fn clear_handoff() -> Result<(), String> {
    if read_handoff()?.is_some() {
        update_handoff(None)?;
    }
    Ok(())
}

struct HandoffLock(OwnedHandle);

impl HandoffLock {
    fn acquire() -> Result<Self, String> {
        let handle = unsafe { CreateMutexW(null(), 0, w!("Global\\DiskStudioWinPeHandoff")) };
        if handle.is_null() {
            return Err(data_windows::err(std::io::Error::last_os_error()));
        }
        let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
        if !matches!(
            unsafe { WaitForSingleObject(handle, 5000) },
            WAIT_OBJECT_0 | WAIT_ABANDONED
        ) {
            return Err("WinPE 续接任务正由另一个程序实例核对。".into());
        }
        Ok(Self(owned))
    }
}

impl Drop for HandoffLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        unsafe { ReleaseMutex(self.0.as_raw_handle()) };
    }
}

pub(super) fn inspect() -> Result<Report, String> {
    with_firmware(|| Snapshot::capture_catalog(read, catalog::enumerate)?.report())
}

pub(super) fn inspect_secure_boot() -> Result<secure_boot::Report, String> {
    with_firmware(|| {
        secure_boot::capture(|name, image_namespace, limit| {
            let namespace = if image_namespace {
                UEFI_IMAGE_SECURITY
            } else {
                UEFI_GLOBAL
            };
            read_named(name, namespace, limit)
        })
    })
}

pub(super) fn preview(request: Request) -> Result<Option<Preview>, String> {
    let source = match &request.change {
        Change::Create { candidate, .. } | Change::Retarget { candidate, .. } => {
            let candidate = loader::candidate(*candidate)?;
            let witness = candidate.verify(Ok)?;
            Some((candidate, witness))
        }
        _ => None,
    };
    let archived_entry = if matches!(request.change, Change::Restore) {
        let Some(path) = rfd::FileDialog::new()
            .set_title(native_locale::text("还原 UEFI 启动项"))
            .add_filter("DiskStudio UEFI", &["dsuefi"])
            .pick_file()
        else {
            return Ok(None);
        };
        let mut input = backup_files::Input::open(&path, None)?;
        let mut bytes = Vec::new();
        (&mut input.file)
            .take(archive::MAX_ARCHIVE_SIZE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(data_windows::err)?;
        data_windows::FileBinding::validate_all(&[input.binding])?;
        Some(archive::decode(&bytes)?)
    } else {
        None
    };
    with_firmware(move || {
        let snapshot = Snapshot::capture_catalog(read, catalog::enumerate)?;
        let plan = match (&request.change, source.as_ref(), archived_entry) {
            (Change::Create { description, .. }, Some((candidate, witness)), _) => {
                snapshot.check_revision(&request.revision)?;
                let path = candidate.device_path(witness)?;
                snapshot.add(new_load_option(description, &path)?, read)?
            }
            (
                Change::Retarget {
                    entry,
                    preserve_parameters,
                    ..
                },
                Some((candidate, witness)),
                _,
            ) => {
                snapshot.check_revision(&request.revision)?;
                let path = candidate.device_path(witness)?;
                snapshot.retarget(*entry, &path, *preserve_parameters)?
            }
            (Change::Restore, _, Some(archived_entry)) => {
                snapshot.check_revision(&request.revision)?;
                snapshot.add(archived_entry, read)?
            }
            _ => snapshot.prepare(&request.revision, &request.change)?,
        };
        let token = uuid::Uuid::new_v4().to_string();
        let clears_handoff = matches!(request.change, Change::ClearOnce | Change::Once { .. });
        let preview = Preview {
            token: token.clone(),
            before: plan.before.report()?,
            after: plan.after.report()?,
            change: request.change,
        };
        *PENDING.lock().map_err(|_| "固件启动项操作已中断。")? = Some(Ticket::new(
            token,
            Pending {
                plan,
                source,
                clears_handoff,
            },
        ));
        Ok(Some(preview))
    })
}

pub(super) fn apply(token: String) -> Result<Report, String> {
    let Pending {
        plan,
        source,
        clears_handoff,
    } = {
        let mut pending = PENDING.lock().map_err(|_| "固件启动项操作已中断。")?;
        consume(&mut pending, &token, Instant::now())?
    };
    if let Some((candidate, expected)) = source {
        candidate.verify(|fresh| {
            if fresh != expected {
                return Err("EFI 加载器或 GPT 范围已变化，请重新预览。".into());
            }
            with_firmware(move || plan.apply_catalog(read, write, catalog::enumerate))
        })
    } else if clears_handoff {
        with_firmware(move || {
            let _handoff = HandoffLock::acquire()?;
            plan.verify_current(read, catalog::enumerate)?;
            clear_handoff()?;
            plan.apply_catalog(read, write, catalog::enumerate)
        })
    } else {
        with_firmware(move || plan.apply_catalog(read, write, catalog::enumerate))
    }
}

pub(super) fn winpe_preview(request: WinPeRequest) -> Result<Option<WinPePreview>, String> {
    let task = match request.task {
        Some(winpe_handoff::Request::Backup { ref source }) => {
            let Some(path) = rfd::FileDialog::new()
                .set_title(native_locale::text("保存离线完整扇区备份"))
                .add_filter(native_locale::text("DiskStudio 备份"), &["dsbackup"])
                .set_file_name("DiskStudio-offline.dsbackup")
                .save_file()
            else {
                return Ok(None);
            };
            let (backup, preview) = Backup::prepare(source.clone(), &request.source.target, &path)?;
            Some((Task::Backup(backup), preview))
        }
        Some(winpe_handoff::Request::Restore { ref destination }) => {
            let Some(path) = rfd::FileDialog::new()
                .set_title(native_locale::text("选择 DiskStudio 备份"))
                .add_filter(native_locale::text("DiskStudio 备份"), &["dsbackup"])
                .pick_file()
            else {
                return Ok(None);
            };
            let Some((restore, preview)) =
                Restore::prepare(destination.clone(), &request.source.target, &path, |id| {
                    rfd::FileDialog::new()
                        .set_title(
                            native_locale::text("选择依赖的基础备份 · {id}").replace("{id}", id),
                        )
                        .add_filter(native_locale::text("DiskStudio 备份"), &["dsbackup"])
                        .pick_file()
                })?
            else {
                return Ok(None);
            };
            Some((Task::Restore(restore), preview))
        }
        Some(winpe_handoff::Request::Clone { ref source, ref destination, mode }) => {
            let (job, preview) = CloneJob::prepare(source.clone(), destination.clone(), mode, &request.source.target)?;
            Some((Task::Clone(job), preview))
        }
        Some(winpe_handoff::Request::Migrate { ref source, ref destination, ref partition_sizes, ref partition_selection }) => {
            let options = crate::system_migration::Options { sizes: partition_sizes.clone(), selection: partition_selection.clone() };
            let (job, preview) = MigrationJob::prepare(source.clone(), destination.clone(), &request.source.target, options)?;
            Some((Task::Migrate(job), preview))
        }
        Some(winpe_handoff::Request::MigrateBatch { ref source, ref destinations }) => {
            let (job, preview) = MigrationBatch::prepare(source.clone(), destinations.clone(), &request.source)?;
            Some((Task::MigrateBatch(job), preview))
        }
        None => None,
    };
    prepare_winpe(
        request,
        std::env::current_exe().map_err(data_windows::err)?,
        task,
    )
    .map(Some)
}

fn prepare_winpe(
    request: WinPeRequest,
    executable: PathBuf,
    task: Option<(Task, winpe_handoff::Preview)>,
) -> Result<WinPePreview, String> {
    let WinPeRequest {
        revision, source, ..
    } = request;
    let retained_source = source.clone();
    let retained_executable = executable.clone();
    loader::verify_winpe(&source, &executable, move |media| {
        with_firmware(move || {
            let snapshot = Snapshot::capture_catalog(read, catalog::enumerate)?;
            let (plan, entry, creates_entry) = snapshot.winpe_once(&revision, &media.path, read)?;
            if task.is_some() && plan.after.report()?.order.contains(&entry) {
                return Err("WinPE 续接需要独立于常规启动顺序的启动项。".into());
            }
            let (task, task_preview) = task.unzip();
            let token = uuid::Uuid::new_v4().to_string();
            let preview = WinPePreview {
                token: token.clone(),
                entry,
                creates_entry,
                media: media.media.clone(),
                task: task_preview,
            };
            *WINPE_PENDING.lock().map_err(|_| "固件启动项操作已中断。")? = Some(Ticket::new(
                token,
                WinPePending {
                    plan,
                    source: retained_source,
                    media,
                    executable: retained_executable,
                    entry,
                    task,
                },
            ));
            Ok(preview)
        })
    })
}

pub(super) fn winpe_restart(token: String) -> Result<WinPeRestart, String> {
    let WinPePending {
        plan,
        source,
        media,
        executable,
        entry,
        task,
    } = {
        let mut pending = WINPE_PENDING.lock().map_err(|_| "固件启动项操作已中断。")?;
        consume(&mut pending, &token, Instant::now())?
    };
    loader::verify_winpe(&source, &executable, move |fresh| {
        if fresh != media {
            return Err("WinPE 分区布局或启动文件已有变化，请重新预览。".into());
        }
        if let Some(task) = &task {
            task.revalidate()?;
        }
        with_firmware(move || {
            enable_assigned_privilege(
                w!("SeShutdownPrivilege"),
                "当前进程没有请求 Windows 重启所需的权限。",
            )?;
            let value = task
                .map(|task| {
                    let entry_bytes = &plan
                        .after
                        .boot_entry(entry)
                        .ok_or("WinPE 启动项缺失。")?
                        .bytes;
                    let record = Armed::new(entry, entry_bytes, media.media.app_sha256, task)?;
                    Ok::<_, String>(Value {
                        attributes: BOOT_VARIABLE_ATTRIBUTES,
                        bytes: record.encode()?,
                    })
                })
                .transpose()?;
            let _handoff = HandoffLock::acquire()?;
            plan.verify_current(read, catalog::enumerate)?;
            clear_handoff()?;
            let report = plan.apply_catalog(
                read,
                |key, replacement| {
                    if let (Variable::Next, Some(value)) = (key, &value) {
                        update_handoff(Some(value))?;
                    }
                    write(key, replacement)
                },
                catalog::enumerate,
            );
            let report = match (report, &value) {
                (Err(error), Some(value)) => {
                    let cleanup = read_handoff().and_then(|current| {
                        if current.as_ref() == Some(value) {
                            update_handoff(None)?;
                        }
                        Ok(())
                    });
                    if let Err(cleanup) = cleanup {
                        return Err(format!(
                            "{error}\n{} {cleanup}",
                            native_locale::text("无法清除本次 WinPE 续接记录。")
                        ));
                    }
                    return Err(error);
                }
                (report, _) => report?,
            };
            let restart = unsafe {
                InitiateSystemShutdownExW(
                    null(),
                    null(),
                    0,
                    0,
                    1,
                    SHTDN_REASON_MAJOR_APPLICATION | SHTDN_REASON_FLAG_PLANNED,
                )
            };
            let error = (restart == 0).then(|| std::io::Error::last_os_error().to_string());
            Ok(WinPeRestart { report, error })
        })
    })
}

pub(super) fn claim_winpe_task() -> Result<Option<Task>, String> {
    let mut kind = FirmwareTypeUnknown;
    if unsafe { GetFirmwareType(&mut kind) } == 0 || kind != FirmwareTypeUefi {
        return Ok(None);
    }
    with_firmware(|| {
        let _handoff = HandoffLock::acquire()?;
        let Some(value) = read_handoff()? else {
            return Ok(None);
        };
        if value.attributes != BOOT_VARIABLE_ATTRIBUTES {
            return Err("WinPE 续接记录属性不符。".into());
        }
        let armed = Armed::decode(&value.bytes)?;
        let snapshot = Snapshot::capture_catalog(read, catalog::enumerate)?;
        let report = snapshot.report()?;
        if report.next.is_some() {
            return Ok(None);
        }
        if report.current != Some(armed.entry) || report.order.contains(&armed.entry) {
            update_handoff(None)?;
            return Ok(None);
        }
        let verify = snapshot
            .boot_entry(armed.entry)
            .ok_or_else(|| "WinPE 启动项缺失。".to_owned())
            .and_then(|entry| {
                armed.verify(
                    &entry.bytes,
                    &std::env::current_exe().map_err(data_windows::err)?,
                )
            });
        update_handoff(None)?;
        verify?;
        Ok(Some(armed.task))
    })
}

pub(super) fn backup(request: BackupRequest) -> Result<Option<BackupResult>, String> {
    let name = format!("Boot{:04X}.dsuefi", request.entry);
    let Some(path) = rfd::FileDialog::new()
        .set_title(native_locale::text("备份 UEFI 启动项"))
        .set_file_name(&name)
        .add_filter("DiskStudio UEFI", &["dsuefi"])
        .save_file()
    else {
        return Ok(None);
    };
    let bytes = with_firmware(move || {
        let snap = Snapshot::capture_catalog(read, catalog::enumerate)?;
        snap.check_revision(&request.revision)?;
        archive::encode(snap.backup_entry(request.entry)?)
    })?;
    let (mut file, publication) =
        backup_files::create_firmware_entry_backup(&path, bytes.len() as u64)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(data_windows::err)?;
    file.seek(SeekFrom::Start(0)).map_err(data_windows::err)?;
    let mut readback = Vec::new();
    (&mut file)
        .take(archive::MAX_ARCHIVE_SIZE as u64 + 1)
        .read_to_end(&mut readback)
        .map_err(data_windows::err)?;
    if readback != bytes {
        return Err("UEFI 启动项备份回读不一致。".into());
    }
    archive::decode(&readback)?;
    drop(file);
    publication.publish()?;
    let name = path
        .file_name()
        .ok_or("备份文件名无效。")?
        .to_string_lossy()
        .into_owned();
    Ok(Some(BackupResult {
        name,
        sha256: format!("{:x}", Sha256::digest(readback)),
    }))
}

pub(super) fn discard(token: &str) {
    if let Ok(mut pending) = PENDING.lock() {
        if pending.as_ref().is_some_and(|ticket| ticket.token == token) {
            pending.take();
        }
    }
    if let Ok(mut pending) = WINPE_PENDING.lock() {
        if pending.as_ref().is_some_and(|ticket| ticket.token == token) {
            pending.take();
        }
    }
}

#[cfg(test)]
#[path = "winpe_restart_lab.rs"]
mod winpe_restart_lab;

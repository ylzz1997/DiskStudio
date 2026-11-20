//! Native dialogs, immutable confirmation, and identity-checked offline moves.
use super::{Outcome, Preview, Request};
use crate::{
    data_io::Region,
    data_tasks::{self, Selection},
    data_windows as device, extfs,
    partition_move::Plan,
    partitions::{platform, policy, policy::Partition, Target},
    table_archive,
};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt as _};
use disk_core::partition_move::{Journal, Stage};
use std::{
    fs::File,
    io,
    os::windows::fs::MetadataExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT},
    System::Ioctl::{IOCTL_DISK_GET_DISK_ATTRIBUTES, IOCTL_DISK_UPDATE_PROPERTIES},
};

const JOURNAL_BYTES: u64 = 3 * disk_core::MIB;
const NAME: &str = "DiskStudio-move.dsmove";

struct JournalDirectory {
    dir: Dir,
    directory_binding: device::FileBinding,
    target: Target,
}

pub(crate) struct JournalLocation {
    directory: Arc<JournalDirectory>,
    name: String,
}

impl JournalLocation {
    fn open(path: &Path, source: &Target) -> Result<Self, String> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| {
                n.len() <= 180
                    && !n.contains([':', '/', '\\'])
                    && n.to_ascii_lowercase().ends_with(".dsmove")
            })
            .ok_or("请选择普通 .dsmove 文件。")?
            .to_owned();
        let directory = Self::open_directory(path.parent().ok_or("断点目录无效。")?, source)?;
        Ok(Self { directory, name })
    }

    fn open_directory(path: &Path, source: &Target) -> Result<Arc<JournalDirectory>, String> {
        let (folder, target) = device::folder_location(path)?;
        if &target == source {
            return Err("断点记录须保存在另一块物理磁盘上。".into());
        }
        let dir =
            Dir::open_ambient_dir(folder, cap_std::ambient_authority()).map_err(device::err)?;
        let directory_binding = device::FileBinding::new(
            &dir.try_clone().map_err(device::err)?.into_std_file(),
            &target,
        )?;
        Ok(Arc::new(JournalDirectory {
            dir,
            directory_binding,
            target,
        }))
    }

    pub(crate) fn select_for_batch(
        source: &Target,
        count: usize,
    ) -> Result<Option<Vec<Self>>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(
                "在另一块物理磁盘上保存批处理移动断点",
            ))
            .pick_folder()
        else {
            return Ok(None);
        };
        let directory = Self::open_directory(&path, source)?;
        let batch_id = uuid::Uuid::new_v4().simple();
        let mut locations = Vec::with_capacity(count);
        for index in 1..=count {
            let location = Self {
                directory: Arc::clone(&directory),
                name: format!("DiskStudio-{batch_id}-{index}.dsmove"),
            };
            location.ensure_unused()?;
            locations.push(location);
        }
        Ok(Some(locations))
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn ensure_unused(&self) -> Result<(), String> {
        device::FileBinding::validate_all(std::slice::from_ref(&self.directory.directory_binding))?;
        match self.directory.dir.symlink_metadata(&self.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err("所选断点文件已存在，请选择新位置。".into()),
            Err(error) => Err(device::err(error)),
        }
    }

    fn file(&self, create: bool) -> Result<(File, device::FileBinding), String> {
        device::FileBinding::validate_all(std::slice::from_ref(&self.directory.directory_binding))?;
        let file = self
            .directory
            .dir
            .open_with(
                &self.name,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(create)
                    .share_mode(0)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT),
            )
            .map_err(device::err)?
            .into_std();
        let meta = file.metadata().map_err(device::err)?;
        if !meta.is_file()
            || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || meta.len() > JOURNAL_BYTES
        {
            return Err("断点记录须为大小受限的普通文件。".into());
        }
        let binding = device::FileBinding::new(&file, &self.directory.target)?;
        device::FileBinding::validate_all(&[
            self.directory.directory_binding.clone(),
            binding.clone(),
        ])?;
        Ok((file, binding))
    }
}

enum Input {
    New {
        request: Request,
        plan: Plan,
        location: JournalLocation,
    },
    Resume {
        source: Selection,
        plan: Plan,
        journal: Journal<File>,
        binding: device::FileBinding,
        name: String,
        committed: usize,
    },
}

struct Ticket {
    token: String,
    expires: Instant,
    fingerprint: String,
    input: Input,
}

static PENDING: Mutex<Option<Ticket>> = Mutex::new(None);

pub(super) fn discard(token: &str) {
    let mut ticket = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if ticket.as_ref().is_some_and(|t| t.token == token) {
        *ticket = None;
    }
}

fn issue(target: String, fingerprint: String, input: Input) -> Preview {
    let (range, name, resuming, copied_bytes, completed) = match &input {
        Input::New { plan, location, .. } => (plan.layout.range, &location.name, false, 0, false),
        Input::Resume {
            plan,
            journal,
            name,
            ..
        } => (
            plan.layout.range,
            name,
            true,
            journal.copied_bytes(),
            journal.completed(),
        ),
    };
    let token = uuid::Uuid::new_v4().to_string();
    let preview = Preview {
        token: token.clone(),
        target,
        source_offset: range.source(),
        destination_offset: range.destination(),
        size: range.length(),
        overlap: range.overlaps(),
        journal_name: name.clone(),
        resuming,
        copied_bytes,
        completed,
    };
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ticket {
        token,
        expires: Instant::now() + Duration::from_secs(300),
        fingerprint,
        input,
    });
    preview
}

fn operation() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。".into())
}

pub(super) fn prepare(request: Request) -> Result<Option<Preview>, String> {
    let _guard = operation()?;
    let context = platform::sector_context(&request.source, None)?;
    let Some(path) = rfd::FileDialog::new()
        .set_title(crate::native_locale::text("在另一块物理磁盘上保存移动断点"))
        .set_file_name(NAME)
        .add_filter(
            crate::native_locale::text("DiskStudio 移动断点"),
            &["dsmove"],
        )
        .save_file()
    else {
        return Ok(None);
    };
    let location = JournalLocation::open(&path, &request.source.target)?;
    location.ensure_unused()?;
    let mut file = device::device(&context.resolved, false)?;
    let plan = read_plan(&request, &context, &mut file, &AtomicBool::new(false))?;
    plan.layout.before.matches(&mut file, || Ok(()))?;
    Ok(Some(issue(
        context.resolved.label,
        context.fingerprint,
        Input::New {
            request,
            plan,
            location,
        },
    )))
}

fn read_plan(
    request: &Request,
    context: &platform::SectorContext,
    file: &mut File,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    let disk = &context.resolved.disk;
    let selected = &context.selected;
    let sector = disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?;
    let parts = context.parts.iter().collect::<Vec<_>>();
    let native = extfs::native_layout(file, &parts)?;
    extfs::check_native_data_target(&native, selected)?;
    let guid = policy::guid(selected.guid.as_deref().ok_or("所选分区 GUID 缺失。")?);
    let archive = table_archive::capture(
        file,
        disk.size,
        sector,
        request.source.target.0.clone(),
        false,
        || check(cancel),
    )?;
    let mut volume = Region::new(&*file, selected.offset, selected.size).map_err(device::err)?;
    let plan = Plan::prepare(
        archive,
        &guid,
        request.destination,
        &mut volume,
        context
            .selected_filesystem()
            .ok_or("未找到可核对的目标卷。")?,
        cancel,
    )?;
    if !extfs::restore_model(&native, sector)?.agrees(&plan.layout.old_layout) {
        return Err("主备 GPT 与 Windows 原生完整布局不同。".into());
    }
    Ok(plan)
}

/// Owner holds the global operation guard and issued a single batch confirmation.
/// This does not change disk state after `start` finishes the verified move.
pub(crate) fn move_for_batch(
    request: Request,
    context: &platform::SectorContext,
    expected: &table_archive::Archive,
    location: JournalLocation,
    id: &str,
    cancel: &AtomicBool,
) -> Result<table_archive::Archive, String> {
    check(cancel)?;
    location.ensure_unused()?;
    let mut file = device::device(&context.resolved, false)?;
    let plan = read_plan(&request, context, &mut file, cancel)?;
    if &plan.layout.before != expected {
        return Err("移动前主备 GPT 与上一已核对步骤不一致。".into());
    }
    let result = plan.layout.result_archive().clone();
    drop(file);
    start(request, plan, location, &context.fingerprint, id, cancel)?;
    Ok(result)
}

pub(super) fn resume(source: Selection) -> Result<Option<Preview>, String> {
    let _guard = operation()?;
    let (context, disk_guid) = platform::gpt_table_read_context(&source, None)?;
    if context.resolved.disk.is_offline != Some(true) {
        return Err("请先选择仍处于脱机状态的移动目标磁盘。".into());
    }
    let Some(path) = rfd::FileDialog::new()
        .set_title(crate::native_locale::text("打开另一块磁盘上的移动断点"))
        .add_filter(
            crate::native_locale::text("DiskStudio 移动断点"),
            &["dsmove"],
        )
        .pick_file()
    else {
        return Ok(None);
    };
    let location = JournalLocation::open(&path, &source.target)?;
    let (file, binding) = location.file(false)?;
    let journal = Journal::open(file, flush)?;
    let plan = Plan::resume(&journal, unprotect)?;
    let mut device = device::device(&context.resolved, false)?;
    let committed = verify_resume(&mut device, &context, &source, &plan, &disk_guid)?;
    Ok(Some(issue(
        context.resolved.label,
        context.fingerprint,
        Input::Resume {
            source,
            plan,
            journal,
            binding,
            name: location.name,
            committed,
        },
    )))
}

fn verify_resume(
    file: &mut File,
    context: &platform::TableReadContext,
    source: &Selection,
    plan: &Plan,
    disk_guid: &str,
) -> Result<usize, String> {
    let disk = &context.resolved.disk;
    let sector = plan.layout.range.sector();
    if source.target.0 != plan.layout.before.source
        || disk.logical_sector_size != Some(sector)
        || disk.size != plan.layout.range.capacity()
        || disk_guid != plan.layout.old_layout.guid
    {
        return Err("断点记录与当前磁盘的身份、容量、扇区大小或 GPT GUID 不同。".into());
    }
    if disk.is_offline != Some(true) || !context.resolved.volumes.is_empty() {
        return Err("续作要求整个目标磁盘脱机且没有已关联的卷。".into());
    }
    check_offline(file)?;
    let parts = context.parts.iter().collect::<Vec<_>>();
    let selected = parts
        .iter()
        .find(|p| p.guid.as_deref().map(policy::guid).as_ref() == Some(&plan.partition_guid))
        .ok_or("续作目标分区未在 Windows 布局中确认。")?;
    check_resume_partition_flags(selected)?;
    // Offline Windows disks can omit partition attributes. The current native
    // target and exact authenticated GPT sectors must both corroborate them.
    let native = extfs::native_layout(file, &parts)?;
    extfs::check_native_data_target(&native, selected)?;
    let native = extfs::restore_model(&native, sector)?;
    if !native.agrees(&plan.layout.old_layout) && !native.agrees(&plan.layout.new_layout) {
        return Err("Windows 原生布局与断点的原始及目标布局都不同。".into());
    }
    plan.layout.committed_ranges(file)
}

fn check_resume_partition_flags(selected: &Partition) -> Result<(), String> {
    if selected.is_boot != Some(false)
        || selected.is_system != Some(false)
        || [
            selected.is_hidden,
            selected.is_read_only,
            selected.is_shadow_copy,
        ]
        .contains(&Some(true))
    {
        return Err("续作目标分区受 Windows 保护，或系统与启动状态不完整。".into());
    }
    Ok(())
}

pub(super) fn execute(token: String) -> Result<Outcome, String> {
    let _guard = operation()?;
    let ticket = take(
        &mut PENDING.lock().unwrap_or_else(|e| e.into_inner()),
        &token,
        Instant::now(),
    )?;
    let (id, cancel) = data_tasks::start("移动分区")?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match ticket.input {
        Input::New {
            request,
            plan,
            location,
        } => start(request, plan, location, &ticket.fingerprint, &id, &cancel),
        Input::Resume {
            source,
            plan,
            journal,
            binding,
            name,
            committed,
        } => {
            let (context, guid) = platform::gpt_table_read_context(&source, None)?;
            if context.fingerprint != ticket.fingerprint {
                return Err("续作预览后的磁盘状态已变化。".into());
            }
            device::FileBinding::validate_all(std::slice::from_ref(&binding))?;
            let mut file = if journal.completed() {
                device::device(&context.resolved, false)?
            } else {
                device::exclusive_device(&context.resolved)?
            };
            if verify_resume(&mut file, &context, &source, &plan, &guid)? != committed {
                return Err("续作预览后 GPT 状态已变化。".into());
            }
            complete(&mut file, &plan, journal, &binding, &name, &id, &cancel)
        }
    }))
    .unwrap_or_else(|_| Err("移动任务异常结束。若磁盘已脱机，请保留并检查断点记录。".into()));
    data_tasks::finish(
        &id,
        result
            .as_ref()
            .map(|o| (o.message.clone(), Some(o.sha256.clone())))
            .map_err(Clone::clone),
    );
    result
}

fn take(pending: &mut Option<Ticket>, token: &str, now: Instant) -> Result<Ticket, String> {
    let ticket = pending.take().ok_or("移动确认已失效，请重新预览。")?;
    if ticket.expires <= now || ticket.token != token {
        return Err("移动确认无效或已过期。".into());
    }
    Ok(ticket)
}

fn start(
    request: Request,
    confirmed: Plan,
    location: JournalLocation,
    fingerprint: &str,
    id: &str,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    check(cancel)?;
    location.ensure_unused()?;
    let context = platform::sector_context(&request.source, None)?;
    if context.fingerprint != fingerprint {
        return Err("移动预览后磁盘或完整卷关联已变化。".into());
    }
    let locks = device::lock(&context.resolved)?;
    let fresh = platform::sector_context(&request.source, Some((&context, &locks)))?;
    let mut file = device::exclusive_device(&fresh.resolved)?;
    let mut plan = read_plan(&request, &fresh, &mut file, cancel)?;
    if plan.metadata() != confirmed.metadata() {
        return Err("锁定后的卷或 GPT 与确认内容不同。".into());
    }
    plan.layout.before.matches(&mut file, || check(cancel))?;
    check(cancel)?;
    device::persist_offline(&file).map_err(|e| format!("{e} 磁盘可能已脱机，尚未搬移数据。"))?;
    let result = (|| {
        check_offline(&file)?;
        let range = plan.layout.range;
        let source_hash = range.source_sha256(&mut file, |n| {
            progress(id, cancel, "计算原始分区摘要", n, range.length())
        })?;
        plan.seal(source_hash, protect)?;
        let (journal_file, binding) = location.file(true)?;
        let journal = Journal::create(
            journal_file,
            flush,
            range,
            source_hash,
            plan.metadata().to_vec(),
        )?;
        let _ = Plan::resume(&journal, unprotect)?;
        Ok::<_, String>((journal, binding))
    })()
    .map_err(|e| format!("{e} 已请求持久脱机，尚未搬移分区数据。"))?;
    complete(
        &mut file,
        &plan,
        result.0,
        &result.1,
        &location.name,
        id,
        cancel,
    )
}

fn complete(
    file: &mut File,
    plan: &Plan,
    mut journal: Journal<File>,
    binding: &device::FileBinding,
    name: &str,
    id: &str,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    let completed = journal.completed();
    let result = (|| {
        check_offline(file)?;
        device::FileBinding::validate_all(std::slice::from_ref(binding))?;
        let total = plan.layout.range.length();
        plan.transfer(
            file,
            &mut journal,
            flush,
            |stage, n| {
                let label = match stage {
                    Stage::CheckSource => "校验已搬移和剩余源数据",
                    Stage::Copy => "搬移与逐块回读",
                    Stage::CheckDestination => "核对完整目标数据",
                };
                progress(id, cancel, label, n, total)
            },
            || {
                let phase = if completed {
                    "复核已完成的 GPT"
                } else {
                    "提交并核对 GPT"
                };
                progress(id, cancel, phase, total, total)
            },
        )?;
        if !completed {
            device::control(file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])?;
        }
        if !extfs::restore_native(file, None, plan.layout.range.sector())?
            .0
            .agrees(&plan.layout.new_layout)
        {
            return Err("完成后的 Windows 原生 GPT 与确认布局不同。".into());
        }
        check_offline(file)?;
        device::FileBinding::validate_all(std::slice::from_ref(binding))?;
        if !completed {
            journal.mark_completed()?;
        }
        let sha256 = journal
            .source_hash()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        Ok(Outcome {
            message: if completed {
                "已复核断点中的完整数据、卷引导区及主备 GPT。磁盘保持脱机。"
            } else {
                "分区已移动并核对完整数据、卷引导区及主备 GPT。磁盘保持脱机。"
            }
            .into(),
            sha256,
        })
    })();
    result.map_err(|e: String| {
        if completed {
            format!("{e} 断点复核未通过。请保留 {name}。")
        } else {
            format!("{e} 磁盘保持脱机，可能已有部分写入。请保留 {name}，通过移动续作核对现状。")
        }
    })
}

fn check_offline(file: &File) -> Result<(), String> {
    let mut bytes = [0; 16];
    if device::control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut bytes)? < 16
        || u64::from_le_bytes(bytes[8..16].try_into().unwrap()) & 3 != 1
    {
        return Err("无法确认设备保持脱机且可写。".into());
    }
    Ok(())
}

fn check(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("移动任务已停止。".into())
    } else {
        Ok(())
    }
}

fn progress(
    id: &str,
    cancel: &AtomicBool,
    phase: &str,
    done: u64,
    total: u64,
) -> Result<(), String> {
    check(cancel)?;
    data_tasks::record_progress(id, phase, done, total);
    Ok(())
}

fn flush(file: &mut File) -> io::Result<()> {
    file.sync_all()
}

fn protect(input: &[u8; 32]) -> Result<Vec<u8>, String> {
    dpapi(input, true)
}
fn unprotect(input: &[u8]) -> Result<[u8; 32], String> {
    dpapi(input, false)?
        .try_into()
        .map_err(|_| "断点中的 Windows 来源凭据无效。".into())
}

fn dpapi(input: &[u8], seal: bool) -> Result<Vec<u8>, String> {
    crate::windows_protection::protect(input, seal, None, 4096)
        .map_err(|_| "无法核对当前 Windows 用户的移动记录来源凭据。".into())
}

#[cfg(test)]
#[test]
fn resume_partition_flags_reject_reported_protection_or_unknown_boot_role() {
    let partition = Partition {
        is_boot: Some(false),
        is_system: Some(false),
        ..Default::default()
    };
    assert!(check_resume_partition_flags(&partition).is_ok());
    for protected in [
        Partition {
            is_hidden: Some(true),
            ..partition.clone()
        },
        Partition {
            is_read_only: Some(true),
            ..partition.clone()
        },
        Partition {
            is_shadow_copy: Some(true),
            ..partition.clone()
        },
        Partition {
            is_boot: None,
            ..partition.clone()
        },
        Partition {
            is_system: Some(true),
            ..partition.clone()
        },
    ] {
        assert!(check_resume_partition_flags(&protected).is_err());
    }
}

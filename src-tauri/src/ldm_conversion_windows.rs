//! Explicit, single-use conversion of an already offline physical GPT disk.
use super::{
    devices, platform, ConversionOutcome, ConversionPreview, ConversionVolume, Members, Saved,
};
use crate::{
    data_io::{hash_exact, Region},
    data_tasks, data_windows as device,
    ldm_basic::Plan,
    table_archive::{self, Archive},
    table_restore,
};
use disk_core::ldm::{self, Member, SimpleExtent};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_UPDATE_PROPERTIES;

struct Pending {
    token: String,
    expires: Instant,
    source: Arc<Saved>,
    plan: Plan,
    volumes: Vec<ConversionVolume>,
}

fn pending() -> MutexGuard<'static, Option<Pending>> {
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    PENDING.lock().unwrap_or_else(|error| error.into_inner())
}

pub(super) fn discard(token: &str) {
    let mut state = pending();
    if state.as_ref().is_some_and(|entry| entry.token == token) {
        *state = None;
    }
}

fn selected(source: &Saved) -> Result<&devices::Member, String> {
    if let Members::Disks(members) = &source.members {
        if let [member] = members.as_slice() {
            return Ok(member);
        }
    }
    Err("转换只接受单个已核对的脱机物理磁盘。".into())
}

fn stop(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("已取消转换，未写入分区表。".into())
    } else {
        Ok(())
    }
}

struct Snapshot {
    archive: Archive,
    extents: Vec<SimpleExtent>,
    volumes: Vec<ConversionVolume>,
}

fn snapshot(
    source: &Saved,
    member: &devices::Member,
    file: &File,
    cancel: &AtomicBool,
    corroborate_wmi: bool,
) -> Result<Snapshot, String> {
    stop(cancel)?;
    device::confirm_writable_offline(file)?;
    let mut disk = Region::new(file, 0, member.size).map_err(device::err)?;
    let group = ldm::open(
        vec![Member {
            reader: &mut disk,
            sector_size: member.sector,
        }],
        || stop(cancel),
    )?;
    if group.fingerprint() != source.fingerprint {
        return Err("动态磁盘布局或数据库已变化，请重新检查。".into());
    }
    let extents = group.standalone_simple_extents()?;
    drop(group);
    let archive = table_archive::capture(
        &mut disk,
        member.size,
        member.sector,
        member.target.0.clone(),
        false,
        || stop(cancel),
    )?;
    let layout = table_restore::archive_layout(&archive)?;
    let (native, services, _) = crate::extfs::restore_native(file, None, member.sector)?;
    if !layout.agrees(&native)
        || services.iter().any(|id| {
            !layout.entries.iter().any(|entry| {
                &entry.guid == id
                    && matches!(
                        entry.kind.as_str(),
                        crate::ldm_basic::LDM_METADATA | crate::ldm_basic::MICROSOFT_RESERVED
                    )
            })
        })
    {
        return Err("原生 GPT 记录与动态盘布局不一致，或包含受保护的服务分区。".into());
    }
    if corroborate_wmi {
        member.conversion_layout(&layout)?;
    }
    let volumes = extents
        .iter()
        .map(|extent| verify_volume(file, member.sector, extent, cancel))
        .collect::<Result<_, _>>()?;
    device::confirm_writable_offline(file)?;
    Ok(Snapshot {
        archive,
        extents,
        volumes,
    })
}

fn verify_volume(
    disk: &File,
    sector: u64,
    extent: &SimpleExtent,
    cancel: &AtomicBool,
) -> Result<ConversionVolume, String> {
    stop(cancel)?;
    let mut volume = Region::for_metadata(disk, extent.offset, extent.size).map_err(device::err)?;
    let first = table_archive::read_at(&mut volume, 0, sector as usize, extent.size)?;
    let last_offset = extent
        .size
        .checked_sub(sector)
        .ok_or("动态卷小于逻辑扇区。")?;
    let last = table_archive::read_at(&mut volume, last_offset, sector as usize, extent.size)?;
    let observed_sector = if &first[3..11] == b"EXFAT   " {
        1u64.checked_shl(u32::from(first[108]))
    } else {
        Some(u64::from(u16::from_le_bytes([first[11], first[12]])))
    };
    if observed_sector != Some(sector) {
        return Err("文件系统扇区大小与动态卷不一致。".into());
    }
    let allocation = if &first[3..11] == b"NTFS    " {
        crate::ntfs_allocation::inspect(&mut volume, cancel)?
    } else {
        crate::fat_recovery::allocation::inspect(&mut volume, cancel)?
    };
    let mut digest = Sha256::new();
    digest.update(first);
    digest.update(last);
    Ok(ConversionVolume {
        name: extent.name.clone(),
        offset: extent.offset,
        size: extent.size,
        filesystem: allocation.filesystem,
        boot_digest: digest.finalize().into(),
    })
}

pub(super) fn prepare(session_id: &str) -> Result<ConversionPreview, String> {
    let _operation = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    *pending() = None;
    let source = platform::inspected(session_id)?;
    source.members.validate()?;
    let member = selected(&source)?;
    let file = member.conversion_file(false)?;
    let snapshot = snapshot(&source, member, &file, &AtomicBool::new(false), true)?;
    let plan = Plan::new(snapshot.archive, &snapshot.extents)?;
    source.members.validate()?;
    let preview = ConversionPreview {
        token: uuid::Uuid::new_v4().to_string(),
        disk: member.number,
        model: member.model.clone(),
        volumes: snapshot.volumes.clone(),
    };
    *pending() = Some(Pending {
        token: preview.token.clone(),
        expires: Instant::now() + Duration::from_secs(300),
        source,
        plan,
        volumes: snapshot.volumes,
    });
    Ok(preview)
}

pub(super) fn execute(token: &str) -> Result<ConversionOutcome, String> {
    let _operation = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let pending = pending()
        .take()
        .filter(|entry| entry.token == token && Instant::now() < entry.expires)
        .ok_or("转换确认已过期或不匹配，请重新预览。")?;
    let (id, cancel) = data_tasks::start("转换为基本磁盘")?;
    let result = apply(pending, &id, &cancel);
    data_tasks::finish(
        &id,
        result
            .as_ref()
            .map(|_| {
                (
                    "动态盘已转为 GPT 基本磁盘，卷内容 SHA-256 校验一致。磁盘保持脱机。".into(),
                    None,
                )
            })
            .map_err(Clone::clone),
    );
    result
}

fn apply(pending: Pending, id: &str, cancel: &AtomicBool) -> Result<ConversionOutcome, String> {
    let source = &pending.source;
    platform::inspected(&source.session_id)?;
    source.members.validate()?;
    let member = selected(source)?;
    let preview_matches = |snapshot: &Snapshot| -> Result<(), String> {
        if !pending.plan.matches(&snapshot.archive) || snapshot.volumes != pending.volumes {
            return Err("磁盘布局或文件系统与预览不一致，请重新预览。".into());
        }
        Ok(())
    };
    {
        let file = member.conversion_file(false)?;
        preview_matches(&snapshot(source, member, &file, cancel, true)?)?;
    }
    source.members.validate()?;
    let mut file = member.conversion_file(true)?;
    preview_matches(&snapshot(source, member, &file, cancel, false)?)?;
    let total = pending
        .volumes
        .iter()
        .map(|volume| volume.size)
        .sum::<u64>();
    let baseline = content_hashes(&file, &pending.volumes, id, total, Some(cancel))?;
    stop(cancel)?;
    *platform::state() = None;
    // Finish the short GPT commit and verify even if cancellation arrives
    // after the first write. No new write is attempted if any range fails.
    let commit = (|| {
        pending.plan.write(
            &mut file,
            |file| file.sync_all().map_err(device::err),
            device::confirm_writable_offline,
        )?;
        device::control(&file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])?;
        let native = crate::extfs::restore_native(&file, None, member.sector)?.0;
        if !native.agrees(&pending.plan.layout) {
            return Err("转换后的 Windows 原生 GPT 与计划不一致。".into());
        }
        pending.plan.verify(&mut file, || Ok(()))?;
        if baseline != content_hashes(&file, &pending.volumes, id, total, None)? {
            return Err("转换后卷内容 SHA-256 与写入前不一致。".into());
        }
        device::confirm_writable_offline(&file)
    })();
    commit.map_err(|error: String| format!("{error} 可能已写入部分 GPT，请核对磁盘状态。"))?;
    Ok(ConversionOutcome {
        verified_bytes: total,
        partitions: pending.volumes.len(),
    })
}

fn content_hashes(
    file: &File,
    volumes: &[ConversionVolume],
    id: &str,
    total: u64,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<String>, String> {
    let mut done = 0;
    let mut hashes = Vec::with_capacity(volumes.len());
    for volume in volumes {
        let mut reader = Region::new(file, volume.offset, volume.size).map_err(device::err)?;
        let hash = hash_exact(&mut reader, volume.size, |position| {
            if let Some(cancel) = cancel {
                stop(cancel)?;
            }
            device::confirm_writable_offline(file)?;
            let (phase, base) = if cancel.is_some() {
                ("校验源数据", 0)
            } else {
                ("回读校验", total)
            };
            data_tasks::record_progress(id, phase, base + done + position, 2 * total);
            Ok(())
        })?;
        hashes.push(hash);
        done += volume.size;
    }
    Ok(hashes)
}

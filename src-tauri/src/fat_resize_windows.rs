//! Confirmed offline FAT/exFAT end-boundary changes on ordinary GPT data disks.
use crate::{
    data_io::Region,
    data_tasks::Selection,
    data_windows as device, disk_identity, extfs,
    fat_recovery::resize::{self, Plan},
    partitions::{policy, Action, Outcome, Request},
    table_archive, table_restore,
    used_space::View,
};
use policy::{Disk, Partition};
use sha2::{Digest, Sha256};
use std::fs::File;
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_DISK_ATTRIBUTES;

pub(crate) fn supported(filesystem: Option<&str>) -> bool {
    filesystem.is_some_and(|s| {
        matches!(
            s.to_ascii_uppercase().as_str(),
            "FAT" | "FAT12" | "FAT16" | "FAT32" | "EXFAT"
        )
    })
}

fn bind(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    partition: &Partition,
) -> Result<(Selection, device::Resolved), String> {
    policy::sector_writable(disk, parts, partition)?;
    let selection = Selection {
        target: request.target.clone(),
        partition: Some(partition.partition_number),
        layout_key: disk_identity::layout_key(
            disk.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        ),
    };
    let resolved = device::resolve(&selection, true, true)?;
    resolved.verify_partition_snapshot(disk, parts)?;
    if !supported(resolved.partition_filesystem(partition.partition_number)) {
        return Err("Windows 未报告可核对的 FAT 或 exFAT 卷。".into());
    }
    Ok((selection, resolved))
}

fn size(request: &Request, p: &Partition) -> Result<u64, String> {
    match request.action {
        Action::FatResize { partition, size } if partition == p.partition_number => Ok(size),
        _ => Err("FAT 调整容量的确认目标不匹配。".into()),
    }
}

fn raw_layout(
    file: &mut File,
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    sector: u64,
) -> Result<table_restore::Layout, String> {
    let archive = table_archive::capture(
        file,
        disk.size,
        sector,
        request.target.0.clone(),
        false,
        || Ok(()),
    )?;
    let raw = table_restore::archive_layout(&archive)?;
    if !raw.agrees(&extfs::restore_native(file, Some(parts), sector)?.0) {
        return Err("主备 GPT 与 Windows 原生布局不一致。".into());
    }
    Ok(raw)
}

fn expected_layout(
    layout: &table_restore::Layout,
    partition: &Partition,
    new_size: u64,
    sector: u64,
) -> Result<table_restore::Layout, String> {
    let guid = policy::guid(partition.guid.as_deref().ok_or("分区 GUID 缺失。")?);
    let mut after = layout.clone();
    let entry = after
        .entries
        .iter_mut()
        .find(|e| e.guid == guid)
        .ok_or("原始 GPT 未找到目标分区。")?;
    if entry.kind != policy::BASIC_DATA
        || entry.offset != partition.offset
        || entry.size != partition.size
        || entry.attributes & !0x8000000000000000 != 0
    {
        return Err("GPT 分区类型、范围或保护属性无法核对。".into());
    }
    let offset = entry.offset;
    let end = offset.checked_add(new_size).ok_or("FAT 新容量溢出。")?;
    entry.size = new_size;
    let usable_end = layout
        .last
        .checked_add(1)
        .and_then(|lba| lba.checked_mul(sector));
    if usable_end.is_none_or(|limit| end > limit)
        || layout
            .entries
            .iter()
            .any(|other| other.offset > offset && other.offset < end)
    {
        return Err("新容量超出 GPT 可用区域或与其他分区重叠。".into());
    }
    Ok(after)
}

fn read_plan(
    file: &File,
    partition: &Partition,
    sector: u64,
    new_size: u64,
    reported: &str,
) -> Result<Plan, String> {
    let mut view = Region::new(file, partition.offset, partition.size).map_err(error)?;
    let plan = if reported.eq_ignore_ascii_case("exFAT") {
        resize::prepare_exfat(&mut view, sector, new_size)?
    } else {
        resize::prepare_classic(&mut view, sector, new_size)?
    };
    if !reported.eq_ignore_ascii_case(&plan.filesystem)
        && !(reported.eq_ignore_ascii_case("FAT")
            && matches!(plan.filesystem.as_str(), "FAT12" | "FAT16"))
    {
        return Err("Windows 报告的文件系统与原始引导扇区不一致。".into());
    }
    Ok(plan)
}

fn proof(layout: &table_restore::Layout, plan: &Plan) -> String {
    let mut hash = Sha256::new();
    hash.update(format!("{layout:?}").as_bytes());
    hash.update(plan.fingerprint());
    format!("{:x}", hash.finalize())
}

pub(crate) fn preview(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    partition: &Partition,
) -> Result<(String, String), String> {
    let new_size = size(request, partition)?;
    let (_, resolved) = bind(request, disk, parts, partition)?;
    let mut file = device::device(&resolved, false)?;
    let sector = disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?;
    let native = extfs::native_layout(&file, parts)?;
    extfs::check_native_data_target(&native, partition)?;
    let layout = raw_layout(&mut file, request, disk, parts, sector)?;
    expected_layout(&layout, partition, new_size, sector)?;
    let reported = resolved
        .partition_filesystem(partition.partition_number)
        .ok_or("未找到目标卷。")?;
    let plan = read_plan(&file, partition, sector, new_size, reported)?;
    if raw_layout(&mut file, request, disk, parts, sector)? != layout {
        return Err("准备期间 GPT 发生变化，请刷新并重新预览。".into());
    }
    Ok((proof(&layout, &plan), crate::native_locale::current().format(
        "{fs}：数据簇数 {old} → {new}，预计搬移 {count} 个簇。保留活动文件、目录和分区起点。扩容需要相邻的未分配空间，必要时扩大分配表并搬移数据区。",
        &[("fs", &plan.filesystem), ("old", &plan.old_clusters.to_string()),
          ("new", &plan.new_clusters.to_string()), ("count", &plan.moved_clusters().to_string())],
    )))
}

pub(crate) fn execute(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    partition: &Partition,
    confirmed: &str,
) -> Result<Outcome, String> {
    let new_size = size(request, partition)?;
    let (selection, resolved) = bind(request, disk, parts, partition)?;
    let locks = device::lock(&resolved)?;
    let locked = device::resolve_locked(&selection, true, &resolved, &locks)?;
    locked.verify_partition_snapshot(disk, parts)?;
    let mut file = device::exclusive_device(&locked)?;
    let sector = disk.logical_sector_size.ok_or("逻辑扇区大小未知。")?;
    let native = extfs::native_layout(&file, parts)?;
    extfs::check_native_data_target(&native, partition)?;
    let layout = raw_layout(&mut file, request, disk, parts, sector)?;
    let expected = expected_layout(&layout, partition, new_size, sector)?;
    let reported = locked
        .partition_filesystem(partition.partition_number)
        .ok_or("未找到锁定的目标卷。")?;
    let plan = read_plan(&file, partition, sector, new_size, reported)?;
    if proof(&layout, &plan) != confirmed {
        return Err("分区、引导元数据或活动分配已变化，请重新预览。".into());
    }
    let mut updated = parts.iter().map(|p| (*p).clone()).collect::<Vec<_>>();
    for p in &mut updated {
        if p.partition_number == partition.partition_number {
            p.size = new_size;
        }
    }
    let updated = updated.iter().collect::<Vec<_>>();
    device::offline(&file)?;
    let result = (|| {
        if raw_layout(&mut file, request, disk, parts, sector)? != layout {
            return Err("脱机后原 GPT 布局已变化。".into());
        }
        let grow = new_size > partition.size;
        if grow {
            extfs::resize_layout(&file, &native, partition, new_size)?;
            let actual = raw_layout(&mut file, request, disk, &updated, sector)?;
            if !actual.agrees(&expected) {
                return Err("扩展后的 GPT 布局核对失败，尚未更新文件系统。".into());
            }
        }
        plan.apply(
            &mut View::bounded(
                &mut file,
                partition.offset,
                partition.size.max(new_size),
                disk.size,
            )
            .map_err(error)?,
        )?;
        file.sync_all().map_err(error)?;
        plan.verify(&mut Region::new(&file, partition.offset, new_size).map_err(error)?)?;
        if !grow {
            extfs::resize_layout(&file, &native, partition, new_size)?;
        }
        if !raw_layout(&mut file, request, disk, &updated, sector)?.agrees(&expected) {
            return Err("最终 GPT 布局与预期不符。".into());
        }
        let mut attributes = [0; 16];
        if device::control(&file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attributes)? < 16
            || u64::from_le_bytes(attributes[8..16].try_into().unwrap()) & 3 != 1
        {
            return Err("无法确认磁盘保持脱机且可写。".into());
        }
        Ok::<_, String>(())
    })();
    result.map_err(|e| {
        crate::native_locale::diagnostic(
            "{error} 操作可能部分完成，磁盘不会自动联机。请检查文件系统与分区边界，不要直接重试。",
            &e,
        )
    })?;
    Ok(Outcome {
        verified: true,
        message: crate::native_locale::current().format(
            "{fs} 末端调整已完成。分区范围、活动目录、簇链和写入的元数据均已核对。已搬移并读回核对 {count} 个簇。磁盘保持脱机。",
            &[("fs", &plan.filesystem), ("count", &plan.moved_clusters().to_string())],
        ),
    })
}

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

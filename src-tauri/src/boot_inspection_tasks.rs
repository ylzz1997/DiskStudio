//! Bind an explicit boot inspection to its retained session or offline disk.
use super::{idle, state, Selection};
use crate::boot_inspection::{self, Report};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Request {
    Image { id: String },
    Backup { id: String, point: String },
    Disk { source: Selection },
}

pub(crate) fn inspect(request: Request) -> Result<Report, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    idle(&state())?;
    match request {
        Request::Image { id } => {
            let image = state()
                .opened_image
                .as_ref()
                .filter(|image| image.browser.info.id == id)
                .cloned()
                .ok_or("镜像浏览会话已失效。")?;
            let info = &image.browser.info;
            let partition = match info.options.kind {
                crate::image_browser::Kind::Disk => false,
                crate::image_browser::Kind::Partition => true,
                crate::image_browser::Kind::Search => {
                    return Err("请选择当前打开的整盘镜像或分区镜像。".into())
                }
            };
            image.validate()?;
            #[cfg(windows)]
            let reader = image.logical_reader()?;
            #[cfg(not(windows))]
            let reader = image.browser.logical_reader()?;
            let report = boot_inspection::inspect(
                reader,
                info.name.clone(),
                info.size,
                info.options.sector,
                partition,
                None,
            )?;
            image.validate()?;
            Ok(report)
        }
        Request::Backup { id, point } => {
            let backup = state()
                .opened_backup
                .as_ref()
                .filter(|backup| {
                    backup.browser.info.id == id && backup.browser.info.selected_point == point
                })
                .cloned()
                .ok_or("备份会话或还原点已变化。")?;
            let info = &backup.browser.info;
            backup.validate()?;
            let partition = info.kind != "disk";
            let range = partition.then_some(0);
            let label = backup.browser.scope_label(range)?;
            let report = boot_inspection::inspect(
                backup.browser.restore_reader(range)?,
                label,
                info.size,
                info.sector,
                partition,
                None,
            )?;
            backup.validate()?;
            Ok(report)
        }
        Request::Disk { source } => inspect_disk(source),
    }
}

#[cfg(windows)]
fn inspect_disk(source: Selection) -> Result<Report, String> {
    use crate::data_windows as device;
    if source.partition.is_some() {
        return Err("请选择经核对的脱机整盘。".into());
    }
    let before = device::resolve(&source, true, false)?;
    if before.disk.is_offline != Some(true) {
        return Err("请选择经核对的脱机整盘。".into());
    }
    let locks = device::state_locks(&before)?;
    let current = device::resolve_locked(&source, false, &before, &locks)?;
    let sector = current
        .disk
        .logical_sector_size
        .ok_or("逻辑扇区大小未知。")?;
    let reader = crate::data_io::Region::for_metadata(
        device::device(&current, false)?,
        0,
        current.size,
    )
    .map_err(|e| e.to_string())?;
    let report = boot_inspection::inspect(
        reader,
        current.label.clone(),
        current.size,
        sector,
        false,
        Some(&current.partition_ranges),
    )?;
    device::resolve_locked(&source, false, &current, &locks)?;
    Ok(report)
}

#[cfg(not(windows))]
fn inspect_disk(_: Selection) -> Result<Report, String> {
    Err("该操作需要 Windows。".into())
}

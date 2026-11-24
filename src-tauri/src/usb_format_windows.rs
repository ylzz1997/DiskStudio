//! One explicit batch confirmation. Each Windows command uses a freshly
//! checked provider target, never a drive letter or a user-provided path.
use super::{
    digest, err, execute_bound, policy, Action, PartitionKind, Request, Session, OPERATION,
};
use crate::{
    maintenance_tasks::{consume, Ticket},
    partitions::{
        quick::{Format, Phase, Progress},
        usb_format::{self as api, model, DevicePlan, Operation, Recipe, Selection},
    },
};
use std::{sync::Mutex, time::Instant};

static PENDING: Mutex<Option<Ticket<Batch>>> = Mutex::new(None);

struct Prepared {
    selection: Selection,
    recipe: Recipe,
    binding: String,
    view: DevicePlan,
}

struct Batch {
    format: Format,
    devices: Vec<Prepared>,
}

fn serial(disk: &policy::Disk) -> String {
    disk.serial_number
        .as_deref()
        .map(policy::clean)
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn candidates() -> Result<Vec<api::Candidate>, String> {
    let _operation = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在执行")?;
    let session = Session::open()?;
    let mut devices: Vec<_> = session
        .disks
        .iter()
        .filter_map(|disk| {
            model::usb(disk).ok()?;
            let target = policy::identity(disk)?;
            let revision = session.revision(&target).ok()?;
            let parts = policy::layout(disk, &session.partitions).ok()?;
            let unpartitioned =
                parts.is_empty() && disk.is_boot == Some(false) && disk.is_system == Some(false);
            let partitions = parts
                .iter()
                .filter(|part| policy::formattable(part))
                .map(|part| api::PartitionChoice {
                    number: part.partition_number,
                    offset: part.offset,
                    size: part.size,
                    drive_letter: part.drive_letter.clone(),
                })
                .collect::<Vec<_>>();
            if partitions.is_empty() && !unpartitioned {
                return None;
            }
            Some(api::Candidate {
                target,
                revision,
                number: disk.number,
                serial: serial(disk),
                name: disk.friendly_name.clone().unwrap_or_default(),
                size: disk.size,
                sector_size: disk.logical_sector_size?,
                partitions,
                unpartitioned,
            })
        })
        .collect();
    devices.sort_by_key(|device| device.number);
    Ok(devices)
}

fn format_action(format: &Format, partition: u32) -> Action {
    Action::Format {
        partition,
        file_system: format.file_system,
        cluster_size: format.cluster_size,
        label: format.label.clone(),
        options: format.options,
    }
}

fn bound(session: &Session, selection: &Selection, format: &Format) -> Result<Prepared, String> {
    let disk = session.disk(&selection.target)?;
    if session.revision(&selection.target)? != selection.revision {
        return Err("USB 磁盘身份或布局已变化，请刷新后重新预览。".into());
    }
    let recipe = model::recipe(disk, &session.partitions, selection.partition, format)?;
    let (binding, volume) = match recipe {
        Recipe::Existing { partition, .. } => session.bind(&Request {
            target: selection.target.clone(),
            revision: selection.revision.clone(),
            action: format_action(format, partition),
        })?,
        Recipe::New { .. } => (digest(&(disk, &selection.revision))?, None),
    };
    let extent = recipe.extent().clone();
    let cluster_size = if let Some(kind) = format.file_system.fat_type() {
        disk_core::fat_format::Plan::new(
            kind,
            extent.size,
            disk.logical_sector_size.ok_or("缺少扇区大小")? as u32,
            format.cluster_size,
            &format.label,
            1,
            0,
        )
        .map_err(err)?
        .cluster_size()
    } else {
        format
            .options
            .effective_cluster(format.file_system, format.cluster_size)
    };
    let view = DevicePlan {
        number: disk.number,
        name: disk.friendly_name.clone().unwrap_or_default(),
        serial: serial(disk),
        partition: selection.partition,
        extent,
        operation: recipe.operation(),
        drive_letter: volume.as_ref().and_then(|v| v.drive_letter.clone()),
        old_file_system: volume.as_ref().and_then(|v| v.file_system.clone()),
        old_label: volume.and_then(|v| v.file_system_label),
        cluster_size,
    };
    Ok(Prepared {
        selection: selection.clone(),
        recipe,
        binding,
        view,
    })
}

pub(crate) fn prepare(mut request: api::Request) -> Result<api::Preview, String> {
    let _operation = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在执行")?;
    *PENDING.lock().map_err(|_| "预览状态不可用。")? = None;
    model::validate_selection(&request.devices)?;
    request.format.label = request.format.label.trim().to_owned();
    if request.format.file_system.fat_type().is_some() {
        request.format.label.make_ascii_uppercase();
    }
    let session = Session::open()?;
    let devices = request
        .devices
        .iter()
        .map(|selection| bound(&session, selection, &request.format))
        .collect::<Result<Vec<_>, _>>()?;
    let preview = api::Preview {
        token: uuid::Uuid::new_v4().to_string(),
        format: request.format.clone(),
        devices: devices.iter().map(|entry| entry.view.clone()).collect(),
    };
    *PENDING.lock().map_err(|_| "预览状态不可用。")? = Some(Ticket::new(
        preview.token.clone(),
        Batch {
            format: request.format,
            devices,
        },
    ));
    Ok(preview)
}

pub(crate) fn discard(token: &str) {
    if let Ok(mut pending) = PENDING.lock() {
        if pending.as_ref().is_some_and(|ticket| ticket.token == token) {
            pending.take();
        }
    }
}

fn recheck(session: &Session, device: &Prepared, format: &Format) -> Result<(), String> {
    let current = bound(session, &device.selection, format)?;
    if current.binding != device.binding || current.recipe != device.recipe {
        return Err("USB 磁盘、分区或卷已变化，请重新预览。".into());
    }
    Ok(())
}

fn act(
    session: Session,
    target: &api::Selection,
    action: Action,
    expected: Option<&str>,
) -> Result<(), String> {
    model::usb(session.disk(&target.target)?)?;
    let revision = if expected.is_some() {
        target.revision.clone()
    } else {
        session.revision(&target.target)?
    };
    let request = Request {
        target: target.target.clone(),
        revision,
        action,
    };
    let (binding, volume) = session.bind(&request)?;
    if expected.is_some_and(|old| old != binding) {
        return Err("USB 目标已变化，请重新预览。".into());
    }
    let outcome = execute_bound(request, session, binding, volume, None)?;
    if outcome.verified {
        Ok(())
    } else {
        Err(outcome.message)
    }
}

fn execute_device(
    device: &Prepared,
    format: &Format,
    completed: &mut Vec<Operation>,
) -> Result<(), String> {
    let selection = &device.selection;
    let session = Session::open()?;
    recheck(&session, device, format)?;
    match &device.recipe {
        Recipe::Existing { partition, .. } => {
            act(
                session,
                selection,
                format_action(format, *partition),
                Some(&device.binding),
            )?;
        }
        Recipe::New { initialize, extent } => {
            let ready = if *initialize {
                act(session, selection, Action::Initialize {}, None)?;
                completed.push(Operation::Initialize);
                Session::open()?
            } else {
                session
            };
            let disk = ready.disk(&selection.target)?;
            let parts = policy::layout(disk, &ready.partitions)?;
            let before_create = model::reserved(&parts, extent, *initialize)?;
            act(
                ready,
                selection,
                Action::Create {
                    offset: extent.offset,
                    size: extent.size,
                    kind: PartitionKind::Windows,
                },
                None,
            )?;
            completed.push(Operation::Create);
            let ready = Session::open()?;
            let disk = ready.disk(&selection.target)?;
            let parts = policy::layout(disk, &ready.partitions)?;
            let matching: Vec<_> = parts
                .iter()
                .copied()
                .filter(|part| {
                    part.offset == extent.offset
                        && part.size == extent.size
                        && policy::formattable(part)
                })
                .collect();
            let [partition] = matching.as_slice() else {
                return Err("未唯一找到预览中的新 USB 数据分区。".into());
            };
            let others = parts
                .iter()
                .copied()
                .filter(|part| part.partition_number != partition.partition_number)
                .collect::<Vec<_>>();
            let after_create = model::reserved(&others, extent, *initialize)?;
            if before_create.is_some() && before_create != after_create {
                return Err("新建分区后原有 USB 分区发生变化。".into());
            }
            let number = partition.partition_number;
            act(ready, selection, format_action(format, number), None)?;
        }
    }
    completed.push(Operation::Format);
    Ok(())
}

pub(crate) fn execute(
    token: String,
    mut progress: impl FnMut(Progress),
) -> Result<api::Outcome, String> {
    let _operation = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在执行")?;
    let batch = {
        let mut pending = PENDING.lock().map_err(|_| "预览状态不可用。")?;
        consume(&mut pending, &token, Instant::now())?
    };
    progress(Progress {
        phase: Phase::Checking,
        completed: 0,
        total: batch.devices.len(),
        current: None,
    });
    let initial = Session::open()?;
    for device in &batch.devices {
        recheck(&initial, device, &batch.format).map_err(|error| {
            format!(
                "磁盘 {} 未通过核对，本批尚未执行。{error}",
                device.view.number
            )
        })?;
    }
    drop(initial);
    let preview: Vec<_> = batch
        .devices
        .iter()
        .map(|entry| entry.view.clone())
        .collect();
    Ok(model::run_ordered(
        &preview,
        |index, steps| execute_device(&batch.devices[index], &batch.format, steps),
        progress,
    ))
}

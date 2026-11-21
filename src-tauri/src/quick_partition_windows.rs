//! Single-confirmation GPT batches. Never calls the public token API in a loop.
//! Each new revision must be the exact expected transition from the last step.
use super::*;
use crate::partitions::quick::{self as model, Entry, Phase, Preview, Progress, Role, Step};

static BATCH: Mutex<Option<PendingBatch>> = Mutex::new(None);
struct PendingBatch {
    request: model::Request,
    preview: Preview,
    state: State,
    deadline: Instant,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    partition: Partition,
    volume: Option<Volume>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    disk: Disk,
    items: Vec<Item>,
}
impl State {
    fn read(session: &Session, target: &Target) -> Result<Self, String> {
        let disk = session.disk(target)?.clone();
        let volumes = volume_snapshot::read(session, &disk)?;
        let items = policy::layout(&disk, &session.partitions)?
            .into_iter()
            .map(|p| {
                Ok(Item {
                    partition: p.clone(),
                    volume: volumes.volume(&disk, p)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self { disk, items })
    }
    fn entry(&self, entry: &Entry) -> Result<&Item, String> {
        let found: Vec<_> = self
            .items
            .iter()
            .filter(|p| p.partition.offset == entry.offset && p.partition.size == entry.size)
            .collect();
        if found.len() != 1
            || found[0]
                .partition
                .gpt_type
                .as_deref()
                .map(policy::guid)
                .as_deref()
                != Some(entry.role.gpt_type())
        {
            return Err("未唯一找到预览中的新分区及其类型；已停止后续步骤。".into());
        }
        Ok(found[0])
    }
}

// No metadata fields are silently refreshed across steps. Only the precise
// allowed target change is removed before equality-testing the complete state.
fn transition(before: &State, after: &State, step: &Step) -> Result<(), String> {
    let mut expected_disk = before.disk.clone();
    expected_disk.number_of_partitions = Some(after.items.len() as u32);
    if expected_disk != after.disk {
        return Err("步骤执行后磁盘身份、容量或保护状态改变。".into());
    }
    let mut old = before.items.clone();
    let mut new = after.items.clone();
    match step {
        Step::Delete {
            partition,
            offset,
            size,
            guid,
        } => {
            let index = old
                .iter()
                .position(|p| {
                    p.partition.partition_number == *partition
                        && p.partition.offset == *offset
                        && p.partition.size == *size
                        && p.partition.guid.as_deref() == Some(guid)
                })
                .ok_or("删除目标不再与确认时一致")?;
            old.remove(index);
            if new
                .iter()
                .any(|p| p.partition.guid.as_deref() == Some(guid) || p.partition.offset == *offset)
            {
                return Err("删除结果尚未确认。".into());
            }
        }
        Step::Create { entry } => {
            let created = after.entry(entry)?;
            let p = &created.partition;
            if before.items.iter().any(|old| {
                old.partition.guid == p.guid || old.partition.partition_number == p.partition_number
            }) || p.guid.as_deref().is_none_or(|g| policy::guid(g).is_empty())
                || p.is_boot != Some(false)
                || p.is_system != Some(false)
                || p.is_offline != Some(false)
            {
                return Err("新分区标识或保护信息不符合预览。".into());
            }
            let suitable = match entry.role {
                Role::Windows => policy::formattable(p) && created.volume.is_some(),
                Role::Linux => {
                    policy::deletable(p)
                        && created.volume.is_none()
                        && p.drive_letter
                            .as_deref()
                            .is_none_or(|s| policy::clean(s).is_empty())
                }
                Role::Efi | Role::Reserved => {
                    p.is_read_only == Some(false)
                        && p.is_shadow_copy == Some(false)
                        && p.is_hidden.is_some()
                        && created.volume.is_none()
                        && p.drive_letter
                            .as_deref()
                            .is_none_or(|s| policy::clean(s).is_empty())
                }
            };
            if !suitable {
                return Err("新分区的保护状态或 Windows 卷关联尚未核对完整。".into());
            }
            new.retain(|p| p.partition.guid != created.partition.guid);
        }
        Step::Format { entry } => {
            let previous = before.entry(entry)?;
            let current = after.entry(entry)?;
            if previous.partition != current.partition {
                return Err("格式化后分区布局或保护信息改变。".into());
            }
            let a = previous.volume.as_ref().ok_or("缺少格式化前卷身份")?;
            let b = current.volume.as_ref().ok_or("缺少格式化后卷身份")?;
            let f = entry.format.as_ref().ok_or("缺少格式化设置")?;
            if a.path != b.path
                || a.object_id != b.object_id
                || a.volume_path != b.volume_path
                || a.drive_letter != b.drive_letter
                || b.file_system
                    .as_deref()
                    .is_none_or(|name| !name.eq_ignore_ascii_case(f.file_system.windows_name()))
                || b.file_system_label.as_deref() != Some(f.label.as_str())
            {
                return Err("格式化后的卷身份、文件系统或卷标不符合确认。".into());
            }
            old.retain(|p| p.partition.guid != previous.partition.guid);
            new.retain(|p| p.partition.guid != current.partition.guid);
        }
    }
    if old != new {
        return Err("步骤执行后其他分区、卷关联或盘符发生变化；已停止后续步骤。".into());
    }
    Ok(())
}

pub(super) fn invalidate() -> Result<(), String> {
    *BATCH.lock().map_err(|_| "批量预览状态不可用")? = None;
    Ok(())
}
pub fn discard(token: &str) {
    if let Ok(mut pending) = BATCH.lock() {
        if pending
            .as_ref()
            .is_some_and(|p| p.preview.plan.token == token)
        {
            *pending = None;
        }
    }
}
pub fn prepare(mut request: model::Request) -> Result<Preview, String> {
    let _lock = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在执行")?;
    invalidate()?;
    *PENDING.lock().map_err(|_| "预览状态不可用")? = None;
    for entry in &mut request.entries {
        if let Some(f) = &mut entry.format {
            f.label = f.label.trim().to_owned();
            if f.file_system.fat_type().is_some() {
                f.label = f.label.to_ascii_uppercase();
            }
        }
    }
    let session = Session::open()?;
    let state = State::read(&session, &request.target)?;
    if session.revision(&request.target)? != request.revision {
        return Err("布局已经改变，请刷新并重新预览。".into());
    }
    let steps = model::validate(&state.disk, &session.partitions, &request)?;
    // Preflight ALL deletions before authorizing the first one. Subsequent
    // format targets do not exist yet and must pass the same binding after create.
    for number in &request.delete_partitions {
        session.bind(&Request {
            target: request.target.clone(),
            revision: request.revision.clone(),
            action: Action::Delete { partition: *number },
        })?;
    }
    let retained_partitions = state
        .items
        .iter()
        .map(|i| i.partition.partition_number)
        .filter(|n| !request.delete_partitions.contains(n))
        .collect();
    let preview = Preview {
        plan: Plan {
            token: uuid::Uuid::new_v4().to_string(),
            title: "GPT 快速分区".into(),
            target_name: format!("磁盘 {} · {}", state.disk.number, state.disk.friendly_name.as_deref().unwrap_or("未提供型号")),
            serial: state.disk.serial_number.as_deref().map(policy::clean).unwrap_or("").into(),
            partition: None,
            before_size: None,
            after_size: None,
            warning: "仅删除明确勾选的分区，其现有数据将丢失；其余分区保留。先按列表删除，再依次新建并格式化。每一步核对成功后才继续；失败或结果不确定立即停止，已执行的步骤不会回滚或重试。Linux、ESP/MSR 仅创建条目，不创建文件系统、启动文件或可启动系统。Windows 数据分区由系统在有空闲盘符时自动分配盘符。格式化调用期间不能取消，请勿关闭应用。".into(),
            expires_at: SystemTime::now().duration_since(UNIX_EPOCH).map_err(err)?.as_millis() as u64 + 300_000,
        },
        steps,
        retained_partitions,
    };
    *BATCH.lock().map_err(|_| "批量预览状态不可用")? = Some(PendingBatch {
        request,
        preview: preview.clone(),
        state,
        deadline: Instant::now() + Duration::from_secs(300),
    });
    Ok(preview)
}

fn action(state: &State, step: &Step) -> Result<Action, String> {
    Ok(match step {
        Step::Delete { partition, .. } => Action::Delete {
            partition: *partition,
        },
        Step::Create { entry } => Action::Create {
            offset: entry.offset,
            size: entry.size,
            kind: match entry.role {
                Role::Windows => PartitionKind::Windows,
                Role::Linux => PartitionKind::Linux,
                _ => return Err("受保护类型使用专用创建路径".into()),
            },
        },
        Step::Format { entry } => {
            let f = entry.format.as_ref().ok_or("缺少格式化设置")?;
            Action::Format {
                partition: state.entry(entry)?.partition.partition_number,
                label: f.label.clone(),
                file_system: f.file_system,
                cluster_size: f.cluster_size,
                options: f.options,
            }
        }
    })
}

fn one_step(
    session: Session,
    before: &State,
    target: &Target,
    step: &Step,
) -> Result<State, String> {
    if let Step::Create { entry } = step {
        if matches!(entry.role, Role::Efi | Role::Reserved) {
            // Reuse the ordinary free-extent/protection check without widening
            // the public Create action to arbitrary GPT types or provider input.
            let d = session.disk(target)?;
            policy::validate(
                d,
                &session.partitions,
                &Action::Create {
                    offset: entry.offset,
                    size: entry.size,
                    kind: PartitionKind::Linux,
                },
            )?;
            if d.is_boot != Some(false) || d.is_system != Some(false) {
                return Err("系统磁盘不允许批量创建 ESP/MSR".into());
            }
            #[derive(Serialize)]
            #[serde(rename_all = "PascalCase")]
            struct Input {
                offset: u64,
                size: u64,
                alignment: u32,
                gpt_type: String,
                assign_drive_letter: bool,
            }
            call::<Disk>(
                &session,
                &d.path,
                "CreatePartition",
                Input {
                    offset: entry.offset,
                    size: entry.size,
                    alignment: policy::MIB as u32,
                    gpt_type: format!("{{{}}}", entry.role.gpt_type()),
                    assign_drive_letter: false,
                },
            )?;
            let mut failure = "Windows 尚未返回可核对的完整新布局".to_owned();
            for _ in 0..8 {
                let checked = Session::open()
                    .and_then(|s| State::read(&s, target))
                    .and_then(|s| {
                        transition(before, &s, step)?;
                        Ok(s)
                    });
                match checked {
                    Ok(s) => return Ok(s),
                    Err(e) => failure = e,
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            return Err(failure);
        }
    }
    let request = Request {
        target: target.clone(),
        revision: session.revision(target)?,
        action: action(before, step)?,
    };
    let (binding, volume) = session.bind(&request)?;
    let outcome = execute_bound(request, session, binding, volume, None)?;
    if !outcome.verified {
        return Err(outcome.message);
    }
    let after = State::read(&Session::open()?, target)?;
    transition(before, &after, step)?;
    Ok(after)
}

// Deliberately no retry/resume entry point. The consumed confirmation authorizes
// this exact ordered run only; the caller gets a durable per-step outcome.
pub(super) fn run_steps<S>(
    steps: &[S],
    mut title: impl FnMut(&S) -> String,
    mut run: impl FnMut(&S) -> Result<(), String>,
    mut progress: impl FnMut(Progress),
) -> model::Execution {
    let mut lines = Vec::new();
    let mut failure = None;
    let mut completed = 0;
    for (index, step) in steps.iter().enumerate() {
        let title = title(step);
        if failure.is_some() {
            lines.push(format!("{}. 未执行：{title}", index + 1));
        } else {
            progress(Progress {
                phase: Phase::Running,
                completed: index,
                total: steps.len(),
                current: Some(index),
            });
            match run(step) {
                Ok(()) => {
                    completed += 1;
                    lines.push(format!("{}. 已完成并核对：{title}", index + 1));
                    progress(Progress {
                        phase: Phase::Verified,
                        completed: index + 1,
                        total: steps.len(),
                        current: None,
                    });
                }
                Err(e) => {
                    lines.push(format!(
                        "{}. 已停止，结果未完整确认：{title}。{e}",
                        index + 1
                    ));
                    failure = Some(e);
                    progress(Progress {
                        phase: Phase::Stopped,
                        completed: index,
                        total: steps.len(),
                        current: Some(index),
                    });
                }
            }
        }
    }
    model::Execution {
        verified: failure.is_none(),
        message: lines.join("\n"),
        completed,
        failure,
    }
}
pub fn execute(
    token: String,
    mut progress: impl FnMut(Progress),
) -> Result<model::Execution, String> {
    let _lock = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在执行")?;
    let pending = BATCH
        .lock()
        .map_err(|_| "批量预览状态不可用")?
        .take()
        .ok_or("批量确认已失效，请重新预览")?;
    if pending.preview.plan.token != token || Instant::now() > pending.deadline {
        return Err("确认无效或已超过 5 分钟。".into());
    }
    progress(Progress {
        phase: Phase::Checking,
        completed: 0,
        total: pending.preview.steps.len(),
        current: None,
    });
    let initial = Session::open()?;
    if State::read(&initial, &pending.request.target)? != pending.state
        || initial.revision(&pending.request.target)? != pending.request.revision
    {
        return Err("磁盘、完整布局或卷关联已经改变；本批未执行。".into());
    }
    // Recheck every original deletion's paging/volume protections before any
    // mutation, then check again immediately before each individual operation.
    for partition in &pending.request.delete_partitions {
        initial.bind(&Request {
            target: pending.request.target.clone(),
            revision: pending.request.revision.clone(),
            action: Action::Delete {
                partition: *partition,
            },
        })?;
    }
    let mut expected = pending.state;
    Ok(run_steps(
        &pending.preview.steps,
        Step::title,
        |step| {
            let session = Session::open()?;
            if State::read(&session, &pending.request.target)? != expected {
                return Err("下一步开始前布局、保护信息或卷关联发生变化。".into());
            }
            expected = one_step(session, &expected, &pending.request.target, step)?;
            Ok(())
        },
        progress,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> State {
        State {
            disk: Disk {
                number: 2,
                number_of_partitions: Some(1),
                ..Default::default()
            },
            items: vec![Item {
                partition: Partition {
                    disk_number: 2,
                    partition_number: 1,
                    offset: policy::MIB,
                    size: 64 * policy::MIB,
                    guid: Some("retained".into()),
                    ..Default::default()
                },
                volume: None,
            }],
        }
    }
    fn entry() -> Entry {
        Entry {
            offset: 128 * policy::MIB,
            size: 64 * policy::MIB,
            role: Role::Windows,
            format: None,
        }
    }
    fn created(before: &State) -> State {
        let e = entry();
        let mut after = before.clone();
        after.disk.number_of_partitions = Some(2);
        after.items.push(Item {
            partition: Partition {
                path: "provider-only".into(),
                disk_number: 2,
                partition_number: 2,
                offset: e.offset,
                size: e.size,
                guid: Some("new-guid".into()),
                gpt_type: Some(policy::BASIC_DATA.into()),
                mbr_type: None,
                is_boot: Some(false),
                is_system: Some(false),
                is_offline: Some(false),
                is_read_only: Some(false),
                is_hidden: Some(false),
                is_shadow_copy: Some(false),
                drive_letter: Some("G".into()),
            },
            volume: Some(Volume {
                path: "provider-volume".into(),
                object_id: "id".into(),
                volume_path: Some("volume-guid".into()),
                file_system: None,
                file_system_label: None,
                drive_letter: Some("G".into()),
                size: Some(e.size),
            }),
        });
        after
    }
    #[test]
    fn create_cannot_authorize_changes_to_other_partitions_or_disk_protection() {
        let before = state();
        let after = created(&before);
        let step = Step::Create { entry: entry() };
        transition(&before, &after, &step).unwrap();
        let mut bad = after.clone();
        bad.items[0].partition.drive_letter = Some("X".into());
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after.clone();
        bad.disk.is_read_only = Some(true);
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after.clone();
        bad.items[1].volume = None;
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after;
        bad.items[1].partition.guid = before.items[0].partition.guid.clone();
        assert!(transition(&before, &bad, &step).is_err());
    }
    #[test]
    fn deletion_requires_exact_target_and_preserves_every_other_field() {
        let before = created(&state());
        let p = &before.items[1].partition;
        let step = Step::Delete {
            partition: 2,
            offset: p.offset,
            size: p.size,
            guid: p.guid.clone().unwrap(),
        };
        let after = state();
        transition(&before, &after, &step).unwrap();
        let mut bad = after;
        bad.items[0].partition.size += policy::MIB;
        assert!(transition(&before, &bad, &step).is_err());
    }
    #[test]
    fn format_may_change_only_its_bound_volumes_filesystem_metadata() {
        let before = created(&state());
        let mut entry = entry();
        entry.format = Some(model::Format {
            file_system: FileSystem::Ntfs,
            label: "DATA".into(),
            cluster_size: 4096,
            options: format_options::FormatOptions::default(),
        });
        let step = Step::Format { entry };
        let mut after = before.clone();
        let volume = after.items[1].volume.as_mut().unwrap();
        volume.file_system = Some("NTFS".into());
        volume.file_system_label = Some("DATA".into());
        transition(&before, &after, &step).unwrap();
        let mut bad = after.clone();
        bad.items[1].volume.as_mut().unwrap().volume_path = Some("other-volume".into());
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after.clone();
        bad.items[1].partition.is_hidden = Some(true);
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after.clone();
        bad.items[0].volume = after.items[1].volume.clone();
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after;
        bad.items[1].volume.as_mut().unwrap().file_system = Some("exFAT".into());
        assert!(transition(&before, &bad, &step).is_err());
    }
    #[test]
    fn new_protected_types_require_known_protection_and_no_windows_volume() {
        let before = state();
        let mut after = created(&before);
        let mut entry = entry();
        entry.role = Role::Reserved;
        let step = Step::Create { entry };
        let p = &mut after.items[1].partition;
        p.gpt_type = Some(Role::Reserved.gpt_type().into());
        p.drive_letter = None;
        p.is_hidden = Some(true);
        after.items[1].volume = None;
        transition(&before, &after, &step).unwrap();
        let mut bad = after.clone();
        bad.items[1].partition.is_read_only = None;
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after.clone();
        bad.items[1].partition.is_boot = Some(true);
        assert!(transition(&before, &bad, &step).is_err());
        let mut bad = after;
        bad.items[1].volume = created(&before).items[1].volume.clone();
        assert!(transition(&before, &bad, &step).is_err());
    }
    #[test]
    fn failure_stops_the_sequence_without_retry_or_implicit_resume() {
        let steps = vec![Step::Create { entry: entry() }; 4];
        let mut calls = 0;
        let result = run_steps(
            &steps,
            Step::title,
            |_| {
                calls += 1;
                if calls == 2 {
                    Err("injected provider failure".into())
                } else {
                    Ok(())
                }
            },
            |_| {},
        );
        assert_eq!(calls, 2);
        assert!(!result.verified);
        assert_eq!(result.completed, 1);
        assert_eq!(result.failure.as_deref(), Some("injected provider failure"));
        assert_eq!(result.message.matches("未执行").count(), 2);
        assert_eq!(result.message.matches("已完成并核对").count(), 1);
        let result = run_steps(&steps, Step::title, |_| Ok(()), |_| {});
        assert!(result.verified);
        assert_eq!(result.completed, steps.len());
        assert_eq!(result.failure, None);
    }
    #[test]
    fn progress_reports_only_verified_steps_and_never_starts_skipped_steps() {
        let steps = vec![Step::Create { entry: entry() }; 4];
        let mut events = vec![];
        let mut calls = 0;
        let outcome = run_steps(
            &steps,
            Step::title,
            |_| {
                calls += 1;
                if calls == 3 {
                    Err("postcondition failed".into())
                } else {
                    Ok(())
                }
            },
            |e| events.push(e),
        );
        assert!(!outcome.verified);
        assert_eq!(calls, 3);
        assert_eq!(
            events
                .iter()
                .map(|e| (e.phase, e.completed, e.current))
                .collect::<Vec<_>>(),
            vec![
                (Phase::Running, 0, Some(0)),
                (Phase::Verified, 1, None),
                (Phase::Running, 1, Some(1)),
                (Phase::Verified, 2, None),
                (Phase::Running, 2, Some(2)),
                (Phase::Stopped, 2, Some(2)),
            ]
        );
        assert!(events.iter().all(|e| e.total == 4));
    }
    #[test]
    fn missing_progress_consumer_does_not_retry_or_change_the_result() {
        let steps = vec![Step::Create { entry: entry() }; 3];
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        let mut calls = 0;
        let result = run_steps(
            &steps,
            Step::title,
            |_| {
                calls += 1;
                Ok(())
            },
            |event| {
                let _ = sender.send(event);
            },
        );
        assert_eq!(calls, 3);
        assert!(result.verified);
    }
}

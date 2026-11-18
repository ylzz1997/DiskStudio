//! Fixed Windows Storage state methods. No layout, filesystem or identity edits.
use super::*;
use crate::{
    data_windows as device,
    device_state::{self as model, Action as StateAction, State},
};
use std::{
    fs::File,
    mem::{offset_of, size_of},
};
use windows_sys::Win32::System::Ioctl::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Identity {
    #[serde(rename = "__PATH")]
    path: String,
    number: u32,
    partition_style: Option<u16>,
    guid: Option<String>,
    signature: Option<u32>,
    boot_from_disk: Option<bool>,
    offline_reason: Option<u16>,
}
fn disk_id(row: &Identity) -> Result<String, String> {
    match row.partition_style {
        Some(0) => Ok("raw".into()),
        Some(1) => row
            .signature
            .filter(|n| *n != 0)
            .map(|n| format!("mbr:{n:08x}"))
            .ok_or("MBR 磁盘签名缺失".into()),
        Some(2) => row
            .guid
            .as_deref()
            .map(policy::guid)
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .filter(|u| !u.is_nil())
            .map(|u| format!("gpt:{u}"))
            .ok_or("GPT 磁盘 GUID 缺失".into()),
        _ => Err("磁盘分区表类型未知".into()),
    }
}
fn identities(s: &Session, d: &Disk, action: StateAction) -> Result<(Identity, String), String> {
    let rows: Vec<Identity> = s.con.raw_query("SELECT __PATH,Number,PartitionStyle,Guid,Signature,BootFromDisk,OfflineReason FROM MSFT_Disk").map_err(err)?;
    identity_check(&rows, &s.disks, &s.partitions, d, action)
}
pub(super) fn access_identity(s: &Session, d: &Disk, opening: bool) -> Result<String, String> {
    let (identity, _) = identities(
        s,
        d,
        if opening {
            StateAction::ReadWrite
        } else {
            StateAction::ReadOnly
        },
    )?;
    digest(&identity)
}
fn identity_check(
    rows: &[Identity],
    disks: &[Disk],
    parts: &[Partition],
    d: &Disk,
    action: StateAction,
) -> Result<(Identity, String), String> {
    if rows.len() != disks.len()
        || disks.iter().any(|disk| {
            rows.iter()
                .filter(|r| {
                    r.number == disk.number
                        && r.path == disk.path
                        && r.partition_style == disk.partition_style
                })
                .count()
                != 1
        })
    {
        return Err("设备标识清单与当前磁盘清单不一致。".into());
    }
    let row = rows
        .iter()
        .find(|r| r.number == d.number)
        .ok_or("目标设备标识缺失")?;
    if row.boot_from_disk != Some(false) {
        return Err("目标可能用于启动或启动保护未知，不能更改设备状态。".into());
    }
    let id = disk_id(row)?;
    if matches!(action, StateAction::Online | StateAction::ReadWrite) {
        if action == StateAction::Online && row.offline_reason != Some(1) {
            return Err(
                "磁盘不是由用户脱机，或脱机原因为冲突、快照、故障或未知；不能直接联机。".into(),
            );
        }
        for other in rows.iter().filter(|r| r.number != d.number) {
            let other_id = disk_id(other)?;
            if id != "raw" && id == other_id {
                return Err(
                    "磁盘 GUID 或 MBR 签名与另一设备重复，不能解除访问限制；不会自动改写标识。"
                        .into(),
                );
            }
        }
        model::unique_partition_ids(disks, parts, d.number)?;
    }
    Ok((row.clone(), id))
}
fn attributes(f: &File) -> Result<State, String> {
    let mut b = [0; 16];
    if device::control(f, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut b)? != 16
        || u32::from_le_bytes(b[..4].try_into().unwrap()) != 16
    {
        return Err("原生设备属性不完整".into());
    }
    let bits = u64::from_le_bytes(b[8..16].try_into().unwrap());
    if bits & !3 != 0 {
        return Err("设备含有未知原生属性".into());
    }
    Ok(State {
        offline: bits & 1 != 0,
        read_only: bits & 2 != 0,
    })
}
fn native(f: &File, ctx: &TableReadContext, identity: &str) -> Result<String, String> {
    let disk = &ctx.resolved.disk;
    if disk.partition_style == Some(2) {
        let (layout, _, fingerprint) = crate::extfs::restore_native(
            f,
            Some(&ctx.parts.iter().collect::<Vec<_>>()),
            disk.logical_sector_size.ok_or("逻辑扇区未知")?,
        )?;
        if identity != format!("gpt:{}", layout.guid)
            || layout.entries.iter().any(|p| {
                matches!(
                    p.kind.as_str(),
                    "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3"
                        | "af9b60a0-1431-4f62-bc68-3311714a69ad"
                        | "e75caf8f-f680-4cee-afa3-b001e56efc2d"
                )
            })
        {
            return Err("原生磁盘 GUID 不一致，或目标含动态磁盘/存储池成员。".into());
        }
        return digest(&(
            layout.guid,
            layout.first,
            layout.last,
            layout.count,
            fingerprint,
        ));
    }
    const HEADER: usize = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    const ENTRY: usize = size_of::<PARTITION_INFORMATION_EX>();
    let mut bytes = vec![0; HEADER + 516 * ENTRY];
    let n = device::control(f, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut bytes)?;
    bytes.truncate(n);
    mbr_raw_layout(&bytes, disk, &ctx.parts, identity)
}
fn mbr_raw_layout(
    bytes: &[u8],
    disk: &Disk,
    expected: &[Partition],
    identity: &str,
) -> Result<String, String> {
    const HEADER: usize = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    const ENTRY: usize = size_of::<PARTITION_INFORMATION_EX>();
    if bytes.len() < HEADER {
        return Err("原生布局响应不完整".into());
    }
    let style = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if count > 516 || bytes.len() < HEADER + count * ENTRY {
        return Err("原生布局条目响应不完整".into());
    }
    if disk.partition_style == Some(0) {
        if style != PARTITION_STYLE_RAW as u32
            || count != 0
            || !expected.is_empty()
            || identity != "raw"
        {
            return Err("RAW 原生布局不一致".into());
        }
        return Ok("raw-empty".into());
    }
    if disk.partition_style != Some(1) || style != PARTITION_STYLE_MBR as u32 || count % 4 != 0 {
        return Err("MBR 原生布局不一致".into());
    }
    let signature = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if identity != format!("mbr:{signature:08x}") || signature == 0 {
        return Err("原生 MBR 签名不一致".into());
    }
    let sector = disk.logical_sector_size.ok_or("逻辑扇区未知")?;
    let mut entries = vec![];
    let mut numbers = std::collections::HashSet::new();
    let mut data_count = 0;
    for i in 0..count {
        let base = HEADER + i * ENTRY;
        for at in [
            offset_of!(PARTITION_INFORMATION_EX, RewritePartition),
            offset_of!(PARTITION_INFORMATION_EX, IsServicePartition),
            offset_of!(PARTITION_INFORMATION_EX, Anonymous)
                + offset_of!(PARTITION_INFORMATION_MBR, BootIndicator),
            offset_of!(PARTITION_INFORMATION_EX, Anonymous)
                + offset_of!(PARTITION_INFORMATION_MBR, RecognizedPartition),
        ] {
            if bytes[base + at] > 1 {
                return Err("原生 MBR 布尔字段无效".into());
            }
        }
        let p = unsafe {
            bytes
                .as_ptr()
                .add(HEADER + i * ENTRY)
                .cast::<PARTITION_INFORMATION_EX>()
                .read_unaligned()
        };
        let m = unsafe { p.Anonymous.Mbr };
        if m.PartitionType == 0 {
            if p.PartitionLength != 0 {
                return Err("未使用 MBR 条目含有效范围".into());
            }
            continue;
        }
        if p.PartitionStyle != PARTITION_STYLE_MBR
            || p.StartingOffset < 0
            || p.PartitionLength <= 0
            || p.StartingOffset as u64 % sector != 0
            || p.PartitionLength as u64 % sector != 0
            || (p.StartingOffset as u64)
                .checked_add(p.PartitionLength as u64)
                .is_none_or(|end| end > disk.size)
            || m.PartitionType == 0x42
        {
            return Err("原生 MBR 范围无效或为动态磁盘".into());
        }
        if !matches!(m.PartitionType, 0x05 | 0x0f | 0x85) {
            if !numbers.insert(p.PartitionNumber)
                || expected
                    .iter()
                    .filter(|e| {
                        e.partition_number == p.PartitionNumber
                            && e.offset == p.StartingOffset as u64
                            && e.size == p.PartitionLength as u64
                    })
                    .count()
                    != 1
            {
                return Err("原生 MBR 数据分区与 WMI 关联不一致".into());
            }
            data_count += 1;
        }
        entries.push((
            p.PartitionNumber,
            p.StartingOffset,
            p.PartitionLength,
            p.IsServicePartition,
            m.PartitionType,
            m.BootIndicator,
            m.HiddenSectors,
        ));
    }
    if data_count != expected.len() {
        return Err("原生 MBR 数据分区清单不完整".into());
    }
    digest(&(signature, entries))
}
struct Context {
    ctx: TableReadContext,
    identity: Identity,
    id: String,
    state: State,
}
fn context(
    request: &model::Request,
    retained: Option<(&TableReadContext, &device::Locks)>,
) -> Result<Context, String> {
    let ctx = disk_read_context(&request.source, retained, true)?;
    let state = model::eligible(&ctx.resolved.disk, &ctx.parts, request.action)?;
    let session = Session::open()?;
    let disk = policy::target_disk(&session.disks, &request.source.target)?;
    if digest(disk)? != digest(&ctx.resolved.disk)?
        || digest(&policy::layout(disk, &session.partitions)?)? != digest(&ctx.parts)?
    {
        return Err("设备保护/布局在核对期间变化".into());
    }
    let (identity, id) = identities(&session, disk, request.action)?;
    Ok(Context {
        ctx,
        identity,
        id,
        state,
    })
}
struct Pending {
    request: model::Request,
    preview: model::Preview,
    expected: ExpectedState,
    deadline: Instant,
}
struct ExpectedState {
    fingerprint: String,
    identity: String,
    native: String,
    state: State,
}
static STATE_PENDING: Mutex<Option<Pending>> = Mutex::new(None);
fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    STATE_PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn discard(token: &str) {
    let mut p = pending();
    if p.as_ref().is_some_and(|p| p.preview.token == token) {
        *p = None;
    }
}
fn consume(state: &mut Option<Pending>, token: &str, now: Instant) -> Result<Pending, String> {
    let p = state.take().ok_or("设备状态确认已失效，请重新预览。")?;
    if p.preview.token != token || now >= p.deadline {
        return Err("设备状态确认已过期或不匹配。".into());
    }
    Ok(p)
}
pub fn prepare(request: model::Request) -> Result<model::Preview, String> {
    let _guard = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在运行")?;
    *pending() = None;
    let c = context(&request, None)?;
    let f = device::device(&c.ctx.resolved, false)?;
    if attributes(&f)? != c.state {
        return Err("原生设备状态与 WMI 不一致".into());
    }
    let native = native(&f, &c.ctx, &c.id)?;
    let after = context(&request, None)?;
    if after.ctx.fingerprint != c.ctx.fingerprint
        || digest(&after.identity)? != digest(&c.identity)?
    {
        return Err("预览期间设备状态变化".into());
    }
    let locale = crate::native_locale::current();
    let warning = match request.action {
        StateAction::Online => "联机后 Windows 可能自动挂载分区并写入文件系统元数据。不会更改磁盘/分区标识，也不会自动解除只读。",
        StateAction::Offline => "脱机后所有分区将无法供其他程序访问。请保存文件并关闭占用程序；不能独占锁定时拒绝操作。",
        StateAction::ReadOnly => "只读后所有分区不能写入。请保存文件并关闭占用程序；不会更改分区自身的保护属性。",
        StateAction::ReadWrite => "解除只读后 Windows 和其他程序将可以写入此磁盘。不会更改分区自身的只读属性。",
    };
    let preview = model::Preview {
        token: uuid::Uuid::new_v4().to_string(),
        title: request.action.title().into(),
        target: format!(
            "{} · {}",
            locale.format(
                "磁盘 {number}",
                &[("number", &c.ctx.resolved.disk.number.to_string())]
            ),
            c.ctx
                .resolved
                .disk
                .serial_number
                .as_deref()
                .unwrap_or_default(),
        ),
        size: c.ctx.resolved.disk.size,
        partitions: c.ctx.parts.len(),
        before: c.state,
        after: request.action.desired(c.state),
        warning: locale.sentences(&[
            warning,
            "确认有效期为 5 分钟；失败时检查实际状态，不会自动重试或回滚。",
        ]),
    };
    *pending() = Some(Pending {
        request,
        preview: preview.clone(),
        expected: ExpectedState {
            fingerprint: c.ctx.fingerprint,
            identity: digest(&c.identity)?,
            native,
            state: c.state,
        },
        deadline: Instant::now() + Duration::from_secs(300),
    });
    Ok(preview)
}
pub fn execute(token: String) -> Result<model::Outcome, String> {
    let _guard = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在运行")?;
    let p = consume(&mut pending(), &token, Instant::now())?;
    execute_checked(&p.request, &p.expected, None, || Ok(()))
}

/// Called inside the batch guard, only after the movement and its journal have
/// been verified. An independently offline disk cannot enter this path.
pub(super) fn online_after_verified_move(
    source: crate::data_tasks::Selection,
    original_disk: &Disk,
    raw: &crate::table_archive::Archive,
    before_dispatch: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let mut expected_disk = original_disk.clone();
    expected_disk.is_offline = Some(true);
    if original_disk.is_offline != Some(false) {
        return Err("本批移动前未确认联机状态。".into());
    }
    change_for_verified_batch(
        source,
        StateAction::Online,
        &expected_disk,
        raw,
        before_dispatch,
    )
}

pub(super) fn offline_after_verified_batch(
    source: crate::data_tasks::Selection,
    expected_disk: &Disk,
    raw: &crate::table_archive::Archive,
) -> Result<(), String> {
    change_for_verified_batch(source, StateAction::Offline, expected_disk, raw, || Ok(()))
}

fn change_for_verified_batch(
    source: crate::data_tasks::Selection,
    action: StateAction,
    expected_disk: &Disk,
    raw: &crate::table_archive::Archive,
    before_dispatch: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let request = model::Request { source, action };
    let current = context(&request, None)?;
    if &current.ctx.resolved.disk != expected_disk
        || raw.source != request.source.target.0
        || raw.size != expected_disk.size
        || Some(raw.sector) != expected_disk.logical_sector_size
    {
        return Err("待切换状态的磁盘与本批已核对的设备身份不同。".into());
    }
    let mut file = device::device(&current.ctx.resolved, false)?;
    raw.matches(&mut file, || Ok(()))?;
    let expected = ExpectedState {
        fingerprint: current.ctx.fingerprint.clone(),
        identity: digest(&current.identity)?,
        native: native(&file, &current.ctx, &current.id)?,
        state: current.state,
    };
    drop(file);
    execute_checked(&request, &expected, Some(raw), before_dispatch)?;
    Ok(())
}

fn execute_checked(
    request: &model::Request,
    expected: &ExpectedState,
    raw: Option<&crate::table_archive::Archive>,
    before_dispatch: impl FnOnce() -> Result<(), String>,
) -> Result<model::Outcome, String> {
    let c = context(request, None)?;
    if c.ctx.fingerprint != expected.fingerprint || digest(&c.identity)? != expected.identity {
        return Err("设备状态与预览不同，请重新预览。".into());
    }
    let locks = device::state_locks(&c.ctx.resolved)?;
    let fresh = context(request, Some((&c.ctx, &locks)))?;
    device::verify_locks(
        &locks,
        &fresh.ctx.resolved,
        &fresh.ctx.parts.iter().collect::<Vec<_>>(),
    )?;
    let mut f = device::device(&fresh.ctx.resolved, false)?;
    if fresh.ctx.fingerprint != expected.fingerprint
        || digest(&fresh.identity)? != expected.identity
        || attributes(&f)? != expected.state
        || native(&f, &fresh.ctx, &fresh.id)? != expected.native
    {
        return Err("锁定后的设备身份、布局或状态变化，未提交变更。".into());
    }
    // Resolve the provider-derived object again directly before the single call.
    let s = Session::open()?;
    let disk = policy::target_disk(&s.disks, &request.source.target)?;
    policy::corroborate(disk, &s.legacy)?;
    let (id, _) = identities(&s, disk, request.action)?;
    if digest(disk)? != digest(&fresh.ctx.resolved.disk)?
        || digest(&id)? != expected.identity
        || digest(&policy::layout(disk, &s.partitions)?)? != digest(&fresh.ctx.parts)?
    {
        return Err("提交前设备发生变化".into());
    }
    if let Some(raw) = raw {
        raw.matches(&mut f, || Ok(()))?;
    }
    let after_state = request.action.desired(expected.state);
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct ReadOnly {
        is_read_only: bool,
    }
    before_dispatch()?;
    let result = match request.action {
        StateAction::Online => call::<Disk>(&s, &disk.path, "Online", ()),
        StateAction::Offline => call::<Disk>(&s, &disk.path, "Offline", ()),
        StateAction::ReadOnly | StateAction::ReadWrite => call::<Disk>(
            &s,
            &disk.path,
            "SetAttributes",
            ReadOnly {
                is_read_only: after_state.read_only,
            },
        ),
    };
    // Release locks before resolving the new volume associations. Never retry a method.
    drop(locks);
    result.map_err(|e| {
        crate::native_locale::diagnostic(
            "{error} 状态可能已改变；请刷新核对，不会自动重试或回滚。",
            &e,
        )
    })?;
    let after = disk_read_context(&request.source, None, true).map_err(|e| {
        crate::native_locale::diagnostic(
            "方法已返回成功，但写后保护/布局核验未完成：{error}；请检查实际状态，不会自动重试或回滚。", &e)
    })?;
    let mut expected_disk = fresh.ctx.resolved.disk.clone();
    expected_disk.is_offline = Some(after_state.offline);
    expected_disk.is_read_only = Some(after_state.read_only);
    let mut checked = device::device(&after.resolved, false)?;
    if digest(&expected_disk)? != digest(&after.resolved.disk)?
        || attributes(&checked)? != after_state
        || native(&checked, &after, &fresh.id)? != expected.native
    {
        return Err("方法已返回成功，但写后设备属性或原生布局与预期不一致；请检查实际状态，不会自动重试或回滚。".into());
    }
    if let Some(raw) = raw {
        raw.matches(&mut checked, || Ok(()))?;
    }
    let locale = crate::native_locale::current();
    Ok(model::Outcome {
        message: locale.format(
            "{action}完成；设备身份、状态及原生分区布局核对通过。",
            &[("action", locale.text(request.action.title()))],
        ),
    })
}

#[cfg(test)]
#[path = "device_state_windows_tests.rs"]
mod tests;

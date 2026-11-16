//! Provider-derived SetAttributes, one call per confirmation, complete GPT readback.
use super::*;
use crate::{data_windows as device, partition_attributes as model, table_archive, table_restore};

struct Context {
    whole: crate::data_tasks::Selection,
    ctx: TableReadContext,
    part: Partition,
    identity: String,
}
fn context(
    request: &model::Request,
    retained: Option<(&TableReadContext, &device::Locks)>,
) -> Result<Context, String> {
    let number = request.source.partition.ok_or("请选择分区。")?;
    let mut whole = request.source.clone();
    whole.partition = None;
    let ctx = table_read_context(&whole, retained)?;
    let part = ctx
        .parts
        .iter()
        .find(|p| p.partition_number == number)
        .ok_or("分区不存在")?
        .clone();
    model::eligible(&ctx.resolved.disk, &part)?;
    let s = Session::open()?;
    let d = policy::target_disk(&s.disks, &whole.target)?;
    if digest(d)? != digest(&ctx.resolved.disk)?
        || digest(&policy::layout(d, &s.partitions)?)? != digest(&ctx.parts)?
    {
        return Err("核对期间磁盘身份或布局变化。".into());
    }
    let identity = device_state::access_identity(&s, d, request.action.opens_access())?;
    Ok(Context {
        whole,
        ctx,
        part,
        identity,
    })
}
fn capture(
    c: &TableReadContext,
    whole: &crate::data_tasks::Selection,
) -> Result<table_archive::Archive, String> {
    let mut file = device::device(&c.resolved, false)?;
    let sector = c.resolved.disk.logical_sector_size.ok_or("扇区大小未知")?;
    let archive = table_archive::capture(
        &mut file,
        c.resolved.size,
        sector,
        whole.target.0.clone(),
        false,
        || Ok(()),
    )?;
    let native =
        crate::extfs::restore_native(&file, Some(&c.parts.iter().collect::<Vec<_>>()), sector)?.0;
    if !table_restore::archive_layout(&archive)?.agrees(&native) {
        return Err("原始 GPT 与 Windows 原生布局不一致。".into());
    }
    Ok(archive)
}
fn selected(c: &Context, a: &table_archive::Archive) -> Result<table_restore::Entry, String> {
    let guid = c
        .part
        .guid
        .as_deref()
        .map(policy::guid)
        .ok_or("分区 GUID 缺失")?;
    let entries = table_restore::archive_layout(a)?.entries;
    let e = entries
        .iter()
        .find(|e| e.guid == guid)
        .ok_or("原始 GPT 分区缺失")?;
    let attrs = model::Attributes::from(e.attributes);
    if e.offset != c.part.offset
        || e.size != c.part.size
        || Some(attrs.hidden) != c.part.is_hidden
        || Some(attrs.read_only) != c.part.is_read_only
        || e.attributes & !model::ALLOWED != 0
    {
        return Err("原始 GPT 属性或范围与保护元数据不一致。".into());
    }
    Ok(e.clone())
}
struct Pending {
    request: model::Request,
    preview: model::Preview,
    fingerprint: String,
    identity: String,
    before: Vec<u8>,
    after: Vec<u8>,
    deadline: Instant,
}
static PENDING_ATTRIBUTES: Mutex<Option<Pending>> = Mutex::new(None);
fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING_ATTRIBUTES.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn discard(token: &str) {
    let mut p = pending();
    if p.as_ref().is_some_and(|p| p.preview.token == token) {
        *p = None;
    }
}
fn consume(state: &mut Option<Pending>, token: &str, now: Instant) -> Result<Pending, String> {
    let p = state.take().ok_or("属性确认已失效，请重新预览。")?;
    if p.preview.token != token || now >= p.deadline {
        return Err("属性确认已过期或不匹配。".into());
    }
    Ok(p)
}
pub fn prepare(request: model::Request) -> Result<model::Preview, String> {
    let _guard = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在运行")?;
    *pending() = None;
    let c = context(&request, None)?;
    let before = capture(&c.ctx, &c.whole)?;
    let e = selected(&c, &before)?;
    let after = model::expected(&before, &e.guid, request.action)?;
    let fresh = context(&request, None)?;
    if c.ctx.fingerprint != fresh.ctx.fingerprint || c.identity != fresh.identity {
        return Err("预览期间属性或身份变化。".into());
    }
    let warning = match request.action {
        model::Action::Hide => "隐藏后 Windows 不再为此分区提供盘符、卷路径和挂载点。文件仍保留。",
        model::Action::Show => {
            "取消隐藏后 Windows 可能重新挂载并写入文件系统元数据；必要时请另行分配盘符。"
        }
        model::Action::ReadOnly => "只读限制作用于所选分区，之后的文件写入会被拒绝。",
        model::Action::ReadWrite => "解除后 Windows 和其他程序可以写入所选分区。",
        model::Action::NoDefaultLetter => {
            "仅禁止发现此分区时自动分配盘符；已有盘符请通过“管理盘符”单独移除。"
        }
        model::Action::AutomaticLetter => {
            "恢复默认盘符分配资格；不会主动分配某个盘符，也不保证立即获得盘符。"
        }
    };
    let preview = model::Preview { token: uuid::Uuid::new_v4().to_string(), title: request.action.title().into(), target: c.ctx.resolved.label.clone(), partition: c.part.partition_number, offset: e.offset, size: e.size,
        before: e.attributes.into(), after: request.action.desired(e.attributes).into(),
        warning: format!("{warning} 请关闭占用程序。每次仅更改所选属性，确认有效期 5 分钟；失败不自动重试或回滚。") };
    *pending() = Some(Pending {
        request,
        preview: preview.clone(),
        fingerprint: c.ctx.fingerprint,
        identity: c.identity,
        before: before.encode()?,
        after: after.encode()?,
        deadline: Instant::now() + Duration::from_secs(300),
    });
    Ok(preview)
}
pub fn execute(token: String) -> Result<model::Outcome, String> {
    let _guard = OPERATION.try_lock().map_err(|_| "另一项磁盘操作正在运行")?;
    let p = consume(&mut pending(), &token, Instant::now())?;
    let c = context(&p.request, None)?;
    if c.ctx.fingerprint != p.fingerprint || c.identity != p.identity {
        return Err("属性或设备身份与预览不同。".into());
    }
    // Read handles allow locking an already read-only partition; every volume
    // still requires exclusive FSCTL_LOCK_VOLUME and a matching native extent.
    let locks = device::attribute_locks(&c.ctx.resolved)?;
    let fresh = context(&p.request, Some((&c.ctx, &locks)))?;
    device::verify_locks(
        &locks,
        &fresh.ctx.resolved,
        &fresh.ctx.parts.iter().collect::<Vec<_>>(),
    )?;
    if fresh.ctx.fingerprint != p.fingerprint
        || fresh.identity != p.identity
        || capture(&fresh.ctx, &fresh.whole)?.encode()? != p.before
    {
        return Err("锁定后属性或 GPT 内容变化，未提交操作。".into());
    }
    // SetAttributes is a storage-provider operation that opens its own volume
    // handles. FSCTL_LOCK_VOLUME would deny those handles access. The lock above
    // proves quiescence and extent association; close it before the final fresh
    // provider resolution. Windows owns access arbitration for its method. This
    // is not an exclusive raw write and must not claim a retained lock throughout.
    drop(locks);
    let s = Session::open()?;
    let d = policy::target_disk(&s.disks, &p.request.source.target)?;
    policy::corroborate(d, &s.legacy)?;
    if digest(d)? != digest(&fresh.ctx.resolved.disk)?
        || digest(&policy::layout(d, &s.partitions)?)? != digest(&fresh.ctx.parts)?
        || device_state::access_identity(&s, d, p.request.action.opens_access())? != p.identity
    {
        return Err("提交前身份或保护状态变化。".into());
    }
    let target = s.partition(&p.request.source.target, fresh.part.partition_number)?;
    let after = &p.preview.after;
    let result = call::<Partition>(
        &s,
        &target.path,
        "SetAttributes",
        model::params(p.request.action),
    );
    result.map_err(|e| format!("{e} 属性可能已改变，请刷新检查实际状态；不会自动重试或回滚。"))?;
    let verify = || -> Result<String, String> {
        let ctx = table_read_context(&fresh.whole, None)?;
        if digest(&ctx.resolved.disk)? != digest(&fresh.ctx.resolved.disk)? {
            return Err("磁盘状态发生非预期变化".into());
        }
        let readback = capture(&ctx, &fresh.whole)?.encode()?;
        if readback != p.after {
            return Err("完整 GPT 与预期属性修改不一致".into());
        }
        let part = ctx
            .parts
            .iter()
            .find(|e| e.partition_number == fresh.part.partition_number)
            .ok_or("分区未就绪")?;
        if part.is_hidden != Some(after.hidden)
            || part.is_read_only != Some(after.read_only)
            || part.is_boot != Some(false)
            || part.is_system != Some(false)
            || part.is_shadow_copy != Some(false)
            || part.guid.as_deref().map(policy::guid)
                != fresh.part.guid.as_deref().map(policy::guid)
        {
            return Err("写后保护/属性元数据不一致".into());
        }
        Ok(format!("{:x}", Sha256::digest(&readback)))
    };
    let sha256 = verify().map_err(|e| {
        format!("方法已返回成功，但写后核验未完成：{e}。请检查实际状态，不会自动重试或回滚。")
    })?;
    Ok(model::Outcome {
        message: format!(
            "{}完成；完整 GPT 回读、SHA-256 与原生布局核验通过。",
            p.request.action.title()
        ),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_and_mismatched_tokens_are_always_consumed() {
        fn ticket(now: Instant) -> Pending {
            Pending {
                request: serde_json::from_value(serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":2},"action":"hide"})).unwrap(),
                preview: model::Preview { token:"once".into(),title:"x".into(),target:"fixture".into(),partition:2,offset:1,size:1,before:0.into(),after:model::HIDDEN.into(),warning:"".into() },
                fingerprint:"".into(),identity:"".into(),before:vec![],after:vec![],deadline:now+Duration::from_secs(300),
            }
        }
        let now = Instant::now();
        for (token, time, ok) in [
            ("bad", now, false),
            ("once", now + Duration::from_secs(300), false),
            ("once", now, true),
        ] {
            let mut state = Some(ticket(now));
            assert_eq!(consume(&mut state, token, time).is_ok(), ok);
            assert!(state.is_none());
            assert!(consume(&mut state, "once", now).is_err());
        }
    }
}

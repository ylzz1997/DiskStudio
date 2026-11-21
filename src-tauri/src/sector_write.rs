#[cfg(windows)]
use crate::{
    data_tasks::Selection,
    data_windows as device, extfs,
    partitions::platform::{self, maintenance, SectorContext, TableReadContext},
    surface_scan::{Options, Range},
};
use crate::{
    sector_edit::{self, Edit},
    sector_tasks::{self, Page},
    table_archive::Archive,
};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub page_id: String,
    pub edits: Vec<Edit>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub offset: u32,
    pub before: u8,
    pub after: u8,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub token: String,
    pub target: String,
    pub relative_sector: u64,
    pub absolute_sector: u64,
    pub sector_size: u64,
    pub write_size: u64,
    pub write_start_lba: u64,
    pub before_sha256: String,
    pub after_sha256: String,
    pub changes: Vec<Change>,
    pub warning: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub message: String,
    pub sha256: String,
}
struct Pending {
    plan: Plan,
    page: Page,
    before: Vec<u8>,
    after: Vec<u8>,
    fingerprint: String,
    proof: Proof,
    expires: Instant,
}
#[derive(PartialEq)]
enum Native {
    Partition(Vec<String>),
    Unallocated(Archive),
}
#[derive(PartialEq)]
struct Proof {
    block: crate::surface_scan::Range,
    native: Native,
}
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn discard(token: String) {
    let mut state = pending();
    if state.as_ref().is_some_and(|p| p.plan.token == token) {
        *state = None
    }
}
fn take(state: &mut Option<Pending>, token: &str, now: Instant) -> Result<Pending, String> {
    let item = state.take().ok_or("确认已失效，请重新预览。")?;
    if item.plan.token != token || now >= item.expires {
        return Err("确认已过期或不匹配。".into());
    }
    Ok(item)
}
#[cfg(windows)]
fn job<T>(title: &str, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let (id, _) = crate::data_tasks::start(title)?;
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_|Err("扇区任务异常结束。若已确认写入，磁盘可能保持脱机或部分写入；请核对实际状态，不会自动重试或回滚。".into()));
    crate::data_tasks::finish(
        &id,
        result
            .as_ref()
            .map(|_| ("扇区任务完成".into(), None))
            .map_err(Clone::clone),
    );
    result
}
#[cfg(windows)]
enum Context {
    Partition(SectorContext),
    Unallocated(TableReadContext),
}

#[cfg(windows)]
impl Context {
    fn open(source: &Selection, old: Option<(&Self, &device::Locks)>) -> Result<Self, String> {
        match (source.partition, old) {
            (Some(_), None) => Ok(Self::Partition(platform::sector_context(source, None)?)),
            (Some(_), Some((Self::Partition(ctx), locks))) => Ok(Self::Partition(
                platform::sector_context(source, Some((ctx, locks)))?,
            )),
            (None, None) => Ok(Self::Unallocated(maintenance::write_context(source, None)?)),
            (None, Some((Self::Unallocated(ctx), locks))) => Ok(Self::Unallocated(
                maintenance::write_context(source, Some((ctx, locks)))?,
            )),
            _ => Err("扇区范围已变化。".into()),
        }
    }

    fn binding(
        &self,
    ) -> (
        &device::Resolved,
        &[crate::partitions::policy::Partition],
        &str,
    ) {
        match self {
            Self::Partition(ctx) => (&ctx.resolved, &ctx.parts, &ctx.fingerprint),
            Self::Unallocated(ctx) => (&ctx.resolved, &ctx.parts, &ctx.fingerprint),
        }
    }

    fn page_range(&self, page: &Page) -> Result<Range, String> {
        let (resolved, _, _) = self.binding();
        if resolved.offset != page.source_offset
            || resolved.size != page.source_size
            || resolved.disk.logical_sector_size != Some(page.sector_size)
        {
            return Err("扇区范围已变化。".into());
        }
        Options {
            start_sector: page.sector,
            sector_count: 1,
        }
        .range(page.source_offset, page.source_size, page.sector_size)
    }

    fn proof(&self, file: &mut File, page: &Page, range: Range) -> Result<Proof, String> {
        let (resolved, parts, _) = self.binding();
        let (physical, alignment_offset) = device::physical_alignment(resolved, file)?;
        let block = sector_edit::physical_block(
            range,
            Range {
                offset: resolved.offset,
                size: resolved.size,
                sector: page.sector_size,
            },
            physical,
            alignment_offset,
        )?;
        let native = match self {
            Self::Partition(ctx) => {
                let layout = extfs::native_layout(file, &parts.iter().collect::<Vec<_>>())?;
                extfs::check_native_data_target(&layout, &ctx.selected)?;
                Native::Partition(extfs::layout_fingerprint(&layout, None))
            }
            Self::Unallocated(ctx) => {
                let (archive, layout) =
                    maintenance::write_table(ctx, file, &page.source.target)?
                        .ok_or("整盘视图只能写回 GPT 可用范围内的未分配物理扇区。")?;
                sector_edit::check_unallocated(
                    block,
                    layout.first,
                    layout.last,
                    layout.entries.iter().map(|part| (part.offset, part.size)),
                )?;
                maintenance::protect_write_range(&archive, &layout, block, true)?;
                Native::Unallocated(archive)
            }
        };
        Ok(Proof { block, native })
    }
}

#[cfg(windows)]
fn read_block(file: &mut File, block: Range) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; block.size as usize];
    file.seek(SeekFrom::Start(block.offset))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}
pub fn prepare(request: Request) -> Result<Plan, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("扇区编辑仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        job("预览扇区修改", || {
            *pending() = None;
            let page = sector_tasks::page_state()
                .as_ref()
                .filter(|p| p.id == request.page_id)
                .cloned()
                .ok_or("扇区页面已失效，请重新读取。")?;
            let edited = sector_edit::apply(&page.bytes, &request.edits)?;
            let ctx = Context::open(&page.source, None)?;
            let range = ctx.page_range(&page)?;
            let (resolved, _, fingerprint) = ctx.binding();
            let mut file = device::device(resolved, false)?;
            let proof = ctx.proof(&mut file, &page, range)?;
            let before = read_block(&mut file, proof.block)?;
            let at = (range.offset - proof.block.offset) as usize;
            if before[at..at + edited.len()] != page.bytes {
                return Err("扇区内容已变化，请重新读取后编辑。".into());
            }
            let mut after = before.clone();
            after[at..at + edited.len()].copy_from_slice(&edited);
            if Context::open(&page.source, None)?.binding().2 != fingerprint {
                return Err("预览期间磁盘或卷信息已变化。".into());
            }
            let changes = page
                .bytes
                .iter()
                .zip(&edited)
                .enumerate()
                .filter_map(|(i, (&before, &after))| {
                    (before != after).then_some(Change {
                        offset: i as u32,
                        before,
                        after,
                    })
                })
                .collect();
            let plan = Plan {
                token: uuid::Uuid::new_v4().to_string(),
                target: resolved.label.clone(),
                relative_sector: page.sector,
                absolute_sector: range.offset / page.sector_size,
                sector_size: page.sector_size,
                write_size: proof.block.size,
                write_start_lba: proof.block.offset / page.sector_size,
                before_sha256: sector_edit::hash(&page.bytes),
                after_sha256: sector_edit::hash(&edited),
                changes,
                warning: "直接修改扇区可能破坏文件或文件系统。确认后将锁定同盘所有卷并让整块磁盘脱机，执行一次写入并回读 SHA-256；成功或失败都保持脱机，不自动恢复原内容。".into(),
            };
            *pending() = Some(Pending {
                plan: plan.clone(),
                page,
                before,
                after,
                fingerprint: fingerprint.to_owned(),
                proof,
                expires: Instant::now() + Duration::from_secs(300),
            });
            Ok(plan)
        })
    }
}
pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("扇区编辑仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        job("保存扇区修改", || {
            let item = take(&mut pending(), &token, Instant::now())?;
            if sector_tasks::page_state()
                .as_ref()
                .is_none_or(|p| p.id != item.page.id)
            {
                return Err("原扇区页面已被替换，请重新预览。".into());
            }
            let ctx = Context::open(&item.page.source, None)?;
            let (resolved, parts, fingerprint) = ctx.binding();
            if fingerprint != item.fingerprint {
                return Err("磁盘或卷状态与预览不一致。".into());
            }
            let locks = device::lock(resolved)?;
            let fresh = Context::open(&item.page.source, Some((&ctx, &locks)))?;
            device::verify_locks(&locks, resolved, &parts.iter().collect::<Vec<_>>())?;
            let range = fresh.page_range(&item.page)?;
            let mut file = device::exclusive_device(fresh.binding().0)?;
            if fresh.proof(&mut file, &item.page, range)? != item.proof
                || read_block(&mut file, item.proof.block)? != item.before
            {
                return Err("原生布局或源扇区与预览不同，未写入。".into());
            }
            *sector_tasks::page_state() = None;
            device::offline(&file).map_err(|e| {
                crate::native_locale::diagnostic("磁盘可能已脱机，尚未发出扇区写入。{error}", &e)
            })?;
            let result = (|| -> Result<String, String> {
                device::confirm_writable_offline(&file)?;
                if fresh.proof(&mut file, &item.page, range)? != item.proof {
                    return Err("脱机后分区布局变化，未写入。".into());
                }
                sector_edit::write_once(
                    &mut file,
                    item.proof.block,
                    &item.before,
                    &item.after,
                    |f| f.sync_all(),
                )?;
                if fresh.proof(&mut file, &item.page, range)? != item.proof {
                    return Err("写入后分区布局校验不一致。".into());
                }
                device::confirm_writable_offline(&file)?;
                Ok(item.plan.after_sha256.clone())
            })();
            let sha256 = result.map_err(|e| {
                crate::native_locale::diagnostic(
                    "磁盘保持脱机；可能存在部分改动，未重试或回滚。{error}",
                    &e,
                )
            })?;
            Ok(Outcome {message:format!("已保存 {} 个字节改动，整个扇区 SHA-256 回读一致。磁盘保持脱机，请核对后再手动联机。",item.plan.changes.len()),sha256})
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Pending {
        Pending {
            plan: Plan {
                token: "once".into(),
                target: "fixture".into(),
                relative_sector: 0,
                absolute_sector: 2048,
                sector_size: 512,
                write_size: 512,
                write_start_lba: 2048,
                before_sha256: String::new(),
                after_sha256: String::new(),
                changes: vec![],
                warning: String::new(),
            },
            page: Page {
                id: "page".into(),
                source: crate::data_tasks::Selection {
                    target: crate::partitions::Target("a".repeat(64)),
                    layout_key: "layout".into(),
                    partition: Some(1),
                },
                label: "synthetic".into(),
                source_offset: 1048576,
                source_size: 1048576,
                sector_size: 512,
                sector: 0,
                bytes: vec![0; 512],
                sha256: String::new(),
            },
            before: vec![0; 512],
            after: vec![1; 512],
            fingerprint: "proof".into(),
            proof: Proof {
                block: crate::surface_scan::Range {
                    offset: 1048576,
                    size: 512,
                    sector: 512,
                },
                native: Native::Partition(vec![]),
            },
            expires: Instant::now() + Duration::from_secs(300),
        }
    }
    #[test]
    fn confirmations_are_single_use_and_expire_including_mismatched_attempts() {
        let mut state = Some(fixture());
        assert!(take(&mut state, "wrong", Instant::now()).is_err());
        assert!(state.is_none());
        state = Some(fixture());
        let expiry = state.as_ref().unwrap().expires;
        assert!(take(&mut state, "once", expiry).is_err());
        assert!(state.is_none());
        state = Some(fixture());
        assert!(take(&mut state, "once", Instant::now()).is_ok());
        assert!(take(&mut state, "once", Instant::now()).is_err());
    }
}

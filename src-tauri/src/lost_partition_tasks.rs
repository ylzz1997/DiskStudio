use crate::{
    data_tasks::{self, Selection},
    lost_partitions::{self, Candidate},
    surface_scan::{Options, Range},
};
use serde::{Deserialize, Serialize};
use std::sync::{atomic::Ordering, Mutex, OnceLock};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub options: Options,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    pub id: String,
    pub candidate: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoverRequest {
    pub id: String,
    pub candidates: Vec<String>,
}
fn checked_ids(ids: &[String]) -> Result<(), String> {
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    if ids.is_empty() || ids.len() > 128 || unique.len() != ids.len() {
        return Err("请选择 1–128 个不重复的已找到候选。".into());
    }
    Ok(())
}
pub fn selected_many(
    request: RecoverRequest,
) -> Result<(Selection, Range, Vec<Candidate>), String> {
    checked_ids(&request.candidates)?;
    let saved = state();
    let saved = saved
        .as_ref()
        .filter(|s| s.snapshot.id == request.id && s.snapshot.state != "running")
        .ok_or("请先停止搜索或等待搜索结束。")?;
    let candidates = request
        .candidates
        .into_iter()
        .map(|id| {
            saved
                .snapshot
                .candidates
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .ok_or("候选已失效，请重新搜索。".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((saved.source.clone(), saved.range, candidates))
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: String,
    pub target: String,
    pub layout_key: String,
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub options: Options,
    pub candidates: Vec<Candidate>,
    pub message: String,
}
struct Saved {
    snapshot: Snapshot,
    source: Selection,
    range: Range,
}
static STATE: OnceLock<Mutex<Option<Saved>>> = OnceLock::new();
fn state() -> std::sync::MutexGuard<'static, Option<Saved>> {
    STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn status() -> Option<Snapshot> {
    state().as_ref().map(|s| s.snapshot.clone())
}
pub fn selected(request: BrowseRequest) -> Result<(Selection, Range, Candidate), String> {
    let saved = state();
    let saved = saved
        .as_ref()
        .filter(|s| s.snapshot.id == request.id && s.snapshot.state != "running")
        .ok_or("请等待搜索结束，或先停止搜索")?;
    let candidate = saved
        .snapshot
        .candidates
        .iter()
        .find(|c| c.id == request.candidate)
        .ok_or("候选已失效，请重新搜索")?
        .clone();
    Ok((saved.source.clone(), saved.range, candidate))
}
pub fn start(request: Request) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("当前仅支持 Windows 设备搜索".into())
    }
    #[cfg(windows)]
    {
        use crate::data_windows as platform;
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行")?;
        if request.source.partition.is_some() {
            return Err("搜索必须绑定整盘身份，范围通过扇区指定".into());
        }
        let resolved = platform::resolve(&request.source, false, false)?;
        let range = request.options.range(
            0,
            resolved.size,
            resolved.disk.logical_sector_size.ok_or("缺少扇区大小")?,
        )?;
        let (id, cancel) = data_tasks::start("搜索丢失分区")?;
        *state() = Some(Saved {
            snapshot: Snapshot {
                id: id.clone(),
                target: request.source.target.0.clone(),
                layout_key: request.source.layout_key.clone(),
                state: "running".into(),
                done: 0,
                total: range.size,
                options: request.options,
                candidates: vec![],
                message: String::new(),
            },
            source: request.source.clone(),
            range,
        });
        let work_id = id.clone();
        drop(guard);
        std::thread::spawn(move || {
            let result: Result<(String, Option<String>), String> =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _guard = crate::OPERATION
                        .try_lock()
                        .map_err(|_| "另一项磁盘操作正在运行")?;
                    if cancel.load(Ordering::Relaxed) {
                        return Err("搜索已取消，尚未读取设备".into());
                    }
                    let resolved = platform::resolve(&request.source, false, false)?;
                    let checked = request.options.range(
                        0,
                        resolved.size,
                        resolved.disk.logical_sector_size.ok_or("缺少扇区大小")?,
                    )?;
                    if checked != range {
                        return Err("搜索范围已变化".into());
                    }
                    let mut reader = crate::data_io::Region::new(
                        platform::device(&resolved, false)?,
                        range.offset,
                        range.size,
                    )
                    .map_err(|e| e.to_string())?;
                    lost_partitions::scan(
                        &mut reader,
                        Range { offset: 0, ..range },
                        &cancel,
                        |done, candidate| {
                            let mut saved = state();
                            let saved = saved
                                .as_mut()
                                .filter(|s| s.snapshot.id == work_id)
                                .ok_or("搜索会话已失效")?;
                            saved.snapshot.done = done;
                            if let Some(mut c) = candidate {
                                c.offset += range.offset;
                                saved.snapshot.candidates.push(c);
                            }
                            data_tasks::record_progress(
                                &work_id,
                                "搜索文件系统边界",
                                done,
                                range.size,
                            );
                            Ok(())
                        },
                    )?;
                    Ok((
                        "范围搜索结束。候选需要通过目录和文件内容继续核对；尚未修改分区表。".into(),
                        None,
                    ))
                }))
                .unwrap_or_else(|_| Err("搜索异常退出，已保留先前候选；没有写入源磁盘".into()));
            if let Some(saved) = state().as_mut().filter(|s| s.snapshot.id == work_id) {
                saved.snapshot.state = if result.is_ok() {
                    "completed"
                } else if cancel.load(Ordering::Relaxed) {
                    "canceled"
                } else {
                    "failed"
                }
                .into();
                saved.snapshot.message = match &result {
                    Ok((s, _)) => s.clone(),
                    Err(e) => e.clone(),
                };
            }
            data_tasks::finish(&work_id, result);
        });
        Ok(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_accepts_only_unique_retained_ids_never_ranges_or_entry_payloads() {
        let good = serde_json::json!({"id":"session","candidates":["retained"]});
        assert!(serde_json::from_value::<RecoverRequest>(good.clone()).is_ok());
        for key in ["offset", "size", "bytes", "path", "source", "guid"] {
            let mut bad = good.clone();
            bad[key] = serde_json::json!("injected");
            assert!(serde_json::from_value::<RecoverRequest>(bad).is_err());
        }
        assert!(checked_ids(&[]).is_err());
        assert!(checked_ids(&vec!["x".into(); 129]).is_err());
        assert!(checked_ids(&["x".into(), "x".into()]).is_err());
        assert!(checked_ids(&["x".into(), "y".into()]).is_ok());
        assert!(selected_many(serde_json::from_value(good).unwrap()).is_err());
    }
    #[test]
    fn ipc_accepts_only_bound_ids_and_typed_ranges() {
        assert!(serde_json::from_value::<BrowseRequest>(
            serde_json::json!({"id":"x","candidate":"y","offset":0})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(serde_json::json!({"source":{"target":"opaque","layoutKey":"x","partition":null},"options":{"startSector":0,"sectorCount":1},"path":"bad"})).is_err());
        assert!(selected(BrowseRequest {
            id: "absent".into(),
            candidate: "absent".into()
        })
        .is_err());
    }
}

use crate::{
    data_tasks::{self, Selection},
    lost_partitions::{self, Candidate},
    lost_search_scope::{self, Scope},
    surface_scan::Range,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    sync::{atomic::Ordering, Mutex, OnceLock},
};
const MAX_RETAINED_SEARCHES: usize = disk_core::btrfs::MAX_MEMBERS;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub options: Scope,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    pub id: String,
    pub candidate: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseGroupRequest {
    pub anchor: BrowseRequest,
    pub others: Vec<BrowseRequest>,
    pub allow_missing: bool,
}
#[derive(Clone)]
pub(crate) struct Source {
    pub selection: Selection,
    pub range: Range,
    pub candidate: Candidate,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberChoice {
    session: String,
    candidate: String,
    target: String,
    layout_key: String,
    member: String,
    start_sector: u64,
    size: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoverRequest {
    pub id: String,
    pub candidates: Vec<String>,
}
fn checked_ids(ids: &[String]) -> Result<(), String> {
    let unique: HashSet<_> = ids.iter().collect();
    if ids.is_empty() || ids.len() > 128 || unique.len() != ids.len() {
        return Err("请选择 1–128 个不重复的已找到候选。".into());
    }
    Ok(())
}
pub fn selected_many(
    request: RecoverRequest,
) -> Result<(Selection, Vec<(Range, Candidate)>), String> {
    checked_ids(&request.candidates)?;
    let saved = state();
    let session = completed(&saved, &request.id).ok_or("请先停止搜索或等待搜索结束。")?;
    if saved
        .iter()
        .find(|s| s.source.target == session.source.target)
        .is_none_or(|s| s.snapshot.id != request.id)
    {
        return Err("候选已失效，请重新搜索。".into());
    }
    let candidates = request
        .candidates
        .into_iter()
        .map(|id| {
            session
                .snapshot
                .candidates
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .ok_or("候选已失效，请重新搜索。".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    if candidates.iter().any(|candidate| !candidate.can_restore) {
        return Err("该候选仅允许浏览和提取，无法登记分区记录。".into());
    }
    let candidates = candidates
        .into_iter()
        .map(|candidate| Ok((range_for(session, &candidate)?, candidate)))
        .collect::<Result<_, String>>()?;
    Ok((session.source.clone(), candidates))
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
    pub options: Scope,
    pub ranges: Vec<Range>,
    pub candidates: Vec<Candidate>,
    pub message: String,
}
struct Saved {
    snapshot: Snapshot,
    source: Selection,
}
static STATE: OnceLock<Mutex<VecDeque<Saved>>> = OnceLock::new();
fn state() -> std::sync::MutexGuard<'static, VecDeque<Saved>> {
    STATE
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn status(target: Option<&str>) -> Option<Snapshot> {
    state()
        .iter()
        .find(|s| target.is_none_or(|target| s.snapshot.target == target))
        .map(|s| s.snapshot.clone())
}
fn completed<'a>(saved: &'a VecDeque<Saved>, id: &str) -> Option<&'a Saved> {
    saved
        .iter()
        .find(|s| s.snapshot.id == id && s.snapshot.state != "running")
}
fn find<'a>(
    saved: &'a VecDeque<Saved>,
    request: &BrowseRequest,
) -> Result<(&'a Saved, &'a Candidate), String> {
    let session = completed(saved, &request.id).ok_or("请等待搜索结束，或先停止搜索")?;
    let candidate = session
        .snapshot
        .candidates
        .iter()
        .find(|c| c.id == request.candidate)
        .ok_or("候选已失效，请重新搜索")?;
    Ok((session, candidate))
}
fn range_for(saved: &Saved, candidate: &Candidate) -> Result<Range, String> {
    lost_search_scope::containing(&saved.snapshot.ranges, candidate.offset, candidate.size)
}

fn bind(saved: &Saved, candidate: &Candidate) -> Result<Source, String> {
    Ok(Source {
        selection: saved.source.clone(),
        range: range_for(saved, candidate)?,
        candidate: candidate.clone(),
    })
}
pub(crate) fn selected(request: BrowseRequest) -> Result<Source, String> {
    let saved = state();
    let (session, candidate) = find(&saved, &request)?;
    if !candidate.can_browse {
        return Err("此成员需要与同一 Btrfs 卷的其他成员一起读取。".into());
    }
    bind(session, candidate)
}
fn overlaps(left: &Candidate, right: &Candidate) -> bool {
    left.offset < right.offset + right.size && right.offset < left.offset + left.size
}
fn group_anchor(candidate: &Candidate) -> Result<&disk_core::btrfs::RecoveryMember, String> {
    candidate
        .btrfs_member
        .as_ref()
        .filter(|member| member.total_members > 1)
        .ok_or_else(|| "请选择多设备 Btrfs 候选。".into())
}
pub fn group_choices(request: BrowseRequest) -> Result<Vec<MemberChoice>, String> {
    let saved = state();
    let (session, candidate) = find(&saved, &request)?;
    let anchor = group_anchor(candidate)?;
    let mut choices = Vec::new();
    let mut seen = HashSet::new();
    for other in saved.iter().filter(|s| s.snapshot.state != "running") {
        for c in &other.snapshot.candidates {
            let Some(member) = c.btrfs_member.as_ref() else {
                continue;
            };
            if member.committed_set != anchor.committed_set
                || member.id == anchor.id
                || member.uuid == anchor.uuid
                || (other.source.target == session.source.target && overlaps(c, candidate))
                || !seen.insert((
                    &other.source.target.0,
                    &other.source.layout_key,
                    c.offset,
                    c.size,
                ))
            {
                continue;
            }
            choices.push(MemberChoice {
                session: other.snapshot.id.clone(),
                candidate: c.id.clone(),
                target: other.source.target.0.clone(),
                layout_key: other.source.layout_key.clone(),
                member: member.id.to_string(),
                start_sector: c.offset / range_for(other, c)?.sector,
                size: c.size,
            });
        }
    }
    Ok(choices)
}
pub(crate) fn selected_group(request: BrowseGroupRequest) -> Result<Vec<Source>, String> {
    let saved = state();
    select_group_from(&saved, request)
}
fn select_group_from(
    saved: &VecDeque<Saved>,
    request: BrowseGroupRequest,
) -> Result<Vec<Source>, String> {
    let (session, candidate) = find(saved, &request.anchor)?;
    let anchor = group_anchor(candidate)?;
    let count = request.others.len() + 1;
    if count < 2
        || count > anchor.total_members
        || (!request.allow_missing && count != anchor.total_members)
    {
        return Err("请选齐 Btrfs 成员，或明确选择读取部分成员。".into());
    }
    let mut members = HashSet::from([anchor.id]);
    let mut uuids = HashSet::from([anchor.uuid]);
    let mut sources = vec![bind(session, candidate)?];
    for other in &request.others {
        let (session, candidate) = find(saved, other)?;
        let member = group_anchor(candidate)?;
        if member.committed_set != anchor.committed_set
            || !members.insert(member.id)
            || !uuids.insert(member.uuid)
            || sources.iter().any(|s| {
                s.selection.target == session.source.target
                    && (s.selection.layout_key != session.source.layout_key
                        || overlaps(candidate, &s.candidate))
            })
        {
            return Err("Btrfs 候选的提交不一致、成员重复或来源范围冲突。".into());
        }
        sources.push(bind(session, candidate)?);
    }
    Ok(sources)
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
        let range = request.options.explicit(
            resolved.size,
            resolved.disk.logical_sector_size.ok_or("缺少扇区大小")?,
        )?;
        let (id, cancel) = data_tasks::start("搜索丢失分区")?;
        let mut saved = state();
        saved.push_front(Saved {
            snapshot: Snapshot {
                id: id.clone(),
                target: request.source.target.0.clone(),
                layout_key: request.source.layout_key.clone(),
                state: "running".into(),
                done: 0,
                total: range.map_or(0, |range| range.size),
                options: request.options,
                ranges: range.into_iter().collect(),
                candidates: vec![],
                message: String::new(),
            },
            source: request.source.clone(),
        });
        saved.truncate(MAX_RETAINED_SEARCHES);
        drop(saved);
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
                    let sector = resolved.disk.logical_sector_size.ok_or("缺少扇区大小")?;
                    let checked = request.options.explicit(
                        resolved.size,
                        sector,
                    )?;
                    if checked != range {
                        return Err("搜索范围已变化".into());
                    }
                    let mut reader = platform::device(&resolved, false)?;
                    let checkpoint = || {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("搜索已取消。".into());
                        }
                        Ok(())
                    };
                    let (ranges, table) = if let Some(range) = checked {
                        (vec![range], None)
                    } else {
                        let scheme = match resolved.disk.partition_style {
                            Some(1) => "MBR",
                            Some(2) => "GPT",
                            _ => return Err("搜索全部未分配空间需要完整的 GPT 或 MBR 分区表。".into()),
                        };
                        let table = crate::table_archive::capture(
                            &mut reader, resolved.size, sector, request.source.target.0.clone(), false, checkpoint,
                        )?;
                        let ranges = lost_search_scope::unallocated(&table, Some((scheme, &resolved.partition_ranges)))?;
                        (ranges, Some(table))
                    };
                    let total = ranges.iter().map(|r| r.size).sum();
                    {
                        let mut saved = state();
                        let saved = saved.front_mut().filter(|s| s.snapshot.id == work_id)
                            .ok_or("搜索会话已失效")?;
                        saved.snapshot.ranges = ranges.clone();
                        saved.snapshot.total = total;
                    }
                    lost_partitions::scan_ranges(
                        reader.try_clone().map_err(|e| e.to_string())?,
                        &ranges,
                        &cancel,
                        |done, candidate| {
                            let mut saved = state();
                            let saved = saved
                                .front_mut()
                                .filter(|s| s.snapshot.id == work_id)
                                .ok_or("搜索会话已失效")?;
                            saved.snapshot.done = done;
                            if let Some(c) = candidate {
                                saved.snapshot.candidates.push(c);
                            }
                            data_tasks::record_progress(
                                &work_id,
                                "搜索文件系统边界",
                                done,
                                total,
                            );
                            Ok(())
                        },
                    )?;
                    if let Some(table) = table {
                        let checked = table.matches(&mut reader, checkpoint)
                            .and_then(|()| platform::resolve(&request.source, false, false).map(|_| ()));
                        if let Err(error) = checked {
                            if let Some(saved) = state().front_mut().filter(|s| s.snapshot.id == work_id) {
                                saved.snapshot.candidates.clear();
                            }
                            return Err(error);
                        }
                    }
                    Ok((
                        "范围搜索结束。候选需要通过目录和文件内容继续核对；尚未修改分区表。".into(),
                        None,
                    ))
                }))
                .unwrap_or_else(|_| Err("搜索异常退出，已保留先前候选；没有写入源磁盘".into()));
            if let Some(saved) = state().front_mut().filter(|s| s.snapshot.id == work_id) {
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
    use crate::surface_scan::Options;
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
        let mut explicit = serde_json::json!({
            "source": {"target": "a".repeat(64), "layoutKey": "b".repeat(64), "partition": null},
            "options": {"startSector": 0, "sectorCount": 1}
        });
        assert!(serde_json::from_value::<Request>(explicit.clone()).is_ok());
        explicit["path"] = serde_json::json!("bad");
        assert!(serde_json::from_value::<Request>(explicit).is_err());
        let mut search = serde_json::json!({
            "source": {"target": "a".repeat(64), "layoutKey": "b".repeat(64), "partition": null},
            "options": {"scope": "allUnallocated"}
        });
        assert!(serde_json::from_value::<Request>(search.clone()).is_ok());
        search["options"]["startSector"] = serde_json::json!(0);
        assert!(serde_json::from_value::<Request>(search).is_err());
        assert!(selected(BrowseRequest {
            id: "absent".into(),
            candidate: "absent".into()
        })
        .is_err());
    }

    #[test]
    fn group_resolves_prior_searches_and_rejects_mixed_duplicate_or_overlapping_members() {
        use crate::btrfs_recovery::fixture::{allocation_image, set64, sign};
        let mut bytes = allocation_image(true, 4096);
        let primary = &mut bytes[64 * 1024..68 * 1024];
        set64(primary, 0x70, 24 * 1024 * 1024);
        set64(primary, 0x88, 3);
        sign(primary);
        let range = Range {
            offset: 0,
            size: bytes.len() as u64,
            sector: 4096,
        };
        let options = Options {
            start_sector: 0,
            sector_count: range.size / 4096,
        };
        let mut found = Vec::new();
        crate::lost_partitions::scan(
            std::io::Cursor::new(bytes),
            range,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_, candidate| {
                found.extend(candidate);
                Ok(())
            },
        )
        .unwrap();
        let mut base = found.pop().unwrap();
        base.id = "member".into();
        let saved = |id: &str, disk: &str, candidate: Candidate| Saved {
            snapshot: Snapshot {
                id: id.into(),
                target: disk.into(),
                layout_key: "layout".into(),
                state: "completed".into(),
                done: range.size,
                total: range.size,
                options: Scope::Range(options),
                ranges: vec![range],
                candidates: vec![candidate],
                message: String::new(),
            },
            source: Selection {
                target: crate::partitions::Target(disk.into()),
                layout_key: "layout".into(),
                partition: None,
            },
        };
        let mut other = base.clone();
        let member = other.btrfs_member.as_mut().unwrap();
        member.id = 2;
        member.uuid = [2; 16];
        let mut mixed = other.clone();
        mixed.btrfs_member.as_mut().unwrap().committed_set[0] ^= 1;
        let sessions = VecDeque::from([
            saved("newer", "b", other.clone()),
            saved("overlap", "a", other),
            saved("duplicate", "b", base.clone()),
            saved("mixed", "b", mixed),
            saved("earlier", "a", base),
        ]);
        let reference = |id: &str| BrowseRequest {
            id: id.into(),
            candidate: "member".into(),
        };
        let select = |id, allow_missing| {
            select_group_from(
                &sessions,
                BrowseGroupRequest {
                    anchor: reference("earlier"),
                    others: vec![reference(id)],
                    allow_missing,
                },
            )
        };
        let bindings = select("newer", true).unwrap();
        assert_eq!(bindings[0].selection.target.0, "a");
        assert_eq!(bindings[1].selection.target.0, "b");
        assert!(select("newer", false).is_err());
        for id in ["overlap", "duplicate", "mixed", "expired"] {
            assert!(select(id, true).is_err(), "accepted {id}");
        }
    }
}

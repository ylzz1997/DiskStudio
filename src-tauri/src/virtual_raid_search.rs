//! Bounded lost-partition search on a reconstructed RAID logical address space.
use super::*;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    id: String,
    #[serde(skip)]
    range: surface_scan::Range,
    state: &'static str,
    done: u64,
    total: u64,
    candidates: Vec<lost_partitions::Candidate>,
    message: String,
}

impl Search {
    pub(super) fn read_only(&self) -> Self {
        let mut snapshot = self.clone();
        for candidate in &mut snapshot.candidates {
            candidate.can_restore = false;
        }
        snapshot
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    id: String,
    options: surface_scan::Options,
}

pub fn search(request: SearchRequest) -> Result<String, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        let saved = {
            let s = state();
            idle(&s)?;
            find(&s, &request.id)?
        };
        if saved.info.options.kind != Kind::Disk {
            return Err("请以整盘范围组装 RAID 后搜索丢失分区。".into());
        }
        let range = request
            .options
            .range(0, saved.info.size, saved.info.options.sector)?;
        let (id, cancel) = start("搜索虚拟 RAID 丢失分区")?;
        state().raid_search = Some(Search {
            id: id.clone(),
            range,
            state: "running",
            done: 0,
            total: range.size,
            candidates: Vec::new(),
            message: String::new(),
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
                        return Err("任务已取消。".into());
                    }
                    let locks = saved.array.lock()?;
                    let reader = saved.array.reader(&locks)?;
                    scan_range(reader, range, &cancel, |done, candidate| {
                        {
                            let mut s = state();
                            let search = s
                                .raid_search
                                .as_mut()
                                .filter(|s| s.id == work_id)
                                .ok_or("搜索会话已失效")?;
                            search.done = done;
                            if let Some(candidate) = candidate {
                                search.candidates.push(candidate);
                            }
                        }
                        progress(&work_id, &cancel, "搜索文件系统边界", done, range.size)
                    })?;
                    for lock in &locks {
                        lock.verify()?;
                    }
                    if !saved.array.physical() {
                        saved.array.validate()?;
                    }
                    Ok((
                        "RAID 分区搜索结束。可浏览通过文件系统校验的候选。".into(),
                        None,
                    ))
                }))
                .unwrap_or_else(|_| Err("RAID 分区搜索异常退出。".into()));
            if let Some(search) = state().raid_search.as_mut().filter(|s| s.id == work_id) {
                search.state = if result.is_ok() {
                    "completed"
                } else if cancel.load(Ordering::Relaxed) {
                    "canceled"
                } else {
                    "failed"
                };
                search.message = match &result {
                    Ok((message, _)) => message.clone(),
                    Err(error) => error.clone(),
                };
            }
            finish(&work_id, result);
        });
        Ok(id)
    }
}

fn scan_range(
    reader: impl Read + Seek,
    range: surface_scan::Range,
    cancel: &AtomicBool,
    mut publish: impl FnMut(u64, Option<lost_partitions::Candidate>) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader =
        data_io::Region::new(reader, range.offset, range.size).map_err(|e| e.to_string())?;
    lost_partitions::scan(
        &mut reader,
        surface_scan::Range { offset: 0, ..range },
        cancel,
        |done, candidate| {
            publish(
                done,
                candidate.map(|mut candidate| {
                    candidate.offset += range.offset;
                    candidate
                }),
            )
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseFoundRequest {
    id: String,
    candidate: String,
    mode: ScanMode,
}

pub fn browse_found(request: BrowseFoundRequest) -> Result<String, String> {
    browse(request.mode, |s| {
        let saved = find(s, &request.id)?;
        let search = s
            .raid_search
            .as_ref()
            .filter(|search| search.state != "running")
            .ok_or("请先停止搜索或等待搜索结束。")?;
        let candidate = search
            .candidates
            .iter()
            .find(|candidate| candidate.id == request.candidate && candidate.can_browse)
            .ok_or("候选已失效，请重新搜索")?;
        Ok(Source {
            saved,
            scope: Scope::Found {
                range: search.range,
                candidate: Arc::new(candidate.clone()),
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::virtual_raid::{Layout, Member, Reader};
    use std::io::Cursor;

    #[test]
    fn raid_search_translates_bounded_candidate_offsets_and_revalidates() {
        let ntfs = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let stripe = 64 * 1024;
        let prefix = 256 * 1024;
        let mut logical = vec![0; prefix];
        logical.extend_from_slice(ntfs);
        let size = logical.len() as u64;
        let mut members = [Vec::new(), Vec::new()];
        for (number, bytes) in logical.chunks_exact(stripe).enumerate() {
            members[number % 2].extend_from_slice(bytes);
        }
        let open = |members: &[Vec<u8>; 2]| {
            Reader::open(
                Config {
                    layout: Layout::Raid0,
                    sector: 512,
                    stripe: stripe as u64,
                },
                members
                    .iter()
                    .map(|bytes| Member {
                        range: Range {
                            start: 0,
                            size: bytes.len() as u64,
                        },
                        reader: Some(Cursor::new(bytes.clone())),
                    })
                    .collect(),
            )
            .unwrap()
        };
        let range = surface_scan::Range {
            offset: stripe as u64,
            size: size - stripe as u64,
            sector: 512,
        };
        let cancel = AtomicBool::new(false);
        let mut candidates = Vec::new();
        scan_range(open(&members), range, &cancel, |_, candidate| {
            candidates.extend(candidate);
            Ok(())
        })
        .unwrap();
        let [found] = candidates.as_slice() else {
            panic!("expected one bounded NTFS candidate")
        };
        // The fixture includes a padding sector outside the filesystem.
        let declared = ntfs.len() - 512;
        assert_eq!((found.offset, found.size), (prefix as u64, declared as u64));
        let mut reader =
            lost_partitions::candidate_reader(open(&members), size, 512, range, found, &cancel)
                .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.as_slice(), &ntfs[..declared]);
        members[0][prefix / 2 + 3] ^= 1;
        assert!(lost_partitions::candidate_reader(
            open(&members),
            size,
            512,
            range,
            found,
            &cancel
        )
        .is_err());
    }
}

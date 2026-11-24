//! Search the logical extent of an open image or verified backup point.
use super::*;
use crate::{
    image_browser::Kind,
    lost_partitions,
    lost_search_scope::{self, Scope},
    surface_scan,
};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum KindOfSource {
    Image,
    Backup,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Session {
    kind: KindOfSource,
    id: String,
}

#[derive(Clone)]
enum Backing {
    Image(image_browse_tasks::Opened),
    Backup(OpenedBackup),
}

impl Backing {
    fn get(s: &State, session: &Session) -> Result<Self, String> {
        match session.kind {
            KindOfSource::Image => s
                .opened_image
                .as_ref()
                .filter(|image| {
                    image.browser.info.id == session.id
                        && image.browser.info.options.kind != Kind::Partition
                })
                .cloned()
                .map(Self::Image),
            KindOfSource::Backup => s
                .opened_backup
                .as_ref()
                .filter(|backup| {
                    backup.browser.info.id == session.id && backup.browser.info.kind == "disk"
                })
                .cloned()
                .map(Self::Backup),
        }
        .ok_or_else(|| "请选择当前打开的整盘镜像或整盘备份。".into())
    }

    fn geometry(&self) -> (u64, u64) {
        match self {
            Self::Image(image) => (image.browser.info.size, image.browser.info.options.sector),
            Self::Backup(backup) => (backup.browser.info.size, backup.browser.info.sector),
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Image(image) => image.validate(),
            Self::Backup(backup) => backup.validate(),
        }
    }

    fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
        self.validate()?;
        match self {
            Self::Image(image) => image.browser.logical_reader(),
            Self::Backup(backup) => backup.browser.restore_reader(None),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Found {
    session: Session,
    backing: Backing,
    range: surface_scan::Range,
    pub(super) candidate: Arc<lost_partitions::Candidate>,
}

impl Found {
    pub(super) fn image(&self) -> Option<&str> {
        (self.session.kind == KindOfSource::Image).then_some(self.session.id.as_str())
    }

    pub(super) fn backup(&self) -> Option<&str> {
        (self.session.kind == KindOfSource::Backup).then_some(self.session.id.as_str())
    }

    pub(super) fn disks(&self) -> Vec<Target> {
        match &self.backing {
            Backing::Image(image) => image.source_disks.clone(),
            Backing::Backup(backup) => backup.source_disks.clone(),
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        self.backing.validate()
    }

    fn partition_label(&self) -> String {
        format!(
            "{} · LBA {}",
            self.candidate.filesystem,
            self.candidate.offset / self.range.sector
        )
    }

    pub(super) fn label(&self) -> Result<String, String> {
        self.validate()?;
        let partition = self.partition_label();
        match &self.backing {
            Backing::Image(image) => Ok(format!("{} / {partition}", image.browser.info.name)),
            Backing::Backup(backup) => {
                let source = backup.browser.provenance_for(partition, None)?;
                Ok(format!(
                    "{} / {} / {}",
                    source.archive_name, source.point_label, source.partition
                ))
            }
        }
    }

    pub(super) fn reader(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Box<dyn backup_archive::Source>, String> {
        let (size, sector) = self.backing.geometry();
        Ok(Box::new(lost_partitions::candidate_reader(
            self.backing.reader()?,
            size,
            sector,
            self.range,
            &self.candidate,
            cancel,
        )?))
    }

    pub(super) fn provenance(&self, plan: &mut recovery_export::Plan) -> Result<(), String> {
        let partition = self.partition_label();
        let extent = surface_scan::Range {
            offset: self.candidate.offset,
            size: self.candidate.size,
            sector: self.range.sector,
        };
        match &self.backing {
            Backing::Image(image) => {
                plan.image_source = Some(recovery_export::ImageSource {
                    file_name: image.browser.info.name.clone(),
                    format: image.browser.info.format.clone(),
                    partition,
                    offset: extent.offset.to_string(),
                    size: extent.size.to_string(),
                    sector: extent.sector,
                    parallels_snapshot: image.browser.info.parallels_snapshot.as_ref().map(|point| point.id),
                })
            }
            Backing::Backup(backup) => {
                plan.backup_source = Some(backup.browser.provenance_for(partition, Some(extent))?);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    id: String,
    #[serde(skip)]
    session: Session,
    ranges: Vec<surface_scan::Range>,
    state: &'static str,
    candidates: Vec<lost_partitions::Candidate>,
}

impl Search {
    pub(super) fn read_only(&self) -> Self {
        let mut status = self.clone();
        for candidate in &mut status.candidates {
            candidate.can_restore = false;
        }
        status
    }

    pub(super) fn for_image(&self, id: &str) -> bool {
        self.session.kind == KindOfSource::Image && self.session.id == id
    }

    pub(super) fn for_backup(&self, id: &str) -> bool {
        self.session.kind == KindOfSource::Backup && self.session.id == id
    }
}

#[cfg(windows)]
pub(super) fn selected_image(
    image: &str,
    search_id: &str,
    ids: &[String],
) -> Result<Vec<(surface_scan::Range, lost_partitions::Candidate)>, String> {
    crate::lost_partition_tasks::checked_ids(ids)?;
    let s = state();
    let search = s.retained_partition_search.as_ref().filter(|search| {
        search.for_image(image) && search.id == search_id && matches!(search.state, "completed" | "canceled")
    }).ok_or("候选已失效，请重新搜索。")?;
    ids.iter().map(|id| {
        let candidate = search.candidates.iter().find(|c| c.id == *id && c.can_restore)
            .ok_or("该候选仅允许浏览和提取，无法登记分区记录。")?;
        let range = lost_search_scope::containing(&search.ranges, candidate.offset, candidate.size)?;
        Ok((range, candidate.clone()))
    }).collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    source: Session,
    options: Scope,
}

pub fn search(request: SearchRequest) -> Result<String, String> {
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let backing = {
        let s = state();
        idle(&s)?;
        Backing::get(&s, &request.source)?
    };
    let (size, sector) = backing.geometry();
    let range = request.options.explicit(size, sector)?;
    let session_id = request.source.id.clone();
    let (id, cancel) = start("搜索丢失分区")?;
    state().retained_partition_search = Some(Search {
        id: id.clone(),
        session: request.source,
        ranges: range.into_iter().collect(),
        state: "running",
        candidates: Vec::new(),
    });
    let work_id = id.clone();
    drop(guard);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = crate::OPERATION
                .try_lock()
                .map_err(|_| "另一项磁盘操作正在运行")?;
            let mut reader = backing.reader()?;
            let ranges = if let Some(range) = range {
                vec![range]
            } else {
                use sha2::{Digest, Sha256};
                let source = format!("{:x}", Sha256::digest(session_id.as_bytes()));
                let table = crate::table_archive::capture(
                    &mut reader,
                    size,
                    sector,
                    source,
                    false,
                    || {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("搜索已取消。".into());
                        }
                        Ok(())
                    },
                )?;
                lost_search_scope::unallocated(&table, None)?
            };
            let total = ranges.iter().map(|range| range.size).sum();
            {
                let mut s = state();
                let search = s
                    .retained_partition_search
                    .as_mut()
                    .filter(|search| search.id == work_id)
                    .ok_or("搜索会话已失效")?;
                search.ranges = ranges.clone();
            }
            lost_partitions::scan_ranges(reader, &ranges, &cancel, |done, found| {
                let mut s = state();
                let search = s
                    .retained_partition_search
                    .as_mut()
                    .filter(|search| search.id == work_id)
                    .ok_or("搜索会话已失效")?;
                if let Some(candidate) = found {
                    search.candidates.push(candidate);
                }
                let count = search.candidates.len();
                if let Some(job) = s.job.as_mut().filter(|job| job.id == work_id) {
                    job.phase = "搜索文件系统边界".into();
                    job.done = done;
                    job.total = total;
                    job.found = count;
                }
                Ok(())
            })?;
            backing.validate()?;
            Ok(("搜索结束。可浏览通过文件系统校验的候选。".into(), None))
        }))
        .unwrap_or_else(|_| Err("分区搜索异常退出。".into()));
        if let Some(search) = state()
            .retained_partition_search
            .as_mut()
            .filter(|search| search.id == work_id)
        {
            search.state = if result.is_ok() {
                "completed"
            } else if cancel.load(Ordering::Relaxed) {
                "canceled"
            } else {
                "failed"
            };
        }
        finish(&work_id, result);
    });
    Ok(id)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    source: Session,
    search_id: String,
    candidate: String,
    mode: ScanMode,
}

pub fn browse(request: BrowseRequest) -> Result<String, String> {
    if !matches!(
        request.mode,
        ScanMode::Existing | ScanMode::AllFiles | ScanMode::Deleted
    ) {
        return Err("请选择文件读取模式。".into());
    }
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let found = {
        let s = state();
        idle(&s)?;
        let backing = Backing::get(&s, &request.source)?;
        let search = s
            .retained_partition_search
            .as_ref()
            .filter(|search| {
                search.session == request.source
                    && search.id == request.search_id
                    && search.state != "running"
            })
            .ok_or("搜索会话已失效")?;
        let candidate = search
            .candidates
            .iter()
            .find(|c| c.id == request.candidate && c.can_browse)
            .ok_or("候选已失效，请重新搜索")?;
        Found {
            session: request.source,
            backing,
            range: lost_search_scope::containing(&search.ranges, candidate.offset, candidate.size)?,
            candidate: Arc::new(candidate.clone()),
        }
    };
    let label = found.label()?;
    scan_source_mode(
        ScanSource::RetainedFound(found),
        None,
        label,
        "读取找到的分区目录",
        request.mode,
        guard,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_only_status_preserves_the_primary_evidence_used_to_reopen() {
        let bytes = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let range = surface_scan::Range {
            offset: 0,
            size: bytes.len() as u64,
            sector: 512,
        };
        let cancel = AtomicBool::new(false);
        let candidate = lost_partitions::probe(&mut Cursor::new(bytes), range, 0, &cancel)
            .unwrap()
            .unwrap();
        let search = Search {
            id: "search".into(),
            session: Session {
                kind: KindOfSource::Image,
                id: "session".into(),
            },
            ranges: vec![range],
            state: "completed",
            candidates: vec![candidate],
        };
        assert!(!search.read_only().candidates[0].can_restore);
        assert!(search.candidates[0].can_restore);
        assert!(lost_partitions::candidate_reader(
            Cursor::new(bytes),
            range.size,
            range.sector,
            range,
            &search.candidates[0],
            &cancel,
        )
        .is_ok());
    }
}

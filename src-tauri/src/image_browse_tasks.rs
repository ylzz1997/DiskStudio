//! Read-only image sessions share the recovery engine, never the device-opening path.
use super::*;
use crate::image_browser::{Browser, Info, Options};
#[path = "image_session_tasks.rs"]
pub mod sessions;
#[path = "image_lvm_tasks.rs"]
pub mod lvm;
#[path = "image_spaces_tasks.rs"]
pub mod spaces;

#[derive(Clone)]
pub(crate) struct Opened {
    pub browser: Arc<Browser>,
    pub source_disks: Vec<Target>,
    pub bridge: Option<Arc<crate::image_bridge::Bridge>>,
    #[cfg(windows)]
    pub group: Arc<crate::image_files::Group>,
}
impl Opened {
    pub(crate) fn validate(&self) -> Result<(), String> {
        #[cfg(windows)]
        self.group.validate()?;
        if let Some(bridge) = &self.bridge {
            bridge.healthy()?;
        }
        Ok(())
    }
}
#[cfg(windows)]
pub(crate) fn opened(id: &str) -> Result<Opened, String> {
    let s = state();
    s.opened_image
        .as_ref()
        .filter(|opened| opened.browser.info.id == id)
        .cloned()
        .ok_or_else(|| "镜像浏览会话已失效。".into())
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(flatten)]
    info: Info,
    source_disks: Vec<Target>,
    scan_id: Option<String>,
    scan_partition: Option<u32>,
    scan_mode: Option<ScanMode>,
    search: Option<retained_partition_tasks::Search>,
}
fn status_in(s: &State) -> Option<Status> {
    let opened = s.opened_image.as_ref()?;
    let current = s.scan.as_ref().and_then(|(id, source)| match source.original() {
        ScanSource::Image {
            opened: scan,
            partition,
        } if scan.browser.info.id == opened.browser.info.id => Some((id.clone(), Some(*partition))),
        ScanSource::RetainedFound(found) if found.image() == Some(opened.browser.info.id.as_str()) => Some((id.clone(), None)),
        _ => None,
    });
    Some(Status {
        info: opened.browser.info.clone(),
        source_disks: opened.source_disks.clone(),
        scan_id: current.as_ref().map(|c| c.0.clone()),
        scan_partition: current.as_ref().and_then(|c| c.1),
        scan_mode: current.map(|_| s.scan_mode),
        search: s.retained_partition_search.as_ref().filter(|search| search.for_image(&opened.browser.info.id)).map(retained_partition_tasks::Search::read_only),
    })
}
pub fn status() -> Option<Status> {
    status_in(&state())
}
fn clear_results(s: &mut State) {
    if s.scan.as_ref().is_some_and(|(_, source)| match source.original() {
        ScanSource::Image { .. } => true,
        ScanSource::RetainedFound(found) => found.image().is_some(),
        _ => false,
    }) {
        s.scan = None;
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name.clear();
        s.recovery_progress = None;
    }
    if s.opened_image.as_ref().is_some_and(|image| {
        s.retained_partition_search.as_ref().is_some_and(|search| search.for_image(&image.browser.info.id))
    }) {
        s.retained_partition_search = None;
    }
    s.pending = None;
}
pub fn close(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    if s.opened_image
        .as_ref()
        .is_none_or(|v| v.browser.info.id != id)
    {
        return Err("镜像浏览会话已失效。".into());
    }
    clear_results(&mut s);
    lvm::clear_for_image(&mut s);
    s.opened_image = None;
    Ok(())
}
fn commit(s: &mut State, job_id: &str, opened: Opened) -> Result<(), String> {
    if s.cancel.load(Ordering::Relaxed)
        || s.job
            .as_ref()
            .is_none_or(|j| j.id != job_id || j.state != "running")
    {
        return Err("镜像打开已取消或任务失效；保留原镜像及目录。".into());
    }
    replace(s, opened);
    let job = s.job.as_mut().unwrap();
    job.state = "completed".into();
    job.phase = "完成".into();
    job.message = "镜像已打开。选择分区并读取目录；没有挂载或修改镜像。".into();
    Ok(())
}
fn replace(s: &mut State, opened: Opened) {
    clear_results(s);
    lvm::clear_for_image(s);
    s.opened_image = Some(opened);
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScanRequest {
    pub id: String,
    pub partition: u32,
    pub mode: ScanMode,
}
pub fn scan(request: ScanRequest) -> Result<String, String> {
    if request.mode == ScanMode::Signatures {
        return Err("镜像目录读取请选择现存文件、全部文件或删除候选。".into());
    }
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let opened = {
        let s = state();
        idle(&s)?;
        s.opened_image
            .as_ref()
            .filter(|b| b.browser.info.id == request.id)
            .ok_or("镜像浏览会话已失效。")?
            .clone()
    };
    opened.validate()?;
    let reader = opened.browser.reader(request.partition)?;
    let label = format!("镜像：{}", opened.browser.label(request.partition)?);
    scan_source_mode(
        ScanSource::Image {
            opened,
            partition: request.partition,
        },
        Some(Box::new(reader)),
        label,
        "读取镜像目录",
        request.mode,
        guard,
    )
}

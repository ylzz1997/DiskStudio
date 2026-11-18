use crate::{
    data_tasks::{self, Selection},
    sector_data::{self, SearchResult},
    sector_query::{self, Direction, Query},
    surface_scan::Options,
};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadRequest {
    pub source: Selection,
    pub sector: u64,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub id: String,
    pub source: Selection,
    pub label: String,
    pub source_offset: u64,
    pub source_size: u64,
    pub sector_size: u64,
    pub sector: u64,
    pub bytes: Vec<u8>,
    pub sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    pub source: Selection,
    pub options: Options,
    pub query: Query,
    #[serde(default)]
    pub direction: Direction,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSnapshot {
    pub id: String,
    pub source: Selection,
    pub label: String,
    pub options: Options,
    pub query: Query,
    pub direction: Direction,
    pub sector_size: u64,
    pub result: SearchResult,
}
pub(crate) fn page_state() -> std::sync::MutexGuard<'static, Option<Page>> {
    static STATE: OnceLock<Mutex<Option<Page>>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
fn search_state() -> std::sync::MutexGuard<'static, Option<SearchSnapshot>> {
    static STATE: OnceLock<Mutex<Option<SearchSnapshot>>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn status() -> Option<SearchSnapshot> {
    search_state().clone()
}

// Reserve the shared job as well as OPERATION, so native close and other jobs
// cannot race a short read or a native output dialog. Finish even after panic.
#[cfg(windows)]
fn run<T>(title: &str, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let (id, _) = data_tasks::start(title)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("扇区任务异常结束；没有写入源设备。".into()));
    data_tasks::finish(
        &id,
        result
            .as_ref()
            .map(|_| ("完成".into(), None))
            .map_err(Clone::clone),
    );
    result
}
#[cfg(windows)]
fn read(request: ReadRequest) -> Result<Page, String> {
    use crate::{data_io::Region, data_windows as platform};
    use sha2::{Digest, Sha256};
    let r = platform::resolve(&request.source, false, false)?;
    let sector_size = r.disk.logical_sector_size.ok_or("扇区大小缺失")?;
    let range = Options {
        start_sector: request.sector,
        sector_count: 1,
    }
    .range(r.offset, r.size, sector_size)?;
    // Limit even cache read-ahead to exactly this one logical sector.
    let mut reader = Region::new(platform::device(&r, false)?, range.offset, sector_size)
        .map_err(|e| e.to_string())?;
    let bytes = sector_data::read_sector(&mut reader, sector_size, sector_size, 0)?;
    platform::resolve(&request.source, false, false)?;
    Ok(Page {
        id: uuid::Uuid::new_v4().to_string(),
        source: request.source,
        label: r.label,
        source_offset: r.offset,
        source_size: r.size,
        sector_size,
        sector: request.sector,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes,
    })
}
pub fn read_page(request: ReadRequest) -> Result<Page, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("扇区查看当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        run("读取扇区", || {
            let page = read(request)?;
            *page_state() = Some(page.clone());
            Ok(page)
        })
    }
}
pub fn export(id: String) -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("扇区导出当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        run("导出扇区", || {
            use std::{fs::File, io::Cursor};
            let page = page_state()
                .as_ref()
                .filter(|p| p.id == id)
                .cloned()
                .ok_or("当前扇区已被替换，请重新读取。")?;
            let Some(path) = rfd::FileDialog::new()
                .set_title(crate::native_locale::text("导出当前扇区到另一块磁盘"))
                .add_filter(crate::native_locale::text("扇区数据"), &["bin"])
                .set_file_name(format!("DiskStudio-sector-{}.bin", page.sector))
                .save_file()
            else {
                return Ok(None);
            };
            crate::data_windows::destination_folder(
                path.parent().ok_or("输出目录无效")?,
                &page.source.target,
                page.sector_size,
            )?;
            let fresh = read(ReadRequest {
                source: page.source.clone(),
                sector: page.sector,
            })?;
            if fresh.bytes != page.bytes
                || fresh.source_offset != page.source_offset
                || fresh.source_size != page.source_size
                || fresh.sector_size != page.sector_size
            {
                return Err("源扇区内容或范围已变化，请重新读取后导出。".into());
            }
            let (mut file, publication) = crate::backup_files::create_sector_output(
                &path,
                &page.source.target,
                page.sector_size,
            )?;
            let name = path.file_name().ok_or("输出文件名无效")?.to_string_lossy();
            (|| {
                let hash = crate::sector_copy::copy_verified(
                    &mut Cursor::new(&page.bytes),
                    &mut file,
                    page.sector_size,
                    File::sync_all,
                    |_, _| Ok(()),
                )?;
                if hash != page.sha256 {
                    return Err("导出文件回读 SHA-256 不一致。".into());
                }
                publication.publish()
            })()
            .map_err(|error: String| {
                crate::native_locale::current().format(
                    "{error} 未完成文件保留为 {name}。",
                    &[("error", &error), ("name", &format!("{name}.partial"))],
                )
            })?;
            Ok(Some(
                path.to_string_lossy()
                    .trim_start_matches("\\\\?\\")
                    .to_string(),
            ))
        })
    }
}
pub fn start(request: SearchRequest) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("扇区搜索当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        use crate::{data_io::Region, data_windows as platform};
        let pattern = request.query.compile_for(request.direction)?;
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let resolved = platform::resolve(&request.source, false, false)?;
        let sector_size = resolved.disk.logical_sector_size.ok_or("扇区大小缺失")?;
        let range = request
            .options
            .range(resolved.offset, resolved.size, sector_size)?;
        let (id, cancel) = data_tasks::start("扇区搜索")?;
        *search_state() = Some(SearchSnapshot {
            id: id.clone(),
            source: request.source.clone(),
            label: resolved.label,
            options: request.options,
            query: request.query.clone(),
            direction: request.direction,
            sector_size,
            result: SearchResult {
                state: "running".into(),
                checked_bytes: 0,
                total_bytes: range.size,
                hits: vec![],
                pattern_bytes: pattern.len(),
                message: String::new(),
            },
        });
        drop(guard);
        let work_id = id.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<SearchResult, String> {
                    let _guard = crate::OPERATION
                        .try_lock()
                        .map_err(|_| "另一项磁盘操作正在运行。")?;
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err("搜索已取消，尚未读取设备。".into());
                    }
                    let r = platform::resolve(&request.source, false, false)?;
                    if request.options.range(
                        r.offset,
                        r.size,
                        r.disk.logical_sector_size.ok_or("扇区大小缺失")?,
                    )? != range
                    {
                        return Err("搜索范围已变化。".into());
                    }
                    let mut reader =
                        Region::new(platform::device(&r, false)?, range.offset, range.size)
                            .map_err(|e| e.to_string())?;
                    let mut result = sector_query::search_direction(
                        &mut reader,
                        range.size,
                        &pattern,
                        request.direction,
                        &cancel,
                        |progress| {
                            data_tasks::record_progress(
                                &work_id,
                                "搜索字节",
                                progress.checked_bytes,
                                progress.total_bytes,
                            );
                            if let Some(saved) = &mut *search_state() {
                                saved.result = progress.clone();
                                saved.result.state = "running".into();
                                saved.result.message.clear();
                            }
                        },
                    )?;
                    if let Err(e) = platform::resolve(&request.source, false, false) {
                        result.state = "failed".into();
                        result.message = crate::native_locale::diagnostic(
                            "结束时设备或布局核对失败。匹配仅对应已读取数据。{error}",
                            &e,
                        );
                    }
                    Ok(result)
                },
            ))
            .unwrap_or_else(|_| Err("搜索线程异常结束，源设备未写入。".into()));
            let mut state = search_state();
            if let Some(saved) = state.as_mut().filter(|s| s.id == work_id) {
                match result {
                    Ok(r) => saved.result = r,
                    Err(e) => {
                        saved.result.state = if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            "canceled"
                        } else {
                            "failed"
                        }
                        .into();
                        saved.result.message = e;
                    }
                }
                data_tasks::finish(
                    &work_id,
                    if matches!(saved.result.state.as_str(), "completed" | "limited") {
                        Ok((saved.result.message.clone(), None))
                    } else {
                        Err(saved.result.message.clone())
                    },
                );
            }
        });
        Ok(id)
    }
}

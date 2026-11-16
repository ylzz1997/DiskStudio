#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use disk_core::Disk;
mod backup_archive;
mod backup_browser;
mod backup_encryption;
#[cfg(windows)]
mod backup_files;
mod backup_secrets;
mod data_io;
mod data_tasks;
#[cfg(windows)]
mod data_windows;
mod device_state;
mod directory_metadata;
mod directory_report;
mod disk_identity;
mod export_metadata;
mod ext_allocation;
mod ext_recovery;
#[cfg(windows)]
mod extfs;
mod fat_recovery;
#[cfg(windows)]
mod fat_volume;
mod file_metadata;
mod free_space_wipe;
mod health_data;
mod health_tasks;
#[cfg(windows)]
mod health_windows;
mod image_bridge;
mod image_browser;
mod image_compression;
mod image_conversion_tasks;
mod image_conversion_wire;
mod image_convert;
mod image_creation;
mod image_files;
mod lost_partition_restore;
mod lost_partition_tasks;
mod lost_partitions;
mod maintenance_io;
mod maintenance_tasks;
mod ntfs_allocation;
mod ntfs_checkpoint;
mod ntfs_compression;
mod ntfs_names;
mod ntfs_scan;
mod ntfs_streams;
mod partition_attributes;
mod partitions;
mod recovery;
mod recovery_checkpoint;
mod recovery_export;
#[cfg(windows)]
mod recovery_output;
mod recovery_tree;
mod sector_data;
mod sector_edit;
mod sector_query;
mod sector_tasks;
mod sector_write;
mod signature_checkpoint;
mod signature_scan;
mod signature_zip;
#[cfg(windows)]
mod snapshot;
mod storage;
mod surface_checkpoint;
mod surface_scan;
mod surface_tasks;
mod table_archive;
mod table_diagnostics;
mod table_inspect_task;
mod table_restore;
mod table_restore_task;
mod table_tasks;
mod used_space;
pub(crate) static OPERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tauri::command]
fn backup_password_status() -> Option<backup_secrets::Prompt> {
    backup_secrets::status()
}
#[tauri::command]
fn backup_password_submit(reply: backup_secrets::Reply) -> Result<(), String> {
    backup_secrets::submit(reply)
}

#[tauri::command]
async fn image_creation_begin(
    options: image_creation::Options,
) -> Result<Option<image_conversion_tasks::Report>, String> {
    tauri::async_runtime::spawn_blocking(move || image_conversion_tasks::create(options))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn image_conversion_begin(
    options: image_convert::Options,
) -> Result<Option<image_conversion_tasks::Report>, String> {
    tauri::async_runtime::spawn_blocking(move || image_conversion_tasks::begin(options))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn image_conversion_chunk(request: tauri::ipc::Request<'_>) -> Result<u64, String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("镜像转换只接受有界二进制数据帧。".into());
    };
    let (id, offset, check, bytes) = image_conversion_wire::decode(body)?;
    tauri::async_runtime::spawn_blocking(move || {
        if check {
            image_conversion_tasks::verify_source(id, offset, bytes)
        } else {
            image_conversion_tasks::push(id, offset, bytes)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn image_conversion_finish(id: String) -> Result<image_conversion_tasks::Report, String> {
    tauri::async_runtime::spawn_blocking(move || image_conversion_tasks::finish(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn image_conversion_cancel(id: String) -> Result<(), String> {
    image_conversion_tasks::cancel(id)
}
#[tauri::command]
fn image_conversion_status() -> Option<image_conversion_tasks::Report> {
    image_conversion_tasks::status()
}

#[tauri::command]
async fn maintenance_prepare(
    request: maintenance_tasks::Request,
) -> Result<maintenance_tasks::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || maintenance_tasks::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn maintenance_execute(token: String) -> Result<maintenance_tasks::Report, String> {
    tauri::async_runtime::spawn_blocking(move || maintenance_tasks::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn maintenance_status() -> Option<maintenance_tasks::Report> {
    maintenance_tasks::status()
}
#[tauri::command]
fn maintenance_discard(token: String) {
    maintenance_tasks::discard(token)
}
#[tauri::command]
async fn maintenance_export(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || maintenance_tasks::export(id))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn partition_attributes_prepare(
    request: partition_attributes::Request,
) -> Result<partition_attributes::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || partition_attributes::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn partition_attributes_execute(
    token: String,
) -> Result<partition_attributes::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || partition_attributes::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn partition_attributes_discard(token: String) {
    partition_attributes::discard(token);
}

#[tauri::command]
async fn device_state_prepare(
    request: device_state::Request,
) -> Result<device_state::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || device_state::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn device_state_execute(token: String) -> Result<device_state::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || device_state::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn device_state_discard(token: String) {
    device_state::discard(token);
}

#[tauri::command]
fn data_status() -> Option<data_tasks::Job> {
    data_tasks::status()
}
#[tauri::command]
fn data_scan_status() -> Option<data_tasks::ScanStatus> {
    data_tasks::scan_status()
}
#[tauri::command]
fn data_recovery_progress_status() -> Option<data_tasks::recovery_tasks::Info> {
    data_tasks::recovery_tasks::status()
}
#[tauri::command]
fn data_recovery_progress_pause(id: String) -> Result<(), String> {
    data_tasks::recovery_tasks::pause(id)
}
#[tauri::command]
async fn data_recovery_progress_resume(
    id: String,
    source: data_tasks::Selection,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::recovery_tasks::resume(id, source))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_recovery_progress_save(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::recovery_tasks::save(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_recovery_progress_load(
    source: data_tasks::Selection,
) -> Result<Option<data_tasks::recovery_tasks::Info>, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::recovery_tasks::load(source))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_browse_id(source: data_tasks::Selection) -> Option<String> {
    data_tasks::browse_id(source)
}
#[tauri::command]
fn data_backup_status() -> Option<data_tasks::BackupStatus> {
    data_tasks::backup_status()
}
#[tauri::command]
fn data_image_status() -> Option<data_tasks::image_browse_tasks::Status> {
    data_tasks::image_browse_tasks::status()
}
#[tauri::command]
async fn image_source_select() -> Result<Option<image_files::Info>, String> {
    tauri::async_runtime::spawn_blocking(data_tasks::image_browse_tasks::sessions::select)
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn image_source_add(id: String) -> Result<image_files::Info, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::image_browse_tasks::sessions::add(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn image_source_discard(id: String) -> Result<(), String> {
    data_tasks::image_browse_tasks::sessions::discard(id)
}
#[tauri::command]
async fn image_source_read(
    group: String,
    file: String,
    offset: u64,
    length: usize,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        image_files::read(&group, &file, offset, length).map(tauri::ipc::Response::new)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn image_source_attach(
    request: data_tasks::image_browse_tasks::sessions::Descriptor,
    events: tauri::ipc::Channel<image_bridge::Event>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        data_tasks::image_browse_tasks::sessions::attach(request, move |e| {
            events.send(e).map_err(|e| e.to_string())
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn image_source_reply(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("镜像响应须为二进制帧".into());
    };
    image_bridge::reply_bytes(body)
}
#[tauri::command]
fn image_source_error(session: String, request: String, message: String) -> Result<(), String> {
    image_bridge::reply_error(&session, &request, message)
}
#[tauri::command]
fn data_close_image(id: String) -> Result<(), String> {
    data_tasks::image_browse_tasks::close(id)
}
#[tauri::command]
async fn data_scan_image(
    request: data_tasks::image_browse_tasks::ScanRequest,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::image_browse_tasks::scan(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_open_backup() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(data_tasks::open_backup)
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_close_backup(id: String) -> Result<(), String> {
    data_tasks::close_backup(id)
}
#[tauri::command]
async fn data_scan_backup(request: data_tasks::BackupScanRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_backup(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_select_backup_point(request: data_tasks::BackupPointRequest) -> Result<String, String> {
    data_tasks::select_backup_point(request)
}
#[tauri::command]
fn data_discard_plan(token: String) {
    data_tasks::discard_plan(token)
}
#[tauri::command]
fn data_cancel(id: String) -> Result<(), String> {
    data_tasks::cancel(id)
}
#[tauri::command]
async fn data_scan(
    source: data_tasks::Selection,
    mode: recovery::ScanMode,
    signatures: Option<signature_scan::Options>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan(source, mode, signatures))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_results(
    scan_id: String,
    offset: usize,
    query: String,
    filter: Option<recovery_tree::Filter>,
) -> Result<data_tasks::Results, String> {
    data_tasks::filtered_results(scan_id, offset, query, filter.unwrap_or_default())
}
#[tauri::command]
fn data_folders(
    scan_id: String,
    folder: Option<String>,
    offset: usize,
) -> Result<recovery_tree::FolderPage, String> {
    data_tasks::folders(scan_id, folder, offset)
}
#[tauri::command]
fn data_select_results(
    scan_id: String,
    query: String,
    filter: recovery_tree::Filter,
) -> Result<Vec<u32>, String> {
    data_tasks::select_results(scan_id, query, filter)
}
#[tauri::command]
async fn data_export_directory_report(
    request: data_tasks::DirectoryReportRequest,
) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::export_directory_report(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_preview(scan_id: String, file_id: u32) -> Result<data_tasks::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::preview(scan_id, file_id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_prepare(request: data_tasks::Request) -> Result<Option<data_tasks::Plan>, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_execute(token: String) -> Result<String, String> {
    data_tasks::execute(token)
}

#[tauri::command]
async fn partitions_status() -> Result<partitions::Status, String> {
    tauri::async_runtime::spawn_blocking(partitions::status)
        .await
        .map_err(|_| "无法读取磁盘操作能力。".into())
}
#[tauri::command]
async fn partitions_prepare(request: partitions::Request) -> Result<partitions::Plan, String> {
    tauri::async_runtime::spawn_blocking(move || partitions::prepare(request))
        .await
        .map_err(|_| "预览任务未能完成。".to_string())?
}
#[tauri::command]
async fn partitions_execute(token: String) -> Result<partitions::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || partitions::execute(token))
        .await
        .map_err(|_| "执行任务未能返回结果，请刷新核对实际状态。".to_string())?
}

#[tauri::command]
async fn local_disks() -> Result<storage::Snapshot, String> {
    tauri::async_runtime::spawn_blocking(storage::read_snapshot)
        .await
        .map_err(|_| "读取本机磁盘信息的任务未能完成。".to_string())?
}

#[tauri::command]
async fn quick_partition_prepare(
    request: partitions::quick::Request,
) -> Result<partitions::quick::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || partitions::quick::prepare(request))
        .await
        .map_err(|_| "快速分区预览任务未能完成。".to_string())?
}
#[tauri::command]
async fn quick_partition_execute(
    token: String,
    progress: tauri::ipc::Channel<partitions::quick::Progress>,
) -> Result<partitions::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        partitions::quick::execute_with_progress(token, |event| {
            // Progress delivery is advisory. A closed renderer must never repeat,
            // interrupt or change the outcome of a dispatched storage operation.
            let _ = progress.send(event);
        })
    })
    .await
    .map_err(|_| "批量执行任务未能返回结果；请核对实际状态，勿重复提交。".to_string())?
}
#[tauri::command]
fn quick_partition_discard(token: String) {
    partitions::quick::discard(token);
}
#[tauri::command]
fn parse_image(head: Vec<u8>, size: u64, filename: String) -> Result<Disk, String> {
    disk_core::parse_image(&head, size, &filename)
}
#[tauri::command]
fn inspect_image(
    head: Vec<u8>,
    tail: Vec<u8>,
    size: u64,
    filename: String,
    ebrs: Option<Vec<disk_core::mbr::EbrSector>>,
) -> Result<disk_core::table_check::ImageInspection, String> {
    disk_core::table_check::inspect_image_with_ebrs(
        &head,
        &tail,
        size,
        &filename,
        &ebrs.unwrap_or_default(),
    )
}
#[tauri::command]
fn image_ebr_next(
    mbr: Vec<u8>,
    size: u64,
    ebrs: Vec<disk_core::mbr::EbrSector>,
) -> Result<Option<u64>, String> {
    disk_core::mbr::next_ebr(&mbr, size, &ebrs)
}

#[tauri::command]
async fn image_inflate_grain(bytes: Vec<u8>, expected: usize) -> Result<Vec<u8>, String> {
    tauri::async_runtime::spawn_blocking(move || image_compression::inflate_grain(&bytes, expected))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_start(request: surface_tasks::Request) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::start(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn lost_partition_start(request: lost_partition_tasks::Request) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || lost_partition_tasks::start(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn lost_partition_prepare_recovery(
    request: lost_partition_tasks::RecoverRequest,
) -> Result<table_restore_task::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || table_restore_task::prepare_recovery(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn lost_partition_status() -> Option<lost_partition_tasks::Snapshot> {
    lost_partition_tasks::status()
}
#[tauri::command]
async fn lost_partition_browse(
    request: lost_partition_tasks::BrowseRequest,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_lost(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn surface_status() -> Option<surface_tasks::Snapshot> {
    surface_tasks::status()
}
#[tauri::command]
fn surface_pause(id: String) -> Result<(), String> {
    surface_tasks::pause(id)
}
#[tauri::command]
async fn surface_resume(id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::resume(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_save_progress(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::save_progress(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_load_progress(
    target: partitions::Target,
) -> Result<Option<surface_tasks::Snapshot>, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::load_progress(target))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_export(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::export(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn health_read(request: health_tasks::Request) -> Result<health_tasks::Report, String> {
    tauri::async_runtime::spawn_blocking(move || health_tasks::read(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn table_backup(
    request: table_tasks::BackupRequest,
) -> Result<Option<table_tasks::Report>, String> {
    tauri::async_runtime::spawn_blocking(move || table_tasks::backup(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn table_verify() -> Result<Option<table_tasks::Report>, String> {
    tauri::async_runtime::spawn_blocking(table_tasks::verify)
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn table_inspect(
    request: table_inspect_task::Request,
) -> Result<table_inspect_task::Report, String> {
    tauri::async_runtime::spawn_blocking(move || table_inspect_task::inspect(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn table_inspect_discard(id: String) {
    table_inspect_task::discard(id)
}
#[tauri::command]
async fn table_repair_prepare(
    request: table_restore_task::RepairRequest,
) -> Result<table_restore_task::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || table_restore_task::prepare_repair(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn table_restore_prepare(
    request: table_restore_task::Request,
) -> Result<Option<table_restore_task::Preview>, String> {
    tauri::async_runtime::spawn_blocking(move || table_restore_task::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn table_restore_execute(token: String) -> Result<table_restore_task::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || table_restore_task::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn table_restore_discard(token: String) {
    table_restore_task::discard(token)
}
#[tauri::command]
async fn sector_read(request: sector_tasks::ReadRequest) -> Result<sector_tasks::Page, String> {
    tauri::async_runtime::spawn_blocking(move || sector_tasks::read_page(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_search(request: sector_tasks::SearchRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || sector_tasks::start(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn sector_status() -> Option<sector_tasks::SearchSnapshot> {
    sector_tasks::status()
}
#[tauri::command]
async fn sector_export(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_tasks::export(id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_prepare_write(
    request: sector_write::Request,
) -> Result<sector_write::Plan, String> {
    tauri::async_runtime::spawn_blocking(move || sector_write::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_execute_write(token: String) -> Result<sector_write::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || sector_write::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn sector_discard_write(token: String) {
    sector_write::discard(token)
}
fn main() {
    tauri::Builder::default()
        .on_window_event(|_, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if data_tasks::status().is_some_and(|j| j.state == "running") {
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            backup_password_status,
            backup_password_submit,
            maintenance_prepare,
            maintenance_execute,
            maintenance_status,
            maintenance_discard,
            maintenance_export,
            device_state_prepare,
            partition_attributes_prepare,
            partition_attributes_execute,
            partition_attributes_discard,
            device_state_execute,
            device_state_discard,
            health_read,
            table_backup,
            table_verify,
            table_inspect,
            table_inspect_discard,
            table_repair_prepare,
            table_restore_prepare,
            table_restore_execute,
            table_restore_discard,
            sector_read,
            sector_search,
            sector_status,
            sector_export,
            sector_prepare_write,
            sector_execute_write,
            sector_discard_write,
            surface_start,
            lost_partition_start,
            lost_partition_status,
            lost_partition_browse,
            lost_partition_prepare_recovery,
            surface_status,
            surface_export,
            surface_pause,
            surface_resume,
            surface_save_progress,
            surface_load_progress,
            parse_image,
            inspect_image,
            image_ebr_next,
            image_inflate_grain,
            image_conversion_begin,
            image_creation_begin,
            image_conversion_chunk,
            image_conversion_finish,
            image_conversion_cancel,
            image_conversion_status,
            local_disks,
            partitions_status,
            partitions_prepare,
            partitions_execute,
            quick_partition_prepare,
            quick_partition_execute,
            quick_partition_discard,
            data_status,
            data_scan_status,
            data_recovery_progress_status,
            data_recovery_progress_pause,
            data_recovery_progress_resume,
            data_recovery_progress_save,
            data_recovery_progress_load,
            data_browse_id,
            data_backup_status,
            data_image_status,
            image_source_select,
            image_source_add,
            image_source_discard,
            image_source_read,
            image_source_attach,
            image_source_reply,
            image_source_error,
            data_close_image,
            data_scan_image,
            data_open_backup,
            data_close_backup,
            data_scan_backup,
            data_select_backup_point,
            data_discard_plan,
            data_cancel,
            data_scan,
            data_results,
            data_folders,
            data_select_results,
            data_export_directory_report,
            data_preview,
            data_prepare,
            data_execute
        ])
        .run(tauri::generate_context!())
        .expect("DiskStudio could not start");
}

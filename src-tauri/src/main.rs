#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use disk_core::Disk;
mod backup_archive;
mod backup_browser;
mod backup_encryption;
#[cfg(windows)]
mod backup_files;
mod bitlocker;
mod backup_secrets;
mod btrfs_recovery;
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
mod ldm_basic;
mod ldm_tasks;
mod lvm_tasks;
mod maintenance_io;
mod maintenance_tasks;
mod native_locale;
mod operation_gate;
mod ntfs_allocation;
mod ntfs_checkpoint;
#[cfg(all(test, windows))]
mod ntfs_clone_engine_test;
mod ntfs_compression;
#[cfg(windows)]
mod ntfs_file_clone;
mod ntfs_names;
mod ntfs_scan;
mod ntfs_streams;
mod partition_attributes;
#[cfg(any(test, windows))]
mod partition_move;
mod partition_move_task;
mod partitions;
mod recovery;
mod recovery_checkpoint;
mod recovery_export;
#[cfg(windows)]
mod recovery_output;
mod recovery_tree;
mod sector_copy;
#[cfg(windows)]
mod windows_protection;
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
mod storage_spaces;
mod surface_checkpoint;
mod surface_repair;
mod surface_report;
mod surface_scan;
mod surface_tasks;
mod table_archive;
mod table_diagnostics;
mod table_inspect_task;
mod table_restore;
mod table_restore_task;
mod table_tasks;
mod used_space;
pub(crate) static OPERATION: operation_gate::OperationGate = operation_gate::OperationGate::new();

#[tauri::command]
async fn read_storage_spaces() -> Result<Vec<storage_spaces::Pool>, String> {
    tauri::async_runtime::spawn_blocking(storage_spaces::read)
        .await
        .map_err(|error| error.to_string())?
}

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
async fn partition_move_prepare(
    request: partition_move_task::Request,
) -> Result<Option<partition_move_task::Preview>, String> {
    tauri::async_runtime::spawn_blocking(move || partition_move_task::prepare(request))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn partition_move_resume(
    source: data_tasks::Selection,
) -> Result<Option<partition_move_task::Preview>, String> {
    tauri::async_runtime::spawn_blocking(move || partition_move_task::resume(source))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn partition_move_execute(token: String) -> Result<partition_move_task::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || partition_move_task::execute(token))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn partition_move_discard(token: String) {
    partition_move_task::discard(token);
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
async fn data_reconstruct_filesystem(
    request: data_tasks::ReconstructionSource,
    mode: recovery::ScanMode,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::reconstruct_filesystem(request, mode))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_scan_btrfs_members(
    members: Vec<data_tasks::Selection>,
    mode: recovery::ScanMode,
    allow_missing: bool,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        data_tasks::scan_btrfs_members(members, mode, allow_missing)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_lvm_inspect(request: lvm_tasks::InspectRequest) -> Result<lvm_tasks::View, String> {
    tauri::async_runtime::spawn_blocking(move || lvm_tasks::inspect(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_lvm_browse(request: lvm_tasks::BrowseRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_lvm(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_dynamic_disk_candidates() -> Result<Vec<ldm_tasks::DeviceCandidate>, String> {
    tauri::async_runtime::spawn_blocking(ldm_tasks::device_candidates)
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn data_dynamic_disk_inspect(request: ldm_tasks::InspectRequest) -> Result<Option<ldm_tasks::View>, String> {
    tauri::async_runtime::spawn_blocking(move || ldm_tasks::inspect(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_dynamic_disk_browse(request: ldm_tasks::BrowseRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_dynamic_disk(request))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn data_dynamic_disk_prepare_basic(session_id: String) -> Result<ldm_tasks::ConversionPreview, String> {
    tauri::async_runtime::spawn_blocking(move || ldm_tasks::prepare_basic_conversion(session_id))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn data_dynamic_disk_execute_basic(token: String) -> Result<ldm_tasks::ConversionOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || ldm_tasks::execute_basic_conversion(token))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
fn data_dynamic_disk_discard_basic(token: String) {
    ldm_tasks::discard_basic_conversion(token);
}
#[tauri::command]
fn data_backup_status() -> Option<data_tasks::BackupStatus> {
    data_tasks::backup_status()
}
#[tauri::command]
fn data_image_status() -> Option<data_tasks::image_browse_tasks::Status> {
    data_tasks::image_browse_tasks::status()
}
use data_tasks::image_browse_tasks::spaces as image_spaces_tasks;
#[tauri::command]
async fn data_image_spaces_select() -> Result<Option<image_spaces_tasks::View>, String> {
    tauri::async_runtime::spawn_blocking(image_spaces_tasks::select)
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn data_image_spaces_open(
    request: image_spaces_tasks::OpenRequest,
) -> Result<data_tasks::image_browse_tasks::Status, String> {
    tauri::async_runtime::spawn_blocking(move || image_spaces_tasks::open(request))
        .await
        .map_err(|error| error.to_string())?
}
#[tauri::command]
fn data_image_spaces_discard(id: String) {
    image_spaces_tasks::discard(id);
}
use data_tasks::image_browse_tasks::lvm as image_lvm_tasks;
#[tauri::command]
fn data_image_lvm_status() -> Option<image_lvm_tasks::Status> {
    image_lvm_tasks::status()
}
#[tauri::command]
async fn data_image_lvm_inspect(
    request: image_lvm_tasks::InspectRequest,
) -> Result<image_lvm_tasks::Status, String> {
    tauri::async_runtime::spawn_blocking(move || image_lvm_tasks::inspect(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_image_lvm_open(allow_degraded_raid: Option<bool>) -> Result<Option<image_lvm_tasks::Status>, String> {
    tauri::async_runtime::spawn_blocking(move || image_lvm_tasks::inspect_raw(allow_degraded_raid.unwrap_or_default()))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_image_lvm_close(id: String) -> Result<(), String> {
    image_lvm_tasks::close(id)
}
#[tauri::command]
async fn data_image_lvm_scan(request: image_lvm_tasks::BrowseRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || image_lvm_tasks::scan(request))
        .await
        .map_err(|e| e.to_string())?
}
use data_tasks::virtual_raid_tasks;
#[tauri::command]
fn data_virtual_raid_status() -> Option<virtual_raid_tasks::Status> {
    virtual_raid_tasks::status()
}
#[tauri::command]
async fn data_virtual_raid_select() -> Result<Option<virtual_raid_tasks::DraftInfo>, String> {
    tauri::async_runtime::spawn_blocking(virtual_raid_tasks::select).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_virtual_raid_select_devices(request: virtual_raid_tasks::PhysicalRequest) -> Result<virtual_raid_tasks::DraftInfo, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::select_physical(request)).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_virtual_raid_detect_md(request: virtual_raid_tasks::DetectRequest) -> Result<virtual_raid_tasks::Detection, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::detect_md(request)).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_virtual_raid_open(request: virtual_raid_tasks::OpenRequest) -> Result<virtual_raid_tasks::Status, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::open(request)).await.map_err(|e| e.to_string())?
}
#[tauri::command]
fn data_virtual_raid_configuration(id: String) -> Result<virtual_raid_tasks::Edit, String> {
    virtual_raid_tasks::configuration(id)
}
#[tauri::command]
fn data_virtual_raid_discard(id: String) -> Result<(), String> {
    virtual_raid_tasks::discard(id)
}
#[tauri::command]
fn data_virtual_raid_close(id: String) -> Result<(), String> {
    virtual_raid_tasks::close(id)
}
#[tauri::command]
async fn data_virtual_raid_scan(request: virtual_raid_tasks::ScanRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::scan(request)).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_virtual_raid_search(request: virtual_raid_tasks::SearchRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::search(request)).await.map_err(|e| e.to_string())?
}
#[tauri::command]
async fn data_virtual_raid_browse_found(request: virtual_raid_tasks::BrowseFoundRequest) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || virtual_raid_tasks::browse_found(request)).await.map_err(|e| e.to_string())?
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
async fn data_scan_bitlocker(
    source: data_tasks::Selection,
    mode: recovery::ScanMode,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_bitlocker(source, mode))
        .await
        .map_err(|e| e.to_string())?
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
) -> Result<partitions::quick::Execution, String> {
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
async fn partition_reallocation_prepare(
    request: partitions::reallocation::Request,
) -> Result<Option<partitions::reallocation::Preview>, String> {
    tauri::async_runtime::spawn_blocking(move || partitions::reallocation::prepare(request))
        .await
        .map_err(|_| "空间重新分配预览任务未能完成。".to_string())?
}
#[tauri::command]
async fn partition_reallocation_execute(
    token: String,
    progress: tauri::ipc::Channel<partitions::quick::Progress>,
) -> Result<partitions::reallocation::Execution, String> {
    tauri::async_runtime::spawn_blocking(move || {
        partitions::reallocation::execute(token, |event| {
            let _ = progress.send(event);
        })
    })
    .await
    .map_err(|_| "批量执行任务未能返回结果，请核对实际状态。".to_string())?
}
#[tauri::command]
fn partition_reallocation_discard(token: String) {
    partitions::reallocation::discard(token);
}
#[tauri::command]
fn partition_reallocation_stop(token: String) -> bool {
    partitions::reallocation::stop(token)
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
    sector: Option<u64>,
    ebrs: Option<Vec<disk_core::mbr::EbrSector>>,
) -> Result<disk_core::table_check::ImageInspection, String> {
    disk_core::table_check::inspect_image_with_geometry(
        &head,
        &tail,
        size,
        &filename,
        sector,
        &ebrs.unwrap_or_default(),
    )
}
#[tauri::command]
fn image_ebr_next(
    mbr: Vec<u8>,
    size: u64,
    sector: Option<u64>,
    ebrs: Vec<disk_core::mbr::EbrSector>,
) -> Result<Option<u64>, String> {
    disk_core::mbr::next_ebr_with_sector(&mbr, size, sector.unwrap_or(512), &ebrs)
}

#[tauri::command]
async fn image_inflate_grain(bytes: Vec<u8>, expected: usize) -> Result<Vec<u8>, String> {
    tauri::async_runtime::spawn_blocking(move || image_compression::inflate_grain(&bytes, expected))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_start(requests: Vec<surface_tasks::Request>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::start(requests))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_prepare_repair(scan_id: String) -> Result<surface_repair::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || surface_repair::prepare(scan_id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_prepare_range_repair(
    request: surface_repair::RangeRequest,
) -> Result<surface_repair::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || surface_repair::prepare_range(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn surface_execute_repair(token: String) -> Result<surface_repair::Outcome, String> {
    tauri::async_runtime::spawn_blocking(move || surface_repair::execute(token))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn surface_discard_repair(token: String) {
    surface_repair::discard(token);
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
fn lost_partition_status(target: Option<String>) -> Option<lost_partition_tasks::Snapshot> {
    lost_partition_tasks::status(target.as_deref())
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
fn lost_partition_btrfs_choices(
    request: lost_partition_tasks::BrowseRequest,
) -> Result<Vec<lost_partition_tasks::MemberChoice>, String> {
    lost_partition_tasks::group_choices(request)
}
#[tauri::command]
async fn lost_partition_browse_btrfs(
    request: lost_partition_tasks::BrowseGroupRequest,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || data_tasks::scan_lost_group(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn surface_status(target: partitions::Target) -> surface_tasks::Status {
    surface_tasks::status(&target)
}
#[tauri::command]
fn surface_pause(id: String) -> Result<(), String> {
    surface_tasks::pause(id)
}
#[tauri::command]
async fn surface_resume(id: String, read_timeout_seconds: Option<u16>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::resume(id, read_timeout_seconds))
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
async fn surface_export(
    id: String,
    format: Option<surface_report::ReportFormat>,
) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || surface_tasks::export(id, format.unwrap_or_default()))
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
async fn sector_copy_start(request: sector_copy::Request) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_copy::start(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_copy_image_start(request: sector_copy::ImageRequest) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_copy::start_image(request))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_copy_resume_file(
    source: data_tasks::Selection,
    reread_filled: Option<bool>,
) -> Result<Option<sector_copy::ResumedFile>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_copy::resume_file(source, reread_filled.unwrap_or(false)))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sector_copy_reread_report(
    source: data_tasks::Selection,
) -> Result<Option<sector_copy::ResumedFile>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_copy::reread_file_report(source))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn sector_copy_report(id: String) -> Option<sector_copy::Report> {
    sector_copy::report(id)
}
#[tauri::command]
async fn sector_copy_save_report(id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || sector_copy::save_report(id))
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
#[tauri::command]
fn set_display_language(language: native_locale::Language) {
    native_locale::set(language);
}
fn main() {
    tauri::Builder::default()
        .on_window_event(|_, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if OPERATION.surface_scanning()
                    || data_tasks::status().is_some_and(|j| j.state == "running")
                {
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            read_storage_spaces,
            set_display_language,
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
            partition_move_prepare,
            partition_move_resume,
            partition_move_execute,
            partition_move_discard,
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
            sector_copy_start,
            sector_copy_image_start,
            sector_copy_resume_file,
            sector_copy_reread_report,
            sector_copy_report,
            sector_copy_save_report,
            sector_prepare_write,
            sector_execute_write,
            sector_discard_write,
            surface_start,
            surface_prepare_repair,
            surface_prepare_range_repair,
            surface_execute_repair,
            surface_discard_repair,
            lost_partition_start,
            lost_partition_status,
            lost_partition_browse,
            lost_partition_btrfs_choices,
            lost_partition_browse_btrfs,
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
            partition_reallocation_prepare,
            partition_reallocation_execute,
            partition_reallocation_discard,
            partition_reallocation_stop,
            data_status,
            data_scan_status,
            data_recovery_progress_status,
            data_recovery_progress_pause,
            data_recovery_progress_resume,
            data_recovery_progress_save,
            data_recovery_progress_load,
            data_browse_id,
            data_reconstruct_filesystem,
            data_backup_status,
            data_image_status,
            data_image_spaces_select,
            data_image_spaces_open,
            data_image_spaces_discard,
            data_image_lvm_status,
            data_image_lvm_inspect,
            data_image_lvm_open,
            data_image_lvm_close,
            data_image_lvm_scan,
            data_virtual_raid_status,
            data_virtual_raid_select,
            data_virtual_raid_select_devices,
            data_virtual_raid_detect_md,
            data_virtual_raid_open,
            data_virtual_raid_configuration,
            data_virtual_raid_discard,
            data_virtual_raid_close,
            data_virtual_raid_scan,
            data_virtual_raid_search,
            data_virtual_raid_browse_found,
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
            data_scan_bitlocker,
            data_scan_btrfs_members,
            data_lvm_inspect,
            data_lvm_browse,
            data_dynamic_disk_candidates,
            data_dynamic_disk_inspect,
            data_dynamic_disk_browse,
            data_dynamic_disk_prepare_basic,
            data_dynamic_disk_execute_basic,
            data_dynamic_disk_discard_basic,
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

//! Production Windows partition backend. Explicit ext4 jobs delegate bounded I/O;
//! shells, arbitrary WQL, user-provided paths, privilege escalation or retries
//! of a mutation. All method targets originate in fresh guarded local metadata.
use super::{
    policy::{self, Disk, LegacyIdentity, Partition},
    *,
};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wmi::WMIConnection;
#[path = "device_state_windows.rs"]
pub(crate) mod device_state;
#[path = "format_windows.rs"]
pub(crate) mod format_windows;
#[path = "maintenance_windows.rs"]
pub(crate) mod maintenance;
#[path = "partition_attributes_windows.rs"]
pub(crate) mod partition_attributes;
#[path = "quick_partition_windows.rs"]
pub(crate) mod quick;
#[path = "volume_manage.rs"]
mod volume_manage;
#[path = "volume_snapshot.rs"]
mod volume_snapshot;

const DISKS: &str = "SELECT __PATH,Number,SerialNumber,UniqueId,FriendlyName,Size,LogicalSectorSize,BusType,PartitionStyle,NumberOfPartitions,IsBoot,IsSystem,IsOffline,IsReadOnly,IsClustered FROM MSFT_Disk";
const PARTITIONS: &str = "SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition";
use crate::OPERATION;
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
struct Pending {
    request: Request,
    binding: String,
    plan: Plan,
    deadline: Instant,
    ext_proof: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
struct Volume {
    #[serde(rename = "__PATH")]
    path: String,
    object_id: String,
    #[serde(rename = "Path")]
    volume_path: Option<String>,
    file_system: Option<String>,
    file_system_label: Option<String>,
    drive_letter: Option<String>,
    size: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename = "MSFT_PartitionToVolume")]
struct PartitionToVolume {}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PageFile {
    name: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PagingVolume {
    #[serde(rename = "DeviceID")]
    device_id: String,
    drive_letter: Option<String>,
    boot_volume: Option<bool>,
    system_volume: Option<bool>,
    page_file_present: Option<bool>,
}

struct PagingProof {
    source_key: String,
    canonical_key: String,
    active_keys: Vec<String>,
}
fn valid_pagefile_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && path.encode_utf16().count() < 32760
        && !path
            .chars()
            .any(|c| c.is_control() || "/*?\"<>|".contains(c))
        && !path[3..].contains(':')
        && path[3..]
            .split('\\')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}
fn paging_proof_with(
    v: &Volume,
    files: &[PageFile],
    mut canonical: impl FnMut(&str, bool) -> Result<String, String>,
) -> Result<PagingProof, String> {
    if files.len() > 256 || files.iter().any(|f| !valid_pagefile_path(&f.name)) {
        return Err("分页文件路径不完整或不是可确认的本地绝对路径。".into());
    }
    let source_key = volume_manage::volume_key(v.volume_path.as_deref().ok_or("缺少目标卷 GUID")?)?;
    let canonical_key = volume_manage::volume_key(&canonical(&format!("{source_key}\\"), false)?)?;
    let active_keys = files
        .iter()
        .map(|f| canonical(&f.name, true).and_then(|s| volume_manage::volume_key(&s)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PagingProof {
        source_key,
        canonical_key,
        active_keys,
    })
}
fn paging_proof(v: &Volume, files: &[PageFile]) -> Result<PagingProof, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    paging_proof_with(v, files, |path, is_file| {
        let input: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut mount = vec![0u16; 32768];
        let root = if is_file {
            if unsafe { GetVolumePathNameW(input.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) }
                == 0
            {
                return Err("无法核对分页文件实际所在的卷，已拒绝操作。".into());
            }
            mount.as_ptr()
        } else {
            input.as_ptr()
        };
        let mut guid = [0u16; 64];
        if unsafe { GetVolumeNameForVolumeMountPointW(root, guid.as_mut_ptr(), guid.len() as u32) }
            == 0
        {
            return Err("无法取得分页文件保护所需的规范卷 GUID。".into());
        }
        let end = guid
            .iter()
            .position(|n| *n == 0)
            .ok_or("卷 GUID 返回值不完整")?;
        String::from_utf16(&guid[..end]).map_err(|_| "卷 GUID 返回值无效".into())
    })
}
fn letterless_paging_clear(
    p: &Partition,
    v: &Volume,
    rows: &[PagingVolume],
    proof: &PagingProof,
) -> Result<(), String> {
    let key = volume_manage::volume_key(v.volume_path.as_deref().ok_or("缺少目标卷 GUID")?)?;
    if !policy::formattable(p)
        || proof.source_key != key
        || proof.active_keys.contains(&proof.canonical_key)
    {
        return Err("目标的分区保护信息缺失、受保护或承载分页文件。".into());
    }
    let matching = rows
        .iter()
        .filter(|r| volume_manage::volume_key(&r.device_id).as_deref() == Ok(key.as_str()))
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err("目标卷的分页文件保护信息不能唯一确认。".into());
    }
    let row = matching[0];
    // RAW volumes can leave all Win32 flags NULL. Protection is established
    // by mandatory MSFT partition flags plus a complete, native-resolved
    // active-pagefile inventory, never by interpreting NULL as false.
    if row.boot_volume == Some(true)
        || row.system_volume == Some(true)
        || row.page_file_present == Some(true)
        || row
            .drive_letter
            .as_deref()
            .is_some_and(|l| !policy::clean(l).is_empty())
    {
        return Err("无盘符目标卷可能为系统、启动或分页文件卷，或其保护信息不完整。".into());
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MethodResult {
    return_value: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Bounds {
    return_value: u32,
    size_min: Option<u64>,
    size_max: Option<u64>,
}

struct Session {
    con: WMIConnection,
    disks: Vec<Disk>,
    partitions: Vec<Partition>,
    pagefiles: Vec<PageFile>,
    legacy: Vec<LegacyIdentity>,
}
pub(crate) struct SectorContext {
    pub resolved: crate::data_windows::Resolved,
    pub parts: Vec<Partition>,
    pub selected: Partition,
    pub fingerprint: String,
    volumes: Vec<(u32, Volume)>,
}
pub(crate) struct TableReadContext {
    pub resolved: crate::data_windows::Resolved,
    pub parts: Vec<Partition>,
    pub fingerprint: String,
    volumes: Vec<(u32, Volume)>,
}
pub(crate) struct TableInspectContext {
    pub resolved: crate::data_windows::Resolved,
    pub parts: Vec<Partition>,
    pub fingerprint: String,
}
/// Read-only diagnostics need complete identity/layout but do not lock volumes,
/// reject the running OS merely for reading, or invoke any Storage methods.
pub(crate) fn table_inspect_context(
    source: &crate::data_tasks::Selection,
) -> Result<TableInspectContext, String> {
    if source.partition.is_some() {
        return Err("分区表检查需要选择整块磁盘。".into());
    }
    let session = Session::open()?;
    let disk = policy::target_disk(&session.disks, &source.target)?;
    policy::corroborate(disk, &session.legacy)?;
    let parts = policy::layout(disk, &session.partitions)?;
    if disk.path.is_empty()
        || disk.is_clustered != Some(false)
        || disk.is_boot.is_none()
        || disk.is_system.is_none()
        || disk.is_read_only.is_none()
        || disk.is_offline.is_none()
        || !matches!(disk.logical_sector_size, Some(512 | 4096))
        || !matches!(disk.partition_style, Some(0 | 1 | 2))
    {
        return Err("磁盘身份或状态不完整，不能检查分区表。".into());
    }
    if crate::disk_identity::layout_key(
        disk.partition_style,
        parts
            .iter()
            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
            .collect(),
    ) != source.layout_key
    {
        return Err("磁盘布局变化，请刷新后重选。".into());
    }
    let fingerprint = digest(&(disk, &parts))?;
    let resolved = crate::data_windows::Resolved {
        disk: disk.clone(),
        offset: 0,
        size: disk.size,
        label: format!(
            "磁盘 {} · {}",
            disk.number,
            disk.serial_number.as_deref().unwrap_or("")
        ),
        volumes: vec![],
        ntfs_volume: false,
        partition_ranges: vec![],
        data_lock_proof: None,
    };
    Ok(TableInspectContext {
        resolved,
        parts: parts.into_iter().cloned().collect(),
        fingerprint,
    })
}
/// Explicit table jobs only. It does not preview a storage-method mutation or
/// take a disk offline. Retained associations require independently held locks.
pub(crate) fn table_read_context(
    source: &crate::data_tasks::Selection,
    retained: Option<(&TableReadContext, &crate::data_windows::Locks)>,
) -> Result<TableReadContext, String> {
    disk_read_context(source, retained, false)
}
fn disk_read_context(
    source: &crate::data_tasks::Selection,
    retained: Option<(&TableReadContext, &crate::data_windows::Locks)>,
    allow_raw: bool,
) -> Result<TableReadContext, String> {
    if source.partition.is_some() {
        return Err("分区表备份需要选择整块磁盘。".into());
    }
    if let Some((old, locks)) = retained {
        crate::data_windows::verify_locks(
            locks,
            &old.resolved,
            &old.parts.iter().collect::<Vec<_>>(),
        )?;
    }
    let session = Session::open()?;
    let disk = policy::target_disk(&session.disks, &source.target)?;
    policy::corroborate(disk, &session.legacy)?;
    let parts = policy::layout(disk, &session.partitions)?;
    if disk.path.is_empty()
        || disk.is_boot != Some(false)
        || disk.is_system != Some(false)
        || disk.is_clustered != Some(false)
        || disk.is_offline.is_none()
        || disk.is_read_only.is_none()
        || !(matches!(disk.partition_style, Some(1 | 2))
            || allow_raw && disk.partition_style == Some(0) && parts.is_empty())
        || !matches!(disk.logical_sector_size, Some(512 | 4096))
        || parts
            .iter()
            .any(|p| p.is_boot != Some(false) || p.is_system != Some(false))
    {
        return Err(
            "一致性分区表备份需要状态完整的非系统 MBR/GPT 磁盘；运行中的系统盘请在离线环境备份。"
                .into(),
        );
    }
    if crate::disk_identity::layout_key(
        disk.partition_style,
        parts
            .iter()
            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
            .collect(),
    ) != source.layout_key
    {
        return Err("分区布局已变化，请刷新后重选。".into());
    }
    let rows: Vec<PagingVolume> = WMIConnection::new()
        .map_err(err)?
        .raw_query(
            "SELECT DeviceID,DriveLetter,BootVolume,SystemVolume,PageFilePresent FROM Win32_Volume",
        )
        .map_err(err)?;
    let mut volumes = vec![];
    let mut keys = std::collections::HashSet::new();
    for p in &parts {
        let old = retained.and_then(|(c, _)| {
            c.volumes
                .iter()
                .find(|(n, _)| *n == p.partition_number)
                .map(|(_, v)| v)
        });
        if let Some(v) = volume_during_validation(session.associated_volume(p)?, old)? {
            let proof = paging_proof(&v, &session.pagefiles)?;
            let matching = rows
                .iter()
                .filter(|r| {
                    volume_manage::volume_key(&r.device_id).as_deref()
                        == Ok(proof.source_key.as_str())
                })
                .collect::<Vec<_>>();
            if matching.len() != 1
                || matching[0].boot_volume == Some(true)
                || matching[0].system_volume == Some(true)
                || matching[0].page_file_present == Some(true)
                || proof.active_keys.contains(&proof.canonical_key)
                || !keys.insert(proof.source_key.clone())
                || p.drive_letter.as_deref().map(policy::clean).unwrap_or("")
                    != v.drive_letter.as_deref().map(policy::clean).unwrap_or("")
            {
                return Err("不能排除同盘系统、分页文件或不唯一卷关联，未锁定或读取设备。".into());
            }
            volumes.push((p.partition_number, v));
        }
    }
    let after: Vec<PageFile> = WMIConnection::new()
        .map_err(err)?
        .raw_query("SELECT Name FROM Win32_PageFileUsage")
        .map_err(err)?;
    let names = |fs: &[PageFile]| {
        let mut result = fs
            .iter()
            .map(|f| f.name.to_ascii_uppercase())
            .collect::<Vec<_>>();
        result.sort();
        result
    };
    if names(&after) != names(&session.pagefiles) {
        return Err("核对期间分页文件清单变化。".into());
    }
    let fingerprint = digest(&(disk, &parts, &volumes))?;
    if retained.is_some_and(|(old, _)| old.fingerprint != fingerprint) {
        return Err("锁定期间磁盘、分区或卷信息变化。".into());
    }
    let resolved = crate::data_windows::Resolved {
        disk: disk.clone(),
        offset: 0,
        size: disk.size,
        label: format!(
            "磁盘 {} · {}",
            disk.number,
            disk.serial_number.as_deref().unwrap_or("")
        ),
        volumes: volumes
            .iter()
            .map(|(_, v)| volume_manage::volume_key(v.volume_path.as_deref().unwrap_or("")))
            .collect::<Result<Vec<_>, _>>()?,
        ntfs_volume: false,
        partition_ranges: vec![],
        data_lock_proof: None,
    };
    Ok(TableReadContext {
        resolved,
        parts: parts.into_iter().cloned().collect(),
        fingerprint,
        volumes,
    })
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TableDiskGuid {
    #[serde(rename = "__PATH")]
    path: String,
    number: u32,
    partition_style: Option<u32>,
    guid: Option<String>,
}
pub(crate) fn table_restore_context(
    source: &crate::data_tasks::Selection,
    retained: Option<(&TableReadContext, &crate::data_windows::Locks)>,
) -> Result<(TableReadContext, String, Vec<String>), String> {
    let ctx = table_read_context(source, retained)?;
    let disk = &ctx.resolved.disk;
    if disk.is_read_only != Some(false) || disk.partition_style != Some(2) {
        return Err("分区表恢复需要可写的非系统 GPT 磁盘。".into());
    }
    let rows: Vec<TableDiskGuid> =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
            .map_err(err)?
            .raw_query("SELECT __PATH,Number,PartitionStyle,Guid FROM MSFT_Disk")
            .map_err(err)?;
    let target = unique_table_guid(&rows, disk)?;
    let protected = ctx
        .parts
        .iter()
        .filter(|p| !table_partition_editable(p, disk))
        .map(|p| {
            p.guid
                .as_deref()
                .map(policy::guid)
                .ok_or("分区 GUID 缺失".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((ctx, target, protected))
}
fn unique_table_guid(rows: &[TableDiskGuid], disk: &Disk) -> Result<String, String> {
    let mut ids = std::collections::HashSet::new();
    let mut target = None;
    for row in rows.iter().filter(|r| r.partition_style == Some(2)) {
        let id = row
            .guid
            .as_deref()
            .map(policy::guid)
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .filter(|u| !u.is_nil())
            .ok_or("磁盘 GUID 清单不完整")?
            .to_string();
        if !ids.insert(id.clone()) {
            return Err("存在重复磁盘 GUID，不能恢复分区表。".into());
        }
        if row.number == disk.number {
            if row.path != disk.path || target.is_some() {
                return Err("磁盘 GUID 来源不一致。".into());
            }
            target = Some(id);
        }
    }
    target.ok_or("目标磁盘 GUID 缺失".into())
}
fn table_partition_editable(p: &Partition, disk: &Disk) -> bool {
    let linux = matches!(
        p.gpt_type.as_deref().map(policy::guid).as_deref(),
        Some(policy::LINUX_DATA | policy::LINUX_SWAP)
    );
    p.is_boot == Some(false)
        && p.is_system == Some(false)
        && (p.is_offline == Some(false)
            || disk.is_offline == Some(true) && p.is_offline == Some(true))
        && (p.is_read_only == Some(false) || linux && p.is_read_only.is_none())
        && (p.is_shadow_copy == Some(false) || linux && p.is_shadow_copy.is_none())
        && (p.is_hidden == Some(false) || linux && p.is_hidden == Some(true))
}
// Explicit sector editing only; never called by metadata inventory. Retained
// associations can be used only with independently verified native lock handles.
pub(crate) fn sector_context(
    source: &crate::data_tasks::Selection,
    retained: Option<(&SectorContext, &crate::data_windows::Locks)>,
) -> Result<SectorContext, String> {
    if let Some((old, locks)) = retained {
        crate::data_windows::verify_locks(
            locks,
            &old.resolved,
            &old.parts.iter().collect::<Vec<_>>(),
        )?;
    }
    let session = Session::open()?;
    let disk = session.disk(&source.target)?;
    let parts = policy::layout(disk, &session.partitions)?;
    let key = crate::disk_identity::layout_key(
        disk.partition_style,
        parts
            .iter()
            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
            .collect(),
    );
    if key != source.layout_key {
        return Err("磁盘布局已变化，请刷新并重新读取。".into());
    }
    let number = source
        .partition
        .ok_or("请先选择普通 GPT 数据分区；整盘扇区暂不支持写回。")?;
    let selected = *parts
        .iter()
        .find(|p| p.partition_number == number)
        .ok_or("目标分区不存在")?;
    policy::sector_writable(disk, &parts, selected)?;
    let rows: Vec<PagingVolume> = WMIConnection::new()
        .map_err(err)?
        .raw_query(
            "SELECT DeviceID,DriveLetter,BootVolume,SystemVolume,PageFilePresent FROM Win32_Volume",
        )
        .map_err(err)?;
    let mut volumes = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for p in &parts {
        let old = retained.and_then(|(ctx, _)| {
            ctx.volumes
                .iter()
                .find(|(n, _)| *n == p.partition_number)
                .map(|(_, v)| v)
        });
        let volume = volume_during_validation(session.associated_volume(p)?, old)?;
        if p.gpt_type.as_deref().map(policy::guid).as_deref() == Some(policy::BASIC_DATA) {
            let v = volume.ok_or("Windows 数据分区的卷关联缺失。")?;
            if !policy::formattable(p) {
                return Err("同盘卷的保护信息不完整，无法独占锁定。".into());
            }
            let proof = paging_proof(&v, &session.pagefiles)?;
            let matches = rows
                .iter()
                .filter(|r| {
                    volume_manage::volume_key(&r.device_id).as_deref()
                        == Ok(proof.source_key.as_str())
                })
                .collect::<Vec<_>>();
            if matches.len() != 1
                || matches[0].boot_volume == Some(true)
                || matches[0].system_volume == Some(true)
                || matches[0].page_file_present == Some(true)
                || proof.active_keys.contains(&proof.canonical_key)
                || !keys.insert(proof.source_key.clone())
            {
                return Err("卷受系统/分页文件保护，或其跨提供程序身份不唯一。".into());
            }
            if p.drive_letter.as_deref().map(policy::clean).unwrap_or("")
                != v.drive_letter.as_deref().map(policy::clean).unwrap_or("")
            {
                return Err("分区与卷盘符不一致。".into());
            }
            volumes.push((p.partition_number, v));
        } else if volume.is_some() {
            return Err("非 Windows 数据分区出现无法安全锁定的卷关联。".into());
        }
    }
    let after: Vec<PageFile> = WMIConnection::new()
        .map_err(err)?
        .raw_query("SELECT Name FROM Win32_PageFileUsage")
        .map_err(err)?;
    let names = |fs: &[PageFile]| {
        let mut names = fs
            .iter()
            .map(|p| p.name.to_ascii_uppercase())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    if names(&after) != names(&session.pagefiles) {
        return Err("核对期间分页文件清单已变化。".into());
    }
    let fingerprint = digest(&(disk, &parts, &volumes))?;
    if retained.is_some_and(|(old, _)| old.fingerprint != fingerprint) {
        return Err("锁定期间磁盘、分区或卷信息已变化。".into());
    }
    let resolved = crate::data_windows::Resolved {
        disk: disk.clone(),
        offset: selected.offset,
        size: selected.size,
        label: format!(
            "磁盘 {} / 分区 {} · {}",
            disk.number,
            number,
            disk.serial_number.as_deref().unwrap_or("")
        ),
        volumes: volumes
            .iter()
            .map(|(_, v)| volume_manage::volume_key(v.volume_path.as_deref().unwrap_or("")))
            .collect::<Result<Vec<_>, _>>()?,
        ntfs_volume: false,
        partition_ranges: vec![],
        data_lock_proof: None,
    };
    Ok(SectorContext {
        resolved,
        selected: selected.clone(),
        parts: parts.into_iter().cloned().collect(),
        fingerprint,
        volumes,
    })
}
impl Session {
    fn open() -> Result<Self, String> {
        let cim = WMIConnection::new().map_err(err)?;
        let pagefiles = cim
            .raw_query("SELECT Name FROM Win32_PageFileUsage")
            .map_err(err)?;
        let con =
            WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
        let disks: Vec<Disk> = con.raw_query(DISKS).map_err(err)?;
        let legacy: Vec<LegacyIdentity> = cim
            .raw_query("SELECT Index,SerialNumber FROM Win32_DiskDrive")
            .map_err(err)?;
        policy::inventory(&disks)?;
        let partitions: Vec<Partition> = con.raw_query(PARTITIONS).map_err(err)?;
        if partitions.len() > 32768 {
            return Err("分区数量异常，已拒绝操作。".into());
        }
        Ok(Self {
            con,
            disks,
            partitions,
            pagefiles,
            legacy,
        })
    }
    fn disk(&self, t: &Target) -> Result<&Disk, String> {
        let d = policy::target_disk(&self.disks, t)?;
        policy::corroborate(d, &self.legacy)?;
        policy::manageable(d)?;
        Ok(d)
    }
    fn revision(&self, t: &Target) -> Result<String, String> {
        let d = self.disk(t)?;
        let parts = policy::layout(d, &self.partitions)?;
        digest(&(d, parts))
    }
    fn partition(&self, t: &Target, number: u32) -> Result<&Partition, String> {
        let d = self.disk(t)?;
        self.partitions
            .iter()
            .find(|p| p.disk_number == d.number && p.partition_number == number)
            .ok_or_else(|| "分区已发生变化。".into())
    }
    fn volume(&self, p: &Partition) -> Result<Volume, String> {
        self.associated_volume(p)?
            .ok_or_else(|| "分区的卷尚未就绪，请刷新后重试。".into())
    }
    fn associated_volume(&self, p: &Partition) -> Result<Option<Volume>, String> {
        let volumes: Vec<Volume> = self
            .con
            .associators::<Volume, PartitionToVolume>(&p.path)
            .map_err(err)?;
        if volumes.is_empty() {
            return Ok(None);
        }
        if volumes.len() != 1 {
            return Err("分区与卷的关联不唯一或尚未就绪，请刷新后重试。".into());
        }
        let v = volumes.into_iter().next().unwrap();
        let reverse: Vec<Partition> = self
            .con
            .associators::<Partition, PartitionToVolume>(&v.path)
            .map_err(err)?;
        if reverse.len() != 1
            || reverse[0].disk_number != p.disk_number
            || reverse[0].partition_number != p.partition_number
            || reverse[0].offset != p.offset
            || reverse[0].size != p.size
            || reverse[0].guid != p.guid
            || !reverse[0].path.eq_ignore_ascii_case(&p.path)
        {
            return Err("卷并非唯一属于选定的分区，已拒绝操作。".into());
        }
        let letter = v.drive_letter.as_deref().map(policy::clean).unwrap_or("");
        if !letter.is_empty() && (letter.len() != 1 || !letter.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err("卷盘符不可确认。".into());
        }
        Ok(Some(v))
    }
    fn protect_paging(&self, p: &Partition, v: &Volume) -> Result<(), String> {
        let letter = v.drive_letter.as_deref().map(policy::clean).unwrap_or("");
        if letter.is_empty() {
            // No drive letter is not proof of either paging or its absence.
            // Resolve every active pagefile including junction/mounted-folder
            // paths. This is explicit-operation work, never inventory access.
            let rows: Vec<PagingVolume> = WMIConnection::new().map_err(err)?.raw_query("SELECT DeviceID,DriveLetter,BootVolume,SystemVolume,PageFilePresent FROM Win32_Volume").map_err(err)?;
            let proof = paging_proof(v, &self.pagefiles)?;
            let after: Vec<PageFile> = WMIConnection::new()
                .map_err(err)?
                .raw_query("SELECT Name FROM Win32_PageFileUsage")
                .map_err(err)?;
            let names = |files: &[PageFile]| {
                let mut names = files
                    .iter()
                    .map(|f| f.name.to_ascii_uppercase())
                    .collect::<Vec<_>>();
                names.sort();
                names
            };
            if names(&after) != names(&self.pagefiles) {
                return Err("核对期间分页文件清单已变化，请重新预览。".into());
            }
            return letterless_paging_clear(p, v, &rows, &proof);
        }
        if self.pagefiles.iter().any(|f| {
            f.name
                .to_ascii_uppercase()
                .starts_with(&format!("{}:", letter.to_ascii_uppercase()))
        }) {
            return Err("目标卷可能承载分页文件，已拒绝操作。".into());
        }
        Ok(())
    }
    fn bounds(&self, d: &Disk, p: &Partition) -> Result<(u64, u64), String> {
        let b: Bounds = self
            .con
            .exec_instance_method::<Partition, _>(&p.path, "GetSupportedSize", ())
            .map_err(err)?;
        checked(b.return_value)?;
        match b.size_min.zip(b.size_max) {
            Some((min, max))
                if min <= max && p.offset.checked_add(max).is_some_and(|end| end <= d.size) =>
            {
                Ok((min, max))
            }
            _ => Err("Windows 未提供可信的调整范围。".into()),
        }
    }
    fn bind(&self, r: &Request) -> Result<(String, Option<Volume>), String> {
        self.bind_inner(r, None)
    }
    fn bind_locked_fat(
        &self,
        r: &Request,
        retained: &Volume,
        locked: &crate::fat_volume::Locked,
    ) -> Result<(String, Option<Volume>), String> {
        let Action::Format {
            partition,
            file_system,
            ..
        } = &r.action
        else {
            return Err("锁卷核验仅适用于 FAT 格式化。".into());
        };
        if file_system.fat_type().is_none() {
            return Err("锁卷核验仅适用于 FAT 格式化。".into());
        }
        // Only a retained, successfully locked native handle can authorize
        // this narrow absence case. Validate its exact GUID/extent/protection.
        locked.verify(
            self.disk(&r.target)?,
            self.partition(&r.target, *partition)?,
        )?;
        self.bind_inner(r, Some(retained))
    }
    fn bind_inner(
        &self,
        r: &Request,
        retained: Option<&Volume>,
    ) -> Result<(String, Option<Volume>), String> {
        let d = self.disk(&r.target)?;
        policy::validate(d, &self.partitions, &r.action)?;
        let revision = self.revision(&r.target)?;
        if revision != r.revision {
            return Err("磁盘布局已发生变化，请刷新并重新预览。".into());
        }
        let volume = if let Some(number) = r.action.partition() {
            let p = self.partition(&r.target, number)?;
            let v = volume_during_validation(self.associated_volume(p)?, retained)?;
            if crate::extfs::is_action(&r.action) && v.is_some() {
                return Err("Linux 分区存在 Windows 卷关联，无法保证独占访问。".into());
            }
            if v.is_none()
                && !crate::extfs::is_action(&r.action)
                && !(matches!(r.action, Action::Delete { .. })
                    && policy::linux(p)
                    && p.drive_letter
                        .as_deref()
                        .is_none_or(|s| policy::clean(s).is_empty()))
            {
                return Err("未找到可确认的 Windows 卷，不能执行此操作。".into());
            }
            if matches!(r.action, Action::Format { .. } | Action::Delete { .. }) {
                if let Some(v) = &v {
                    self.protect_paging(p, v)?;
                }
            }
            if let Action::Format {
                file_system,
                cluster_size,
                options,
                ..
            } = r.action
            {
                if file_system.fat_type().is_none() {
                    self.format_support(
                        v.as_ref().ok_or("无法确认目标卷。")?,
                        file_system,
                        options.effective_cluster(file_system, cluster_size),
                    )?;
                }
            }
            if let Action::Resize { size, .. } = r.action {
                let v = v.as_ref().ok_or("无法确认目标卷。")?;
                if v.file_system.as_deref() != Some("NTFS") {
                    return Err("当前只调整已格式化为 NTFS 的数据分区。".into());
                }
                let (min, max) = self.bounds(d, p)?;
                if size < min || size > max {
                    return Err(format!(
                        "Windows 当前允许的范围为 {:.3}–{:.3} GiB；不可移动文件可能限制缩容。",
                        min as f64 / policy::GIB as f64,
                        max as f64 / policy::GIB as f64
                    ));
                }
            }
            v
        } else {
            None
        };
        // Exclude live usage statistics; include the exact volume binding and
        // filesystem/label so a changed target cannot reuse an old confirmation.
        let metadata = if matches!(
            r.action,
            Action::SetLabel { .. } | Action::SetDriveLetter { .. }
        ) {
            Some(volume_manage::preview(
                self,
                r,
                volume.as_ref().ok_or("无法确认目标卷")?,
            )?)
        } else {
            None
        };
        let binding = digest(&(
            revision,
            metadata,
            volume.as_ref().map(|v| {
                (
                    &v.path,
                    &v.object_id,
                    &v.file_system,
                    &v.file_system_label,
                    &v.drive_letter,
                    &v.volume_path,
                )
            }),
        ))?;
        Ok((binding, volume))
    }
    // Explicit preview/execute only: inventory never invokes these methods.
    fn format_support(&self, v: &Volume, fs: FileSystem, cluster: u32) -> Result<(), String> {
        // Query the specific filesystem. Server 2025's general format list
        // excludes exFAT, and its exFAT cluster query returns an empty list.
        // The narrow documented-format exception is in format_options.
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Input {
            file_system: &'static str,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Clusters {
            return_value: u32,
            supported_cluster_sizes: Option<Vec<u32>>,
        }
        let sizes: Clusters = self
            .con
            .exec_instance_method::<Volume, _>(
                &v.path,
                "GetSupportedClusterSizes",
                Input {
                    file_system: fs.name(),
                },
            )
            .map_err(err)?;
        checked(sizes.return_value).map_err(|e| format!("核对簇大小支持失败：{e}"))?;
        if !fs.provider_clusters_allow(sizes.supported_cluster_sizes.as_deref(), cluster) {
            return Err(format!(
                "Windows 未提供此卷使用 {} / {cluster} B 的有效簇大小选项。",
                fs.name()
            ));
        }
        Ok(())
    }
}

fn verify_cluster(v: &Volume, cluster: u32) -> Result<bool, String> {
    if cluster == 0 {
        return Ok(true);
    }
    let path = v
        .volume_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or("格式化后的卷路径缺失，无法核对簇大小。")?;
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct LegacyVolume {
        #[serde(rename = "DeviceID")]
        device_id: String,
        block_size: Option<u64>,
    }
    let con = WMIConnection::new().map_err(err)?;
    let volumes: Vec<LegacyVolume> = con
        .raw_query("SELECT DeviceID,BlockSize FROM Win32_Volume")
        .map_err(err)?;
    let matches: Vec<_> = volumes
        .iter()
        .filter(|v| v.device_id.eq_ignore_ascii_case(path))
        .collect();
    Ok(matches.len() == 1 && matches[0].block_size == Some(u64::from(cluster)))
}
fn digest(value: &impl Serialize) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(err)?)
    ))
}
fn err(e: impl std::fmt::Display) -> String {
    format!("Windows 存储接口：{e}")
}
fn checked(code: u32) -> Result<(), String> {
    if code == 0 {
        return Ok(());
    }
    let message = match code {
        1 => "系统不支持此操作",
        3 => "操作超时，实际结果需重新读取",
        5 => "参数无效",
        6 => "卷正在使用",
        40001 => "当前进程权限不足，Windows 拒绝执行此操作",
        40002 => "Windows 存储服务资源不足，操作可能只完成了一部分",
        40003 => "存储缓存已过期",
        41000 => "磁盘尚未初始化",
        41002 => "磁盘是只读的",
        41003 => "磁盘已脱机",
        42000 => "分区已删除，但访问路径未全部清理",
        42008 => "卷存在错误，不能缩容",
        42009 => "文件系统不支持调整",
        42010 => "系统或关键分区受保护",
        43000 => "簇大小无效，请选择默认值或目标卷支持的簇大小",
        43001 => "目标卷不支持所选文件系统",
        43002 => "此卷不能快速格式化",
        43003 => "簇数量超过文件系统支持范围",
        43005 => "簇大小必须是物理扇区大小的整数倍",
        43006 => "卷为只读状态，不能格式化",
        _ => "Windows 未完成此操作",
    };
    Err(format!(
        "{message}（代码 {code}）。请刷新检查实际状态；不会自动重试或回滚。"
    ))
}
fn call<C: DeserializeOwned>(
    s: &Session,
    path: &str,
    method: &'static str,
    params: impl Serialize,
) -> Result<(), String> {
    let result: MethodResult = s
        .con
        .exec_instance_method::<C, _>(path, method, params)
        .map_err(err)?;
    checked(result.return_value).map_err(|e| format!("{method}：{e}"))
}
pub fn status() -> Result<Status, String> {
    let _lock = OPERATION
        .try_lock()
        .map_err(|_| "分区操作正在执行，请稍后刷新。")?;
    let s = Session::open()?;
    // This capability read performs fixed SELECT queries only. No storage
    // methods are called merely by opening or refreshing the application.
    let available_letters = volume_manage::free_letters(&s)?;
    let targets = s
        .disks
        .iter()
        .filter_map(|d| {
            let target = policy::identity(d)?;
            let parts = policy::layout(d, &s.partitions);
            let reason = s
                .disk(&target)
                .map(|_| ())
                .and_then(|_| parts.as_ref().map(|_| ()).map_err(Clone::clone))
                .err();
            let available = reason.is_none();
            Some(TargetInfo {
                disk_id: format!("local-{}", d.number),
                name: format!("磁盘 {}", d.number),
                serial: d
                    .serial_number
                    .as_deref()
                    .map(policy::clean)
                    .unwrap_or("")
                    .into(),
                revision: if available {
                    s.revision(&target).unwrap_or_default()
                } else {
                    String::new()
                },
                layout_key: crate::disk_identity::layout_key(
                    d.partition_style,
                    s.partitions
                        .iter()
                        .filter(|p| p.disk_number == d.number)
                        .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                        .collect(),
                ),
                free_ranges: parts
                    .as_ref()
                    .map(|parts| policy::free_ranges(d, parts))
                    .unwrap_or_default(),
                format_partitions: if available {
                    parts
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|p| policy::formattable(p))
                        .map(|p| p.partition_number)
                        .collect()
                } else {
                    vec![]
                },
                delete_partitions: if available {
                    parts
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|p| policy::deletable(p))
                        .map(|p| p.partition_number)
                        .collect()
                } else {
                    vec![]
                },
                resize_partitions: if available {
                    parts
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|p| policy::resizable(p))
                        .map(|p| p.partition_number)
                        .collect()
                } else {
                    vec![]
                },
                ext_partitions: if available {
                    let ps = parts.as_ref().unwrap();
                    ps.iter()
                        .filter(|p| policy::ext_candidate(d, ps, p))
                        .map(|p| p.partition_number)
                        .collect()
                } else {
                    vec![]
                },
                metadata_partitions: if available {
                    parts
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|p| policy::formattable(p))
                        .map(|p| p.partition_number)
                        .collect()
                } else {
                    vec![]
                },
                available_drive_letters: available_letters.clone(),
                target,
                available,
                reason,
            })
        })
        .collect();
    Ok(Status {
        enabled: true,
        reason: None,
        targets,
    })
}
pub fn prepare(mut request: Request) -> Result<Plan, String> {
    let _lock = OPERATION.try_lock().map_err(|_| "另一项操作正在执行。")?;
    quick::invalidate()?;
    // Invalidate an older preview even when the new request fails validation.
    *PENDING.lock().map_err(|_| "预览状态不可用。")? = None;
    if let Action::Format {
        file_system, label, ..
    } = &mut request.action
    {
        if file_system.fat_type().is_some() {
            *label = label.to_ascii_uppercase();
        }
    }
    let s = Session::open()?;
    if let Action::SetLabel { partition, label } = &mut request.action {
        let v = s.volume(s.partition(&request.target, *partition)?)?;
        *label =
            volume_manage::normalize_label(v.file_system.as_deref().ok_or("文件系统未知")?, label)?;
    }
    let (binding, _) = s.bind(&request)?;
    let ext_proof = if crate::extfs::is_action(&request.action) {
        let d = s.disk(&request.target)?;
        Some(crate::extfs::preview(
            &request,
            d,
            &policy::layout(d, &s.partitions)?,
            s.partition(&request.target, request.action.partition().unwrap())?,
        )?)
    } else {
        None
    };
    let before = request
        .action
        .partition()
        .map(|n| s.partition(&request.target, n).map(|p| p.size))
        .transpose()?;
    let after = match request.action {
        Action::Create { size, .. }
        | Action::Resize { size, .. }
        | Action::ExtResize { size, .. } => Some(size),
        Action::Delete { .. } => Some(0),
        _ => before,
    };
    let mut warning = match request.action {
        Action::Initialize {} => "将在空白磁盘写入 GPT 分区表。",
        Action::Create { kind: PartitionKind::Linux, .. } => "创建 Linux 数据分区表项，不分配 Windows 盘符；创建后可选择格式化为 Ext2/3/4。",
        Action::Create { .. } => "创建真实数据分区，有可用的 C–Z 盘符时自动分配；盘符用完时创建无盘符分区。文件系统需另行格式化。",
        Action::Format { .. } => "",
        Action::SetLabel { .. } => "只修改所选卷的名称，保留文件及分区边界。",
        Action::SetDriveLetter { .. } => "更改或移除盘符后，依赖原路径的软件和快捷方式可能不可用；请先关闭正在使用该卷的程序。更换盘符需先移除旧盘符，若后续分配失败，卷可能暂时没有盘符；不会自动回滚。",
        Action::Resize { .. } => {
            "由 Windows 同时调整分区和 NTFS 文件系统；执行期间请勿关闭应用或计算机。"
        }
        Action::Delete { .. } => "删除所选数据分区及其卷，分区内的数据将无法正常访问。",
        Action::ExtFormat { .. } => "所选分区的现有数据将被清除，并建立所选 Ext 文件系统。会锁定整块磁盘的卷并使磁盘脱机，完成后保持脱机。执行期间请勿关闭程序或计算机。",
        Action::ExtResize { .. } => "已检查 Ext 文件系统。扩容先扩大分区再扩大文件系统；缩容先缩小文件系统再缩小分区。保留现有文件，不移动起始位置。会锁定整块磁盘并保持脱机；断电或 I/O 失败仍可能损坏数据，请先备份，执行期间请勿关闭程序。",
    }.to_string();
    if let Action::SetLabel { partition, label } = &request.action {
        let v = s.volume(s.partition(&request.target, *partition)?)?;
        warning.push_str(&format!(
            " 卷标：{:?} → {:?}（空字符串表示清除卷标）。",
            v.file_system_label.unwrap_or_default(),
            label
        ));
    }
    if let Action::SetDriveLetter { partition, letter } = &request.action {
        let p = s.partition(&request.target, *partition)?;
        let old = p
            .drive_letter
            .as_deref()
            .map(policy::clean)
            .filter(|s| !s.is_empty())
            .unwrap_or("无");
        warning.push_str(&format!(
            " 盘符：{old} → {}。",
            letter.as_ref().map(|l| l.0.as_str()).unwrap_or("无")
        ));
    }
    if let Action::Format {
        file_system,
        label,
        cluster_size,
        partition,
        options,
    } = &request.action
    {
        warning = options.warning(*file_system, *cluster_size);
        if let Some(kind) = file_system.fat_type() {
            let p = s.partition(&request.target, *partition)?;
            let d = s.disk(&request.target)?;
            let geometry = disk_core::fat_format::Plan::new(
                kind,
                p.size,
                d.logical_sector_size.ok_or("缺少扇区大小")? as u32,
                *cluster_size,
                label,
                1,
                0,
            )
            .map_err(err)?;
            warning.push_str(&format!(
                " 实际簇大小为 {} 字节；会独占锁定所选卷并核对两份完整 FAT 分配表。",
                geometry.cluster_size()
            ));
        }
    }
    let plan = Plan {
        token: uuid::Uuid::new_v4().to_string(),
        title: request.action.title().into(),
        target_name: format!(
            "磁盘 {} · {}",
            s.disk(&request.target)?.number,
            s.disk(&request.target)?
                .friendly_name
                .as_deref()
                .unwrap_or("未提供型号")
        ),
        serial: s
            .disk(&request.target)?
            .serial_number
            .as_deref()
            .map(policy::clean)
            .unwrap_or("")
            .into(),
        partition: request.action.partition(),
        before_size: before,
        after_size: after,
        warning: warning.into(),
        expires_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(err)?
            .as_millis() as u64
            + 300_000,
    };
    *PENDING.lock().map_err(|_| "预览状态不可用。")? = Some(Pending {
        request,
        binding,
        plan: plan.clone(),
        deadline: Instant::now() + Duration::from_secs(300),
        ext_proof,
    });
    Ok(plan)
}
pub fn execute(token: String) -> Result<Outcome, String> {
    let _lock = OPERATION
        .try_lock()
        .map_err(|_| "另一项分区操作正在执行。")?;
    let pending = PENDING
        .lock()
        .map_err(|_| "预览状态不可用。")?
        .take()
        .ok_or("确认已失效，请重新预览。")?;
    if token != pending.plan.token || Instant::now() > pending.deadline {
        return Err("确认无效或已超过 5 分钟，请重新预览。".into());
    }
    let r = pending.request;
    let s = Session::open()?; // Fresh identity, complete inventory and protection checks immediately before EVERY mutation.
    let (binding, volume) = s.bind(&r)?;
    if binding != pending.binding {
        return Err("目标分区或卷已改变，请重新预览。".into());
    }
    execute_bound(r, s, binding, volume, pending.ext_proof)
}

// Caller retains OPERATION for the complete single operation or batch. A batch
// may invoke this only after validating its exact preceding layout transition.
fn execute_bound(
    r: Request,
    s: Session,
    binding: String,
    volume: Option<Volume>,
    ext_proof: Option<String>,
) -> Result<Outcome, String> {
    let d = s.disk(&r.target)?;
    if matches!(
        r.action,
        Action::SetLabel { .. } | Action::SetDriveLetter { .. }
    ) {
        return volume_manage::execute(&s, &r, volume.as_ref().ok_or("无法确认目标卷")?);
    }
    if crate::extfs::is_action(&r.action) {
        return crate::extfs::execute(
            &r,
            d,
            &policy::layout(d, &s.partitions)?,
            s.partition(&r.target, r.action.partition().unwrap())?,
            ext_proof.as_deref().ok_or("缺少 Ext 文件系统预览")?,
        );
    }
    let mut format_readback = None;
    match &r.action {
        Action::Initialize {} => {
            #[derive(Serialize)]
            #[serde(rename_all = "PascalCase")]
            struct Input {
                partition_style: u16,
            }
            call::<Disk>(&s, &d.path, "Initialize", Input { partition_style: 2 })?;
        }
        Action::Create { offset, size, kind } => {
            // Windows' automatic assignment uses C–Z, not the A/B letters
            // that can still be explicitly selected in drive-letter management.
            // Decide before the single mutation; never retry a partial create.
            let assign_drive_letter =
                automatic_create_letter(*kind, &volume_manage::free_letters(&s)?);
            #[derive(Serialize)]
            #[serde(rename_all = "PascalCase")]
            struct Input {
                offset: u64,
                size: u64,
                alignment: u32,
                assign_drive_letter: bool,
                gpt_type: String,
                is_hidden: bool,
            }
            call::<Disk>(
                &s,
                &d.path,
                "CreatePartition",
                Input {
                    offset: *offset,
                    size: *size,
                    alignment: policy::MIB as u32,
                    assign_drive_letter,
                    gpt_type: format!(
                        "{{{}}}",
                        match kind {
                            PartitionKind::Windows => policy::BASIC_DATA,
                            PartitionKind::Linux => policy::LINUX_DATA,
                        }
                    ),
                    is_hidden: false,
                },
            )
            .map_err(|e| format!("{e} 新分区可能已经存在；请先刷新核对分区列表，不要重复创建。"))?;
        }
        Action::Format {
            label,
            file_system,
            cluster_size,
            partition,
            options,
        } => {
            let v = volume.as_ref().ok_or("无法确认目标卷。")?;
            if let Some(kind) = file_system.fat_type() {
                let p = s.partition(&r.target, *partition)?;
                let sector = d.logical_sector_size.ok_or("缺少扇区大小")? as u32;
                let id =
                    u32::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap());
                let geometry = disk_core::fat_format::Plan::new(
                    kind,
                    p.size,
                    sector,
                    *cluster_size,
                    label,
                    id,
                    (p.offset / u64::from(sector)).try_into().unwrap_or(0),
                )
                .map_err(err)?;
                format_readback = crate::fat_volume::format(
                    &r,
                    d,
                    &policy::layout(d, &s.partitions)?,
                    p,
                    v.volume_path.as_deref().ok_or("Windows 未提供卷路径")?,
                    &geometry,
                    |phase| {
                        let fresh = Session::open()?;
                        let bound = match phase {
                            crate::fat_volume::ValidationPhase::BeforeLock => fresh.bind(&r)?,
                            crate::fat_volume::ValidationPhase::Locked(locked) => {
                                fresh.bind_locked_fat(&r, v, locked)?
                            }
                        };
                        if bound.0 != binding {
                            return Err("卷锁定后目标身份或布局改变。".into());
                        }
                        Ok(())
                    },
                )?;
            } else {
                call::<Volume>(
                    &s,
                    &v.path,
                    "Format",
                    format_options::FormatInput::new(*file_system, label, *cluster_size, *options),
                ).map_err(|e| format!("{e} Windows 格式化请求可能已部分执行；请先检查实际状态，不要重复提交。不会自动重试或回滚。"))?;
            }
        }
        Action::Resize { partition, size } => {
            #[derive(Serialize)]
            #[serde(rename_all = "PascalCase")]
            struct Input {
                size: u64,
            }
            call::<Partition>(
                &s,
                &s.partition(&r.target, *partition)?.path,
                "Resize",
                Input { size: *size },
            )?;
        }
        Action::Delete { partition } => call::<Partition>(
            &s,
            &s.partition(&r.target, *partition)?.path,
            "DeleteObject",
            (),
        )?,
        Action::ExtFormat { .. }
        | Action::ExtResize { .. }
        | Action::SetLabel { .. }
        | Action::SetDriveLetter { .. } => unreachable!(),
    }
    // Only verification is retried, never the mutation. Missing postconditions
    // are reported as unconfirmed, not as rollback or success.
    let mut verified = false;
    let mut verification_error = None;
    for _ in 0..8 {
        if let Ok(fresh) = Session::open() {
            match verify(&fresh, &r, &s) {
                Ok(true) => {
                    verified = true;
                    break;
                }
                Ok(false) => {}
                Err(reason) => verification_error = Some(reason),
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(Outcome {
        verified,
        message: if verified {
            format!(
                "{}已完成，并已重新读取 Windows 存储信息核对。{}",
                r.action.title(),
                format_readback
                    .map(|proof| format!(
                        " 完整回读 {} 字节，SHA-256：{}。",
                        proof.bytes, proof.sha256
                    ))
                    .unwrap_or_default()
            )
        } else if matches!(
            r.action,
            Action::Create {
                kind: PartitionKind::Windows,
                ..
            }
        ) {
            "新建请求已提交，但分区及 Windows 卷关联尚未核对完整。请刷新检查，不要重复创建或继续格式化。".into()
        } else {
            format!(
                "结果尚未核对完整。请刷新检查实际状态，不要重复提交。{}",
                verification_error
                    .map(|reason| format!(" 核对原因：{reason}"))
                    .unwrap_or_default()
            )
        },
    })
}
fn verify(current: &Session, r: &Request, before: &Session) -> Result<bool, String> {
    let d = current.disk(&r.target)?;
    let parts = policy::layout(d, &current.partitions)?;
    Ok(match &r.action {
        Action::Initialize {} => d.partition_style == Some(2),
        Action::Create { offset, size, kind } => {
            created_partition_ready(&parts, *offset, *size, *kind, |p| {
                current.associated_volume(p)
            })?
        }
        Action::Delete { partition } => {
            let old = before.partition(&r.target, *partition)?;
            !parts
                .iter()
                .any(|p| p.guid == old.guid || p.offset == old.offset)
        }
        Action::Resize { partition, size } => {
            let old = before.partition(&r.target, *partition)?;
            if let Some(p) = parts.iter().find(|p| p.guid == old.guid) {
                let v = current.volume(p)?;
                p.size == *size
                    && v.file_system.as_deref() == Some("NTFS")
                    && v.size
                        .is_some_and(|n| n <= *size && *size - n <= policy::MIB)
            } else {
                false
            }
        }
        Action::Format {
            partition,
            label,
            file_system,
            cluster_size,
            options,
        } => {
            let old = before.partition(&r.target, *partition)?;
            let expected_cluster = if let Some(kind) = file_system.fat_type() {
                disk_core::fat_format::Plan::new(
                    kind,
                    old.size,
                    d.logical_sector_size.ok_or("缺少扇区大小")? as u32,
                    *cluster_size,
                    label,
                    1,
                    0,
                )
                .map_err(err)?
                .cluster_size()
            } else {
                options.effective_cluster(*file_system, *cluster_size)
            };
            if let Some(p) = parts.iter().find(|p| p.guid == old.guid) {
                let v = current.volume(p)?;
                p.offset == old.offset
                    && p.size == old.size
                    && v.file_system
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(file_system.windows_name()))
                    && v.file_system_label.as_deref() == Some(label.as_str())
                    && verify_cluster(&v, expected_cluster)?
                    && if *file_system == FileSystem::Ntfs {
                        format_windows::verify(current, r, p, &v, *options)?;
                        true
                    } else {
                        true
                    }
                    && crate::disk_identity::layout_key(
                        d.partition_style,
                        parts
                            .iter()
                            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                            .collect(),
                    ) == crate::disk_identity::layout_key(
                        d.partition_style,
                        policy::layout(before.disk(&r.target)?, &before.partitions)?
                            .iter()
                            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                            .collect(),
                    )
            } else {
                false
            }
        }
        Action::ExtFormat { .. }
        | Action::ExtResize { .. }
        | Action::SetLabel { .. }
        | Action::SetDriveLetter { .. } => false,
    })
}

fn volume_during_validation(
    fresh: Option<Volume>,
    retained: Option<&Volume>,
) -> Result<Option<Volume>, String> {
    // A conflicting/ambiguous association is never replaced by cached data.
    // The caller propagates query/reverse-association errors before this step.
    if let (Some(now), Some(old)) = (&fresh, retained) {
        if now != old {
            return Err("锁定期间卷信息发生变化。".into());
        }
    }
    Ok(fresh.or_else(|| retained.cloned()))
}

// Keep automatic drive letters separate from explicit A/B assignment.
fn automatic_create_letter(kind: PartitionKind, available: &[String]) -> bool {
    kind == PartitionKind::Windows
        && available
            .iter()
            .any(|l| l.len() == 1 && (b'C'..=b'Z').contains(&l.as_bytes()[0]))
}

// A new Windows partition can appear before the provider publishes its volume.
// Keep polling metadata in the existing bounded verification loop; never issue
// CreatePartition again. RAW volumes can legitimately report Size=0 and no FS.
fn created_partition_ready(
    parts: &[&Partition],
    offset: u64,
    size: u64,
    kind: PartitionKind,
    mut volume: impl FnMut(&Partition) -> Result<Option<Volume>, String>,
) -> Result<bool, String> {
    let matching = parts
        .iter()
        .filter(|p| {
            p.offset == offset
                && p.size == size
                && policy::deletable(p)
                && p.gpt_type.as_deref().map(policy::guid).as_deref()
                    == Some(match kind {
                        PartitionKind::Windows => policy::BASIC_DATA,
                        PartitionKind::Linux => policy::LINUX_DATA,
                    })
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Ok(false);
    }
    if kind == PartitionKind::Linux {
        return Ok(true);
    }
    let p = matching[0];
    let Some(v) = volume(p)? else {
        return Ok(false);
    };
    let Some(path) = v.volume_path.as_deref() else {
        return Ok(false);
    };
    Ok(!v.path.is_empty()
        && !v.object_id.is_empty()
        && volume_manage::volume_key(path).is_ok()
        && p.drive_letter
            .as_deref()
            .map(policy::clean)
            .unwrap_or("")
            .eq_ignore_ascii_case(v.drive_letter.as_deref().map(policy::clean).unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn letterless_paging_requires_complete_partition_flags_and_resolved_inventory() {
        let (mut p, mut v) = created_fixture();
        v.drive_letter = None;
        p.drive_letter = None;
        let key = volume_manage::volume_key(v.volume_path.as_deref().unwrap()).unwrap();
        let mut proof = PagingProof {
            source_key: key.clone(),
            canonical_key: key.clone(),
            active_keys: vec![],
        };
        let make = || PagingVolume {
            device_id: v.volume_path.clone().unwrap(),
            drive_letter: None,
            boot_volume: Some(false),
            system_volume: Some(false),
            page_file_present: Some(false),
        };
        assert!(letterless_paging_clear(&p, &v, &[make()], &proof).is_ok());
        assert!(letterless_paging_clear(&p, &v, &[], &proof).is_err());
        assert!(letterless_paging_clear(&p, &v, &[make(), make()], &proof).is_err());
        let mut raw = make();
        raw.boot_volume = None;
        raw.system_volume = None;
        raw.page_file_present = None;
        assert!(letterless_paging_clear(&p, &v, &[raw], &proof).is_ok());
        for flag in [Some(true)] {
            let mut row = make();
            row.page_file_present = flag;
            assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
            let mut row = make();
            row.boot_volume = flag;
            assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
            let mut row = make();
            row.system_volume = flag;
            assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
        }
        let mut row = make();
        row.drive_letter = Some("T:".into());
        assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
        let mut row = make();
        row.device_id = r"\\?\Volume{210ba738-0cdb-426d-a6bf-e7709f06e498}\".into();
        assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
        proof.active_keys.push(key);
        assert!(letterless_paging_clear(&p, &v, &[make()], &proof).is_err());
        proof.active_keys.clear();
        p.is_system = None;
        assert!(letterless_paging_clear(&p, &v, &[make()], &proof).is_err());
        p.is_system = Some(false);
        p.is_boot = None;
        assert!(letterless_paging_clear(&p, &v, &[make()], &proof).is_err());
    }
    #[test]
    fn paging_resolution_rejects_unknown_paths_failures_and_same_volume_aliases() {
        let (mut p, mut v) = created_fixture();
        p.drive_letter = None;
        v.drive_letter = None;
        let canonical = r"\\?\Volume{310ba738-0cdb-426d-a6bf-e7709f06e498}\";
        for bad in [
            "",
            r"pagefile.sys",
            r"C:pagefile.sys",
            r"\Device\Volume1\pagefile.sys",
            r"\\server\share\pagefile.sys",
            r"C:\folder\..\pagefile.sys",
            "C:\\page\0file.sys",
        ] {
            assert!(
                paging_proof_with(&v, &[PageFile { name: bad.into() }], |_, _| panic!(
                    "invalid paths must not be resolved"
                ))
                .is_err()
            );
        }
        assert!(paging_proof_with(
            &v,
            &[PageFile {
                name: r"C:\mount\pagefile.sys".into()
            }],
            |_, _| Err("unresolved mount".into())
        )
        .is_err());
        let proof = paging_proof_with(
            &v,
            &[PageFile {
                name: r"C:\mount\pagefile.sys".into(),
            }],
            |_, _| Ok(canonical.into()),
        )
        .unwrap();
        assert_eq!(proof.active_keys, vec![proof.canonical_key.clone()]);
        let row = PagingVolume {
            device_id: v.volume_path.clone().unwrap(),
            drive_letter: None,
            boot_volume: None,
            system_volume: None,
            page_file_present: None,
        };
        assert!(letterless_paging_clear(&p, &v, &[row], &proof).is_err());
    }
    #[test]
    fn create_without_letter_when_only_reserved_letters_remain() {
        for available in [vec![], vec!["A".into(), "B".into()]] {
            assert!(!automatic_create_letter(PartitionKind::Windows, &available));
        }
        assert!(automatic_create_letter(
            PartitionKind::Windows,
            &["Z".into()]
        ));
        assert!(!automatic_create_letter(
            PartitionKind::Linux,
            &["Z".into()]
        ));
        let (mut p, mut v) = created_fixture();
        p.drive_letter = None;
        v.drive_letter = None;
        assert!(
            created_partition_ready(&[&p], p.offset, p.size, PartitionKind::Windows, |_| Ok(
                Some(v.clone())
            ))
            .unwrap()
        );
        v.drive_letter = Some("Z".into());
        assert!(
            !created_partition_ready(&[&p], p.offset, p.size, PartitionKind::Windows, |_| Ok(
                Some(v.clone())
            ))
            .unwrap()
        );
    }
    #[test]
    fn absent_volume_needs_explicit_retained_lock_context() {
        let (_, v) = created_fixture();
        assert!(volume_during_validation(None, None).unwrap().is_none());
        assert_eq!(
            volume_during_validation(None, Some(&v)).unwrap(),
            Some(v.clone())
        );
        assert_eq!(
            volume_during_validation(Some(v.clone()), Some(&v)).unwrap(),
            Some(v.clone())
        );
        assert_eq!(
            volume_during_validation(Some(v.clone()), None).unwrap(),
            Some(v)
        );
    }
    #[test]
    fn retained_volume_never_masks_changed_identity_or_metadata() {
        let (_, v) = created_fixture();
        let mut cases = vec![];
        let mut x = v.clone();
        x.path = "another-provider-object".into();
        cases.push(x);
        let mut x = v.clone();
        x.object_id = "another-id".into();
        cases.push(x);
        let mut x = v.clone();
        x.volume_path = None;
        cases.push(x);
        let mut x = v.clone();
        x.drive_letter = Some("S".into());
        cases.push(x);
        let mut x = v.clone();
        x.file_system = Some("NTFS".into());
        cases.push(x);
        let mut x = v.clone();
        x.file_system_label = Some("changed".into());
        cases.push(x);
        let mut x = v.clone();
        x.size = Some(1024);
        cases.push(x);
        for changed in cases {
            assert!(volume_during_validation(Some(changed), Some(&v)).is_err());
        }
    }
    fn created_fixture() -> (Partition, Volume) {
        (
            Partition {
                offset: policy::MIB,
                size: 512 * policy::MIB,
                guid: Some("a10ba738-0cdb-426d-a6bf-e7709f06e498".into()),
                gpt_type: Some(policy::BASIC_DATA.into()),
                drive_letter: Some("R".into()),
                is_boot: Some(false),
                is_system: Some(false),
                is_read_only: Some(false),
                is_offline: Some(false),
                is_hidden: Some(false),
                is_shadow_copy: Some(false),
                ..Default::default()
            },
            Volume {
                path: "provider-owned-reference".into(),
                object_id: "synthetic-id".into(),
                volume_path: Some(r"\\?\Volume{110ba738-0cdb-426d-a6bf-e7709f06e498}\".into()),
                file_system: Some(String::new()),
                file_system_label: None,
                drive_letter: Some("R".into()),
                size: Some(0),
            },
        )
    }
    #[test]
    fn created_windows_partition_waits_for_volume_without_recreating() {
        let (p, v) = created_fixture();
        let mut observations = [None, None, Some(v)].into_iter();
        let mut observe = |_: &Partition| Ok(observations.next().unwrap());
        for expected in [false, false, true] {
            assert_eq!(
                created_partition_ready(
                    &[&p],
                    p.offset,
                    p.size,
                    PartitionKind::Windows,
                    &mut observe
                )
                .unwrap(),
                expected
            );
        }
    }
    #[test]
    fn created_windows_partition_rejects_incomplete_or_conflicting_volume() {
        let (p, v) = created_fixture();
        let mut wrong_letter = v.clone();
        wrong_letter.drive_letter = Some("S".into());
        let mut no_path = v.clone();
        no_path.volume_path = None;
        let mut invalid_path = v.clone();
        invalid_path.volume_path = Some(r"C:\".into());
        let mut no_id = v.clone();
        no_id.object_id.clear();
        for bad in [wrong_letter, no_path, invalid_path, no_id] {
            assert!(!created_partition_ready(
                &[&p],
                p.offset,
                p.size,
                PartitionKind::Windows,
                |_| Ok(Some(bad.clone()))
            )
            .unwrap());
        }
        assert!(
            created_partition_ready(&[&p], p.offset, p.size, PartitionKind::Windows, |_| Err(
                "ambiguous reverse association".into()
            ))
            .is_err()
        );
        assert!(!created_partition_ready(
            &[&p, &p],
            p.offset,
            p.size,
            PartitionKind::Windows,
            |_| panic!("ambiguous partition must not be resolved")
        )
        .unwrap());
    }
    #[test]
    fn created_partition_preserves_linux_and_letterless_raw_behavior() {
        let (mut p, mut v) = created_fixture();
        p.drive_letter = None;
        v.drive_letter = None;
        // Windows may run out of drive letters. A GUID-bound RAW volume is valid.
        assert!(
            created_partition_ready(&[&p], p.offset, p.size, PartitionKind::Windows, |_| Ok(
                Some(v.clone())
            ))
            .unwrap()
        );
        p.gpt_type = Some(policy::LINUX_DATA.into());
        assert!(created_partition_ready(
            &[&p],
            p.offset,
            p.size,
            PartitionKind::Linux,
            |_| panic!("Linux data does not require a Windows volume")
        )
        .unwrap());
        p.is_system = Some(true);
        assert!(!created_partition_ready(
            &[&p],
            p.offset,
            p.size,
            PartitionKind::Linux,
            |_| panic!("protected partition")
        )
        .unwrap());
    }
    #[test]
    fn confirmation_tokens_are_single_use_and_expire() {
        // No WMI calls: invalid/absent tickets are rejected before opening a session.
        assert!(execute("unissued".into()).is_err());
        let request = Request {
            target: Target("a".repeat(64)),
            revision: "synthetic".into(),
            action: Action::Initialize {},
        };
        let plan = Plan {
            token: "once".into(),
            title: "test".into(),
            target_name: "test".into(),
            serial: "test".into(),
            partition: None,
            before_size: None,
            after_size: None,
            warning: "test".into(),
            expires_at: 0,
        };
        *PENDING.lock().unwrap() = Some(Pending {
            request,
            binding: "test".into(),
            plan,
            deadline: Instant::now() - Duration::from_secs(1),
            ext_proof: None,
        });
        assert!(execute("once".into()).unwrap_err().contains("5 分钟"));
        assert!(execute("once".into()).is_err());
    }
    #[test]
    fn mutation_errors_never_report_success() {
        assert!(checked(40001).is_err());
        assert!(checked(42000).is_err());
        assert!(checked(0).is_ok());
    }
    #[test]
    fn table_restore_guid_requires_unique_provider_identity() {
        let disk = Disk {
            number: 1,
            path: "provider-disk-1".into(),
            ..Default::default()
        };
        let make = |number, path: &str, id: Option<&str>| TableDiskGuid {
            number,
            path: path.into(),
            partition_style: Some(2),
            guid: id.map(str::to_string),
        };
        let id = "11111111-1111-1111-1111-111111111111";
        assert_eq!(
            unique_table_guid(&[make(1, &disk.path, Some(id))], &disk).unwrap(),
            id
        );
        for rows in [
            vec![],
            vec![make(1, "wrong", Some(id))],
            vec![make(1, &disk.path, None)],
            vec![
                make(1, &disk.path, Some(id)),
                make(2, "provider-2", Some(id)),
            ],
            vec![make(1, &disk.path, Some(id)), make(2, "provider-2", None)],
        ] {
            assert!(unique_table_guid(&rows, &disk).is_err());
        }
    }
    #[test]
    fn table_restore_preserves_unknown_protection_and_only_allows_exact_linux_exception() {
        let online = Disk {
            is_offline: Some(false),
            ..Default::default()
        };
        let p = Partition {
            gpt_type: Some(policy::BASIC_DATA.into()),
            is_boot: Some(false),
            is_system: Some(false),
            is_offline: Some(false),
            is_hidden: Some(false),
            is_read_only: Some(false),
            is_shadow_copy: Some(false),
            ..Default::default()
        };
        assert!(table_partition_editable(&p, &online));
        for index in 0..6 {
            let mut bad = p.clone();
            let flag = match index {
                0 => &mut bad.is_boot,
                1 => &mut bad.is_system,
                2 => &mut bad.is_offline,
                3 => &mut bad.is_hidden,
                4 => &mut bad.is_read_only,
                _ => &mut bad.is_shadow_copy,
            };
            *flag = None;
            assert!(!table_partition_editable(&bad, &online));
        }
        let mut linux = p.clone();
        linux.gpt_type = Some(policy::LINUX_DATA.into());
        linux.is_hidden = Some(true);
        linux.is_read_only = None;
        linux.is_shadow_copy = None;
        assert!(table_partition_editable(&linux, &online));
        linux.is_read_only = Some(true);
        assert!(!table_partition_editable(&linux, &online));
        let mut off = p.clone();
        off.is_offline = Some(true);
        assert!(!table_partition_editable(&off, &online));
        assert!(table_partition_editable(
            &off,
            &Disk {
                is_offline: Some(true),
                ..Default::default()
            }
        ));
    }
}

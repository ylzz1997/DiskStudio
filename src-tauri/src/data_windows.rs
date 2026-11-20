//! Explicit data jobs only. Inventory never opens devices; paths are derived
//! from fresh Windows metadata, never supplied by the webview.
use crate::{
    data_io::Region,
    data_tasks::Selection,
    disk_identity,
    partitions::{policy, Target},
};
use policy::{Disk, LegacyIdentity, Partition};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::windows::{
        fs::{FileExt, MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Ioctl::*, IO::DeviceIoControl},
};
use wmi::WMIConnection;

const DISKS: &str = "SELECT __PATH,Number,SerialNumber,UniqueId,FriendlyName,Size,LogicalSectorSize,BusType,PartitionStyle,NumberOfPartitions,IsBoot,IsSystem,IsOffline,IsReadOnly,IsClustered FROM MSFT_Disk";
const PARTITIONS: &str = "SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition";
#[derive(Clone)]
pub struct Resolved {
    pub disk: Disk,
    pub offset: u64,
    pub size: u64,
    pub label: String,
    pub volumes: Vec<String>,
    pub ntfs_volume: bool,
    pub partition_ranges: Vec<crate::used_space::Span>,
    pub(crate) data_lock_proof: Option<DataLockProof>,
}
impl Resolved {
    pub(crate) fn same_data_binding(&self, other: &Self) -> bool {
        self.data_lock_proof
            .as_ref()
            .zip(other.data_lock_proof.as_ref())
            .is_some_and(|(left, right)| left.fingerprint == right.fingerprint)
    }

    pub(crate) fn verify_partition_snapshot(
        &self,
        disk: &Disk,
        parts: &[&Partition],
    ) -> Result<(), String> {
        let proof = self.data_lock_proof.as_ref().ok_or("缺少卷和分区凭据。")?;
        if lock_fingerprint(
            disk,
            parts,
            &proof.associations,
            &proof.source,
            proof.writing,
        )? != proof.fingerprint
        {
            return Err("完整分区快照或保护状态已变化。".into());
        }
        Ok(())
    }

    pub(crate) fn partition_filesystem(&self, number: u32) -> Option<&str> {
        self.partition_volume(number)?.file_system.as_deref()
    }

    pub(crate) fn has_partition_volume(&self, number: u32) -> bool {
        self.partition_volume(number).is_some()
    }

    fn partition_volume(&self, number: u32) -> Option<&Volume> {
        self.data_lock_proof
            .as_ref()?
            .associations
            .iter()
            .find(|(partition, _)| *partition == number)
            .map(|(_, volume)| volume)
    }
}
#[derive(Clone)]
pub(crate) struct DataLockProof {
    source: Selection,
    writing: bool,
    fingerprint: String,
    associations: Vec<(u32, Volume)>,
    ranges: Vec<crate::used_space::Span>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
struct Volume {
    #[serde(rename = "__PATH")]
    path: String,
    unique_id: Option<String>,
    file_system: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename = "MSFT_PartitionToVolume")]
struct Association {}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PageFile {
    name: String,
}

struct Inventory {
    disks: Vec<Disk>,
    parts: Vec<Partition>,
    legacy: Vec<LegacyIdentity>,
    pagefiles: Vec<PageFile>,
    con: WMIConnection,
}
impl Inventory {
    fn read() -> Result<Self, String> {
        let cim = WMIConnection::new().map_err(err)?;
        let legacy = cim
            .raw_query("SELECT Index,SerialNumber FROM Win32_DiskDrive")
            .map_err(err)?;
        let pagefiles = cim
            .raw_query("SELECT Name FROM Win32_PageFileUsage")
            .map_err(err)?;
        let con =
            WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
        let disks = con.raw_query(DISKS).map_err(err)?;
        let parts: Vec<Partition> = con.raw_query(PARTITIONS).map_err(err)?;
        policy::inventory(&disks)?;
        if parts.len() > 32768 {
            return Err("分区数量异常".into());
        }
        Ok(Self {
            disks,
            parts,
            legacy,
            pagefiles,
            con,
        })
    }
}
pub fn resolve(s: &Selection, stable: bool, writing: bool) -> Result<Resolved, String> {
    resolve_impl(s, stable, writing, None)
}
/// Missing WMI associations may be retained only while the exact previously
/// acquired native locks remain held. Conflicts, errors and changed metadata
/// are never replaced by retained information.
pub fn resolve_locked(
    s: &Selection,
    writing: bool,
    before: &Resolved,
    locks: &Locks,
) -> Result<Resolved, String> {
    let proof = before
        .data_lock_proof
        .as_ref()
        .ok_or("缺少锁定前的数据范围凭据。")?;
    validate_proof_selection(proof, s, writing)?;
    verify_data_locks(locks, before)?;
    let fresh = resolve_impl(s, true, writing, Some(proof))?;
    verify_data_locks(locks, &fresh)?;
    Ok(fresh)
}
fn validate_proof_selection(
    proof: &DataLockProof,
    s: &Selection,
    writing: bool,
) -> Result<(), String> {
    if proof.source.target != s.target
        || proof.source.layout_key != s.layout_key
        || proof.source.partition != s.partition
        || proof.writing != writing
    {
        return Err("卷锁凭据与本次来源、目标或范围不一致。".into());
    }
    Ok(())
}
fn retained_association(
    fresh: Vec<Volume>,
    previous: Option<&Volume>,
) -> Result<Option<Volume>, String> {
    if fresh.len() > 1 {
        return Err("分区与卷关联不唯一。".into());
    }
    let fresh = fresh.into_iter().next();
    if let (Some(now), Some(old)) = (&fresh, previous) {
        if now != old {
            return Err("锁定期间卷信息发生变化。".into());
        }
    }
    Ok(fresh.or_else(|| previous.cloned()))
}
fn lock_scope(s: &Selection, writing: bool, partition: u32) -> bool {
    writing || s.partition.is_none() || s.partition == Some(partition)
}
fn lock_fingerprint(
    d: &Disk,
    parts: &[&Partition],
    associations: &[(u32, Volume)],
    s: &Selection,
    writing: bool,
) -> Result<String, String> {
    serde_json::to_string(&(d, parts, associations, s, writing)).map_err(err)
}
fn resolve_impl(
    s: &Selection,
    stable: bool,
    writing: bool,
    retained: Option<&DataLockProof>,
) -> Result<Resolved, String> {
    let inv = Inventory::read()?;
    let d = policy::target_disk(&inv.disks, &s.target)?;
    policy::corroborate(d, &inv.legacy)?;
    if d.is_clustered != Some(false)
        || d.is_boot.is_none()
        || d.is_system.is_none()
        || d.is_offline.is_none()
        || d.size == 0
        || !matches!(d.logical_sector_size, Some(512 | 4096))
    {
        return Err("设备状态不完整或为复杂存储，已拒绝访问。".into());
    }
    let parts = policy::layout(d, &inv.parts)?;
    let layout = disk_identity::layout_key(
        d.partition_style,
        parts
            .iter()
            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
            .collect(),
    );
    if layout != s.layout_key {
        return Err("磁盘布局已变化，请刷新并重新选择。".into());
    }
    if (writing || (stable && s.partition.is_none()))
        && (d.is_boot != Some(false) || d.is_system != Some(false))
    {
        return Err(
            "当前采用锁定卷方式，不支持正在运行的系统磁盘；请在离线环境处理系统盘。".into(),
        );
    }
    if (stable || writing)
        && parts
            .iter()
            .filter(|p| writing || s.partition.is_none() || s.partition == Some(p.partition_number))
            .any(|p| p.is_boot != Some(false) || p.is_system != Some(false))
    {
        return Err("磁盘包含系统/启动分区或其保护状态未知，不能锁定或覆盖。".into());
    }
    if writing && d.is_read_only != Some(false) {
        return Err("目标磁盘只读或保护状态未知。".into());
    }
    let selected = if let Some(n) = s.partition {
        Some(
            *parts
                .iter()
                .find(|p| p.partition_number == n)
                .ok_or("所选分区已不存在")?,
        )
    } else {
        None
    };
    if writing
        && selected.is_some_and(|p| {
            p.is_boot != Some(false)
                || p.is_system != Some(false)
                || p.is_hidden != Some(false)
                || p.is_read_only != Some(false)
                || p.gpt_type.as_deref().map(policy::guid).as_deref() != Some(policy::BASIC_DATA)
        })
    {
        return Err("还原/克隆目标必须为普通 GPT 数据分区。".into());
    }
    if (stable || writing)
        && inv.pagefiles.iter().any(|f| {
            parts
                .iter()
                .filter(|p| {
                    writing || s.partition.is_none() || s.partition == Some(p.partition_number)
                })
                .any(|p| {
                    p.drive_letter.as_deref().is_some_and(|l| {
                        !l.is_empty()
                            && f.name
                                .to_uppercase()
                                .starts_with(&format!("{}:", l.to_uppercase()))
                    })
                })
        })
    {
        return Err("磁盘承载分页文件，不能锁定或覆盖。".into());
    }
    let mut volumes = vec![];
    let mut ntfs_volume = true;
    let mut associations = vec![];
    let mut ranges = vec![];
    let mut volume_keys = std::collections::HashSet::new();
    if stable || writing {
        for p in &parts {
            // All volumes of a destination disk are locked before it goes offline.
            if !lock_scope(s, writing, p.partition_number) {
                continue;
            }
            let vs: Vec<Volume> = inv
                .con
                .associators::<Volume, Association>(&p.path)
                .map_err(err)?;
            // Validate every currently returned reverse association before any
            // missing-association fallback. A query failure propagates above.
            for v in &vs {
                let reverse: Vec<Partition> = inv
                    .con
                    .associators::<Partition, Association>(&v.path)
                    .map_err(err)?;
                if reverse.len() != 1
                    || reverse[0].path != p.path
                    || reverse[0].size != p.size
                    || reverse[0].offset != p.offset
                {
                    return Err("卷与分区关联改变。".into());
                }
            }
            let previous = retained.and_then(|old| {
                old.associations
                    .iter()
                    .find(|(n, _)| *n == p.partition_number)
                    .map(|(_, v)| v)
            });
            if let Some(v) = retained_association(vs, previous)? {
                ntfs_volume &= v
                    .file_system
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case("NTFS"));
                let id = v.unique_id.as_deref().ok_or("卷标识缺失")?;
                let id = id.trim_end_matches('\\');
                if !id.starts_with("\\\\?\\Volume{")
                    || !id.ends_with('}')
                    || uuid::Uuid::parse_str(&id[11..id.len() - 1]).is_err()
                {
                    return Err("Windows 卷标识格式异常。".into());
                }
                let uuid = uuid::Uuid::parse_str(&id[11..id.len() - 1]).map_err(err)?;
                if !volume_keys.insert(uuid) {
                    return Err("多个分区返回了同一个卷标识。".into());
                }
                volumes.push(format!(r"\\?\Volume{{{uuid}}}"));
                associations.push((p.partition_number, v));
                ranges.push(crate::used_space::Span {
                    start: p.offset,
                    end: p.offset + p.size,
                });
            }
        }
    }
    let (offset, size) = selected.map(|p| (p.offset, p.size)).unwrap_or((0, d.size));
    if offset % 4096 != 0 || size % 512 != 0 {
        return Err("数据范围未满足扇区对齐要求。".into());
    }
    let data_lock_proof = if stable || writing {
        let fingerprint = lock_fingerprint(d, &parts, &associations, s, writing)?;
        if retained.is_some_and(|old| old.fingerprint != fingerprint) {
            return Err("锁定期间磁盘、完整分区布局、保护或卷关联已变化。".into());
        }
        Some(DataLockProof {
            source: s.clone(),
            writing,
            fingerprint,
            associations,
            ranges,
        })
    } else {
        None
    };
    let locale = crate::native_locale::current();
    let disk_title = locale.format("磁盘 {number}", &[("number", &d.number.to_string())]);
    let location = match s.partition {
        Some(number) => format!(
            "{disk_title} / {}",
            locale.format("分区 {partition}", &[("partition", &number.to_string())])
        ),
        None => disk_title,
    };
    Ok(Resolved {
        disk: d.clone(),
        offset,
        size,
        label: format!("{location} · {}", d.serial_number.as_deref().unwrap_or("")),
        volumes,
        ntfs_volume,
        data_lock_proof,
        partition_ranges: parts
            .iter()
            .map(|p| crate::used_space::Span {
                start: p.offset,
                end: p.offset + p.size,
            })
            .collect(),
    })
}

pub fn control(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            if input.is_empty() {
                std::ptr::null()
            } else {
                input.as_ptr().cast()
            },
            input.len() as u32,
            if output.is_empty() {
                std::ptr::null_mut()
            } else {
                output.as_mut_ptr().cast()
            },
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(format!(
            "Windows 访问失败：{}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(returned as usize)
    }
}
pub fn device(r: &Resolved, write: bool) -> Result<File, String> {
    open_device(r, write, FILE_SHARE_READ | FILE_SHARE_WRITE)
}
pub fn exclusive_device(r: &Resolved) -> Result<File, String> {
    open_device(r, true, FILE_SHARE_READ)
}
fn open_device(r: &Resolved, write: bool, sharing: u32) -> Result<File, String> {
    open_device_flags(r, write, sharing, 0)
}
fn open_device_flags(r: &Resolved, write: bool, sharing: u32, flags: u32) -> Result<File, String> {
    let f = OpenOptions::new()
        .read(true)
        .write(write)
        .share_mode(sharing)
        .custom_flags(flags)
        .open(format!("\\\\.\\PhysicalDrive{}", r.disk.number))
        .map_err(|e| format!("无法打开所选设备：{e}。程序不会自动提权。"))?;
    let mut number = [0; 12];
    if control(&f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number)? < 12
        || u32::from_le_bytes(number[4..8].try_into().unwrap()) != r.disk.number
    {
        return Err("设备句柄编号与选择不一致。".into());
    }
    let mut length = [0; 8];
    if control(&f, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut length)? < 8
        || u64::from_le_bytes(length) != r.disk.size
    {
        return Err("设备句柄容量与选择不一致。".into());
    }
    let mut geometry = [0u8; 24];
    if control(&f, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[], &mut geometry)? < 24
        || Some(u32::from_le_bytes(geometry[20..24].try_into().unwrap()) as u64)
            != r.disk.logical_sector_size
    {
        return Err("设备句柄扇区大小与选择不一致。".into());
    }
    let mut descriptor = vec![0; 65536];
    let n = control(&f, IOCTL_STORAGE_QUERY_PROPERTY, &[0; 12], &mut descriptor)?;
    if n < 36 {
        return Err("无法核对设备序列号。".into());
    }
    let start = u32::from_le_bytes(descriptor[24..28].try_into().unwrap()) as usize;
    if start < 36 || start >= n {
        return Err("设备未提供可核验的序列号。".into());
    }
    let serial = String::from_utf8_lossy(
        &descriptor[start..n]
            .split(|b| *b == 0)
            .next()
            .unwrap_or_default(),
    )
    .trim()
    .to_string();
    if Some(serial.as_str()) != r.disk.serial_number.as_deref().map(policy::clean) {
        return Err("设备句柄序列号与 Windows 元数据不一致，已拒绝 I/O。".into());
    }
    Ok(f)
}
pub fn reader(r: &Resolved) -> Result<Region<File>, String> {
    Region::new(device(r, false)?, r.offset, r.size).map_err(err)
}

pub(crate) struct StableRead {
    source: Selection,
    bound: Resolved,
    locks: Locks,
}
impl StableRead {
    pub(crate) fn lock(source: &Selection) -> Result<Self, String> {
        let bound = resolve(source, true, false)?;
        if bound.disk.is_read_only.is_none() {
            return Err("设备状态不完整或为复杂存储，已拒绝访问。".into());
        }
        let locks = state_locks(&bound)?;
        resolve_locked(source, false, &bound, &locks)?;
        Ok(Self {
            source: source.clone(),
            bound,
            locks,
        })
    }

    pub(crate) fn reader(&self) -> Result<Region<File>, String> {
        reader(&self.resolved()?)
    }

    pub(crate) fn bitlocker_reader(
        &self,
        expected_volume: &str,
    ) -> Result<Region<LockedVolumeInput>, String> {
        let r = self.resolved()?;
        let ([volume], [file], Some(partition)) =
            (r.volumes.as_slice(), self.locks.0.as_slice(), self.source.partition)
        else {
            return Err("无法唯一核对 BitLocker 分区及其卷锁。".into());
        };
        if volume != expected_volume || r.disk.is_offline != Some(false) || !r.ntfs_volume {
            return Err("BitLocker 卷关联已变化，或 Windows 未提供已解锁的 NTFS 卷。".into());
        }
        let mut number = [0u8; 12];
        let mut length = [0u8; 8];
        if control(file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number)? != number.len()
            || number[..4] != 7u32.to_le_bytes()
            || number[4..8] != r.disk.number.to_le_bytes()
            || number[8..] != partition.to_le_bytes()
            || control(file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut length)? != length.len()
            || u64::from_le_bytes(length) != r.size
        {
            return Err("BitLocker 卷的原生设备或容量与分区不一致。".into());
        }
        let mut boot = [0u8; 512];
        Region::for_metadata(device(&r, false)?, r.offset, r.size)
            .and_then(|mut raw| raw.read_exact(&mut boot))
            .map_err(err)?;
        if !crate::bitlocker::boot_marker(&boot) {
            return Err("分区缺少与 BitLocker 卷对应的加密引导标记。".into());
        }
        control(file, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut [])?;
        let input = LockedVolumeInput {
            file: file.try_clone().map_err(err)?,
            position: 0,
            size: r.size,
        };
        let mut volume = Region::new(input, 0, r.size).map_err(err)?;
        volume.read_exact(&mut boot).map_err(err)?;
        if &boot[3..11] != b"NTFS    " {
            return Err("Windows 卷句柄未提供可验证的解密 NTFS 数据。".into());
        }
        volume.seek(SeekFrom::Start(0)).map_err(err)?;
        let ntfs = ntfs::Ntfs::new(&mut volume)
            .map_err(|_| "Windows 卷句柄未提供可验证的解密 NTFS 数据。")?;
        if Some(u64::from(ntfs.sector_size())) != r.disk.logical_sector_size
            || ntfs.size() == 0
            || ntfs.size() > r.size
        {
            return Err("解密 NTFS 的扇区大小或文件系统范围与分区不一致。".into());
        }
        volume.seek(SeekFrom::Start(0)).map_err(err)?;
        Ok(volume)
    }

    pub(crate) fn resolved(&self) -> Result<Resolved, String> {
        resolve_locked(&self.source, false, &self.bound, &self.locks)
    }

    pub(crate) fn verify(&self) -> Result<(), String> {
        self.resolved().map(|_| ())
    }
}

/// Reads on duplicates of the already locked handle use absolute offsets, so
/// concurrent export workers never depend on a shared Windows file position.
pub(crate) struct LockedVolumeInput {
    file: File,
    position: u64,
    size: u64,
}
impl Read for LockedVolumeInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let len = bytes.len().min((self.size - self.position).try_into().unwrap_or(usize::MAX));
        if len == 0 {
            return Ok(0);
        }
        let count = self.file.seek_read(&mut bytes[..len], self.position)?;
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for LockedVolumeInput {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.size) + i128::from(n),
        };
        if !(0..=i128::from(self.size)).contains(&position) {
            return Err(io::Error::other("Seek outside locked volume"));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}
/// Surface reads bypass the Windows cache. Buffers are 4096-aligned; reject
/// unknown or larger physical alignment rather than silently using cached I/O.
pub fn surface_device(r: &Resolved) -> Result<File, String> {
    benchmark_device(r, false)
}
/// Raw writes must be authorized separately, locked and taken offline by caller.
pub fn benchmark_device(r: &Resolved, write: bool) -> Result<File, String> {
    let f = open_device_flags(
        r,
        write,
        if write {
            FILE_SHARE_READ
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        },
        FILE_FLAG_NO_BUFFERING | if write { FILE_FLAG_WRITE_THROUGH } else { 0 },
    )?;
    let mut query = [0u8; 12];
    query[..4].copy_from_slice(&6u32.to_le_bytes()); // StorageAccessAlignmentProperty
    let mut descriptor = [0u8; 28];
    let n = control(&f, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut descriptor)?;
    let logical = u32::from_le_bytes(descriptor[16..20].try_into().unwrap());
    let physical = u32::from_le_bytes(descriptor[20..24].try_into().unwrap());
    if n < 28
        || Some(logical as u64) != r.disk.logical_sector_size
        || !matches!(physical, 512 | 4096)
        || physical < logical
    {
        return Err("无法确认无缓存读取的设备对齐要求。".into());
    }
    Ok(f)
}
struct LockIdentity {
    disk: String,
    volumes: Vec<String>,
    offline: bool,
}
impl LockIdentity {
    fn new(r: &Resolved) -> Result<Self, String> {
        let offline = r
            .disk
            .is_offline
            .ok_or("磁盘联机状态未知，不能建立卷锁凭据。")?;
        if r.volumes
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != r.volumes.len()
        {
            return Err("卷锁目标重复。".into());
        }
        Ok(Self {
            disk: serde_json::to_string(&r.disk).map_err(err)?,
            volumes: r.volumes.clone(),
            offline,
        })
    }
    fn validate(&self, r: &Resolved, count: usize) -> Result<(), String> {
        if serde_json::to_string(&r.disk).map_err(err)? != self.disk || r.volumes != self.volumes {
            return Err("卷锁的磁盘身份、状态或卷集合已变化。".into());
        }
        let expected = if self.offline { 0 } else { self.volumes.len() };
        if count != expected {
            return Err(format!("卷锁数量不完整：持有 {count}，应有 {expected}。"));
        }
        Ok(())
    }
}
pub struct Locks(Vec<File>, LockIdentity);
impl Drop for Locks {
    fn drop(&mut self) {
        for f in &self.0 {
            let _ = control(f, FSCTL_UNLOCK_VOLUME, &[], &mut []);
        }
    }
}
pub fn lock(r: &Resolved) -> Result<Locks, String> {
    lock_with_access(r, true)
}
/// Only state changes may retain locks on a disk that is already read-only.
pub fn state_locks(r: &Resolved) -> Result<Locks, String> {
    lock_with_access(r, r.disk.is_read_only != Some(true))
}
pub fn attribute_locks(r: &Resolved) -> Result<Locks, String> {
    lock_with_access(r, false)
}
fn lock_with_access(r: &Resolved, write: bool) -> Result<Locks, String> {
    let mut locks = Locks(vec![], LockIdentity::new(r)?);
    if r.disk.is_offline == Some(true) {
        verify_offline_device(r)?;
        return Ok(locks);
    }
    for v in &r.volumes {
        let f = OpenOptions::new()
            .read(true)
            .write(write)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(v)
            .map_err(err)?;
        control(&f, FSCTL_LOCK_VOLUME, &[], &mut [])
            .map_err(|e| format!("无法独占锁定卷，请关闭占用它的程序。{e}"))?;
        locks.0.push(f);
    }
    Ok(locks)
}
pub fn verify_locks(locks: &Locks, r: &Resolved, parts: &[&Partition]) -> Result<(), String> {
    verify_lock_extents(locks, r, |offset, size| {
        parts.iter().any(|p| p.offset == offset && p.size == size)
    })
}
pub fn verify_data_locks(locks: &Locks, r: &Resolved) -> Result<(), String> {
    let proof = r
        .data_lock_proof
        .as_ref()
        .ok_or("缺少数据任务卷范围凭据。")?;
    if proof.ranges.len() != r.volumes.len() {
        return Err("卷锁范围与卷关联数量不一致。".into());
    }
    verify_lock_extents(locks, r, |offset, size| {
        // The proof is built from the selected source's associations for reads,
        // and ALL destination-disk associations for writes. It is not inferred
        // from the single partition whose content will be replaced.
        proof
            .ranges
            .iter()
            .any(|p| p.start == offset && p.end.checked_sub(p.start) == Some(size))
    })
}
fn verify_lock_extents(
    locks: &Locks,
    r: &Resolved,
    matches: impl Fn(u64, u64) -> bool,
) -> Result<(), String> {
    locks.1.validate(r, locks.0.len())?;
    if locks.1.offline {
        // A cached WMI volume association can remain on an already-offline
        // disk. No mounted-volume lock exists there; corroborate the native
        // state on an identity-checked disk handle every time instead.
        return verify_offline_device(r);
    }
    let mut seen = std::collections::HashSet::new();
    for file in &locks.0 {
        let mut b = [0u8; 32];
        let n = control(file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut b)?;
        validate_lock_extent(&b[..n.min(b.len())], r.disk.number, &matches, &mut seen)?;
    }
    Ok(())
}
fn validate_lock_extent(
    bytes: &[u8],
    disk_number: u32,
    matches: &impl Fn(u64, u64) -> bool,
    seen: &mut std::collections::HashSet<(u64, u64)>,
) -> Result<(), String> {
    let (number, offset, size) = single_extent(bytes)?;
    if number != disk_number || !matches(offset, size) || !seen.insert((offset, size)) {
        return Err("锁定卷的原生磁盘范围与选择不一致。".into());
    }
    Ok(())
}
pub(crate) fn verify_offline_device(r: &Resolved) -> Result<(), String> {
    let file = device(r, false)?;
    verify_offline_handle(r, &file)
}

pub(crate) fn verify_offline_handle(r: &Resolved, file: &File) -> Result<(), String> {
    let mut bytes = [0; 16];
    let n = control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut bytes)?;
    validate_offline_attributes(&r.disk, &bytes[..n.min(bytes.len())])
}
fn validate_offline_attributes(disk: &Disk, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 16 || u32::from_le_bytes(bytes[0..4].try_into().unwrap()) < 16 {
        return Err("原生设备状态返回不完整。".into());
    }
    let flags = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if disk.is_offline != Some(true) || flags & 1 == 0 || disk.is_read_only != Some(flags & 2 != 0)
    {
        return Err("原生设备未确认脱机，或保护状态与卷锁凭据不同。".into());
    }
    Ok(())
}
pub fn offline(file: &File) -> Result<(), String> {
    set_offline(file, false)
}

pub(crate) fn persist_offline(file: &File) -> Result<(), String> {
    set_offline(file, true)
}

pub(crate) fn confirm_writable_offline(file: &File) -> Result<(), String> {
    let mut bytes = [0; 16];
    if control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut bytes)? != 16
        || u32::from_le_bytes(bytes[..4].try_into().unwrap()) != 16
        || u64::from_le_bytes(bytes[8..].try_into().unwrap()) != 1
    {
        return Err("无法确认目标保持可写且脱机。".into());
    }
    Ok(())
}

fn set_offline(file: &File, persist: bool) -> Result<(), String> {
    let mut attributes = [0u8; 40];
    attributes[0..4].copy_from_slice(&40u32.to_le_bytes());
    attributes[4] = u8::from(persist);
    attributes[8..16].copy_from_slice(&1u64.to_le_bytes());
    attributes[16..24].copy_from_slice(&1u64.to_le_bytes());
    control(file, IOCTL_DISK_SET_DISK_ATTRIBUTES, &attributes, &mut [])?;
    let mut verify = [0; 16];
    if control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut verify)? < 16
        || u64::from_le_bytes(verify[8..16].try_into().unwrap()) & 1 == 0
    {
        return Err("无法确认目标磁盘已脱机，未执行数据写入。".into());
    }
    Ok(())
}
pub fn folder_location(path: &Path) -> Result<(PathBuf, Target), String> {
    let canonical = path.canonicalize().map_err(err)?;
    for p in canonical.ancestors() {
        if p.metadata().map_err(err)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("输出目录不能经过链接或挂载点。".into());
        }
    }
    let normal = canonical
        .to_string_lossy()
        .trim_start_matches("\\\\?\\")
        .to_string();
    let bytes = normal.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || &bytes[1..3] != b":\\"
        || normal[2..].contains(':')
    {
        return Err("请选择本机普通盘符下的目录。".into());
    }
    let inv = Inventory::read()?;
    let parts: Vec<_> = inv
        .parts
        .iter()
        .filter(|p| {
            p.drive_letter
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(&normal[..1]))
        })
        .collect();
    if parts.len() != 1 {
        return Err("不能唯一确认输出目录所在的物理磁盘。".into());
    }
    let disk = inv
        .disks
        .iter()
        .find(|d| d.number == parts[0].disk_number)
        .ok_or("输出磁盘信息缺失")?;
    policy::corroborate(disk, &inv.legacy)?;
    let target = policy::identity(disk).ok_or("不能唯一确认目录所在磁盘身份")?;
    Ok((canonical, target))
}
#[derive(Clone, PartialEq, Eq)]
pub struct FileLocation {
    pub target: Target,
    volume: String,
    layout: String,
    extent: (u32, u64, u64),
}
impl FileLocation {
    /// Compare a retained ordinary handle's observed location to an already
    /// validated partition. This grants no device access or write authority.
    pub(crate) fn confirm_partition(
        &self,
        target: &Target,
        layout: &str,
        volume: &str,
        extent: (u32, u64, u64),
    ) -> Result<(), String> {
        if &self.target != target
            || self.layout != layout
            || self.volume != volume_root(volume)?
            || self.extent != extent
        {
            return Err("打开的目录与已确认分区的卷、范围或完整布局不一致".into());
        }
        Ok(())
    }
}
pub(crate) fn volume_root(path: &str) -> Result<String, String> {
    let tail = path
        .strip_prefix("\\\\?\\Volume{")
        .ok_or("文件不在本机普通卷上")?;
    let (id, rest) = tail.split_once('}').ok_or("卷标识格式无效")?;
    if id.len() != 36 || (!rest.is_empty() && !rest.starts_with('\\')) {
        return Err("卷标识格式无效".into());
    }
    let id = uuid::Uuid::parse_str(id).map_err(|_| "卷标识格式无效")?;
    if id.is_nil() {
        return Err("卷标识缺失".into());
    }
    Ok(format!("\\\\?\\Volume{{{id}}}"))
}
fn single_extent(bytes: &[u8]) -> Result<(u32, u64, u64), String> {
    if bytes.len() != 32 || u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != 1 {
        return Err("仅支持具有唯一物理范围的普通卷".into());
    }
    let number = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let offset = i64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let size = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
    if offset < 0 || size <= 0 || offset.checked_add(size).is_none() {
        return Err("卷的物理范围无效".into());
    }
    Ok((number, offset as u64, size as u64))
}
/// Explicit file jobs only. Resolve the retained file/directory handle to its
/// volume GUID, then corroborate metadata and native extents. Never read a raw
/// device or depend on the original path/drive letter here.
fn file_volume(file: &impl AsRawHandle) -> Result<String, String> {
    let mut path = vec![0u16; 32768];
    let n = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path.as_mut_ptr(),
            path.len() as u32,
            VOLUME_NAME_GUID | FILE_NAME_OPENED,
        )
    };
    if n == 0 {
        return Err(err(std::io::Error::last_os_error()));
    }
    if n as usize >= path.len() {
        return Err("文件所在卷路径过长".into());
    }
    volume_root(&String::from_utf16(&path[..n as usize]).map_err(err)?)
}
fn volume_location(volume: &str, inv: &Inventory) -> Result<FileLocation, String> {
    let handle = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(volume)
        .map_err(err)?;
    let mut bytes = [0; 32];
    let n = control(
        &handle,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        &mut bytes,
    )?;
    let extent = single_extent(&bytes[..n.min(bytes.len())])?;
    let mut device = [0; 12];
    if control(&handle, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut device)? != 12 {
        return Err("卷设备编号缺失".into());
    }
    let disk_number = u32::from_le_bytes(device[4..8].try_into().unwrap());
    let part_number = u32::from_le_bytes(device[8..12].try_into().unwrap());
    if u32::from_le_bytes(device[..4].try_into().unwrap()) != 7 || disk_number != extent.0 {
        return Err("卷设备与物理范围不一致".into());
    }
    let disk = inv
        .disks
        .iter()
        .find(|d| d.number == disk_number)
        .ok_or("文件所在磁盘信息缺失")?;
    let target = policy::identity(disk).ok_or("文件所在磁盘身份缺失")?;
    policy::target_disk(&inv.disks, &target)?;
    policy::corroborate(disk, &inv.legacy)?;
    if disk.is_clustered != Some(false) || disk.is_offline != Some(false) {
        return Err("文件所在磁盘状态不完整或为复杂存储".into());
    }
    let parts = policy::layout(disk, &inv.parts)?;
    let part = parts
        .iter()
        .find(|p| p.partition_number == part_number && p.offset == extent.1 && p.size == extent.2)
        .ok_or("卷范围与分区元数据不一致")?;
    let volumes: Vec<Volume> = inv
        .con
        .associators::<Volume, Association>(&part.path)
        .map_err(err)?;
    if volumes.len() != 1
        || volume_root(volumes[0].unique_id.as_deref().ok_or("卷身份缺失")?)? != volume
    {
        return Err("文件卷与分区关联不唯一或已改变".into());
    }
    let reverse: Vec<Partition> = inv
        .con
        .associators::<Partition, Association>(&volumes[0].path)
        .map_err(err)?;
    if reverse.len() != 1
        || reverse[0].path != part.path
        || reverse[0].offset != extent.1
        || reverse[0].size != extent.2
    {
        return Err("文件卷反向关联不一致".into());
    }
    Ok(FileLocation {
        target,
        volume: volume.into(),
        extent,
        layout: disk_identity::layout_key(
            disk.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        ),
    })
}
pub fn file_location(file: &impl AsRawHandle) -> Result<FileLocation, String> {
    let volume = file_volume(file)?;
    let location = volume_location(&volume, &Inventory::read()?)?;
    if file_volume(file)? != volume {
        return Err("文件所在卷在核对期间改变".into());
    }
    Ok(location)
}
// The cache exists only inside one validation call. Every retained file handle
// is checked before and after the shared volume lookup; a later job starts fresh.
fn validate_locations(
    expected: &[&FileLocation],
    mut volume_of: impl FnMut(usize) -> Result<String, String>,
    mut resolve_volume: impl FnMut(&str) -> Result<FileLocation, String>,
) -> Result<(), String> {
    if expected.is_empty() || expected.len() > crate::backup_archive::MAX_CHAIN {
        return Err("备份文件卷身份数量无效".into());
    }
    let mut locations = std::collections::HashMap::new();
    for (i, expected) in expected.iter().enumerate() {
        let volume = volume_of(i)?;
        if volume != expected.volume {
            return Err("备份文件所在卷已改变，请重新打开备份".into());
        }
        if !locations.contains_key(&volume) {
            locations.insert(volume.clone(), resolve_volume(&volume)?);
        }
        if locations.get(&volume) != Some(*expected) {
            return Err("备份文件所在卷或磁盘布局已改变，请重新打开备份".into());
        }
    }
    for (i, expected) in expected.iter().enumerate() {
        if volume_of(i)? != expected.volume {
            return Err("备份文件所在卷在核对期间改变".into());
        }
    }
    Ok(())
}
#[derive(Clone)]
pub struct FileBinding {
    file: std::sync::Arc<File>,
    location: FileLocation,
}
impl FileBinding {
    pub(crate) fn target(&self) -> &Target {
        &self.location.target
    }

    pub fn new(file: &File, expected: &Target) -> Result<Self, String> {
        let location = file_location(file)?;
        if &location.target != expected {
            return Err("打开的备份文件与所选目录不在同一磁盘".into());
        }
        Ok(Self {
            file: std::sync::Arc::new(file.try_clone().map_err(err)?),
            location,
        })
    }
    pub fn validate_all(bindings: &[Self]) -> Result<(), String> {
        let expected: Vec<_> = bindings.iter().map(|b| &b.location).collect();
        let mut inventory = None;
        validate_locations(
            &expected,
            |i| file_volume(bindings[i].file.as_ref()),
            |volume| {
                if inventory.is_none() {
                    inventory = Some(Inventory::read()?);
                }
                volume_location(volume, inventory.as_ref().unwrap())
            },
        )
    }
}
#[cfg(test)]
mod file_location_tests {
    use super::*;
    #[test]
    fn batch_checks_every_handle_twice_and_resolves_each_volume_once_per_call() {
        let a = FileLocation {
            target: Target("a".into()),
            volume: "volume-a".into(),
            layout: "a-layout".into(),
            extent: (1, 4096, 8192),
        };
        let b = FileLocation {
            target: Target("b".into()),
            volume: "volume-b".into(),
            layout: "b-layout".into(),
            extent: (2, 4096, 8192),
        };
        let expected = [&a, &a, &b];
        for _ in 0..2 {
            let mut handles = vec![];
            let mut volumes = vec![];
            validate_locations(
                &expected,
                |i| {
                    handles.push(i);
                    Ok(expected[i].volume.clone())
                },
                |v| {
                    volumes.push(v.to_string());
                    Ok(if v == a.volume { a.clone() } else { b.clone() })
                },
            )
            .unwrap();
            assert_eq!(handles, [0, 1, 2, 0, 1, 2]);
            assert_eq!(volumes, ["volume-a", "volume-b"]);
        }
        let mut changed = a.clone();
        changed.layout = "new-layout".into();
        assert!(
            validate_locations(&[&a, &changed], |_| Ok(a.volume.clone()), |_| Ok(a.clone()))
                .is_err()
        );
        let mut calls = 0;
        assert!(validate_locations(
            &[&a, &a],
            |_| {
                calls += 1;
                Ok(if calls == 4 {
                    b.volume.clone()
                } else {
                    a.volume.clone()
                })
            },
            |_| Ok(a.clone())
        )
        .is_err());
        assert!(validate_locations(
            &[&a],
            |_| Ok(b.volume.clone()),
            |_| panic!("moved handle must fail before lookup")
        )
        .is_err());
        assert!(validate_locations(
            &[&a],
            |_| Ok(a.volume.clone()),
            |_| Err("unavailable metadata".into())
        )
        .is_err());
        assert!(validate_locations(&[], |_| panic!(), |_| panic!()).is_err());
        assert!(validate_locations(&vec![&a; 17], |_| panic!(), |_| panic!()).is_err());
    }
    #[test]
    fn guid_paths_reject_devices_network_paths_and_malformed_roots() {
        let root = r"\\?\Volume{c190f61a-52cd-4c3b-a095-c7133b7aeb57}";
        assert_eq!(
            volume_root(&format!("{root}\\a\\b.dsbackup")).unwrap(),
            root
        );
        for bad in [
            r"C:\a",
            r"\\server\share\a",
            r"\\.\PhysicalDrive0",
            r"\\?\Volume{not-guid}\a",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\a",
        ] {
            assert!(volume_root(bad).is_err());
        }
        assert!(volume_root(&format!("{root}suffix")).is_err());
    }
    #[test]
    fn extents_reject_multi_disk_truncated_negative_and_overflow() {
        let mut b = [0; 32];
        b[0] = 1;
        b[8] = 2;
        b[16..24].copy_from_slice(&1048576i64.to_le_bytes());
        b[24..32].copy_from_slice(&16777216i64.to_le_bytes());
        assert_eq!(single_extent(&b).unwrap(), (2, 1048576, 16777216));
        assert!(single_extent(&b[..31]).is_err());
        b[0] = 2;
        assert!(single_extent(&b).is_err());
        b[0] = 1;
        b[16..24].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(single_extent(&b).is_err());
        b[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        assert!(single_extent(&b).is_err());
        b[16..24].fill(0);
        b[24..32].fill(0);
        assert!(single_extent(&b).is_err());
    }
}
pub fn destination_folder(path: &Path, source: &Target, required: u64) -> Result<PathBuf, String> {
    destination_location(path, std::slice::from_ref(source), required).map(|(path, _)| path)
}

pub(crate) fn destination_location(
    path: &Path,
    sources: &[Target],
    required: u64,
) -> Result<(PathBuf, Target), String> {
    let (canonical, target) = folder_location(path)?;
    if sources.contains(&target) {
        return Err("输出必须位于另一块物理磁盘，不能写回恢复/备份源盘。".into());
    }
    if required == 0 {
        return Ok((canonical, target));
    }
    let mut available = 0;
    let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
        || available < required.saturating_add(16 * 1024 * 1024)
    {
        return Err("输出磁盘可用空间不足。".into());
    }
    Ok((canonical, target))
}
use std::os::windows::ffi::OsStrExt;
pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[cfg(test)]
#[path = "data_lock_tests.rs"]
mod data_lock_tests;

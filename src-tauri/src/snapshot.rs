//! Nonpersistent, autoreleased snapshots for explicitly bound NTFS volumes.
//! Volume backup uses FILE_SHARE_BACKUP. System migration coordinates only the
//! Windows Registry Writer so the copied system hives are durable at capture.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::fs::OpenOptionsExt,
    ptr,
    time::{Duration, Instant},
};
use winapi::{
    shared::guiddef::GUID,
    um::{
        combaseapi::{CoInitializeEx, CoUninitialize},
        objbase::COINIT_MULTITHREADED,
        vsbackup::{CreateVssBackupComponents, IVssBackupComponents, VssFreeSnapshotProperties},
        vss::*,
        vsserror::{VSS_S_ASYNC_FINISHED, VSS_S_ASYNC_PENDING},
    },
};
fn check(hr: i32, what: &str) -> Result<(), String> {
    if hr < 0 {
        Err(format!("{what}失败 (0x{:08X})。未请求提权。", hr as u32))
    } else {
        Ok(())
    }
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
pub struct Snapshot {
    components: *mut IVssBackupComponents,
    _com: Com,
    volumes: Vec<(File, u64)>,
    registry_pending: bool,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        self.volumes.clear();
        // This context does not set NO_AUTO_RELEASE. Releasing the requestor
        // releases only snapshots created by this instance, including errors.
        if !self.components.is_null() {
            unsafe {
                if self.registry_pending {
                    (*self.components).AbortBackup();
                }
                (*self.components).Release();
            }
        }
    }
}
struct Async(*mut IVssAsync);
impl Drop for Async {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                (*self.0).Cancel();
                (*self.0).Release();
            }
        }
    }
}

impl Async {
    unsafe fn run(
        launch: impl FnOnce(*mut *mut IVssAsync) -> i32,
        operation: &str,
        tick: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        tick()?;
        let mut task = Self(ptr::null_mut());
        check(launch(&mut task.0), operation)?;
        if task.0.is_null() {
            return Err("VSS 未返回任务".into());
        }
        let began = Instant::now();
        loop {
            tick()?;
            if began.elapsed() > Duration::from_secs(180) {
                return Err("VSS 快照超时，任务已取消".into());
            }
            check((*task.0).Wait(250), "等待快照")?;
            let mut status = 0;
            check(
                (*task.0).QueryStatus(&mut status, ptr::null_mut()),
                "查询快照状态",
            )?;
            match status {
                VSS_S_ASYNC_FINISHED => return Ok(()),
                VSS_S_ASYNC_PENDING => {}
                other => {
                    check(other, operation)?;
                    return Err("VSS 快照未完成".into());
                }
            }
        }
    }
}

// Microsoft's in-box VSS writer for the system registry hives.
const REGISTRY_WRITER: GUID = GUID {
    Data1: 0xafbab4a2,
    Data2: 0x367d,
    Data3: 0x4d15,
    Data4: [0xa5, 0x86, 0x71, 0xdb, 0xb1, 0x8f, 0x84, 0x85],
};

unsafe fn check_registry_writer(
    components: &IVssBackupComponents,
    complete: bool,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    Async::run(
        |task| components.GatherWriterStatus(task),
        "获取注册表写入器状态",
        tick,
    )?;
    let result = (|| {
        let mut count = 0;
        check(
            components.GetWriterStatusCount(&mut count),
            "读取 VSS 写入器数量",
        )?;
        if count != 1 {
            return Err("无法确认唯一的 Windows 注册表写入器。".into());
        }
        let mut instance = std::mem::zeroed();
        let mut writer = std::mem::zeroed();
        let mut name = ptr::null_mut();
        let mut state = VSS_WS_UNKNOWN;
        let mut failure = 0;
        let result = components.GetWriterStatus(
            0,
            &mut instance,
            &mut writer,
            &mut name,
            &mut state,
            &mut failure,
        );
        windows_sys::Win32::Foundation::SysFreeString(name);
        check(result, "读取 VSS 写入器状态")?;
        check(failure, "Windows 注册表写入器")?;
        if guid_key(writer) != guid_key(REGISTRY_WRITER)
            || !(state == VSS_WS_STABLE || !complete && state == VSS_WS_WAITING_FOR_BACKUP_COMPLETE)
        {
            return Err(format!("Windows 注册表写入器未就绪（状态 {state}）。"));
        }
        Ok(())
    })();
    let free = check(components.FreeWriterStatus(), "释放 VSS 写入器状态");
    result.and(free)
}
unsafe fn string(p: *const u16) -> Result<String, String> {
    if p.is_null() {
        return Err("VSS 返回空路径".into());
    }
    let mut n = 0;
    while n < 1024 && *p.add(n) != 0 {
        n += 1;
    }
    if n == 1024 {
        return Err("VSS 路径长度异常".into());
    }
    String::from_utf16(std::slice::from_raw_parts(p, n)).map_err(|e| e.to_string())
}
pub fn create(
    r: &crate::data_windows::Resolved,
    mut tick: impl FnMut() -> Result<(), String>,
) -> Result<Snapshot, String> {
    if r.volumes.len() != 1 || !r.ntfs_volume || r.offset == 0 || r.disk.is_offline != Some(false) {
        return Err("VSS 热备需要一个在线 NTFS 数据分区。".into());
    }
    tick()?;
    // Validate the actual source device, then release it before snapshotting.
    drop(crate::data_windows::device(r, false)?);
    start_set(
        &[Volume {
            root: &r.volumes[0],
            bytes: r.size,
            sector: r.disk.logical_sector_size.ok_or("快照逻辑扇区未知")?,
            original_boot: None,
        }],
        false,
        tick,
    )
}

pub(crate) struct Volume<'a> {
    pub(crate) root: &'a str,
    pub(crate) bytes: u64,
    pub(crate) sector: u64,
    pub(crate) original_boot: Option<&'a [u8]>,
}

pub(crate) fn create_migration_set(
    volumes: &[Volume<'_>],
    tick: impl FnMut() -> Result<(), String>,
) -> Result<Snapshot, String> {
    start_set(volumes, true, tick)
}

fn start_set(
    volumes: &[Volume<'_>],
    registry: bool,
    mut tick: impl FnMut() -> Result<(), String>,
) -> Result<Snapshot, String> {
    if volumes.is_empty() || volumes.len() > 64 {
        return Err("VSS 快照集的卷数量超出范围。".into());
    }
    let mut roots = std::collections::BTreeSet::new();
    for volume in volumes {
        if !roots.insert(crate::data_windows::volume_root(volume.root)?.to_ascii_lowercase())
            || !matches!(volume.sector, 512 | 4096)
            || volume.bytes < 2 * volume.sector
            || volume.bytes % volume.sector != 0
            || volume
                .original_boot
                .is_some_and(|boot| boot.len() != volume.sector as usize)
        {
            return Err("VSS 卷标识或范围无效。".into());
        }
    }
    tick()?;
    unsafe {
        check(
            CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED),
            "初始化 COM",
        )?;
        let mut snap = Snapshot {
            components: ptr::null_mut(),
            _com: Com,
            volumes: Vec::with_capacity(volumes.len()),
            registry_pending: false,
        };
        check(
            CreateVssBackupComponents(&mut snap.components),
            "创建 VSS 请求",
        )?;
        if snap.components.is_null() {
            return Err("VSS 请求为空".into());
        }
        let c = &*snap.components;
        check(c.InitializeForBackup(ptr::null_mut()), "初始化快照")?;
        let context = if registry {
            VSS_CTX_BACKUP
        } else {
            VSS_CTX_FILE_SHARE_BACKUP
        };
        check(c.SetContext(context as i32), "设置非持久快照")?;
        check(
            c.SetBackupState(false, false, VSS_BT_COPY, false),
            "设置备份状态",
        )?;
        if registry {
            // The first EnableWriterClasses enables only the supplied class.
            check(
                c.EnableWriterClasses(&REGISTRY_WRITER, 1),
                "启用注册表写入器",
            )?;
            Async::run(
                |task| c.GatherWriterMetadata(task),
                "获取注册表写入器信息",
                &mut tick,
            )?;
            let mut count = 0;
            check(c.GetWriterMetadataCount(&mut count), "核对注册表写入器数量")?;
            if count != 1 {
                return Err("无法确认唯一的 Windows 注册表写入器。".into());
            }
        }
        let mut set: GUID = std::mem::zeroed();
        check(c.StartSnapshotSet(&mut set), "开始快照集")?;
        snap.registry_pending = registry;
        let mut ids = Vec::with_capacity(volumes.len());
        for volume in volumes {
            tick()?;
            let original = format!("{}\\", crate::data_windows::volume_root(volume.root)?);
            let mut wide: Vec<u16> = original.encode_utf16().chain(Some(0)).collect();
            let mut id: GUID = std::mem::zeroed();
            check(
                c.AddToSnapshotSet(wide.as_mut_ptr(), std::mem::zeroed(), &mut id),
                "添加源卷",
            )?;
            ids.push((id, original));
        }
        if registry {
            Async::run(
                |task| c.PrepareForBackup(task),
                "准备系统注册表热迁移",
                &mut tick,
            )?;
        }
        Async::run(|task| c.DoSnapshotSet(task), "创建卷快照", &mut tick)?;
        if registry {
            check_registry_writer(c, false, &mut tick)?;
        }
        let mut paths = std::collections::BTreeSet::new();
        for ((id, original), volume) in ids.into_iter().zip(volumes) {
            tick()?;
            let mut properties: VSS_SNAPSHOT_PROP = std::mem::zeroed();
            check(c.GetSnapshotProperties(id, &mut properties), "查询快照属性")?;
            let details: Result<String, String> = (|| {
                let source = string(properties.m_pwszOriginalVolumeName)?;
                let path = string(properties.m_pwszSnapshotDeviceObject)?;
                if !source.eq_ignore_ascii_case(&original)
                    || properties.m_eStatus != VSS_SS_CREATED
                    || guid_key(properties.m_SnapshotSetId) != guid_key(set)
                    || guid_key(properties.m_SnapshotId) != guid_key(id)
                    || !valid_snapshot_path(&path)
                    || !paths.insert(path.clone())
                {
                    return Err("快照来源或设备路径不匹配".into());
                }
                Ok(path)
            })();
            VssFreeSnapshotProperties(&mut properties);
            snap.volumes
                .push((open_volume(details?, volume)?, volume.bytes));
        }
        Ok(snap)
    }
}

fn guid_key(id: GUID) -> (u32, u16, u16, [u8; 8]) {
    (id.Data1, id.Data2, id.Data3, id.Data4)
}

fn open_volume(path: String, volume: &Volume<'_>) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(
            windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
        )
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut length = [0; 8];
    let count = crate::data_windows::control(
        &file,
        windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO,
        &[],
        &mut length,
    )?;
    if count != length.len() || u64::from_le_bytes(length) != volume.bytes {
        return Err("快照容量与源分区不一致".into());
    }
    // NTFS otherwise truncates raw volume reads at its last useful cluster,
    // before the partition's final sectors. This READ-ONLY snapshot handle
    // may read the complete verified partition; Region still bounds all IO.
    crate::data_windows::control(
        &file,
        windows_sys::Win32::System::Ioctl::FSCTL_ALLOW_EXTENDED_DASD_IO,
        &[],
        &mut [],
    )
    .map_err(|e| format!("设置快照完整范围读取失败：{e}"))?;
    let sector = volume.sector;
    let mut boot = vec![0; sector as usize];
    file.read_exact(&mut boot)
        .map_err(|e| format!("读取 VSS 引导扇区失败：{e}"))?;
    if &boot[3..11] != b"NTFS    "
        || volume
            .original_boot
            .is_some_and(|original| original != boot)
    {
        return Err("VSS 快照的 NTFS 引导扇区与原卷不匹配。".into());
    }
    file.seek(SeekFrom::Start(volume.bytes - sector))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut boot)
        .map_err(|e| format!("快照末尾扇区不可完整读取：{e}"))?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    Ok(file)
}
impl Snapshot {
    pub(crate) fn complete_registry_copy(
        &mut self,
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.registry_pending {
            return Err("VSS 注册表会话已结束。".into());
        }
        unsafe {
            let components = &*self.components;
            Async::run(
                |task| components.BackupComplete(task),
                "通知注册表写入器完成",
                &mut tick,
            )?;
            self.registry_pending = false;
            check_registry_writer(components, true, &mut tick)
        }
    }

    pub fn reader(&self) -> Result<crate::data_io::Region<File>, String> {
        let [(file, size)] = self.volumes.as_slice() else {
            return Err("快照不可用".into());
        };
        crate::data_io::Region::new(file.try_clone().map_err(|e| e.to_string())?, 0, *size)
            .map_err(|e| e.to_string())
    }
    pub(crate) fn file(&self, index: usize) -> Result<&File, String> {
        self.volumes
            .get(index)
            .map(|(file, _)| file)
            .ok_or_else(|| "快照不可用".into())
    }
}
fn valid_snapshot_path(path: &str) -> bool {
    path.strip_prefix("\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy")
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.len() < 12 && suffix.bytes().all(|c| c.is_ascii_digit())
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_paths_cannot_reference_a_disk_or_traverse() {
        assert!(valid_snapshot_path(
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy42"
        ));
        assert!(!valid_snapshot_path("\\\\.\\PhysicalDrive0"));
        assert!(!valid_snapshot_path(
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy1\\..\\Harddisk0"
        ));
    }
}

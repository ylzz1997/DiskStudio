//! Nonpersistent, autoreleased VSS snapshots of one explicitly selected data
//! volume. FILE_SHARE_BACKUP is filesystem consistent, not VSS-writer aware.
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
    file: Option<File>,
    size: u64,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        self.file.take();
        // This context does not set NO_AUTO_RELEASE. Releasing the requestor
        // releases only snapshots created by this instance, including errors.
        if !self.components.is_null() {
            unsafe {
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
    unsafe {
        check(
            CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED),
            "初始化 COM",
        )?;
        let mut snap = Snapshot {
            components: ptr::null_mut(),
            _com: Com,
            file: None,
            size: r.size,
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
        check(
            c.SetContext(VSS_CTX_FILE_SHARE_BACKUP as i32),
            "设置非持久快照",
        )?;
        check(
            c.SetBackupState(false, false, VSS_BT_COPY, false),
            "设置备份状态",
        )?;
        let mut set: GUID = std::mem::zeroed();
        let mut id: GUID = std::mem::zeroed();
        check(c.StartSnapshotSet(&mut set), "开始快照集")?;
        let original = format!("{}\\", r.volumes[0]);
        let mut wide: Vec<u16> = original.encode_utf16().chain(Some(0)).collect();
        check(
            c.AddToSnapshotSet(wide.as_mut_ptr(), std::mem::zeroed(), &mut id),
            "添加源卷",
        )?;
        let mut task = Async(ptr::null_mut());
        check(c.DoSnapshotSet(&mut task.0), "创建卷快照")?;
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
            if status == VSS_S_ASYNC_FINISHED {
                break;
            }
            if status != VSS_S_ASYNC_PENDING {
                check(status, "快照")?;
                return Err("VSS 快照未完成".into());
            }
        }
        drop(task);
        let mut properties: VSS_SNAPSHOT_PROP = std::mem::zeroed();
        check(c.GetSnapshotProperties(id, &mut properties), "查询快照属性")?;
        let details: Result<String, String> = (|| {
            let source = string(properties.m_pwszOriginalVolumeName)?;
            let path = string(properties.m_pwszSnapshotDeviceObject)?;
            if !source.eq_ignore_ascii_case(&original)
                || properties.m_eStatus != VSS_SS_CREATED
                || !valid_snapshot_path(&path)
            {
                return Err("快照来源或设备路径不匹配".into());
            }
            Ok(path)
        })();
        VssFreeSnapshotProperties(&mut properties);
        let path = details?;
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            )
            .open(path)
            .map_err(|e| e.to_string())?;
        let mut length = [0; 8];
        crate::data_windows::control(
            &file,
            windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO,
            &[],
            &mut length,
        )?;
        if u64::from_le_bytes(length) != r.size {
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
        let sector = r.disk.logical_sector_size.ok_or("快照逻辑扇区未知")?;
        let mut boot = vec![0; sector as usize];
        file.read_exact(&mut boot)
            .map_err(|e| format!("读取 VSS 引导扇区失败：{e}"))?;
        if &boot[3..11] != b"NTFS    " {
            return Err("VSS 热备当前仅支持 NTFS 数据分区".into());
        }
        file.seek(SeekFrom::Start(r.size - sector))
            .map_err(|e| e.to_string())?;
        file.read_exact(&mut boot)
            .map_err(|e| format!("快照末尾扇区不可完整读取：{e}"))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        snap.file = Some(file);
        Ok(snap)
    }
}
impl Snapshot {
    pub fn reader(&self) -> Result<crate::data_io::Region<File>, String> {
        crate::data_io::Region::new(
            self.file
                .as_ref()
                .ok_or("快照不可用")?
                .try_clone()
                .map_err(|e| e.to_string())?,
            0,
            self.size,
        )
        .map_err(|e| e.to_string())
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

//! Explicit FAT formatting through the single locked Windows volume handle.
//! Never opens a writable PhysicalDrive and never offlines the whole disk.
use crate::{
    data_tasks::Selection,
    data_windows as device,
    partitions::{policy, Request},
};
use disk_core::fat_format::Plan;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
use windows_sys::Win32::{
    Foundation::ERROR_UNRECOGNIZED_VOLUME,
    Storage::FileSystem::{
        GetVolumeInformationByHandleW, FILE_SHARE_READ, FILE_SHARE_WRITE,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, IOCTL_VOLUME_OFFLINE,
    },
    System::Ioctl::*,
};

fn volume_path(path: &str) -> Result<&str, String> {
    let path = path.trim_end_matches('\\');
    if path.len() != 48
        || !path.starts_with("\\\\?\\Volume{")
        || !path.ends_with('}')
        || uuid::Uuid::parse_str(&path[11..47]).is_err()
    {
        return Err("Windows 卷标识无效。".into());
    }
    Ok(path)
}
pub(crate) fn verify_extent(
    file: &File,
    d: &policy::Disk,
    p: &policy::Partition,
) -> Result<(), String> {
    let mut b = [0u8; 32];
    let n = device::control(file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut b)?;
    check_extent(&b[..n.min(b.len())], d, p)?;
    let mut info = vec![0u8; std::mem::size_of::<PARTITION_INFORMATION_EX>()];
    if device::control(file, IOCTL_DISK_GET_PARTITION_INFO_EX, &[], &mut info)? < info.len() {
        return Err("缺少原生分区信息。".into());
    }
    let native = unsafe {
        info.as_ptr()
            .cast::<PARTITION_INFORMATION_EX>()
            .read_unaligned()
    };
    check_partition(native, p)
}
fn check_extent(b: &[u8], d: &policy::Disk, p: &policy::Partition) -> Result<(), String> {
    if b.len() < 32
        || u32::from_le_bytes(b[..4].try_into().unwrap()) != 1
        || u32::from_le_bytes(b[8..12].try_into().unwrap()) != d.number
        || u64::from_le_bytes(b[16..24].try_into().unwrap()) != p.offset
        || u64::from_le_bytes(b[24..32].try_into().unwrap()) != p.size
    {
        return Err("锁定卷的原生范围与所选分区不一致。".into());
    }
    Ok(())
}
fn check_partition(native: PARTITION_INFORMATION_EX, p: &policy::Partition) -> Result<(), String> {
    if native.PartitionStyle != PARTITION_STYLE_GPT
        || native.PartitionNumber != p.partition_number
        || native.StartingOffset != p.offset as i64
        || native.PartitionLength != p.size as i64
        || native.IsServicePartition
    {
        return Err("原生分区身份或保护状态不匹配。".into());
    }
    let g = unsafe { native.Anonymous.Gpt };
    let guid = |g: windows_sys::core::GUID| {
        uuid::Uuid::from_fields(g.data1, g.data2, g.data3, &g.data4).to_string()
    };
    if p.guid.as_deref().map(policy::guid).as_deref() != Some(guid(g.PartitionId).as_str())
        || guid(g.PartitionType) != policy::BASIC_DATA
        || g.Attributes & !0x8000000000000000 != 0
    {
        return Err("GPT 分区标识、类型或保护属性不允许格式化。".into());
    }
    Ok(())
}
// Constructed only after FSCTL_LOCK_VOLUME succeeds; the field stays private.
pub(crate) struct Locked(File);
impl Locked {
    pub(crate) fn verify(&self, d: &policy::Disk, p: &policy::Partition) -> Result<(), String> {
        verify_extent(&self.0, d, p)
    }
}
pub(crate) enum ValidationPhase<'a> {
    BeforeLock,
    Locked(&'a Locked),
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = device::control(&self.0, FSCTL_UNLOCK_VOLUME, &[], &mut []);
    }
}
// RAW's filesystem driver rejects ALLOW_EXTENDED_DASD_IO with error 87.
// Identify that state through the retained native handle, never by ignoring an
// arbitrary failed control or inferring a filesystem from the GPT type.
fn needs_extended_io(result: io::Result<String>) -> Result<bool, String> {
    match result {
        Ok(name) if name.eq_ignore_ascii_case("RAW") => Ok(false),
        Ok(name) if !name.is_empty() => Ok(true),
        Ok(_) => Err("锁定卷未返回文件系统信息。".into()),
        Err(e) if e.raw_os_error() == Some(ERROR_UNRECOGNIZED_VOLUME as i32) => Ok(false),
        Err(e) => Err(format!("无法确认锁定卷的文件系统：{e}")),
    }
}
fn filesystem(file: &File) -> io::Result<String> {
    let mut name = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = name
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "文件系统名称未终止"))?;
    String::from_utf16(&name[..len]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
struct VolumeIo<'a> {
    file: &'a mut File,
    size: u64,
    sector: u64,
    pos: u64,
}
impl Seek for VolumeIo<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.size as i128 + n as i128,
        };
        if next < 0 || next > self.size as i128 {
            return Err(io::Error::other("越出已确认的 FAT 卷范围"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}
impl VolumeIo<'_> {
    fn check(&mut self, n: usize) -> io::Result<()> {
        if self.pos % self.sector != 0
            || n as u64 % self.sector != 0
            || n as u64 > self.size - self.pos
        {
            return Err(io::Error::other("FAT I/O 未对齐或越界"));
        }
        self.file.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }
}
impl Read for VolumeIo<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.check(b.len())?;
        self.file.read_exact(b)?;
        self.pos += b.len() as u64;
        Ok(b.len())
    }
}
impl Write for VolumeIo<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.check(b.len())?;
        self.file.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}
pub fn format(
    request: &Request,
    d: &policy::Disk,
    parts: &[&policy::Partition],
    p: &policy::Partition,
    path: &str,
    plan: &Plan,
    mut revalidate: impl FnMut(ValidationPhase<'_>) -> Result<(), String>,
) -> Result<Option<disk_core::fat_format::FullFormatVerification>, String> {
    let full = match &request.action {
        crate::partitions::Action::Format {
            file_system,
            options,
            ..
        } if file_system.fat_type().is_some() => {
            options.mode == crate::partitions::format_options::FormatMode::Full
        }
        _ => return Err("所选操作不是 FAT 格式化。".into()),
    };
    let path = volume_path(path)?;
    let selection = Selection {
        target: request.target.clone(),
        partition: Some(p.partition_number),
        layout_key: crate::disk_identity::layout_key(
            d.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        ),
    };
    let resolved = device::resolve(&selection, true, false)?;
    // Read-only physical handle verifies serial/capacity/sector cross-provider;
    // actual writes are possible only through the locked selected volume.
    let _identity = device::device(&resolved, false)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .map_err(device::err)?;
    verify_extent(&file, d, p)?;
    revalidate(ValidationPhase::BeforeLock)?;
    device::control(&file, FSCTL_LOCK_VOLUME, &[], &mut [])
        .map_err(|e| format!("无法独占锁定所选卷，请关闭占用程序。{e}"))?;
    let mut locked = Locked(file);
    verify_extent(&locked.0, d, p)?;
    revalidate(ValidationPhase::Locked(&locked))?;
    verify_extent(&locked.0, d, p)?;
    if plan.size() != p.size
        || u64::from(plan.sector_size()) != d.logical_sector_size.ok_or("缺少扇区大小")?
    {
        return Err("格式化几何信息改变。".into());
    }
    if needs_extended_io(filesystem(&locked.0))? {
        device::control(&locked.0, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut [])
            .map_err(|e| format!("无法启用所选卷的扩展 I/O：{e}"))?;
    }
    // Windows' documented replacement sequence is lock -> format -> dismount
    // -> unlock/close. Dismounting before format leaves the retained RAW handle
    // unable to accept a second dismount (error 87). The successful lock itself
    // flushes the volume and excludes other access for write and readback.
    let mut io = VolumeIo {
        file: &mut locked.0,
        size: p.size,
        sector: u64::from(plan.sector_size()),
        pos: 0,
    };
    let formatted = if full {
        plan.format_full(&mut io, |_, _| Ok(())).map(Some)
    } else {
        plan.format(&mut io, |_, _| Ok(())).map(|_| None)
    }
    .map_err(|e| format!("FAT 写入或读回校验失败：{e}"));
    // Attempt dismount exactly once, including after a partial write. Never
    // repeat a failed dismount; independently request offline on any failure.
    let dismounted = device::control(&locked.0, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
        .map(|_| ())
        .map_err(|e| format!("FAT 写入后卸载卷失败：{e}"));
    let result = formatted.and_then(|proof| dismounted.map(|_| proof));
    if let Err(e) = &result {
        let offline = device::control(&locked.0, IOCTL_VOLUME_OFFLINE, &[], &mut []);
        return Err(format!(
            "{e} 格式化可能部分完成，不会自动重试或回滚。{}",
            if offline.is_ok() {
                "目标卷已请求脱机，请检查后手动处理。"
            } else {
                "无法将目标卷脱机，请立即检查状态。"
            }
        ));
    }
    result // Unlock/close allows Windows to recognize the new filesystem.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_native_unrecognized_volume_can_omit_extended_io() {
        assert!(!needs_extended_io(Err(io::Error::from_raw_os_error(1005))).unwrap());
        for name in ["RAW", "raw"] {
            assert!(!needs_extended_io(Ok(name.into())).unwrap());
        }
        for name in ["NTFS", "FAT", "FAT32", "exFAT", "ReFS", "OTHER", " RAW"] {
            assert!(needs_extended_io(Ok(name.into())).unwrap());
        }
        assert!(needs_extended_io(Ok(String::new())).is_err());
        for code in [1, 5, 6, 21, 32, 87, 1117] {
            assert!(needs_extended_io(Err(io::Error::from_raw_os_error(code))).is_err());
        }
    }
    #[test]
    fn native_volume_identity_and_protection_cannot_be_substituted() {
        let d = policy::Disk {
            number: 7,
            ..Default::default()
        };
        let p = policy::Partition {
            partition_number: 3,
            offset: 1024 * 1024,
            size: 128 * 1024 * 1024,
            guid: Some("110ba738-0cdb-426d-a6bf-e7709f06e498".into()),
            ..Default::default()
        };
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&1u32.to_le_bytes());
        b[8..12].copy_from_slice(&d.number.to_le_bytes());
        b[16..24].copy_from_slice(&p.offset.to_le_bytes());
        b[24..32].copy_from_slice(&p.size.to_le_bytes());
        assert!(check_extent(&b, &d, &p).is_ok());
        assert!(check_extent(&b[..31], &d, &p).is_err());
        for at in [0, 8, 16, 24] {
            let mut bad = b;
            bad[at] ^= 1;
            assert!(check_extent(&bad, &d, &p).is_err());
        }
        let guid = |s: &str| {
            let id = uuid::Uuid::parse_str(s).unwrap();
            let (a, b, c, e) = id.as_fields();
            windows_sys::core::GUID {
                data1: a,
                data2: b,
                data3: c,
                data4: *e,
            }
        };
        let mut n = PARTITION_INFORMATION_EX {
            PartitionStyle: PARTITION_STYLE_GPT,
            PartitionNumber: p.partition_number,
            StartingOffset: p.offset as i64,
            PartitionLength: p.size as i64,
            ..Default::default()
        };
        n.Anonymous.Gpt = PARTITION_INFORMATION_GPT {
            PartitionType: guid(policy::BASIC_DATA),
            PartitionId: guid(p.guid.as_deref().unwrap()),
            ..Default::default()
        };
        assert!(check_partition(n, &p).is_ok());
        for attribute in [
            1,
            2,
            0x1000000000000000,
            0x2000000000000000,
            0x4000000000000000,
        ] {
            let mut bad = n;
            let mut g = unsafe { bad.Anonymous.Gpt };
            g.Attributes = attribute;
            bad.Anonymous.Gpt = g;
            assert!(check_partition(bad, &p).is_err());
        }
        n.IsServicePartition = true;
        assert!(check_partition(n, &p).is_err());
        n.IsServicePartition = false;
        n.PartitionNumber += 1;
        assert!(check_partition(n, &p).is_err());
    }
    #[test]
    fn only_canonical_provider_volume_ids_are_accepted() {
        assert!(volume_path(r"\\?\Volume{110ba738-0cdb-426d-a6bf-e7709f06e498}\").is_ok());
        for value in [
            r"\\.\PhysicalDrive1",
            r"C:\",
            r"\\?\Volume{invalid}",
            r"\\?\Volume{110ba738-0cdb-426d-a6bf-e7709f06e498}\file",
        ] {
            assert!(volume_path(value).is_err());
        }
    }
}

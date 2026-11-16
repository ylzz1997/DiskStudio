//! Disposable guest fixture creation only. No scan, format, mount or partition edit.
use super::*;
pub fn create() -> Result<String, String> {
    use cap_std::fs::{Dir, OpenOptions as CapOptions};
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x00560000;
    inventory()?;
    let (number, root) = part(B, 6 * GIB)?;
    if number != 2 {
        return Err("STOP: expected existing B partition 2".into());
    }
    let source = selection(B, Some(number))?;
    let resolved = data_windows::resolve(&source, false, true)?;
    if resolved.disk.number != 2
        || resolved.size != 6 * GIB
        || resolved.disk.is_offline != Some(false)
    {
        return Err("STOP: unexpected B fixture partition".into());
    }
    // Production native device-number/capacity/sector/serial verification; read handle only.
    let _device = data_windows::device(&resolved, false)?;
    let (canonical, target) = data_windows::folder_location(&root)?;
    if target != source.target {
        return Err("STOP: drive root changed disks".into());
    }
    let letter = root.to_str().ok_or("Invalid derived root")?;
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(format!("\\\\.\\{}", &letter[..2]))
        .map_err(err)?;
    let mut extents = [0u8; 32];
    let len = data_windows::control(
        &volume,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        &mut extents,
    )?;
    if len < 32
        || u32::from_le_bytes(extents[..4].try_into().unwrap()) != 1
        || u32::from_le_bytes(extents[8..12].try_into().unwrap()) != resolved.disk.number
        || u64::from_le_bytes(extents[16..24].try_into().unwrap()) != resolved.offset
        || u64::from_le_bytes(extents[24..32].try_into().unwrap()) != resolved.size
    {
        return Err("STOP: native volume extent mismatch".into());
    }
    let root = Dir::open_ambient_dir(canonical, cap_std::ambient_authority()).map_err(err)?;
    let name = "DiskStudio-NTFS-resume-fixtures";
    root.create_dir(name).map_err(err)?; // Never reuse/overwrite an earlier run.
    let dir = root.open_dir(name).map_err(err)?;
    dir.create_dir("empty-folder").map_err(err)?;
    for group in 0..100 {
        let sub = format!("G{group:03}");
        dir.create_dir(&sub).map_err(err)?;
        let child = dir.open_dir(&sub).map_err(err)?;
        for item in 0..600 {
            let filename = format!("R{item:04}.TXT");
            let content = format!("DiskStudio NTFS resume fixture {group:03}/{item:04}\r\n");
            let mut opts = CapOptions::new();
            opts.read(true).write(true).create_new(true);
            let mut f = child.open_with(&filename, &opts).map_err(err)?;
            f.write_all(content.as_bytes()).map_err(err)?;
        }
    }
    // Verify each ordinary file. These are retained-dir accesses on B only.
    for group in 0..100 {
        let child = dir.open_dir(format!("G{group:03}")).map_err(err)?;
        for item in 0..600 {
            let expected = format!("DiskStudio NTFS resume fixture {group:03}/{item:04}\r\n");
            let mut actual = String::new();
            child
                .open(format!("R{item:04}.TXT"))
                .map_err(err)?
                .read_to_string(&mut actual)
                .map_err(err)?;
            if actual != expected {
                return Err("Fixture content verification failed".into());
            }
        }
    }
    inventory()?;
    data_windows::resolve(&source, false, true)?;
    Ok("PASS: 60,000 unique text fixtures and 101 directories created and read back on DS-TEST-B-0001 / partition 2 (6 GiB NTFS).\n\nNo existing file was overwritten. No partition, mount, online/offline or raw write operation was performed.\n\nClose this dialog and test pause/save/restart/resume in the unchanged production DiskStudio.exe.".into())
}

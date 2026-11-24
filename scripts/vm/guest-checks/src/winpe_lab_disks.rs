//! Fixed identity and volume guards for the WinPE A/B lab.
use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::process::Command;
use windows_sys::Win32::Storage::FileSystem::{
    GetLogicalDrives, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DISK_ATTRIBUTES, IOCTL_DISK_GET_DRIVE_GEOMETRY,
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use wmi::WMIConnection;

const UUID: &str = "e20adb9f-f665-432e-9a62-dd5d897b06a0";
const DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Deserialize)]
struct Computer {
    #[serde(rename = "UUID")]
    uuid: String,
    #[serde(rename = "Vendor")]
    vendor: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "IdentifyingNumber")]
    serial: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Disk {
    pub(crate) index: u32,
    pub(crate) serial_number: String,
}

fn device(path: &str) -> io::Result<File> {
    OpenOptions::new().read(true).share_mode(3).open(path)
}

fn control<const N: usize>(file: &File, code: u32, input: &[u8]) -> io::Result<([u8; N], usize)> {
    let mut data = [0; N];
    let mut size = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            data.as_mut_ptr().cast(),
            N as u32,
            &mut size,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("Device metadata query {code:#010x}: {error}"),
        ));
    }
    Ok((data, size as usize))
}

fn field(data: &[u8], start: usize) -> u32 {
    u32::from_le_bytes(data[start..start + 4].try_into().unwrap())
}

fn wide_field(data: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(data[start..start + 8].try_into().unwrap())
}

pub(crate) fn disk_inventory() -> Result<Vec<Disk>, Box<dyn std::error::Error>> {
    if !std::env::current_exe()?.starts_with(r"X:\")
        || !std::env::var("SystemRoot")?.eq_ignore_ascii_case(r"X:\Windows")
    {
        return Err("The WinPE test runner must be loaded from the guest RAM disk".into());
    }
    let wmi = WMIConnection::new()?;
    let computers: Vec<Computer> = wmi
        .raw_query("SELECT UUID,Vendor,Name,IdentifyingNumber FROM Win32_ComputerSystemProduct")?;
    if !matches!(computers.as_slice(), [vm] if vm.uuid.eq_ignore_ascii_case(UUID)
        && vm.vendor == "DiskStudio" && vm.name == "DiskStudio-Test" && vm.serial == "DS-VM-0001")
    {
        return Err("Unexpected WinPE guest identity".into());
    }
    Ok(wmi.raw_query("SELECT Index,SerialNumber FROM Win32_DiskDrive")?)
}

pub(crate) fn verify_disk_geometry(
    index: u32,
    controller_serial: &str,
    namespace_serial: &str,
    sector: u32,
) -> Result<File, Box<dyn std::error::Error>> {
    let file = device(&format!(r"\\.\PhysicalDrive{index}"))?;
    let (number, n) = control::<12>(&file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[])?;
    let (length, l) = control::<8>(&file, IOCTL_DISK_GET_LENGTH_INFO, &[])?;
    let (geometry, g) = control::<24>(&file, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[])?;
    let (serial, s) = control::<4096>(&file, IOCTL_STORAGE_QUERY_PROPERTY, &[0; 12])?;
    let start = field(&serial, 24) as usize;
    let descriptor = serial.get(start..s).and_then(|bytes| {
        let end = bytes.iter().position(|&byte| byte == 0)?;
        std::str::from_utf8(&bytes[..end]).ok()
    });
    if n != 12
        || field(&number, 4) != index
        || l != 8
        || wide_field(&length, 0) != DISK_BYTES
        || g != 24
        || field(&geometry, 20) != sector
    {
        return Err(format!(
            "Unexpected test disk geometry: number field {n}, length field {l}, geometry field {g}"
        )
        .into());
    }
    if sector == 4096 {
        let mut query = [0; 12];
        query[..4].copy_from_slice(&6u32.to_le_bytes()); // StorageAccessAlignmentProperty
        let (alignment, size) = control::<28>(&file, IOCTL_STORAGE_QUERY_PROPERTY, &query)?;
        if size != 28 || field(&alignment, 16) != sector || field(&alignment, 20) != sector {
            return Err("Expected 4096-byte logical and physical test disk alignment".into());
        }
    }
    let descriptor_matches = descriptor
        .map(str::trim)
        .is_some_and(|value| value == controller_serial || value == namespace_serial);
    if start < 36 || field(&serial, 28) != 17 || !descriptor_matches {
        return Err(format!(
            "Unexpected test disk native identity: bus {}, serial match {}",
            field(&serial, 28),
            descriptor_matches
        )
        .into());
    }
    Ok(file)
}

pub(crate) fn verify_blank_disk(
    index: u32,
    controller_serial: &str,
    namespace_serial: &str,
    sector: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = verify_disk_geometry(index, controller_serial, namespace_serial, sector)?;
    let (layout, p) = control::<4096>(&file, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[])?;
    let style = field(&layout, 0);
    let count = field(&layout, 4) as usize;
    // Windows may expose a zeroed disk as MBR with four empty slots.
    let first_entry = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    let empty_mbr = style == 0
        && count == 4
        && p >= first_entry + count * entry_size
        && layout[first_entry..first_entry + count * entry_size]
            .chunks_exact(entry_size)
            .all(|entry| {
                wide_field(entry, offset_of!(PARTITION_INFORMATION_EX, StartingOffset)) == 0
                    && wide_field(entry, offset_of!(PARTITION_INFORMATION_EX, PartitionLength)) == 0
            });
    if p < 8 || !((matches!(style, 0 | 2) && count == 0) || empty_mbr) {
        return Err(format!(
            "Unexpected test disk layout: style {style}, entries {count}, response bytes {p}"
        )
        .into());
    }
    let mut boundary = vec![0u8; 1024 * 1024];
    for offset in [0, DISK_BYTES - boundary.len() as u64] {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut boundary)?;
        if boundary.iter().any(|&byte| byte != 0) {
            return Err("The test disk has nonzero bytes at a partition table boundary".into());
        }
    }
    Ok(())
}

pub(crate) fn verify_offline(file: &File) -> Result<(), Box<dyn std::error::Error>> {
    let (attributes, size) = control::<16>(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[])?;
    if size != 16 || wide_field(&attributes, 8) & 1 == 0 {
        return Err("The verified source disk is not offline".into());
    }
    Ok(())
}

pub(crate) fn check_drive_letter(letter: char) -> Result<(), Box<dyn std::error::Error>> {
    let drives = unsafe { GetLogicalDrives() };
    if !letter.is_ascii_uppercase() || drives == 0 || drives & (1 << (letter as u8 - b'A')) != 0 {
        return Err(format!("Test drive letter {letter}: must be unused").into());
    }
    Ok(())
}

pub(crate) fn prepare_volume(
    index: u32,
    letter: char,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    check_drive_letter(letter)?;
    let script = r"X:\ds-partition.txt";
    fs::write(script, format!("select disk {index}\r\nconvert gpt\r\ncreate partition primary\r\nformat fs=ntfs quick label={label}\r\nassign letter={letter}\r\n"))?;
    let status = Command::new(r"X:\Windows\System32\diskpart.exe")
        .args(["/s", script])
        .status()?;
    if !status.success() {
        return Err(format!("DiskPart stopped with {status}").into());
    }
    verify_volume(index, letter)
}

pub(crate) fn verify_volume(index: u32, letter: char) -> Result<(), Box<dyn std::error::Error>> {
    let volume = device(&format!(r"\\.\{letter}:"))?;
    let (extent, size) = control::<32>(&volume, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[])?;
    let (start, length) = (wide_field(&extent, 16), wide_field(&extent, 24));
    if size != 32
        || field(&extent, 0) != 1
        || field(&extent, 8) != index
        || start < 1024 * 1024
        || length < 15 * 1024 * 1024 * 1024
        || start
            .checked_add(length)
            .is_none_or(|end| end >= DISK_BYTES)
    {
        return Err(
            format!("The {letter}: volume does not match the verified test disk extent").into(),
        );
    }
    Ok(())
}

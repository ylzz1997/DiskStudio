//! Isolated VM fault fixture. NEVER execute on the host. Not part of the app.
//! Only corrupts one CRC bit in B's backup GPT; cannot repair or bring disks online.
#![windows_subsystem = "windows"]
use disk_core::{
    table_check::{self, CheckState, TableReport},
    MIB,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Ioctl::*, IO::DeviceIoControl},
    UI::WindowsAndMessaging::*,
};
use wmi::WMIConnection;
const UUID: &str = "e20adb9f-f665-432e-9a62-dd5d897b06a0";
const A: &str = "DS-TEST-A-0001      _00000001.";
const B: &str = "DS-TEST-B-0001      _00000001.";
const GIB: u64 = 1024 * 1024 * 1024;
const SIZE: u64 = 16 * GIB;
#[derive(Deserialize)]
struct Product {
    #[serde(rename = "UUID")]
    uuid: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct Disk {
    number: u32,
    serial_number: Option<String>,
    size: u64,
    logical_sector_size: Option<u64>,
    bus_type: Option<u16>,
    partition_style: Option<u16>,
    number_of_partitions: Option<u32>,
    is_boot: Option<bool>,
    is_system: Option<bool>,
    is_offline: Option<bool>,
    is_read_only: Option<bool>,
    is_clustered: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Legacy {
    index: u32,
    serial_number: Option<String>,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn serial(d: &Disk) -> &str {
    d.serial_number.as_deref().unwrap_or("").trim()
}
fn guard(uuid: &str, disks: &[Disk], legacy: &[Legacy]) -> Result<Disk, String> {
    if !uuid.eq_ignore_ascii_case(UUID) || disks.len() != 3 || legacy.len() != 3 {
        return Err("STOP: VM UUID or exact inventory mismatch".into());
    }
    for (number, expected, size) in [(0, "DS-OS-0001", 80 * GIB), (1, A, SIZE), (2, B, SIZE)] {
        let ds: Vec<_> = disks
            .iter()
            .filter(|d| d.number == number && serial(d) == expected)
            .collect();
        let ls: Vec<_> = legacy
            .iter()
            .filter(|d| {
                d.index == number && d.serial_number.as_deref().map(str::trim) == Some(expected)
            })
            .collect();
        if ds.len() != 1 || ls.len() != 1 || ds[0].size != size {
            return Err(
                "STOP: providers disagree on exact disk identity or Storage capacity".into(),
            );
        }
        let d = ds[0];
        if number == 0 {
            if d.is_boot != Some(true) || d.is_system != Some(true) {
                return Err("STOP: OS protection identity mismatch".into());
            }
        } else if d.is_boot != Some(false)
            || d.is_system != Some(false)
            || d.is_read_only != Some(false)
            || d.is_offline != Some(true)
            || d.is_clustered != Some(false)
            || d.bus_type != Some(17)
            || d.logical_sector_size != Some(512)
            || d.partition_style != Some(2)
        {
            return Err(
                "STOP: test disk protection/geometry mismatch; both must already be offline".into(),
            );
        }
    }
    let b = disks.iter().find(|d| d.number == 2).unwrap();
    if b.number_of_partitions != Some(9) {
        return Err("STOP: expected B's unchanged 9-partition fixture".into());
    }
    Ok(b.clone())
}
fn inventory() -> Result<Disk, String> {
    let cim = WMIConnection::new().map_err(err)?;
    let p: Vec<Product> = cim
        .raw_query("SELECT UUID FROM Win32_ComputerSystemProduct")
        .map_err(err)?;
    if p.len() != 1 || !p[0].uuid.eq_ignore_ascii_case(UUID) {
        return Err("STOP: not DiskStudio-Test; no device opened".into());
    }
    // Win32_DiskDrive.Size is calculated from CHS geometry, not exact bytes.
    // Cross-provider identity uses index + full serial. Exact capacity is
    // required from MSFT_Disk here and the verified device handle below.
    // https://learn.microsoft.com/windows/win32/cimwin32prov/win32-diskdrive
    let legacy: Vec<Legacy> = cim
        .raw_query("SELECT Index,SerialNumber FROM Win32_DiskDrive")
        .map_err(err)?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let disks:Vec<Disk>=con.raw_query("SELECT Number,SerialNumber,Size,LogicalSectorSize,BusType,PartitionStyle,NumberOfPartitions,IsBoot,IsSystem,IsOffline,IsReadOnly,IsClustered FROM MSFT_Disk").map_err(err)?;
    guard(&p[0].uuid, &disks, &legacy)
}
fn control(f: &File, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            code,
            if input.is_empty() {
                std::ptr::null()
            } else {
                input.as_ptr().cast()
            },
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(err(std::io::Error::last_os_error()))
    } else {
        Ok(returned as usize)
    }
}
fn native(f: &File) -> Result<(), String> {
    let mut n = [0; 12];
    let mut len = [0; 8];
    let mut geo = [0; 24];
    let mut attrs = [0; 16];
    if control(f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut n)? < 12
        || u32::from_le_bytes(n[4..8].try_into().unwrap()) != 2
        || control(f, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut len)? < 8
        || u64::from_le_bytes(len) != SIZE
        || control(f, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[], &mut geo)? < 24
        || u32::from_le_bytes(geo[20..24].try_into().unwrap()) != 512
        || control(f, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attrs)? < 16
        || u64::from_le_bytes(attrs[8..16].try_into().unwrap()) & 3 != 1
    {
        return Err("STOP: native number/geometry/offline attributes mismatch".into());
    }
    let mut desc = vec![0; 65536];
    let count = control(f, IOCTL_STORAGE_QUERY_PROPERTY, &[0; 12], &mut desc)?;
    if count < 36 || count > desc.len() {
        return Err("STOP: invalid native descriptor".into());
    }
    let start = u32::from_le_bytes(desc[24..28].try_into().unwrap()) as usize;
    if start < 36 || start >= count {
        return Err("STOP: no native serial".into());
    }
    let value = std::str::from_utf8(desc[start..count].split(|b| *b == 0).next().unwrap())
        .map_err(err)?
        .trim();
    if value != B {
        return Err("STOP: native serial mismatch".into());
    }
    Ok(())
}
fn read_at(f: &mut File, at: u64, len: usize) -> Result<Vec<u8>, String> {
    if at.checked_add(len as u64).is_none_or(|end| end > SIZE) || len > MIB as usize {
        return Err("STOP: read outside bounded fixture".into());
    }
    let mut b = vec![0; len];
    f.seek(SeekFrom::Start(at))
        .and_then(|_| f.read_exact(&mut b))
        .map_err(err)?;
    Ok(b)
}
fn healthy(head: &[u8], tail: &[u8]) -> Result<(), String> {
    let r = table_check::inspect_image(head, tail, SIZE, "vm-fixture.img")?.report;
    if r.scheme != "GPT"
        || r.sector_size != Some(512)
        || r.items
            .iter()
            .any(|i| matches!(i.state, CheckState::Failed | CheckState::Unchecked))
        || !["primary-gpt", "backup-gpt", "gpt-agreement"]
            .iter()
            .all(|c| passed(&r, c))
        || !["primary-layout", "backup-layout"].iter().all(|c| {
            r.items.iter().any(|i| {
                i.code == *c && matches!(i.state, CheckState::Passed | CheckState::Warning)
            })
        })
    {
        return Err("STOP: both GPT copies must be fully valid before fault injection; never retry an existing fault".into());
    }
    Ok(())
}
fn passed(r: &TableReport, code: &str) -> bool {
    r.items
        .iter()
        .any(|i| i.code == code && i.state == CheckState::Passed)
}
fn digest(head: &[u8], tail: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(SIZE.to_le_bytes());
    h.update(512u64.to_le_bytes());
    h.update(head);
    h.update(tail);
    format!("{:x}", h.finalize())
}
fn message(text: &str, flags: u32) -> i32 {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "DiskStudio VM GPT fixture"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}
fn run() -> Result<String, String> {
    if std::env::args_os().len() != 1 {
        return Err("STOP: arguments are not accepted".into());
    }
    let initial = inventory()?;
    if message("VM identity verified.\n\nTarget: disk 2, DS-TEST-B-0001      _00000001.\n16 GiB / 512 B / 9 partitions / OFFLINE\n\nFlip ONE bit in the BACKUP GPT header CRC only.\nPrimary GPT and all partition entries remain unchanged.\nNo automatic repair or retry.\n\nContinue fault injection?",MB_YESNO|MB_ICONWARNING|MB_DEFBUTTON2)!=IDYES {return Ok("Cancelled; no device opened.".into());}
    if inventory()? != initial {
        return Err("STOP: inventory changed".into());
    }
    // Fixed path only after exact UUID, inventory and protection validation.
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(r"\\.\PhysicalDrive2")
        .map_err(err)?;
    native(&f)?;
    let head = read_at(&mut f, 0, MIB as usize)?;
    let tail = read_at(&mut f, SIZE - MIB, MIB as usize)?;
    healthy(&head, &tail)?;
    let baseline = digest(&head, &tail);
    let mut expected = tail.clone();
    let at = expected.len() - 512;
    expected[at + 16] ^= 1;
    if inventory()? != initial
        || read_at(&mut f, 0, MIB as usize)? != head
        || read_at(&mut f, SIZE - MIB, MIB as usize)? != tail
    {
        return Err("STOP: fixture changed before write".into());
    }
    native(&f)?;
    f.seek(SeekFrom::Start(SIZE - 512)).map_err(err)?;
    if f.write(&expected[at..]).map_err(err)? != 512 {
        return Err("Partial write; stopped without retry or rollback".into());
    }
    f.sync_all().map_err(err)?;
    let after_head = read_at(&mut f, 0, MIB as usize)?;
    let after_tail = read_at(&mut f, SIZE - MIB, MIB as usize)?;
    if after_head != head || after_tail != expected {
        return Err("Readback differs; stopped without retry or rollback".into());
    }
    native(&f)?;
    if inventory()? != initial {
        return Err("Post-write metadata changed; stopped".into());
    }
    let report =
        table_check::inspect_image(&after_head, &after_tail, SIZE, "vm-fixture.img")?.report;
    if !passed(&report, "primary-gpt")
        || !report
            .items
            .iter()
            .any(|i| i.code == "backup-gpt" && i.state == CheckState::Failed)
    {
        return Err("Fault postcondition failed".into());
    }
    Ok(format!("Fault injected and verified on B only.\n1 changed byte / one 512-byte write.\nPrimary GPT and all other inspected bytes unchanged.\nB remains OFFLINE.\n\nBaseline snapshot SHA-256:\n{baseline}\n\nFault snapshot SHA-256:\n{}\n\nUse the production DiskStudio app to inspect and repair.",digest(&after_head,&after_tail)))
}
fn main() {
    match run() {
        Ok(s) => {
            message(&s, MB_OK | MB_ICONINFORMATION);
        }
        Err(e) => {
            message(
                &format!("{e}\n\nNo automatic retry or rollback."),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Vec<Disk>, Vec<Legacy>) {
        let disks: Vec<_> = ["DS-OS-0001", A, B]
            .iter()
            .enumerate()
            .map(|(i, s)| Disk {
                number: i as u32,
                serial_number: Some(s.to_string()),
                size: if i == 0 { 80 * GIB } else { SIZE },
                logical_sector_size: Some(512),
                bus_type: Some(17),
                partition_style: Some(2),
                number_of_partitions: Some(9),
                is_boot: Some(i == 0),
                is_system: Some(i == 0),
                is_offline: Some(i != 0),
                is_read_only: Some(false),
                is_clustered: Some(false),
            })
            .collect();
        let legacy = disks
            .iter()
            .map(|d| Legacy {
                index: d.number,
                serial_number: d.serial_number.clone(),
            })
            .collect();
        (disks, legacy)
    }
    #[test]
    fn exact_vm_inventory_required_before_device_access() {
        let (mut ds, mut ls) = fixture();
        assert!(guard(UUID, &ds, &ls).is_ok());
        assert!(guard("host", &ds, &ls).is_err());
        ds.push(ds[2].clone());
        assert!(guard(UUID, &ds, &ls).is_err());
        ds.pop();
        for case in 0..8 {
            let mut bad = ds.clone();
            match case {
                0 => bad[2].serial_number = Some("DS-TEST-B-0001".into()),
                1 => bad[2].size -= 512,
                2 => bad[2].is_offline = Some(false),
                3 => bad[2].is_read_only = None,
                4 => bad[2].is_boot = Some(true),
                5 => bad[2].number_of_partitions = Some(8),
                6 => bad[2].number = 0,
                _ => bad[2].logical_sector_size = Some(4096),
            };
            assert!(guard(UUID, &bad, &ls).is_err());
        }
        ls[2].serial_number = Some(A.into());
        assert!(guard(UUID, &ds, &ls).is_err());
    }
}

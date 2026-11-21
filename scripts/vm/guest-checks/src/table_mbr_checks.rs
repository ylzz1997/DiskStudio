//! Isolated 4Kn MBR fixture and independent partition-table export check.
use super::*;
use disk_core::MIB;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use table_archive::{Extent, Kind};

const SECTOR: u64 = 4096;
const FOLDER: &str = "DS-TableMBR";
const EXPORT: &str = "MBR.dspt";
const BASELINE: &str = "baseline.json";
const SIZE: u64 = 16 * GIB;

#[derive(Serialize, Deserialize, PartialEq)]
struct Baseline {
    os_layout: String,
    a_layout: String,
    b_layout: String,
    sectors: Vec<String>,
}

fn guest_disk<'a>(disks: &'a [Disk], serial: &str) -> Result<&'a Disk, String> {
    disks
        .iter()
        .find(|d| {
            d.serial_number.as_deref().map(str::trim) == Some(serial)
                && d.logical_sector_size == Some(SECTOR)
                && d.is_read_only == Some(false)
        })
        .ok_or_else(|| format!("Full 4Kn identity required for {serial}"))
}

fn source(b: &Disk) -> Result<Selection, String> {
    let target = identity(b).ok_or("B identity is unavailable")?;
    let snapshot = storage::read_snapshot()?;
    let key = snapshot
        .disks
        .iter()
        .find(|d| d.hardware.number == b.number && d.hardware.identity.as_ref() == Some(&target.0))
        .and_then(|d| d.hardware.table_layout_key.clone())
        .ok_or("B extended MBR table identity is unavailable")?;
    Ok(Selection {
        target,
        layout_key: key,
        partition: None,
    })
}

fn entry(block: &[u8], number: usize) -> (u8, u64, u64) {
    let start = 446 + number * 16;
    let field = |at| u64::from(u32::from_le_bytes(block[at..at + 4].try_into().unwrap())) * SECTOR;
    (block[start + 4], field(start + 8), field(start + 12))
}

fn check_layout(disks: &[Disk]) -> Result<(Baseline, Vec<Extent>), String> {
    let b = guest_disk(disks, B)?;
    if b.partition_style != Some(1) || b.is_offline != Some(true) {
        return Err("Expected the prepared offline MBR test disk B".into());
    }
    let selected = source(b)?;
    let ctx = partitions::platform::table_inspect_context(&selected)?;
    let mut file = data_windows::device(&ctx.resolved, false)?;
    let mut read = |offset| table_archive::read_at(&mut file, offset, SECTOR as usize, SIZE);
    let mbr = read(0)?;
    let (primary, primary_start, primary_len) = entry(&mbr, 0);
    let (container, base, container_len) = entry(&mbr, 1);
    if mbr[510..512] != [0x55, 0xaa]
        || !matches!(primary, 0x06 | 0x07)
        || primary_start != MIB
        || primary_len != 32 * MIB
        || !matches!(container, 5 | 0x0f)
        || base < primary_start + primary_len
        || container_len != 96 * MIB
        || base + container_len > 256 * MIB
        || ctx.mbr_container != Some((base, container_len))
        || mbr[478..510].iter().any(|&b| b != 0)
    {
        return Err("Unexpected native MBR partition records".into());
    }
    let mut table = vec![Extent {
        kind: Kind::Mbr,
        offset: 0,
        bytes: mbr,
    }];
    let mut parts = vec![(primary_start, primary_len)];
    let mut offset = base;
    for index in 0..2 {
        let ebr = read(offset)?;
        let (kind, rel, len) = entry(&ebr, 0);
        let (link, next, link_len) = entry(&ebr, 1);
        let start = offset + rel;
        if ebr[510..512] != [0x55, 0xaa]
            || ebr[478..510].iter().any(|&b| b != 0)
            || !matches!(kind, 0x06 | 0x07)
            || rel < SECTOR
            || len != 32 * MIB
            || start < parts.last().map(|&(p, l)| p + l).unwrap_or(0)
            || start + len > base + container_len
        {
            return Err("Unexpected native EBR partition records".into());
        }
        parts.push((start, len));
        table.push(Extent {
            kind: Kind::Ebr,
            offset,
            bytes: ebr,
        });
        if index == 0 {
            if !matches!(link, 5 | 0x0f)
                || base + next < start + len
                || link_len < SECTOR
                || next + link_len > container_len
            {
                return Err("Unexpected EBR link".into());
            }
            offset = base + next;
        } else if (link, next, link_len) != (0, 0, 0) {
            return Err("Unexpected additional logical partition link".into());
        }
    }
    let ranges: Vec<_> = ctx.parts.iter().map(|p| (p.offset, p.size)).collect();
    if parts != ranges {
        return Err("Raw MBR and EBR ranges do not match the Windows inventory".into());
    }
    for (offset, _) in parts {
        table.push(Extent {
            kind: Kind::Boot,
            offset,
            bytes: read(offset)?,
        });
    }
    table.sort_by_key(|e| e.offset);
    let baseline = Baseline {
        os_layout: reallocation_checks::os_layout(disks)?,
        a_layout: selection(A, None)?.layout_key,
        b_layout: selected.layout_key,
        sectors: table
            .iter()
            .map(|e| format!("{:x}", Sha256::digest(&e.bytes)))
            .collect(),
    };
    Ok((baseline, table))
}

pub(super) fn prepare() -> Result<String, String> {
    let _desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let old_os = reallocation_checks::os_layout(&disks)?;
    for serial in [A, B] {
        guest_disk(&disks, serial)?;
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let mut log = backup_protection::begin("table-mbr-4kn")?;
    online(A)?;
    operation(A, partitions::Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (partitions::FileSystem::Ntfs, "DSMBRTABLE", 4096),
        32 * MIB,
        2 * GIB,
    )?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(FOLDER).map_err(err)?;
    let folder = root.open_dir(FOLDER).map_err(err)?;
    online(B)?;
    let disks = inventory()?;
    whole_file_clone_checks::empty_raw(&disks, B)?;
    let b = guest_disk(&disks, B)?;
    let script = format!(
        "select disk {}\r\nconvert mbr\r\ncreate partition primary size=32 offset=1024\r\ncreate partition extended size=96\r\ncreate partition logical size=32\r\ncreate partition logical size=32\r\noffline disk\r\nexit\r\n",
        b.number,
    );
    folder.write("prepare.txt", script).map_err(err)?;
    let output = std::process::Command::new(r"C:\Windows\System32\diskpart.exe")
        .arg("/s")
        .arg(path.join(FOLDER).join("prepare.txt"))
        .output()
        .map_err(err)?;
    folder
        .write("prepare-output.txt", &output.stdout)
        .map_err(err)?;
    if !output.status.success() {
        return Err(format!(
            "DiskPart stopped with {:?}. Output is in {FOLDER}\\prepare-output.txt.",
            output.status.code()
        ));
    }
    let (baseline, _) = check_layout(&inventory()?)?;
    if baseline.os_layout != old_os {
        return Err("Guest OS layout changed".into());
    }
    folder
        .write(BASELINE, serde_json::to_vec(&baseline).map_err(err)?)
        .map_err(err)?;
    Ok(format!("PASS: B is offline with one primary and two logical partitions. Check its table and save {} with boot sectors enabled.", path.join(FOLDER).join(EXPORT).display()))
}

pub(super) fn record() -> Result<String, String> {
    let _desktop = btrfs_checks::desktop()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(FOLDER).map_err(err)?;
    folder.open("prepare-output.txt").map_err(err)?;
    let (baseline, _) = check_layout(&inventory()?)?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let file = folder.open_with(BASELINE, &options).map_err(err)?;
    serde_json::to_writer(file, &baseline).map_err(err)?;
    Ok(format!("PASS: Recorded existing offline B with one primary and two logical partitions. Save {} with boot sectors enabled.", path.join(FOLDER).join(EXPORT).display()))
}

pub(super) fn verify() -> Result<String, String> {
    let _desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(folder.open(BASELINE).map_err(err)?.take(4096)).map_err(err)?;
    let (current, expected) = check_layout(&inventory()?)?;
    let mut bytes = vec![];
    folder
        .open(EXPORT)
        .map_err(err)?
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    let archive = table_archive::Archive::decode(&bytes)?;
    if current != baseline
        || archive.size != SIZE
        || archive.sector != SECTOR
        || !archive.includes_boot
        || archive.extents != expected
    {
        return Err("Layouts, raw table or boot sectors, or exported bytes differ".into());
    }
    Ok(format!("PASS: Full MBR, both EBRs, three partition boot sectors and all layouts match. B remains offline. Archive SHA-256 {:x}", Sha256::digest(bytes)))
}

//! Verify the physical-device export using the existing 4Kn FAT restore pair.
//! Reads and the explicit optional offline transition are bound to Test B.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom, Write},
};

const FOLDER: &str = "DS-Device-Image";
const PART: u32 = 3;
const PART_START: u64 = 64 * MIB;
const PART_SIZE: u64 = 24 * MIB;
const SECTOR: u64 = 4096;
const DISK_SIZE: u64 = 16 * GIB;

#[derive(serde::Serialize, Deserialize, PartialEq, Eq)]
struct Baseline {
    desktop: String,
    tables: [String; 3],
    partition: String,
    masked_partition: String,
    skipped: u64,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn fixture_partition() -> Result<Selection, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let target = selection(B, Some(PART))?;
    let resolved = data_windows::resolve(&target, false, false)?;
    if resolved.offset != PART_START
        || resolved.size != PART_SIZE
        || resolved.disk.size != DISK_SIZE
    {
        return Err("STOP: Test B/3 does not match the retained FAT16 fixture".into());
    }
    Ok(target)
}

fn independently_mask_fat16(bytes: &mut [u8]) -> Result<u64, String> {
    if bytes.len() as u64 != PART_SIZE {
        return Err("Unexpected FAT16 fixture length".into());
    }
    let sector = usize::from(u16_at(bytes, 11));
    let spc = usize::from(bytes[13]);
    let reserved = usize::from(u16_at(bytes, 14));
    let fat_sectors = usize::from(u16_at(bytes, 22));
    let roots = usize::from(u16_at(bytes, 17)) * 32;
    let total16 = usize::from(u16_at(bytes, 19));
    let total = if total16 == 0 {
        u32_at(bytes, 32) as usize
    } else {
        total16
    };
    if sector as u64 != SECTOR
        || !spc.is_power_of_two()
        || bytes[16] != 2
        || reserved == 0
        || fat_sectors == 0
        || roots == 0
        || total != bytes.len() / sector
        || u64::from(u32_at(bytes, 28)) != PART_START / SECTOR
        || bytes[510..512] != [0x55, 0xaa]
    {
        return Err(format!(
            "Unexpected FAT16 boot geometry: {sector} B/sector, {spc} sectors/cluster, {total} sectors, {} FAT copies",
            bytes[16],
        ));
    }
    let data_sector = reserved + 2 * fat_sectors + roots.div_ceil(sector);
    let clusters = total
        .checked_sub(data_sector)
        .ok_or("FAT16 data starts outside the fixture")?
        / spc;
    if !(4085..=65524).contains(&clusters) || (clusters + 2) * 2 > fat_sectors * sector {
        return Err("FAT16 cluster count differs from the checked layout".into());
    }
    let fat_start = reserved * sector;
    let fat_end = fat_start + fat_sectors * sector;
    if bytes[fat_start..fat_end] != bytes[fat_end..fat_end + fat_sectors * sector] {
        return Err("The two FAT16 copies differ".into());
    }
    let mut skipped = 0;
    for index in 0..clusters {
        if u16_at(bytes, fat_start + (index + 2) * 2) == 0 {
            let start = (data_sector + index * spc) * sector;
            bytes[start..start + spc * sector].fill(0);
            skipped += (spc * sector) as u64;
        }
    }
    if skipped == 0 || skipped >= PART_SIZE {
        return Err("No bounded free FAT16 clusters".into());
    }
    Ok(skipped)
}

fn baseline(desktop: String, source: &std::fs::File) -> Result<Baseline, String> {
    let mut data = vec![0; PART_SIZE as usize];
    capacity_restore_checks::region(source, PART_START, PART_SIZE)?
        .read_exact(&mut data)
        .map_err(err)?;
    let partition = digest(&data);
    let skipped = independently_mask_fat16(&mut data)?;
    Ok(Baseline {
        desktop,
        tables: ntfs_reconstruction_checks::table_hashes()?,
        partition,
        masked_partition: digest(&data),
        skipped,
    })
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let partition = fixture_partition()?;
    let state = data_windows::resolve(&partition, false, false)?
        .disk
        .is_offline;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    if root.symlink_metadata(FOLDER).is_ok() {
        return Err("Device image output folder already exists".into());
    }
    if state == Some(false) {
        let request = device_state::Request {
            source: selection(B, None)?,
            action: device_state::Action::Offline,
        };
        let plan = device_state::prepare(request)?;
        if plan.size != DISK_SIZE || !plan.after.offline {
            return Err("Unexpected Test B transition".into());
        }
        device_state::execute(plan.token)?;
    }
    let source = capacity_restore_checks::b_device(true)?;
    let proof = baseline(desktop, &source)?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let mut witness = dir
        .open_with(
            "baseline.json",
            OpenOptions::new().write(true).create_new(true),
        )
        .map_err(err)?;
    witness
        .write_all(&serde_json::to_vec(&proof).map_err(err)?)
        .map_err(err)?;
    witness.sync_all().map_err(err)?;
    Ok(format!("PASS: verified offline Test B, 4Kn, B/3 FAT16 at 64 MiB, 24 MiB. Save whole.vhdx, partition.img and used.img in {}. FAT16 verified free bytes: {}.", path.join(FOLDER).display(), proof.skipped))
}

fn read_at(file: &mut (impl Read + Seek), offset: u64, length: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; length];
    file.seek(SeekFrom::Start(offset)).map_err(err)?;
    file.read_exact(&mut bytes).map_err(err)?;
    Ok(bytes)
}

fn verify_whole(mut file: cap_std::fs::File, source: &std::fs::File) -> Result<String, String> {
    let size = file.metadata().map_err(err)?.len();
    if !(4 * MIB..=512 * MIB).contains(&size) {
        return Err("Unexpected physical length of the sparse VHDX output".into());
    }
    let header = read_at(&mut file, 0, MIB as usize)?;
    let regions = &header[192 * 1024..256 * 1024];
    if &header[..8] != b"vhdxfile"
        || &regions[..4] != b"regi"
        || regions != &header[256 * 1024..320 * 1024]
        || u32_at(regions, 8) != 2
        || header[128 * 1024 + 48..128 * 1024 + 64]
            .iter()
            .any(|b| *b != 0)
    {
        return Err("The standalone VHDX header, log state or region tables differ".into());
    }
    let region = |id| -> Result<(u64, u64), String> {
        let id = uuid::Uuid::parse_str(id).map_err(err)?.to_bytes_le();
        let row = regions[16..80]
            .chunks_exact(32)
            .find(|row| row[..16] == id)
            .ok_or("Required VHDX region missing")?;
        let offset = u64_at(row, 16);
        let length = u64::from(u32_at(row, 24));
        if offset < MIB
            || offset % MIB != 0
            || length != MIB
            || offset.checked_add(length).is_none_or(|end| end > size)
        {
            return Err("VHDX region outside the fixture geometry".into());
        }
        Ok((offset, length))
    };
    let (bat_at, bat_len) = region("2dc27766-f623-4200-9d64-115e9bfd4a08")?;
    let (meta_at, meta_len) = region("8b7ca206-4790-4b9a-b8fe-575f050f886e")?;
    if bat_at == meta_at {
        return Err("Overlapping VHDX regions".into());
    }
    let min_payload = (bat_at + bat_len).max(meta_at + meta_len);
    let meta = read_at(&mut file, meta_at, meta_len as usize)?;
    if &meta[..8] != b"metadata" || u16_at(&meta, 10) != 5 {
        return Err("VHDX metadata table differs".into());
    }
    let field = |id, len| -> Result<&[u8], String> {
        let id = uuid::Uuid::parse_str(id).map_err(err)?.to_bytes_le();
        let row = meta[32..192]
            .chunks_exact(32)
            .find(|row| row[..16] == id)
            .ok_or("Required VHDX metadata missing")?;
        let from = u32_at(row, 16) as usize;
        if len != u32_at(row, 20) as usize {
            return Err("Unexpected VHDX field length".into());
        }
        meta.get(from..from + len)
            .ok_or_else(|| "VHDX metadata range invalid".into())
    };
    let parameters = field("caa16737-fa36-4d43-b3b6-33f0aa44e76b", 8)?;
    let block = u32_at(parameters, 0) as u64;
    let logical_size = u64_at(field("2fa54224-cd1b-4876-b211-5dbed83bf4b8", 8)?, 0);
    let sector = u32_at(field("8141bf1d-a96f-4709-ba47-f233a8faab5f", 4)?, 0) as u64;
    if logical_size != DISK_SIZE
        || sector != SECTOR
        || block != 8 * MIB
        || u32_at(parameters, 4) != 0
    {
        return Err("Expected a dynamic 16 GiB VHDX with 4Kn geometry".into());
    }
    let bat = read_at(&mut file, bat_at, bat_len as usize)?;
    let ratio = (1u64 << 23) * sector / block;
    let mut observed = BTreeSet::new();
    let mut device = capacity_restore_checks::region(source, 0, DISK_SIZE)?;
    let mut expected = vec![0; block as usize];
    let mut actual = vec![0; block as usize];
    let mut sha = Sha256::new();
    for index in 0..DISK_SIZE / block {
        let entry = u64_at(&bat, ((index + index / ratio) * 8) as usize);
        device.read_exact(&mut expected).map_err(err)?;
        if entry == 2 {
            actual.fill(0);
        } else {
            let at = entry & !(MIB - 1);
            if entry & (MIB - 1) != 6
                || at < min_payload
                || at % block != min_payload % block
                || at.checked_add(block).is_none_or(|end| end > size)
                || !observed.insert(at)
            {
                return Err("Invalid allocation in native VHDX output".into());
            }
            file.seek(SeekFrom::Start(at)).map_err(err)?;
            file.read_exact(&mut actual).map_err(err)?;
        }
        if expected != actual {
            return Err(format!("VHDX payload differs at byte {}", index * block));
        }
        sha.update(&actual);
    }
    if observed.is_empty() || min_payload + observed.len() as u64 * block != size {
        return Err("Unexpected unaccounted VHDX storage".into());
    }
    Ok(format!("{:x}", sha.finalize()))
}

fn partition_hash(dir: &Dir, name: &str) -> Result<String, String> {
    let mut logical = Vec::new();
    dir.open(name)
        .map_err(err)?
        .take(PART_SIZE + 1)
        .read_to_end(&mut logical)
        .map_err(err)?;
    if logical.len() as u64 != PART_SIZE {
        return Err(format!("Incorrect RAW partition length in {name}"));
    }
    Ok(digest(&logical))
}

pub(super) fn verify() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fixture_partition()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let mut record = Vec::new();
    dir.open("baseline.json")
        .map_err(err)?
        .take(4097)
        .read_to_end(&mut record)
        .map_err(err)?;
    let saved: Baseline = serde_json::from_slice(&record).map_err(err)?;
    let source = capacity_restore_checks::b_device(true)?;
    if record.len() > 4096 || baseline(desktop, &source)? != saved {
        return Err("Disk layout, source partition or desktop payload changed".into());
    }
    if partition_hash(&dir, "partition.img")? != saved.partition
        || partition_hash(&dir, "used.img")? != saved.masked_partition
    {
        return Err(
            "The original or allocation-masked RAW digest differs from offline Test B/3".into(),
        );
    }
    let whole = verify_whole(dir.open("whole.vhdx").map_err(err)?, &source)?;
    Ok(format!("PASS: 16 GiB VHDX equals offline Test B, SHA-256 {whole}. Both 24 MiB RAW images and {} masked FAT16 bytes verified. Both GPTs and guest OS layout match.", saved.skipped))
}

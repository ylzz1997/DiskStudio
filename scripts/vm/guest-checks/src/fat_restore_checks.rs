//! Prepare and independently read back the remaining FAT restore cases on 4Kn.
use super::*;
use cap_std::fs::{Dir, OpenOptions as DirOptions};
use capacity_restore_checks::{
    advance, archive_hash, b_device, backup, create, fat_tail, hash, receipt_digest,
    record_receipt, region, verify_file,
};
use disk_core::{fat_format, MIB};
use partitions::{policy, Action, FileSystem};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read, Seek, SeekFrom};

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fat_fixture;

const SECTOR: usize = 4096;
const FOLDER: &str = "DS-FatRestore";
const MISMATCH: &str = "Geometry.dsbackup";
const SMALL_FILE: &[u8] = b"DiskStudio FAT restore on 4Kn";
const FAT12_SERIAL: u32 = 0x81fa_1200;
const FAT16_SERIAL: u32 = 0x81fa_1600;

#[derive(Clone, Copy)]
struct Case {
    archive: &'static str,
    receipt: &'static str,
    mode: &'static str,
    format: &'static str,
    label: &'static str,
    offset: u64,
    size: u64,
    serial: u32,
    file: &'static str,
    content: &'static [u8],
}

const CASES: [Case; 5] = [
    Case {
        archive: "FAT12.dsbackup",
        receipt: "12-files.sha256",
        mode: "files",
        format: "FAT12",
        label: "FILFAT12",
        offset: 32 * MIB,
        size: 4 * MIB,
        serial: FAT12_SERIAL,
        file: "SAMPLE.BIN",
        content: SMALL_FILE,
    },
    Case {
        archive: "FAT16.dsbackup",
        receipt: "16-files.sha256",
        mode: "files",
        format: "FAT16",
        label: "FILFAT16",
        offset: 64 * MIB,
        size: 24 * MIB,
        serial: FAT16_SERIAL,
        file: "SAMPLE.BIN",
        content: SMALL_FILE,
    },
    Case {
        archive: "FAT32.dsbackup",
        receipt: "32-files.sha256",
        mode: "files",
        format: "FAT12",
        label: "FILFAT32",
        offset: 128 * MIB,
        size: 8 * MIB,
        serial: 0x37be_24d7,
        file: "FATVIEW.TXT",
        content: fat_fixture::FAT_CONTENT,
    },
    Case {
        archive: "FAT16.dsbackup",
        receipt: "16-resize.sha256",
        mode: "resize",
        format: "FAT16",
        label: "RZSFAT16",
        offset: 192 * MIB,
        size: 32 * MIB,
        serial: FAT16_SERIAL,
        file: "SAMPLE.BIN",
        content: SMALL_FILE,
    },
    Case {
        archive: "exFAT.dsbackup",
        receipt: "exfat-resize.sha256",
        mode: "resize",
        format: "exFAT",
        label: "RZSEXFAT",
        offset: 256 * MIB,
        size: 6 * MIB,
        serial: 0xe1ec_a27b,
        file: "exfat-view.txt",
        content: fat_fixture::EXFAT_CONTENT,
    },
];
const ARCHIVES: [&str; 5] = [
    "FAT12.dsbackup",
    "FAT16.dsbackup",
    "FAT32.dsbackup",
    "exFAT.dsbackup",
    MISMATCH,
];

#[derive(serde::Serialize, Deserialize)]
struct Witness {
    desktop: String,
    layouts: [String; 3],
    archives: Vec<String>,
    edges: Vec<[String; 2]>,
    tail: [u32; 2],
}

fn word(boot: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(boot[at..at + 2].try_into().unwrap())
}

fn dword(boot: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(boot[at..at + 4].try_into().unwrap())
}

fn fixed_root(boot: &[u8]) -> usize {
    usize::from(word(boot, 14)) + usize::from(boot[16]) * usize::from(word(boot, 22))
}

fn classic(kind: fat_format::FatType, size: usize, serial: u32) -> Result<(Vec<u8>, u32), String> {
    let format = fat_format::Plan::new(
        kind,
        size as u64,
        SECTOR as u32,
        SECTOR as u32,
        "FATRESTORE",
        serial,
        0,
    )
    .map_err(err)?;
    let mut image = Cursor::new(vec![0; size]);
    format.format(&mut image, |_, _| Ok(())).map_err(err)?;
    let mut image = image.into_inner();
    let root = fixed_root(&image);
    let heap = root + (usize::from(word(&image, 17)) * 32).div_ceil(SECTOR);
    let reserved = usize::from(word(&image, 14));
    let fat = usize::from(word(&image, 22));
    let cluster = if kind == fat_format::FatType::Fat16 {
        size / SECTOR - heap + 1
    } else {
        2
    };
    for copy in [reserved, reserved + fat] {
        let at = copy * SECTOR + cluster * 2;
        match kind {
            fat_format::FatType::Fat12 => {
                image[copy * SECTOR + 3..copy * SECTOR + 5].copy_from_slice(&[0xff, 0x0f])
            }
            fat_format::FatType::Fat16 => image[at..at + 2].fill(0xff),
            fat_format::FatType::Fat32 => {
                return Err("Expected classic FAT with a fixed root".into())
            }
        }
    }
    let file = &mut image[root * SECTOR + 32..root * SECTOR + 64];
    file[..11].copy_from_slice(b"SAMPLE  BIN");
    file[11] = 0x20;
    file[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    file[28..32].copy_from_slice(&(SMALL_FILE.len() as u32).to_le_bytes());
    let data = (heap + cluster - 2) * SECTOR;
    image[data..data + SMALL_FILE.len()].copy_from_slice(SMALL_FILE);
    Ok((image, cluster as u32))
}

fn exfat_tail() -> (Vec<u8>, u32) {
    let mut image = fat_fixture::exfat(SECTOR);
    let root = dword(&image, 88) as usize * SECTOR;
    let cluster = dword(&image, 92) + 1;
    let bitmap_bit = (cluster - 2) as usize;
    image[root + SECTOR] &= !(1 << 2);
    image[root + SECTOR + bitmap_bit / 8] |= 1 << (bitmap_bit % 8);
    image.copy_within(
        root + 2 * SECTOR..root + 3 * SECTOR,
        root + bitmap_bit * SECTOR,
    );
    image[root + 2 * SECTOR..root + 3 * SECTOR].fill(0);
    let entries = &mut image[root + 64..root + 160];
    entries[52..56].copy_from_slice(&cluster.to_le_bytes());
    entries[2..4].fill(0);
    let sum = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 2 | 3))
        .fold(0u16, |sum, (_, &byte)| {
            sum.rotate_right(1).wrapping_add(u16::from(byte))
        });
    entries[2..4].copy_from_slice(&sum.to_le_bytes());
    (image, cluster)
}

fn foreign_gpt() -> Result<Vec<u8>, String> {
    let size = 8 * MIB;
    let gpt = table_restore::new_gpt(
        size,
        512,
        format!("{:x}", Sha256::digest(b"archive-sector-mismatch-4kn")),
        uuid::Uuid::from_u128(0xbf0b_96e0_7047_4138_8d05_6543_827d_0883),
        &[table_restore::Entry {
            slot: 1,
            guid: "E54538D3-E741-4342-A2AF-282DD78B25F7".into(),
            kind: policy::BASIC_DATA.into(),
            offset: 2 * MIB,
            size: MIB,
            attributes: 0,
            name: vec![],
        }],
    )?;
    let mut image = vec![0; size as usize];
    for extent in gpt.extents {
        let at = extent.offset as usize;
        image[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
    }
    Ok(image)
}

fn build_sources() -> Result<([Vec<u8>; 5], [u32; 2]), String> {
    let (fat12, _) = classic(fat_format::FatType::Fat12, 8 * MIB as usize, FAT12_SERIAL)?;
    let (fat16, fat16_tail) = classic(fat_format::FatType::Fat16, 48 * MIB as usize, FAT16_SERIAL)?;
    let (fat32, _) = fat_tail();
    let (exfat, exfat_tail) = exfat_tail();
    let sources = [fat12, fat16, fat32, exfat, foreign_gpt()?];
    let cancel = AtomicBool::new(false);
    for (case, index) in CASES.into_iter().zip([0, 1, 2, 1, 3]) {
        let mut reader = Cursor::new(&sources[index]);
        if case.mode == "files" {
            let plan = fat_recovery::resize::file_clone::prepare(
                &mut reader,
                SECTOR as u64,
                case.size,
                case.offset,
                1,
                &cancel,
            )?;
            if plan.destination_format != case.format
                || plan.source_format != ["FAT12", "FAT16", "FAT32"][index]
                || plan.files != 1
            {
                return Err(format!("Unexpected file plan for {}", case.label));
            }
        } else {
            let plan = fat_recovery::resize::clone::prepare(
                &mut reader,
                SECTOR as u64,
                case.size,
                case.offset,
            )?;
            if plan.filesystem != case.format || plan.moved_clusters() != 1 {
                return Err(format!("Expected one relocated cluster for {}", case.label));
            }
        }
    }
    Ok((sources, [fat16_tail, exfat_tail]))
}

fn archive_hashes(dir: &Dir) -> Result<Vec<String>, String> {
    ARCHIVES
        .into_iter()
        .map(|name| archive_hash(dir, name))
        .collect()
}

fn edges(file: &fs::File) -> Result<Vec<[String; 2]>, String> {
    let sector = SECTOR as u64;
    CASES
        .into_iter()
        .map(|case| {
            Ok([
                hash(file, case.offset - sector, sector)?,
                hash(file, case.offset + case.size, sector)?,
            ])
        })
        .collect()
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    for serial in [A, B] {
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let (sources, tail) = build_sources()?;
    let mut log = backup_protection::begin("fat-variant-restore-4kn")?;
    online(A)?;
    operation(A, Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (FileSystem::Ntfs, "DSFATRESTOR", SECTOR as u32),
        32 * MIB,
        2 * GIB,
    )?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    for (name, bytes) in ARCHIVES.into_iter().zip(sources) {
        let kind = if name == MISMATCH {
            "disk"
        } else {
            "partition"
        };
        backup(&dir, name, bytes.as_slice(), bytes.len() as u64, kind)?;
    }
    online(B)?;
    operation(B, Action::Initialize {})?;
    let mut instructions = String::new();
    for case in CASES {
        let fs = if case.size >= 16 * MIB {
            FileSystem::Fat16
        } else {
            FileSystem::Fat12
        };
        let id = create(case.offset, case.size, fs, case.label)?;
        instructions.push_str(&format!(
            "\n{} {} → B/{id}, {} MiB",
            case.archive,
            case.mode,
            case.size / MIB
        ));
    }
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    if layouts[2] != os {
        return Err("Guest OS layout changed".into());
    }
    let witness = Witness {
        desktop,
        layouts,
        archives: archive_hashes(&dir)?,
        edges: edges(&b_device(false)?)?,
        tail,
    };
    let mut metadata = dir
        .open_with(
            "baseline.json",
            DirOptions::new().write(true).create_new(true),
        )
        .map_err(err)?;
    serde_json::to_writer(&mut metadata, &witness).map_err(err)?;
    metadata.sync_all().map_err(err)?;
    Ok(format!("PASS: {}. Open Geometry.dsbackup without writing, then restore in this order:{instructions}", path.join(FOLDER).display()))
}

fn readback(file: &fs::File, case: Case, tail: [u32; 2]) -> Result<Option<(u32, u32)>, String> {
    let mut partition = region(file, case.offset, case.size)?;
    let mut boot = [0; SECTOR];
    partition.read_exact(&mut boot).map_err(err)?;
    let (serial, moved) = if case.format == "exFAT" {
        let sectors = u64::from_le_bytes(boot[72..80].try_into().unwrap());
        let start = u64::from_le_bytes(boot[64..72].try_into().unwrap());
        if &boot[3..11] != b"EXFAT   "
            || boot[108] != 12
            || boot[109] != 0
            || sectors != case.size / SECTOR as u64
            || start != case.offset / SECTOR as u64
        {
            return Err("Unexpected restored exFAT extent".into());
        }
        let root = u64::from(dword(&boot, 88))
            + u64::from(dword(&boot, 96))
                .checked_sub(2)
                .ok_or("Missing exFAT root")?;
        let mut entry = [0; 32];
        partition
            .seek(SeekFrom::Start(root * SECTOR as u64 + 96))
            .map_err(err)?;
        partition.read_exact(&mut entry).map_err(err)?;
        if entry[0] != 0xc0 {
            return Err("Missing restored exFAT stream record".into());
        }
        (dword(&boot, 100), Some((tail[1], dword(&entry, 20))))
    } else {
        let count = usize::from(word(&boot, 19));
        let sectors = if count == 0 {
            dword(&boot, 32) as usize
        } else {
            count
        };
        let root = fixed_root(&boot);
        let heap = root + (usize::from(word(&boot, 17)) * 32).div_ceil(SECTOR);
        let cluster_size = usize::from(boot[13]);
        let clusters = if cluster_size.is_power_of_two() {
            sectors.saturating_sub(heap) / cluster_size
        } else {
            0
        };
        let kind = match clusters {
            1..=4084 => "FAT12",
            4085..=65524 => "FAT16",
            _ => "unknown",
        };
        if kind != case.format
            || word(&boot, 11) as usize != SECTOR
            || sectors as u64 != case.size / SECTOR as u64
            || u64::from(dword(&boot, 28)) != case.offset / SECTOR as u64
        {
            return Err(format!("Unexpected restored FAT extent for {}", case.label));
        }
        let moved = if case.mode == "resize" {
            let mut entry = [0; 32];
            partition
                .seek(SeekFrom::Start((root * SECTOR + 32) as u64))
                .map_err(err)?;
            partition.read_exact(&mut entry).map_err(err)?;
            if &entry[..11] != b"SAMPLE  BIN" {
                return Err("Missing relocated FAT16 file record".into());
            }
            Some((tail[0], u32::from(word(&entry, 26))))
        } else {
            None
        };
        (dword(&boot, 39), moved)
    };
    if (serial == case.serial) != (case.mode == "resize")
        || moved.is_some_and(|(from, to)| !(2..from).contains(&to))
    {
        return Err(format!(
            "Unexpected volume ID or cluster relocation for {}",
            case.label
        ));
    }
    partition.seek(SeekFrom::Start(0)).map_err(err)?;
    verify_file(&mut partition, case.file, case.content)?;
    Ok(moved)
}

fn completed(dir: &Dir) -> Result<Vec<String>, String> {
    let mut digests = Vec::new();
    for (index, case) in CASES.into_iter().enumerate() {
        if dir.try_exists(case.receipt).map_err(err)? {
            if digests.len() != index {
                return Err("Unexpected out-of-order restore receipt".into());
            }
            digests.push(receipt_digest(dir, case.receipt)?);
        }
    }
    Ok(digests)
}

pub(super) fn verify() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Witness =
        serde_json::from_reader(dir.open("baseline.json").map_err(err)?).map_err(err)?;
    let prior = completed(&dir)?;
    let index = prior.len();
    let case = CASES
        .get(index)
        .copied()
        .ok_or("All five readbacks already completed")?;
    let file = b_device(true)?;
    if desktop != baseline.desktop
        || ntfs_reconstruction_checks::table_hashes()? != baseline.layouts
        || archive_hashes(&dir)? != baseline.archives
        || edges(&file)? != baseline.edges
    {
        return Err(
            "Desktop, source archive, GPT, guest OS layout or adjacent sectors changed".into(),
        );
    }
    for earlier in &CASES[..index] {
        readback(&file, *earlier, baseline.tail)?;
    }
    let moved = readback(&file, case, baseline.tail)?;
    let digest = hash(&file, case.offset, case.size)?;
    let relocation = moved
        .map(|(from, to)| format!(". Cluster {from} to {to}"))
        .unwrap_or_default();
    let disposition = if let Some(next) = CASES.get(index + 1) {
        advance(&dir, case.receipt, &digest, file)?;
        format!(
            "B was offline during readback and is online for {} {}.",
            next.archive, next.mode
        )
    } else {
        drop(file);
        record_receipt(&dir, case.receipt, &digest)?;
        "All five results verified. B stays offline.".into()
    };
    Ok(format!("PASS: {} {} → {}, {} MiB{relocation}. File content, volume ID, archive files, GPTs, adjacent sectors and OS layout verified. {disposition}\n\n{digest}", case.archive, case.mode, case.format, case.size / MIB))
}

#[test]
fn remaining_fat_restore_fixtures_fit_their_expected_target_formats() {
    build_sources().unwrap();
}

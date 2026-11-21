//! Tests three unregistered filesystems in one previously zero Test B window.
use super::*;
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fixture;

const SECTOR: u64 = 512;
const START: u64 = 13 * GIB + 512 * MIB;
const SIZE: u64 = 48 * MIB;
const FOLDER: &str = "DS-LostBackupBoot";
const BASELINE: &str = "baseline.json";
const FAT32: u64 = START + 4 * MIB;
const EXFAT: u64 = START + 40 * MIB;
const NTFS: &[u8] = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");
const EXT_START: u64 = 8 * GIB;
const EXT_WINDOW: u64 = 66 * MIB;
const EXT_FOLDER: &str = "DS-LostExt4";
const EXT: &[u8] = include_bytes!("../../../../tests/fixtures/test_disk1.bin.zst");

struct Fixture {
    start: u64,
    size: u64,
    folder: &'static str,
    images: Vec<(u64, Vec<u8>)>,
}

fn boot_fixture() -> Fixture {
    let mut ntfs = NTFS.to_vec();
    ntfs[..512].fill(0);
    ntfs[16384..16384 + 2048].fill(0);
    let mut fat32 = fixture::fat32(SECTOR as usize);
    fat32[..512].fill(0);
    let mut exfat = fixture::exfat(SECTOR as usize);
    exfat[..12 * 512].fill(0);
    Fixture {
        start: START,
        size: SIZE,
        folder: FOLDER,
        images: vec![(START, ntfs), (FAT32, fat32), (EXFAT, exfat)],
    }
}

fn ext_fixture() -> Result<Fixture, String> {
    let mut image = zstd::stream::decode_all(EXT).map_err(err)?;
    if image.len() as u64 != 64 * MIB || image[1080..1082] != [0x53, 0xef] {
        return Err("Unexpected independent Ext4 fixture geometry".into());
    }
    image[1080..1082].fill(0);
    Ok(Fixture {
        start: EXT_START,
        size: EXT_WINDOW,
        folder: EXT_FOLDER,
        images: vec![(EXT_START + SECTOR, image)],
    })
}

fn read_window(file: &mut File, fixture: &Fixture) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; (fixture.size + 2 * SECTOR) as usize];
    file.seek(SeekFrom::Start(fixture.start - SECTOR))
        .map_err(err)?;
    for chunk in bytes.chunks_mut(128 * 1024) {
        file.read_exact(chunk).map_err(err)?;
    }
    Ok(bytes)
}

fn guard_range(
    a: &table_restore::Layout,
    b: &table_restore::Layout,
    fixture: &Fixture,
) -> Result<(), String> {
    if a.guid == b.guid
        || a.entries
            .iter()
            .any(|part| b.entries.iter().any(|other| part.guid == other.guid))
    {
        return Err("STOP: duplicate GPT identities on the test disks".into());
    }
    if (fixture.start - SECTOR) / SECTOR < b.first
        || (fixture.start + fixture.size) / SECTOR > b.last
        || b.entries.iter().any(|e| {
            e.offset < fixture.start + fixture.size + SECTOR
                && fixture.start - SECTOR < e.offset + e.size
        })
    {
        return Err(
            "STOP: space in the fixture window is outside usable LBAs or already assigned".into(),
        );
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Baseline {
    tables: [String; 3],
    before: String,
    after: String,
}

fn check_window(file: &mut File, baseline: &Baseline, fixture: &Fixture) -> Result<(), String> {
    let data = read_window(file, fixture)?;
    if hash(&data[..SECTOR as usize]) != baseline.before
        || hash(&data[data.len() - SECTOR as usize..]) != baseline.after
    {
        return Err("An adjacent sector differs from the baseline".into());
    }
    let mut start = SECTOR as usize;
    for (address, expected) in &fixture.images {
        let at = (address - fixture.start + SECTOR) as usize;
        if data[start..at].iter().any(|&byte| byte != 0)
            || data[at..at + expected.len()] != *expected
        {
            return Err(format!(
                "Fixture or padding changed at LBA {}",
                address / SECTOR
            ));
        }
        start = at + expected.len();
    }
    if data[start..data.len() - SECTOR as usize]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err("The trailing space in the fixture window changed".into());
    }
    Ok(())
}

fn prepare_fixture(fixture: &Fixture) -> Result<PathBuf, String> {
    btrfs_checks::desktop()?;
    let (root, output) = ntfs_reconstruction_checks::verified_a_root()?;
    if root.symlink_metadata(fixture.folder).is_ok() {
        return Err(
            "STOP: this fixture window already has a baseline; do not repeat preparation".into(),
        );
    }
    let a = quick_partition_checks::table(A)?;
    let b = quick_partition_checks::table(B)?;
    let original = table_restore::archive_layout(&b)?;
    guard_range(&table_restore::archive_layout(&a)?, &original, fixture)?;
    let source = selection(B, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    let locks = data_windows::lock(&ctx.resolved)?;
    let (fresh, _, _) = partitions::platform::table_restore_context(&source, Some((&ctx, &locks)))?;
    let parts = fresh.parts.iter().collect::<Vec<_>>();
    data_windows::verify_locks(&locks, &fresh.resolved, &parts)?;

    let mut file = data_windows::exclusive_device(&fresh.resolved)?;
    let table =
        table_archive::capture(&mut file, 16 * GIB, SECTOR, source.target.0, false, || {
            Ok(())
        })?;
    if table.encode()? != b.encode()?
        || !original.agrees(&extfs::restore_native(&file, Some(&parts), SECTOR)?.0)
    {
        return Err("STOP: B's raw and native tables changed or disagree".into());
    }
    let empty = read_window(&mut file, fixture)?;
    if empty[SECTOR as usize..empty.len() - SECTOR as usize]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err("STOP: the dedicated data window is not entirely zero".into());
    }
    data_windows::offline(&file)?;
    data_windows::confirm_writable_offline(&file)?;
    if !original.agrees(&extfs::restore_native(&file, Some(&parts), SECTOR)?.0)
        || read_window(&mut file, fixture)? != empty
    {
        return Err("STOP: B changed after taking it offline; no fixture data was written".into());
    }
    let baseline = Baseline {
        tables: [
            hash(&a.encode()?),
            hash(&b.encode()?),
            reallocation_checks::os_layout(&inventory()?)?,
        ],
        before: hash(&empty[..SECTOR as usize]),
        after: hash(&empty[empty.len() - SECTOR as usize..]),
    };
    root.create_dir(fixture.folder).map_err(err)?;
    let dir = root.open_dir(fixture.folder).map_err(err)?;
    let mut record = dir
        .open_with(BASELINE, CapOptions::new().write(true).create_new(true))
        .map_err(err)?;
    serde_json::to_writer(&mut record, &baseline).map_err(err)?;
    record.sync_all().map_err(err)?;
    for (address, data) in &fixture.images {
        file.seek(SeekFrom::Start(*address)).map_err(err)?;
        for chunk in data.chunks(128 * 1024) {
            if file.write(chunk).map_err(err)? != chunk.len() {
                return Err("STOP: short fixture write; no retry, B remains offline".into());
            }
        }
    }
    file.sync_all().map_err(err)?;
    check_window(&mut file, &baseline, fixture)?;
    drop(file);
    drop(locks);
    reallocation_checks::state(&inventory()?, B, true)?;
    if ntfs_reconstruction_checks::table_hashes()? != baseline.tables {
        return Err("STOP: GPT or guest OS layout changed".into());
    }
    Ok(output.join(fixture.folder))
}

pub(super) fn prepare() -> Result<String, String> {
    let output = prepare_fixture(&boot_fixture())?;
    Ok(format!(
        "PASS: three unregistered damaged volumes on offline B. Search from LBA {} for {} sectors. Expected starts NTFS {}, FAT32 {}, exFAT {}. A output: {}. Full fixture, padding, adjacent sectors and unchanged tables checked.",
        START / SECTOR, SIZE / SECTOR, START / SECTOR, FAT32 / SECTOR, EXFAT / SECTOR,
        output.display(),
    ))
}

fn check_exports(dir: &Dir) -> Result<(), String> {
    let mut verified = std::collections::BTreeSet::new();
    for entry in dir.entries().map_err(err)? {
        let name = entry.map_err(err)?.file_name();
        if !name.to_string_lossy().starts_with("DiskStudio-recovered-") {
            continue;
        }
        let out = dir.open_dir(name).map_err(err)?;
        let manifest = out.open(recovery_export::REPORT).map_err(err)?;
        let report: serde_json::Value =
            serde_json::from_reader(manifest.take(128 * 1024)).map_err(err)?;
        let proof = &report["fatReconstruction"];
        let (filesystem, filename, expected, anchor, bytes) = match proof["filesystem"].as_str() {
            Some("FAT32") => (
                "FAT32",
                "FATVIEW.TXT",
                fixture::FAT_CONTENT,
                6,
                68_608 * SECTOR,
            ),
            Some("exFAT") => ("exFAT", "exfat-view.txt", fixture::EXFAT_CONTENT, 12, MIB),
            _ => return Err("Unexpected export in the dedicated acceptance folder".into()),
        };
        let leaf = report["entries"][0]["name"]
            .as_str()
            .filter(|name| !name.contains(['/', '\\', ':']) && *name == data_io::safe_name(name))
            .ok_or("Invalid export file name")?;
        if report["completed"] != true
            || report["savedFiles"] != 1
            || report["failedFiles"] != 0
            || report["entries"][0]["originalName"] != filename
            || report["entries"][0]["folder"] != 0
            || report["outcomes"][0]["sha256"] != hash(expected)
            || proof["backupStartSector"] != anchor
            || proof["bytes"] != bytes
            || out.read(leaf).map_err(err)? != expected
            || !verified.insert(filesystem)
        {
            return Err(format!(
                "{filesystem} exported content or reconstruction manifest differs"
            ));
        }
    }
    if verified.len() != 2 {
        return Err("Expected one independently checked export from each FAT source".into());
    }
    Ok(())
}

fn verify_fixture(fixture: &Fixture) -> Result<Dir, String> {
    btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(fixture.folder).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(dir.open(BASELINE).map_err(err)?.take(4096)).map_err(err)?;
    reallocation_checks::state(&inventory()?, B, true)?;
    if ntfs_reconstruction_checks::table_hashes()? != baseline.tables {
        return Err("GPT or guest OS layout changed".into());
    }
    let source = selection(B, None)?;
    let mut file = data_windows::device(&data_windows::resolve(&source, false, false)?, false)?;
    check_window(&mut file, &baseline, fixture)?;
    Ok(dir)
}

pub(super) fn verify() -> Result<String, String> {
    let dir = verify_fixture(&boot_fixture())?;
    check_exports(&dir)?;
    Ok("PASS: FAT32 and exFAT file bytes, SHA-256 and reconstruction manifests match. Offline B's full fixture window and adjacent sectors, both GPTs and the guest OS layout are unchanged.".into())
}

pub(super) fn prepare_ext() -> Result<String, String> {
    let fixture = ext_fixture()?;
    let output = prepare_fixture(&fixture)?;
    Ok(format!(
        "PASS: unregistered Ext4 volume on offline B starts at LBA {}. Search from LBA {} for {} sectors. Preview small_file and export it to {}.",
        fixture.images[0].0 / SECTOR, fixture.start / SECTOR, fixture.size / SECTOR,
        output.display(),
    ))
}

pub(super) fn verify_ext() -> Result<String, String> {
    let fixture = ext_fixture()?;
    let dir = verify_fixture(&fixture)?;
    let mut found = 0;
    for entry in dir.entries().map_err(err)? {
        let name = entry.map_err(err)?.file_name();
        if !name.to_string_lossy().starts_with("DiskStudio-recovered-") {
            continue;
        }
        let out = dir.open_dir(name).map_err(err)?;
        let report: serde_json::Value = serde_json::from_reader(
            out.open(recovery_export::REPORT)
                .map_err(err)?
                .take(128 * 1024),
        )
        .map_err(err)?;
        let expected = b"hello, world!";
        let proof = &report["extReconstruction"];
        if report["completed"] != true
            || report["savedFiles"] != 1
            || report["failedFiles"] != 0
            || report["entries"][0]["originalName"] != "small_file"
            || report["entries"][0]["name"] != "000001_small_file"
            || report["entries"][0]["folder"] != 0
            || report["outcomes"][0]["sha256"] != hash(expected)
            || proof["backupGroup"] != 1
            || proof["backupOffset"] != 8_389_632u64
            || proof["sectorSize"] != SECTOR
            || proof["bytes"] != fixture.images[0].1.len()
            || out.read("000001_small_file").map_err(err)? != expected
        {
            return Err("The Ext4 file or reconstruction manifest differs".into());
        }
        found += 1;
    }
    if found != 1 {
        return Err("Expected exactly one checked Ext4 export".into());
    }
    Ok("PASS: Ext4 export bytes, SHA-256 and backup location match. Offline B's full extent and adjacent sectors, both GPTs and the guest OS layout are unchanged.".into())
}

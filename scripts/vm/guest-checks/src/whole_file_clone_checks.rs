//! End-to-end fixtures and independent readback for the dedicated whole-clone pair.
use super::*;
use disk_core::MIB;
use partitions::FileSystem;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek},
};

const KIND: &str = "whole-file-clone-4kn";
const PREPARED_WITH: &str = "c96b1a79caef234f24cef4d70be9fc0d7b7571a00c3b29e448c89d305aba29a1";
const MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
pub(super) const SAMPLE: &str = "DSCLONE.BIN";
const FIXTURES: [(FileSystem, &str, u64, u64, u64); 3] = [
    (FileSystem::Ntfs, "DSWHOLE_N", 32, 96, 48),
    (FileSystem::Fat16, "DSWHOLE_F", 256, 80, 64),
    (FileSystem::Exfat, "DSWHOLE_E", 416, 96, 80),
];

#[derive(serde::Serialize, Deserialize)]
struct Baseline {
    source: table_archive::Archive,
    os: String,
    samples: Vec<String>,
}

fn verified_inventory() -> Result<Vec<Disk>, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    Ok(disks)
}

pub(super) fn empty_raw(disks: &[Disk], serial: &str) -> Result<(), String> {
    if !disks.iter().any(|d| {
        d.serial_number.as_deref().map(disk_identity::clean) == Some(serial)
            && d.partition_style == Some(0)
            && d.number_of_partitions == Some(0)
            && d.is_read_only == Some(false)
    }) {
        return Err(format!("Fresh RAW {serial} fixture required"));
    }
    let source = selection(serial, None)?;
    let ctx = partitions::platform::table_inspect_context(&source)?;
    let mut file = data_windows::device(&ctx.resolved, false)?;
    let sector = test_sector();
    let mut layout = vec![0; 65536];
    let n = data_windows::control(
        &file,
        windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
        &[],
        &mut layout,
    )?;
    let style = u32::from_le_bytes(layout[..4].try_into().unwrap());
    let count = u32::from_le_bytes(layout[4..8].try_into().unwrap());
    let empty_mbr_header = n == 48 && style == 0 && layout[8..12] == [0; 4];
    if n < 48 || (style != 2 && !empty_mbr_header) || count != 0 || !ctx.parts.is_empty() {
        return Err(format!(
            "{serial}: native layout is not empty RAW. Bytes {n}, style {style}, native entries {count}, provider entries {}.",
            ctx.parts.len()
        ));
    }
    for offset in [0, 16 * GIB - MIB] {
        if table_archive::read_at(&mut file, offset, MIB as usize, 16 * GIB)?
            .iter()
            .any(|&byte| byte != 0)
        {
            return Err("RAW fixture contains unexpected boundary data".into());
        }
    }
    if ctx.resolved.disk.logical_sector_size != Some(sector) {
        return Err("The fixture sector size changed".into());
    }
    Ok(())
}

pub(super) fn sample(index: usize) -> Vec<u8> {
    (0..131_087 + index * 4096)
        .map(|i| ((i * 47 + i / 137 + index * 19) % 251) as u8)
        .collect()
}

fn witness(desktop: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-whole-file-clone-{desktop}.json"
    ))
}

fn read_baseline(desktop: &str) -> Result<Baseline, String> {
    serde_json::from_reader(File::open(witness(desktop)).map_err(err)?.take(1024 * 1024))
        .map_err(err)
}

fn write_baseline(desktop: &str, baseline: &Baseline) -> Result<(), String> {
    let mut proof = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(witness(desktop))
        .map_err(err)?;
    serde_json::to_writer(&mut proof, baseline).map_err(err)?;
    proof.sync_all().map_err(err)
}

pub(super) fn prepare(resume: bool) -> Result<String, String> {
    let disks = verified_inventory()?;
    let desktop = btrfs_checks::desktop()?;
    empty_raw(&disks, B)?;
    let os = reallocation_checks::os_layout(&disks)?;
    if witness(&desktop).try_exists().map_err(err)? {
        return Err("The source witness already exists".into());
    }
    if resume {
        reallocation_checks::state(&disks, A, false)?;
        let layout = fat_resize_checks::model(A)?;
        let mut data = layout.entries.iter().filter(|e| e.kind != MSR);
        let first = data.next().ok_or("Prior NTFS fixture is missing")?;
        if first.offset != FIXTURES[0].2 * MIB
            || first.size != FIXTURES[0].3 * MIB
            || first.kind != partitions::policy::BASIC_DATA
            || first.attributes != 0
            || data.next().is_some()
        {
            return Err(
                "Resume requires the exact prior NTFS fixture and no other data entries".into(),
            );
        }
    } else {
        empty_raw(&disks, A)?;
    }
    let mut log = backup_protection::begin(KIND)?;
    if !resume {
        online(A)?;
        operation(A, partitions::Action::Initialize {})?;
    }
    let mut samples = Vec::new();
    let mut choices = Vec::new();
    for (index, (fs, label, offset, size, target)) in FIXTURES.into_iter().enumerate() {
        let offset = offset * MIB;
        let size = size * MIB;
        let payload = sample(index);
        let selection = if resume && index == 0 {
            let (selection, actual) =
                allocation_batch::fixture_selection(fs.name(), Some(label), size)?;
            if offset != actual {
                return Err("Prior NTFS partition position differs".into());
            }
            selection
        } else {
            let (selection, _) = fat_resize_checks::create_capacity_fixture(
                &mut log,
                (fs, label, 4096),
                offset,
                size,
            )?;
            allocation_batch::write_fixture(&selection, offset, size, SAMPLE, &payload, true)?;
            selection
        };
        let source = allocation_batch::resolved(&selection, offset, size, true)?;
        let locks = data_windows::state_locks(&source)?;
        let locked = data_windows::resolve_locked(&selection, false, &source, &locks)?;
        let mut reader = data_windows::reader(&locked)?;
        samples.push(
            sample_hash(&mut reader, &payload)
                .map_err(|error| format!("{label} locked source readback: {error}"))?,
        );
        data_windows::verify_data_locks(&locks, &locked)?;
        choices.push(format!(
            "A/{} {} -> {} MiB",
            selection.partition.ok_or("Missing partition number")?,
            fs.name(),
            target
        ));
    }
    let after = verified_inventory()?;
    empty_raw(&after, B)?;
    if reallocation_checks::os_layout(&after)? != os {
        return Err("The guest OS layout changed during fixture preparation".into());
    }
    let baseline = Baseline {
        source: quick_partition_checks::table(A)?,
        os,
        samples,
    };
    write_baseline(&desktop, &baseline)?;
    Ok(format!("PASS: 4Kn mixed-filesystem source on A, verified empty RAW B.\n{}\nUse these capacities in DiskStudio.", choices.join("\n")))
}

pub(super) fn rebind() -> Result<String, String> {
    let disks = verified_inventory()?;
    let desktop = btrfs_checks::desktop()?;
    empty_raw(&disks, B)?;
    reallocation_checks::state(&disks, A, false)?;
    let baseline = read_baseline(PREPARED_WITH)?;
    if quick_partition_checks::table(A)? != baseline.source
        || reallocation_checks::os_layout(&disks)? != baseline.os
    {
        return Err("The prepared source GPT or guest OS layout changed".into());
    }
    for (index, (fs, label, offset, size, _)) in FIXTURES.into_iter().enumerate() {
        let (selection, actual) =
            allocation_batch::fixture_selection(fs.name(), Some(label), size * MIB)?;
        if actual != offset * MIB {
            return Err("Prepared partition location differs".into());
        }
        let hash =
            allocation_batch::check_file(&selection, actual, size * MIB, SAMPLE, &sample(index))?;
        if baseline.samples.get(index) != Some(&hash) {
            return Err("The prepared sample differs from the previous readback".into());
        }
    }
    write_baseline(&desktop, &baseline)?;
    Ok("PASS: exact source GPT, three sample files and empty RAW B rechecked with this desktop build. Use A/2 48 MiB, A/3 64 MiB, A/4 80 MiB.".into())
}

pub(super) fn sample_hash(reader: &mut (impl Read + Seek), expected: &[u8]) -> Result<String, String> {
    let mut matches = Vec::new();
    recovery::scan_filesystem_with_directories(
        reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| {
            if !file.deleted && file.name.eq_ignore_ascii_case(SAMPLE) {
                matches.push(file);
            }
        },
        |_| Ok(()),
    )?;
    let [file] = matches.as_slice() else {
        return Err(format!(
            "Expected one {SAMPLE} entry, found {}",
            matches.len()
        ));
    };
    if file.size != expected.len() as u64 {
        return Err("The cloned sample length differs".into());
    }
    let mut result = Vec::new();
    recovery::extract(reader, file, &mut result, file.size, |_| Ok(()))?;
    if result != expected {
        return Err("The cloned sample contents differ".into());
    }
    Ok(format!("{:x}", Sha256::digest(result)))
}

pub(super) fn verify() -> Result<String, String> {
    let disks = verified_inventory()?;
    let desktop = btrfs_checks::desktop()?;
    reallocation_checks::state(&disks, A, false)?;
    reallocation_checks::state(&disks, B, true)?;
    let baseline = read_baseline(&desktop)?;
    if quick_partition_checks::table(A)? != baseline.source
        || reallocation_checks::os_layout(&disks)? != baseline.os
    {
        return Err("Source GPT or guest OS layout changed".into());
    }
    let archive = quick_partition_checks::table(B)?;
    let mut layout = table_restore::archive_layout(&archive)?;
    layout.entries.sort_by_key(|entry| entry.offset);
    let mut source = table_restore::archive_layout(&baseline.source)?;
    source.entries.sort_by_key(|entry| entry.offset);
    if source.guid == layout.guid
        || source.entries.len() != layout.entries.len()
        || layout
            .entries
            .iter()
            .any(|entry| source.entries.iter().any(|old| old.guid == entry.guid))
    {
        return Err("Target layout count or newly assigned identifiers differ".into());
    }
    let bound = data_windows::resolve(&selection(B, None)?, false, false)?;
    let file = data_windows::device(&bound, false)?;
    data_windows::confirm_writable_offline(&file)?;
    let mut cursor = MIB;
    let mut tested = 0;
    let mut occupied = archive
        .extents
        .iter()
        .map(|extent| (extent.offset, extent.offset + extent.bytes.len() as u64))
        .collect::<Vec<_>>();
    for (entry, old) in layout.entries.iter().zip(&source.entries) {
        if entry.offset != cursor
            || entry.name != old.name
            || entry.kind != old.kind
            || entry.attributes != 0
        {
            return Err("Packed target placement or GPT fields differ".into());
        }
        if entry.kind == MSR {
            if entry.size != old.size {
                return Err("MSR capacity changed".into());
            }
        } else {
            let &(fs, label, at, size, target) =
                FIXTURES.get(tested).ok_or("Unexpected data partition")?;
            if old.offset != at * MIB || old.size != size * MIB || entry.size != target * MIB {
                return Err("A source or resized target extent differs".into());
            }
            let expected = sample(tested);
            let (selection, _) =
                allocation_batch::fixture_selection(fs.name(), Some(label), old.size)?;
            let original =
                allocation_batch::check_file(&selection, old.offset, old.size, SAMPLE, &expected)?;
            let mut reader =
                data_io::Region::for_metadata(&file, entry.offset, entry.size).map_err(err)?;
            if baseline.samples.get(tested) != Some(&original)
                || sample_hash(&mut reader, &expected)? != original
            {
                return Err("Source or target samples changed".into());
            }
            tested += 1;
            occupied.push((entry.offset, entry.offset + entry.size));
        }
        cursor = (entry.offset + entry.size).div_ceil(MIB) * MIB;
    }
    if tested != FIXTURES.len() {
        return Err("A cloned data partition is missing".into());
    }
    let digest = verify_bytes(&file, bound.size, occupied)?;
    data_windows::confirm_writable_offline(&file)?;
    Ok(format!("PASS: NTFS, FAT16, exFAT files and resized layout match.\nB offline. Source GPT and OS layout match. Unused areas are zero.\nFull disk SHA-256\n{digest}"))
}

pub(super) fn verify_bytes(file: &File, size: u64, mut occupied: Vec<(u64, u64)>) -> Result<String, String> {
    occupied.sort_unstable();
    let mut at = 0;
    let mut hasher = Sha256::new();
    let mut bytes = vec![0u8; MIB as usize];
    let mut input = data_io::Region::new(file, 0, size).map_err(err)?;
    for (start, end) in occupied.into_iter().chain([(size, size)]) {
        if start < at || end < start || end > size {
            return Err("Independent readback has invalid ranges".into());
        }
        for (limit, zero) in [(start, true), (end, false)] {
            while at < limit {
                let len = (limit - at).min(MIB) as usize;
                input.read_exact(&mut bytes[..len]).map_err(err)?;
                if zero && bytes[..len].iter().any(|&byte| byte != 0) {
                    return Err(format!("Unexpected data in unused range at byte {at}"));
                }
                hasher.update(&bytes[..len]);
                at += len as u64;
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

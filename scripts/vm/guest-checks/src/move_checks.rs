//! Prepare and independently inspect a native move. Never dispatch a move
//! from this helper. Run only inside the exact isolated Windows A/B lab.
use super::*;
use allocation_batch::{check_file, record, set_online, write_fixture};
use disk_core::{partition_move::Journal, MIB};
use fat_resize_checks::create_capacity_fixture;
use partitions::{Action, DriveLetter, FileSystem, PartitionKind};
use serde::Serialize;
use serde_json::json;
use std::{
    io::{self, Read, Seek, SeekFrom},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

const SAMPLE: &str = "DSMOVE.BIN";
const JOURNAL: &str = "DiskStudio-move.dsmove";
const JOURNAL_LABEL: &str = "DS4KMOVEJRN";
const FIRST_4KN_DRIVER: &str = "4c1e5d87f20a380cfd0db162ccf81cc93a2c71d65530294139794ecc1a3ded6e";

pub(super) struct Case {
    base: &'static str,
    label: &'static str,
    filesystem: FileSystem,
    size: u64,
    shift: i64,
    existing_offset: Option<u64>,
    journal_folder: Option<&'static str>,
    prepare_log: &'static str,
    interrupted_log: &'static str,
    inspect_log: &'static str,
}

pub(super) const FIRST: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-20261117",
    label: "DSMOVE",
    filesystem: FileSystem::Fat16,
    size: 512 * MIB,
    shift: 64 * MIB as i64,
    existing_offset: None,
    journal_folder: None,
    prepare_log: "partition-move-prepare",
    interrupted_log: "partition-move-interrupted",
    inspect_log: "partition-move-inspect",
};

pub(super) const STOP: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-stop-20261117",
    label: "DSMOVESTOP",
    filesystem: FileSystem::Fat16,
    size: 512 * MIB,
    shift: 64 * MIB as i64,
    existing_offset: None,
    journal_folder: Some("DiskStudio-move-stop"),
    prepare_log: "partition-move-stop-prepare",
    interrupted_log: "partition-move-stop-interrupted",
    inspect_log: "partition-move-stop-inspect",
};

pub(super) const EXFAT_4KN: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-4kn-exfat-20261117",
    label: "DSMOVEEX4K",
    filesystem: FileSystem::Exfat,
    size: 96 * MIB,
    shift: 4 * MIB as i64,
    existing_offset: None,
    journal_folder: Some("DiskStudio-move-4kn-exfat"),
    prepare_log: "partition-move-4kn-exfat-prepare",
    interrupted_log: "partition-move-4kn-exfat-interrupted",
    inspect_log: "partition-move-4kn-exfat-inspect",
};

pub(super) const NTFS_4KN: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-4kn-ntfs-20261117",
    label: "DSMOVENT4K",
    filesystem: FileSystem::Ntfs,
    size: 128 * MIB,
    shift: -4 * MIB as i64,
    existing_offset: None,
    journal_folder: Some("DiskStudio-move-4kn-ntfs"),
    prepare_log: "partition-move-4kn-ntfs-prepare",
    interrupted_log: "partition-move-4kn-ntfs-interrupted",
    inspect_log: "partition-move-4kn-ntfs-inspect",
};

pub(super) const FAT12_4KN: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-4kn-fat12-20261117",
    label: "DSSEC12",
    filesystem: FileSystem::Fat12,
    size: 8 * MIB,
    shift: 4 * MIB as i64,
    existing_offset: Some(664 * MIB),
    journal_folder: Some("DiskStudio-move-4kn-fat12"),
    prepare_log: "partition-move-4kn-fat12-prepare",
    interrupted_log: "partition-move-4kn-fat12-interrupted",
    inspect_log: "partition-move-4kn-fat12-inspect",
};

pub(super) const FAT32_4KN: Case = Case {
    base: r"C:\ProgramData\DiskStudio-partition-move-4kn-fat32-20261117",
    label: "DSSEC32",
    filesystem: FileSystem::Fat32,
    size: 320 * MIB,
    shift: -4 * MIB as i64,
    existing_offset: Some(212 * MIB),
    journal_folder: Some("DiskStudio-move-4kn-fat32"),
    prepare_log: "partition-move-4kn-fat32-prepare",
    interrupted_log: "partition-move-4kn-fat32-interrupted",
    inspect_log: "partition-move-4kn-fat32-inspect",
};

#[derive(Serialize, Deserialize)]
struct Fixture {
    desktop: String,
    guid: String,
    old_offset: u64,
    partition: u32,
    b_volume: String,
    b_letter: String,
    #[serde(default)]
    boot_offsets: Option<Vec<u64>>,
}

fn bytes() -> Vec<u8> {
    (0..131_089)
        .map(|i| ((i * 53 + i / 131) % 251) as u8)
        .collect()
}

impl Case {
    fn sample(&self) -> (&'static str, Vec<u8>) {
        if self.existing_offset.is_some() {
            (
                fat_sector_checks::SAMPLE,
                fat_sector_checks::sample_bytes(self.size / MIB),
            )
        } else {
            (SAMPLE, bytes())
        }
    }
}

fn snapshot(serial: &str) -> Result<storage::LocalDisk, String> {
    let target = selection(serial, None)?.target;
    storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&target.0))
        .ok_or_else(|| "Bound disk not visible in Windows inventory".into())
}

fn save_new(path: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(err)
}

pub fn prepare(case: &Case) -> Result<String, String> {
    prepare_fixture(case, false)
}

pub fn prepare_4kn(case: &Case, provision_journal: bool) -> Result<String, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    if provision_journal {
        let b = disks
            .iter()
            .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
            .ok_or("4Kn B disk missing")?;
        if b.partition_style != Some(0) || b.number_of_partitions != Some(0) {
            return Err("First move batch expects the untouched RAW 4Kn B disk".into());
        }
    }
    prepare_fixture(case, provision_journal)
}

pub fn prepare_existing_4kn_fat(
    case: &Case,
    previous: &Case,
    was_onlined: bool,
) -> Result<String, String> {
    let log_name = if was_onlined {
        "partition-move-4kn-fat12-continue-prepare"
    } else {
        case.prepare_log
    };
    let mut log = backup_protection::begin(log_name)?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    if !disks.iter().any(|d| {
        d.serial_number.as_deref().map(disk_identity::clean) == Some(A)
            && d.is_offline == Some(!was_onlined)
            && d.is_read_only == Some(false)
    }) {
        return Err("The 4Kn A disk is not in the expected state for preparation".into());
    }
    let fixture = load_fixture(previous)?;
    let (previous_journal, binding) = journal(previous, &fixture)?;
    if !previous_journal.completed() {
        return Err("The preceding move checkpoint is unfinished".into());
    }
    verify_witness_layout(
        previous,
        &fixture,
        Some(previous_journal.range().destination()),
    )?;
    data_windows::FileBinding::validate_all(&[binding])?;
    drop(previous_journal);

    let base = case.base;
    let prepared = b"existing FAT move fixture";
    if was_onlined {
        if saved(&format!("{base}.once"), 4096)? != prepared
            || ["json", "a.dspt", "b.dspt"]
                .iter()
                .any(|extension| Path::new(&format!("{base}.{extension}")).exists())
        {
            return Err("An unrecorded FAT preparation cannot be continued".into());
        }
    } else {
        save_new(&format!("{base}.once"), prepared)?;
        set_online(A)?;
    }
    let (selected, offset) =
        allocation_batch::fixture_selection(case.filesystem.name(), Some(case.label), case.size)?;
    if Some(offset) != case.existing_offset {
        return Err(format!(
            "Previously seeded 4Kn FAT range changed to {offset} bytes"
        ));
    }
    let current = fat_resize_checks::model(A)?;
    let destination = offset
        .checked_add_signed(case.shift)
        .ok_or("Invalid fixture destination")?;
    let end = destination
        .checked_add(case.size)
        .ok_or("Invalid destination size")?;
    if destination / test_sector() < current.first
        || end / test_sector() > current.last + 1
        || current.entries.iter().any(|entry| {
            (entry.offset, entry.size) != (offset, case.size)
                && entry.offset < end
                && destination < entry.offset + entry.size
        })
    {
        return Err(
            "The existing FAT fixture's destination is occupied or outside GPT bounds".into(),
        );
    }
    let (name, expected) = case.sample();
    let digest = check_file(&selected, offset, case.size, name, &expected)?;
    let folder = journal_folder(case, true)?;
    let b_before = quick_partition_checks::table(B)?.encode()?;
    finish_fixture(case, &mut log, &selected, offset, folder, b_before, digest)
}

fn provision_4kn_journal(log: &mut fs::File) -> Result<(), String> {
    const OFFSET: u64 = 32 * MIB;
    const LENGTH: u64 = 256 * MIB;
    record(log, json!({"event":"initialize-4kn-test-b-once"}))?;
    operation(B, Action::Initialize {})?;
    let base = fat_resize_checks::model(B)?;
    record(
        log,
        json!({"event":"create-4kn-test-b-journal-volume","offset":OFFSET,"size":LENGTH}),
    )?;
    operation(
        B,
        Action::Create {
            offset: OFFSET,
            size: LENGTH,
            kind: PartitionKind::Windows,
        },
    )?;
    let mut created = fat_resize_checks::model(B)?;
    let entries = created.entries.len();
    created.entries.retain(|e| {
        e.offset != OFFSET || e.size != LENGTH || e.kind != partitions::policy::BASIC_DATA
    });
    if created.entries.len() + 1 != entries || !created.agrees(&base) {
        return Err("Creating the 4Kn journal volume changed unexpected GPT entries".into());
    }
    let b = snapshot(B)?;
    let mut found = b
        .partitions
        .iter()
        .filter(|p| p.base.start == OFFSET && p.base.size == LENGTH);
    let partition = found
        .next()
        .ok_or("4Kn journal partition is absent")?
        .number;
    if found.next().is_some() {
        return Err("4Kn journal partition is ambiguous".into());
    }
    operation(
        B,
        Action::Format {
            partition,
            label: JOURNAL_LABEL.into(),
            file_system: FileSystem::Ntfs,
            cluster_size: 4096,
            options: Default::default(),
        },
    )?;
    let b = snapshot(B)?;
    let p = b
        .partitions
        .iter()
        .find(|p| p.number == partition && p.base.start == OFFSET && p.base.size == LENGTH)
        .ok_or("4Kn journal volume absent after formatting")?;
    if p.drive_letter.is_none() {
        operation(
            B,
            Action::SetDriveLetter {
                partition: p.number,
                letter: Some(DriveLetter("Z".into())),
            },
        )?;
    }
    Ok(())
}

fn prepare_fixture(case: &Case, provision_journal: bool) -> Result<String, String> {
    let mut log = backup_protection::begin(case.prepare_log)?;
    inventory()?;
    let base = case.base;
    if provision_journal {
        fat_resize_checks::model(A)?;
    }
    save_new(&format!("{base}.once"), b"partition move fixture")?;
    set_online(A)?;
    set_online(B)?;
    if provision_journal {
        provision_4kn_journal(&mut log)?;
    }
    let (folder, letter) = journal_folder(case, true)?;
    let b_before = quick_partition_checks::table(B)?.encode()?;
    let a_target = selection(A, None)?.target;
    let status = partitions::status();
    let target = status
        .targets
        .into_iter()
        .find(|t| t.serial == A && t.target == a_target && t.available)
        .ok_or("A has no available partition management")?;
    let space = target
        .free_ranges
        .iter()
        .find(|r| r.size >= case.size + case.shift.unsigned_abs() + 32 * MIB)
        .ok_or("A has no single free range for an isolated overlapping move")?;
    let offset = space.offset.div_ceil(MIB) * MIB
        + 16 * MIB
        + if case.shift < 0 {
            case.shift.unsigned_abs()
        } else {
            0
        };
    let cluster = if case.filesystem == FileSystem::Fat16 {
        16 * 1024
    } else {
        4096
    };
    let (selected, _) = create_capacity_fixture(
        &mut log,
        (case.filesystem, case.label, cluster),
        offset,
        case.size,
    )?;
    write_fixture(&selected, offset, case.size, SAMPLE, &bytes(), true)?;
    let digest = check_file(&selected, offset, case.size, SAMPLE, &bytes())?;
    finish_fixture(
        case,
        &mut log,
        &selected,
        offset,
        (folder, letter),
        b_before,
        digest,
    )
}

fn journal_folder(case: &Case, create: bool) -> Result<(PathBuf, String), String> {
    let b = snapshot(B)?;
    let b_target = selection(B, None)?.target;
    let b_part = b
        .partitions
        .iter()
        .find(|p| {
            p.base.fs.eq_ignore_ascii_case("NTFS")
                && p.available.is_some_and(|n| n > 16 * MIB)
                && p.drive_letter.as_deref().is_some_and(|s| s.len() == 1)
                && (test_sector() == 512 || p.volume_label.as_deref() == Some(JOURNAL_LABEL))
        })
        .ok_or("B requires an existing NTFS volume with a letter and available space")?;
    let b_letter = b_part
        .drive_letter
        .clone()
        .ok_or("Missing B drive letter")?;
    let root = PathBuf::from(format!(r"{b_letter}:\"));
    let (volume_root, owner) = data_windows::folder_location(&root)?;
    if owner != b_target {
        return Err("B journal directory is not bound to Test B".into());
    }
    let folder = match case.journal_folder {
        Some(relative) => {
            let requested = volume_root.join(relative);
            if create {
                fs::create_dir(&requested).map_err(err)?;
            }
            let (folder, owner) = data_windows::folder_location(&requested)?;
            if owner != b_target {
                return Err("New journal directory is not bound to Test B".into());
            }
            folder
        }
        None => volume_root,
    };
    if folder.join(JOURNAL).exists() {
        return Err(
            "B journal folder does not match its witness or already contains the file".into(),
        );
    }
    Ok((folder, b_letter))
}

fn finish_fixture(
    case: &Case,
    log: &mut fs::File,
    selected: &Selection,
    offset: u64,
    (folder, b_letter): (PathBuf, String),
    b_before: Vec<u8>,
    digest: String,
) -> Result<String, String> {
    let base = case.base;
    let label = case.label;
    let destination = offset
        .checked_add_signed(case.shift)
        .ok_or("Invalid fixture destination")?;
    let boot_offsets = boot_offsets(case, selected, offset)?;
    if boot_offsets
        .iter()
        .any(|&lba| lba != 0 && lba != offset / test_sector())
    {
        return Err("Original boot addresses disagree with the new test partition".into());
    }
    // Take the baseline after provisioning, without recording unrelated volumes.
    let raw = quick_partition_checks::table(A)?;
    let layout = table_restore::archive_layout(&raw)?;
    let entry = layout
        .entries
        .iter()
        .find(|e| e.offset == offset && e.size == case.size)
        .ok_or("Prepared A partition absent")?;
    let partition = selected.partition.ok_or("A partition number absent")?;
    let fixture = Fixture {
        desktop: option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
            .ok_or("Missing build digest")?
            .to_owned(),
        guid: entry.guid.clone(),
        old_offset: offset,
        partition,
        b_volume: folder.to_string_lossy().into_owned(),
        b_letter: b_letter.clone(),
        boot_offsets: Some(boot_offsets),
    };
    save_new(&format!("{base}.a.dspt"), &raw.encode()?)?;
    save_new(&format!("{base}.b.dspt"), &b_before)?;
    save_new(
        &format!("{base}.json"),
        &serde_json::to_vec(&fixture).map_err(err)?,
    )?;
    record(
        log,
        json!({"event":"prepared", "partition":partition, "sourceMiB":offset / MIB,
        "destinationMiB":destination / MIB,"sizeMiB":case.size / MIB,"sampleSha256":digest,
        "bootOffsets":fixture.boot_offsets,
        "journalDrive":b_letter,"journalDirectory":case.journal_folder}),
    )?;
    let relative = case
        .journal_folder
        .map_or(String::new(), |name| format!("{name}\\"));
    Ok(format!("PASS: A/{partition} {label} {} MiB, start {} MiB; move once to {} MiB.\nSave the journal as {b_letter}:\\{relative}{JOURNAL} on B.\nThe fixture is ready for native UI. No move was submitted.", case.size / MIB, offset / MIB, destination / MIB))
}

fn boot_offsets(case: &Case, selected: &Selection, offset: u64) -> Result<Vec<u64>, String> {
    let resolved = allocation_batch::resolved(selected, offset, case.size, false)?;
    let mut reader = data_windows::reader(&resolved)?;
    let sector = test_sector() as usize;
    let mut first = vec![0; sector];
    reader.read_exact(&mut first).map_err(err)?;
    let signature = |sector: &[u8]| sector[510..512] == [0x55, 0xaa];
    let hidden = |sector: &[u8]| u32::from_le_bytes(sector[28..32].try_into().unwrap()) as u64;
    if !signature(&first) {
        return Err("Missing filesystem boot signature".into());
    }
    match case.filesystem {
        FileSystem::Ntfs => {
            let mut backup = vec![0; sector];
            let backup_at = case.size - sector as u64;
            reader.seek(SeekFrom::Start(backup_at)).map_err(err)?;
            reader.read_exact(&mut backup).map_err(err)?;
            for data in [&first, &backup] {
                if !signature(data)
                    || &data[3..11] != b"NTFS    "
                    || u16::from_le_bytes(data[11..13].try_into().unwrap()) as usize != sector
                    || u64::from_le_bytes(data[40..48].try_into().unwrap())
                        != backup_at / sector as u64
                {
                    return Err(
                        "NTFS primary or last-sector backup boot geometry is invalid".into(),
                    );
                }
            }
            Ok(vec![hidden(&first), hidden(&backup)])
        }
        FileSystem::Exfat => {
            reader.seek(SeekFrom::Start(0)).map_err(err)?;
            let mut regions = vec![0; 24 * sector];
            reader.read_exact(&mut regions).map_err(err)?;
            let mut offsets = Vec::new();
            for region in regions.chunks_exact(12 * sector) {
                if !signature(region)
                    || &region[3..11] != b"EXFAT   "
                    || 1usize.checked_shl(region[108] as u32) != Some(sector)
                    || u64::from_le_bytes(region[72..80].try_into().unwrap())
                        != case.size / sector as u64
                {
                    return Err("exFAT main or backup boot geometry is invalid".into());
                }
                let (payload, checksum_sector) = region.split_at(11 * sector);
                let mut checksum = 0u32;
                for (index, &byte) in payload.iter().enumerate() {
                    if !matches!(index, 106 | 107 | 112) {
                        checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
                    }
                }
                if checksum_sector
                    .chunks_exact(4)
                    .any(|word| word != checksum.to_le_bytes())
                {
                    return Err("exFAT boot region checksum differs".into());
                }
                offsets.push(u64::from_le_bytes(region[64..72].try_into().unwrap()));
            }
            Ok(offsets)
        }
        FileSystem::Fat12 | FileSystem::Fat16 | FileSystem::Fat32 => {
            let word = |at| u64::from(u16::from_le_bytes(first[at..at + 2].try_into().unwrap()));
            let long = |at| u64::from(u32::from_le_bytes(first[at..at + 4].try_into().unwrap()));
            let reserved = word(14);
            let total = if word(19) != 0 { word(19) } else { long(32) };
            let fat32 = case.filesystem == FileSystem::Fat32;
            let fat_sectors = if fat32 { long(36) } else { word(22) };
            let root_sectors = (word(17) * 32).div_ceil(sector as u64);
            let cluster_sectors = u64::from(first[13]);
            if word(11) != sector as u64
                || !cluster_sectors.is_power_of_two()
                || reserved == 0
                || !matches!(first[16], 1 | 2)
                || fat_sectors == 0
                || total > case.size / sector as u64
            {
                return Err("Invalid FAT fixture boot geometry".into());
            }
            let clusters = total
                .checked_sub(reserved + u64::from(first[16]) * fat_sectors + root_sectors)
                .ok_or("FAT metadata consumes more than the volume")?
                / cluster_sectors;
            let kind = if clusters < 4085 {
                FileSystem::Fat12
            } else if clusters < 65525 {
                FileSystem::Fat16
            } else {
                FileSystem::Fat32
            };
            if clusters == 0 || kind != case.filesystem {
                return Err("Actual cluster count differs from the expected FAT variant".into());
            }
            if !fat32 {
                return Ok(vec![hidden(&first)]);
            }
            let backup_sector = word(50);
            if backup_sector == 0 || backup_sector >= reserved || word(22) != 0 || word(17) != 0 {
                return Err("Invalid FAT32 backup boot sector or reserved area".into());
            }
            let mut backup = vec![0; sector];
            reader
                .seek(SeekFrom::Start(backup_sector * sector as u64))
                .map_err(err)?;
            reader.read_exact(&mut backup).map_err(err)?;
            if !signature(&backup) || first[11..90] != backup[11..90] {
                return Err("FAT32 primary and backup BPB differ".into());
            }
            Ok(vec![hidden(&first), hidden(&backup)])
        }
    }
}

fn saved(path: &str, limit: u64) -> Result<Vec<u8>, String> {
    let meta = fs::metadata(path).map_err(err)?;
    if !meta.is_file() || meta.len() > limit {
        return Err("Unexpected acceptance log length".into());
    }
    fs::read(path).map_err(err)
}

pub fn diagnose_4kn_ntfs() -> Result<String, String> {
    let mut log = backup_protection::begin("partition-move-4kn-ntfs-diagnose")?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let case = &NTFS_4KN;
    if saved(&format!("{}.once", case.base), 64)? != b"partition move fixture"
        || Path::new(&format!("{}.json", case.base)).exists()
    {
        return Err("Expected the once-prepared NTFS fixture without a complete baseline".into());
    }
    let old = saved(
        &format!(
            r"C:\ProgramData\DiskStudio-{}-{FIRST_4KN_DRIVER}.jsonl",
            case.prepare_log
        ),
        16384,
    )?;
    let events = std::str::from_utf8(&old)
        .map_err(err)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [start, create, format] = events.as_slice() else {
        return Err("Unexpected previous preparation stages".into());
    };
    let (selected, offset) =
        allocation_batch::fixture_selection("NTFS", Some(case.label), case.size)?;
    if start["event"] != "start"
        || start["helper"] != FIRST_4KN_DRIVER
        || start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || create["event"] != "create-fixture-once"
        || create["filesystem"] != "NTFS"
        || create["offset"] != offset
        || create["size"] != case.size
        || format["event"] != "format-fixture-once"
        || format["filesystem"] != "NTFS"
        || format["partition"].as_u64() != selected.partition.map(u64::from)
    {
        return Err("The current NTFS fixture and prior preparation record disagree".into());
    }
    let folder = journal_folder(case, false)?;
    let b_before = quick_partition_checks::table(B)?.encode()?;
    if b_before
        != saved(
            &format!("{}.b.dspt", EXFAT_4KN.base),
            table_archive::MAX_FILE as u64,
        )?
    {
        return Err("4Kn B GPT differs from the accepted exFAT case".into());
    }
    let bound = allocation_batch::resolved(&selected, offset, case.size, true)?;
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(allocation_batch::root(&bound)?.join(SAMPLE))
        .map_err(err)?;
    let binding = data_windows::FileBinding::new(&file, &selected.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    let expected = bytes();
    let mut sample = vec![0; expected.len()];
    if file.metadata().map_err(err)?.len() != sample.len() as u64 {
        return Err("Windows sentinel size changed".into());
    }
    file.read_exact(&mut sample).map_err(err)?;
    if sample != expected {
        return Err("Windows sentinel contents changed".into());
    }
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(err(io::Error::last_os_error()));
    }
    let info = unsafe { info.assume_init() };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("Windows sentinel is a reparse point".into());
    }
    let reference = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let file_number = reference & ((1 << 48) - 1);
    let raw_bound = allocation_batch::resolved(&selected, offset, case.size, false)?;
    let mut reader = data_windows::reader(&raw_bound)?;
    let ntfs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    let file_record = ntfs.file(&mut reader, file_number);
    let record_description = match &file_record {
        Ok(raw) => format!(
            "sequence {}, expected {}; {:?}",
            raw.sequence_number(),
            reference >> 48,
            crate::ntfs_names::read(&mut reader, raw)
                .map(|names| names.into_iter().map(|n| n.display()).collect::<Vec<_>>())
        ),
        Err(error) => format!("{error}"),
    };
    let allocation =
        crate::ntfs_allocation::inspect(&mut reader, &AtomicBool::new(false)).map(|_| ());
    let mut raw_sizes = Vec::new();
    let scan = recovery::scan_filesystem(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |item| {
            if !item.deleted && item.name.eq_ignore_ascii_case(SAMPLE) {
                raw_sizes.push(item.size);
            }
        },
    );
    let raw_sample = check_file(&selected, offset, case.size, SAMPLE, &expected);
    data_windows::FileBinding::validate_all(&[binding])?;
    record(
        &mut log,
        json!({"event":"diagnosed", "partition":selected.partition,"nativeFileNumber":file_number,
        "recordSize":ntfs.file_record_size(),"record":record_description,"allocationError":allocation.as_ref().err(),
        "rawSampleError":raw_sample.as_ref().err(),"rawCandidateSizes":raw_sizes,"scan":scan,
        "nativeSampleMatched":true}),
    )?;
    match (allocation, raw_sample) {
        (Ok(()), Ok(digest)) => finish_fixture(case, &mut log, &selected, offset, folder, b_before, digest),
        (allocation, sample) => Ok(format!(
            "Read-only NTFS witness: Windows sample matched.\nMFT record {file_number}, size {}: {record_description}\nAllocation: {:?}\nRaw sample: {:?}, candidate sizes {raw_sizes:?}",
            ntfs.file_record_size(), allocation, sample)),
    }
}

fn load_fixture(case: &Case) -> Result<Fixture, String> {
    let fixture: Fixture =
        serde_json::from_slice(&saved(&format!("{}.json", case.base), 4096)?).map_err(err)?;
    if Some(fixture.desktop.as_str()) != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256") {
        return Err("Original fixture belongs to a different production build".into());
    }
    Ok(fixture)
}

fn journal(
    case: &Case,
    fixture: &Fixture,
) -> Result<(Journal<fs::File>, data_windows::FileBinding), String> {
    let b_dir = PathBuf::from(&fixture.b_volume);
    let (folder, who) = data_windows::folder_location(&b_dir)?;
    let b = selection(B, None)?;
    if who != b.target {
        return Err("Journal no longer resides on B".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder.join(JOURNAL))
        .map_err(err)?;
    use std::os::windows::fs::MetadataExt;
    let meta = file.metadata().map_err(err)?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("Journal has unexpected file attributes".into());
    }
    let binding = data_windows::FileBinding::new(&file, &b.target)?;
    let journal = Journal::open(file, |_| Err(io::Error::other("read-only inspection")))?;
    let range = journal.range();
    if range.source() != fixture.old_offset
        || Some(range.destination()) != fixture.old_offset.checked_add_signed(case.shift)
        || range.length() != case.size
        || range.capacity() != 16 * GIB
        || range.sector() != test_sector()
    {
        return Err("Journal range differs from the accepted test plan".into());
    }
    Ok((journal, binding))
}

pub fn inspect(case: &Case, partial: bool) -> Result<String, String> {
    let kind = if partial {
        case.interrupted_log
    } else {
        case.inspect_log
    };
    let mut log = backup_protection::begin(kind)?;
    let fixture = load_fixture(case)?;
    let disks = inventory()?;
    if test_sector() == 4096 {
        fat_sector_checks::confirm_4kn(&disks)?;
    }
    let a = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("A absent")?;
    if a.is_offline != Some(true) || a.is_read_only != Some(false) {
        return Err("The move did not leave A offline and writable".into());
    }
    let (journal, binding) = journal(case, &fixture)?;
    let range = journal.range();
    if partial && !(journal.copied_bytes() > 0 && !journal.copy_finished())
        || !partial && !journal.completed()
    {
        return Err(format!(
            "Unexpected checkpoint state: copied {} / {}, complete {}",
            journal.copied_bytes(),
            case.size,
            journal.completed()
        ));
    }
    verify_witness_layout(case, &fixture, (!partial).then_some(range.destination()))?;
    let part = selection(A, Some(fixture.partition))?;
    let (digest, final_boot_offsets) = if partial {
        (None, None)
    } else {
        let moved_offsets = boot_offsets(case, &part, range.destination())?;
        let original = fixture
            .boot_offsets
            .unwrap_or_else(|| vec![range.source() / test_sector()]);
        let expected: Vec<_> = original
            .into_iter()
            .map(|old| {
                if old == 0 {
                    0
                } else {
                    range.destination() / test_sector()
                }
            })
            .collect();
        if moved_offsets != expected {
            return Err(format!(
                "Moved volume boot addresses differ: expected {expected:?}, got {moved_offsets:?}"
            ));
        }
        let (name, expected) = case.sample();
        (
            Some(check_file(
                &part,
                range.destination(),
                case.size,
                name,
                &expected,
            )?),
            Some(moved_offsets),
        )
    };
    data_windows::FileBinding::validate_all(&[binding])?;
    record(
        &mut log,
        json!({"event":"passed","partial":partial,"bytesCopied":journal.copied_bytes(),
        "offline":true,"aGptVerified":true,"bGptUnchanged":true,"sampleSha256":digest,"bootOffsets":final_boot_offsets}),
    )?;
    Ok(format!(
        "PASS: A offline, B GPT preserved, {} / {} bytes checkpointed. {}",
        journal.copied_bytes(),
        case.size,
        if partial {
            "Original GPT still exact. Ready for restart and native resume."
        } else {
            "Moved boot addresses, exact GPT and complete sentinel verified."
        }
    ))
}

fn verify_witness_layout(
    case: &Case,
    fixture: &Fixture,
    destination: Option<u64>,
) -> Result<(), String> {
    let base = case.base;
    let before = saved(&format!("{base}.a.dspt"), table_archive::MAX_FILE as u64)?;
    let archive = table_archive::Archive::decode(&before)?;
    let mut expected = table_restore::archive_layout(&archive)?;
    let row = expected
        .entries
        .iter_mut()
        .find(|p| p.guid == fixture.guid && p.offset == fixture.old_offset && p.size == case.size)
        .ok_or("Baseline A GPT does not contain the authorized fixture")?;
    if let Some(offset) = destination {
        row.offset = offset;
    }
    let actual = quick_partition_checks::table(A)?;
    if (destination.is_none() && actual.encode()? != before)
        || !table_restore::archive_layout(&actual)?.agrees(&expected)
        || quick_partition_checks::table(B)?.encode()?
            != saved(&format!("{base}.b.dspt"), table_archive::MAX_FILE as u64)?
    {
        return Err("Independent GPT witness differs".into());
    }
    Ok(())
}

pub fn diagnose_restart() -> Result<String, String> {
    let mut log = backup_protection::begin("partition-move-restart-diagnose")?;
    let fixture = load_fixture(&FIRST)?;
    let (journal, binding) = journal(&FIRST, &fixture)?;
    if !journal.completed() {
        return Err("Expected the completed native checkpoint".into());
    }
    let sealed: serde_json::Value = serde_json::from_slice(journal.metadata()).map_err(err)?;
    let recorded: serde_json::Value =
        serde_json::from_str(sealed["plan"].as_str().ok_or("Missing native plan")?).map_err(err)?;
    let archive: table_archive::Archive =
        serde_json::from_value(recorded["table"].clone()).map_err(err)?;
    let original = table_restore::archive_layout(&archive)?;
    let source = selection(A, None)?;
    let (context, guid, protected) = partitions::platform::table_restore_context(&source, None)?;
    let d = &context.resolved.disk;
    let part = context
        .parts
        .iter()
        .find(|p| p.guid.as_deref().map(partitions::policy::guid).as_ref() == Some(&fixture.guid))
        .ok_or("Selected fixture absent")?;
    let mut file = data_windows::device(&context.resolved, false)?;
    let raw = quick_partition_checks::table(A)?;
    let native = extfs::native_layout(&file, &context.parts.iter().collect::<Vec<_>>())?;
    let native_target = extfs::check_native_data_target(&native, part).is_ok();
    let entry = table_restore::archive_layout(&raw)?
        .entries
        .into_iter()
        .find(|e| e.guid == fixture.guid)
        .ok_or("Raw fixture absent")?;
    let mut attributes = [0; 16];
    let count = data_windows::control(
        &file,
        windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_DISK_ATTRIBUTES,
        &[],
        &mut attributes,
    )?;
    if count < attributes.len() {
        return Err("Native disk attributes missing".into());
    }
    let native_flags = u64::from_le_bytes(attributes[8..16].try_into().unwrap());
    raw.matches(&mut file, || Ok(()))?;
    let flags = json!({"diskBinding":source.target.0 == archive.source,"guid":guid == original.guid,
        "sector":d.logical_sector_size == Some(journal.range().sector()),"size":d.size == journal.range().capacity(),
        "diskOffline":d.is_offline,"diskReadOnly":d.is_read_only,"nativeDiskFlags":native_flags,
        "partitionOffline":part.is_offline,"partitionReadOnly":part.is_read_only,"partitionHidden":part.is_hidden,
        "partitionBoot":part.is_boot,"partitionSystem":part.is_system,"partitionShadow":part.is_shadow_copy,
        "protectedByTableContext":protected.contains(&fixture.guid),"nativeTargetAllowed":native_target,"rawGptAttributes":entry.attributes});
    data_windows::FileBinding::validate_all(&[binding])?;
    record(&mut log, json!({"event":"diagnosed","flags":flags}))?;
    Ok(format!(
        "Read-only restart diagnostic:\n{}",
        serde_json::to_string_pretty(&flags).map_err(err)?
    ))
}

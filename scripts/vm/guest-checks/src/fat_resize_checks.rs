//! Guest-only integration with previously seeded A disk test volumes.
use super::*;
use allocation_batch::{check_file, fixture_selection, record, set_online, write_fixture};
use partitions::{Action, FileSystem, PartitionKind, Request};
use quick_partition_checks::table;
use serde_json::json;

const MIB: u64 = 1024 * 1024;
const FILE: &str = "DSRZSAMP.BIN";
const RELOCATION_FILE: &str = "DSRZMOVE.BIN";
const FIRST_MOVE_HELPER: &str = "a06099dd3d5061c4f1615aa7a4048ee112fe0214758b13d551d15024f99a46a1";
const CASES: [(&str, &str, u64, u64); 4] = [
    ("FAT12", "DSFAT12", 2, 1),
    ("FAT16", "DSFAT16", 128, 120),
    ("FAT32", "DSFAT32", 256, 248),
    ("exFAT", "DSexFAT", 512, 504),
];

pub(super) fn model(serial: &str) -> Result<table_restore::Layout, String> {
    table_restore::archive_layout(&table(serial)?)
}

fn check_offline() -> Result<(), String> {
    if !inventory()?.iter().any(|d| {
        d.serial_number.as_deref().map(disk_identity::clean) == Some(A)
            && d.is_offline == Some(true)
            && d.is_read_only == Some(false)
    }) {
        return Err("Resize did not leave exact A disk offline and writable".into());
    }
    Ok(())
}

// Independent boot-field read for the two new, bound guest fixtures.
fn capacity_geometry(
    selected: &Selection,
    offset: u64,
    size: u64,
    filesystem: FileSystem,
) -> Result<(u64, u64, u64), String> {
    use std::io::Read;
    let resolved = allocation_batch::resolved(selected, offset, size, false)?;
    let mut reader = data_windows::reader(&resolved)?;
    let mut boot = [0u8; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    let word = |i| u64::from(u16::from_le_bytes([boot[i], boot[i + 1]]));
    let dword = |i: usize| u64::from(u32::from_le_bytes(boot[i..i + 4].try_into().unwrap()));
    match filesystem {
        FileSystem::Fat12 if word(11) == 512 && boot[13] == 1 && boot[16] == 2 => {
            let fat = word(22);
            let heap = word(14) + 2 * fat + (word(17) * 32).div_ceil(512);
            Ok((fat, heap, 0))
        }
        FileSystem::Exfat if &boot[3..11] == b"EXFAT   " && boot[108] == 9 && boot[109] == 3 => {
            let bitmap = dword(92).div_ceil(8).div_ceil(4096);
            Ok((dword(84), dword(88), bitmap))
        }
        _ => Err("Guest fixture boot geometry disagrees with its confirmed format".into()),
    }
}

pub(super) fn create_capacity_fixture(
    log: &mut std::fs::File,
    (fs, label, cluster): (FileSystem, &str, u32),
    offset: u64,
    size: u64,
) -> Result<(Selection, table_restore::Layout), String> {
    inventory()?;
    set_online(A)?;
    let before = model(A)?;
    record(
        log,
        json!({"event":"create-fixture-once", "filesystem":fs.name(), "offset":offset, "size":size}),
    )?;
    operation(
        A,
        Action::Create {
            offset,
            size,
            kind: PartitionKind::Windows,
        },
    )?;
    let created = model(A)?;
    let added = created
        .entries
        .iter()
        .find(|entry| {
            entry.offset == offset
                && entry.size == size
                && entry.kind == partitions::policy::BASIC_DATA
        })
        .ok_or("New GPT fixture did not appear at the exact confirmed range")?;
    let mut unchanged = created.clone();
    unchanged.entries.retain(|entry| entry.guid != added.guid);
    if !unchanged.agrees(&before) {
        return Err("Creating the test fixture changed another GPT entry".into());
    }
    let disk = selection(A, None)?;
    let snap = storage::read_snapshot()?;
    let known = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&disk.target.0))
        .ok_or("Missing fixture disk")?;
    let mut partitions = known
        .partitions
        .iter()
        .filter(|p| p.base.start == offset && p.base.size == size);
    let number = partitions
        .next()
        .ok_or("Windows did not report the new test partition")?
        .number;
    if partitions.next().is_some() {
        return Err("Windows reports ambiguous new partition identity".into());
    }
    record(
        log,
        json!({"event":"format-fixture-once","filesystem":fs.name(),"partition":number}),
    )?;
    operation(
        A,
        Action::Format {
            partition: number,
            label: label.into(),
            file_system: fs,
            cluster_size: cluster,
            options: Default::default(),
        },
    )?;
    let (selected, actual_offset) = fixture_selection(fs.name(), Some(label), size)?;
    if actual_offset != offset || selected.partition != Some(number) || !model(A)?.agrees(&created)
    {
        return Err("Formatted fixture geometry or GPT identity changed".into());
    }
    Ok((selected, created))
}

pub fn check_capacity() -> Result<String, String> {
    run_capacity(None)
}

pub fn continue_capacity() -> Result<String, String> {
    run_capacity(Some(capacity_stop()?))
}

fn capacity_stop() -> Result<u64, String> {
    const FIRST: &str = "1bbedfb005a0d73ae4f8739586699dcf2809c668933cb8b8cb11ac49445c99de";
    inventory()?;
    let path = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-fat-capacity-{FIRST}.jsonl"
    ));
    if std::fs::metadata(&path).map_err(err)?.len() > 8192 {
        return Err("Unexpected capacity run record size".into());
    }
    let prior = std::fs::read_to_string(path).map_err(err)?;
    let events = prior
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [start, create, format, grow, verified, geometry, stop] = events.as_slice() else {
        return Err("Prior capacity run did not stop after FAT12 growth".into());
    };
    if start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || start["helper"] != FIRST
        || start["event"] != "start"
        || create["event"] != "create-fixture-once"
        || create["filesystem"] != "FAT12"
        || format["event"] != "format-fixture-once"
        || format["partition"] != 20
        || grow["event"] != "confirmed-once"
        || grow["beforeSize"] != MIB
        || grow["afterSize"] != 2 * MIB
        || verified["event"] != "resize-verified"
        || verified["partition"] != 20
        || geometry["event"] != "larger-geometry-verified"
        || geometry["fatSectors"] != json!([6, 12])
        || stop["event"] != "failed"
        || !stop["error"]
            .as_str()
            .is_some_and(|error| error.contains("os error 3"))
    {
        return Err("Prior capacity run identity or stop reason differs".into());
    }
    create["offset"]
        .as_u64()
        .filter(|&value| value == 12234784768)
        .ok_or_else(|| "Missing bound FAT12 fixture offset".into())
}

fn diagnose_fat12_tail(offset: u64) -> Result<Vec<(String, u64)>, String> {
    let selected = selection(A, Some(20))?;
    let size = 2 * MIB;
    if capacity_geometry(&selected, offset, size, FileSystem::Fat12)? != (12, 57, 0) {
        return Err("FAT12 expanded geometry differs from the verified run".into());
    }
    let bound = allocation_batch::resolved(&selected, offset, size, false)?;
    let mut reader = data_windows::reader(&bound)?;
    let mut found = Vec::new();
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| {
            if [FILE, RELOCATION_FILE, "DSRZFILL.TMP"]
                .iter()
                .any(|name| file.name.eq_ignore_ascii_case(name))
            {
                found.push((file.name, file.size));
            }
        },
        |_| Ok(()),
    )?;
    Ok(found)
}

pub fn check_capacity_fat12_tail() -> Result<String, String> {
    const EXFAT_HELPER: &str = "3ddfc4dfde71c840f9e8541f510e6d663040c1696de8e872ac5263e18d18eb56";
    const FILL: &str = "DSRZFILL.TMP";
    const FILLED: u64 = 5 * MIB / 4;
    let offset = capacity_stop()?;
    let completed = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-fat-capacity-remaining-{EXFAT_HELPER}.jsonl"
    ));
    if std::fs::metadata(&completed).map_err(err)?.len() > 8192 {
        return Err("Unexpected exFAT run record size".into());
    }
    let prior = std::fs::read_to_string(completed).map_err(err)?;
    let events = prior
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let start = events.first().ok_or("Missing exFAT start record")?;
    if start["helper"] != EXFAT_HELPER
        || start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || events.last().is_none_or(|event| event["event"] != "passed")
    {
        return Err("The independent exFAT run has not completed successfully".into());
    }
    let mut log = backup_protection::begin("fat-capacity-fat12-tail")?;
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(r"C:\ProgramData\DiskStudio-fat-capacity-fat12-tail-20261117.once")
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        let initial_a = model(A)?;
        let initial_b = table(B)?.encode()?;
        if initial_a.guid == model(B)?.guid {
            return Err("A and B share a disk GUID".into());
        }
        let entry = initial_a
            .entries
            .iter()
            .find(|entry| {
                entry.offset == offset
                    && entry.size == 2 * MIB
                    && entry.kind == partitions::policy::BASIC_DATA
            })
            .ok_or("Bound FAT12 GPT entry is absent")?;
        let mut shrunk = initial_a.clone();
        shrunk
            .entries
            .iter_mut()
            .find(|candidate| candidate.guid == entry.guid)
            .ok_or("Missing cloned FAT12 GPT entry")?
            .size = MIB;
        set_online(A)?;
        let (selected, actual) = fixture_selection("FAT12", Some("DSCAP12"), 2 * MIB)?;
        if selected.partition != Some(20) || actual != offset {
            return Err("FAT12 fixture identity changed".into());
        }
        let found = diagnose_fat12_tail(offset)?;
        let bytes = (0..65_553)
            .map(|i| ((i * 67 + i / 191 + 512) % 251) as u8)
            .collect::<Vec<_>>();
        if found.len() != 3
            || !found
                .iter()
                .any(|(name, size)| name.eq_ignore_ascii_case(FILL) && *size == FILLED)
        {
            return Err("The expected retained filler is missing or has changed length".into());
        }
        check_file(&selected, offset, 2 * MIB, FILE, &bytes)?;
        check_file(&selected, offset, 2 * MIB, RELOCATION_FILE, &bytes)?;
        // The fixture reserved space with SetEndOfFile, which does not
        // specify contents when a file is extended.
        let bound = allocation_batch::resolved(&selected, offset, 2 * MIB, true)?;
        {
            use cap_std::fs::OpenOptionsExt;
            let directory = cap_std::fs::Dir::open_ambient_dir(
                allocation_batch::root(&bound)?,
                cap_std::ambient_authority(),
            )
            .map_err(err)?;
            let parent = directory.try_clone().map_err(err)?.into_std_file();
            let mut opts = cap_std::fs::OpenOptions::new();
            opts.read(true)
                .write(true)
                .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
            let filler = directory.open_with(FILL, &opts).map_err(err)?.into_std();
            let bindings = [
                data_windows::FileBinding::new(&parent, &selected.target)?,
                data_windows::FileBinding::new(&filler, &selected.target)?,
            ];
            data_windows::FileBinding::validate_all(&bindings)?;
            if filler.metadata().map_err(err)?.len() != FILLED {
                return Err("Filler length changed after resolution".into());
            }
            // Release only the allocation of the already verified fixture.
            // Keep its directory entry so the old failed path removal stays historical.
            record(
                &mut log,
                json!({"event":"shorten-fixture-once", "partition":20, "oldBytes":FILLED}),
            )?;
            filler.set_len(0).map_err(err)?;
            filler.sync_all().map_err(err)?;
            if filler.metadata().map_err(err)?.len() != 0 {
                return Err("Filler length did not become zero".into());
            }
            data_windows::FileBinding::validate_all(&bindings)?;
        }
        resize(
            &mut log,
            "FAT12",
            &selected,
            offset,
            2 * MIB,
            MIB,
            RELOCATION_FILE,
            &bytes,
            Some(true),
            &shrunk,
        )?;
        let final_selection = selection(A, Some(20))?;
        let original_digest = check_file(&final_selection, offset, MIB, FILE, &bytes)?;
        if capacity_geometry(&final_selection, offset, MIB, FileSystem::Fat12)? != (12, 57, 0)
            || table(B)?.encode()? != initial_b
        {
            return Err("Final FAT12 geometry or untouched B GPT differs".into());
        }
        record(
            &mut log,
            json!({"event":"original-file-verified", "sha256":original_digest}),
        )?;
        Ok("PASS: first FAT12 shrink moved the retained tail sample, preserved both payloads, resized A/20 from 2 MiB to 1 MiB, and left A offline. B GPT unchanged.".into())
    })();
    record(
        &mut log,
        json!({"event":if result.is_ok() {"passed"} else {"failed"},
        "result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

fn run_capacity(continuation: Option<u64>) -> Result<String, String> {
    let (kind, marker) = if continuation.is_some() {
        (
            "fat-capacity-remaining",
            r"C:\ProgramData\DiskStudio-fat-capacity-remaining-20261117.once",
        )
    } else {
        (
            "fat-capacity",
            r"C:\ProgramData\DiskStudio-fat-capacity-20261117.once",
        )
    };
    let mut log = backup_protection::begin(kind)?;
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        let initial_b = table(B)?.encode()?;
        if model(A)?.guid == model(B)?.guid {
            return Err("A and B share a disk GUID".into());
        }
        set_online(A)?;
        if let Some(offset) = continuation {
            let diagnostic = diagnose_fat12_tail(offset);
            record(
                &mut log,
                json!({"event":"fat12-read-only-diagnostic", "files":diagnostic.as_ref().ok(), "error":diagnostic.as_ref().err()}),
            )?;
        }
        let target = partitions::status()
            .targets
            .into_iter()
            .find(|t| t.serial == A && t.available)
            .ok_or("Exact A target unavailable")?;
        let start = target
            .free_ranges
            .iter()
            .find(|range| range.size >= 1092 * MIB)
            .ok_or("No isolated A gap of at least 1092 MiB for the new fixtures")?
            .offset;
        for (fs, label, cluster, offset, small, large) in [
            (FileSystem::Fat12, "DSCAP12", 512, start, MIB, 2 * MIB),
            (
                FileSystem::Exfat,
                "DSCAPEX",
                4096,
                start + 4 * MIB,
                96 * MIB,
                1088 * MIB,
            ),
        ]
        .into_iter()
        .skip(usize::from(continuation.is_some()))
        {
            let (selected, original) =
                create_capacity_fixture(&mut log, (fs, label, cluster), offset, small)?;
            let number = selected
                .partition
                .ok_or("Missing owned fixture partition")?;
            let data = (0..65_553)
                .map(|i| ((i * 67 + i / 191 + cluster as usize) % 251) as u8)
                .collect::<Vec<_>>();
            write_fixture(&selected, offset, small, FILE, &data, true)?;
            let before = capacity_geometry(&selected, offset, small, fs)?;
            let mut larger = original.clone();
            larger
                .entries
                .iter_mut()
                .find(|entry| entry.offset == offset && entry.size == small)
                .ok_or("Created fixture missing from GPT")?
                .size = large;
            resize(
                &mut log,
                fs.name(),
                &selected,
                offset,
                small,
                large,
                FILE,
                &data,
                (fs == FileSystem::Fat12).then_some(true),
                &larger,
            )?;
            let now = selection(A, Some(number))?;
            let grown = capacity_geometry(&now, offset, large, fs)?;
            if grown.0 <= before.0
                || (fs == FileSystem::Fat12 && grown.1 <= before.1)
                || (fs == FileSystem::Exfat && grown.2 <= before.2)
            {
                return Err(
                    "Guest growth did not expand the expected FAT or bitmap geometry".into(),
                );
            }
            record(
                &mut log,
                json!({"event":"larger-geometry-verified", "filesystem":fs.name(),
                "fatSectors": [before.0, grown.0], "heapSector": [before.1, grown.1],
                "bitmapClusters": [before.2, grown.2]}),
            )?;
            set_online(A)?;
            let (after, recovered_offset) = fixture_selection(fs.name(), Some(label), large)?;
            if after.target != selected.target
                || after.partition != Some(number)
                || recovered_offset != offset
            {
                return Err("Windows did not rediscover the expanded fixture".into());
            }
            let (file, relocation) = if fs == FileSystem::Fat12 {
                seed_tail(&after, offset, large, small, &data)?;
                (RELOCATION_FILE, Some(true))
            } else {
                (FILE, None)
            };
            resize(
                &mut log,
                fs.name(),
                &after,
                offset,
                large,
                small,
                file,
                &data,
                relocation,
                &original,
            )?;
            let final_selection = selection(A, Some(number))?;
            check_file(&final_selection, offset, small, FILE, &data)?;
            let shrunk = capacity_geometry(&final_selection, offset, small, fs)?;
            if shrunk.0 != grown.0 || shrunk.1 != grown.1 || shrunk.2 != before.2 {
                return Err("Shrunk fixture has an unexpected FAT, heap or bitmap size".into());
            }
            if table(B)?.encode()? != initial_b {
                return Err("B GPT changed during FAT capacity acceptance".into());
            }
            record(
                &mut log,
                json!({"event":"smaller-geometry-verified", "filesystem":fs.name(),
                "fatSectors":shrunk.0,"heapSector":shrunk.1,"bitmapClusters":shrunk.2}),
            )?;
        }
        let tested = if continuation.is_some() {
            "exFAT"
        } else {
            "FAT12 and exFAT"
        };
        Ok(format!("PASS: {tested} grew beyond original FAT capacity and then shrank. exFAT added and released bitmap clusters. Samples stayed intact on A, unrelated A and all B GPT entries were preserved, and A ends offline."))
    })();
    record(
        &mut log,
        json!({"event":if result.is_ok() {"passed"} else {"failed"},
        "result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

fn seed_tail(
    selection: &Selection,
    offset: u64,
    size: u64,
    shrink_to: u64,
    bytes: &[u8],
) -> Result<(), String> {
    const FILL: &str = "DSRZFILL.TMP";
    let resolved = allocation_batch::resolved(selection, offset, size, true)?;
    let directory = cap_std::fs::Dir::open_ambient_dir(
        allocation_batch::root(&resolved)?,
        cap_std::ambient_authority(),
    )
    .map_err(err)?;
    let parent = directory.try_clone().map_err(err)?.into_std_file();
    let binding = data_windows::FileBinding::new(&parent, &selection.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    let filler = directory.open_with(FILL, &options).map_err(err)?.into_std();
    let file_binding = data_windows::FileBinding::new(&filler, &selection.target)?;
    data_windows::FileBinding::validate_all(&[binding, file_binding])?;
    filler
        .set_len(shrink_to + (size - shrink_to) / 4)
        .map_err(err)?;
    filler.sync_all().map_err(err)?;
    write_fixture(selection, offset, size, RELOCATION_FILE, bytes, true)?;
    let binding = data_windows::FileBinding::new(&parent, &selection.target)?;
    let file_binding = data_windows::FileBinding::new(&filler, &selection.target)?;
    data_windows::FileBinding::validate_all(&[binding, file_binding])?;
    // Release the allocation through the already validated handle. This also
    // works on drive-letterless volumes whose path cannot be reopened for deletion.
    filler.set_len(0).map_err(err)?;
    filler.sync_all().map_err(err)?;
    Ok(())
}

pub(super) fn resize(
    log: &mut std::fs::File,
    filesystem: &str,
    selected: &Selection,
    offset: u64,
    before_size: u64,
    size: u64,
    sample: &str,
    bytes: &[u8],
    expected_movement: Option<bool>,
    expected: &table_restore::Layout,
) -> Result<(), String> {
    inventory()?;
    let number = selected.partition.ok_or("Missing target partition")?;
    let status = partitions::status()
        .targets
        .into_iter()
        .find(|t| t.serial == A && t.target == selected.target && t.available)
        .ok_or("Exact A target unavailable")?;
    if !status.fat_resize_partitions.contains(&number) || status.layout_key != selected.layout_key {
        return Err("A partition lacks bound FAT resize capability".into());
    }
    let plan = partitions::prepare(Request {
        target: status.target,
        revision: status.revision,
        action: Action::FatResize {
            partition: number,
            size,
        },
    })?;
    if plan.serial != A
        || plan.before_size != Some(before_size)
        || plan.after_size != Some(size)
        || !plan.warning.contains(&format!("{filesystem}："))
    {
        return Err("Production preview binding differs".into());
    }
    let moves = plan
        .warning
        .split_once("预计搬移 ")
        .and_then(|(_, suffix)| suffix.split_once(" 个簇"))
        .and_then(|(amount, _)| amount.parse::<usize>().ok())
        .ok_or("Production preview did not report a relocation count")?;
    if expected_movement.is_some_and(|expected| expected != (moves > 0)) {
        return Err("The isolated sentinel did not produce the expected cluster movement".into());
    }
    record(
        log,
        json!({
            "event":"confirmed-once", "filesystem":filesystem, "partition":number,
            "offset":offset, "beforeSize":before_size, "afterSize":size,
            "movedClusters":moves,
        }),
    )?;
    let outcome = partitions::execute(plan.token)?;
    if !outcome.verified {
        return Err(format!("Resize result unverified: {}", outcome.message));
    }
    check_offline()?;
    if !model(A)?.agrees(expected) {
        return Err("Independent A GPT check differs from expected state".into());
    }
    let fresh = selection(A, Some(number))?;
    if fresh.target != selected.target {
        return Err("Fresh independent selection identity changed".into());
    }
    let digest = check_file(&fresh, offset, size, sample, bytes)?;
    record(
        log,
        json!({
            "event":"resize-verified", "filesystem":filesystem, "partition":number,
            "size":size, "sha256":digest, "sentinelBytes":bytes.len(), "offline":true,
            "movedClusters":moves,
        }),
    )
}

pub fn check() -> Result<String, String> {
    run(false, 0)
}

pub fn check_relocation() -> Result<String, String> {
    run(true, 0)
}

pub fn continue_relocation() -> Result<String, String> {
    inventory()?;
    let path = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-fat-relocation-{FIRST_MOVE_HELPER}.jsonl"
    ));
    if std::fs::metadata(&path).map_err(err)?.len() > 4096 {
        return Err("Unexpected prior run record size".into());
    }
    let prior = std::fs::read_to_string(path).map_err(err)?;
    let events = prior
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [start, seed, stop] = events.as_slice() else {
        return Err("Prior run did not stop during FAT12 fixture preparation".into());
    };
    if start["event"] != "start"
        || start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || start["helper"] != FIRST_MOVE_HELPER
        || seed["event"] != "fixture-seed"
        || seed["filesystem"] != "FAT12"
        || seed["partition"] != 17
        || stop["event"] != "failed"
        || !stop["error"]
            .as_str()
            .is_some_and(|message| message.contains("os error 112"))
    {
        return Err("Prior run stop or source identity differs".into());
    }
    run(true, 1)
}

fn run(relocation: bool, first_case: usize) -> Result<String, String> {
    let (kind, marker, sample) = if first_case == 1 {
        (
            "fat-relocation-remaining",
            r"C:\ProgramData\DiskStudio-fat-relocation-remaining-20261117.once",
            RELOCATION_FILE,
        )
    } else if relocation {
        (
            "fat-relocation",
            r"C:\ProgramData\DiskStudio-fat-relocation-20261117.once",
            RELOCATION_FILE,
        )
    } else {
        (
            "fat-resize",
            r"C:\ProgramData\DiskStudio-fat-resize-20261117.once",
            FILE,
        )
    };
    let mut log = backup_protection::begin(kind)?;
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        let base_a = model(A)?;
        let base_b = table(B)?.encode()?;
        if base_a.guid == model(B)?.guid {
            return Err("A and B share a disk GUID; refuse online transition".into());
        }
        set_online(A)?;
        let cases = CASES
            .iter()
            .skip(first_case)
            .map(|&(filesystem, label, old, new)| {
                let (selection, offset) = fixture_selection(filesystem, Some(label), old * MIB)?;
                let matching = base_a
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.offset == offset
                            && entry.size == old * MIB
                            && entry.kind == partitions::policy::BASIC_DATA
                    })
                    .collect::<Vec<_>>();
                if matching.len() != 1 {
                    return Err("Owned A fixture missing from baseline GPT".into());
                }
                Ok((
                    selection,
                    offset,
                    matching[0].guid.clone(),
                    filesystem,
                    old * MIB,
                    new * MIB,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        for (index, (selected, offset, guid, filesystem, old_size, new_size)) in
            cases.iter().enumerate()
        {
            let index = index + first_case;
            set_online(A)?;
            let number = selected.partition.ok_or("Missing fixture partition")?;
            let fresh = selection(A, Some(number))?;
            if fresh.target != selected.target || fresh.layout_key != selected.layout_key {
                return Err("Baseline A fixture changed before seeding".into());
            }
            let bytes = (0..65_553)
                .map(|i| ((i * 47 + i / 137 + index) % 251) as u8)
                .collect::<Vec<_>>();
            record(
                &mut log,
                json!({"event":"fixture-seed","filesystem":filesystem,"partition":number}),
            )?;
            if relocation {
                seed_tail(&fresh, *offset, *old_size, *new_size, &bytes)?;
            } else {
                write_fixture(&fresh, *offset, *old_size, sample, &bytes, true)?;
            }
            check_file(&fresh, *offset, *old_size, sample, &bytes)?;
            let mut shrunk = base_a.clone();
            shrunk
                .entries
                .iter_mut()
                .find(|entry| &entry.guid == guid)
                .ok_or("Missing target GUID")?
                .size = *new_size;
            resize(
                &mut log,
                filesystem,
                &fresh,
                *offset,
                *old_size,
                *new_size,
                sample,
                &bytes,
                relocation.then_some(true),
                &shrunk,
            )?;
            // Online is a separate explicit operation to check actual Windows
            // recognition and enable the next enlargement. No resize retries.
            set_online(A)?;
            let (after, new_offset) =
                fixture_selection(filesystem, Some(CASES[index].1), *new_size)?;
            if after.target != selected.target
                || after.partition != Some(number)
                || new_offset != *offset
            {
                return Err(
                    "Windows did not recognize the resized fixture at its original start".into(),
                );
            }
            resize(
                &mut log,
                filesystem,
                &after,
                *offset,
                *new_size,
                *old_size,
                sample,
                &bytes,
                relocation.then_some(false),
                &base_a,
            )?;
            if table(B)?.encode()? != base_b {
                return Err("Independent B GPT comparison failed".into());
            }
        }
        let tested = CASES[first_case..]
            .iter()
            .map(|case| case.0)
            .collect::<Vec<_>>()
            .join("/");
        let moves = if relocation {
            " All shrinks moved allocated tail clusters."
        } else {
            ""
        };
        Ok(format!("PASS: {tested} each shrank and regrew in place.{moves} All {} GPT extent checks, Windows rediscovery after shrink and independent 65,553-byte file comparisons passed. All unrelated A GPT fields and complete B GPT preserved. A stays offline.", 2 * cases.len()))
    })();
    record(
        &mut log,
        json!({
            "event":if result.is_ok() {"passed"} else {"failed"},
            "result":result.as_ref().ok(), "error":result.as_ref().err(),
        }),
    )?;
    result
}

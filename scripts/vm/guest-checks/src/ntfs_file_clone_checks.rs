//! Native NTFS file clone acceptance on the two guarded fixture disks.
use super::*;
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use disk_core::MIB;
use partitions::{Action, DriveLetter, FileSystem, PartitionKind};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

#[path = "ntfs_file_clone_source_lock.rs"]
pub(super) mod source_lock;

#[path = "ntfs_file_clone_4kn.rs"]
pub(super) mod four_kn;

const SOURCE_AT: u64 = 15 * GIB + 128 * MIB;
const TARGET_FIRST: u64 = 13 * GIB + 896 * MIB;
const TARGET_END: u64 = 16 * GIB - MIB;
const SOURCE_BYTES: u64 = 96 * MIB;
const TARGET_BYTES: u64 = 48 * MIB;
const FOLDER: &str = "DS-FileCloneNtfs";
const TARGET_LABEL: &str = "NTDEST";
const PRIMARY: &str = "原始文件.bin";
const ALIAS: &str = "链接.bin";
const STREAM: &str = "样本流";
const METADATA_SAMPLE: &str = "metadata-source-windows.bin";
const TEST_EA_NAME: &[u8] = b"DISKSTUDIO.CHECK";
const TEST_EA_SIZE: usize = 4199;

#[derive(serde::Serialize, Deserialize)]
struct Witness {
    desktop: String,
    source: u32,
    target: u32,
    target_at: u64,
    source_digest: String,
    source_edges: [String; 2],
    target_edges: [String; 2],
    layouts: [String; 3],
}

fn raw(serial: &str) -> Result<fs::File, String> {
    inventory()?;
    let resolved = data_windows::resolve(&selection(serial, None)?, false, false)?;
    if resolved
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(serial)
        || resolved.disk.logical_sector_size != Some(test_sector())
        || resolved.disk.size != 16 * GIB
    {
        return Err("NTFS fixture disk identity changed".into());
    }
    data_windows::device(&resolved, false)
}

fn edges(serial: &str, at: u64, size: u64) -> Result<[String; 2], String> {
    let mut file = raw(serial)?;
    let sector = test_sector();
    let mut values = [String::new(), String::new()];
    for (value, offset) in values.iter_mut().zip([at - sector, at + size]) {
        *value = format!(
            "{:x}",
            Sha256::digest(table_archive::read_at(
                &mut file,
                offset,
                sector as usize,
                16 * GIB
            )?)
        );
    }
    Ok(values)
}

fn available(layout: &table_restore::Layout, at: u64, size: u64) -> bool {
    let sector = test_sector();
    (at - sector) / sector >= layout.first
        && (at + size) / sector <= layout.last
        && !layout
            .entries
            .iter()
            .any(|part| part.offset < at + size + sector && at - sector < part.offset + part.size)
}

fn first_nonzero(file: &mut fs::File, at: u64, size: u64) -> Result<Option<u64>, String> {
    file.seek(SeekFrom::Start(at)).map_err(err)?;
    let mut block = vec![0u8; MIB as usize];
    for index in 0..size / MIB {
        file.read_exact(&mut block).map_err(err)?;
        if block.iter().any(|&value| value != 0) {
            return Ok(Some(at + index * MIB));
        }
    }
    Ok(None)
}

fn target_window(layout: &table_restore::Layout) -> Result<u64, String> {
    blank_window(B, layout, TARGET_FIRST, TARGET_END, TARGET_BYTES)
}

fn blank_window(
    serial: &str,
    layout: &table_restore::Layout,
    mut at: u64,
    end: u64,
    size: u64,
) -> Result<u64, String> {
    let mut file = raw(serial)?;
    while at + size <= end {
        if available(layout, at, size) {
            match first_nonzero(&mut file, at, size)? {
                None => return Ok(at),
                Some(used) => at = used,
            }
        }
        at += MIB;
    }
    Err(format!(
        "No blank {} MiB window fits on {serial}",
        size / MIB
    ))
}

fn prepared_target() -> Result<storage::LocalPartition, String> {
    let selected = selection(B, None)?;
    let mut found = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|disk| disk.hardware.identity.as_deref() == Some(selected.target.0.as_str()))
        .ok_or("Bound B disk is missing")?
        .partitions
        .into_iter()
        .filter(|part| part.volume_label.as_deref() == Some(TARGET_LABEL));
    let target = found.next().ok_or("The prepared B volume is missing")?;
    if found.next().is_some()
        || target.base.system
        || target.base.size != TARGET_BYTES
        || !(TARGET_FIRST..=TARGET_END - TARGET_BYTES).contains(&target.base.start)
        || target.base.start % MIB != 0
    {
        return Err("The prepared B volume is ambiguous or outside the fixture range".into());
    }
    Ok(target)
}

fn confirm_prepared_target(
    target: &data_windows::Resolved,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut reader = data_windows::reader(target)?;
    let fat = fat_recovery::resize::file_clone::prepare(
        &mut reader,
        test_sector(),
        TARGET_BYTES,
        target.offset,
        12,
        cancel,
    )?;
    if fat.source_format != "FAT16" || fat.files > 2 || fat.directories > 1 {
        return Err("The prepared B volume is no longer a fresh FAT16 fixture".into());
    }
    let mut unexpected_file = false;
    let mut unexpected_directory = false;
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| {
            unexpected_file |=
                !matches!(file.name.as_str(), "WPSettings.dat" | "IndexerVolumeGuid");
        },
        |dir| {
            unexpected_directory |=
                dir.id != recovery_tree::ROOT && dir.name != "System Volume Information";
            Ok(())
        },
    )?;
    if unexpected_file || unexpected_directory {
        return Err("The prepared B volume contains unexpected files".into());
    }
    Ok(())
}

fn resolved(
    serial: &str,
    number: u32,
    at: u64,
    size: u64,
    online: bool,
) -> Result<(Selection, data_windows::Resolved), String> {
    inventory()?;
    let selected = selection(serial, Some(number))?;
    let part = data_windows::resolve(&selected, online, false)?;
    if part.disk.serial_number.as_deref().map(disk_identity::clean) != Some(serial)
        || part.offset != at
        || part.size != size
        || part.disk.logical_sector_size != Some(test_sector())
        || part.disk.is_boot != Some(false)
        || part.disk.is_system != Some(false)
        || part.disk.is_offline != Some(!online)
    {
        return Err(format!("The NTFS fixture partition on {serial} changed"));
    }
    Ok((selected, part))
}

fn create(
    serial: &str,
    at: u64,
    size: u64,
    fs: FileSystem,
    label: &str,
    cluster: u32,
    short_names: bool,
) -> Result<u32, String> {
    allocation_batch::set_online(serial)?;
    operation(
        serial,
        Action::Create {
            offset: at,
            size,
            kind: PartitionKind::Windows,
        },
    )?;
    let number =
        reallocation_checks::metadata_partition(serial, None, at / MIB, size / MIB)?.number;
    operation(
        serial,
        Action::Format {
            partition: number,
            file_system: fs,
            label: label.into(),
            cluster_size: cluster,
            options: partitions::format_options::FormatOptions {
                ntfs_short_names: short_names,
                ..Default::default()
            },
        },
    )?;
    Ok(number)
}

fn contents() -> Vec<u8> {
    (0..65_553)
        .map(|i| ((i * 17 + i / 159 + 9) % 253) as u8)
        .collect()
}

fn seed(
    selected: &Selection,
    source: &data_windows::Resolved,
    drive_letter: Option<&str>,
) -> Result<(), String> {
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected one A fixture volume".into());
    };
    let path = PathBuf::from(match drive_letter {
        Some(letter) if letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic() => {
            format!("{letter}:\\")
        }
        Some(_) => return Err("Invalid source drive letter".into()),
        None => format!("{}\\", data_windows::volume_root(volume)?),
    });
    let confirm = |file: &fs::File| {
        data_windows::file_location(file)?.confirm_partition(
            &selected.target,
            &selected.layout_key,
            volume,
            (source.disk.number, source.offset, source.size),
        )
    };
    if drive_letter.is_none() {
        return seed_by_volume(&path, confirm);
    }
    let root = Dir::open_ambient_dir(&path, cap_std::ambient_authority())
        .map_err(|error| format!("Open the exact source root: {error}"))?;
    confirm(&root.try_clone().map_err(err)?.into_std_file())?;
    root.create_dir("资料")
        .map_err(|error| format!("Create the fixture directory: {error}"))?;
    let folder = root.open_dir("资料").map_err(err)?;
    confirm(&folder.try_clone().map_err(err)?.into_std_file())?;
    folder.create_dir("空目录").map_err(err)?;
    let mut file = folder
        .open_with(PRIMARY, CapOptions::new().create_new(true).write(true))
        .map_err(err)?
        .into_std();
    confirm(&file)?;
    file.write_all(&contents()).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    folder.hard_link(PRIMARY, &root, ALIAS).map_err(err)?;
    seed_stream(&path.join("资料"), confirm)
}

fn seed_stream(
    directory: &Path,
    confirm: impl Fn(&fs::File) -> Result<(), String>,
) -> Result<(), String> {
    let mut stream = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(directory.join(format!("{PRIMARY}:{STREAM}:$DATA")))
        .map_err(err)?;
    confirm(&stream)?;
    stream.write_all(b"NTFS clone named stream").map_err(err)?;
    stream.sync_all().map_err(err)
}

fn seed_by_volume(
    path: &Path,
    confirm: impl Fn(&fs::File) -> Result<(), String>,
) -> Result<(), String> {
    let open_dir = |path: &Path| {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&file)?;
        Ok::<_, String>(file)
    };
    let _root = open_dir(path)?;
    let directory = path.join("资料");
    fs::create_dir(&directory)
        .map_err(|error| format!("Create the volume fixture directory: {error}"))?;
    let _directory = open_dir(&directory)?;
    fs::create_dir(directory.join("空目录")).map_err(err)?;
    let create = |path: &Path| {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&file)?;
        Ok::<_, String>(file)
    };
    let primary = directory.join(PRIMARY);
    let mut file = create(&primary)?;
    file.write_all(&contents()).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    fs::hard_link(primary, path.join(ALIAS)).map_err(err)?;
    seed_stream(&directory, &confirm)?;
    let mut sample = create(&directory.join(METADATA_SAMPLE))?;
    sample
        .write_all(b"NTFS short name and EA fixture")
        .map_err(err)?;
    seed_ea(&sample)?;
    sample.sync_all().map_err(err)
}

fn seed_ea(file: &fs::File) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::{Wdk::Storage::FileSystem::NtSetEaFile, Win32::System::IO::IO_STATUS_BLOCK};
    let mut ea = vec![0; 8];
    ea[5] = TEST_EA_NAME.len() as u8;
    ea[6..8].copy_from_slice(&(TEST_EA_SIZE as u16).to_le_bytes());
    ea.extend_from_slice(TEST_EA_NAME);
    ea.push(0);
    ea.resize(ea.len() + TEST_EA_SIZE, 0xa5);
    let aligned = ea
        .chunks(4)
        .map(|chunk| {
            let mut bytes = [0; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            u32::from_le_bytes(bytes)
        })
        .collect::<Vec<_>>();
    let status = unsafe {
        NtSetEaFile(
            file.as_raw_handle(),
            &mut IO_STATUS_BLOCK::default(),
            aligned.as_ptr().cast(),
            ea.len() as u32,
        )
    };
    if status < 0 {
        return Err(format!(
            "Setting the fixture EA failed with NTSTATUS {status:#010x}"
        ));
    }
    Ok(())
}

fn metadata_sample(catalogue: &partitions::merge::Catalogue) -> Result<(), String> {
    let path = ["资料", METADATA_SAMPLE].map(|part| part.encode_utf16().collect::<Vec<_>>());
    let id = catalogue
        .names
        .get(path.as_slice())
        .ok_or("Metadata fixture file is missing")?;
    let aliases =
        partitions::merge::metadata::short_name_links(catalogue, &AtomicBool::new(false))?;
    if !aliases.contains_key(&path.to_vec()) {
        return Err("Windows did not create a separate short name for the metadata fixture".into());
    }
    let (info, ea) = partitions::merge::metadata::extended_attributes(&catalogue.nodes[id])?
        .ok_or("The Windows fixture EA is missing")?;
    partitions::merge::metadata::visit_ea(info, ea, |record, _, last| {
        let value_at = 5 + TEST_EA_NAME.len();
        if !last
            || record.len() != value_at + TEST_EA_SIZE
            || record[0] != 0
            || record[1] as usize != TEST_EA_NAME.len()
            || &record[4..value_at - 1] != TEST_EA_NAME
            || record[value_at..].iter().any(|&byte| byte != 0xa5)
        {
            return Err("The Windows fixture EA does not match its seed".into());
        }
        Ok(())
    })
}

fn sample(catalogue: &partitions::merge::Catalogue) -> Result<(), String> {
    let path = |parts: &[&str]| {
        parts
            .iter()
            .map(|part| part.encode_utf16().collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let id = catalogue
        .names
        .get(&path(&["资料", "原始文件.bin"]))
        .ok_or("Missing test file")?;
    let alias = catalogue.names.get(&path(&[ALIAS]));
    let empty = catalogue.names.get(&path(&["资料", "空目录"]));
    let node = &catalogue.nodes[id];
    let named = STREAM
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    if alias != Some(id)
        || !empty.is_some_and(|value| catalogue.nodes[value].directory)
        || node.streams.len() != 2
        || !node.streams.iter().any(|stream| {
            stream.locator.name.is_empty()
                && stream.sha256 == <[u8; 32]>::from(Sha256::digest(contents()))
        })
        || !node.streams.iter().any(|stream| {
            stream.locator.name == named
                && stream.sha256 == <[u8; 32]>::from(Sha256::digest(b"NTFS clone named stream"))
        })
    {
        return Err("The sample hard links, streams or empty directory do not match".into());
    }
    Ok(())
}

pub(super) fn prepare() -> Result<String, String> {
    prepare_fixture(Preparation::Fresh)
}

pub(super) fn continue_preparation() -> Result<String, String> {
    prepare_fixture(Preparation::Formatted)
}

pub(super) fn prepare_seeded() -> Result<String, String> {
    prepare_fixture(Preparation::Seeded)
}

pub(super) fn prepare_existing_target() -> Result<String, String> {
    prepare_fixture(Preparation::TargetFormatted)
}

pub(super) fn inspect_seeded() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (witness_root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    check_earlier_layouts(&witness_root, None)?;
    let fixture = reallocation_checks::metadata_partition(
        A,
        Some("DSNTCOPY"),
        SOURCE_AT / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (selected, input) = resolved(A, fixture.number, SOURCE_AT, SOURCE_BYTES, true)?;
    let locks = data_windows::state_locks(&input)?;
    let locked = data_windows::resolve_locked(&selected, false, &input, &locks)?;
    let mut reader = data_windows::reader(&locked)?;
    let source = catalogue(&mut reader, &AtomicBool::new(false))?;
    sample(&source)?;
    if source.names.len() > 16 {
        return Err("Too many entries for this fixture inspection".into());
    }
    let mut output = Vec::new();
    for (path, id) in &source.names {
        let node = &source.nodes[id];
        let display = path
            .iter()
            .map(|p| String::from_utf16_lossy(p))
            .collect::<Vec<_>>()
            .join("/");
        let meta = node
            .metadata
            .iter()
            .map(|(kind, bytes)| format!("{kind:#x}/{}", bytes.len()))
            .collect::<Vec<_>>();
        let flags = node
            .streams
            .iter()
            .map(|stream| format!("{:#06x}", stream.storage_flags))
            .collect::<Vec<_>>();
        output.push(format!(
            "{} {} flags {:#010x}, metadata {:?}, streams {:?}",
            id.0, display, node.attributes, meta, flags
        ));
    }
    Ok(format!(
        "Read-only A/{} catalogue and known streams match.\n{}",
        fixture.number,
        output.join("\n")
    ))
}

enum Preparation {
    Fresh,
    Formatted,
    Seeded,
    TargetFormatted,
}

fn catalogue(
    reader: &mut (impl Read + Seek),
    cancel: &AtomicBool,
) -> Result<partitions::merge::Catalogue, String> {
    match partitions::merge::catalogue_file_clone(reader, cancel) {
        Ok(catalogue) => Ok(catalogue),
        Err(error) => Err(format!(
            "{error}\n{}",
            index_summary(reader).unwrap_or_else(|e| e)
        )),
    }
}

fn index_summary(reader: &mut (impl Read + Seek)) -> Result<String, String> {
    use ntfs::{Ntfs, NtfsAttributeType};
    use std::collections::{BTreeMap, BTreeSet};
    let fs = Ntfs::new(reader).map_err(err)?;
    let mft = fs.file(reader, 0).map_err(err)?;
    let bitmap = mft
        .attributes_raw()
        .find_map(|item| {
            let attr = item.ok()?;
            (attr.ty().ok()? == NtfsAttributeType::Bitmap && attr.name_length() == 0)
                .then_some(attr)
        })
        .ok_or("No fixture MFT bitmap")?;
    if bitmap.value_length() > 512 {
        return Err("Fixture MFT exceeds diagnostic scope".into());
    }
    let mut bytes = vec![0; bitmap.value_length() as usize];
    bitmap
        .value(reader)
        .map_err(err)?
        .attach(reader)
        .read_exact(&mut bytes)
        .map_err(err)?;
    let mut directories = BTreeSet::from([5, 11]);
    let mut expected = BTreeMap::<u64, BTreeSet<(u64, u16, u8, String)>>::new();
    for (offset, value) in bytes.into_iter().enumerate() {
        for bit in 0..8 {
            let id = (offset * 8 + bit) as u64;
            if value & (1 << bit) == 0 || id < 16 {
                continue;
            }
            let file = fs.file(reader, id).map_err(err)?;
            if ntfs_names::merge_base_reference(reader, &file)? != 0 {
                continue;
            }
            if file.is_directory() {
                directories.insert(id);
            }
            for name in ntfs_names::read_for_merge(reader, &file)? {
                expected.entry(name.parent.0).or_default().insert((
                    id,
                    file.sequence_number(),
                    name.namespace,
                    name.display(),
                ));
            }
        }
    }
    let mut differences = Vec::new();
    for number in directories {
        let file = fs.file(reader, number).map_err(err)?;
        let index = file.directory_index(reader).map_err(err)?;
        let mut entries = index.entries();
        let mut observed = BTreeSet::new();
        while let Some(entry) = entries.next(reader) {
            let entry = entry.map_err(err)?;
            let reference = entry.file_reference();
            if reference.file_record_number() < 16 {
                continue;
            }
            let key = entry
                .key()
                .ok_or("Missing fixture index name")?
                .map_err(err)?;
            observed.insert((
                reference.file_record_number(),
                reference.sequence_number(),
                key.namespace() as u8,
                key.name().to_string_lossy(),
            ));
        }
        let required = expected.remove(&number).unwrap_or_default();
        if required != observed {
            differences.push(format!(
                "Directory {number}: missing {:?}, extra {:?}",
                required.difference(&observed).take(3).collect::<Vec<_>>(),
                observed.difference(&required).take(3).collect::<Vec<_>>()
            ));
        }
    }
    Ok(if differences.is_empty() {
        "Active user names cover each inspected index.".into()
    } else {
        differences.join("\n")
    })
}

fn prepare_fixture(stage: Preparation) -> Result<String, String> {
    let continuing = !matches!(stage, Preparation::Fresh);
    let desktop = btrfs_checks::desktop()?;
    let (witness_root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    if witness_root.symlink_metadata(FOLDER).is_ok() != continuing {
        return Err("The fixture witness folder already exists".into());
    }
    let before_a = fat_resize_checks::model(A)?;
    let before_b = fat_resize_checks::model(B)?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    if before_a.guid == before_b.guid
        || before_a
            .entries
            .iter()
            .any(|a| before_b.entries.iter().any(|b| a.guid == b.guid))
    {
        return Err("Fixture disk identifiers overlap".into());
    }
    let existing = if matches!(stage, Preparation::TargetFormatted) {
        Some(prepared_target()?)
    } else {
        None
    };
    if continuing {
        check_earlier_layouts(&witness_root, existing.as_ref().map(|part| part.base.start))?;
    } else if !available(&before_a, SOURCE_AT, SOURCE_BYTES)
        || first_nonzero(&mut raw(A)?, SOURCE_AT, SOURCE_BYTES)?.is_some()
    {
        return Err("The dedicated A source window is unavailable".into());
    }
    let target_at = match &existing {
        Some(part) => part.base.start,
        None => target_window(&before_b)?,
    };
    let source_edges = edges(A, SOURCE_AT, SOURCE_BYTES)?;
    let target_edges = edges(B, target_at, TARGET_BYTES)?;
    if !continuing {
        witness_root.create_dir(FOLDER).map_err(err)?;
    }
    let witness_dir = witness_root.open_dir(FOLDER).map_err(err)?;
    if witness_dir.symlink_metadata("baseline.json").is_ok() {
        return Err("A completed NTFS fixture cannot be prepared again".into());
    }
    if !continuing {
        create(
            A,
            SOURCE_AT,
            SOURCE_BYTES,
            FileSystem::Ntfs,
            "DSNTCOPY",
            4096,
            false,
        )?;
    }
    let fixture = reallocation_checks::metadata_partition(
        A,
        Some("DSNTCOPY"),
        SOURCE_AT / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let source = fixture.number;
    if matches!(stage, Preparation::Formatted) {
        let (_, input) = resolved(A, source, SOURCE_AT, SOURCE_BYTES, true)?;
        let mut reader = data_windows::reader(&input)?;
        let catalogue = catalogue(&mut reader, &AtomicBool::new(false))?;
        if catalogue.names.len() != 1 {
            return Err(
                "The prepared source already contains non-system files or directories".into(),
            );
        }
    }
    if matches!(stage, Preparation::Fresh | Preparation::Formatted) {
        let letter = match fixture.drive_letter {
            Some(letter) => letter,
            None => {
                operation(
                    A,
                    Action::SetDriveLetter {
                        partition: source,
                        letter: Some(DriveLetter("A".into())),
                    },
                )?;
                "A".into()
            }
        };
        let (selected, input) = resolved(A, source, SOURCE_AT, SOURCE_BYTES, true)?;
        seed(&selected, &input, Some(&letter))?;
    }
    let (selected, input) = resolved(A, source, SOURCE_AT, SOURCE_BYTES, true)?;
    let locks = data_windows::state_locks(&input)?;
    let locked = data_windows::resolve_locked(&selected, false, &input, &locks)?;
    let mut reader = data_windows::reader(&locked)?;
    let cancel = AtomicBool::new(false);
    sample(&catalogue(&mut reader, &cancel)?)?;
    let source_plan = crate::ntfs_file_clone::Plan::prepare(
        &mut reader,
        surface_scan::Range {
            offset: target_at,
            size: TARGET_BYTES,
            sector: 512,
        },
        0x3910_8133_4579_4021,
        &cancel,
    )?;
    drop(reader);
    drop(locks);
    let target = match &existing {
        Some(part) => part.number,
        None => create(
            B,
            target_at,
            TARGET_BYTES,
            FileSystem::Fat16,
            TARGET_LABEL,
            4096,
            false,
        )?,
    };
    let (_, destination) = resolved(B, target, target_at, TARGET_BYTES, true)?;
    if existing.is_some() {
        confirm_prepared_target(&destination, &cancel)?;
    }
    let mut observed_a = fat_resize_checks::model(A)?;
    if !continuing {
        observed_a
            .entries
            .retain(|p| p.offset != SOURCE_AT || p.size != SOURCE_BYTES);
    }
    let mut observed_b = fat_resize_checks::model(B)?;
    if existing.is_none() {
        observed_b
            .entries
            .retain(|p| p.offset != target_at || p.size != TARGET_BYTES);
    }
    if !observed_a.agrees(&before_a)
        || !observed_b.agrees(&before_b)
        || reallocation_checks::os_layout(&inventory()?)? != os
        || edges(A, SOURCE_AT, SOURCE_BYTES)? != source_edges
        || edges(B, target_at, TARGET_BYTES)? != target_edges
    {
        return Err("The protected fixture state changed during preparation".into());
    }
    // Unlocking a Windows volume can update NTFS housekeeping records.
    // Compare the checked file catalogue, then take the byte baseline after
    // releasing the source lock and completing target preparation.
    source_plan.revalidate(&mut data_windows::reader(&input)?, &cancel)?;
    let source_digest = resize_clone_checks::hash(&input)?;
    let witness = Witness {
        desktop,
        source,
        target,
        target_at,
        source_digest,
        source_edges,
        target_edges,
        layouts: ntfs_reconstruction_checks::table_hashes()?,
    };
    let mut out = witness_dir
        .open_with(
            "baseline.json",
            CapOptions::new().write(true).create_new(true),
        )
        .map_err(err)?;
    serde_json::to_writer(&mut out, &witness).map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok(format!("PASS: ready to clone A/{source} DSNTCOPY (96 MiB) to B/{target} NTDEST (48 MiB). Main stream, named stream, hard link and empty directory verified."))
}

fn check_earlier_layouts(witness_root: &Dir, prepared_b: Option<u64>) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Earlier {
        layouts: [String; 3],
    }
    let prior: Earlier = serde_json::from_reader(
        witness_root
            .open_dir("DS-FileCloneExfat")
            .map_err(err)?
            .open("baseline.json")
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let current = ntfs_reconstruction_checks::table_hashes()?;
    let b_matches = match prepared_b {
        Some(offset) => retained_restore_checks::original_gpt_matches(
            &quick_partition_checks::table(B)?,
            &prior.layouts[1],
            &[(offset, TARGET_BYTES)],
        )?,
        None => prior.layouts[1] == current[1],
    };
    if !b_matches
        || prior.layouts[2] != current[2]
        || !retained_restore_checks::original_gpt_matches(
            &quick_partition_checks::table(A)?,
            &prior.layouts[0],
            &[(SOURCE_AT, SOURCE_BYTES)],
        )?
    {
        return Err("The pre-existing GPTs or guest OS layout differ".into());
    }
    Ok(())
}

struct CompletedFixture {
    witness: Witness,
    source_selection: Selection,
    source: data_windows::Resolved,
    target: data_windows::Resolved,
}

fn completed_fixture() -> Result<CompletedFixture, String> {
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let witness: Witness = serde_json::from_reader(
        root.open_dir(FOLDER)
            .map_err(err)?
            .open("baseline.json")
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let target_at = witness.target_at;
    if !(TARGET_FIRST..=TARGET_END - TARGET_BYTES).contains(&target_at) || target_at % MIB != 0 {
        return Err("The witness target lies outside the dedicated B fixture range".into());
    }
    let (source_selection, source) = resolved(A, witness.source, SOURCE_AT, SOURCE_BYTES, true)?;
    let (_, target) = resolved(B, witness.target, target_at, TARGET_BYTES, false)?;
    if desktop != witness.desktop || target.disk.is_read_only != Some(false) {
        return Err("Desktop or target protection differs from the witness".into());
    }
    verify_scope(&witness)?;
    Ok(CompletedFixture {
        witness,
        source_selection,
        source,
        target,
    })
}

fn verify_scope(witness: &Witness) -> Result<(), String> {
    if ntfs_reconstruction_checks::table_hashes()? != witness.layouts {
        return Err("An A/B GPT or guest OS disk layout changed".into());
    }
    if edges(A, SOURCE_AT, SOURCE_BYTES)? != witness.source_edges
        || edges(B, witness.target_at, TARGET_BYTES)? != witness.target_edges
    {
        return Err("An NTFS fixture's neighboring sectors changed".into());
    }
    Ok(())
}

pub(super) fn verify() -> Result<String, String> {
    let CompletedFixture {
        witness,
        source: src,
        target: dst,
        ..
    } = completed_fixture()?;
    let source_digest = resize_clone_checks::hash(&src)?;
    let digest = verify_target(&src, &dst, witness.target_at, sample)?;
    if source_digest != witness.source_digest {
        return Err(format!(
            "Target NTFS files, streams, hard links and metadata match. B is offline.\nTarget SHA-256 {digest}.\nBoth GPTs, guest OS layout and neighboring sectors match.\nSource full-partition SHA-256 differs from the baseline.\nBefore {}\nAfter  {source_digest}",
            witness.source_digest
        ));
    }
    Ok(format!("PASS: NTFS files, named stream, hard link, times, attributes and security match. B remains offline. Full target SHA-256 {digest}. Source, GPTs, OS layout and neighboring sectors match."))
}

fn verify_target(
    src: &data_windows::Resolved,
    dst: &data_windows::Resolved,
    target_at: u64,
    verify_sample: impl FnOnce(&partitions::merge::Catalogue) -> Result<(), String>,
) -> Result<String, String> {
    let sector = test_sector();
    let cancel = AtomicBool::new(false);
    let mut reader = data_windows::reader(dst)?;
    let fs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    let mut start = [0; 4];
    reader.seek(SeekFrom::Start(28)).map_err(err)?;
    reader.read_exact(&mut start).map_err(err)?;
    if fs.size() + sector != dst.size
        || u64::from(fs.sector_size()) != sector
        || fs.cluster_size() != 4096
        || u64::from(u32::from_le_bytes(start)) != target_at / sector
        || !fs
            .volume_name(&mut reader)
            .transpose()
            .map_err(err)?
            .is_some_and(|n| n.name() == "DISKCLONE")
    {
        return Err("Independent NTFS geometry or label differs".into());
    }
    let target = partitions::merge::catalogue_file_clone(&mut reader, &cancel)?;
    verify_sample(&target)?;
    let mut reader = data_windows::reader(src)?;
    if ntfs::Ntfs::new(&mut reader).map_err(err)?.serial_number() == fs.serial_number() {
        return Err("The clone duplicates the source NTFS serial".into());
    }
    let source = partitions::merge::catalogue_file_clone(&mut reader, &cancel)?;
    source.verify_file_clone(&target)?;
    resize_clone_checks::hash(dst)
}

//! Independent file and partition verification on the isolated A/B disk pair.
use super::*;
use cap_std::fs::OpenOptions as CapOptions;
use disk_core::MIB;
use partitions::{Action, FileSystem, PartitionKind};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const SAMPLE: &str = "DSRZSAMP.BIN";

pub(super) enum Case {
    Fat,
    Exfat,
}

impl Case {
    fn source_info(&self) -> (&'static str, &'static str, u64) {
        match self {
            Self::Fat => ("FAT16", "DSFAT16", 128 * MIB),
            Self::Exfat => ("exFAT", "DSCAPEX", 96 * MIB),
        }
    }

    fn target_info(&self) -> (u64, u64, FileSystem, &'static str, u32, &'static str) {
        match self {
            Self::Fat => (
                13 * GIB + 848 * MIB,
                2 * MIB,
                FileSystem::Fat12,
                "FILECLONE",
                512,
                "DS-FileClone",
            ),
            Self::Exfat => (
                13 * GIB + 864 * MIB,
                16 * MIB,
                FileSystem::Fat16,
                "EXCLONE",
                2048,
                "DS-FileCloneExfat",
            ),
        }
    }
}

#[derive(serde::Serialize, Deserialize)]
struct Witness {
    desktop: String,
    source: u32,
    source_hash: String,
    target: u32,
    layouts: [String; 3],
    boundaries: [String; 2],
    files: usize,
    directories: usize,
    contents: String,
}

fn source(case: &Case) -> Result<(Selection, data_windows::Resolved), String> {
    let (filesystem, label, size) = case.source_info();
    let (selected, offset) = allocation_batch::fixture_selection(filesystem, Some(label), size)?;
    let disk = allocation_batch::resolved(&selected, offset, size, false)?;
    if disk.disk.logical_sector_size != Some(512) {
        return Err("The source fixture uses a different sector size".into());
    }
    let expected: Vec<_> = (0..65_553)
        .map(|i| {
            (match case {
                Case::Fat => i * 47 + i / 137 + 1,
                Case::Exfat => i * 67 + i / 191 + 4096,
            } % 251) as u8
        })
        .collect();
    allocation_batch::check_file(&selected, offset, size, SAMPLE, &expected)?;
    Ok((selected, disk))
}

fn target(number: u32, offset: u64, size: u64) -> Result<data_windows::Resolved, String> {
    let resolved = data_windows::resolve(&selection(B, Some(number))?, false, false)?;
    if resolved
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(B)
        || resolved.offset != offset
        || resolved.size != size
        || resolved.disk.logical_sector_size != Some(512)
        || resolved.disk.is_boot != Some(false)
        || resolved.disk.is_system != Some(false)
    {
        return Err("B fixture identity or geometry changed".into());
    }
    Ok(resolved)
}

fn boundaries(file: &mut fs::File, start: u64, size: u64) -> Result<[String; 2], String> {
    let mut values = [String::new(), String::new()];
    for (offset, value) in [start - 512, start + size].into_iter().zip(&mut values) {
        *value = format!(
            "{:x}",
            Sha256::digest(table_archive::read_at(file, offset, 512, 16 * GIB)?)
        );
    }
    Ok(values)
}

fn raw_b() -> Result<fs::File, String> {
    data_windows::device(
        &data_windows::resolve(&selection(B, None)?, false, false)?,
        false,
    )
}

#[derive(serde::Serialize)]
struct Entry {
    path: Vec<String>,
    metadata: file_metadata::Metadata,
    content: Option<String>,
}

fn file_tree(r: &mut (impl Read + Seek), size: u64) -> Result<(usize, usize, String), String> {
    use recovery_tree::{Catalog, ROOT};
    let mut catalog = Catalog::default();
    let mut files = Vec::new();
    recovery::scan_filesystem_with_directories(
        r,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| files.push(file),
        |dir| catalog.insert(dir),
    )?;
    let names = |id: &str| {
        catalog.path(id).map(|path| {
            path.into_iter()
                .skip(1)
                .map(|dir| dir.name)
                .collect::<Vec<_>>()
        })
    };
    let mut entries = Vec::new();
    for dir in catalog.snapshot().into_iter().filter(|dir| dir.id != ROOT) {
        entries.push(Entry {
            path: names(&dir.id)?,
            metadata: dir.metadata,
            content: None,
        });
    }
    let directories = entries.len();
    let mut remaining = size;
    for file in &files {
        remaining = remaining
            .checked_sub(file.size)
            .ok_or("Fixture exceeds target file budget")?;
        let mut bytes = Vec::new();
        let extracted =
            recovery::extract_with_metadata(r, file, &mut bytes, file.size, |_| Ok(()))?;
        if extracted.bytes != file.size || bytes.len() as u64 != file.size || file.deleted {
            return Err("Independent live-file extraction is incomplete".into());
        }
        let mut path = names(
            file.directory
                .as_deref()
                .ok_or("Fixture file has no directory")?,
        )?;
        path.push(file.name.clone());
        entries.push(Entry {
            path,
            metadata: extracted.metadata,
            content: Some(format!("{:x}", Sha256::digest(bytes))),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if entries.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err("Independent file tree has ambiguous names".into());
    }
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&entries).map_err(err)?)
    );
    Ok((files.len(), directories, digest))
}

pub(super) fn prepare(case: Case) -> Result<String, String> {
    prepare_fixture(case, false)
}

pub(super) fn continue_exfat_preparation() -> Result<String, String> {
    prepare_fixture(Case::Exfat, true)
}

fn prepare_fixture(case: Case, continuing: bool) -> Result<String, String> {
    let (start, size, format, target_label, cluster_size, folder) = case.target_info();
    let (source_format, source_label, source_size) = case.source_info();
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let old_a = fat_resize_checks::model(A)?;
    let old_b = fat_resize_checks::model(B)?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    let range_end = start + size;
    let known_target = |offset, capacity| offset == start && capacity == size;
    let collision = |offset, capacity| {
        offset < range_end + 512
            && start - 512 < offset + capacity
            && !(continuing && known_target(offset, capacity))
    };
    if old_a.guid == old_b.guid
        || old_a
            .entries
            .iter()
            .any(|a| old_b.entries.iter().any(|b| a.guid == b.guid))
        || (start - 512) / 512 < old_b.first
        || range_end / 512 > old_b.last
        || old_b.entries.iter().any(|p| collision(p.offset, p.size))
    {
        return Err("The test target is unavailable or drive identifiers overlap".into());
    }
    if continuing {
        let dir = root.open_dir(folder).map_err(err)?;
        let mut started = String::new();
        dir.open("started")
            .map_err(err)?
            .take(65)
            .read_to_string(&mut started)
            .map_err(err)?;
        if started != desktop || dir.symlink_metadata("baseline.json").is_ok() {
            return Err("No unfinished exFAT fixture bound to this desktop build".into());
        }
        reallocation_checks::state(&inventory()?, B, false)?;
        let prior: Witness = serde_json::from_reader(
            root.open_dir("DS-FileClone")
                .map_err(err)?
                .open("baseline.json")
                .map_err(err)?
                .take(4096),
        )
        .map_err(err)?;
        let layouts = ntfs_reconstruction_checks::table_hashes()?;
        if prior.layouts[0] != layouts[0]
            || prior.layouts[2] != layouts[2]
            || !retained_restore_checks::original_gpt_matches(
                &quick_partition_checks::table(B)?,
                &prior.layouts[1],
                &[(start, size)],
            )?
        {
            return Err("The earlier GPT and guest OS layout witness does not match the known test addition".into());
        }
    }
    let mut file = raw_b()?;
    let edges = boundaries(&mut file, start, size)?;
    file.seek(SeekFrom::Start(start)).map_err(err)?;
    let mut data = vec![0; size as usize];
    file.read_exact(&mut data).map_err(err)?;
    if data.iter().any(|&byte| byte != 0) {
        return Err("The target window is not blank".into());
    }
    drop(file);
    let (selected, input) = source(&case)?;
    let source_number = selected
        .partition
        .ok_or("Source fixture partition is missing")?;
    let source_hash = resize_clone_checks::hash(&input)?;
    let mut reader = data_windows::reader(&input)?;
    let (files, directories, contents) = file_tree(&mut reader, size)?;
    let cancel = AtomicBool::new(false);
    let fit =
        fat_recovery::resize::file_clone::prepare(&mut reader, 512, size, start, 12, &cancel)?;
    if files == 0
        || fit.source_format != source_format
        || fit.destination_format
            != match case {
                Case::Fat => "FAT12",
                Case::Exfat => "exFAT",
            }
        || fit.files != files
        || fit.directories != directories
    {
        return Err("Fixture tree does not match the planned file clone".into());
    }
    if !continuing {
        root.create_dir(folder).map_err(err)?;
    }
    let dir = root.open_dir(folder).map_err(err)?;
    let marker = if continuing { "continued" } else { "started" };
    dir.open_with(marker, CapOptions::new().write(true).create_new(true))
        .map_err(err)?
        .write_all(desktop.as_bytes())
        .map_err(err)?;
    if !continuing {
        allocation_batch::set_online(B)?;
    }
    if fat_resize_checks::model(B)? != old_b {
        return Err("B GPT changed before test preparation".into());
    }
    if !continuing {
        operation(
            B,
            Action::Create {
                offset: start,
                size,
                kind: PartitionKind::Windows,
            },
        )?;
    }
    let number = reallocation_checks::metadata_partition(B, None, start / MIB, size / MIB)?.number;
    operation(
        B,
        Action::Format {
            partition: number,
            file_system: format,
            label: target_label.into(),
            cluster_size,
            options: Default::default(),
        },
    )?;
    let mut observed = fat_resize_checks::model(B)?;
    if !continuing {
        observed.entries.retain(|p| !known_target(p.offset, p.size));
    }
    if !observed.agrees(&old_b)
        || continuing && observed != old_b
        || fat_resize_checks::model(A)? != old_a
        || reallocation_checks::os_layout(&inventory()?)? != os
        || boundaries(&mut raw_b()?, start, size)? != edges
        || resize_clone_checks::hash(&input)? != source_hash
    {
        return Err("Protected test state changed during fixture preparation".into());
    }
    let witness = Witness {
        desktop,
        source: source_number,
        source_hash,
        target: number,
        layouts: ntfs_reconstruction_checks::table_hashes()?,
        boundaries: edges,
        files,
        directories,
        contents,
    };
    let mut out = dir
        .open_with(
            "baseline.json",
            CapOptions::new().write(true).create_new(true),
        )
        .map_err(err)?;
    serde_json::to_writer(&mut out, &witness).map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok(format!("PASS: clone A/{source_number} {source_label} ({} MiB) by files to B/{number} {target_label} ({} MiB). Files: {files}, directories: {directories}.", source_size / MIB, size / MIB))
}

pub(super) fn verify(case: Case) -> Result<String, String> {
    let (start, size, _, _, _, folder) = case.target_info();
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let witness: Witness = serde_json::from_reader(
        root.open_dir(folder)
            .map_err(err)?
            .open("baseline.json")
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let (selected, input) = source(&case)?;
    if desktop != witness.desktop
        || selected.partition != Some(witness.source)
        || resize_clone_checks::hash(&input)? != witness.source_hash
        || ntfs_reconstruction_checks::table_hashes()? != witness.layouts
        || boundaries(&mut raw_b()?, start, size)? != witness.boundaries
    {
        return Err("The source, GPTs, OS layout or neighboring sectors changed".into());
    }
    let output = target(witness.target, start, size)?;
    if output.disk.is_offline != Some(true) || output.disk.is_read_only != Some(false) {
        return Err("B did not stay writable and offline".into());
    }
    let mut reader = data_windows::reader(&output)?;
    verify_boot(&mut reader, &case, start, size)?;
    if file_tree(&mut reader, size)? != (witness.files, witness.directories, witness.contents) {
        return Err("The complete file or directory metadata and content tree differs".into());
    }
    let digest = resize_clone_checks::hash(&output)?;
    Ok(format!("PASS: {} files, {} directories, contents and FAT metadata match. B remains offline. Full target SHA-256 {digest}. Source, GPTs, OS layout and neighboring sectors match.", witness.files, witness.directories))
}

fn verify_boot(
    reader: &mut (impl Read + Seek),
    case: &Case,
    start: u64,
    size: u64,
) -> Result<(), String> {
    let mut bytes = [0u8; 24 * 512];
    reader.read_exact(&mut bytes).map_err(err)?;
    let boot = &bytes[..512];
    match case {
        Case::Fat
            if &boot[54..62] == b"FAT12   "
                && &boot[43..54] == b"DISKCLONE  "
                && u64::from(u32::from_le_bytes(boot[28..32].try_into().unwrap()))
                    == start / 512
                && u64::from(u16::from_le_bytes(boot[19..21].try_into().unwrap()))
                    == size / 512 =>
        {
            Ok(())
        }
        Case::Exfat
            if &boot[3..11] == b"EXFAT   "
                && &bytes[..12 * 512] == &bytes[12 * 512..]
                && u64::from_le_bytes(boot[64..72].try_into().unwrap()) == start / 512
                && u64::from_le_bytes(boot[72..80].try_into().unwrap()) == size / 512 =>
        {
            let checksum = fat_recovery::exfat_boot_checksum(&bytes[..11 * 512]).to_le_bytes();
            if bytes[11 * 512..12 * 512]
                .chunks_exact(4)
                .any(|word| word != checksum)
            {
                return Err("exFAT boot checksum differs".into());
            }
            let heap = u64::from(u32::from_le_bytes(boot[88..92].try_into().unwrap())) * 512;
            let root = u64::from(u32::from_le_bytes(boot[96..100].try_into().unwrap()));
            let cluster = 512u64
                .checked_shl(u32::from(boot[109]))
                .filter(|&s| s <= 32 * MIB)
                .ok_or("Invalid exFAT cluster size")?;
            let root = root
                .checked_sub(2)
                .and_then(|index| index.checked_mul(cluster))
                .and_then(|bytes| heap.checked_add(bytes))
                .filter(|&at| at <= size - 32)
                .ok_or("exFAT root outside partition")?;
            reader.seek(SeekFrom::Start(root)).map_err(err)?;
            let mut label = [0u8; 32];
            reader.read_exact(&mut label).map_err(err)?;
            if label[..2] != [0x83, 9]
                || label[2..20]
                    .chunks_exact(2)
                    .map(|v| u16::from_le_bytes(v.try_into().unwrap()))
                    .ne("DISKCLONE".encode_utf16())
            {
                return Err("exFAT target label differs".into());
            }
            Ok(())
        }
        _ => Err("Destination FAT/exFAT boot or geometry differs".into()),
    }
}

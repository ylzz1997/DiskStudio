//! Verify exact and adjusted-size restores from retained archive inputs.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use partitions::{Action, FileSystem, PartitionKind};
use resize_clone_checks::{hash, source, verify_sample, CASES};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const FOLDER: &str = "DS-RetainedRestore";
const BASELINE: &str = "baseline.json";
const ARCHIVES: [&str; 3] = ["Exact.dsbackup", "Earlier.dsbackup", "Latest.dsbackup"];
const OFFSETS: [u64; 2] = [13 * GIB + 736 * MIB, 13 * GIB + 752 * MIB];
const SIZES: [u64; 2] = [MIB, 64 * MIB];
const SECTOR: u64 = 512;
const SECOND_PREPARATION_OFFSET: u64 = 13 * GIB + 832 * MIB;

#[derive(serde::Serialize, Deserialize)]
struct Baseline {
    partitions: [u32; 2],
    layouts: [String; 3],
    source_hashes: [String; 2],
    exact_content: String,
    backup_hashes: [String; 3],
    boundaries: [String; 4],
    blank_resize: String,
    serial_earlier: u32,
    serial_latest: u32,
}

fn output(dir: &Dir, name: &str) -> Result<fs::File, String> {
    Ok(dir
        .open_with(
            name,
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .map_err(err)?
        .into_std())
}

fn backup(
    dir: &Dir,
    name: &str,
    image: &[u8],
    parent: Option<&backup_archive::Chain>,
) -> Result<backup_archive::Archive, String> {
    let mut file = output(dir, name)?;
    backup_archive::create(
        &mut &image[..],
        &mut file,
        "partition",
        image.len() as u64,
        SECTOR,
        true,
        parent,
        |_| Ok(()),
    )?;
    file.sync_all().map_err(err)?;
    backup_archive::Archive::open(file)
}

fn boot_serial(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[100..104].try_into().unwrap())
}

fn set_serial(image: &mut [u8], serial: u32) {
    for boot in image[..24 * 512].chunks_exact_mut(12 * 512) {
        boot[100..104].copy_from_slice(&serial.to_le_bytes());
        let checksum = fat_recovery::exfat_boot_checksum(&boot[..11 * 512]);
        for item in boot[11 * 512..].chunks_exact_mut(4) {
            item.copy_from_slice(&checksum.to_le_bytes());
        }
    }
}

fn raw_b() -> Result<fs::File, String> {
    data_windows::device(
        &data_windows::resolve(&selection(B, None)?, false, false)?,
        false,
    )
}

fn edges(file: &mut fs::File) -> Result<[String; 4], String> {
    let mut result = std::array::from_fn(|_| String::new());
    for (index, hashes) in result.chunks_exact_mut(2).enumerate() {
        for (offset, hash) in [OFFSETS[index] - SECTOR, OFFSETS[index] + SIZES[index]]
            .into_iter()
            .zip(hashes)
        {
            let bytes = table_archive::read_at(file, offset, SECTOR as usize, 16 * GIB)?;
            *hash = format!("{:x}", Sha256::digest(bytes));
        }
    }
    Ok(result)
}

fn file_digests(dir: &Dir) -> Result<[String; 3], String> {
    let mut digests = std::array::from_fn(|_| String::new());
    for (name, result) in ARCHIVES.into_iter().zip(&mut digests) {
        let mut file = dir.open(name).map_err(err)?.into_std();
        let bytes = file.metadata().map_err(err)?.len();
        if bytes == 0 || bytes > 98 * MIB {
            return Err("Unexpected fixture archive size".into());
        }
        *result = data_io::hash_exact(&mut file, bytes, |_| Ok(()))?;
    }
    Ok(digests)
}

fn target(index: usize, number: u32) -> Result<data_windows::Resolved, String> {
    let r = data_windows::resolve(&selection(B, Some(number))?, false, false)?;
    if r.offset != OFFSETS[index]
        || r.size != SIZES[index]
        || r.disk.logical_sector_size != Some(SECTOR)
        || r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(B)
    {
        return Err("Target geometry does not match the owned fixture".into());
    }
    Ok(r)
}

pub(super) fn prepare() -> Result<String, String> {
    let sha = btrfs_checks::desktop()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let original_b = fat_resize_checks::model(B)?;
    let original_a = fat_resize_checks::model(A)?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    if original_b.guid == original_a.guid
        || original_b
            .entries
            .iter()
            .any(|p| original_a.entries.iter().any(|q| q.guid == p.guid))
    {
        return Err("Source and target have duplicate GPT identities".into());
    }
    let mut file = raw_b()?;
    let boundaries = edges(&mut file)?;
    let mut bytes = vec![0; MIB as usize];
    for (offset, size) in OFFSETS.into_iter().zip(SIZES) {
        let end = offset + size;
        if (offset - SECTOR) / SECTOR < original_b.first
            || end / SECTOR > original_b.last
            || original_b
                .entries
                .iter()
                .any(|p| p.offset < end + SECTOR && offset - SECTOR < p.offset + p.size)
        {
            return Err("Target window overlaps a GPT entry or exceeds usable LBAs".into());
        }
        file.seek(SeekFrom::Start(offset)).map_err(err)?;
        for _ in 0..size / MIB {
            file.read_exact(&mut bytes).map_err(err)?;
            if bytes.iter().any(|&value| value != 0) {
                return Err("Reserved B window is not empty".into());
            }
        }
    }
    drop(file);
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    output(&dir, "started")?
        .write_all(sha.as_bytes())
        .map_err(err)?;
    let mut images = [Vec::new(), Vec::new()];
    for (i, bytes) in images.iter_mut().enumerate() {
        let current = source(&CASES[i])?;
        verify_sample(&CASES[i], &current)?;
        let selected = selection(A, Some(20 + i as u32))?;
        let read = data_windows::StableRead::lock(&selected)?;
        read.reader()?.read_to_end(bytes).map_err(err)?;
        read.verify()?;
    }
    let source_hashes = images
        .each_ref()
        .map(|image| format!("{:x}", Sha256::digest(image)));
    let exact_content = source_hashes[0].clone();
    backup(&dir, ARCHIVES[0], &images[0], None)?;
    let mut chain = backup_archive::Chain::new(backup(&dir, ARCHIVES[1], &images[1], None)?);
    let serial_earlier = boot_serial(&images[1]);
    let serial_latest = if serial_earlier == 437_183_710 {
        73
    } else {
        437_183_710
    };
    set_serial(&mut images[1], serial_latest);
    let latest = backup(&dir, ARCHIVES[2], &images[1], Some(&chain))?;
    chain = chain.prepend(latest)?;
    chain.verify(|_| Ok(()))?;
    drop(chain);

    allocation_batch::set_online(B)?;
    if fat_resize_checks::model(B)? != original_b {
        return Err("Bringing B online changed the original GPT".into());
    }
    let mut records = [0u32; 2];
    for i in 0..2 {
        operation(
            B,
            Action::Create {
                offset: OFFSETS[i],
                size: SIZES[i],
                kind: PartitionKind::Windows,
            },
        )?;
        let raw = data_windows::resolve(&selection(B, None)?, false, false)?;
        let matches: Vec<_> = quick_partition_checks::parts()?
            .into_iter()
            .filter(|p| {
                p.disk_number == raw.disk.number && p.offset == OFFSETS[i] && p.size == SIZES[i]
            })
            .collect();
        let [selected] = matches.as_slice() else {
            return Err("New target partition is ambiguous".into());
        };
        records[i] = selected.partition_number;
        operation(
            B,
            Action::Format {
                partition: records[i],
                label: format!("RESTORE-{}", i + 1),
                file_system: if i == 0 {
                    FileSystem::Fat12
                } else {
                    FileSystem::Exfat
                },
                cluster_size: if i == 0 { 512 } else { 4096 },
                options: Default::default(),
            },
        )?;
        let mut layout = fat_resize_checks::model(B)?;
        for j in 0..=i {
            let before = layout.entries.len();
            layout
                .entries
                .retain(|e| e.offset != OFFSETS[j] || e.size != SIZES[j]);
            if layout.entries.len() + 1 != before {
                return Err(format!("Missing unique test entry {} in the B GPT", j + 1));
            }
        }
        // Creating new partitions can reposition old entries in the GPT
        // array. Compare all other entry fields, including their unique IDs.
        if !layout.agrees(&original_b) {
            return Err(format!(
                "Original B GPT entries changed while preparing target {}",
                i + 1
            ));
        }
        if edges(&mut raw_b()?)? != boundaries {
            return Err(format!(
                "A boundary sector changed while preparing target {}",
                i + 1
            ));
        }
    }
    if fat_resize_checks::model(A)? != original_a {
        return Err("The source A GPT changed during preparation".into());
    }
    if reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("The guest OS partition metadata changed during preparation".into());
    }
    for (case, expected) in CASES.iter().zip(&source_hashes) {
        if hash(&source(case)?)? != *expected {
            return Err("A source partition changed while preparing B".into());
        }
    }
    let baseline = Baseline {
        partitions: records,
        layouts: ntfs_reconstruction_checks::table_hashes()?,
        source_hashes,
        exact_content,
        backup_hashes: file_digests(&dir)?,
        boundaries,
        blank_resize: hash(&target(1, records[1])?)?,
        serial_earlier,
        serial_latest,
    };
    let mut witness = output(&dir, BASELINE)?;
    serde_json::to_writer(&mut witness, &baseline).map_err(err)?;
    witness.sync_all().map_err(err)?;
    Ok(format!("PASS: archives at {}. Restore Exact partition to B/{} (1 MiB). Open Latest with Earlier dependency; choose the earliest point and resize to B/{} (64 MiB).", path.join(FOLDER).display(), records[0], records[1]))
}

// A prior batch recorded the complete raw GPT archive digest. Reconstruct
// that archive in memory from the observed table, dropping only the three
// dedicated test entries. No disk repair is performed if it does not match.
pub(super) fn original_gpt_matches(
    current: &table_archive::Archive,
    expected: &str,
    ranges: &[(u64, u64)],
) -> Result<bool, String> {
    let layout = table_restore::archive_layout(current)?;
    let mut slots = Vec::with_capacity(ranges.len());
    for &(offset, size) in ranges {
        let matches: Vec<_> = layout
            .entries
            .iter()
            .filter(|e| e.offset == offset && e.size == size)
            .collect();
        match matches.as_slice() {
            [entry]
                if entry.kind == partitions::policy::BASIC_DATA
                    && entry.attributes & !(1 << 63) == 0 =>
            {
                slots.push(entry.slot)
            }
            _ => return Err("An interrupted preparation entry no longer matches its scope".into()),
        }
    }
    for compact in [false, true] {
        let mut candidate = current.clone();
        for extent in &mut candidate.extents {
            if !matches!(
                extent.kind,
                table_archive::Kind::PrimaryEntries | table_archive::Kind::BackupEntries
            ) {
                continue;
            }
            let used = layout.count as usize * 128;
            let table = &mut extent.bytes[..used];
            for &slot in &slots {
                let index = (slot as usize - 1) * 128;
                table[index..index + 128].fill(0);
            }
            if compact {
                let retained: Vec<_> = table
                    .chunks_exact(128)
                    .filter(|entry| entry[..16].iter().any(|&byte| byte != 0))
                    .flatten()
                    .copied()
                    .collect();
                table.fill(0);
                table[..retained.len()].copy_from_slice(&retained);
            }
        }
        let candidate = table_restore::edit_gpt_entry(&candidate, &layout, 1, |_| {})?;
        if format!("{:x}", Sha256::digest(candidate.encode()?)) == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn assess_preparation() -> Result<String, String> {
    let sha = btrfs_checks::desktop()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let mut started = String::new();
    dir.open("started")
        .map_err(err)?
        .take(65)
        .read_to_string(&mut started)
        .map_err(err)?;
    if started != sha || dir.symlink_metadata(BASELINE).is_ok() {
        return Err("No matching interrupted preparation".into());
    }
    #[derive(Deserialize)]
    struct Prior {
        layout: [String; 3],
        source: [String; 2],
    }
    let prior: Prior = serde_json::from_reader(
        root.open_dir("DS-ResizeClone")
            .map_err(err)?
            .open(BASELINE)
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    if prior.layout[0] != layouts[0] || prior.layout[2] != layouts[2] {
        return Err("Protected A GPT or OS layout differs from the previous verified batch".into());
    }
    let ranges = [
        (OFFSETS[0], SIZES[0]),
        (OFFSETS[1], SIZES[1]),
        (SECOND_PREPARATION_OFFSET, MIB),
    ];
    if !original_gpt_matches(
        &quick_partition_checks::table(B)?,
        &prior.layout[1],
        &ranges,
    )? {
        return Err("The prior B GPT digest cannot be reconstructed from the known additions. No acceptance witness was written.".into());
    }
    for (case, expected) in CASES.iter().zip(&prior.source) {
        let source = source(case)?;
        if hash(&source)? != *expected {
            return Err("An original A source image changed".into());
        }
        verify_sample(case, &source)?;
    }
    let mut records = [0u32; 2];
    for (i, number) in records.iter_mut().enumerate() {
        let row =
            reallocation_checks::metadata_partition(B, None, OFFSETS[i] / MIB, SIZES[i] / MIB)?;
        let wanted = format!("RESTORE-{}", i + 1);
        let valid_fs = if i == 0 {
            matches!(row.base.fs.as_str(), "FAT12" | "FAT")
        } else {
            row.base.fs.eq_ignore_ascii_case("exFAT")
        };
        *number = row.number;
        if row.volume_label.as_deref() != Some(wanted.as_str())
            || !valid_fs
            || target(i, *number)?.disk.is_offline != Some(false)
        {
            return Err("An expected B test target is unavailable".into());
        }
    }
    let boundaries = edges(&mut raw_b()?)?;
    let zero_sector = format!("{:x}", Sha256::digest([0; SECTOR as usize]));
    if boundaries.iter().any(|hash| hash != &zero_sector) {
        return Err("An expected blank boundary outside the test targets contains data".into());
    }
    let archive =
        |i: usize| backup_archive::Archive::open(dir.open(ARCHIVES[i]).map_err(err)?.into_std());
    let mut exact = backup_archive::Chain::new(archive(0)?);
    let mut earlier = backup_archive::Chain::new(archive(1)?);
    if exact.header().sha256 != prior.source[0] || earlier.header().sha256 != prior.source[1] {
        return Err("A prepared archive does not match its full original source".into());
    }
    exact.verify(|_| Ok(()))?;
    earlier.verify(|_| Ok(()))?;
    let mut bytes = Vec::new();
    earlier.reader().read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() != 96 * MIB as usize {
        return Err("Earlier exFAT archive geometry differs".into());
    }
    let serial_earlier = boot_serial(&bytes);
    let serial_latest = if serial_earlier == 437_183_710 {
        73
    } else {
        437_183_710
    };
    set_serial(&mut bytes, serial_latest);
    let mut latest = earlier.prepend(archive(2)?)?;
    if latest.header().sha256 != format!("{:x}", Sha256::digest(&bytes)) {
        return Err("The latest point differs beyond the fixture boot fields".into());
    }
    latest.verify(|_| Ok(()))?;
    let baseline = Baseline {
        partitions: records,
        layouts,
        exact_content: prior.source[0].clone(),
        source_hashes: prior.source,
        backup_hashes: file_digests(&dir)?,
        boundaries,
        blank_resize: hash(&target(1, records[1])?)?,
        serial_earlier,
        serial_latest,
    };
    if ntfs_reconstruction_checks::table_hashes()? != baseline.layouts
        || edges(&mut raw_b()?)? != baseline.boundaries
    {
        return Err("The layout or target boundaries changed during the independent check".into());
    }
    let mut witness = output(&dir, BASELINE)?;
    serde_json::to_writer(&mut witness, &baseline).map_err(err)?;
    witness.sync_all().map_err(err)?;
    Ok(format!("PASS: prior GPT digest reproduced. Archives at {}. Exact B/{} (1 MiB), earlier point B/{} (64 MiB).", path.join(FOLDER).display(), records[0], records[1]))
}

pub(super) fn verify(exact_only: bool) -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(dir.open(BASELINE).map_err(err)?.take(4096)).map_err(err)?;
    if file_digests(&dir)? != baseline.backup_hashes
        || ntfs_reconstruction_checks::table_hashes()? != baseline.layouts
        || edges(&mut raw_b()?)? != baseline.boundaries
    {
        return Err("An archive, protected layout or neighbor sector changed".into());
    }
    for (i, expected) in baseline.source_hashes.iter().enumerate() {
        if hash(&source(&CASES[i])?)? != *expected {
            return Err("A source partition changed".into());
        }
    }
    let exact = target(0, baseline.partitions[0])?;
    let adapted = target(1, baseline.partitions[1])?;
    if exact.disk.is_offline != Some(true)
        || adapted.disk.is_offline != Some(true)
        || exact.disk.is_read_only != Some(false)
        || adapted.disk.is_read_only != Some(false)
    {
        return Err("B was not left writable and offline".into());
    }
    let exact_sha = hash(&exact)?;
    if exact_sha != baseline.exact_content {
        return Err("Exact restored bytes differ".into());
    }
    verify_sample(&CASES[0], &exact)?;
    if exact_only {
        if hash(&adapted)? != baseline.blank_resize {
            return Err("The second target changed before its restore".into());
        }
        allocation_batch::set_online(B)?;
        return Ok(format!("PASS: exact restored partition matched {exact_sha}. Second target unchanged. B is online for the selected earlier point."));
    }
    let mut reader = data_windows::reader(&adapted)?;
    let mut boots = [0u8; 24 * 512];
    reader.read_exact(&mut boots).map_err(err)?;
    let boot = &boots[..12 * 512];
    let serial = boot_serial(boot);
    if boot != &boots[12 * 512..]
        || serial != baseline.serial_earlier
        || serial == baseline.serial_latest
        || u64::from_le_bytes(boot[64..72].try_into().unwrap()) != OFFSETS[1] / SECTOR
        || u64::from_le_bytes(boot[72..80].try_into().unwrap()) != SIZES[1] / SECTOR
    {
        return Err("Resized point has the wrong serial, boot copies or geometry".into());
    }
    verify_sample(&CASES[1], &adapted)?;
    let resized_sha = hash(&adapted)?;
    Ok(format!("PASS: exact {exact_sha}. Earlier exFAT point serial {serial:08x}, resized SHA-256 {resized_sha}. Verified sample, full sources, archives, GPTs, OS layout and neighbors. B remains offline."))
}

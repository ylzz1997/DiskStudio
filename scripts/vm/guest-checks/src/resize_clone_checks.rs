//! Different-sized FAT clone acceptance on fixed disposable A/B ranges.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use partitions::{Action, FileSystem, PartitionKind};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const FOLDER: &str = "DS-ResizeClone";
const BASELINE: &str = "baseline.json";
const SECTOR: u64 = 512;
const SAMPLE: &str = "DSRZSAMP.BIN";

pub(super) struct Case {
    source: u32,
    source_offset: u64,
    source_size: u64,
    target_offset: u64,
    target_size: u64,
    filesystem: FileSystem,
    cluster: u32,
    label: &'static str,
}
pub(super) const CASES: [Case; 2] = [
    Case {
        source: 20,
        source_offset: 12_234_784_768,
        source_size: MIB,
        target_offset: 13 * GIB + 640 * MIB,
        target_size: 2 * MIB,
        filesystem: FileSystem::Fat12,
        cluster: 512,
        label: "CLONE-GROW",
    },
    Case {
        source: 21,
        source_offset: 12_241_076_224,
        source_size: 96 * MIB,
        target_offset: 13 * GIB + 656 * MIB,
        target_size: 64 * MIB,
        filesystem: FileSystem::Exfat,
        cluster: 4096,
        label: "CLONE-SHR",
    },
];

#[derive(serde::Serialize, Deserialize)]
struct Baseline {
    partitions: [u32; 2],
    layout: [String; 3],
    source: [String; 2],
    target: [String; 2],
    boundaries: [String; 4],
}

pub(super) fn source(case: &Case) -> Result<data_windows::Resolved, String> {
    allocation_batch::resolved(
        &selection(A, Some(case.source))?,
        case.source_offset,
        case.source_size,
        false,
    )
}

fn target(case: &Case, number: u32) -> Result<data_windows::Resolved, String> {
    let r = data_windows::resolve(&selection(B, Some(number))?, false, false)?;
    if r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(B)
        || r.offset != case.target_offset
        || r.size != case.target_size
        || r.disk.logical_sector_size != Some(SECTOR)
    {
        return Err("Target fixture identity or geometry changed".into());
    }
    Ok(r)
}

pub(super) fn hash(r: &data_windows::Resolved) -> Result<String, String> {
    data_io::hash_exact(&mut data_windows::reader(r)?, r.size, |_| Ok(()))
}

fn boundaries(file: &mut fs::File) -> Result<[String; 4], String> {
    let mut result = std::array::from_fn(|_| String::new());
    for (case, entries) in CASES.iter().zip(result.chunks_exact_mut(2)) {
        let offsets = [
            case.target_offset - SECTOR,
            case.target_offset + case.target_size,
        ];
        for (offset, output) in offsets.into_iter().zip(entries) {
            let bytes = table_archive::read_at(file, offset, SECTOR as usize, 16 * GIB)?;
            *output = format!("{:x}", Sha256::digest(bytes));
        }
    }
    Ok(result)
}

pub(super) fn verify_sample(case: &Case, r: &data_windows::Resolved) -> Result<(), String> {
    let mut reader = data_windows::reader(r)?;
    let mut files = Vec::new();
    recovery::scan_filesystem(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| files.push(file),
    )?;
    let ids: Vec<_> = files
        .iter()
        .filter(|file| file.name.eq_ignore_ascii_case(SAMPLE))
        .collect();
    let [sample] = ids.as_slice() else {
        return Err("Expected exactly one live source sample in cloned filesystem".into());
    };
    if sample.size != 65_553 {
        return Err("Sample size changed".into());
    }
    let mut bytes = Vec::new();
    recovery::extract(&mut reader, sample, &mut bytes, sample.size, |_| Ok(()))?;
    if bytes
        .iter()
        .copied()
        .ne((0..65_553).map(|i| ((i * 67 + i / 191 + case.cluster as usize) % 251) as u8))
    {
        return Err("Independent sample bytes differ".into());
    }
    Ok(())
}

fn verify_target(case: &Case, number: u32) -> Result<String, String> {
    let r = target(case, number)?;
    if r.disk.is_offline != Some(true) || r.disk.is_read_only != Some(false) {
        return Err("Clone did not leave B writable and offline".into());
    }
    let mut reader = data_windows::reader(&r)?;
    let mut boot = [0u8; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    let lba = case.target_offset / SECTOR;
    let sectors = case.target_size / SECTOR;
    if case.filesystem == FileSystem::Fat12 {
        let total = u64::from(u16::from_le_bytes(boot[19..21].try_into().unwrap()));
        let hidden = u64::from(u32::from_le_bytes(boot[28..32].try_into().unwrap()));
        if &boot[54..62] != b"FAT12   " || total != sectors || hidden != lba {
            return Err("FAT12 total sectors or hidden sectors differ".into());
        }
    } else {
        let mut backup = [0u8; 512];
        reader.seek(SeekFrom::Start(12 * SECTOR)).map_err(err)?;
        reader.read_exact(&mut backup).map_err(err)?;
        if &boot[3..11] != b"EXFAT   "
            || boot != backup
            || u64::from_le_bytes(boot[64..72].try_into().unwrap()) != lba
            || u64::from_le_bytes(boot[72..80].try_into().unwrap()) != sectors
        {
            return Err("exFAT boot copies, declared offset or volume length differ".into());
        }
    }
    verify_sample(case, &r)?;
    hash(&r)
}

fn test_b() -> Result<fs::File, String> {
    data_windows::device(
        &data_windows::resolve(&selection(B, None)?, false, false)?,
        false,
    )
}

fn member(case: &Case, whole: &Selection) -> Result<u32, String> {
    let snapshot = storage::read_snapshot()?;
    let disk = snapshot
        .disks
        .iter()
        .find(|disk| disk.hardware.identity.as_deref() == Some(&whole.target.0))
        .ok_or("Missing target disk metadata")?;
    let rows: Vec<_> = disk
        .partitions
        .iter()
        .filter(|part| part.base.start == case.target_offset && part.base.size == case.target_size)
        .collect();
    let [row] = rows.as_slice() else {
        return Err("Expected one target partition".into());
    };
    Ok(row.number)
}

fn write_file(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

fn require_blank(file: &mut fs::File, case: &Case) -> Result<(), String> {
    let mut bytes = vec![0; MIB as usize];
    file.seek(SeekFrom::Start(case.target_offset))
        .map_err(err)?;
    for _ in 0..case.target_size / MIB {
        file.read_exact(&mut bytes).map_err(err)?;
        if bytes.iter().any(|&value| value != 0) {
            return Err("Reserved test partition contains data".into());
        }
    }
    Ok(())
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let old = fat_resize_checks::model(B)?;
    let a = fat_resize_checks::model(A)?;
    if old.guid == a.guid
        || old
            .entries
            .iter()
            .any(|p| a.entries.iter().any(|q| p.guid == q.guid))
    {
        return Err("Duplicate GPT identifiers between test disks".into());
    }
    let mut file = test_b()?;
    let before_boundaries = boundaries(&mut file)?;
    for case in &CASES {
        verify_sample(case, &source(case)?)?;
        let end = case.target_offset + case.target_size;
        if case.target_offset / SECTOR <= old.first
            || end / SECTOR > old.last
            || old
                .entries
                .iter()
                .any(|p| p.offset < end + SECTOR && case.target_offset - SECTOR < p.offset + p.size)
        {
            return Err("Reserved B test range overlaps an existing partition".into());
        }
        require_blank(&mut file, case)?;
    }
    drop(file);
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    write_file(&dir, "started", desktop.as_bytes())?;
    allocation_batch::set_online(B)?;
    let mut partitions = [0u32; 2];
    for (case, number) in CASES.iter().zip(&mut partitions) {
        operation(
            B,
            Action::Create {
                offset: case.target_offset,
                size: case.target_size,
                kind: PartitionKind::Windows,
            },
        )?;
        *number = member(case, &selection(B, None)?)?;
        operation(
            B,
            Action::Format {
                partition: *number,
                label: case.label.into(),
                file_system: case.filesystem,
                cluster_size: case.cluster,
                options: Default::default(),
            },
        )?;
    }
    let mut observed_old = fat_resize_checks::model(B)?;
    observed_old.entries.retain(|part| {
        !CASES
            .iter()
            .any(|case| part.offset == case.target_offset && part.size == case.target_size)
    });
    if observed_old != old
        || fat_resize_checks::model(A)? != a
        || boundaries(&mut test_b()?)? != before_boundaries
    {
        return Err(
            "An existing partition or boundary sector changed while preparing targets".into(),
        );
    }
    save_baseline(&dir, partitions, before_boundaries)
}

fn save_baseline(
    dir: &Dir,
    partitions: [u32; 2],
    boundaries: [String; 4],
) -> Result<String, String> {
    let baseline = Baseline {
        partitions,
        layout: ntfs_reconstruction_checks::table_hashes()?,
        source: [hash(&source(&CASES[0])?)?, hash(&source(&CASES[1])?)?],
        target: [
            hash(&target(&CASES[0], partitions[0])?)?,
            hash(&target(&CASES[1], partitions[1])?)?,
        ],
        boundaries,
    };
    write_file(dir, BASELINE, &serde_json::to_vec(&baseline).map_err(err)?)?;
    Ok(format!("PASS: clone A/20 FAT12 to B/{} (1 to 2 MiB), then A/21 exFAT to B/{} (96 to 64 MiB). Both fresh targets verified.", partitions[0], partitions[1]))
}

// The first staging run created both entries but exFAT rejected its label
// before issuing a format token. Accept only that still-blank second entry.
pub(super) fn continue_preparation() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let mut started = String::new();
    dir.open("started")
        .map_err(err)?
        .take(65)
        .read_to_string(&mut started)
        .map_err(err)?;
    if started != desktop || dir.symlink_metadata(BASELINE).is_ok() {
        return Err("No matching interrupted fixture preparation".into());
    }
    let disk = selection(B, None)?;
    let partitions = [member(&CASES[0], &disk)?, member(&CASES[1], &disk)?];
    let before = ntfs_reconstruction_checks::table_hashes()?;
    let fat = data_windows::resolve(&selection(B, Some(partitions[0]))?, true, false)?;
    if fat.disk.is_offline != Some(false)
        || !matches!(
            fat.partition_filesystem(partitions[0]),
            Some("FAT" | "FAT12")
        )
        || fat.offset != CASES[0].target_offset
        || fat.size != CASES[0].target_size
    {
        return Err("The first target no longer matches the prepared FAT12 fixture".into());
    }
    let mut boot = [0u8; 512];
    data_windows::reader(&fat)?
        .read_exact(&mut boot)
        .map_err(err)?;
    if &boot[43..54] != b"CLONE-GROW " || &boot[54..62] != b"FAT12   " {
        return Err("The first target boot identity differs".into());
    }
    let fat_before = hash(&fat)?;
    let mut file = test_b()?;
    let edges = boundaries(&mut file)?;
    require_blank(&mut file, &CASES[1])?;
    drop(file);
    for case in &CASES {
        verify_sample(case, &source(case)?)?;
    }
    write_file(&dir, "continued", desktop.as_bytes())?;
    let case = &CASES[1];
    operation(
        B,
        Action::Format {
            partition: partitions[1],
            label: case.label.into(),
            file_system: case.filesystem,
            cluster_size: case.cluster,
            options: Default::default(),
        },
    )?;
    if ntfs_reconstruction_checks::table_hashes()? != before
        || boundaries(&mut test_b()?)? != edges
        || hash(&target(&CASES[0], partitions[0])?)? != fat_before
    {
        return Err("Formatting the blank target changed protected bytes".into());
    }
    save_baseline(&dir, partitions, edges)
}

pub(super) fn verify(growth_only: bool) -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(dir.open(BASELINE).map_err(err)?.take(4096)).map_err(err)?;
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    if tables != baseline.layout || boundaries(&mut test_b()?)? != baseline.boundaries {
        return Err("A/B GPT, adjacent sectors or guest OS metadata changed".into());
    }
    for (index, case) in CASES.iter().enumerate() {
        let expected = &baseline.source[index];
        if hash(&source(case)?)? != *expected {
            return Err("A source partition bytes changed".into());
        }
    }
    let grown = verify_target(&CASES[0], baseline.partitions[0])?;
    if growth_only {
        if hash(&target(&CASES[1], baseline.partitions[1])?)? != baseline.target[1] {
            return Err("The second target changed before its job".into());
        }
        allocation_batch::set_online(B)?;
        return Ok(format!("PASS: FAT12 grown with matching sample, boot geometry and SHA-256 {grown}. B is online for the next explicit case."));
    }
    let shrunk = verify_target(&CASES[1], baseline.partitions[1])?;
    Ok(format!("PASS: both cloned boot layouts and fully extracted sample files match. A source bytes, A/B GPT, OS partition metadata, and target boundary sectors match baseline. B remains offline. Final SHA-256 FAT12 {grown}, exFAT {shrunk}."))
}

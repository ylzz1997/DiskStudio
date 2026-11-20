//! VM fixture and independent readback for file-based and capacity restores.
use super::*;
use cap_std::fs::{Dir, OpenOptions as DirOptions};
use disk_core::MIB;
use partitions::{policy, Action, FileSystem, PartitionKind};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read, Seek, SeekFrom};

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fat_fixture;

const SECTOR: u64 = 4096;
const FOLDER: &str = "DS-CapacityRestore";
const ARCHIVES: [&str; 3] = ["NTFS.dsbackup", "FAT32.dsbackup", "exFAT.dsbackup"];
const OFFSETS: [u64; 3] = [256 * MIB, 512 * MIB, 1024 * MIB];
const SIZES: [u64; 3] = [48 * MIB, 260 * MIB, 6 * MIB];
const LABELS: [&str; 3] = ["RESTNTFS", "RESTFAT32", "RESTEXFAT"];
const SOURCE_OFFSET: u64 = 32 * MIB;
const SOURCE_SIZE: u64 = 96 * MIB;

#[derive(serde::Serialize, Deserialize)]
struct Witness {
    desktop: String,
    tables: [String; 3],
    archives: [String; 3],
    edges: [[String; 2]; 3],
    ntfs_serial: u64,
    fat_tail: u32,
    exfat_serial: u32,
}

pub(super) fn fat_tail() -> (Vec<u8>, u32) {
    let sector = SECTOR as usize;
    let mut image = fat_fixture::fat32(sector);
    let reserved = u16::from_le_bytes(image[14..16].try_into().unwrap()) as usize;
    let fat = u32::from_le_bytes(image[36..40].try_into().unwrap()) as usize;
    let heap = (reserved + 2 * fat) * sector;
    let count = ((image.len() - heap) / sector) as u32;
    let last = count + 1;
    for copy in [reserved, reserved + fat] {
        let first = copy * sector;
        image[first + 12..first + 16].fill(0);
        let at = first + last as usize * 4;
        image[at..at + 4].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
    }
    let record = heap + 32;
    image[record + 20..record + 22].copy_from_slice(&((last >> 16) as u16).to_le_bytes());
    image[record + 26..record + 28].copy_from_slice(&(last as u16).to_le_bytes());
    let tail = image.len() - sector;
    image.copy_within(heap + sector..heap + 2 * sector, tail);
    image[heap + sector..heap + 2 * sector].fill(0);
    let info = u16::from_le_bytes(image[48..50].try_into().unwrap()) as usize;
    let mirror = u16::from_le_bytes(image[50..52].try_into().unwrap()) as usize;
    for at in [info * sector, (mirror + info) * sector] {
        image[at + 488..at + 492].copy_from_slice(&(count - 2).to_le_bytes());
        image[at + 492..at + 496].copy_from_slice(&3u32.to_le_bytes());
    }
    (image, last)
}

fn candidates() -> Result<(Vec<u8>, u32, Vec<u8>), String> {
    let (fat, tail) = fat_tail();
    let plan =
        fat_recovery::resize::clone::prepare(&mut Cursor::new(&fat), SECTOR, SIZES[1], OFFSETS[1])?;
    if plan.filesystem != "FAT32" || plan.moved_clusters() != 1 {
        return Err("The FAT32 fixture must relocate one live cluster when shrunk".into());
    }
    let exfat = fat_fixture::exfat(SECTOR as usize);
    fat_recovery::resize::file_clone::prepare(
        &mut Cursor::new(&exfat),
        SECTOR,
        SIZES[2],
        OFFSETS[2],
        5,
        &AtomicBool::new(false),
    )?;
    Ok((fat, tail, exfat))
}

pub(super) fn create(offset: u64, size: u64, fs: FileSystem, label: &str) -> Result<u32, String> {
    operation(
        B,
        Action::Create {
            offset,
            size,
            kind: PartitionKind::Windows,
        },
    )?;
    let number = partition_at(offset, size)?;
    format(number, fs, label)?;
    Ok(number)
}

fn partition_at(offset: u64, size: u64) -> Result<u32, String> {
    let dst = data_windows::resolve(&selection(B, None)?, false, false)?;
    let parts = quick_partition_checks::parts()?;
    let matches = parts
        .iter()
        .filter(|part| {
            part.disk_number == dst.disk.number && part.offset == offset && part.size == size
        })
        .collect::<Vec<_>>();
    let [part] = matches.as_slice() else {
        return Err("Expected one exact B partition".into());
    };
    Ok(part.partition_number)
}

fn format(number: u32, fs: FileSystem, label: &str) -> Result<(), String> {
    operation(
        B,
        Action::Format {
            partition: number,
            label: label.into(),
            file_system: fs,
            cluster_size: SECTOR as u32,
            options: Default::default(),
        },
    )
}

pub(super) fn backup(
    dir: &Dir,
    name: &str,
    mut source: impl Read,
    size: u64,
    kind: &str,
) -> Result<(), String> {
    let mut file = dir
        .open_with(name, DirOptions::new().write(true).create_new(true))
        .map_err(err)?
        .into_std();
    backup_archive::create(
        &mut source,
        &mut file,
        kind,
        size,
        SECTOR,
        true,
        None,
        |_| Ok(()),
    )?;
    Ok(())
}

pub(super) fn archive_hash(dir: &Dir, name: &str) -> Result<String, String> {
    let mut file = dir.open(name).map_err(err)?.into_std();
    let size = file.metadata().map_err(err)?.len();
    data_io::hash_exact(&mut file, size, |_| Ok(()))
}

fn archive_hashes(dir: &Dir) -> Result<[String; 3], String> {
    let [first, second, third] = ARCHIVES;
    Ok([
        archive_hash(dir, first)?,
        archive_hash(dir, second)?,
        archive_hash(dir, third)?,
    ])
}

pub(super) fn b_device(offline: bool) -> Result<fs::File, String> {
    let r = data_windows::resolve(&selection(B, None)?, false, false)?;
    if r.size != 16 * GIB
        || r.disk.is_offline != Some(offline)
        || r.disk.serial_number.as_deref().map(policy::clean) != Some(B)
    {
        return Err("Unexpected B disk or online status".into());
    }
    let file = data_windows::device(&r, false)?;
    if offline {
        data_windows::verify_offline_handle(&r, &file)?;
    }
    Ok(file)
}

pub(super) fn region(
    file: &fs::File,
    start: u64,
    size: u64,
) -> Result<data_io::Region<&fs::File>, String> {
    data_io::Region::new(file, start, size).map_err(err)
}

pub(super) fn hash(file: &fs::File, start: u64, size: u64) -> Result<String, String> {
    data_io::hash_exact(&mut region(file, start, size)?, size, |_| Ok(()))
}

fn edges(file: &fs::File) -> Result<[[String; 2]; 3], String> {
    let mut values = std::array::from_fn(|_| [String::new(), String::new()]);
    for (index, result) in values.iter_mut().enumerate() {
        *result = [
            hash(file, OFFSETS[index] - SECTOR, SECTOR)?,
            hash(file, OFFSETS[index] + SIZES[index], SECTOR)?,
        ];
    }
    Ok(values)
}

fn prepare_archives(fat: &[u8], exfat: &[u8]) -> Result<(), String> {
    let mut log = backup_protection::begin("file-and-capacity-restore-4kn")?;
    online(A)?;
    operation(A, Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (FileSystem::Ntfs, "DSRESTORE", SECTOR as u32),
        32 * MIB,
        2 * GIB,
    )?;
    online(B)?;
    operation(B, Action::Initialize {})?;
    let source_id = create(SOURCE_OFFSET, SOURCE_SIZE, FileSystem::Ntfs, "RESTSOURCE")?;
    let selected = selection(B, Some(source_id))?;
    let source = data_windows::resolve(&selected, true, false)?;
    let drive_letter = quick_partition_checks::parts()?
        .into_iter()
        .find(|p| p.disk_number == source.disk.number && p.partition_number == source_id)
        .and_then(|p| p.drive_letter);
    ntfs_file_clone_checks::seed(&selected, &source, drive_letter.as_deref())?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let locked = data_windows::StableRead::lock(&selected)?;
    backup(
        &dir,
        ARCHIVES[0],
        locked.reader()?,
        SOURCE_SIZE,
        "partition",
    )?;
    locked.verify()?;
    drop(locked);
    backup(&dir, ARCHIVES[1], fat, fat.len() as u64, "partition")?;
    backup(&dir, ARCHIVES[2], exfat, exfat.len() as u64, "partition")
}

fn verify_sources(dir: &Dir, fat: &[u8], exfat: &[u8]) -> Result<u64, String> {
    let open = |name| {
        let file = dir.open(name).map_err(err)?.into_std();
        backup_archive::Archive::open(file).map(backup_archive::Chain::new)
    };
    for (name, expected) in [(ARCHIVES[1], fat), (ARCHIVES[2], exfat)] {
        let mut chain = open(name)?;
        if chain.header().size != expected.len() as u64
            || chain.header().sector != SECTOR
            || chain.header().kind != "partition"
            || chain.header().sha256 != format!("{:x}", Sha256::digest(expected))
        {
            return Err("A prepared FAT or exFAT archive no longer matches its fixture".into());
        }
        chain.verify(|_| Ok(()))?;
    }
    let mut ntfs = open(ARCHIVES[0])?;
    if ntfs.header().size != SOURCE_SIZE
        || ntfs.header().sector != SECTOR
        || ntfs.header().kind != "partition"
    {
        return Err("The NTFS source archive has unexpected geometry".into());
    }
    ntfs.verify(|_| Ok(()))?;
    let mut reader = ntfs.reader();
    ntfs_file_clone_checks::sample(&partitions::merge::catalogue_file_clone(
        &mut reader,
        &AtomicBool::new(false),
    )?)?;
    Ok(ntfs::Ntfs::new(&mut reader).map_err(err)?.serial_number())
}

fn existing_target() -> Result<u32, String> {
    let before = fat_resize_checks::model(B)?;
    if before.entries.len() != 3
        || ![(SOURCE_OFFSET, SOURCE_SIZE), (OFFSETS[0], SIZES[0])]
            .into_iter()
            .all(|(offset, size)| {
                before.entries.iter().any(|entry| {
                    entry.kind == policy::BASIC_DATA && entry.offset == offset && entry.size == size
                })
            })
    {
        return Err("The first preparation's B layout does not match the known stop".into());
    }
    let file = b_device(false)?;
    let mut target = region(&file, OFFSETS[0], SIZES[0])?;
    let mut bytes = [0u8; 64 * 1024];
    for _ in 0..SIZES[0] / bytes.len() as u64 {
        target.read_exact(&mut bytes).map_err(err)?;
        if bytes.iter().any(|&byte| byte != 0) {
            return Err("The existing B target is not empty".into());
        }
    }
    partition_at(OFFSETS[0], SIZES[0])
}

pub(super) fn prepare(continued: bool) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    let (fat, fat_tail, exfat) = candidates()?;
    if !continued {
        for serial in [A, B] {
            whole_file_clone_checks::empty_raw(&disks, serial)?;
        }
        prepare_archives(&fat, &exfat)?;
    }
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let ntfs_serial = verify_sources(&dir, &fat, &exfat)?;
    if dir.entries().map_err(err)?.count() != ARCHIVES.len() {
        return Err("The fixture folder already contains a baseline or unexpected entries".into());
    }
    let mut partitions = [0; 3];
    if continued {
        let destination = existing_target()?;
        backup_protection::begin("file-and-capacity-restore-4kn-continue")?;
        format(destination, FileSystem::Fat16, LABELS[0])?;
        partitions[0] = destination;
    } else {
        partitions[0] = create(OFFSETS[0], SIZES[0], FileSystem::Fat16, LABELS[0])?;
    }
    for (index, fs) in [(1, FileSystem::Ntfs), (2, FileSystem::Fat12)] {
        partitions[index] = create(OFFSETS[index], SIZES[index], fs, LABELS[index])?;
    }
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    if tables[2] != os {
        return Err("Guest OS layout changed".into());
    }
    let witness = Witness {
        desktop,
        tables,
        archives: archive_hashes(&dir)?,
        edges: edges(&b_device(false)?)?,
        ntfs_serial,
        fat_tail,
        exfat_serial: u32::from_le_bytes(exfat[100..104].try_into().unwrap()),
    };
    let mut output = dir.create("baseline.json").map_err(err)?;
    serde_json::to_writer(&mut output, &witness).map_err(err)?;
    output.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: {}. Restore NTFS by files to B/{} (48 MiB); resize FAT32 to B/{} (260 MiB, one moved cluster); restore exFAT by files to B/{} (6 MiB).",
        path.join(FOLDER).display(), partitions[0], partitions[1], partitions[2],
    ))
}

pub(super) fn verify_file(
    reader: &mut (impl Read + Seek),
    name: &str,
    expected: &[u8],
) -> Result<(), String> {
    let mut candidates = Vec::new();
    recovery::scan_filesystem(
        reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |candidate| candidates.push(candidate),
    )?;
    let matches = candidates
        .iter()
        .filter(|entry| entry.name.eq_ignore_ascii_case(name))
        .collect::<Vec<_>>();
    let [entry] = matches.as_slice() else {
        return Err(format!("Missing or ambiguous restored file: {name}"));
    };
    let mut actual = Vec::new();
    recovery::extract(reader, entry, &mut actual, entry.size, |_| Ok(()))?;
    if actual != expected {
        return Err(format!("Restored file differs: {name}"));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Ntfs,
    Fat32,
    Exfat,
}

pub(super) fn record_receipt(dir: &Dir, receipt: &str, digest: &str) -> Result<(), String> {
    let mut witness = dir
        .open_with(receipt, DirOptions::new().write(true).create_new(true))
        .map_err(err)?;
    witness.write_all(digest.as_bytes()).map_err(err)?;
    witness.sync_all().map_err(err)?;
    Ok(())
}

pub(super) fn advance(
    dir: &Dir,
    receipt: &str,
    digest: &str,
    file: fs::File,
) -> Result<(), String> {
    drop(file);
    record_receipt(dir, receipt, digest)?;
    online(B)?;
    drop(b_device(false)?);
    Ok(())
}

pub(super) fn receipt_digest(dir: &Dir, name: &str) -> Result<String, String> {
    let mut digest = String::with_capacity(64);
    dir.open(name)
        .map_err(err)?
        .take(65)
        .read_to_string(&mut digest)
        .map_err(err)?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("Invalid readback receipt: {name}"));
    }
    Ok(digest)
}

pub(super) fn verify(stage: Stage) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let before: Witness =
        serde_json::from_reader(dir.open("baseline.json").map_err(err)?).map_err(err)?;
    let file = b_device(true)?;
    if before.desktop != desktop
        || before.archives != archive_hashes(&dir)?
        || before.tables != ntfs_reconstruction_checks::table_hashes()?
        || before.edges != edges(&file)?
    {
        return Err("Desktop, archive, partition table or adjacent sectors changed".into());
    }
    let cancel = AtomicBool::new(false);
    let mut source = region(&file, SOURCE_OFFSET, SOURCE_SIZE)?;
    ntfs_file_clone_checks::sample(&partitions::merge::catalogue_file_clone(
        &mut source,
        &cancel,
    )?)?;
    let mut ntfs = region(&file, OFFSETS[0], SIZES[0])?;
    if ntfs::Ntfs::new(&mut ntfs).map_err(err)?.serial_number() == before.ntfs_serial {
        return Err("The NTFS restore did not generate a new volume ID".into());
    }
    ntfs_file_clone_checks::sample(&partitions::merge::catalogue_file_clone(
        &mut ntfs, &cancel,
    )?)?;
    if stage == Stage::Ntfs {
        let digest = hash(&file, OFFSETS[0], SIZES[0])?;
        advance(&dir, "after-ntfs.sha256", &digest, file)?;
        return Ok(format!(
            "PASS: NTFS sample (named stream, hard link and empty directory), new volume ID, GPTs, archives, OS layout and adjacent sectors. B was offline for readback and is online for FAT32.\n\n48 MiB SHA-256\n{digest}"
        ));
    }
    let ntfs_sha = receipt_digest(&dir, "after-ntfs.sha256")?;
    let mut fat = region(&file, OFFSETS[1], SIZES[1])?;
    let mut boot = [0; SECTOR as usize];
    fat.read_exact(&mut boot).map_err(err)?;
    let total = u32::from_le_bytes(boot[32..36].try_into().unwrap());
    let reserved = u16::from_le_bytes(boot[14..16].try_into().unwrap()) as u64;
    let fat_sectors = u32::from_le_bytes(boot[36..40].try_into().unwrap()) as u64;
    let root_offset = (reserved + 2 * fat_sectors) * SECTOR;
    fat.seek(SeekFrom::Start(root_offset + 32)).map_err(err)?;
    let mut entry = [0; 32];
    fat.read_exact(&mut entry).map_err(err)?;
    let cluster = (u16::from_le_bytes(entry[20..22].try_into().unwrap()) as u32) << 16
        | u16::from_le_bytes(entry[26..28].try_into().unwrap()) as u32;
    if u64::from(total) * SECTOR != SIZES[1]
        || u32::from_le_bytes(boot[67..71].try_into().unwrap()) != 0x37be_24d7
        || cluster < 2
        || cluster >= before.fat_tail
        || &entry[..11] != b"FATVIEW TXT"
    {
        return Err("The FAT32 capacity, original serial or relocated file cluster differs".into());
    }
    fat.seek(SeekFrom::Start(0)).map_err(err)?;
    verify_file(&mut fat, "FATVIEW.TXT", fat_fixture::FAT_CONTENT)?;
    if stage == Stage::Fat32 {
        let digest = hash(&file, OFFSETS[1], SIZES[1])?;
        advance(&dir, "after-fat32.sha256", &digest, file)?;
        return Ok(format!(
            "PASS: FAT32 cluster {} to {cluster}, file content, original volume ID, GPTs, archives, OS layout and adjacent sectors. B was offline for readback and is online for exFAT.\n\n260 MiB SHA-256\n{digest}",
            before.fat_tail,
        ));
    }
    let fat_sha = receipt_digest(&dir, "after-fat32.sha256")?;
    let mut exfat = region(&file, OFFSETS[2], SIZES[2])?;
    exfat.read_exact(&mut boot).map_err(err)?;
    if u64::from_le_bytes(boot[72..80].try_into().unwrap()) * SECTOR != SIZES[2]
        || u32::from_le_bytes(boot[100..104].try_into().unwrap()) == before.exfat_serial
    {
        return Err("The exFAT capacity or new serial differs".into());
    }
    exfat.seek(SeekFrom::Start(0)).map_err(err)?;
    verify_file(&mut exfat, "exfat-view.txt", fat_fixture::EXFAT_CONTENT)?;
    let exfat_sha = hash(&file, OFFSETS[2], SIZES[2])?;
    Ok(format!(
        "PASS: NTFS sample, FAT32 cluster {} to {cluster}, exFAT file and new volume ID. GPTs, archives, OS layout and adjacent sectors verified. B stays offline.\n\nSHA-256 at each offline readback\nNTFS {ntfs_sha}\nFAT32 {fat_sha}\nexFAT {exfat_sha}",
        before.fat_tail,
    ))
}

#[cfg(test)]
#[test]
fn synthetic_fat_and_exfat_archives_fit_the_native_targets() {
    candidates().unwrap();
}

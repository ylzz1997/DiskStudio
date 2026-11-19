//! Native Btrfs UI fixture and independent readback for the isolated 4Kn A disk.
use super::*;
use disk_core::MIB;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

#[path = "btrfs_allocation_checks.rs"]
pub(super) mod allocation;
#[path = "btrfs_lost_checks.rs"]
pub(super) mod lost;
#[path = "lvm_checks.rs"]
pub(super) mod lvm;

const CREATE_SIZE: u64 = 512 * MIB;
const SHRUNK: u64 = 256 * MIB;
const GROWN: u64 = 768 * MIB;
const LABEL: &str = "DSBTRFS4KN";
const MOVE_SIZE: u64 = 384 * MIB;
const MOVE_TARGET: u64 = 192 * MIB;
const MOVE_LABEL: &str = "DSBTRFSMOVE";
const MOVE_BYTES: u64 = 80 * MIB;
const MOVE_IMAGE: &str = "Btrfs-Move-Source.raw";
const PREVIOUS_DESKTOP: &str = "28ac3b7b7c2800601f37dc444b26afd9965e4e1b10c439df31028e9008f2eec3";

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Witness {
    partition: u32,
    offset: u64,
    other_layout: Vec<(u32, u32, u64, u64, String)>,
    os_layout: String,
}

fn desktop() -> Result<String, String> {
    inventory()?;
    let expected = option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        .ok_or("Expected the desktop build SHA-256")?;
    let mut binary = File::open(
        std::env::current_exe()
            .map_err(err)?
            .with_file_name("DiskStudio.exe"),
    )
    .map_err(err)?;
    let mut digest = Sha256::new();
    let mut bytes = vec![0; 1024 * 1024];
    loop {
        let count = binary.read(&mut bytes).map_err(err)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err("Desktop payload differs from the accepted build".into());
    }
    Ok(expected.to_owned())
}

fn report_file(sha: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\ProgramData\DiskStudio-btrfs-4kn-{sha}.json"))
}

fn test_a() -> Result<Disk, String> {
    test_disk(A)
}

fn test_disk(serial: &str) -> Result<Disk, String> {
    if ![A, B].contains(&serial) {
        return Err("Only the isolated test A/B disks are allowed".into());
    }
    inventory()?
        .into_iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or("Exact test disk serial is missing".into())
}

fn witness(partition: u32, offset: u64) -> Result<Witness, String> {
    witness_on(A, partition, offset)
}

fn witness_on(serial: &str, partition: u32, offset: u64) -> Result<Witness, String> {
    Ok(Witness {
        partition,
        offset,
        other_layout: format_checks::layouts(&[(test_disk(serial)?.number, partition)])?,
        os_layout: reallocation_checks::os_layout(&inventory()?)?,
    })
}

pub(super) fn create() -> Result<String, String> {
    let sha = desktop()?;
    let mut log = backup_protection::begin("btrfs-4kn-create")?;
    if report_file(&sha).exists() {
        return Err("A Btrfs fixture has already been allocated".into());
    }
    online(A)?;
    let status = partitions::status();
    let target = status
        .targets
        .iter()
        .find(|t| t.serial == A)
        .ok_or("A unavailable")?;
    let gap = target
        .free_ranges
        .iter()
        .find(|range| range.size >= GROWN)
        .ok_or("A needs one fresh contiguous 768 MiB extent")?;
    let baseline = witness(0, gap.offset)?;
    operation(
        A,
        partitions::Action::Create {
            offset: gap.offset,
            size: CREATE_SIZE,
            kind: partitions::PartitionKind::Linux,
        },
    )?;
    let selected =
        reallocation_checks::metadata_partition(A, None, gap.offset / MIB, CREATE_SIZE / MIB)?;
    let created = witness(selected.number, gap.offset)?;
    if created.other_layout != baseline.other_layout || created.os_layout != baseline.os_layout {
        return Err("Existing partition layout changed while creating the test fixture".into());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report_file(&sha))
        .map_err(err)?;
    serde_json::to_writer(&mut file, &created).map_err(err)?;
    file.flush().map_err(err)?;
    file.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({ "partition": created.partition, "offsetMiB": created.offset / MIB, "sizeMiB": CREATE_SIZE / MIB }),
    )?;
    Ok(format!("PASS. Format test A partition {} at {} MiB on the matching desktop. Set the Btrfs label to {LABEL}.", created.partition, created.offset / MIB))
}

fn verify_fixture(expected: Option<u64>) -> Result<(Witness, u64), String> {
    verify_fixture_for(&desktop()?, expected)
}

fn verify_fixture_for(sha: &str, expected: Option<u64>) -> Result<(Witness, u64), String> {
    let saved: Witness =
        serde_json::from_reader(File::open(report_file(sha)).map_err(err)?).map_err(err)?;
    if witness(saved.partition, saved.offset)? != saved {
        return Err("Existing A/B or guest OS partition metadata changed".into());
    }
    let authorized = selection(A, Some(saved.partition))?;
    let resolved = data_windows::resolve(&authorized, false, false)?;
    if resolved.offset != saved.offset
        || ![CREATE_SIZE, SHRUNK, GROWN].contains(&resolved.size)
        || expected.is_some_and(|size| size != resolved.size)
    {
        return Err("Btrfs fixture no longer has the expected geometry".into());
    }
    let mut reader = data_windows::reader(&resolved)?;
    verify_empty(&mut reader, resolved.size)?;
    Ok((saved, resolved.size))
}

fn verify_empty(reader: &mut (impl Read + Seek), size: u64) -> Result<(), String> {
    reader.seek(SeekFrom::Start(65536 + 0x12b)).map_err(err)?;
    let mut label = [0; 256];
    reader.read_exact(&mut label).map_err(err)?;
    if label.split(|byte| *byte == 0).next() != Some(LABEL.as_bytes()) {
        return Err("Btrfs fixture label differs".into());
    }
    let mut volume = disk_core::btrfs::open(&mut *reader, || Ok(()))?;
    let [tree] = volume.inventory().subvolumes.as_slice() else {
        return Err("Expected the fixture's single file tree".into());
    };
    let root = tree.root_directory;
    if !volume.read_directory(5, root, || Ok(()))?.is_empty() {
        return Err("Expected the empty native UI fixture".into());
    }
    // Plan the alternate allowed end to verify both supers, the device tree,
    // and the current committed size against the actual GPT extent.
    let alternate = if size == GROWN { SHRUNK } else { GROWN };
    disk_core::btrfs::plan_resize(reader, alternate)?;
    Ok(())
}

pub(super) fn inspect() -> Result<String, String> {
    let (saved, size) = verify_fixture(None)?;
    if test_a()?.is_offline != Some(true) {
        return Err("The operated disk did not remain offline".into());
    }
    Ok(format!("PASS. A partition {} at {} MiB has a verified {} MiB Btrfs filesystem. A is offline. Other GPT entries are unchanged.", saved.partition, saved.offset / MIB, size / MIB))
}

pub(super) fn online_fixture() -> Result<String, String> {
    let (saved, size) = verify_fixture(None)?;
    if ![CREATE_SIZE, SHRUNK].contains(&size) || test_a()?.is_offline != Some(true) {
        return Err("Only the verified format or shrink state may be brought online".into());
    }
    online(A)?;
    Ok(format!(
        "PASS. Test A is online. Continue on partition {} only. Current size {} MiB.",
        saved.partition,
        size / MIB
    ))
}

#[derive(Serialize, Deserialize)]
struct MoveWitness {
    layout: Witness,
    contents: String,
}

fn move_report(sha: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-move-4kn-{sha}.json"
    ))
}

fn move_contents(reader: &mut (impl Read + Seek)) -> Result<String, String> {
    reader.seek(SeekFrom::Start(65536 + 0x12b)).map_err(err)?;
    let mut label = [0; 256];
    reader.read_exact(&mut label).map_err(err)?;
    if label.split(|&byte| byte == 0).next() != Some(MOVE_LABEL.as_bytes()) {
        return Err("Unexpected relocation fixture label".into());
    }
    let mut volume = disk_core::btrfs::open(reader, || Ok(()))?;
    let [tree] = volume.inventory().subvolumes.as_slice() else {
        return Err("Expected one source file tree".into());
    };
    let mut entries = volume.read_directory(tree.id, tree.root_directory, || Ok(()))?;
    if entries.len() != 51 || entries.iter().any(|item| item.file_kind != 1) {
        return Err("Expected 51 regular files in the relocation fixture".into());
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut digest = Sha256::new();
    let mut total = 0;
    for item in entries {
        let mut data = Vec::new();
        volume.read_file(5, item.target.object_id, MOVE_SIZE, &mut data, || Ok(()))?;
        digest.update((item.name.len() as u64).to_le_bytes());
        digest.update(item.name);
        digest.update((data.len() as u64).to_le_bytes());
        digest.update(&data);
        total += data.len();
    }
    if total != 27_430_912 {
        return Err("Unexpected relocation fixture content length".into());
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn create_linux_fixture(serial: &str, size: u64) -> Result<Witness, String> {
    test_disk(serial)?;
    online(serial)?;
    let status = partitions::platform::status()?;
    let target = status
        .targets
        .iter()
        .find(|t| t.serial == serial)
        .ok_or("Test disk has no verified partition management target")?;
    if let Some(reason) = &target.reason {
        return Err(format!(
            "Test disk is unavailable for fixture creation: {reason}"
        ));
    }
    let offset = target
        .free_ranges
        .iter()
        .find(|range| range.size >= size)
        .ok_or_else(|| {
            format!(
                "Test disk needs one fresh contiguous {} MiB extent",
                size / MIB
            )
        })?
        .offset;
    let baseline = witness_on(serial, 0, offset)?;
    operation(
        serial,
        partitions::Action::Create {
            offset,
            size,
            kind: partitions::PartitionKind::Linux,
        },
    )?;
    let selected = reallocation_checks::metadata_partition(serial, None, offset / MIB, size / MIB)?;
    let layout = witness_on(serial, selected.number, offset)?;
    if layout.other_layout != baseline.other_layout || layout.os_layout != baseline.os_layout {
        return Err("Unexpected layout after creating the fixture entry".into());
    }
    Ok(layout)
}

pub(super) fn prepare_move(reuse: bool) -> Result<String, String> {
    let sha = desktop()?;
    let expected = option_env!("DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256")
        .ok_or("Missing bound relocation fixture digest")?;
    let exe = std::env::current_exe().map_err(err)?;
    let (name, size, run) = if reuse {
        ("Btrfs-Move-Reuse.raw", GROWN, "btrfs-4kn-relocation-reuse")
    } else {
        (MOVE_IMAGE, MOVE_SIZE, "btrfs-4kn-relocation-prepare")
    };
    let path = exe.with_file_name(name);
    let mut image = File::open(&path).map_err(err)?;
    if image.metadata().map_err(err)?.len() != size
        || allocation_batch::hash_file(&path)? != expected
        || disk_core::btrfs::plan_resize(&mut image, MOVE_TARGET)?.relocation_bytes() != MOVE_BYTES
    {
        return Err("Unexpected source image, layout or amount of relocation".into());
    }
    let contents = move_contents(&mut image)?;
    let mut log = backup_protection::begin(run)?;
    if move_report(&sha).exists() {
        return Err("Relocation fixture has already been prepared".into());
    }
    let layout = if reuse {
        let (original, _) = verify_fixture_for(PREVIOUS_DESKTOP, Some(GROWN))?;
        if (original.partition, original.offset) != (33, 9088 * MIB) {
            return Err("Expected the previous empty DSBTRFS4KN fixture on A/33".into());
        }
        online(A)?;
        if witness(original.partition, original.offset)? != original {
            return Err("Previous fixture layout changed after bringing A online".into());
        }
        original
    } else {
        create_linux_fixture(A, size)?
    };
    let offset = layout.offset;
    let partition = layout.partition;
    seed_linux_fixture(
        A,
        &layout,
        size,
        &mut image,
        expected,
        |region| {
            if reuse {
                verify_empty(region, size)
            } else {
                Ok(())
            }
        },
        |region| {
            if move_contents(region)? != contents {
                return Err("Relocation fixture content differs after seeding".into());
            }
            Ok(())
        },
    )?;
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(move_report(&sha))
        .map_err(err)?;
    serde_json::to_writer(&mut report, &MoveWitness { layout, contents }).map_err(err)?;
    report.sync_all().map_err(err)?;
    online(A)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({
            "partition": partition, "offsetMiB": offset / MIB, "sourceSha256": expected,
            "sizeMiB": size / MIB
        }),
    )?;
    Ok(format!("PASS. In the desktop, select test A partition {partition} at {} MiB and shrink it from {} to 192 MiB. The preview must report 80 MiB of Btrfs relocation.", offset / MIB, size / MIB))
}

fn seed_linux_fixture(
    serial: &str,
    layout: &Witness,
    size: u64,
    image: &mut File,
    expected: &str,
    before: impl FnOnce(&mut data_io::Region<File>) -> Result<(), String>,
    after: impl FnOnce(&mut data_io::Region<File>) -> Result<(), String>,
) -> Result<(), String> {
    test_disk(serial)?;
    let _guard = OPERATION.try_lock().map_err(err)?;
    let offset = layout.offset;
    let source = selection(serial, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    let locks = data_windows::lock(&ctx.resolved)?;
    let (fresh, _, _) = partitions::platform::table_restore_context(&source, Some((&ctx, &locks)))?;
    if fresh.fingerprint != ctx.fingerprint {
        return Err("Test disk layout changed during locking".into());
    }
    let parts = fresh.parts.iter().collect::<Vec<_>>();
    data_windows::verify_locks(&locks, &fresh.resolved, &parts)?;
    let mut device = data_windows::exclusive_device(&fresh.resolved)?;
    let raw = table_archive::capture(
        &mut device,
        16 * GIB,
        4096,
        "a".repeat(64),
        false,
        || Ok(()),
    )?;
    let gpt = table_restore::archive_layout(&raw)?;
    let native = extfs::restore_native(&device, Some(&parts), 4096)?.0;
    if !native.agrees(&gpt)
        || gpt
            .entries
            .iter()
            .filter(|entry| {
                entry.offset == offset
                    && entry.size == size
                    && entry.kind == partitions::policy::LINUX_DATA
                    && entry.attributes == 0
            })
            .count()
            != 1
    {
        return Err("New test extent is not an independently verified Linux GPT partition".into());
    }
    data_windows::offline(&device)?;
    if !extfs::restore_native(&device, Some(&parts), 4096)?
        .0
        .agrees(&gpt)
    {
        return Err("Test disk layout changed after taking it offline".into());
    }
    let mut region = data_io::Region::new(device, offset, size).map_err(err)?;
    before(&mut region)?;
    image.rewind().map_err(err)?;
    region.rewind().map_err(err)?;
    let mut bytes = vec![0; MIB as usize];
    for _ in 0..size / MIB {
        image.read_exact(&mut bytes).map_err(err)?;
        region.write_all(&bytes).map_err(err)?;
    }
    region.flush().map_err(err)?;
    region.rewind().map_err(err)?;
    let mut digest = Sha256::new();
    for _ in 0..size / MIB {
        region.read_exact(&mut bytes).map_err(err)?;
        digest.update(&bytes);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err("Fixture failed raw SHA-256 readback".into());
    }
    after(&mut region)?;
    drop(region);
    drop(locks);
    if witness_on(serial, layout.partition, offset)? != *layout
        || test_disk(serial)?.is_offline != Some(true)
    {
        return Err(
            "Test disk must be offline with the same layout after preparing the fixture".into(),
        );
    }
    Ok(())
}

pub(super) fn inspect_move() -> Result<String, String> {
    let sha = desktop()?;
    let saved: MoveWitness =
        serde_json::from_reader(File::open(move_report(&sha)).map_err(err)?).map_err(err)?;
    let target = saved.layout.partition;
    if witness(target, saved.layout.offset)? != saved.layout || test_a()?.is_offline != Some(true) {
        return Err("Other partitions changed or A did not remain offline".into());
    }
    let resolved = data_windows::resolve(&selection(A, Some(target))?, false, false)?;
    if resolved.size != MOVE_TARGET || resolved.offset != saved.layout.offset {
        return Err("Relocation fixture GPT boundary differs".into());
    }
    let mut region = data_windows::reader(&resolved)?;
    if move_contents(&mut region)? != saved.contents {
        return Err("Relocation fixture file content differs".into());
    }
    disk_core::btrfs::plan_resize(&mut region, MOVE_SIZE)?;
    Ok(format!("PASS. A partition {target} shrank to 192 MiB. Read back all 51 files, content SHA-256 {}. A remains offline. Other partitions and the guest OS layout match the baseline.", saved.contents))
}

const CHECKSUM_SOURCE: u64 = 256 * MIB;
const CHECKSUM_TARGET: u64 = 192 * MIB;
const CHECKSUM_IMAGE: &str = "Btrfs-Sha256-Source.raw";

#[derive(Serialize, Deserialize)]
struct ChecksumWitness {
    layout: Witness,
    a: table_archive::Archive,
    b: table_archive::Archive,
}

fn checksum_report(sha: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-4kn-sha256-{sha}.json"
    ))
}

fn checksum_contents(reader: &mut (impl Read + Seek)) -> Result<(), String> {
    let mut algorithm = [0; 2];
    reader.seek(SeekFrom::Start(65536 + 0xc4)).map_err(err)?;
    reader.read_exact(&mut algorithm).map_err(err)?;
    if u16::from_le_bytes(algorithm) != 2 {
        return Err("Expected the Linux SHA-256 filesystem".into());
    }
    checksum_volume(disk_core::btrfs::open(reader, || Ok(()))?)
}

fn checksum_volume(mut volume: disk_core::btrfs::Volume<impl Read + Seek>) -> Result<(), String> {
    let [tree] = volume.inventory().subvolumes.as_slice() else {
        return Err("Expected one Btrfs top-level file tree".into());
    };
    let mut entries = volume.read_directory(5, tree.root_directory, || Ok(()))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let [inline, nested] = entries.as_slice() else {
        return Err("Expected a regular file and a nested directory".into());
    };
    if inline.name != b"checksum-inline.txt"
        || inline.file_kind != 1
        || nested.name != b"nested"
        || nested.file_kind != 2
    {
        return Err("Unexpected root directory contents".into());
    }
    let contents = volume.read_directory(5, nested.target.object_id, || Ok(()))?;
    let [compressed] = contents.as_slice() else {
        return Err("Expected one compressed file in the nested directory".into());
    };
    if compressed.name != b"checksum-compressed.bin" || compressed.file_kind != 1 {
        return Err("Unexpected compressed file name or type".into());
    }
    for (entry, size, digest) in [
        (
            inline,
            34,
            "ecd1c486cbdd94dc81da14877aef40b5b1974c7f17cd90cc40cdb9827513674b",
        ),
        (
            compressed,
            249257,
            "ae8bbe2a5fea29843fb622e18f84e4470049d85762560aaf8ab74a4f1c61d6c2",
        ),
    ] {
        let mut bytes = Vec::new();
        let file = volume.read_file(5, entry.target.object_id, size + 1, &mut bytes, || Ok(()))?;
        if file.size != size
            || bytes.len() as u64 != size
            || format!("{:x}", Sha256::digest(&bytes)) != digest
        {
            return Err("Btrfs file length or SHA-256 differs from the Linux fixture".into());
        }
    }
    Ok(())
}

pub(super) fn prepare_checksum() -> Result<String, String> {
    let sha = desktop()?;
    let expected = option_env!("DISKSTUDIO_BTRFS_SHA256_SOURCE_SHA256")
        .ok_or("Missing bound SHA-256 fixture image digest")?;
    let path = std::env::current_exe()
        .map_err(err)?
        .with_file_name(CHECKSUM_IMAGE);
    let mut image = File::open(&path).map_err(err)?;
    if image.metadata().map_err(err)?.len() != CHECKSUM_SOURCE
        || allocation_batch::hash_file(&path)? != expected
        || disk_core::btrfs::plan_resize(&mut image, CHECKSUM_TARGET)?.relocation_bytes() != 0
    {
        return Err("Unexpected SHA-256 fixture image or shrink preview".into());
    }
    checksum_contents(&mut image)?;
    let report_path = checksum_report(&sha);
    let mut log = backup_protection::begin("btrfs-4kn-sha256-prepare")?;
    if report_path.exists() {
        return Err("The SHA-256 fixture already exists".into());
    }
    let layout = create_linux_fixture(A, CHECKSUM_SOURCE)?;
    seed_linux_fixture(
        A,
        &layout,
        CHECKSUM_SOURCE,
        &mut image,
        expected,
        |_| Ok(()),
        |region| checksum_contents(region),
    )?;
    let a = quick_partition_checks::table(A)?;
    let b = quick_partition_checks::table(B)?;
    let result = format!(
        "PASS. Select A/{} at {} MiB in the desktop. Shrink the verified SHA-256 Btrfs partition from 256 to 192 MiB.",
        layout.partition, layout.offset / MIB
    );
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report_path)
        .map_err(err)?;
    serde_json::to_writer(&mut report, &ChecksumWitness { layout, a, b }).map_err(err)?;
    report.sync_all().map_err(err)?;
    online(A)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result": "pass", "sourceSha256": expected}),
    )?;
    Ok(result)
}

pub(super) fn inspect_checksum() -> Result<String, String> {
    let sha = desktop()?;
    let saved: ChecksumWitness =
        serde_json::from_reader(File::open(checksum_report(&sha)).map_err(err)?).map_err(err)?;
    let layout = saved.layout;
    if witness(layout.partition, layout.offset)? != layout || test_a()?.is_offline != Some(true) {
        return Err("Other partitions or the guest OS layout changed, or A is online".into());
    }
    let mut expected = table_restore::archive_layout(&saved.a)?;
    let target = expected
        .entries
        .iter_mut()
        .find(|entry| {
            entry.offset == layout.offset
                && entry.size == CHECKSUM_SOURCE
                && entry.kind == partitions::policy::LINUX_DATA
        })
        .ok_or("Missing saved SHA-256 GPT entry")?;
    target.size = CHECKSUM_TARGET;
    if table_restore::archive_layout(&quick_partition_checks::table(A)?)? != expected
        || quick_partition_checks::table(B)?.encode()? != saved.b.encode()?
    {
        return Err("A/B GPT differs from the exact planned end-boundary change".into());
    }
    let source = data_windows::resolve(&selection(A, Some(layout.partition))?, false, false)?;
    if source.size != CHECKSUM_TARGET || source.offset != layout.offset {
        return Err("Unexpected physical SHA-256 fixture extent after shrinking".into());
    }
    let mut region = data_windows::reader(&source)?;
    checksum_contents(&mut region)?;
    disk_core::btrfs::plan_resize(&mut region, CHECKSUM_SOURCE)?;
    Ok(format!(
        "PASS. A/{} is 192 MiB and remains offline. Both files match Linux SHA-256. A has the exact planned GPT change, B and the guest OS layout are unchanged.",
        layout.partition
    ))
}

const HISTORY_SIZE: u64 = 256 * MIB;
const COMPRESSED_BYTES: u64 = 249276;
const COMPRESSED_SHA: &str = "beba164e83ce4162f32975a9a45d1c99ecde33a269c6075b53cf6867c32fec55";

const MEMBER_NAMES: [&str; 2] = ["Btrfs-Member-1.raw", "Btrfs-Member-2.raw"];

#[derive(Serialize, Deserialize)]
struct MemberWitness {
    members: [Witness; 2],
    tables: [String; 2],
}

fn member_report(sha: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-4kn-members-{sha}.json"
    ))
}

fn member_images(names: [&str; 2]) -> Result<(Vec<File>, [&'static str; 2]), String> {
    let first =
        option_env!("DISKSTUDIO_BTRFS_MEMBER_1_SHA256").ok_or("Missing first media digest")?;
    let second =
        option_env!("DISKSTUDIO_BTRFS_MEMBER_2_SHA256").ok_or("Missing second media digest")?;
    let expected = [first, second];
    let exe = std::env::current_exe().map_err(err)?;
    let mut readers = Vec::new();
    for (name, digest) in names.into_iter().zip(expected) {
        let path = exe.with_file_name(name);
        let file = File::open(&path).map_err(err)?;
        if file.metadata().map_err(err)?.len() != HISTORY_SIZE
            || allocation_batch::hash_file(&path)? != digest
        {
            return Err(format!("Unexpected media file {name}"));
        }
        readers.push(file);
    }
    Ok((readers, expected))
}

fn member_contents<R: Read + Seek>(readers: &mut [R]) -> Result<(), String> {
    checked_member_contents(
        readers,
        0,
        655360,
        "a7b17ca58cd2d07524ab80dfa5cc599431dfbb6f34a4dea62756bc84f3af9734",
    )
}

fn checked_member_contents<R: Read + Seek>(
    readers: &mut [R],
    missing: usize,
    live_size: u64,
    live_sha: &str,
) -> Result<(), String> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    use recovery::ScanMode;
    fn open<R: Read + Seek>(
        readers: Vec<R>,
        missing: usize,
    ) -> Result<disk_core::btrfs::Volume<R>, String> {
        let volume = if missing == 0 {
            disk_core::btrfs::open_members(readers, || Ok(()))?
        } else {
            disk_core::btrfs::open_degraded(readers, || Ok(()))?
        };
        if volume.missing_members() != missing {
            return Err("Unexpected number of missing Btrfs members".into());
        }
        Ok(volume)
    }
    for (mode, name, size, digest) in [
        (ScanMode::Existing, "striped.bin", live_size, live_sha),
        (
            ScanMode::Deleted,
            "deleted.txt",
            55,
            "e4fa60184b258fe1c4bb39a53be74c23b2621cafdab8ca164729517effb87930",
        ),
    ] {
        let mut files = Vec::new();
        let volume = open(readers.iter_mut().collect(), missing)?;
        btrfs_recovery::scan_volume(
            volume,
            mode,
            &cancel,
            |_, _| Ok(()),
            |file| files.push(file),
            |_| Ok(()),
        )?;
        if mode == ScanMode::Deleted && files.len() != 1 {
            return Err("Expected only the file deleted from the live subvolume".into());
        }
        let file = files
            .iter()
            .find(|file| file.name == name && file.size == size && file.supported)
            .ok_or("Expected a verified Btrfs file")?;
        let recovery::Locator::Btrfs(location) = &file.locator else {
            return Err("Expected the Btrfs recovery engine".into());
        };
        let volume = open(readers.iter_mut().rev().collect(), missing)?;
        let mut output = Vec::new();
        let result = btrfs_recovery::extract_from(
            volume,
            file,
            location,
            &mut output,
            size,
            &cancel,
            |_| Ok(()),
        )?;
        if result.bytes != size || format!("{:x}", Sha256::digest(&output)) != digest {
            return Err(format!("Linux data differs for {name}"));
        }
    }
    Ok(())
}

pub(super) fn prepare_members() -> Result<String, String> {
    let sha = desktop()?;
    let report = member_report(&sha);
    let (mut readers, digests) = member_images(MEMBER_NAMES)?;
    member_contents(&mut readers)?;
    let mut log = backup_protection::begin("btrfs-4kn-members-prepare")?;
    if report.exists() {
        return Err("The Btrfs member fixture already exists".into());
    }
    let mut layouts = Vec::new();
    for ((serial, image), digest) in [A, B].into_iter().zip(&mut readers).zip(digests) {
        let layout = create_linux_fixture(serial, HISTORY_SIZE)?;
        seed_linux_fixture(
            serial,
            &layout,
            HISTORY_SIZE,
            image,
            digest,
            |_| Ok(()),
            |_| Ok(()),
        )?;
        layouts.push(layout);
    }
    let [a, b]: [Witness; 2] = layouts
        .try_into()
        .map_err(|_| "Expected two member layouts")?;
    let result = format!("PASS. Both 256 MiB members are offline. Select A/{} at {} MiB and B/{} at {} MiB in recovery or file browsing. Deleted mode finds deleted.txt. The live striped.bin has 655360 bytes.",
        a.partition, a.offset / MIB, b.partition, b.offset / MIB);
    let saved = MemberWitness {
        members: [a, b],
        tables: history_tables()?,
    };
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(&mut report, &saved).map_err(err)?;
    report.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result": "pass", "memberSha256":digests}),
    )?;
    Ok(result)
}

pub(super) fn inspect_members() -> Result<String, String> {
    let sha = desktop()?;
    let saved: MemberWitness =
        serde_json::from_reader(File::open(member_report(&sha)).map_err(err)?).map_err(err)?;
    let (_, expected) = member_images(MEMBER_NAMES)?;
    if history_tables()? != saved.tables {
        return Err("A/B GPT changed during Btrfs reads".into());
    }
    let mut readers = Vec::new();
    for ((serial, layout), digest) in [A, B].into_iter().zip(&saved.members).zip(expected) {
        let actual = test_disk(serial)?;
        if actual.is_offline != Some(true)
            || reallocation_checks::os_layout(&inventory()?)? != layout.os_layout
        {
            return Err("The test disk or guest OS layout changed".into());
        }
        let source =
            data_windows::resolve(&selection(serial, Some(layout.partition))?, false, false)?;
        if source.size != HISTORY_SIZE || source.offset != layout.offset {
            return Err("The source member extent changed".into());
        }
        let mut reader = data_windows::reader(&source)?;
        if data_io::hash_exact(&mut reader, HISTORY_SIZE, |_| Ok(()))? != digest {
            return Err("The tested member image bytes changed".into());
        }
        readers.push(reader);
    }
    member_contents(&mut readers)?;
    Ok("PASS. Both live striped data and the deleted snapshot file match Linux. A and B stayed offline and their full member digests, both GPTs and the guest OS layout match the baseline.".into())
}

pub(super) struct HistoryFixture {
    kind: &'static str,
    image: &'static str,
    sha: Option<&'static str>,
    output: &'static str,
    method: &'static str,
    files: [(&'static str, u64, &'static str); 2],
}

pub(super) const SNAPSHOT: HistoryFixture = HistoryFixture {
    kind: "snapshot",
    image: "Btrfs-Snapshot-Source.raw",
    sha: option_env!("DISKSTUDIO_BTRFS_SNAPSHOT_SOURCE_SHA256"),
    output: "DSBTRFS-Snapshot-Recovery",
    method: "Btrfs 快照候选",
    files: [
        (
            "deleted-inline.txt",
            44,
            "e4504d3f6b8ff6e639795bef9bf125f8a5e0905fe79a80bc112288103a759efb",
        ),
        ("deleted-compressed.bin", COMPRESSED_BYTES, COMPRESSED_SHA),
    ],
};
pub(super) const RETAINED: HistoryFixture = HistoryFixture {
    kind: "retained",
    image: "Btrfs-Retained-Source.raw",
    sha: option_env!("DISKSTUDIO_BTRFS_RETAINED_SOURCE_SHA256"),
    output: "DSBTRFS-Retained-Recovery",
    method: "Btrfs 保留事务候选",
    files: [
        (
            "retained-inline.txt",
            46,
            "1eb45c367550fe0d80b89b88b6c8396d80fe478632abf37bca5d67e944394921",
        ),
        ("retained-compressed.bin", COMPRESSED_BYTES, COMPRESSED_SHA),
    ],
};

#[derive(Serialize, Deserialize)]
struct HistoryWitness {
    layout: Witness,
    tables: [String; 2],
    destination: PathBuf,
}

fn history_report(kind: &str, sha: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-{kind}-4kn-{sha}.json"
    ))
}

fn history_tables() -> Result<[String; 2], String> {
    let hash = |serial| -> Result<String, String> {
        let bytes = quick_partition_checks::table(serial)?.encode()?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    };
    Ok([hash(A)?, hash(B)?])
}

fn history_destination(name: &str) -> Result<PathBuf, String> {
    let output = reallocation_checks::metadata_partition(B, Some("DS4KMOVEJRN"), 32, 256)?;
    let drive = output
        .drive_letter
        .ok_or("B recovery output volume needs a drive letter")?;
    let disk = selection(B, Some(output.number))?;
    let root = PathBuf::from(format!("{drive}:\\"));
    if data_windows::folder_location(&root)?.1 != disk.target {
        return Err("B recovery output volume identity did not match".into());
    }
    Ok(root.join(name))
}

fn history_source(fixture: &HistoryFixture, reader: &mut (impl Read + Seek)) -> Result<(), String> {
    if fixture.kind == "retained" {
        let mut volume = disk_core::btrfs::open(&mut *reader, || Ok(()))?;
        if !volume.snapshots().is_empty()
            || volume.inventory().subvolumes.len() != 2
            || volume.retained_roots(|| Ok(()))?.is_empty()
        {
            return Err(
                "Expected only the top-level and live subvolume, with a retained root".into(),
            );
        }
    }
    let mut candidates = Vec::new();
    btrfs_recovery::scan(
        &mut *reader,
        recovery::ScanMode::Deleted,
        &AtomicBool::new(false),
        |_, _| Ok(()),
        |candidate| candidates.push(candidate),
        |_| Ok(()),
    )?;
    if candidates.len() != fixture.files.len() {
        return Err(format!(
            "Expected two {} candidates, found {}",
            fixture.kind,
            candidates.len()
        ));
    }
    for (name, size, sha) in fixture.files {
        let candidate = candidates
            .iter()
            .find(|c| {
                c.name == name
                    && c.deleted
                    && c.supported
                    && c.size == size
                    && c.method == fixture.method
            })
            .ok_or_else(|| format!("Missing {} deletion candidate {name}", fixture.kind))?;
        let mut bytes = Vec::new();
        recovery::extract(reader, candidate, &mut bytes, size, |_| Ok(()))?;
        if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(&bytes)) != sha {
            return Err(format!("Unexpected source candidate bytes for {name}"));
        }
    }
    Ok(())
}

pub(super) fn prepare_history(fixture: HistoryFixture) -> Result<String, String> {
    let sha = desktop()?;
    let expected = fixture
        .sha
        .ok_or("Expected the bound recovery fixture SHA-256")?;
    let file = std::env::current_exe()
        .map_err(err)?
        .with_file_name(fixture.image);
    let mut image = File::open(&file).map_err(err)?;
    if image.metadata().map_err(err)?.len() != HISTORY_SIZE
        || allocation_batch::hash_file(&file)? != expected
    {
        return Err("Unexpected recovery fixture size or SHA-256".into());
    }
    history_source(&fixture, &mut image)?;
    let mut log = backup_protection::begin(&format!("btrfs-4kn-{}-prepare", fixture.kind))?;
    let destination = history_destination(fixture.output)?;
    let report_path = history_report(fixture.kind, &sha);
    if report_path.exists() || destination.try_exists().map_err(err)? {
        return Err("Recovery fixture or its export directory already exists".into());
    }
    let layout = create_linux_fixture(A, HISTORY_SIZE)?;
    seed_linux_fixture(
        A,
        &layout,
        HISTORY_SIZE,
        &mut image,
        expected,
        |_| Ok(()),
        |region| history_source(&fixture, region),
    )?;
    let tables = history_tables()?;
    fs::create_dir(&destination).map_err(err)?;
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report_path)
        .map_err(err)?;
    let result = format!(
        "PASS. A/{} at {} MiB is offline with the verified 256 MiB {} fixture. Scan it in Deleted files mode, select both candidates and preserve folders. Export to {} on B.",
        layout.partition, layout.offset / MIB, fixture.kind, destination.display()
    );
    serde_json::to_writer(
        &mut report,
        &HistoryWitness {
            layout,
            tables,
            destination,
        },
    )
    .map_err(err)?;
    report.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass", "sourceSha256":expected}),
    )?;
    Ok(result)
}

pub(super) fn inspect_history(fixture: HistoryFixture) -> Result<String, String> {
    let sha = desktop()?;
    let saved: HistoryWitness =
        serde_json::from_reader(File::open(history_report(fixture.kind, &sha)).map_err(err)?)
            .map_err(err)?;
    let layout = saved.layout;
    if witness(layout.partition, layout.offset)? != layout
        || test_a()?.is_offline != Some(true)
        || history_tables()? != saved.tables
        || history_destination(fixture.output)? != saved.destination
    {
        return Err("The source, destination or guest disk layouts changed".into());
    }
    let source = data_windows::resolve(&selection(A, Some(layout.partition))?, false, false)?;
    if source.size != HISTORY_SIZE || source.offset != layout.offset {
        return Err("The source fixture boundary changed".into());
    }
    let current =
        data_io::hash_exact(
            &mut data_windows::reader(&source)?,
            HISTORY_SIZE,
            |_| Ok(()),
        )?;
    if Some(current.as_str()) != fixture.sha {
        return Err("The source fixture bytes changed".into());
    }
    verify_recovered(&saved.destination, &fixture.files)?;
    Ok(format!("PASS. Both {} files and the nested directory were recovered to B. Complete sizes and SHA-256 match the Linux fixture. A is offline, its source image and both GPTs match, and the OS layout is unchanged.", fixture.kind))
}

fn verify_recovered(destination: &Path, files: &[(&str, u64, &str)]) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let dirs = fs::read_dir(destination)
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [directory] = dirs.as_slice() else {
        return Err("Expected one recovery result directory on B".into());
    };
    let path = directory.path();
    if !path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("DiskStudio-recovered-"))
    {
        return Err("Unexpected recovery result directory name".into());
    }
    let mut pending = vec![path.clone()];
    let mut outputs = std::collections::BTreeMap::new();
    let mut entries = 0;
    while let Some(folder) = pending.pop() {
        let metadata = fs::symlink_metadata(&folder).map_err(err)?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("Recovery result directory is a link or unavailable".into());
        }
        for entry in fs::read_dir(folder).map_err(err)? {
            let entry = entry.map_err(err)?;
            entries += 1;
            if entries > 16 {
                return Err("Unexpected number of recovery entries".into());
            }
            let metadata = entry.metadata().map_err(err)?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err("Unexpected reparse point in the recovery output".into());
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if !metadata.is_file()
                || outputs.insert(entry.file_name(), entry.path()).is_some()
            {
                return Err("Unexpected duplicate or special recovery output".into());
            }
        }
    }
    let manifest = outputs
        .remove(std::ffi::OsStr::new(recovery_export::REPORT))
        .ok_or("Missing completed recovery manifest")?;
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(manifest).map_err(err)?).map_err(err)?;
    let outcomes = manifest["outcomes"]
        .as_array()
        .ok_or("Missing recovery outcomes")?;
    if outputs.len() != files.len()
        || outcomes.len() != files.len()
        || manifest["completed"] != true
        || manifest["savedFiles"] != files.len()
        || manifest["layout"] != "tree"
    {
        return Err("Recovery manifest does not prove all expected file exports".into());
    }
    for &(name, size, expected) in files {
        let file = outputs
            .get(std::ffi::OsStr::new(name))
            .ok_or_else(|| format!("Missing recovered {name}"))?;
        if fs::metadata(file).map_err(err)?.len() != size
            || allocation_batch::hash_file(file)? != expected
            || !outcomes
                .iter()
                .any(|o| o["saved"] == true && o["sha256"] == expected)
            || ((name == "striped.bin" || name.ends_with("-compressed.bin"))
                && file.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("nested")))
        {
            return Err(format!(
                "Recovered file bytes, path or readback report differ for {name}"
            ));
        }
    }
    Ok(())
}

const PARITY_NAMES: [&str; 3] = [
    "Btrfs-Raid5-1.raw",
    "Btrfs-Raid6-1.raw",
    "Btrfs-Raid6-2.raw",
];
const PARITY_LIVE: (&str, u64, &str) = (
    "striped.bin",
    22 * 64 * 1024,
    "09d94d912e58567987c05ff9c225947619edddd261f2a5cf856a296a0b40a1d0",
);
const PARITY_DESTINATION: &str = "DSBTRFS-Raid6-Recovery";

#[derive(Serialize, Deserialize)]
struct ParityWitness {
    members: [(u32, u64); 3],
    os_layout: String,
    tables: [String; 2],
    destination: PathBuf,
}

fn parity_digests() -> Result<[&'static str; 3], String> {
    option_env!("DISKSTUDIO_BTRFS_PARITY_SHA256")
        .ok_or("Missing parity media digests")?
        .split(',')
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "Expected three bound parity media digests".into())
}

fn parity_contents<R: Read + Seek>(readers: &mut [R]) -> Result<(), String> {
    let (raid5, raid6) = readers.split_at_mut(1);
    for (members, missing) in [(raid5, 1), (raid6, 2)] {
        if disk_core::btrfs::open_members(members.iter_mut().collect(), || Ok(())).is_ok() {
            return Err("Accepted a missing Btrfs member without explicit degraded reading".into());
        }
        checked_member_contents(members, missing, PARITY_LIVE.1, PARITY_LIVE.2)?;
    }
    Ok(())
}

pub(super) fn prepare_parity() -> Result<String, String> {
    let sha = desktop()?;
    let report = history_report("parity", &sha);
    if report.exists() {
        return Err("The Btrfs parity fixture already exists".into());
    }
    let expected = parity_digests()?;
    let executable = std::env::current_exe().map_err(err)?;
    let mut readers = Vec::new();
    for (name, digest) in PARITY_NAMES.into_iter().zip(expected) {
        let path = executable.with_file_name(name);
        let file = File::open(&path).map_err(err)?;
        if file.metadata().map_err(err)?.len() != HISTORY_SIZE
            || allocation_batch::hash_file(&path)? != digest
        {
            return Err(format!("Unexpected parity media file {name}"));
        }
        readers.push(file);
    }
    parity_contents(&mut readers)?;
    let mut log = backup_protection::begin("btrfs-4kn-parity-prepare")?;
    online(B)?;
    let destination = history_destination(PARITY_DESTINATION)?;
    if destination.try_exists().map_err(err)? {
        return Err("The parity export directory already exists".into());
    }
    let os_layout = reallocation_checks::os_layout(&inventory()?)?;
    let mut members = Vec::new();
    for (image, digest) in readers.iter_mut().zip(expected) {
        let layout = create_linux_fixture(A, HISTORY_SIZE)?;
        seed_linux_fixture(
            A,
            &layout,
            HISTORY_SIZE,
            image,
            digest,
            |_| Ok(()),
            |_| Ok(()),
        )?;
        members.push((layout.partition, layout.offset));
    }
    let members: [(u32, u64); 3] = members.try_into().map_err(|_| "Missing prepared member")?;
    let result = format!(
        "PASS. RAID5 A/{} at {} MiB. RAID6 A/{} at {} MiB and A/{} at {} MiB. A is offline. Enable degraded read. Export only live/nested/striped.bin from RAID6 to {} with the tree layout.",
        members[0].0, members[0].1 / MIB, members[1].0, members[1].1 / MIB,
        members[2].0, members[2].1 / MIB, destination.display(),
    );
    let saved = ParityWitness {
        members,
        os_layout,
        tables: history_tables()?,
        destination,
    };
    fs::create_dir(&saved.destination).map_err(err)?;
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(&mut report, &saved).map_err(err)?;
    report.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass","memberSha256":expected}),
    )?;
    Ok(result)
}

pub(super) fn inspect_parity() -> Result<String, String> {
    let sha = desktop()?;
    let saved: ParityWitness =
        serde_json::from_reader(File::open(history_report("parity", &sha)).map_err(err)?)
            .map_err(err)?;
    if test_a()?.is_offline != Some(true)
        || history_tables()? != saved.tables
        || reallocation_checks::os_layout(&inventory()?)? != saved.os_layout
        || history_destination(PARITY_DESTINATION)? != saved.destination
    {
        return Err("The test disks or guest OS layout changed".into());
    }
    let mut readers = Vec::new();
    for ((partition, offset), digest) in saved.members.into_iter().zip(parity_digests()?) {
        let source = data_windows::resolve(&selection(A, Some(partition))?, false, false)?;
        if source.size != HISTORY_SIZE || source.offset != offset {
            return Err("A parity fixture extent changed".into());
        }
        let mut reader = data_windows::reader(&source)?;
        if data_io::hash_exact(&mut reader, HISTORY_SIZE, |_| Ok(()))? != digest {
            return Err("A parity fixture image digest changed".into());
        }
        readers.push(reader);
    }
    parity_contents(&mut readers)?;
    verify_recovered(&saved.destination, &[PARITY_LIVE])?;
    Ok("PASS. Independently recovered the Linux files with one missing RAID5 and two missing RAID6 members. The Windows UI export on B has the expected path and SHA-256. All three source images and both GPTs matched, A stayed offline, and the guest OS layout matched.".into())
}

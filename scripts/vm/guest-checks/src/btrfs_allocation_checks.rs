//! Guest-only acceptance of the Btrfs used-space backup and clone UI.
use super::*;
use crate::backup_archive::{Archive, Chain};

const IMAGE: &str = "Btrfs-Allocation-Source.raw";
const OUTPUT: &str = "DSBTRFS-Allocation";
const BACKUP: &str = "btrfs-used.dsbackup";
const BLOCK_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct CopyWitness {
    source: Witness,
    target: Witness,
    tables: [String; 2],
    backup: PathBuf,
    map_digest: String,
    skipped: u64,
    initial_target_free: String,
}

fn input_digest() -> Result<&'static str, String> {
    option_env!("DISKSTUDIO_BTRFS_ALLOCATION_SHA256")
        .ok_or_else(|| "Missing bound allocation fixture SHA-256".into())
}

fn read_report(sha: &str) -> Result<CopyWitness, String> {
    serde_json::from_reader(File::open(history_report("allocation", sha)).map_err(err)?)
        .map_err(err)
}

fn archive_receipt(sha: &str) -> PathBuf {
    history_report("allocation-archive", sha)
}

fn fixture_files(reader: &mut (impl Read + Seek)) -> Result<(), String> {
    let mut volume = disk_core::btrfs::open(reader, || Ok(()))?;
    let root = |volume: &disk_core::btrfs::Volume<_>, id| {
        volume
            .inventory()
            .subvolumes
            .iter()
            .find(|subvolume| subvolume.id == id)
            .map(|subvolume| subvolume.root_directory)
            .ok_or("Btrfs subvolume missing")
    };
    let lookup = |volume: &mut disk_core::btrfs::Volume<_>, tree, parent, name: &str| {
        volume
            .read_directory(tree, parent, || Ok(()))?
            .into_iter()
            .find(|entry| entry.name == name.as_bytes())
            .map(|entry| entry.target.object_id)
            .ok_or_else(|| format!("Expected Btrfs fixture entry {name}"))
    };
    let top = root(&volume, 5)?;
    let live = lookup(&mut volume, 5, top, "live")?;
    let snapshot = lookup(&mut volume, 5, top, "snapshot")?;
    let live_root = root(&volume, live)?;
    let snapshot_root = root(&volume, snapshot)?;
    let nested = lookup(&mut volume, live, live_root, "nested")?;
    let snapshot_nested = lookup(&mut volume, snapshot, snapshot_root, "nested")?;
    let data_sha = "cd4e6c9279aa4f0792e275b5740ce2e37f31d94066dbd495d7449135d9f8bc59";
    for (tree, parent, name, size, sha) in [
        (live, nested, "contents.bin", 481701, data_sha),
        (live, nested, "reflink.bin", 481701, data_sha),
        (
            live,
            live_root,
            "sparse.bin",
            266240,
            "a720dd077e66b6919b1c86f3ccfb1ef28ade0072ce98cc65975c2f13be74594f",
        ),
        (
            live,
            live_root,
            "preallocated.bin",
            2097152,
            "5647f05ec18958947d32874eeb788fa396a05d0bab7c1b71f112ceb7e9b31eee",
        ),
        (
            snapshot,
            snapshot_nested,
            "deleted.bin",
            481701,
            "9b07d1d36744f0a3074a11f04e7fccaa6c2e350b870e4594d7d19ffd57f32fb6",
        ),
    ] {
        let inode = lookup(&mut volume, tree, parent, name)?;
        let mut bytes = Vec::new();
        volume.read_file(tree, inode, size, &mut bytes, || Ok(()))?;
        if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(&bytes)) != sha {
            return Err(format!("Btrfs fixture bytes differ for {name}"));
        }
    }
    if volume.snapshots().len() != 1 || lookup(&mut volume, live, nested, "deleted.bin").is_ok() {
        return Err("The deleted fixture file must only exist in its snapshot".into());
    }
    volume.verify_view(|| Ok(()))
}

fn plan(reader: &mut impl backup_archive::Source) -> Result<used_space::Plan, String> {
    let cancel = AtomicBool::new(false);
    let plan = used_space::inspect(reader, HISTORY_SIZE, 4096, None, &cancel)?;
    if plan.volumes.len() != 1 || plan.volumes[0].filesystem != "Btrfs" || plan.skipped < 128 * MIB
    {
        return Err(
            "Expected independently verified Btrfs allocation and substantial free space".into(),
        );
    }
    plan.proof.verify(reader, &cancel)?;
    Ok(plan)
}

fn reader(serial: &str, witness: &Witness) -> Result<data_io::Region<File>, String> {
    test_disk(serial)?;
    let source = data_windows::resolve(&selection(serial, Some(witness.partition))?, false, false)?;
    if source.offset != witness.offset
        || source.size != HISTORY_SIZE
        || source.disk.logical_sector_size != Some(4096)
    {
        return Err("Fixture boundary changed".into());
    }
    data_windows::reader(&source)
}

fn free_digest(view: &mut (impl Read + Seek), plan: &used_space::Plan) -> Result<String, String> {
    let mut hasher = Sha256::new();
    let mut bytes = vec![0; BLOCK_BYTES];
    for range in plan.free_ranges() {
        view.seek(SeekFrom::Start(range.start)).map_err(err)?;
        let mut remaining = range.size();
        while remaining != 0 {
            let take = remaining.min(BLOCK_BYTES as u64) as usize;
            view.read_exact(&mut bytes[..take]).map_err(err)?;
            hasher.update(&bytes[..take]);
            remaining -= take as u64;
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn target_partition() -> Result<Witness, String> {
    let base = witness_on(B, 0, 0)?;
    let status = partitions::platform::status()?;
    let target = status
        .targets
        .iter()
        .find(|target| target.serial == B)
        .ok_or("The isolated B disk is unavailable")?;
    if let Some(reason) = &target.reason {
        return Err(format!("The B fixture target is protected: {reason}"));
    }
    let offset = target
        .free_ranges
        .iter()
        .find(|range| range.size >= HISTORY_SIZE)
        .ok_or("B needs one fresh 256 MiB range")?
        .offset;
    operation(
        B,
        partitions::Action::Create {
            offset,
            size: HISTORY_SIZE,
            kind: partitions::PartitionKind::Windows,
        },
    )?;
    let partition = reallocation_checks::metadata_partition(B, None, offset / MIB, 256)?;
    let fresh = witness_on(B, partition.number, offset)?;
    if fresh.other_layout != base.other_layout || fresh.os_layout != base.os_layout {
        return Err("Unexpected layout after creating the B clone target".into());
    }
    Ok(fresh)
}

pub(crate) fn prepare() -> Result<String, String> {
    let sha = desktop()?;
    let report = history_report("allocation", &sha);
    if report.exists() {
        return Err("The Btrfs allocation fixture already exists".into());
    }
    let image_path = std::env::current_exe().map_err(err)?.with_file_name(IMAGE);
    let expected = input_digest()?;
    let mut image = File::open(&image_path).map_err(err)?;
    if image.metadata().map_err(err)?.len() != HISTORY_SIZE
        || allocation_batch::hash_file(&image_path)? != expected
    {
        return Err("Unexpected Btrfs allocation fixture image".into());
    }
    let parsed = plan(&mut image)?;
    fixture_files(&mut image)?;
    let mut log = backup_protection::begin("btrfs-4kn-allocation-prepare")?;
    online(B)?;
    let directory = history_destination(OUTPUT)?;
    if directory.try_exists().map_err(err)? {
        return Err("The Btrfs allocation backup directory already exists".into());
    }
    let source = create_linux_fixture(A, HISTORY_SIZE)?;
    seed_linux_fixture(
        A,
        &source,
        HISTORY_SIZE,
        &mut image,
        expected,
        |_| Ok(()),
        |_| Ok(()),
    )?;
    let target = target_partition()?;
    let saved = CopyWitness {
        initial_target_free: free_digest(&mut reader(B, &target)?, &parsed)?,
        source,
        target,
        tables: history_tables()?,
        backup: directory.join(BACKUP),
        map_digest: parsed.digest,
        skipped: parsed.skipped,
    };
    fs::create_dir(&directory).map_err(err)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(&mut output, &saved).map_err(err)?;
    output.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({
            "result": "pass", "sourceSha256": expected, "skippedBytes": saved.skipped
        }),
    )?;
    Ok(format!(
        "PASS. Source A/{} at {} MiB is offline. B/{} at {} MiB is the empty clone target. Create a full used-space backup of A/{} at {} and run the Archive checker first. Then clone A/{} to B/{} in used-space mode.",
        saved.source.partition, saved.source.offset / MIB, saved.target.partition,
        saved.target.offset / MIB, saved.source.partition, saved.backup.display(),
        saved.source.partition, saved.target.partition,
    ))
}

fn verify_layout(saved: &CopyWitness) -> Result<(), String> {
    if test_a()?.is_offline != Some(true)
        || history_tables()? != saved.tables
        || reallocation_checks::os_layout(&inventory()?)? != saved.source.os_layout
    {
        return Err("A/B layout, guest OS layout, or offline A changed".into());
    }
    Ok(())
}

fn verify_map(saved: &CopyWitness, plan: &used_space::Plan) -> Result<(), String> {
    if plan.digest != saved.map_digest || plan.skipped != saved.skipped {
        return Err("Btrfs allocation map differs from the checked Linux fixture".into());
    }
    Ok(())
}

pub(crate) fn inspect_archive() -> Result<String, String> {
    let sha = desktop()?;
    let saved = read_report(&sha)?;
    verify_layout(&saved)?;
    if history_destination(OUTPUT)?.join(BACKUP) != saved.backup {
        return Err("The backup is outside its verified folder on B".into());
    }
    let file = File::open(&saved.backup).map_err(err)?;
    let mut archive = Chain::new(Archive::open(file)?);
    let header = archive.header();
    if header.kind != "partition"
        || header.size != HISTORY_SIZE
        || header.sector != 4096
        || !header.allocation.as_ref().is_some_and(|policy| {
            policy.skipped_bytes == saved.skipped && policy.map_sha256 == saved.map_digest
        })
    {
        return Err("Backup lacks the matching Btrfs used-space policy".into());
    }
    let expected = header.sha256.clone();
    archive.verify(|_| Ok(()))?;
    let mut source = reader(A, &saved.source)?;
    let plan = plan(&mut source)?;
    verify_map(&saved, &plan)?;
    if data_io::hash_exact(&mut plan.masked(&mut source), HISTORY_SIZE, |_| Ok(()))? != expected {
        return Err("Backup logical SHA-256 differs from the independently masked source".into());
    }
    fixture_files(&mut archive.reader())?;
    let mut receipt = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(archive_receipt(&sha))
        .map_err(err)?;
    receipt.write_all(expected.as_bytes()).map_err(err)?;
    receipt.sync_all().map_err(err)?;
    Ok(format!("PASS. Archive on B verified all logical bytes and five Linux files. Btrfs allocation omitted {} bytes. SHA-256 {expected}. Ready for the native clone.", saved.skipped))
}

pub(crate) fn inspect_clone() -> Result<String, String> {
    let sha = desktop()?;
    let receipt = fs::read_to_string(archive_receipt(&sha)).map_err(err)?;
    if receipt.len() != 64 || !receipt.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("The prior backup verification receipt is missing".into());
    }
    let saved = read_report(&sha)?;
    verify_layout(&saved)?;
    if test_disk(B)?.is_offline != Some(true) {
        return Err("The clone destination must remain offline".into());
    }
    let mut source = reader(A, &saved.source)?;
    let mut target = reader(B, &saved.target)?;
    if data_io::hash_exact(&mut source, HISTORY_SIZE, |_| Ok(()))? != input_digest()? {
        return Err("Source image SHA-256 changed".into());
    }
    let source_plan = plan(&mut source)?;
    let target_plan = plan(&mut target)?;
    verify_map(&saved, &source_plan)?;
    verify_map(&saved, &target_plan)?;
    let cancel = AtomicBool::new(false);
    let digest = source_plan.retained_digest(&mut source, &cancel, |_, _| {})?;
    if target_plan.retained_digest(&mut target, &cancel, |_, _| {})? != digest
        || free_digest(&mut target, &target_plan)? != saved.initial_target_free
    {
        return Err("Cloned retained bytes or original target free bytes differ".into());
    }
    fixture_files(&mut target)?;
    Ok(format!(
        "PASS. Cloned known Btrfs bytes match, {} skipped bytes on B stayed intact, five Linux files match, both GPTs and guest OS layout match, and A and B are offline.",
        saved.skipped,
    ))
}

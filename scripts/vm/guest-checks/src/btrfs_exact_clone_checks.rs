//! Reuse the independently accepted 4Kn Btrfs restore disks for exact cloning.
use super::*;
use crate::capacity_restore_checks::{hash, region};
use crate::partitions::platform::maintenance;
use crate::partitions::{policy, Action, PartitionKind};
use crate::surface_scan::Range;

const PRIOR_DESKTOP: &str = "cfa598fd4c1dad9450d92ae02c4ea426d01f01f77016fcda73600ec6bb9265d1";
const SOURCE_DIGESTS: [&str; 3] = [
    "d5665a81b673cbe87645815f3c7fb02ba6f135d2229e381567116b5c15738732",
    "71f0129c6b9e04bc1092b3f8922522df3e6cdeffc4c165a49db9f2c867149324",
    "8331a68fdf84d50a6c59726f03bec7f9fb88352e9ca0fcaa5977a93c632b87c0",
];
const TARGETS: [u64; 2] = [2304 * MIB, 2816 * MIB];
const SECTOR: u64 = 4096;
const MARKER: [u8; SECTOR as usize] = [0xa7; SECTOR as usize];

#[derive(Serialize, Deserialize)]
struct CloneBaseline {
    tables: [String; 3],
    target_edges: [[String; 2]; 2],
    marker_at: u64,
    used_digest: String,
    used_full_digest: String,
    copied: u64,
}

fn witness(desktop: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-exact-clone-{desktop}.json"
    ))
}

fn original_source(file: &File) -> Result<Baseline, String> {
    let prior: Baseline =
        serde_json::from_reader(File::open(receipt(PRIOR_DESKTOP)).map_err(err)?.take(MIB))
            .map_err(err)?;
    for ((offset, size), expected) in OFFSETS.into_iter().zip(SIZES).zip(SOURCE_DIGESTS) {
        if hash(file, offset, size)? != expected {
            return Err("The accepted Btrfs restore bytes do not match the original run".into());
        }
    }
    Ok(prior)
}

fn target_edges(file: &File) -> Result<[[String; 2]; 2], String> {
    let adjacent = |at| -> Result<_, String> {
        Ok([
            hash(file, at - SECTOR, SECTOR)?,
            hash(file, at + MOVE_SIZE, SECTOR)?,
        ])
    };
    Ok([adjacent(TARGETS[0])?, adjacent(TARGETS[1])?])
}

fn target_slots() -> Result<[u32; 2], String> {
    let layout = fat_resize_checks::model(A)?;
    if layout.entries.len() != 4 {
        return Err("Expected the original A partitions plus two new Linux targets".into());
    }
    let locate = |offset| -> Result<u32, String> {
        if !layout.entries.iter().any(|entry| {
            entry.offset == offset
                && entry.size == MOVE_SIZE
                && entry.kind == policy::LINUX_DATA
                && entry.attributes == 0
        }) {
            return Err("The dedicated A clone target is absent or protected".into());
        }
        Ok(reallocation_checks::metadata_partition(A, None, offset / MIB, MOVE_SIZE / MIB)?.number)
    };
    Ok([locate(TARGETS[0])?, locate(TARGETS[1])?])
}

fn require_empty(file: &File, offset: u64) -> Result<(), String> {
    let mut area = region(file, offset, MOVE_SIZE)?;
    let mut buffer = vec![0; MIB as usize];
    for _ in 0..MOVE_SIZE / MIB {
        area.read_exact(&mut buffer).map_err(err)?;
        if buffer.iter().any(|&byte| byte != 0) {
            return Err("A clone target is not empty".into());
        }
    }
    Ok(())
}

fn seed_marker(partition: u32, marker_at: u64) -> Result<(), String> {
    if marker_at % SECTOR != 0 || marker_at + SECTOR > MOVE_SIZE {
        return Err("Marker is outside the bound extent".into());
    }
    let selected = selection(A, None)?;
    let range = Range {
        offset: TARGETS[0],
        size: MOVE_SIZE,
        sector: SECTOR,
    };
    let verify_context = |ctx: &partitions::platform::TableReadContext| -> Result<(), String> {
        let resolved = &ctx.resolved;
        let parts = ctx.parts.iter().collect::<Vec<_>>();
        if resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(A)
            || resolved.disk.logical_sector_size != Some(SECTOR)
            || resolved.size != 16 * GIB
            || !parts.iter().any(|part| {
                part.partition_number == partition
                    && part.offset == range.offset
                    && part.size == range.size
                    && policy::linux_clone_candidate(&resolved.disk, &parts, part)
            })
            || ctx.has_volume(partition)
        {
            return Err("Marker target identity, layout or protection differs".into());
        }
        Ok(())
    };
    let before = maintenance::write_context(&selected, None)?;
    verify_context(&before)?;
    let locks = data_windows::lock(&before.resolved)?;
    let current = maintenance::write_context(&selected, Some((&before, &locks)))?;
    verify_context(&current)?;
    let mut file = data_windows::exclusive_device(&current.resolved)?;
    let inspect = |file: &mut File| -> Result<_, String> {
        let table =
            maintenance::capture_write_table(&current, file, &selected.target, range, true)?
                .ok_or("Missing bound A GPT")?;
        let (native, protected, _) = extfs::restore_native(
            file,
            Some(&current.parts.iter().collect::<Vec<_>>()),
            SECTOR,
        )?;
        if !table_restore::archive_layout(&table)?.agrees(&native)
            || !native.entries.iter().any(|entry| {
                entry.slot == partition
                    && entry.kind == policy::LINUX_DATA
                    && entry.offset == range.offset
                    && entry.size == range.size
                    && entry.attributes == 0
                    && !protected.contains(&entry.guid)
            })
        {
            return Err("Marker target GPT or native layout differs".into());
        }
        Ok(table)
    };
    let table = inspect(&mut file)?;
    data_windows::persist_offline(&file)?;
    data_windows::confirm_writable_offline(&file)?;
    if inspect(&mut file)? != table {
        return Err("A GPT changed before the marker write".into());
    }
    for offset in TARGETS {
        require_empty(&file, offset)?;
    }
    let mut output = data_io::Region::new(file.try_clone().map_err(err)?, range.offset, range.size)
        .map_err(err)?;
    output.seek(SeekFrom::Start(marker_at)).map_err(err)?;
    output.write_all(&MARKER).map_err(err)?;
    file.sync_all().map_err(err)?;
    if hash(&file, range.offset + marker_at, SECTOR)? != format!("{:x}", Sha256::digest(MARKER))
        || inspect(&mut file)? != table
    {
        return Err("A marker or its adjacent layout failed readback".into());
    }
    data_windows::confirm_writable_offline(&file)
}

fn marker_digest(input: &mut (impl Read + Seek), marker_at: u64) -> Result<String, String> {
    if marker_at % SECTOR != 0 || marker_at + SECTOR > MOVE_SIZE {
        return Err("Out-of-range marker".into());
    }
    input.rewind().map_err(err)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; MIB as usize];
    for (index, start) in (0..MOVE_SIZE).step_by(MIB as usize).enumerate() {
        input.read_exact(&mut buffer).map_err(err)?;
        if marker_at / MIB == index as u64 {
            let at = (marker_at - start) as usize;
            if buffer[at..at + MARKER.len()].iter().any(|&byte| byte != 0) {
                return Err("The selected source free sector must be zero".into());
            }
            buffer[at..at + MARKER.len()].copy_from_slice(&MARKER);
        }
        digest.update(&buffer);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn prepare() -> Result<String, String> {
    let desktop = desktop()?;
    if witness(&desktop).try_exists().map_err(err)? {
        return Err("The exact clone fixture already has a baseline".into());
    }
    verify_receipt(PRIOR_DESKTOP)?;
    let source = disk_b(true)?;
    let prior = original_source(&source)?;
    let mut reader = region(&source, OFFSETS[0], MOVE_SIZE)?;
    let cancel = AtomicBool::new(false);
    let plan = used_space::inspect(&mut reader, MOVE_SIZE, SECTOR, None, &cancel)?;
    plan.proof.verify(&mut reader, &cancel)?;
    if plan.volumes[0].filesystem != "Btrfs" || plan.skipped < SECTOR {
        return Err("Expected a verified Btrfs free space plan".into());
    }
    let free = plan.free_ranges()[0];
    let marker_at = free.start;
    let used_full_digest = marker_digest(&mut reader, marker_at)?;
    let used_digest = plan.retained_digest(&mut reader, &cancel, |_, _| ())?;
    let before = fat_resize_checks::model(A)?;
    backup_protection::begin("btrfs-exact-clone-4kn")?;
    for offset in TARGETS {
        operation(
            A,
            Action::Create {
                offset,
                size: MOVE_SIZE,
                kind: PartitionKind::Linux,
            },
        )?;
    }
    let slots = target_slots()?;
    let mut after = fat_resize_checks::model(A)?;
    after.entries.retain(|entry| !slots.contains(&entry.slot));
    if !after.agrees(&before) {
        return Err("Existing A GPT entries changed".into());
    }
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    if tables[1..] != prior.tables[1..] {
        return Err("B GPT or guest OS layout changed".into());
    }
    seed_marker(slots[0], marker_at)?;
    let a = data_windows::resolve(&selection(A, None)?, false, false)?;
    let file = data_windows::device(&a, false)?;
    data_windows::verify_offline_handle(&a, &file)?;
    let baseline = CloneBaseline {
        tables,
        target_edges: target_edges(&file)?,
        marker_at,
        used_digest,
        used_full_digest,
        copied: plan.copied(),
    };
    let mut out = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(witness(&desktop))
        .map_err(err)?;
    serde_json::to_writer(&mut out, &baseline).map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok(format!("PASS: Both disks offline. Clone B/2 to A/{} by used space and A/{} by all sectors, each 384 MiB. A/{} has a single marker in a confirmed free sector at relative byte {marker_at}.", slots[0], slots[1], slots[0]))
}

pub(crate) fn verify() -> Result<String, String> {
    let desktop = desktop()?;
    let baseline: CloneBaseline =
        serde_json::from_reader(File::open(witness(&desktop)).map_err(err)?.take(MIB))
            .map_err(err)?;
    let source = disk_b(true)?;
    let prior = original_source(&source)?;
    let a = data_windows::resolve(&selection(A, None)?, false, false)?;
    let target = data_windows::device(&a, false)?;
    data_windows::verify_offline_handle(&a, &target)?;
    target_slots()?;
    if baseline.tables != ntfs_reconstruction_checks::table_hashes()?
        || baseline.target_edges != target_edges(&target)?
    {
        return Err("Two GPTs, guest OS layout or neighboring sectors changed".into());
    }
    for (offset, expected) in TARGETS
        .into_iter()
        .zip([baseline.used_full_digest.as_str(), SOURCE_DIGESTS[0]])
    {
        let mut reader = region(&target, offset, MOVE_SIZE)?;
        if hash(&target, offset, MOVE_SIZE)? != expected
            || marker(&mut reader, 65536 + 32)? != prior.identity
            || move_contents(&mut reader)? != prior.contents
        {
            return Err(
                "Clone UUID, file contents or whole extent differs from the baseline".into(),
            );
        }
    }
    let used_marker = hash(&target, TARGETS[0] + baseline.marker_at, SECTOR)?;
    if used_marker != format!("{:x}", Sha256::digest(MARKER))
        || hash(&target, TARGETS[1] + baseline.marker_at, SECTOR)?
            != format!("{:x}", Sha256::digest([0; SECTOR as usize]))
    {
        return Err("Free-sector marker behavior differed from the clone modes".into());
    }
    let mut reader = region(&target, TARGETS[0], MOVE_SIZE)?;
    let cancel = AtomicBool::new(false);
    let plan = used_space::inspect(&mut reader, MOVE_SIZE, SECTOR, None, &cancel)?;
    if plan.copied() != baseline.copied
        || plan.retained_digest(&mut reader, &cancel, |_, _| ())? != baseline.used_digest
    {
        return Err("Used-space allocation or copied bytes differ".into());
    }
    Ok(format!("PASS: Both modes verified 51 files and original UUID. Used-space preserved the marker. All-sector copied source zeros. GPTs, OS layout, source and neighboring sectors match. Both disks stay offline.\nUsed space {} bytes\n{}\nAll sectors\n{}", baseline.copied, baseline.used_digest, SOURCE_DIGESTS[0]))
}

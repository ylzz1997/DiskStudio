//! Fixture preparation and independent readback for Btrfs capacity cloning.
use super::*;

const IMAGE: &str = "Btrfs-Clone-Source.raw";
const GROW_TARGET: u64 = 448 * MIB;

#[derive(Serialize, Deserialize)]
struct CloneWitness {
    a: table_archive::Archive,
    b: table_archive::Archive,
    os: String,
    source: Witness,
    shrink: Witness,
    grow: Witness,
    contents: String,
    source_sha256: String,
}

fn report(sha: &str, suffix: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-clone-{sha}-{suffix}.json"
    ))
}

pub(in super::super) fn prepare() -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    for serial in [A, B] {
        crate::whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let os = reallocation_checks::os_layout(&disks)?;
    let expected = option_env!("DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256")
        .ok_or("Missing bound relocation source digest")?;
    let path = std::env::current_exe().map_err(err)?.with_file_name(IMAGE);
    let mut image = File::open(&path).map_err(err)?;
    if image.metadata().map_err(err)?.len() != MOVE_SIZE
        || allocation_batch::hash_file(&path)? != expected
        || disk_core::btrfs::plan_resize(&mut image, MOVE_TARGET)?.relocation_bytes() != MOVE_BYTES
    {
        return Err("Unexpected relocation fixture image".into());
    }
    let contents = move_contents(&mut image)?;
    let _log = backup_protection::begin("btrfs-capacity-clone-4kn")?;
    for serial in [A, B] {
        online(serial)?;
        operation(serial, partitions::Action::Initialize {})?;
    }
    let shrink = create_linux_fixture(B, MOVE_TARGET)?;
    let grow = create_linux_fixture(B, GROW_TARGET)?;
    let source = create_linux_fixture(A, MOVE_SIZE)?;
    seed_linux_fixture(
        A,
        &source,
        MOVE_SIZE,
        &mut image,
        expected,
        |_| Ok(()),
        |reader| {
            if move_contents(reader)? != contents {
                return Err("Seeded files differ from the independent fixture".into());
            }
            Ok(())
        },
    )?;
    if reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("Guest OS layout changed during fixture preparation".into());
    }
    let saved = CloneWitness {
        a: quick_partition_checks::table(A)?,
        b: quick_partition_checks::table(B)?,
        os,
        source,
        shrink,
        grow,
        contents,
        source_sha256: expected.into(),
    };
    save_baseline(&desktop, &saved)
}

fn save_baseline(desktop: &str, saved: &CloneWitness) -> Result<String, String> {
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report(desktop, "baseline"))
        .map_err(err)?;
    serde_json::to_writer(&mut log, saved).map_err(err)?;
    log.sync_all().map_err(err)?;
    Ok(format!("PASS: A/{} 384 MiB remains offline. Clone to B/{} 192 MiB first (80 MiB relocation), then B/{} 448 MiB while B stays offline.",
        saved.source.partition, saved.shrink.partition, saved.grow.partition))
}

fn prepared_layout(
    serial: &str,
    archive: &table_archive::Archive,
    sizes: &[u64],
) -> Result<Vec<Witness>, String> {
    let mut layout = table_restore::archive_layout(archive)?;
    layout
        .entries
        .retain(|entry| entry.kind != "e3c9e316-0b5c-4db8-817d-f92df00215ae");
    if layout.entries.len() != sizes.len() {
        return Err("Unexpected prepared Linux partition count".into());
    }
    sizes
        .iter()
        .map(|size| {
            let found = layout
                .entries
                .iter()
                .filter(|entry| {
                    entry.kind == partitions::policy::LINUX_DATA
                        && entry.size == *size
                        && entry.attributes == 0
                        && entry.offset % MIB == 0
                })
                .collect::<Vec<_>>();
            let [entry] = found.as_slice() else {
                return Err("Prepared partition is missing or ambiguous".into());
            };
            let part = reallocation_checks::metadata_partition(
                serial,
                None,
                entry.offset / MIB,
                entry.size / MIB,
            )?;
            witness_on(serial, part.number, entry.offset)
        })
        .collect()
}

pub(in super::super) fn diagnose() -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let saved: CloneWitness = serde_json::from_reader(
        File::open(report(&desktop, "baseline"))
            .map_err(err)?
            .take(1024 * 1024),
    )
    .map_err(err)?;
    let mut lines = Vec::new();
    for (role, serial, witness) in [("A source", A, saved.source), ("B target", B, saved.shrink)] {
        let selected = selection(serial, Some(witness.partition))?;
        let bound = data_windows::resolve(&selected, true, false)?;
        let whole_disk = selection(serial, None)?;
        let context = partitions::platform::table_inspect_context(&whole_disk)?;
        let parts = context.parts.iter().collect::<Vec<_>>();
        bound.verify_partition_snapshot(&context.resolved.disk, &parts)?;
        let part = parts
            .iter()
            .find(|p| p.partition_number == witness.partition && p.offset == witness.offset)
            .ok_or("Prepared partition is absent from metadata")?;
        let matches = partitions::policy::linux_clone_candidate(&bound.disk, &parts, part);
        lines.push(format!(
            "{role}: disk offline={:?}, style={:?}, volumes={}, eligible={matches}\n\
             Partition {}: offline={:?}, hidden={:?}, ro={:?}, shadow={:?}, letter={:?}\n\
             Offset={}, size={}, type={:?}, id present={}\n\
             Layout boot/system={:?}",
            bound.disk.is_offline,
            bound.disk.partition_style,
            bound.volumes.len(),
            part.partition_number,
            part.is_offline,
            part.is_hidden,
            part.is_read_only,
            part.is_shadow_copy,
            part.drive_letter,
            part.offset,
            part.size,
            part.gpt_type,
            part.guid.is_some(),
            parts
                .iter()
                .map(|p| (p.is_boot, p.is_system))
                .collect::<Vec<_>>(),
        ));
    }
    Ok(lines.join("\n\n"))
}

pub(in super::super) fn resume() -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    reallocation_checks::state(&disks, A, true)?;
    reallocation_checks::state(&disks, B, false)?;
    let os = reallocation_checks::os_layout(&disks)?;
    let expected = option_env!("DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256")
        .ok_or("Missing bound relocation source digest")?;
    let a = quick_partition_checks::table(A)?;
    let b = quick_partition_checks::table(B)?;
    let [source]: [Witness; 1] = prepared_layout(A, &a, &[MOVE_SIZE])?
        .try_into()
        .map_err(|_| "Expected one source")?;
    let [shrink, grow]: [Witness; 2] = prepared_layout(B, &b, &[MOVE_TARGET, GROW_TARGET])?
        .try_into()
        .map_err(|_| "Expected two targets")?;
    let bound = partition(A, &source, MOVE_SIZE, true)?;
    if resize_clone_checks::hash(&bound)? != expected {
        return Err("Prepared source differs from the bound original image".into());
    }
    let mut reader = data_windows::reader(&bound)?;
    if disk_core::btrfs::plan_resize(&mut reader, MOVE_TARGET)?.relocation_bytes() != MOVE_BYTES {
        return Err("Unexpected prepared relocation amount".into());
    }
    let contents = move_contents(&mut reader)?;
    check_blank(&partition(B, &shrink, MOVE_TARGET, false)?)?;
    check_blank(&partition(B, &grow, GROW_TARGET, false)?)?;
    save_baseline(
        &desktop,
        &CloneWitness {
            a,
            b,
            os,
            source,
            shrink,
            grow,
            contents,
            source_sha256: expected.into(),
        },
    )
}

fn partition(
    serial: &str,
    saved: &Witness,
    size: u64,
    offline: bool,
) -> Result<data_windows::Resolved, String> {
    let resolved = data_windows::resolve(&selection(serial, Some(saved.partition))?, false, false)?;
    if resolved.offset != saved.offset
        || resolved.size != size
        || resolved.disk.is_offline != Some(offline)
        || resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(serial)
    {
        return Err("Scoped Btrfs fixture geometry or offline state differs".into());
    }
    Ok(resolved)
}

fn check_blank(bound: &data_windows::Resolved) -> Result<(), String> {
    let mut reader = data_windows::reader(bound)?;
    let mut bytes = vec![0; MIB as usize];
    for _ in 0..bound.size / MIB {
        reader.read_exact(&mut bytes).map_err(err)?;
        if bytes.iter().any(|&byte| byte != 0) {
            return Err("The Btrfs clone target differs from its blank baseline".into());
        }
    }
    Ok(())
}

fn fsid(reader: &mut (impl Read + Seek)) -> Result<[u8; 16], String> {
    reader.seek(SeekFrom::Start(65536 + 32)).map_err(err)?;
    let mut bytes = [0; 16];
    reader.read_exact(&mut bytes).map_err(err)?;
    Ok(bytes)
}

fn verify_clone(
    saved: &Witness,
    size: u64,
    contents: &str,
    uuid: [u8; 16],
) -> Result<String, String> {
    let bound = partition(B, saved, size, true)?;
    let mut reader = data_windows::reader(&bound)?;
    if move_contents(&mut reader)? != contents || fsid(&mut reader)? != uuid {
        return Err("The cloned Btrfs file contents or UUID differs".into());
    }
    disk_core::btrfs::plan_resize(&mut reader, MOVE_SIZE)?;
    resize_clone_checks::hash(&bound)
}

pub(in super::super) fn verify(grow: bool) -> Result<String, String> {
    let desktop = desktop()?;
    let saved: CloneWitness = serde_json::from_reader(
        File::open(report(&desktop, "baseline"))
            .map_err(err)?
            .take(1024 * 1024),
    )
    .map_err(err)?;
    if [
        quick_partition_checks::table(A)?,
        quick_partition_checks::table(B)?,
    ] != [saved.a, saved.b]
        || reallocation_checks::os_layout(&inventory()?)? != saved.os
    {
        return Err("A, B or the guest OS partition layout changed".into());
    }
    let source = partition(A, &saved.source, MOVE_SIZE, true)?;
    if resize_clone_checks::hash(&source)? != saved.source_sha256 {
        return Err("Source partition SHA-256 changed".into());
    }
    let uuid = fsid(&mut data_windows::reader(&source)?)?;
    let small = verify_clone(&saved.shrink, MOVE_TARGET, &saved.contents, uuid)?;
    let marker = report(&desktop, "shrink");
    if grow {
        if fs::read_to_string(marker).map_err(err)? != small {
            return Err("The previously verified small clone changed".into());
        }
        let large = verify_clone(&saved.grow, GROW_TARGET, &saved.contents, uuid)?;
        Ok(format!("PASS: 448 MiB growth clone. All 51 files and prior shrink SHA-256 match. B stays offline. Full destination SHA-256:\n{large}"))
    } else {
        check_blank(&partition(B, &saved.grow, GROW_TARGET, true)?)?;
        let mut log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(err)?;
        log.write_all(small.as_bytes()).map_err(err)?;
        log.sync_all().map_err(err)?;
        Ok(format!("PASS: 192 MiB relocation clone. All 51 files, source and untouched growth target match. B stays offline. Full destination SHA-256:\n{small}"))
    }
}

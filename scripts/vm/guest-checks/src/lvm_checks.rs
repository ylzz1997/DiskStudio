//! Isolated 4Kn fixture for the production LVM2 browser.
use super::*;
#[path = "lvm_cross_checks.rs"]
pub(crate) mod cross;
#[path = "lvm_group_image_checks.rs"]
pub(crate) mod group_images;
#[path = "lvm_image_checks.rs"]
pub(crate) mod images;

const SIZE: u64 = 256 * MIB;
const OFFSETS: [u64; 2] = [10 * GIB, 10 * GIB + 512 * MIB];
const MEDIA: [&str; 2] = ["Lvm-PV-1.raw", "Lvm-PV-2.raw"];
const FOLDER: &str = "DSLVM-Recovery";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureFile {
    bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaRecord {
    member: usize,
    image_sha256: String,
    files: std::collections::BTreeMap<String, FixtureFile>,
}

#[derive(Serialize, Deserialize)]
struct Report {
    members: [Witness; 2],
    tables: [String; 2],
    destination: PathBuf,
}

fn reference() -> Result<(Vec<File>, [MediaRecord; 2]), String> {
    media_reference(MEDIA)
}

fn media_reference(media: [&str; 2]) -> Result<(Vec<File>, [MediaRecord; 2]), String> {
    let executable = std::env::current_exe().map_err(err)?;
    let expected =
        option_env!("DISKSTUDIO_LVM_FIXTURE_SHA256").ok_or("Missing LVM2 fixture digests")?;
    let mut members = Vec::new();
    let mut records = Vec::new();
    for (index, (name, digest)) in media.into_iter().zip(expected.split(',')).enumerate() {
        let path = executable.with_file_name(name);
        let file = File::open(&path).map_err(err)?;
        let record: MediaRecord =
            serde_json::from_reader(File::open(path.with_extension("json")).map_err(err)?)
                .map_err(err)?;
        if file.metadata().map_err(err)?.len() != SIZE
            || record.member != index + 1
            || record.image_sha256 != digest
            || allocation_batch::hash_file(&path)? != digest
        {
            return Err("LVM2 immutable fixture media differs".into());
        }
        members.push(file);
        records.push(record);
    }
    Ok((
        members,
        records.try_into().map_err(|_| "Expected two fixtures")?,
    ))
}

fn contents<R: Read + Seek + 'static>(
    mut readers: impl FnMut() -> Result<Vec<R>, String>,
    expected: &MediaRecord,
) -> Result<(), String> {
    for (name, bytes) in [("linear", 32 * MIB), ("striped", 96 * MIB)] {
        let group = disk_core::lvm::open(readers()?, || Ok(()))?;
        let inventory = group.inventory();
        if inventory.name != "ds_lvm_fixture"
            || (
                inventory.supplied_members,
                inventory.member_count,
                inventory.extent_size,
            ) != (2, 2, 4 * MIB)
        {
            return Err("Unexpected LVM2 volume group inventory".into());
        }
        let id = inventory
            .volumes
            .iter()
            .find(|volume| {
                volume.name == name
                    && volume.profile == name
                    && volume.readable
                    && volume.size == bytes
            })
            .ok_or("Expected a readable Linux fixture logical volume")?
            .id
            .clone();
        let digest = expected.files.get(name).ok_or("Linux file proof missing")?;
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let mut files = Vec::new();
        recovery::scan_filesystem_owned(
            group.reader(&id)?,
            recovery::ScanMode::AllFiles,
            cancel.clone(),
            |_, _| Ok(()),
            |candidate| files.push(candidate),
            |_| Ok(()),
        )?;
        let expected_name = format!("{name}.bin");
        let file = files
            .iter()
            .find(|file| file.name == expected_name && file.supported && file.size == digest.bytes)
            .ok_or("Expected the checked nested Ext4 file")?;
        let group = disk_core::lvm::open(readers()?, || Ok(()))?;
        let mut out = Vec::new();
        let actual = recovery::extract_owned(
            group.reader(&id)?,
            file,
            &mut out,
            file.size,
            cancel,
            |_| Ok(()),
        )?;
        if actual != digest.bytes || format!("{:x}", Sha256::digest(&out)) != digest.sha256 {
            return Err("Logical file differs from the independent Linux digest".into());
        }
    }
    Ok(())
}

fn empty(reader: &mut (impl Read + Seek)) -> Result<(), String> {
    let mut buffer = vec![0; MIB as usize];
    reader.rewind().map_err(err)?;
    for _ in 0..SIZE / MIB {
        reader.read_exact(&mut buffer).map_err(err)?;
        if buffer.iter().any(|&byte| byte != 0) {
            return Err("Fixed LVM2 test extent contains existing bytes".into());
        }
    }
    Ok(())
}

pub(crate) fn prepare() -> Result<String, String> {
    let sha = desktop()?;
    let report = history_report("lvm", &sha);
    if report.exists() {
        return Err("LVM2 fixture already prepared".into());
    }
    let (mut media, reference) = reference()?;
    contents(
        || {
            media
                .iter()
                .map(|file| file.try_clone().map_err(err))
                .collect()
        },
        &reference[0],
    )?;
    let mut log = backup_protection::begin("lvm-4kn-prepare")?;
    online(B)?;
    let destination = history_destination(FOLDER)?;
    if destination.try_exists().map_err(err)? {
        return Err("LVM2 recovery destination already exists".into());
    }
    for offset in OFFSETS {
        check_unused(A, offset)?;
    }
    let [_, b] = history_tables()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    let initial_layout = format_checks::layouts(&[])?;
    online(A)?;
    let mut partitions = Vec::new();
    for offset in OFFSETS {
        operation(
            A,
            partitions::Action::Create {
                offset,
                size: SIZE,
                kind: partitions::PartitionKind::Linux,
            },
        )?;
        let partition = reallocation_checks::metadata_partition(A, None, offset / MIB, SIZE / MIB)?;
        partitions.push(partition.number);
    }
    let a = test_a()?;
    if initial_layout
        != format_checks::layouts(
            &partitions
                .iter()
                .map(|&n| (a.number, n))
                .collect::<Vec<_>>(),
        )?
        || os != reallocation_checks::os_layout(&inventory()?)?
    {
        return Err("Existing VM partition layouts were changed while preparing LVM2".into());
    }
    let mut members = Vec::new();
    for (((offset, partition), image), reference) in OFFSETS
        .into_iter()
        .zip(partitions)
        .zip(&mut media)
        .zip(&reference)
    {
        let layout = witness(partition, offset)?;
        seed_linux_fixture(
            A,
            &layout,
            SIZE,
            image,
            &reference.image_sha256,
            |reader| empty(reader),
            |_| Ok(()),
        )?;
        members.push(layout);
    }
    let members: [Witness; 2] = members
        .try_into()
        .map_err(|_| "Expected two A partitions")?;
    let tables = history_tables()?;
    if tables[1] != b {
        return Err("B table changed".into());
    }
    fs::create_dir(&destination).map_err(err)?;
    let msg = format!("PASS. Both PVs on A are offline. Select A partitions {} and {}. First try the first PV alone, then select both. Read linear.bin and export striped.bin to {} with folders.",
        members[0].partition, members[1].partition, destination.display());
    let mut evidence = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(
        &mut evidence,
        &Report {
            members,
            tables,
            destination,
        },
    )
    .map_err(err)?;
    evidence.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass","layout":OFFSETS}),
    )?;
    Ok(msg)
}

fn check_unused(serial: &str, offset: u64) -> Result<(), String> {
    let table = quick_partition_checks::table(serial)?;
    let gpt = table_restore::archive_layout(&table)?;
    if offset / 4096 < gpt.first
        || (offset + SIZE) / 4096 > gpt.last + 1
        || gpt
            .entries
            .iter()
            .any(|e| offset < e.offset + e.size && e.offset < offset + SIZE)
    {
        return Err("Fixed LVM2 test scope is not wholly unallocated".into());
    }
    let resolved = data_windows::resolve(&selection(serial, None)?, false, false)?;
    empty(
        &mut data_io::Region::new(data_windows::device(&resolved, false)?, offset, SIZE)
            .map_err(err)?,
    )
}

pub(crate) fn inspect() -> Result<String, String> {
    let sha = desktop()?;
    let saved: Report =
        serde_json::from_reader(File::open(history_report("lvm", &sha)).map_err(err)?)
            .map_err(err)?;
    if history_tables()? != saved.tables
        || test_a()?.is_offline != Some(true)
        || history_destination(FOLDER)? != saved.destination
    {
        return Err(
            "The protected layouts, destination binding, or A offline state changed".into(),
        );
    }
    let (_, records) = reference()?;
    let open = || -> Result<Vec<_>, String> {
        saved
            .members
            .iter()
            .map(|layout| {
                let selected = selection(A, Some(layout.partition))?;
                let resolved = data_windows::resolve(&selected, false, false)?;
                if resolved.offset != layout.offset || resolved.size != SIZE {
                    return Err("The inspected PV range changed".into());
                }
                data_windows::reader(&resolved)
            })
            .collect()
    };
    for ((layout, mut member), record) in saved.members.iter().zip(open()?).zip(&records) {
        if witness(layout.partition, layout.offset)? != *layout
            || data_io::hash_exact(&mut member, SIZE, |_| Ok(()))? != record.image_sha256
        {
            return Err("Original PV or protected layout changed".into());
        }
    }
    contents(open, &records[0])?;
    let file = records[0]
        .files
        .get("striped")
        .ok_or("Missing Linux striped proof")?;
    verify_recovered(
        &saved.destination,
        &[("striped.bin", file.bytes, &file.sha256)],
    )?;
    Ok("PASS. Linear and striped Ext4 readback, exported striped.bin, both complete PV digests, both GPTs and the guest OS layout match. Source A stayed offline.".into())
}

//! Verify LVM2 recovery over a striped Btrfs LV with two physical source layouts.
use super::*;

const MEDIA: [&str; 2] = ["Lvm-Cross-PV-1.raw", "Lvm-Cross-PV-2.raw"];
const RECEIPT: &str = "lvm-cross-deleted";
const FOLDER: &str = "DSLVM-History";
const FIXTURES: [(&str, u64, usize); 3] = [
    (A, 13 * GIB, 0),
    (A, 13 * GIB + 512 * MIB, 1),
    (B, 13 * GIB, 1),
];

fn recover_history<R: Read + Seek + 'static>(
    mut readers: impl FnMut() -> Result<Vec<R>, String>,
    expected: &FixtureFile,
) -> Result<(), String> {
    let group = disk_core::lvm::open(readers()?, || Ok(()))?;
    let view = group.inventory();
    let [volume] = view.volumes.as_slice() else {
        return Err("Expected one logical volume".into());
    };
    if view.name != "ds_lvm_deleted_fixture"
        || (view.supplied_members, view.member_count) != (2, 2)
        || volume.name != "history"
        || volume.profile != "striped"
        || volume.size != 384 * MIB
        || !volume.readable
    {
        return Err("Wrong striped history fixture".into());
    }
    let id = volume.id.clone();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let mut files = Vec::new();
    recovery::scan_filesystem_owned(
        group.reader(&id)?,
        recovery::ScanMode::Deleted,
        cancel.clone(),
        |_, _| Ok(()),
        |file| files.push(file),
        |_| Ok(()),
    )?;
    let [file] = files.as_slice() else {
        return Err(format!(
            "Expected only deleted.bin, found {} candidates",
            files.len()
        ));
    };
    if !file.deleted || !file.supported || file.name != "deleted.bin" || file.size != expected.bytes
    {
        return Err("Unexpected LVM2 deleted candidate".into());
    }
    let mut bytes = Vec::new();
    let size = recovery::extract_owned(
        disk_core::lvm::open(readers()?, || Ok(()))?.reader(&id)?,
        file,
        &mut bytes,
        expected.bytes,
        cancel,
        |_| Ok(()),
    )?;
    if size != expected.bytes || format!("{:x}", Sha256::digest(bytes)) != expected.sha256 {
        return Err("LVM2 deleted candidate did not match the Linux snapshot".into());
    }
    Ok(())
}

fn deleted_proof(records: &[MediaRecord; 2]) -> Result<&FixtureFile, String> {
    let proof = records[0]
        .files
        .get("deleted")
        .ok_or("Missing Linux history proof")?;
    if proof.bytes != 1179648
        || !records[1]
            .files
            .get("deleted")
            .is_some_and(|other| other.bytes == proof.bytes && other.sha256 == proof.sha256)
    {
        return Err("Physical volumes have different Linux history proofs".into());
    }
    Ok(proof)
}

#[derive(Serialize, Deserialize)]
struct CrossReport {
    members: [Witness; 3],
    tables: [String; 2],
    previous_layout: Vec<(u32, u32, u64, u64, String)>,
    os: String,
    destination: PathBuf,
}

pub(crate) fn prepare() -> Result<String, String> {
    let sha = desktop()?;
    let receipt = history_report(RECEIPT, &sha);
    if receipt.exists() {
        return Err("LVM2 cross-disk fixture already prepared".into());
    }
    let (mut images, records) = media_reference(MEDIA)?;
    recover_history(
        || {
            images
                .iter()
                .map(|image| image.try_clone().map_err(err))
                .collect()
        },
        deleted_proof(&records)?,
    )?;
    let mut log = backup_protection::begin("lvm-cross-deleted-4kn-prepare")?;
    for (serial, offset, _) in FIXTURES {
        check_unused(serial, offset)
            .map_err(|cause| format!("{serial} at {} MiB: {cause}", offset / MIB))?;
    }
    online(B)?;
    let destination = history_destination(FOLDER)?;
    if destination.try_exists().map_err(err)? {
        return Err("LVM2 recovery destination already exists".into());
    }
    let previous_layout = format_checks::layouts(&[])?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    let mut locations = Vec::new();
    for (serial, offset, _) in FIXTURES {
        online(serial)?;
        operation(
            serial,
            partitions::Action::Create {
                offset,
                size: SIZE,
                kind: partitions::PartitionKind::Linux,
            },
        )?;
        let partition =
            reallocation_checks::metadata_partition(serial, None, offset / MIB, SIZE / MIB)?;
        locations.push((test_disk(serial)?.number, partition.number));
    }
    if format_checks::layouts(&locations)? != previous_layout
        || reallocation_checks::os_layout(&inventory()?)? != os
    {
        return Err("Prior GPT entries or the guest OS layout changed".into());
    }
    let mut members = Vec::new();
    for ((serial, offset, image), (_, partition)) in FIXTURES.into_iter().zip(locations) {
        online(serial)?;
        let layout = witness_on(serial, partition, offset)?;
        seed_linux_fixture(
            serial,
            &layout,
            SIZE,
            &mut images[image],
            &records[image].image_sha256,
            |reader| empty(reader),
            |_| Ok(()),
        )?;
        members.push(layout);
    }
    online(B)?;
    images::check_dir(
        destination.parent().ok_or("Missing output volume root")?,
        B,
        None,
    )?;
    fs::create_dir(&destination).map_err(err)?;
    images::check_dir(&destination, B, None)?;
    let members: [Witness; 3] = members
        .try_into()
        .map_err(|_| "Expected three fixture partitions")?;
    let source_a = test_disk(A)?.number;
    let source_b = test_disk(B)?.number;
    let message = format!(
        "PASS. PV1: A Disk {source_a}/{}. PV2: A Disk {source_a}/{} or B Disk {source_b}/{}. On A+B, read the deleted candidate and reject B as output. On A+A, export deleted.bin with folders to {}.",
        members[0].partition, members[1].partition, members[2].partition, destination.display()
    );
    let tables = history_tables()?;
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(
        &mut report,
        &CrossReport {
            members,
            tables,
            previous_layout,
            os,
            destination,
        },
    )
    .map_err(err)?;
    report.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass", "layout":FIXTURES}),
    )?;
    Ok(message)
}

pub(crate) fn inspect() -> Result<String, String> {
    let sha = desktop()?;
    let saved: CrossReport =
        serde_json::from_reader(File::open(history_report(RECEIPT, &sha)).map_err(err)?)
            .map_err(err)?;
    if history_tables()? != saved.tables
        || test_disk(A)?.is_offline != Some(true)
        || reallocation_checks::os_layout(&inventory()?)? != saved.os
        || history_destination(FOLDER)? != saved.destination
    {
        return Err("Protected layouts, disk state or output binding changed".into());
    }
    images::check_dir(&saved.destination, B, None)?;
    let (_, records) = media_reference(MEDIA)?;
    let open = |index: usize| {
        let (serial, offset, _) = FIXTURES[index];
        let resolved = data_windows::resolve(
            &selection(serial, Some(saved.members[index].partition))?,
            false,
            false,
        )?;
        if saved.members[index].offset != offset
            || resolved.offset != offset
            || resolved.size != SIZE
        {
            return Err("Physical volume extent differs".to_owned());
        }
        data_windows::reader(&resolved)
    };
    let mut locations = Vec::new();
    for (index, (serial, _, image)) in FIXTURES.into_iter().enumerate() {
        locations.push((test_disk(serial)?.number, saved.members[index].partition));
        if data_io::hash_exact(&mut open(index)?, SIZE, |_| Ok(()))? != records[image].image_sha256
        {
            return Err("Full physical volume digest changed".into());
        }
    }
    if format_checks::layouts(&locations)? != saved.previous_layout {
        return Err("Prior partition layouts changed".into());
    }
    let proof = deleted_proof(&records)?;
    recover_history(|| Ok(vec![open(0)?, open(2)?]), proof)?;
    verify_recovered(
        &saved.destination,
        &[("deleted.bin", proof.bytes, &proof.sha256)],
    )?;
    let result = fs::read_dir(&saved.destination)
        .map_err(err)?
        .next()
        .ok_or("Missing LVM2 export")?
        .map_err(err)?;
    let root = result.path();
    let parents = fs::read_dir(&root)
        .map_err(err)?
        .map(|item| item.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    if !parents
        .iter()
        .any(|path| path.join("nested/deleted.bin").is_file())
    {
        return Err("Recovered file lost its historical root or nested directory".into());
    }
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(root.join(recovery_export::REPORT)).map_err(err)?)
            .map_err(err)?;
    let source = &manifest["physicalLvmSource"];
    let members = source["members"]
        .as_array()
        .ok_or("Missing export PV ranges")?;
    if source["volumeGroup"] != "ds_lvm_deleted_fixture"
        || source["logicalVolume"] != "history"
        || source["sequence"].as_u64().is_none_or(|n| n == 0)
        || source["logicalVolumeId"].as_str().is_none_or(str::is_empty)
        || members.len() != 2
        || FIXTURES[..2]
            .iter()
            .zip(&locations[..2])
            .any(|((_, offset, _), (disk, partition))| {
                !members.iter().any(|member| {
                    member["diskNumber"] == *disk
                        && member["partition"] == *partition
                        && member["offset"] == offset.to_string()
                        && member["size"] == SIZE.to_string()
                })
            })
    {
        return Err("Physical LVM2 export provenance differs".into());
    }
    Ok("PASS. Cross-disk deleted candidate and the exported file match the Linux digest. Original directories, three physical-volume digests, both GPTs and guest OS layout match.".into())
}

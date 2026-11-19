//! Stage verified Linux fixtures as ordinary files on the isolated test A disk.
use super::*;
use std::os::windows::fs::MetadataExt;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const SOURCE: &str = "DSLVM-Images";
const OUTPUTS: [&str; 2] = ["DSLVM-Image-Raw", "DSLVM-Image-Disk"];
const DISK: &str = "Lvm-Group.vhdx";
const STARTS: [u64; 2] = [MIB, 2 * MIB + SIZE];
const STAGE_LABEL: &str = "DSLVMIMG";
const STAGE_OFFSET: u64 = 12 * GIB;
const STAGE_SIZE: u64 = GIB;

#[derive(Deserialize, Serialize)]
struct ImageReport {
    source: PathBuf,
    destination: [PathBuf; 2],
    digests: [String; 3],
    tables: [String; 2],
    os: String,
}

pub(super) fn source_directory() -> Result<PathBuf, String> {
    let partition = reallocation_checks::metadata_partition(
        A,
        Some(STAGE_LABEL),
        STAGE_OFFSET / MIB,
        STAGE_SIZE / MIB,
    )?;
    let letter = partition
        .drive_letter
        .ok_or("A fixture volume has no drive letter")?;
    let root = PathBuf::from(format!("{letter}:\\"));
    let target = selection(A, Some(partition.number))?.target;
    if data_windows::folder_location(&root)?.1 != target {
        return Err("A image directory identity did not match".into());
    }
    Ok(root.join(SOURCE))
}

fn create_stage_volume() -> Result<(), String> {
    use partitions::{Action, DriveLetter, FileSystem, PartitionKind};
    let before = table_restore::archive_layout(&quick_partition_checks::table(A)?)?;
    let end = STAGE_OFFSET + STAGE_SIZE;
    if STAGE_OFFSET / 4096 < before.first
        || end / 4096 > before.last + 1
        || before
            .entries
            .iter()
            .any(|e| e.offset < end && STAGE_OFFSET < e.offset + e.size)
    {
        return Err("The dedicated A fixture range is occupied; preserve it".into());
    }
    let others = format_checks::layouts(&[])?;
    let guard_disk = selection(B, None)?.target;
    operation(
        A,
        Action::Create {
            offset: STAGE_OFFSET,
            size: STAGE_SIZE,
            kind: PartitionKind::Windows,
        },
    )?;
    let partition =
        reallocation_checks::metadata_partition(A, None, STAGE_OFFSET / MIB, STAGE_SIZE / MIB)?
            .number;
    operation(
        A,
        Action::Format {
            partition,
            label: STAGE_LABEL.into(),
            file_system: FileSystem::Ntfs,
            cluster_size: 0,
            options: Default::default(),
        },
    )?;
    let formatted = reallocation_checks::metadata_partition(
        A,
        Some(STAGE_LABEL),
        STAGE_OFFSET / MIB,
        STAGE_SIZE / MIB,
    )?;
    if formatted.drive_letter.is_none() {
        operation(
            A,
            Action::SetDriveLetter {
                partition,
                letter: Some(DriveLetter("A".into())),
            },
        )?;
    }
    if others != format_checks::layouts(&[(test_a()?.number, partition)])? {
        return Err("An existing test or system partition changed during staging".into());
    }
    let source = source_directory()?;
    data_windows::destination_folder(
        source.parent().ok_or("Missing fixture volume root")?,
        &guard_disk,
        2 * SIZE + 64 * MIB,
    )?;
    Ok(())
}

pub(super) fn check_dir(path: &Path, serial: &str, partition: Option<u32>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(err)?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || data_windows::folder_location(path)?.1 != selection(serial, partition)?.target
    {
        return Err("Image acceptance directory is not on the expected isolated disk".into());
    }
    Ok(())
}

pub(super) fn create_fixture(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(err)?;
    if data_windows::file_location(&file)?.target != selection(A, None)?.target {
        return Err("Staging file did not bind to test A".into());
    }
    Ok(file)
}

pub(crate) fn prepare() -> Result<String, String> {
    let sha = desktop()?;
    let receipt = history_report("lvm-images", &sha);
    if receipt.exists() {
        return Err("LVM2 image fixtures already staged".into());
    }
    let (mut media, records) = reference()?;
    let container_sha = option_env!("DISKSTUDIO_LVM_CONTAINER_SHA256")
        .ok_or("Missing immutable whole-disk image digest")?;
    let container_path = std::env::current_exe().map_err(err)?.with_file_name(DISK);
    let mut container = File::open(&container_path).map_err(err)?;
    let bytes = container.metadata().map_err(err)?.len();
    if !(1..=64 * MIB).contains(&bytes)
        || allocation_batch::hash_file(&container_path)? != container_sha
    {
        return Err("The immutable whole-disk fixture differs from its digest".into());
    }
    let before = history_tables()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    online(A)?;
    online(B)?;
    let destination = [
        history_destination(OUTPUTS[0])?,
        history_destination(OUTPUTS[1])?,
    ];
    if destination.iter().any(|path| path.exists()) {
        return Err("Preserve existing LVM2 result directories".into());
    }
    create_stage_volume()?;
    let tables = history_tables()?;
    if tables[1] != before[1] || reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("B or the guest OS layout changed during A fixture preparation".into());
    }
    let source = source_directory()?;
    fs::create_dir(&source).map_err(err)?;
    check_dir(&source, A, None)?;
    let disk_path = source.join(DISK);
    for ((name, input), record) in MEDIA.into_iter().zip(&mut media).zip(&records) {
        let path = source.join(name);
        let mut output = create_fixture(&path)?;
        std::io::copy(input, &mut output).map_err(|e| format!("Copying {name}: {e}"))?;
        output.sync_all().map_err(err)?;
        drop(output);
        let file = File::open(&path).map_err(err)?;
        let meta = file.metadata().map_err(err)?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || meta.len() != SIZE
            || data_windows::file_location(&file)?.target != selection(A, None)?.target
            || data_io::hash_exact(&mut &file, SIZE, |_| Ok(()))? != record.image_sha256
        {
            return Err("Staged PV file differs from the Linux fixture".into());
        }
    }
    let mut disk = create_fixture(&disk_path)?;
    std::io::copy(&mut container, &mut disk)
        .map_err(|e| format!("Staging VHDX at {}: {e}", disk_path.display()))?;
    disk.sync_all()
        .map_err(|e| format!("Flushing composed image: {e}"))?;
    drop(disk);
    if allocation_batch::hash_file(&disk_path)? != container_sha {
        return Err("The copied whole-disk image differs from its digest".into());
    }
    for path in &destination {
        fs::create_dir(path).map_err(err)?;
        check_dir(path, B, None)?;
    }
    if history_tables()? != tables || reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("A partition layout changed during ordinary-file staging".into());
    }
    let saved = ImageReport {
        source,
        destination,
        digests: [
            records[0].image_sha256.clone(),
            records[1].image_sha256.clone(),
            container_sha.into(),
        ],
        tables,
        os,
    };
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(&mut file, &saved).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!(
        "PASS. Matching desktop payload. Select both PV files from {} and then test the VHDX GPT image {}. Export striped.bin with its directory to {} and {} respectively.",
        saved.source.display(), disk_path.display(), saved.destination[0].display(), saved.destination[1].display()
    ))
}

pub(crate) fn inspect() -> Result<String, String> {
    let sha = desktop()?;
    let saved: ImageReport =
        serde_json::from_reader(File::open(history_report("lvm-images", &sha)).map_err(err)?)
            .map_err(err)?;
    if source_directory()? != saved.source
        || history_tables()? != saved.tables
        || reallocation_checks::os_layout(&inventory()?)? != saved.os
    {
        return Err("Fixture source binding or protected layouts changed".into());
    }
    check_dir(&saved.source, A, None)?;
    for (name, digest) in [MEDIA[0], MEDIA[1], DISK].into_iter().zip(&saved.digests) {
        let path = saved.source.join(name);
        let file = File::open(path).map_err(err)?;
        if data_windows::file_location(&file)?.target != selection(A, None)?.target
            || data_io::hash_exact(&mut &file, file.metadata().map_err(err)?.len(), |_| Ok(()))?
                != *digest
        {
            return Err("A source image changed".into());
        }
    }
    let (_, records) = reference()?;
    let striped = records[0]
        .files
        .get("striped")
        .ok_or("Missing striped fixture proof")?;
    for (index, (path, name)) in saved.destination.iter().zip(OUTPUTS).enumerate() {
        if *path != history_destination(name)? {
            return Err("The export destination binding changed".into());
        }
        check_dir(path, B, None)?;
        verify_recovered(path, &[("striped.bin", striped.bytes, &striped.sha256)])?;
        let result = fs::read_dir(path)
            .map_err(err)?
            .next()
            .ok_or("Missing result")?
            .map_err(err)?;
        let manifest: serde_json::Value = serde_json::from_reader(
            File::open(result.path().join(recovery_export::REPORT)).map_err(err)?,
        )
        .map_err(err)?;
        let proof = &manifest["imageLvmSource"];
        if proof["volumeGroup"] != "ds_lvm_fixture"
            || proof["logicalVolume"] != "striped"
            || proof["sequence"] != 3
        {
            return Err("Missing image LVM2 export provenance".into());
        }
        let expected = if index == 0 {
            [(MEDIA[0], 0), (MEDIA[1], 0)]
        } else {
            [(DISK, STARTS[0]), (DISK, STARTS[1])]
        };
        let members = proof["members"]
            .as_array()
            .ok_or("Missing image member provenance")?;
        let format = if index == 0 { "RAW" } else { "VHDX" };
        if members.len() != 2
            || expected.iter().any(|(file, offset)| {
                !members.iter().any(|member| {
                    member["files"] == serde_json::json!([file])
                        && member["format"] == format
                        && member["offset"] == offset.to_string()
                        && member["size"] == SIZE.to_string()
                })
            })
        {
            return Err("Export provenance image names or bounded ranges differ".into());
        }
    }
    Ok("PASS. Both exported striped.bin files match the independent Linux SHA-256. Both manifests identify the original image ranges. All three input images, both GPTs and the guest OS layout match the baseline.".into())
}

//! Native retained signature scans and independent verification of their exports.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};

const ROOT: &str = "DS-Retained-Signatures";
const FILES: [&str; 3] = ["test.vhdx", "base.dsbackup", "latest.dsbackup"];
const PDF: &[u8] = include_bytes!("../../../../tests/fixtures/ext-complete-sample.pdf");
const SECTOR: u64 = 4096;
const SIZE: u64 = 24 * MIB;
const PART_START: u64 = 8 * MIB;
const BASE_PDF: u64 = 2 * MIB;
const LATEST_PDF: u64 = 4 * MIB;

#[derive(Deserialize, serde::Serialize)]
struct Baseline {
    desktop: String,
    layouts: [String; 3],
    inputs: Vec<String>,
    base: String,
    latest: String,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn inputs(folder: &Dir) -> Result<Vec<String>, String> {
    FILES
        .iter()
        .map(|name| folder.read(name).map(|bytes| sha(&bytes)).map_err(err))
        .collect()
}

fn create(folder: &Dir, name: &str) -> Result<std::fs::File, String> {
    folder
        .open_with(
            name,
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .map(|file| file.into_std())
        .map_err(err)
}

fn baseline(input: &Dir) -> Result<Baseline, String> {
    serde_json::from_reader(input.open("baseline.json").map_err(err)?.take(8192)).map_err(err)
}

fn fixture() -> Result<(Vec<u8>, u64), String> {
    let (mut volume, free) = signature_scope_checks::fixture(signature_scope_checks::Sample::Pdf)?;
    volume[28..32].copy_from_slice(&((PART_START / SECTOR) as u32).to_le_bytes());
    let start = free as usize;
    volume[start..start + SECTOR as usize].fill(0);
    volume[start..start + PDF.len()].copy_from_slice(PDF);
    let table = table_restore::new_gpt(
        SIZE,
        SECTOR,
        sha(b"DiskStudio retained signature fixture"),
        uuid::Uuid::from_u128(0xa12f_1411_2e79_4bfd_b713_d5da_f856_3c01),
        &[table_restore::Entry {
            slot: 1,
            guid: "2b3d84d7-e5a4-4f37-9daf-bbaa11a23402".into(),
            kind: partitions::policy::BASIC_DATA.into(),
            offset: PART_START,
            size: volume.len() as u64,
            attributes: 0,
            name: "FAT12 samples".encode_utf16().collect(),
        }],
    )?;
    let mut disk = vec![0; SIZE as usize];
    for extent in table.extents {
        disk[extent.offset as usize..][..extent.bytes.len()].copy_from_slice(&extent.bytes);
    }
    disk[PART_START as usize..][..volume.len()].copy_from_slice(&volume);
    disk[BASE_PDF as usize..][..PDF.len()].copy_from_slice(PDF);
    Ok((disk, free))
}

fn prepare_volumes() -> Result<(), String> {
    use partitions::{Action, FileSystem, PartitionKind};
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    for serial in [A, B] {
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    const OFFSET: u64 = 32 * MIB;
    const LENGTH: u64 = 2 * GIB;
    for (serial, label) in [(A, "DSSIGOUT"), (B, "DSSIGIN")] {
        online(serial)?;
        operation(serial, Action::Initialize {})?;
        operation(
            serial,
            Action::Create {
                offset: OFFSET,
                size: LENGTH,
                kind: PartitionKind::Windows,
            },
        )?;
        let created = fat_resize_checks::model(serial)?;
        let part =
            reallocation_checks::metadata_partition(serial, None, OFFSET / MIB, LENGTH / MIB)?;
        operation(
            serial,
            Action::Format {
                partition: part.number,
                label: label.into(),
                file_system: FileSystem::Ntfs,
                cluster_size: 4096,
                options: Default::default(),
            },
        )?;
        if !fat_resize_checks::model(serial)?.agrees(&created) {
            return Err("Preparing an NTFS volume changed its GPT".into());
        }
    }
    Ok(())
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    prepare_volumes()?;
    let (input, input_path) = ext_reconstruct_checks::b_root()?;
    let (output, output_path) = ntfs_reconstruction_checks::verified_a_root()?;
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    if layouts[2] != os {
        return Err("Guest OS layout changed".into());
    }
    input.create_dir(ROOT).map_err(err)?;
    output.create_dir(ROOT).map_err(err)?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    output.create_dir("image").map_err(err)?;
    output.create_dir("backup").map_err(err)?;
    let (mut disk, _) = fixture()?;
    let mut encoder = image_convert::Encoder::new(
        create(&input, FILES[0])?,
        image_convert::Options {
            format: image_convert::Format::VhdxDynamic,
            size: SIZE,
            sector: SECTOR,
        },
        image_convert::Identity {
            created: *uuid::Uuid::new_v4().as_bytes(),
            modified: *uuid::Uuid::new_v4().as_bytes(),
            vhd_timestamp: 0,
            file_name: FILES[0].into(),
        },
    )?;
    for (index, bytes) in disk.chunks(image_convert::CHUNK).enumerate() {
        encoder.push((index * image_convert::CHUNK) as u64, bytes)?;
    }
    for (index, bytes) in disk.chunks(image_convert::CHUNK).enumerate() {
        encoder.verify_source((index * image_convert::CHUNK) as u64, bytes)?;
    }
    encoder.finish(|file| file.sync_all().map_err(err), |_| Ok(()))?;
    let base = backup_archive::create(
        &mut Cursor::new(&disk),
        &mut create(&input, FILES[1])?,
        "disk",
        SIZE,
        SECTOR,
        true,
        None,
        |_| Ok(()),
    )?;
    let chain = backup_archive::Chain::new(backup_archive::Archive::from_reader(
        input.open(FILES[1]).map_err(err)?.into_std(),
    )?);
    disk[BASE_PDF as usize..][..PDF.len()].fill(0);
    disk[LATEST_PDF as usize..][..PDF.len()].copy_from_slice(PDF);
    let latest = backup_archive::create(
        &mut Cursor::new(&disk),
        &mut create(&input, FILES[2])?,
        "disk",
        SIZE,
        SECTOR,
        true,
        Some(&chain),
        |_| Ok(()),
    )?;
    let witness = Baseline {
        desktop,
        layouts,
        inputs: inputs(&input)?,
        base: base.id,
        latest: latest.id,
    };
    let receipt = create(&input, "baseline.json")?;
    serde_json::to_writer(&receipt, &witness).map_err(err)?;
    receipt.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: Sources in {}. Export VHDX partition (PDF only, free clusters, sector aligned) to image, and earliest backup point (whole disk, PDF only, 2-3 MiB, sector aligned) to backup under {}.",
        input_path.join(ROOT).display(), output_path.join(ROOT).display(),
    ))
}

fn check_export(output: &Dir, offset: u64) -> Result<serde_json::Value, String> {
    let mut entries = output.entries().map_err(err)?;
    let entry = entries
        .next()
        .ok_or("Expected one export folder")?
        .map_err(err)?;
    let folder = entry.file_name();
    if entries.next().is_some()
        || !folder
            .to_string_lossy()
            .starts_with("DiskStudio-recovered-")
    {
        return Err("Unexpected native output folder".into());
    }
    let dir = output.open_dir(folder).map_err(err)?;
    let manifest: serde_json::Value = serde_json::from_reader(
        dir.open(recovery_export::REPORT)
            .map_err(err)?
            .take(128 * 1024),
    )
    .map_err(err)?;
    let name = format!("recovered_{offset:016x}.pdf");
    if manifest["completed"] != true
        || manifest["savedFiles"] != 1
        || manifest["failedFiles"] != 0
        || manifest["entries"][0]["originalName"] != name
        || manifest["outcomes"][0]["sha256"] != sha(PDF)
        || dir.read(format!("000000_{name}")).map_err(err)? != PDF
    {
        return Err("Recovered PDF, source-relative offset or SHA-256 differs".into());
    }
    Ok(manifest)
}

pub(super) fn verify() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (input, _) = ext_reconstruct_checks::b_root()?;
    let (output, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    let baseline = baseline(&input)?;
    if desktop != baseline.desktop
        || inputs(&input)? != baseline.inputs
        || ntfs_reconstruction_checks::table_hashes()? != baseline.layouts
    {
        return Err("The desktop payload, source files or verified layouts changed".into());
    }
    let (_, free) = fixture()?;
    let image = check_export(&output.open_dir("image").map_err(err)?, free)?;
    let backup = check_export(&output.open_dir("backup").map_err(err)?, BASE_PDF)?;
    if image["imageSource"]["fileName"] != FILES[0]
        || image["imageSource"]["offset"] != PART_START.to_string()
        || image["imageSource"]["size"] != (8 * MIB).to_string()
        || image["imageSource"]["sector"] != SECTOR
        || backup["backupSource"]["pointId"] != baseline.base
        || backup["backupSource"]["pointId"] == baseline.latest
    {
        return Err("The export image geometry or backup point differs".into());
    }
    Ok(format!(
        "PASS: both PDF files match SHA-256 {}. Verified free-cluster image offset, earlier backup point, all original files, both GPTs and guest OS layout.",
        sha(PDF),
    ))
}

pub(super) fn prepare_progress() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (input, source_path) = ext_reconstruct_checks::b_root()?;
    let (output, output_path) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    if baseline(&input)?.layouts != ntfs_reconstruction_checks::table_hashes()? {
        return Err("Retained fixture GPTs or guest OS layout changed".into());
    }
    for name in [
        "image-progress",
        "backup-progress",
        "resume-image",
        "resume-backup",
    ] {
        output.create_dir(name).map_err(err)?;
    }
    let mut receipt = create(&output, "progress-desktop.txt")?;
    receipt.write_all(desktop.as_bytes()).map_err(err)?;
    receipt.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: sources retained in {}. Save the paused VHDX FAT12 PDF scan to image-progress. Save the earliest whole-disk backup PDF scan of 2-3 MiB to backup-progress. Load each progress file and export to resume-image or resume-backup under {}.",
        source_path.join(ROOT).display(), output_path.join(ROOT).display(),
    ))
}

fn saved_progress(output: &Dir, folder: &str) -> Result<serde_json::Value, String> {
    let dir = output.open_dir(folder).map_err(err)?;
    let mut entries = dir.entries().map_err(err)?;
    let name = entries
        .next()
        .ok_or("Missing native progress file")?
        .map_err(err)?
        .file_name();
    if entries.next().is_some() || !name.to_string_lossy().ends_with(".dsrecover") {
        return Err("Expected a single native .dsrecover file".into());
    }
    let envelope: serde_json::Value =
        serde_json::from_reader(dir.open(name).map_err(err)?.take(1024 * 1024)).map_err(err)?;
    if envelope["version"] != 6
        || envelope["format"] != "DiskStudio retained signature progress"
        || envelope["progress"]["options"]["kinds"] != serde_json::json!(["pdf"])
        || envelope["progress"]["options"]["sectorAligned"] != true
        || envelope["progress"]["binding"]["sector"] != SECTOR
        || envelope["progress"]["cursor"]["count"] != 1
    {
        return Err("Retained progress envelope, options or candidate count differs".into());
    }
    Ok(envelope["progress"].clone())
}

pub(super) fn verify_progress() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (input, _) = ext_reconstruct_checks::b_root()?;
    let (output, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    let baseline = baseline(&input)?;
    if output.read_to_string("progress-desktop.txt").map_err(err)? != desktop
        || inputs(&input)? != baseline.inputs
        || ntfs_reconstruction_checks::table_hashes()? != baseline.layouts
    {
        return Err("The desktop payload, source files or verified layouts changed".into());
    }
    let (_, free) = fixture()?;
    let image = saved_progress(&output, "image-progress")?;
    let backup = saved_progress(&output, "backup-progress")?;
    let done = image["cursor"]["consumed"]
        .as_u64()
        .ok_or("Image checkpoint cursor missing")?;
    if image["kind"] != "image"
        || image["partition"].is_null()
        || image["binding"]["offset"] != PART_START
        || image["binding"]["size"] != 8 * MIB
        || image["options"]["freeOnly"] != true
        || image["cursor"]["sealed"] != false
        || image["records"][0]["offset"] != free
        || !(free..(8 * MIB)).contains(&done)
        || backup["kind"] != "backup"
        || !backup["partition"].is_null()
        || backup["binding"]["size"] != SIZE
        || backup["options"]["start"] != BASE_PDF
        || backup["options"]["length"] != MIB
        || backup["cursor"]["sealed"] != true
        || backup["records"][0]["offset"] != BASE_PDF
    {
        return Err("Unexpected paused image or completed backup checkpoint".into());
    }
    let image = check_export(&output.open_dir("resume-image").map_err(err)?, free)?;
    let backup = check_export(&output.open_dir("resume-backup").map_err(err)?, BASE_PDF)?;
    if image["imageSource"]["fileName"] != FILES[0]
        || image["imageSource"]["offset"] != PART_START.to_string()
        || backup["backupSource"]["pointId"] != baseline.base
    {
        return Err("The export image geometry or backup point differs".into());
    }
    Ok(format!("PASS: version 6 paused image at {done} B and completed backup checkpoint verified. Both PDF SHA-256 values match. Three source files, both GPTs and guest OS layout match."))
}

const CHAIN_ROOT: &str = "DS-Vhdx-Chain";
const CHAIN: [&str; 3] = ["base.vhdx", "middle.avhdx", "latest.avhdx"];

fn native_child(directory: &std::path::Path, parent: &str, child: &str) -> Result<(), String> {
    use std::{
        os::windows::ffi::OsStrExt,
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{Foundation::CloseHandle, Storage::Vhd::*};
    let wide = |name: &str| {
        directory
            .join(name)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let parent = wide(parent);
    let output = wide(child);
    let kind = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let mut parameters = CREATE_VIRTUAL_DISK_PARAMETERS {
        Version: CREATE_VIRTUAL_DISK_VERSION_2,
        ..Default::default()
    };
    parameters.Anonymous.Version2 = CREATE_VIRTUAL_DISK_PARAMETERS_0_1 {
        ParentPath: parent.as_ptr(),
        ParentVirtualStorageType: kind,
        ..Default::default()
    };
    let mut handle = null_mut();
    // Only ordinary files in the verified B directory. No virtual disk is attached.
    let result = unsafe {
        CreateVirtualDisk(
            &kind,
            output.as_ptr(),
            VIRTUAL_DISK_ACCESS_NONE,
            null_mut(),
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &parameters,
            null(),
            &mut handle,
        )
    };
    if result != 0 {
        return Err(format!(
            "Windows could not create {child}: {}",
            std::io::Error::from_raw_os_error(result as i32)
        ));
    }
    if unsafe { CloseHandle(handle) } == 0 {
        return Err(err(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn chain_hashes(folder: &Dir) -> Result<Vec<String>, String> {
    CHAIN
        .iter()
        .map(|name| {
            let data = folder.read(name).map_err(err)?;
            Ok(sha(&data))
        })
        .collect()
}

pub(super) fn prepare_vhdx_chain() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (b, source) = ext_reconstruct_checks::b_root()?;
    let (a, output) = ntfs_reconstruction_checks::verified_a_root()?;
    let prior = b.open_dir(ROOT).map_err(err)?;
    let reference = baseline(&prior)?;
    if ntfs_reconstruction_checks::table_hashes()? != reference.layouts
        || inputs(&prior)? != reference.inputs
    {
        return Err("Retained fixture sources or layouts changed".into());
    }
    b.create_dir(CHAIN_ROOT).map_err(err)?;
    a.create_dir(CHAIN_ROOT).map_err(err)?;
    let input = b.open_dir(CHAIN_ROOT).map_err(err)?;
    let writer = a.open_dir(CHAIN_ROOT).map_err(err)?;
    writer.create_dir("pdf").map_err(err)?;
    let mut seed = create(&input, CHAIN[0])?;
    std::io::copy(&mut prior.open(FILES[0]).map_err(err)?, &mut seed).map_err(err)?;
    seed.sync_all().map_err(err)?;
    drop(seed);
    let source = source.join(CHAIN_ROOT);
    native_child(&source, CHAIN[0], CHAIN[1])?;
    native_child(&source, CHAIN[1], CHAIN[2])?;
    let baseline = serde_json::json!({
        "desktop": desktop,
        "sources": chain_hashes(&input)?,
        "layouts": reference.layouts,
    });
    let mut receipt = create(&writer, "baseline.json")?;
    serde_json::to_writer(&mut receipt, &baseline).map_err(err)?;
    receipt.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: Windows created the VHDX/AVHDX chain in {}. Open latest.avhdx, verify that the parents are required, then select base.vhdx and middle.avhdx. Scan the FAT12 partition with PDF, free clusters and sector alignment. Save one recovered PDF to pdf and flatten the chain to merged.img in {}.",
        source.display(), output.join(CHAIN_ROOT).display(),
    ))
}

pub(super) fn verify_vhdx_chain() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (b, _) = ext_reconstruct_checks::b_root()?;
    let (a, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = b.open_dir(CHAIN_ROOT).map_err(err)?;
    let output = a.open_dir(CHAIN_ROOT).map_err(err)?;
    let baseline: serde_json::Value =
        serde_json::from_reader(output.open("baseline.json").map_err(err)?.take(4096))
            .map_err(err)?;
    // A corrected application can be tested on the already verified inputs.
    // The test build must name the original fixture's desktop digest explicitly.
    let seeded_by = option_env!("DISKSTUDIO_ACCEPTANCE_VHDX_CHAIN_SEED_SHA256").unwrap_or(&desktop);
    let prior = b.open_dir(ROOT).map_err(err)?;
    if baseline["desktop"] != seeded_by
        || baseline["sources"] != serde_json::json!(chain_hashes(&input)?)
        || baseline["layouts"] != serde_json::json!(ntfs_reconstruction_checks::table_hashes()?)
        || inputs(&prior)? != self::baseline(&prior)?.inputs
    {
        return Err("The verified package, layouts or source files changed".into());
    }
    let (disk, free) = fixture()?;
    let mut file = output.open("merged.img").map_err(err)?.take(SIZE + 1);
    let mut merged = Vec::new();
    file.read_to_end(&mut merged).map_err(err)?;
    let report = check_export(&output.open_dir("pdf").map_err(err)?, free)?;
    if merged != disk
        || report["imageSource"]["fileName"] != CHAIN[2]
        || report["imageSource"]["offset"] != PART_START.to_string()
        || report["imageSource"]["sector"] != SECTOR
    {
        return Err("The merged logical bytes or the PDF provenance differs".into());
    }
    Ok(format!(
        "PASS: 3 immutable chain members. 24 MiB flattened bytes SHA-256 {} and inherited PDF SHA-256 {} match the original fixture. Both test GPTs and guest OS layout match.",
        sha(&merged), sha(PDF),
    ))
}

#[test]
fn synthetic_fixture_excludes_allocated_and_unaligned_pdf_controls() {
    let (disk, free) = fixture().unwrap();
    let mut part = Cursor::new(&disk[PART_START as usize..(PART_START + 8 * MIB) as usize]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let plan = used_space::inspect(&mut part, 8 * MIB, SECTOR, None, &cancel).unwrap();
    let options = signature_scan::Options {
        kinds: vec![signature_scan::Kind::Pdf],
        free_only: true,
        sector_aligned: true,
        ..Default::default()
    };
    let mut results = Vec::new();
    signature_scan::continue_scan_scoped(
        &mut part,
        8 * MIB,
        &options,
        &mut signature_scan::Checkpoint::default(),
        SECTOR,
        Some(&plan),
        |_, _| Ok(()),
        |candidate| results.push(candidate),
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, format!("recovered_{free:016x}.pdf"));
    assert_eq!(results[0].size, PDF.len() as u64);
    signature_scan::verify_free_candidates(&plan, &results).unwrap();
}

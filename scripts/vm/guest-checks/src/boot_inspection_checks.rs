//! Stage fixed boot-inspection files on the verified guest A data volume.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read, Write};

const FOLDER: &str = "DS-Boot-Inspection";
const FILES: [(&str, u64, &str); 3] = [
    (
        "Boot-UEFI.img",
        44_040_192,
        "b0bf3431e2252cc21f78e672af32dc0c9421d47dafcb5d388ed1831281c5dbdc",
    ),
    (
        "Boot-BIOS.dsbackup",
        10_489_856,
        "ed851a4367a24942443e01e4948938c9ed7316649eccac684acb457aee4399d1",
    ),
    (
        "Boot-Dirty.img",
        8_388_608,
        "e6cf5730826753a81ace40466e2471e982338cf97cc1598d45599471567c88a1",
    ),
];
const REFERENCE_FOLDER: &str = "DS-Boot-References";
const REFERENCE_FILES: [(&str, u64, &str); 2] = [
    (
        "Boot-References-GPT.img",
        53_477_376,
        "8253e33937bd4e6efd6d2c4dae28e85c4e51b5e516b87cbd6148978175f92622",
    ),
    (
        "Boot-References-MBR.dsbackup",
        19_927_040,
        "18136e5393630e6e6f8b4f8eb5bda62417d8bc6454e23ff7f79f108c760544bb",
    ),
];
const INHERITANCE_FOLDER: &str = "DS-Boot-Inheritance";
const INHERITANCE_IMAGE: (&str, u64, &str) = (
    "Boot-Inheritance-NTFS.img",
    70_254_592,
    "3bcbdeb8c5ad5aee45b135295fb32bdb291943c2c547e5dab7e36fb094f4380b",
);
const WINLOAD: &str = "2829a8b2f97f018cb148f0235aedce5b49303155f01c2d6118c4748ce452162f";
const REPAIR_FOLDER: &str = "DS-Boot-Repair";
const REPAIR_ENTRY: &str = "eaf9ef92-c25a-48f2-bc9a-13c6debf0001";
const RESTORED_MANAGER: &str = "d8ee3eb9725b14981aeca1cb2e9e984d39d6e8c6f6cec7f8a6d1cd4b15f7b45b";
const FILES_FOLDER: &str = "DS-Boot-Files";
const FILES_BACKUP: &str = "EFI-files-before-repair.zip";
const STORE_FOLDER: &str = "DS-Boot-Store";
const TEMPLATE_SHA: &str = "e105605b19d922ec5f3783a20dcb5d0f0d655beaa65574e114c044ffa2a7962a";
const STORE_LOADER: &str = "b012b84d-c47c-4ed5-b722-c0c42163e569";
const STORE_LOGS: [(&str, u8, usize); 3] = [
    ("BCD.LOG", 0x76, 19),
    ("BCD.LOG1", 0x72, 777),
    ("BCD.LOG2", 0x39, 4133),
];
const DIRTY_FOLDER: &str = "DS-Boot-Dirty";
const DAMAGED_FOLDER: &str = "DS-Boot-Damaged";
const REPLAY_DEFAULT: &str = "089eaa02-cd8c-440b-83e6-b007e0cbb175";
const REPLAY_OTHER: &str = "41c60b9e-4322-4ecb-88ed-0e391760c789";

fn fixture(folder: &Dir, (name, size, expected): (&str, u64, &str)) -> Result<Vec<u8>, String> {
    let bytes = image_chain_checks::bounded(folder, name, size)?;
    if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err(format!("Boot fixture differs: {name}"));
    }
    Ok(bytes)
}

fn witness() -> Result<serde_json::Value, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let source = selection(B, None)?;
    let resolved = data_windows::resolve(&source, true, false)?;
    if resolved.disk.is_offline != Some(true) || resolved.size != 16 * GIB {
        return Err("Expected the original 16 GiB offline Test B".into());
    }
    Ok(
        serde_json::json!({"desktop": desktop, "layouts": ntfs_reconstruction_checks::table_hashes()?}),
    )
}

fn new_file(folder: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = folder
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

pub(super) fn prepare() -> Result<String, String> {
    stage(FOLDER, &FILES)
}

pub(super) fn prepare_references() -> Result<String, String> {
    stage(REFERENCE_FOLDER, &REFERENCE_FILES)
}

pub(super) fn prepare_inheritance() -> Result<String, String> {
    stage(INHERITANCE_FOLDER, &[INHERITANCE_IMAGE])
}

fn stage(folder_name: &str, files: &[(&str, u64, &str)]) -> Result<String, String> {
    let baseline = witness()?;
    let cd = image_chain_checks::optical()?;
    let sources = files
        .iter()
        .copied()
        .map(|file| fixture(&cd, file))
        .collect::<Result<Vec<_>, _>>()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(folder_name).map_err(err)?;
    let folder = root.open_dir(folder_name).map_err(err)?;
    for (file, bytes) in files.iter().copied().zip(sources) {
        new_file(&folder, file.0, &bytes)?;
        fixture(&folder, file)?;
    }
    new_file(
        &folder,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!(
        "PASS: {} boot inspection fixtures verified in {}. Test B remains offline.",
        files.len(),
        path.join(folder_name).display()
    ))
}

fn staged_folder(folder_name: &str) -> Result<Dir, String> {
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(folder_name).map_err(err)?;
    let baseline: serde_json::Value = serde_json::from_slice(&image_chain_checks::bounded(
        &folder,
        "baseline.json",
        4096,
    )?)
    .map_err(err)?;
    // The read-only audit may follow an updated desktop build, but its
    // embedded payload hash and the original fixture layouts must still match.
    let current = witness()?;
    if baseline["layouts"] != current["layouts"] {
        return Err("Boot inspection partition layout changed".into());
    }
    Ok(folder)
}

pub(super) fn verify() -> Result<String, String> {
    let folder = staged_folder(FOLDER)?;
    for file in FILES {
        fixture(&folder, file)?;
    }
    Ok("PASS: all three boot fixture hashes, both test GPTs and guest OS layout match. Test B remains offline.".into())
}

pub(super) fn verify_references() -> Result<String, String> {
    let folder = staged_folder(REFERENCE_FOLDER)?;
    let expected = [
        (
            0,
            42 * MIB,
            "2829a8b2f97f018cb148f0235aedce5b49303155f01c2d6118c4748ce452162f",
            "missingPartition",
        ),
        (
            4096,
            10 * MIB,
            "c80278b66a43005cf837f997a46754d7d47a947798aa6e23c827ac12aa14ab1e",
            "outsideSource",
        ),
    ];
    for (file, (header, partition, loader, unbound)) in REFERENCE_FILES.into_iter().zip(expected) {
        let bytes = fixture(&folder, file)?;
        let disk = &bytes[header..];
        let actual = boot_inspection::inspect(
            std::io::Cursor::new(disk),
            file.0.into(),
            disk.len() as u64,
            512,
            false,
            None,
        )?;
        let report = serde_json::to_value(actual).map_err(err)?;
        let checked = report["volumes"][0]["references"]
            .as_array()
            .ok_or("Missing boot reference results")?;
        if checked.len() != 6
            || checked[0]["application"]["state"] != "relative"
            || checked[1]["application"]["state"] != "matched"
            || checked[1]["application"]["offset"] != partition
            || checked[1]["system"]["state"] != "matched"
            || checked[1]["loader"]["state"] != "present"
            || checked[1]["loader"]["machine"] != 0x8664
            || checked[1]["loader"]["sha256"] != loader
            || checked[1]["systemRoot"]["state"] != "present"
            || checked[2]["application"]["state"] != unbound
            || !checked[2]["loader"].is_null()
            || checked[3]["loader"]["state"] != "missing"
            || checked[3]["systemRoot"]["state"] != "missing"
            || checked[4]["application"]["state"] != "unresolved"
            || !checked[4]["loader"].is_null()
            || checked[5]["loader"]["state"] != "invalid"
        {
            return Err(format!(
                "Boot reference result differs: {} {checked:?}",
                file.0
            ));
        }
    }
    Ok("PASS: GPT and MBR BCD results and loader hashes, both fixtures, both test GPTs and guest OS layout match. B remains offline.".into())
}

fn check_inheritance_fixture(bytes: &[u8]) -> Result<serde_json::Value, String> {
    if bytes.len() as u64 != INHERITANCE_IMAGE.1
        || format!("{:x}", Sha256::digest(bytes)) != INHERITANCE_IMAGE.2
    {
        return Err("Unexpected BCD inheritance fixture".into());
    }
    let result = boot_inspection::inspect(
        std::io::Cursor::new(bytes),
        INHERITANCE_IMAGE.0.into(),
        INHERITANCE_IMAGE.1,
        512,
        false,
        None,
    )?;
    let report = serde_json::to_value(result).map_err(err)?;
    let volume = &report["volumes"][0];
    let store = &volume["store"];
    let entries = &store["entries"];
    let loader = &entries[0];
    let references = &volume["references"];
    let inherited = loader["inheritedSettings"]
        .as_array()
        .ok_or("Missing setting sources")?;
    let source = |field: &str, object: &str| {
        inherited
            .iter()
            .any(|setting| setting["field"] == field && setting["object"] == object)
    };
    if store["timeoutSeconds"] != "23"
        || store["displayOrder"][0] != loader["id"]
        || store["default"] != loader["id"]
        || store["bootSequence"][0] != loader["id"]
        || store["toolsOrder"][0] != entries[4]["id"]
        || loader["recoverySequence"][0] != entries[4]["id"]
        || loader["recoveryEnabled"] != true
        || loader["locale"] != "en-US"
        || !source("applicationPath", "eaf9ef92-c25a-48f2-bc9a-13c6debf0008")
        || !source("systemDevice", "eaf9ef92-c25a-48f2-bc9a-13c6debf0006")
        || references[1]["application"]["offset"] != 42 * MIB
        || references[1]["loader"]["state"] != "present"
        || references[1]["loader"]["machine"] != 0x8664
        || references[1]["loader"]["sha256"] != WINLOAD
        || references[1]["systemRoot"]["state"] != "present"
        || entries[1]["settingIssues"][0]["problem"] != "conflict"
        || !references[2]["loader"].is_null()
        || entries[2]["inheritanceIssue"]["problem"] != "cycle"
        || references[3]["loader"]["sha256"] != WINLOAD
        || entries[3]["inheritanceIssue"]["problem"] != "missingObject"
        || !references[4]["loader"].is_null()
    {
        return Err(format!("Unexpected BCD or NTFS result: {volume:#}"));
    }
    Ok(report)
}

pub(super) fn verify_inheritance() -> Result<String, String> {
    let folder = staged_folder(INHERITANCE_FOLDER)?;
    check_inheritance_fixture(&fixture(&folder, INHERITANCE_IMAGE)?)?;
    Ok("PASS: inherited BCD values, unresolved branches, NTFS loader SHA-256, image, A/B GPTs and guest OS layout match. B remains offline.".into())
}

fn physical_repair_report() -> Result<serde_json::Value, String> {
    let source = selection(B, None)?;
    let report = data_tasks::boot_inspection_tasks::inspect(
        data_tasks::boot_inspection_tasks::Request::Disk { source },
    )?;
    let json = serde_json::to_value(report).map_err(err)?;
    if json["volumes"][0]["repairVolume"] != "6b736944-7453-6475-696f-426f6f743031"
        || json["repairPartitions"][0]["id"] != "6b736944-7453-6475-696f-446174613031"
        || json["volumes"][0]["filesystem"] != "FAT32"
        || json["volumes"][0]["size"] != 272 * MIB
        || json["repairPartitions"][0]["offset"] != 274 * MIB
        || json["repairPartitions"][0]["size"] != 24 * MIB
    {
        return Err("Expected the exact dedicated offline boot repair partition extents".into());
    }
    Ok(json)
}

fn repair_digests(report: &serde_json::Value) -> Result<serde_json::Value, String> {
    let artifacts = report["volumes"][0]["artifacts"]
        .as_array()
        .ok_or("Missing boot artifacts")?;
    let digest = |path: &str| {
        artifacts
            .iter()
            .find(|file| file["path"] == path && file["state"] == "present")
            .and_then(|file| file["sha256"].as_str())
            .ok_or("Missing boot artifact digest")
    };
    Ok(serde_json::json!({
        "bcd": digest(r"\EFI\Microsoft\Boot\BCD")?,
        "manager": digest(r"\EFI\Microsoft\Boot\bootmgfw.efi")?,
    }))
}

fn repair_reference(volume: &serde_json::Value) -> Result<&serde_json::Value, String> {
    volume["references"]
        .as_array()
        .and_then(|references| references.iter().find(|entry| entry["id"] == REPAIR_ENTRY))
        .ok_or_else(|| "Expected the fixture's Windows loader object".into())
}

pub(super) fn prepare_repair() -> Result<String, String> {
    let mut baseline = witness()?;
    let report = physical_repair_report()?;
    let volume = &report["volumes"][0];
    let reference = repair_reference(volume)?;
    if reference["application"]["state"] != "missingPartition"
        || reference["system"]["state"] != "missingPartition"
        || volume["store"]["default"] == REPAIR_ENTRY
    {
        return Err("Expected both stale BCD devices and the old default".into());
    }
    baseline["repair"] = repair_digests(&report)?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(REPAIR_FOLDER).map_err(err)?;
    let folder = root.open_dir(REPAIR_FOLDER).map_err(err)?;
    new_file(
        &folder,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!(
        "PASS: stale BCD confirmed on offline Test B. Save the repair backup to {}.",
        path.join(REPAIR_FOLDER)
            .join("BCD-before-repair.bcd")
            .display()
    ))
}

pub(super) fn verify_repair() -> Result<String, String> {
    let folder = staged_folder(REPAIR_FOLDER)?;
    let baseline: serde_json::Value = serde_json::from_slice(&image_chain_checks::bounded(
        &folder,
        "baseline.json",
        4096,
    )?)
    .map_err(err)?;
    let backup = image_chain_checks::bounded(
        &folder,
        "BCD-before-repair.bcd",
        boot_configuration::MAX_STORE as u64,
    )?;
    let backup_sha256 = format!("{:x}", Sha256::digest(backup));
    let report = physical_repair_report()?;
    let after = repair_digests(&report)?;
    let volume = &report["volumes"][0];
    let reference = repair_reference(volume)?;
    if baseline["repair"]["bcd"] != backup_sha256
        || after["bcd"] == backup_sha256
        || after["manager"] != baseline["repair"]["manager"]
        || volume["store"]["default"] != REPAIR_ENTRY
        || reference["application"]["state"] != "matched"
        || reference["application"]["offset"] != 274 * MIB
        || reference["system"]["state"] != "matched"
        || reference["system"]["offset"] != 274 * MIB
        || reference["loader"]["sha256"] != WINLOAD
        || reference["systemRoot"]["state"] != "present"
    {
        return Err("BCD repair readback, loader, or backup digest was unexpected".into());
    }
    Ok(format!("PASS: BCD {} verified, original backup {backup_sha256}, unchanged boot manager and A/B and OS layouts. Test B remains offline.", after["bcd"]))
}

fn efi_artifact<'a>(
    report: &'a serde_json::Value,
    path: &str,
    state: &str,
) -> Result<&'a serde_json::Value, String> {
    report["volumes"][0]["artifacts"]
        .as_array()
        .and_then(|artifacts| {
            artifacts
                .iter()
                .find(|file| file["path"] == path && file["state"] == state)
        })
        .ok_or_else(|| format!("Unexpected EFI artifact state: {path}"))
}

pub(super) fn prepare_files() -> Result<String, String> {
    let mut baseline = witness()?;
    let report = physical_repair_report()?;
    let manager = efi_artifact(&report, r"\EFI\Microsoft\Boot\bootmgfw.efi", "invalid")?;
    if manager["sha256"] != format!("{:x}", Sha256::digest([0xbd; 719])) {
        return Err("The original corrupt EFI fixture differs".into());
    }
    efi_artifact(&report, r"\EFI\Boot\bootx64.efi", "missing")?;
    let bcd = efi_artifact(&report, r"\EFI\Microsoft\Boot\BCD", "present")?;
    baseline["files"] = serde_json::json!({"bcd": bcd["sha256"], "manager": manager["sha256"]});
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(FILES_FOLDER).map_err(err)?;
    let directory = root.open_dir(FILES_FOLDER).map_err(err)?;
    new_file(
        &directory,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!(
        "PASS: corrupt manager and missing x64 fallback confirmed on offline B. Use {}.",
        path.join(FILES_FOLDER).join(FILES_BACKUP).display()
    ))
}

pub(super) fn verify_files() -> Result<String, String> {
    let folder = staged_folder(FILES_FOLDER)?;
    let baseline: serde_json::Value = serde_json::from_slice(&image_chain_checks::bounded(
        &folder,
        "baseline.json",
        4096,
    )?)
    .map_err(err)?;
    let backup = image_chain_checks::bounded(&folder, FILES_BACKUP, 64 * 1024)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(backup)).map_err(err)?;
    if zip.len() != 2 {
        return Err("Expected one original EFI file and its ZIP manifest".into());
    }
    let mut original = Vec::new();
    zip.by_name("EFI/Microsoft/Boot/bootmgfw.efi")
        .map_err(err)?
        .take(720)
        .read_to_end(&mut original)
        .map_err(err)?;
    if original != [0xbd; 719]
        || baseline["files"]["manager"] != format!("{:x}", Sha256::digest(original))
    {
        return Err("The verified ZIP does not contain the original manager bytes".into());
    }
    let manifest: serde_json::Value =
        serde_json::from_reader(zip.by_name("manifest.json").map_err(err)?.take(4096))
            .map_err(err)?;
    let report = physical_repair_report()?;
    let manager = efi_artifact(&report, r"\EFI\Microsoft\Boot\bootmgfw.efi", "present")?;
    let fallback = efi_artifact(&report, r"\EFI\Boot\bootx64.efi", "present")?;
    let bcd = efi_artifact(&report, r"\EFI\Microsoft\Boot\BCD", "present")?;
    if manager["sha256"] != RESTORED_MANAGER
        || fallback["sha256"] != RESTORED_MANAGER
        || manager["machine"] != 0x8664
        || fallback["machine"] != 0x8664
        || bcd["sha256"] != baseline["files"]["bcd"]
        || manifest["version"] != 1
        || manifest["files"][0]["originalSha256"] != baseline["files"]["manager"]
        || manifest["files"][1]["originalSha256"] != serde_json::Value::Null
    {
        return Err("EFI file repair readback or ZIP manifest did not match".into());
    }
    Ok("PASS: original manager ZIP, official x64 manager and fallback, existing BCD, test GPTs and guest OS layout verified. Test B remains offline.".into())
}

fn store_files() -> Result<[Option<Vec<u8>>; 4], String> {
    let source = selection(B, None)?;
    let before = data_windows::resolve(&source, true, false)?;
    let locks = data_windows::state_locks(&before)?;
    let current = data_windows::resolve_locked(&source, false, &before, &locks)?;
    if current.disk.is_offline != Some(true)
        || current.size != 16 * GIB
        || current.disk.logical_sector_size != Some(4096)
    {
        return Err("Expected offline 4Kn Test B".into());
    }
    let device = data_windows::device(&current, false)?;
    data_windows::verify_offline_handle(&current, &device)?;
    let mut view = data_io::Region::for_file_view(device, MIB, 272 * MIB).map_err(err)?;
    let volume = fat_recovery::boot_files::Volume::open(&mut view)?;
    let mut files = [None, None, None, None];
    for (file, name) in files.iter_mut().zip(
        ["BCD"]
            .into_iter()
            .chain(STORE_LOGS.map(|(name, _, _)| name)),
    ) {
        *file = volume.read_file(&mut view, &["EFI", "Microsoft", "Boot", name], 64 * 1024)?;
    }
    data_windows::resolve_locked(&source, false, &current, &locks)?;
    Ok(files)
}

pub(super) fn prepare_store() -> Result<String, String> {
    let baseline = witness()?;
    let report = physical_repair_report()?;
    efi_artifact(&report, r"\EFI\Microsoft\Boot\BCD", "missing")?;
    let manager = efi_artifact(&report, r"\EFI\Microsoft\Boot\bootmgfw.efi", "present")?;
    if manager["sha256"] != RESTORED_MANAGER {
        return Err("Expected the original x64 Windows boot manager".into());
    }
    let [bcd, first, second, third] = store_files()?;
    if bcd.is_some()
        || [first, second, third]
            .into_iter()
            .zip(STORE_LOGS)
            .any(|(file, (_, byte, length))| file.as_deref() != Some(&vec![byte; length]))
    {
        return Err("The missing BCD and stale log fixture differs".into());
    }
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(STORE_FOLDER).map_err(err)?;
    let folder = root.open_dir(STORE_FOLDER).map_err(err)?;
    new_file(
        &folder,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!(
        "PASS: BCD is missing and three old logs are present on offline B. Save the backup to {}.",
        path.join(STORE_FOLDER).join(FILES_BACKUP).display()
    ))
}

pub(super) fn verify_store() -> Result<String, String> {
    let folder = staged_folder(STORE_FOLDER)?;
    let report = physical_repair_report()?;
    let manager = efi_artifact(&report, r"\EFI\Microsoft\Boot\bootmgfw.efi", "present")?;
    let volume = &report["volumes"][0];
    let reference = volume["references"]
        .as_array()
        .and_then(|refs| refs.iter().find(|entry| entry["id"] == STORE_LOADER))
        .ok_or("Expected the installed UEFI loader")?;
    if manager["sha256"] != RESTORED_MANAGER
        || volume["store"]["default"] != STORE_LOADER
        || volume["store"]["displayOrder"] != serde_json::json!([STORE_LOADER])
        || reference["application"]["state"] != "matched"
        || reference["system"]["state"] != "matched"
        || reference["application"]["offset"] != 274 * MIB
        || reference["system"]["offset"] != 274 * MIB
        || reference["loader"]["sha256"] != WINLOAD
        || reference["systemRoot"]["state"] != "present"
    {
        return Err("Reconstructed BCD references do not match the test disk".into());
    }
    let [Some(store), None, None, None] = store_files()? else {
        return Err("The new BCD is missing or a stale log remains".into());
    };
    let sha = format!("{:x}", Sha256::digest(&store));
    let backup = image_chain_checks::bounded(&folder, FILES_BACKUP, 64 * 1024)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(backup)).map_err(err)?;
    if archive.len() != 4 {
        return Err("The backup should contain the manifest and three old logs".into());
    }
    let manifest: serde_json::Value = serde_json::from_reader(
        archive
            .by_name("manifest.json")
            .map_err(err)?
            .take(16 * 1024),
    )
    .map_err(err)?;
    if manifest["newBcd"]["loader"] != STORE_LOADER
        || manifest["newBcd"]["templateSha256"] != TEMPLATE_SHA
        || manifest["files"][0]["originalSha256"] != serde_json::Value::Null
        || manifest["files"][0]["installedSha256"] != sha
    {
        return Err("BCD backup manifest does not describe the reconstructed store".into());
    }
    for (index, (name, byte, length)) in STORE_LOGS.into_iter().enumerate() {
        let mut log = Vec::new();
        archive
            .by_name(&format!("EFI/Microsoft/Boot/{name}"))
            .map_err(err)?
            .take(length as u64 + 1)
            .read_to_end(&mut log)
            .map_err(err)?;
        if log != vec![byte; length]
            || manifest["newBcd"]["retiredLogs"][index]["sha256"]
                != format!("{:x}", Sha256::digest(&log))
        {
            return Err(format!("Unexpected bytes for the original {name}"));
        }
    }
    let listing = bcdedit_copy(STORE_FOLDER, &folder, &store)?;
    if listing.matches(STORE_LOADER).count() < 3
        || !listing.contains(r"\windows\system32\winload.efi")
        || !listing.contains(r"\efi\microsoft\boot\bootmgfw.efi")
    {
        return Err(format!(
            "BCDEdit did not enumerate the expected Windows entry: {listing}"
        ));
    }
    Ok(format!(
        "PASS: BCDEdit read the new Windows boot entry. Verified {sha}, the three archived logs, both GPT layouts and the guest OS layout. Test B remains offline."
    ))
}

fn bcdedit_copy(folder_name: &str, folder: &Dir, store: &[u8]) -> Result<String, String> {
    // Enumerate a new file on Test A. The command uses an explicit /store.
    let (_, root) = ntfs_reconstruction_checks::verified_a_root()?;
    let verification_copy = "Bcd-Readback.bcd";
    new_file(folder, verification_copy, store)?;
    let path = root.join(folder_name).join(verification_copy);
    let windows = std::env::var_os("SystemRoot").ok_or("Guest Windows directory is unavailable")?;
    let output =
        std::process::Command::new(std::path::PathBuf::from(windows).join("System32/bcdedit.exe"))
            .arg("/store")
            .arg(path)
            .args(["/enum", "all", "/v"])
            .output()
            .map_err(err)?;
    let listing = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    new_file(folder, "BcdEdit-Readback.txt", &output.stdout)?;
    if !output.status.success() {
        return Err(format!(
            "BCDEdit did not enumerate the reconstructed store: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(listing)
}

pub(super) fn prepare_damaged(replay: bool) -> Result<String, String> {
    let mut baseline = witness()?;
    let report = physical_repair_report()?;
    efi_artifact(&report, r"\EFI\Microsoft\Boot\BCD", "invalid")?;
    let [Some(bcd), None, Some(log1), Some(log2)] = store_files()? else {
        return Err("Expected a damaged BCD and both retained logs".into());
    };
    if replay {
        let header = regf::BaseBlock::parse(&bcd).map_err(err)?;
        if !header.checksum_valid
            || header.secondary_sequence != 42
            || header.primary_sequence != 43
        {
            return Err("The dirty BCD fixture header differs".into());
        }
    } else if bcd != vec![0xbe; 591] {
        return Err("The corrupt BCD fixture differs".into());
    }
    for (log, sequence) in [(&log1, 43), (&log2, 42)] {
        let log = regf::transaction_log::NewFormatLog::parse(Cursor::new(log)).map_err(err)?;
        if log.log_entries.len() != 1 || log.log_entries[0].sequence_number != sequence {
            return Err("The fixture must contain the two rotated BCD log records".into());
        }
    }
    let names = ["BCD", "BCD.LOG1", "BCD.LOG2"];
    let digests = [bcd, log1, log2].map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    baseline["files"] = serde_json::to_value(
        names
            .into_iter()
            .zip(digests)
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
    .map_err(err)?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder_name = if replay { DIRTY_FOLDER } else { DAMAGED_FOLDER };
    root.create_dir(folder_name).map_err(err)?;
    let folder = root.open_dir(folder_name).map_err(err)?;
    new_file(
        &folder,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!(
        "PASS: unreadable BCD and both rotated logs confirmed on offline B. Save the backup to {}.",
        path.join(folder_name).join(FILES_BACKUP).display()
    ))
}

pub(super) fn verify_damaged(replay: bool) -> Result<String, String> {
    let name = if replay { DIRTY_FOLDER } else { DAMAGED_FOLDER };
    let folder = staged_folder(name)?;
    let baseline: serde_json::Value = serde_json::from_slice(&image_chain_checks::bounded(
        &folder,
        "baseline.json",
        4096,
    )?)
    .map_err(err)?;
    let report = physical_repair_report()?;
    let manager = efi_artifact(&report, r"\EFI\Microsoft\Boot\bootmgfw.efi", "present")?;
    let expected = if replay { REPLAY_DEFAULT } else { STORE_LOADER };
    let volume = &report["volumes"][0];
    let reference = volume["references"]
        .as_array()
        .and_then(|refs| refs.iter().find(|entry| entry["id"] == expected))
        .ok_or("Expected a valid default Windows loader")?;
    if manager["sha256"] != RESTORED_MANAGER
        || volume["store"]["default"] != expected
        || reference["application"]["state"] != "matched"
        || reference["system"]["state"] != "matched"
        || reference["loader"]["sha256"] != WINLOAD
    {
        return Err("The default BCD entry or Windows files do not match the fixture".into());
    }
    let [Some(store), None, None, None] = store_files()? else {
        return Err("The repaired BCD is missing or old logs remain".into());
    };
    let sha = format!("{:x}", Sha256::digest(&store));
    let mut zip = zip::ZipArchive::new(Cursor::new(image_chain_checks::bounded(
        &folder,
        FILES_BACKUP,
        128 * 1024,
    )?))
    .map_err(err)?;
    if zip.len() != 4 {
        return Err("Expected the backup manifest, original BCD and two logs".into());
    }
    for file in ["BCD", "BCD.LOG1", "BCD.LOG2"] {
        let mut data = Vec::new();
        zip.by_name(&format!("EFI/Microsoft/Boot/{file}"))
            .map_err(err)?
            .take(64 * 1024)
            .read_to_end(&mut data)
            .map_err(err)?;
        if baseline["files"][file] != format!("{:x}", Sha256::digest(&data)) {
            return Err(format!("The original {file} backup digest differs"));
        }
    }
    let manifest: serde_json::Value =
        serde_json::from_reader(zip.by_name("manifest.json").map_err(err)?.take(16 * 1024))
            .map_err(err)?;
    if manifest["files"][0]["originalSha256"] != baseline["files"]["BCD"]
        || manifest["files"][0]["installedSha256"] != sha
    {
        return Err("The backup manifest does not match the original and repaired BCD".into());
    }
    let listing = bcdedit_copy(name, &folder, &store)?;
    let valid = listing.matches(expected).count() >= 3
        && if replay {
            listing.contains(REPLAY_OTHER)
                && listing.contains("windows main-new")
                && listing.contains("windows backup-new")
                && volume["store"]["displayOrder"]
                    == serde_json::json!([REPLAY_DEFAULT, REPLAY_OTHER])
                && manifest["recoveredBcd"]["transactions"] == 2
                && manifest["recoveredBcd"]["logs"] == serde_json::json!(["BCD.LOG2", "BCD.LOG1"])
        } else {
            volume["store"]["displayOrder"] == serde_json::json!([STORE_LOADER])
                && manifest["newBcd"]["templateSha256"] == TEMPLATE_SHA
        };
    if !valid {
        return Err(format!(
            "BCDEdit did not confirm the repaired boot options: {listing}"
        ));
    }
    let action = if replay {
        "both recovered Windows entries and both logged changes"
    } else {
        "the replacement Windows entry"
    };
    Ok(format!(
        "PASS: BCDEdit read {action}. Verified {sha}, the original BCD and two archived logs, A/B GPT and guest OS layout. B remains offline."
    ))
}

#[cfg(test)]
#[test]
#[ignore = "requires the generated ordinary Boot-Inheritance-NTFS image"]
fn synthetic_boot_inheritance_matches_the_ntfs_loader() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.vm/payload/Boot-Inheritance-NTFS.img");
    assert!(path.is_file());
    let report = check_inheritance_fixture(&std::fs::read(path).unwrap()).unwrap();
    println!("BCD_REPORT={report}");
}

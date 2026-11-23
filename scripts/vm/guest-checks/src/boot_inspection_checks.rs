//! Stage fixed boot-inspection files on the verified guest A data volume.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::Write;

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

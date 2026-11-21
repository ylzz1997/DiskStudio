//! Reuse the accepted offline 4Kn NTFS WOF clone as the recovery source.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;

// Original range recorded by the WOF image clone fixture. Disk numbers and
// partition numbers can change when another test removes an earlier entry.
const SOURCE_OFFSET: u64 = 13_090_422_784;
const SOURCE_SIZE: u64 = 48 * disk_core::MIB;
const FILE_SIZE: u64 = 192 * 1024 + 123;
const FOLDER: &str = "DS-WofRecovery";
const PROGRESS: &str = "Wof.dsrecover";

#[derive(serde::Serialize, Deserialize, PartialEq)]
struct Witness {
    a: String,
    b: String,
    os: String,
    source_sha256: String,
}

fn source() -> Result<(Selection, data_windows::Resolved), String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let part = reallocation_checks::metadata_partition(
        B,
        None,
        SOURCE_OFFSET / disk_core::MIB,
        SOURCE_SIZE / disk_core::MIB,
    )?;
    let selection = selection(B, Some(part.number))?;
    let bound = data_windows::resolve(&selection, true, false)?;
    if bound
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(B)
        || bound.disk.is_offline != Some(true)
        || bound.disk.partition_style != Some(2)
        || bound.offset != SOURCE_OFFSET
        || bound.size != SOURCE_SIZE
    {
        return Err("Expected the previously verified 48 MiB WOF clone on offline B".into());
    }
    Ok((selection, bound))
}

fn witness(source: &Selection, bound: &data_windows::Resolved) -> Result<Witness, String> {
    let mut reader = data_windows::reader(bound)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    for _ in 0..(SOURCE_SIZE / buffer.len() as u64) {
        reader.read_exact(&mut buffer).map_err(err)?;
        hash.update(&buffer);
    }
    Ok(Witness {
        a: selection(A, None)?.layout_key,
        b: source.layout_key.clone(),
        os: reallocation_checks::os_layout(&inventory()?)?,
        source_sha256: format!("{:x}", hash.finalize()),
    })
}

pub(super) fn baseline() -> Result<String, String> {
    let _desktop = btrfs_checks::desktop()?;
    let (source, bound) = source()?;
    let (root, path) = ntfs_file_clone_checks::four_kn::witness_volume()?;
    let mut reader = data_windows::reader(&bound)?;
    let mut found = Vec::new();
    recovery::scan_ntfs(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| found.push(file),
    )?;
    for algorithm in 0..4 {
        let name = format!("wof-{algorithm}.bin");
        if found
            .iter()
            .filter(|c| c.name == name && c.supported && c.size == FILE_SIZE)
            .count()
            != 1
        {
            return Err(format!("Known WOF candidate is missing: {name}"));
        }
    }
    let witness = witness(&source, &bound)?;
    root.create_dir(FOLDER).map_err(err)?;
    root.open_dir(FOLDER)
        .map_err(err)?
        .write("baseline.json", serde_json::to_vec(&witness).map_err(err)?)
        .map_err(err)?;
    Ok(format!("PASS: Offline B/{}, 48 MiB. Scan existing files. Save {PROGRESS} and export four WOF files (flat) to {}.", source.partition.unwrap(), path.join(FOLDER).display()))
}

pub(super) fn verify() -> Result<String, String> {
    let _desktop = btrfs_checks::desktop()?;
    let (source, bound) = source()?;
    let (root, _) = ntfs_file_clone_checks::four_kn::witness_volume()?;
    let folder = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Witness =
        serde_json::from_reader(folder.open("baseline.json").map_err(err)?.take(4096))
            .map_err(err)?;
    if witness(&source, &bound)? != baseline {
        return Err("Source bytes, A/B layouts or guest OS layout changed".into());
    }
    let mut saved = Vec::new();
    folder
        .open(PROGRESS)
        .map_err(err)?
        .take(1024 * 1024)
        .read_to_end(&mut saved)
        .map_err(err)?;
    let progress = recovery_checkpoint::decode(&saved, &source)?;
    let (files, _) = progress.validate(&source)?;
    let mut reports = Vec::new();
    for entry in folder.entries().map_err(err)? {
        let entry = entry.map_err(err)?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("DiskStudio-recovered-")
        {
            reports.push(entry.open_dir().map_err(err)?);
        }
    }
    let [output] = reports.as_slice() else {
        return Err("Expected one native export".into());
    };
    let report: serde_json::Value = serde_json::from_reader(
        output
            .open(recovery_export::REPORT)
            .map_err(err)?
            .take(128 * 1024),
    )
    .map_err(err)?;
    let entries = report["entries"]
        .as_array()
        .ok_or("Export entries unavailable")?;
    let outcomes = report["outcomes"]
        .as_array()
        .ok_or("Export outcomes unavailable")?;
    if report["completed"] != true
        || report["savedFiles"] != 4
        || report["failedFiles"] != 0
        || report["layout"] != "flat"
        || entries.len() != 4
        || outcomes.len() != 4
    {
        return Err("Unexpected native export result".into());
    }
    for algorithm in 0..4 {
        let name = format!("wof-{algorithm}.bin");
        let candidate = files
            .iter()
            .find(|file| file.name == name && file.supported && file.size == FILE_SIZE)
            .ok_or("Saved WOF candidate unavailable")?;
        if !matches!(
            candidate.locator,
            recovery::Locator::Ntfs { wof: Some(_), .. }
        ) {
            return Err("Saved WOF proof is missing".into());
        }
        let index = entries
            .iter()
            .position(|entry| entry["originalName"] == name)
            .ok_or("Exported WOF candidate missing from the manifest")?;
        let leaf = format!("{:06}_{name}", candidate.id);
        let expected: Vec<_> = (0..FILE_SIZE)
            .map(|i| ((i / 113 + i % 41 + algorithm * 17) % 251) as u8)
            .collect();
        let mut actual = Vec::new();
        output
            .open(&leaf)
            .map_err(err)?
            .take(FILE_SIZE + 1)
            .read_to_end(&mut actual)
            .map_err(err)?;
        if entries[index]["name"] != leaf
            || actual != expected
            || outcomes[index]["sha256"] != format!("{:x}", Sha256::digest(expected))
        {
            return Err(format!("Native export or SHA-256 differs for {name}"));
        }
    }
    Ok(format!("PASS: Four WOF exports ({} bytes) and progress match. B remains offline. Source bytes and all layouts are unchanged.", FILE_SIZE * 4))
}

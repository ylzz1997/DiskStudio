//! Verify lost partition exports from ordinary files on the isolated test pair.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};

const ROOT: &str = "DS-Retained-Lost";
const IMAGE: &str = "missing-table.img";
const BASE: &str = "base.dsbackup";
const LATEST: &str = "latest.dsbackup";
const VOLUME: &[u8] = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");
const FILE: &str = "file-with-12345";
const EXPORTED: &str = "000001_file-with-12345";
const SHA_INPUTS: [&str; 3] = [IMAGE, BASE, LATEST];
const SECTOR: u64 = 512;
const IMAGE_OFFSET: u64 = MIB + SECTOR;
const BASE_OFFSET: u64 = 2 * MIB + SECTOR;

#[derive(Deserialize, serde::Serialize)]
struct Baseline {
    desktop: String,
    layouts: [String; 3],
    sources: Vec<String>,
    latest: String,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn files(dir: &Dir) -> Result<Vec<String>, String> {
    SHA_INPUTS
        .iter()
        .map(|name| dir.read(name).map(|bytes| hash(&bytes)).map_err(err))
        .collect()
}

fn bytes(offset: u64) -> Vec<u8> {
    let start = offset as usize;
    let mut disk = vec![0u8; (6 * MIB) as usize];
    disk[start..start + VOLUME.len()].copy_from_slice(VOLUME);
    disk[start..start + SECTOR as usize].fill(0);
    disk[start + 16384..start + 18432].fill(0);
    disk
}

fn new_file(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

fn archive(
    dir: &Dir,
    name: &str,
    bytes: &[u8],
    parent: Option<&backup_archive::Chain>,
) -> Result<String, String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?
        .into_std();
    let header = backup_archive::create(
        &mut Cursor::new(bytes),
        &mut file,
        "disk",
        bytes.len() as u64,
        SECTOR,
        true,
        parent,
        |_| Ok(()),
    )?;
    Ok(header.id)
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (input, input_path) = ext_reconstruct_checks::b_root()?;
    let (output, output_path) = ntfs_reconstruction_checks::verified_a_root()?;
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    input.create_dir(ROOT).map_err(err)?;
    output.create_dir(ROOT).map_err(err)?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    output.create_dir("image").map_err(err)?;
    output.create_dir("backup").map_err(err)?;
    let raw = bytes(IMAGE_OFFSET);
    archive(&input, BASE, &bytes(BASE_OFFSET), None)?;
    let base = input.open(BASE).map_err(err)?.into_std();
    let chain = backup_archive::Chain::new(backup_archive::Archive::from_reader(base)?);
    let latest_id = archive(&input, LATEST, &raw, Some(&chain))?;
    new_file(&input, IMAGE, &raw)?;
    let baseline = Baseline {
        desktop,
        layouts,
        sources: files(&input)?,
        latest: latest_id,
    };
    new_file(
        &input,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    if ntfs_reconstruction_checks::table_hashes()? != baseline.layouts {
        return Err("Guest layouts changed while preparing ordinary files".into());
    }
    Ok(format!(
        "PASS: sources {}. Export {FILE} into the corresponding image and backup folders under {}. Expected LBA {} for the image and latest point, {} for the base.",
        input_path.join(ROOT).display(), output_path.join(ROOT).display(),
        IMAGE_OFFSET / SECTOR, BASE_OFFSET / SECTOR,
    ))
}

fn report(output: &Dir) -> Result<serde_json::Value, String> {
    let names = output
        .entries()
        .map_err(err)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [name] = names.as_slice() else {
        return Err("Expected exactly one export folder".into());
    };
    if !name.to_string_lossy().starts_with("DiskStudio-recovered-") {
        return Err("Unexpected export folder".into());
    }
    let dir = output.open_dir(name).map_err(err)?;
    let manifest: serde_json::Value = serde_json::from_reader(
        dir.open(recovery_export::REPORT)
            .map_err(err)?
            .take(128 * 1024),
    )
    .map_err(err)?;
    if manifest["completed"] != true
        || manifest["savedFiles"] != 1
        || manifest["failedFiles"] != 0
        || manifest["entries"][0]["originalName"] != FILE
        || manifest["outcomes"][0]["sha256"] != hash(b"12345")
        || manifest["ntfsReconstruction"].is_null()
        || dir.read(EXPORTED).map_err(err)? != b"12345"
    {
        return Err("Export bytes or the reconstructed NTFS manifest differ".into());
    }
    Ok(manifest)
}

pub(super) fn verify() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (input, _) = ext_reconstruct_checks::b_root()?;
    let (output, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = input.open_dir(ROOT).map_err(err)?;
    let output = output.open_dir(ROOT).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(input.open("baseline.json").map_err(err)?.take(4096))
            .map_err(err)?;
    if baseline.desktop != desktop
        || baseline.sources != files(&input)?
        || baseline.layouts != ntfs_reconstruction_checks::table_hashes()?
    {
        return Err(
            "An input file, test disk table, guest layout, or desktop payload changed".into(),
        );
    }
    let image = report(&output.open_dir("image").map_err(err)?)?;
    let backup = report(&output.open_dir("backup").map_err(err)?)?;
    let extent =
        serde_json::json!({ "offset": IMAGE_OFFSET, "size": VOLUME.len(), "sector": SECTOR });
    if image["imageSource"]["fileName"] != IMAGE
        || image["imageSource"]["offset"] != IMAGE_OFFSET.to_string()
        || image["imageSource"]["size"] != VOLUME.len().to_string()
        || image["imageSource"]["sector"] != SECTOR
        || backup["backupSource"]["archiveName"] != LATEST
        || backup["backupSource"]["pointId"] != baseline.latest
        || backup["backupSource"]["foundExtent"] != extent
    {
        return Err("The export source, selected backup point, or extent differs".into());
    }
    Ok(format!(
        "PASS: both 5-byte exports match SHA-256 {}. Verified source names, extent, restore point, three input files, both test tables and guest OS layout.",
        hash(b"12345"),
    ))
}

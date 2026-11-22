//! Native Ext4 image reconstruction on the standard isolated 512-byte A/B pair.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::io::Read;

const FOLDER: &str = "DS-Ext-Reconstruction";
const IMAGE: &str = "damaged-ext4.img";
const SAMPLE: &[u8] = b"hello, world!";
const EXPORTED_SAMPLE: &str = "000001_small_file";
const FIXTURE: &[u8] = include_bytes!("../../../../tests/fixtures/test_disk1.bin.zst");

fn image() -> Result<Vec<u8>, String> {
    let mut bytes = zstd::stream::decode_all(FIXTURE).map_err(err)?;
    bytes[1080..1082].fill(0);
    Ok(bytes)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn b_root() -> Result<(Dir, PathBuf), String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|disk| disk.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("Test B identity is missing")?;
    for part in quick_partition_checks::parts()? {
        let Some(letter) = part
            .drive_letter
            .filter(|letter| letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic())
        else {
            continue;
        };
        if part.disk_number != b.number {
            continue;
        }
        let source = selection(B, Some(part.partition_number))?;
        let Ok(resolved) = data_windows::resolve(&source, true, false) else {
            continue;
        };
        let [volume] = resolved.volumes.as_slice() else {
            continue;
        };
        if resolved.disk.is_offline != Some(false)
            || resolved.disk.is_read_only != Some(false)
            || !resolved.ntfs_volume
        {
            continue;
        }
        let path = PathBuf::from(format!("{letter}:\\"));
        let dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(err)?;
        data_windows::file_location(&dir.try_clone().map_err(err)?.into_std_file())?
            .confirm_partition(
                &source.target,
                &source.layout_key,
                volume,
                (resolved.disk.number, resolved.offset, resolved.size),
            )?;
        return Ok((dir, path));
    }
    Err("Test B has no verified online NTFS output volume with a drive letter".into())
}

fn new_file(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

pub(super) fn prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (source, source_path) = ntfs_reconstruction_checks::verified_a_root()?;
    let (target, target_path) = b_root()?;
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    source.create_dir(FOLDER).map_err(err)?;
    target.create_dir(FOLDER).map_err(err)?;
    let source = source.open_dir(FOLDER).map_err(err)?;
    let image = image()?;
    new_file(&source, IMAGE, &image)?;
    new_file(
        &source,
        "layout.json",
        &serde_json::to_vec(&tables).map_err(err)?,
    )?;
    if ntfs_reconstruction_checks::table_hashes()? != tables {
        return Err("Guest disk layouts changed during fixture preparation".into());
    }
    Ok(format!(
        "PASS: source {} SHA-256 {}. Export small_file from the reconstructed view into {}.",
        source_path.join(FOLDER).join(IMAGE).display(),
        hash(&image),
        target_path.join(FOLDER).display(),
    ))
}

pub(super) fn verify() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (source, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let (target, _) = b_root()?;
    let source = source.open_dir(FOLDER).map_err(err)?;
    let target = target.open_dir(FOLDER).map_err(err)?;
    let layout: [String; 3] =
        serde_json::from_reader(source.open("layout.json").map_err(err)?.take(1024))
            .map_err(err)?;
    let expected = image()?;
    let mut observed = Vec::with_capacity(expected.len());
    source
        .open(IMAGE)
        .map_err(err)?
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut observed)
        .map_err(err)?;
    if observed != expected || ntfs_reconstruction_checks::table_hashes()? != layout {
        return Err("The source image or a guest disk layout changed".into());
    }
    let mut verified = 0;
    for entry in target.entries().map_err(err)? {
        let name = entry.map_err(err)?.file_name();
        if !name.to_string_lossy().starts_with("DiskStudio-recovered-") {
            continue;
        }
        let output = target.open_dir(name).map_err(err)?;
        let report: serde_json::Value = serde_json::from_reader(
            output
                .open(recovery_export::REPORT)
                .map_err(err)?
                .take(128 * 1024),
        )
        .map_err(err)?;
        let proof = &report["extReconstruction"];
        if report["completed"] != true
            || report["savedFiles"] != 1
            || report["failedFiles"] != 0
            || report["entries"][0]["originalName"] != "small_file"
            || report["entries"][0]["name"] != EXPORTED_SAMPLE
            || report["entries"][0]["folder"] != 0
            || report["outcomes"][0]["sha256"] != hash(SAMPLE)
            || proof["backupGroup"] != 1
            || proof["backupOffset"] != 8_389_632u64
            || proof["bytes"] != expected.len()
            || output.read(EXPORTED_SAMPLE).map_err(err)? != SAMPLE
        {
            return Err("The exported file or Ext4 reconstruction manifest differs".into());
        }
        verified += 1;
    }
    if verified != 1 {
        return Err("Expected exactly one verified reconstructed export".into());
    }
    Ok(format!(
        "PASS: cross-disk export matches all {} known bytes and SHA-256 {}. The reconstruction manifest, entire input image, both test tables and guest OS layout match.",
        SAMPLE.len(), hash(SAMPLE),
    ))
}

//! Native image acceptance with ordinary fixture files on the verified A/2 volume.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::io::Read;

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fixture;

const FOLDER: &str = "DS-FAT-Reconstruction";
const BASELINE: &str = "layout.json";
const FAT32: &str = "damaged-fat32.img";
const EXFAT: &str = "damaged-exfat.img";
const CONFLICT: &str = "conflicting-exfat.img";
const SECTOR: usize = 512;

fn images() -> [(&'static str, Vec<u8>); 3] {
    let mut fat32 = fixture::fat32(SECTOR);
    fat32[..SECTOR].fill(0);
    let mut exfat = fixture::exfat(SECTOR);
    let mut conflict = exfat.clone();
    exfat[..12 * SECTOR].fill(0);
    conflict[100] ^= 1;
    [(FAT32, fat32), (EXFAT, exfat), (CONFLICT, conflict)]
}

fn new_file(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

fn check_images(dir: &Dir) -> Result<(), String> {
    for (name, expected) in images() {
        let file = dir.open(name).map_err(err)?;
        if file.metadata().map_err(err)?.len() != expected.len() as u64 {
            return Err(format!("{name} size changed"));
        }
        let mut contents = Vec::with_capacity(expected.len());
        file.take(expected.len() as u64 + 1)
            .read_to_end(&mut contents)
            .map_err(err)?;
        if contents != expected {
            return Err(format!("{name} bytes changed"));
        }
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Baseline {
    tables: [String; 3],
    b_offline: bool,
    b_read_only: bool,
}

fn layout() -> Result<Baseline, String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|disk| disk.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("Test B identity is missing")?;
    Ok(Baseline {
        tables: ntfs_reconstruction_checks::table_hashes()?,
        b_offline: b.is_offline.ok_or("Test B online state is unavailable")?,
        b_read_only: b.is_read_only.ok_or("Test B read-only state is unavailable")?,
    })
}

pub(super) fn prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, volume) = ntfs_reconstruction_checks::verified_a_root()?;
    let baseline = layout()?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    for (name, bytes) in images() {
        new_file(&dir, name, &bytes)?;
    }
    check_images(&dir)?;
    if layout()? != baseline {
        return Err("Disk or guest OS layout changed during fixture preparation".into());
    }
    new_file(&dir, BASELINE, &serde_json::to_vec(&baseline).map_err(err)?)?;
    let digest = |bytes| format!("{:x}", Sha256::digest(bytes));
    Ok(format!(
        "PASS: three verified test images in {}. FAT32 view SHA-256 {}. exFAT view SHA-256 {}. GPTs and guest OS layout unchanged.",
        volume.join(FOLDER).display(), digest(fixture::FAT_CONTENT), digest(fixture::EXFAT_CONTENT)
    ))
}

pub(super) fn verify() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let baseline: Baseline =
        serde_json::from_reader(dir.open(BASELINE).map_err(err)?.take(1024)).map_err(err)?;
    check_images(&dir)?;
    if layout()? != baseline {
        return Err("Disk or guest OS layout changed during image reconstruction".into());
    }
    Ok("PASS: all three reference images are byte-for-byte unchanged. Both GPTs, the guest OS layout and Test B's online and read-only states match the baseline.".into())
}

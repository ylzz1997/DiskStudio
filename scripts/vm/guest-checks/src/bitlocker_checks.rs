//! Compare a recognizable but synthetic BitLocker boot marker with plain NTFS.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::io::Read;

const FOLDER: &str = "DS-BitLockerMarker";
const BASELINE: &str = "layout.json";
const NTFS: &[u8] = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");

fn images() -> [(&'static str, Vec<u8>); 2] {
    let mut marker = NTFS.to_vec();
    marker[3..11].copy_from_slice(b"-FVE-FS-");
    marker[11..17].copy_from_slice(&[0, 2, 8, 32, 0, 2]);
    marker[1080..1082].copy_from_slice(&[0x53, 0xef]);
    [("bitlocker-marker.img", marker), ("plain-ntfs.img", NTFS.to_vec())]
}

fn save(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

fn compare(dir: &Dir) -> Result<(), String> {
    for (name, expected) in images() {
        let file = dir.open(name).map_err(err)?;
        let mut observed = Vec::with_capacity(expected.len());
        file.take(expected.len() as u64 + 1)
            .read_to_end(&mut observed)
            .map_err(err)?;
        if observed != expected {
            return Err(format!("{name} differs from the fixture"));
        }
    }
    Ok(())
}

pub(super) fn prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, volume) = ntfs_reconstruction_checks::verified_a_root()?;
    let layout = ntfs_reconstruction_checks::table_hashes()?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    for (name, bytes) in images() {
        save(&dir, name, &bytes)?;
    }
    compare(&dir)?;
    save(&dir, BASELINE, &serde_json::to_vec(&layout).map_err(err)?)?;
    Ok(format!(
        "PASS: synthetic marker and plain NTFS images prepared on the verified A/2 volume at {}. Plain sample SHA-256 {:x}.",
        volume.join(FOLDER).display(),
        Sha256::digest(b"12345"),
    ))
}

pub(super) fn verify() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let layout: [String; 3] =
        serde_json::from_reader(dir.open(BASELINE).map_err(err)?.take(1024)).map_err(err)?;
    compare(&dir)?;
    if ntfs_reconstruction_checks::table_hashes()? != layout {
        return Err("A/B partition tables or guest OS partition metadata differ from the baseline".into());
    }
    Ok("PASS: both fixture images match every original byte. A/B GPTs and guest OS partition metadata are unchanged.".into())
}

//! Retained 4Kn search fixtures and readback on the isolated 512-byte test pair.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fat_view;

const ROOT: &str = "DS-Unallocated-Search";
const FILES: [&str; 2] = ["gaps.vhdx", "gaps.dsbackup"];
const SECTOR: u64 = 4096;

#[derive(Deserialize, serde::Serialize)]
struct Witness {
    desktop: String,
    inputs: Vec<String>,
}

fn inputs(folder: &Dir) -> Result<Vec<String>, String> {
    FILES
        .iter()
        .map(|name| {
            folder
                .read(name)
                .map(|bytes| format!("{:x}", Sha256::digest(&bytes)))
                .map_err(err)
        })
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

fn image() -> Result<Vec<u8>, String> {
    let size = 48 * MIB;
    let partition = table_restore::Entry {
        slot: 1,
        guid: "2b3d84d7-e5a4-4f37-9daf-bbaa11a23401".into(),
        kind: partitions::policy::BASIC_DATA.into(),
        offset: 16 * MIB,
        size: 8 * MIB,
        attributes: 0,
        name: "Registered".encode_utf16().collect(),
    };
    let table = table_restore::new_gpt(
        size,
        SECTOR,
        format!("{:x}", Sha256::digest(b"unallocated search fixture")),
        uuid::Uuid::from_u128(0xa12f_1411_2e79_4bfd_b713_d5da_f856_3c00),
        &[partition],
    )?;
    let mut image = vec![0; size as usize];
    for extent in table.extents {
        image[extent.offset as usize..extent.offset as usize + extent.bytes.len()]
            .copy_from_slice(&extent.bytes);
    }
    let exfat = fat_view::exfat(SECTOR as usize);
    for at in [2 * MIB, 16 * MIB, 32 * MIB] {
        image[at as usize..at as usize + exfat.len()].copy_from_slice(&exfat);
    }
    Ok(image)
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    // Prove the existing offline B Ext4 fixture before the native search.
    lost_backup_boot_checks::verify_ext()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(ROOT).map_err(err)?;
    let folder = root.open_dir(ROOT).map_err(err)?;
    let image = image()?;
    let size = image.len() as u64;
    let mut encoder = image_convert::Encoder::new(
        create(&folder, FILES[0])?,
        image_convert::Options {
            format: image_convert::Format::VhdxDynamic,
            size,
            sector: SECTOR,
        },
        image_convert::Identity {
            created: *uuid::Uuid::new_v4().as_bytes(),
            modified: *uuid::Uuid::new_v4().as_bytes(),
            vhd_timestamp: 0,
            file_name: FILES[0].into(),
        },
    )?;
    for (index, bytes) in image.chunks(image_convert::CHUNK).enumerate() {
        encoder.push((index * image_convert::CHUNK) as u64, bytes)?;
    }
    for (index, bytes) in image.chunks(image_convert::CHUNK).enumerate() {
        encoder.verify_source((index * image_convert::CHUNK) as u64, bytes)?;
    }
    encoder.finish(|file| file.sync_all().map_err(err), |_| Ok(()))?;
    backup_archive::create(
        &mut Cursor::new(&image),
        &mut create(&folder, FILES[1])?,
        "disk",
        size,
        SECTOR,
        true,
        None,
        |_| Ok(()),
    )?;
    let witness = Witness {
        desktop,
        inputs: inputs(&folder)?,
    };
    let out = create(&folder, "baseline.json")?;
    serde_json::to_writer(&out, &witness).map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: {} holds a 4Kn VHDX and backup. Search all unallocated space. Expect exFAT at LBA 512 and 8192. The registered copy at LBA 4096 is excluded. Offline B has the prior Ext4 fixture at LBA 16777217.",
        path.join(ROOT).display(),
    ))
}

pub(super) fn verify() -> Result<String, String> {
    btrfs_checks::desktop()?;
    lost_backup_boot_checks::verify_ext()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(ROOT).map_err(err)?;
    let witness: Witness =
        serde_json::from_reader(folder.open("baseline.json").map_err(err)?.take(4096))
            .map_err(err)?;
    if witness.inputs != inputs(&folder)? {
        return Err("The VHDX or backup differs from baseline".into());
    }
    Ok("PASS: both retained inputs match full SHA-256. Offline B's Ext4 fixture and adjacent sectors, both GPTs and the guest system disk layout match the baseline.".into())
}

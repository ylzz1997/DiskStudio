//! Guest-only readback of files produced through the native sector window.
use super::*;
use allocation_batch::record;
use disk_core::MIB;
use reallocation_checks::{metadata_partition, os_layout, state};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

const BASELINE: &str = "sector-copy-4kn-baseline";
pub(super) const VERIFY: &str = "sector-copy-4kn-verify";
const SECTOR: u64 = 4096;
const PARTITION_START_MIB: u64 = 8704;
const PARTITION_SIZE_MIB: u64 = 384;
const PARTITION_LBA: u64 = PARTITION_START_MIB * MIB / SECTOR;
const EXPORTS: [(&str, u64, u64); 3] = [
    ("DiskStudio-LBA-0-2.bin", 0, 2),
    ("DiskStudio-LBA-2228224-3.bin", PARTITION_LBA, 3),
    ("DiskStudio-sector-0.bin", 0, 1),
];
const REJECTED: &str = "DiskStudio-LBA-0-1.bin";

fn witness_path() -> Result<PathBuf, String> {
    let desktop = option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        .ok_or("Build must bind the completed desktop SHA-256")?;
    Ok(PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-sector-copy-4kn-{desktop}.json"
    )))
}

fn fixture() -> Result<(PathBuf, Selection, Selection, serde_json::Value), String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    state(&disks, B, false)?;
    metadata_partition(A, None, PARTITION_START_MIB, PARTITION_SIZE_MIB)?;
    let destination = metadata_partition(B, Some("DS4KMOVEJRN"), 32, 256)?;
    let drive = destination
        .drive_letter
        .ok_or("B output volume has no drive letter")?;
    let bound_b = selection(B, Some(destination.number))?;
    let (folder, target) = data_windows::folder_location(Path::new(&format!("{drive}:\\")))?;
    if target != bound_b.target {
        return Err("Output folder is not on the authorized B test disk".into());
    }
    let archive_hash = |serial| -> Result<String, String> {
        let archive = quick_partition_checks::table(serial)?;
        let bytes = serde_json::to_vec(&archive).map_err(err)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    };
    let snapshot = json!({
        "aGptSha256": archive_hash(A)?,
        "bGptSha256": archive_hash(B)?,
        "guestOsLayout": os_layout(&disks)?,
        "aOffline": true,
    });
    Ok((folder, selection(A, None)?, bound_b, snapshot))
}

fn absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(e)),
        Ok(_) => Err(format!("Expected no existing file at {}", path.display())),
    }
}

pub(super) fn baseline() -> Result<String, String> {
    let mut log = backup_protection::begin(BASELINE)?;
    let (folder, _, _, snapshot) = fixture()?;
    for name in EXPORTS
        .map(|(name, _, _)| name)
        .into_iter()
        .chain([REJECTED])
    {
        absent(&folder.join(name))?;
        absent(&folder.join(format!("{name}.partial")))?;
    }
    let mut witness = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(witness_path()?)
        .map_err(err)?;
    serde_json::to_writer(&mut witness, &snapshot).map_err(err)?;
    witness.sync_all().map_err(err)?;
    record(&mut log, snapshot)?;
    Ok(format!(
        "BASELINE SAVED. A offline, A/B GPT and OS metadata recorded. Save results to B DS4KMOVEJRN at {}. Partition range on A starts at absolute LBA {PARTITION_LBA}.",
        folder.display()
    ))
}

pub(super) fn verify() -> Result<String, String> {
    let mut log = backup_protection::begin(VERIFY)?;
    let (folder, bound_a, bound_b, snapshot) = fixture()?;
    let witness = fs::File::open(witness_path()?).map_err(err)?;
    let before: serde_json::Value = serde_json::from_reader(witness.take(4096)).map_err(err)?;
    if before != snapshot {
        return Err("A/B GPT or OS layout changed".into());
    }
    let a = data_windows::resolve(&bound_a, false, false)?;
    let mut source = data_windows::device(&a, false)?;
    for (name, lba, count) in EXPORTS {
        absent(&folder.join(format!("{name}.partial")))?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder.join(name))
            .map_err(err)?;
        let binding = data_windows::FileBinding::new(&file, &bound_b.target)?;
        data_windows::FileBinding::validate_all(&[binding])?;
        let size = count * SECTOR;
        if file.metadata().map_err(err)?.len() != size {
            return Err(format!("{name} length differs from {size} B"));
        }
        let mut expected = vec![0; size as usize];
        source.seek(SeekFrom::Start(lba * SECTOR)).map_err(err)?;
        source.read_exact(&mut expected).map_err(err)?;
        let mut actual = Vec::with_capacity(size as usize);
        file.take(size + 1).read_to_end(&mut actual).map_err(err)?;
        if actual != expected {
            return Err(format!("{name} differs from A LBA {lba} count {count}"));
        }
        record(
            &mut log,
            json!({
                "file": name, "absoluteLba": lba, "count": count, "bytes": size,
                "sha256": format!("{:x}", Sha256::digest(&actual)),
            }),
        )?;
    }
    absent(&folder.join(REJECTED))?;
    absent(&folder.join(format!("{REJECTED}.partial")))?;
    record(
        &mut log,
        json!({
            "result": "pass", "exports": 3, "sameDiskOutputAbsent": true, "snapshot": snapshot,
        }),
    )?;
    Ok("PASS: three native exports matched A byte for byte on B; A/B GPT and OS layout unchanged, A remains offline. Same-disk output and partial files absent.".into())
}

//! Guest-only readback of files produced through the native sector window.
use super::*;
use allocation_batch::record;
use disk_core::MIB;
use reallocation_checks::{metadata_partition, os_layout, state};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::File;
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
const FAULT_START: u64 = PARTITION_LBA + 4096;
const FAULT_LBA: u64 = FAULT_START + 3;
const FAULT_COUNT: u64 = 8;
const STRICT: &str = "ds-sector-strict.bin";
const REVERSE: &str = "ds-sector-reverse.bin";
const FORWARD: &str = "ds-sector-forward.bin";
const FAULT_REPORT: &str = "ds-sector-reverse.json";

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

fn fault_reference(bound: &Selection) -> Result<(Vec<u8>, i32), String> {
    let resolved = data_windows::resolve(bound, false, false)?;
    let mut device = data_windows::device(&resolved, false)?;
    let mut bytes = vec![0; (FAULT_COUNT * SECTOR) as usize];
    let mut code = None;
    for (index, block) in bytes.chunks_exact_mut(SECTOR as usize).enumerate() {
        let lba = FAULT_START + index as u64;
        device.seek(SeekFrom::Start(lba * SECTOR)).map_err(err)?;
        let read = device.read_exact(block);
        if lba == FAULT_LBA {
            let error = read
                .err()
                .ok_or("The fixture LBA was unexpectedly readable")?;
            code = error.raw_os_error();
            if !matches!(code, Some(23 | 27 | 1117)) {
                return Err(format!("Unexpected native fault at LBA {lba}: {error}"));
            }
            block.fill(0);
        } else {
            read.map_err(err)?;
        }
    }
    Ok((bytes, code.ok_or("Expected one native sector read error")?))
}

pub(super) fn prepare_surface_refinement() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let destination = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("Test B missing")?;
    let b_offline = destination
        .is_offline
        .ok_or("Test B offline state unknown")?;
    state(&disks, B, b_offline)?;
    if destination.is_clustered != Some(false) {
        return Err("Test B cluster state differs from the fixture".into());
    }
    metadata_partition(B, None, 32, 256)?;
    let tables = reread_tables(&disks)?;
    if b_offline {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct OfflineReason {
            number: u32,
            offline_reason: Option<u16>,
        }
        let connection =
            WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
        let reasons: Vec<OfflineReason> = connection
            .raw_query("SELECT Number,OfflineReason FROM MSFT_Disk")
            .map_err(err)?;
        let reasons: Vec<_> = reasons
            .iter()
            .filter(|r| r.number == destination.number)
            .collect();
        if reasons.len() != 1 || reasons[0].offline_reason != Some(1) {
            return Err("Test B was not explicitly taken offline earlier".into());
        }
    }
    offline_fault_source()?;
    online(B)?;
    let (folder, _, _, _) = fixture()?;
    if reread_tables(&inventory()?)? != tables {
        return Err("A/B GPT or the system layout changed during fixture preparation".into());
    }
    Ok(format!("PASS: desktop {desktop}. Test A is offline and Test B is online. GPTs and system layout unchanged. B report folder: {}.", folder.display()))
}

pub(super) fn inspect_surface_refinement() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (folder, bound_a, _, _) = fixture()?;
    let (_, code) = fault_reference(&bound_a)?;
    Ok(format!(
        "PASS: desktop {desktop}. {A} is offline. {B} is online. Both NVMe disks are 16 GiB, 4Kn. Only A LBA {FAULT_LBA} in the reference range returned Win32 code {code}. Save to {}.",
        folder.display()
    ))
}

fn surface_result(folder: &Path, bound_b: &Selection, name: &str) -> Result<serde_json::Value, String> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder.join(name))
        .map_err(err)?;
    data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
        &file,
        &bound_b.target,
    )?])?;
    if file.metadata().map_err(err)?.len() > 2 * MIB {
        return Err(format!("Unexpected file length for {name}"));
    }
    serde_json::from_reader(file.take(2 * MIB)).map_err(err)
}

pub(super) fn verify_surface_refinement() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (folder, bound_a, bound_b, _) = fixture()?;
    let (_, code) = fault_reference(&bound_a)?;
    let coarse = surface_result(&folder, &bound_b, "SurfaceCoarse.json")?;
    let refined = surface_result(&folder, &bound_b, "SurfaceRefined.json")?;
    let progress = surface_result(&folder, &bound_b, "SurfaceRefined.dsscan")?;
    let sectors_per_block = MIB / SECTOR;
    let start = FAULT_LBA / sectors_per_block * sectors_per_block;
    for file in [&coarse, &refined] {
        let report = &file["result"]["report"];
        let initial = &report["failures"][0];
        if file["format"] != "DiskStudio surface report"
            || file["version"] != 1
            || file["result"]["target"] != bound_a.target.0
            || !file["result"]["partition"].is_null()
            || file["result"]["options"]
                != json!({"startSector": start, "sectorCount": 2 * sectors_per_block})
            || report["state"] != "completed"
            || report["sectorSize"] != SECTOR
            || report["offset"] != start * SECTOR
            || report["checkedBytes"] != 2 * MIB
            || report["readableBytes"] != MIB
            || report["failedBytes"] != MIB
            || report["omittedFailures"] != 0
            || report["failures"].as_array().map(Vec::len) != Some(1)
            || initial["startSector"] != start
            || initial["sectorCount"] != sectors_per_block
            || initial["code"] != code
        {
            return Err(
                "Surface report's initial read results differ from the fixed fault fixture".into(),
            );
        }
    }
    let expected = json!({
        "reads": 16,
        "failedSectors": 1,
        "unresolvedSectors": 0,
        "ranges": [{"startSector": FAULT_LBA, "sectorCount": 1, "code": code, "resolution": "sectorReadFailed"}],
        "omittedRanges": 0,
    });
    if coarse["result"]["report"].get("refinement").is_some()
        || refined["result"]["report"]["refinement"] != expected
        || progress["format"] != "DiskStudio surface progress"
        || progress["version"] != 1
        || progress["progress"]["snapshot"]["report"] != refined["result"]["report"]
        || progress["progress"]["options"] != refined["result"]["options"]
    {
        return Err(
            "Refined report or saved progress differs from the expected sector result".into(),
        );
    }
    Ok(format!(
        "PASS: desktop {desktop}. A is offline. Both reports match the initial 1 MiB error. B's refined report and checkpoint agree: LBA {FAULT_LBA}, Win32 code {code}, 16 follow-up reads, 255 sectors read on follow-up, none unresolved."
    ))
}

pub(super) fn verify_surface_timeout() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (folder, bound_a, bound_b, _) = fixture()?;
    let checkpoint = surface_result(&folder, &bound_b, "SurfaceTimeout.dsscan")?;
    let resumed = surface_result(&folder, &bound_b, "SurfaceTimeoutResumed.json")?;
    let previous = &checkpoint["progress"]["snapshot"];
    let current = &resumed["result"];
    let start = FAULT_START + MIB / SECTOR;
    let options = json!({"startSector": start, "sectorCount": 2 * MIB / SECTOR});
    for snapshot in [previous, current] {
        let report = &snapshot["report"];
        if snapshot["target"] != bound_a.target.0
            || !snapshot["partition"].is_null()
            || snapshot["options"] != options
            || report["sectorSize"] != SECTOR
            || report["offset"] != start * SECTOR
            || report["size"] != 2 * MIB
            || report["failedBytes"] != 0
            || report["failures"] != json!([])
        {
            return Err("Surface timeout case has an unexpected source, range, or media failure".into());
        }
    }
    let stopped = &previous["report"];
    let finished = &current["report"];
    let checked = stopped["checkedBytes"]
        .as_u64()
        .ok_or("Missing completed prefix")?;
    let timed_out_read = &stopped["timedOutRead"];
    let timed_out_lba = timed_out_read["startSector"]
        .as_u64()
        .ok_or("Missing timeout LBA")?;
    let timed_out_sectors = timed_out_read["sectorCount"]
        .as_u64()
        .ok_or("Missing timeout extent")?;
    let block_start = start + checked / SECTOR;
    if checkpoint["format"] != "DiskStudio surface progress"
        || checkpoint["version"] != 1
        || checkpoint["progress"]["options"] != options
        || stopped["state"] != "timedOut"
        || stopped["readTimeoutSeconds"] != 1
        || !matches!(checked, 0 | MIB)
        || stopped["readableBytes"] != checked
        || timed_out_sectors == 0
        || timed_out_lba < block_start
        || timed_out_lba
            .checked_add(timed_out_sectors)
            .is_none_or(|end| end > block_start + MIB / SECTOR)
        || resumed["format"] != "DiskStudio surface report"
        || resumed["version"] != 1
        || current["loaded"] != true
        || current["resumeCount"] != 1
        || finished["state"] != "completed"
        || finished["readTimeoutSeconds"] != 3
        || finished.get("timedOutRead").is_some()
        || finished["checkedBytes"] != 2 * MIB
        || finished["readableBytes"] != 2 * MIB
    {
        return Err("Unexpected timeout checkpoint or resumed report properties".into());
    }
    Ok(format!("PASS: desktop {desktop}. A remains offline. One-second timeout at LBA {timed_out_lba} across {timed_out_sectors} sectors recorded {checked} completed bytes and zero media errors. Reloaded checkpoint resumed with three seconds and completed 2 MiB. Exact files verified on B."))
}

pub(super) fn fault_prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let receipt = witness_path()?.with_extension("fault.json");
    absent(&receipt)?;
    offline_fault_source()?;
    let (folder, bound_a, _, snapshot) = fixture()?;
    for name in [STRICT, REVERSE, FORWARD, FAULT_REPORT] {
        absent(&folder.join(name))?;
        absent(&folder.join(format!("{name}.partial")))?;
    }
    let (bytes, code) = fault_reference(&bound_a)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(
        &mut file,
        &json!({
            "desktop": desktop, "snapshot": snapshot, "windowsCode": code,
            "referenceSha256": format!("{:x}", Sha256::digest(bytes)),
        }),
    )
    .map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: A offline. Native error {code} at LBA {FAULT_LBA} only. Export {FAULT_COUNT} sectors starting at whole-disk LBA {FAULT_START} into {}",
        folder.display()
    ))
}

fn offline_fault_source() -> Result<(), String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    metadata_partition(A, None, PARTITION_START_MIB, PARTITION_SIZE_MIB)?;
    let a = disks
        .iter()
        .find(|disk| disk.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("Test A missing")?;
    if a.is_offline == Some(false) && a.is_read_only == Some(false) && a.is_clustered == Some(false)
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Outcome {
            return_value: u32,
        }
        let con =
            WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
        let result: Outcome = con
            .exec_instance_method::<Disk, _>(&a.path, "Offline", ())
            .map_err(err)?;
        if result.return_value != 0 {
            return Err(format!(
                "Cannot take Test A offline: {}",
                result.return_value
            ));
        }
    }
    state(&inventory()?, A, true)
}

pub(super) fn fault_verify() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (folder, bound_a, bound_b, snapshot) = fixture()?;
    let (reference, code) = fault_reference(&bound_a)?;
    let before: serde_json::Value = serde_json::from_reader(
        File::open(witness_path()?.with_extension("fault.json"))
            .map_err(err)?
            .take(16 * 1024),
    )
    .map_err(err)?;
    if before
        != json!({
            "desktop": desktop, "snapshot": snapshot, "windowsCode": code,
            "referenceSha256": format!("{:x}", Sha256::digest(&reference)),
        })
    {
        return Err("A/B GPT, OS layout, reference data or native fault changed".into());
    }
    let open = |name: &str| -> Result<File, String> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder.join(name))
            .map_err(err)?;
        data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
            &file,
            &bound_b.target,
        )?])?;
        Ok(file)
    };
    absent(&folder.join(STRICT))?;
    if open(&format!("{STRICT}.partial"))?
        .metadata()
        .map_err(err)?
        .len()
        != 0
    {
        return Err("Unexpected data in the strict failure partial".into());
    }
    let mut reverse_sha = String::new();
    for name in [FORWARD, REVERSE] {
        absent(&folder.join(format!("{name}.partial")))?;
        let mut expected = reference.clone();
        if name == REVERSE {
            for sector in
                expected[SECTOR as usize..(4 * SECTOR) as usize].chunks_exact_mut(SECTOR as usize)
            {
                for (byte, pattern) in sector.iter_mut().zip(b"LOSS".iter().cycle()) {
                    *byte = *pattern;
                }
            }
        }
        let mut actual = Vec::new();
        open(name)?
            .take(FAULT_COUNT * SECTOR + 1)
            .read_to_end(&mut actual)
            .map_err(err)?;
        if actual != expected {
            return Err(format!(
                "{name} differs from independently read or filled sectors"
            ));
        }
        if name == REVERSE {
            reverse_sha = format!("{:x}", Sha256::digest(actual));
        }
    }
    absent(&folder.join(format!("{FAULT_REPORT}.partial")))?;
    let report: serde_json::Value =
        serde_json::from_reader(open(FAULT_REPORT)?.take(8192)).map_err(err)?;
    let expected = json!({
        "version": 1, "startLba": FAULT_START, "sectorSize": SECTOR, "sectorCount": FAULT_COUNT,
        "direction": "backward", "errors": {"kind": "fill", "skipSectors": 2, "marker": "LOSS"},
        "outputName": REVERSE, "sha256": reverse_sha,
        "failures": {"filledSectors": 3, "unreadableSectors": 1, "omitted": 0, "ranges": [
            {"failedLba": FAULT_LBA, "firstFilledLba": FAULT_LBA - 2, "filledSectors": 3, "windowsCode": code}
        ]},
        "elapsedMs": report["elapsedMs"],
    });
    if !report["elapsedMs"].is_u64() || report != expected {
        return Err("Saved report differs from the observed error, bounds, copy options or byte-order digest".into());
    }
    Ok(format!("PASS: strict partial, forward zero fill, reverse LOSS fill and saved report match. A/B GPT and OS layout preserved. Native error {code}. SHA-256 {reverse_sha}"))
}

const SCRATCH_BYTES: u64 = 32 * MIB;
const DIRECT_OFFSET: u64 = 16;
const PARTITION_OFFSET: u64 = 40;
const DEVICE_FIXTURE_ONCE: &str = r"C:\ProgramData\DiskStudio-sector-copy-device-4kn-20261120.once";

#[derive(Deserialize, Serialize)]
struct DeviceWitness {
    desktop: String,
    partition: u32,
    offset: u64,
    a_gpt: String,
    b_gpt: String,
    os_layout: String,
    neighbors: [String; 2],
    direct_sha256: String,
    zero_filled_reference: String,
    windows_code: i32,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_span(bound: &Selection, offset: u64, bytes: u64) -> Result<Vec<u8>, String> {
    let resolved = data_windows::resolve(bound, false, false)?;
    if bound.partition.is_some()
        || bytes > SCRATCH_BYTES
        || resolved.disk.logical_sector_size != Some(SECTOR)
        || offset % SECTOR != 0
        || bytes % SECTOR != 0
        || offset
            .checked_add(bytes)
            .is_none_or(|end| end > resolved.size)
    {
        return Err("Unsafe device fixture read bounds".into());
    }
    let mut file = data_windows::device(&resolved, false)?;
    file.seek(SeekFrom::Start(offset)).map_err(err)?;
    let mut value = vec![0; bytes as usize];
    for chunk in value.chunks_mut(128 * 1024) {
        file.read_exact(chunk).map_err(err)?;
    }
    Ok(value)
}

fn neighbor_digests(bound: &Selection, offset: u64) -> Result<[String; 2], String> {
    Ok([
        digest(&read_span(bound, offset - SECTOR, SECTOR)?),
        digest(&read_span(bound, offset + SCRATCH_BYTES, SECTOR)?),
    ])
}

pub(super) fn device_prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let receipt = witness_path()?.with_extension("device.json");
    absent(&receipt)?;
    absent(Path::new(DEVICE_FIXTURE_ONCE))?;
    offline_fault_source()?;
    let disks = inventory()?;
    state(&disks, B, false)?;
    let os_before = os_layout(&disks)?;
    let a_before = quick_partition_checks::table(A)?;
    let b_before = table_restore::archive_layout(&quick_partition_checks::table(B)?)?;
    let bound_a = selection(A, None)?;
    let bound_b = selection(B, None)?;
    let direct = read_span(&bound_a, 0, FAULT_COUNT * SECTOR)?;
    if direct.iter().all(|byte| *byte == 0) {
        return Err("Expected nonzero reference sectors on A".into());
    }
    let (reference, code) = fault_reference(&bound_a)?;
    let status = partitions::status();
    let target = status
        .targets
        .iter()
        .find(|target| target.serial == B && target.target == bound_b.target)
        .ok_or("B target unavailable")?;
    let gap = target
        .free_ranges
        .iter()
        .filter(|gap| gap.size >= SCRATCH_BYTES + 2 * MIB)
        .max_by_key(|gap| gap.offset)
        .ok_or("B requires an unallocated 34 MiB scratch extent")?;
    let offset = gap.offset + MIB;
    if offset % MIB != 0
        || offset / SECTOR <= b_before.first
        || (offset + SCRATCH_BYTES) / SECTOR > b_before.last
        || b_before
            .entries
            .iter()
            .any(|e| offset < e.offset + e.size && e.offset < offset + SCRATCH_BYTES)
        || read_span(&bound_b, offset, SCRATCH_BYTES)?
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(
            "B scratch extent is not clean, aligned, and free in the independent GPT".into(),
        );
    }
    let neighbors = neighbor_digests(&bound_b, offset)?;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(DEVICE_FIXTURE_ONCE)
        .map_err(err)?
        .sync_all()
        .map_err(err)?;
    operation(
        B,
        partitions::Action::Create {
            offset,
            size: SCRATCH_BYTES,
            kind: partitions::PartitionKind::Windows,
        },
    )?;
    let part = metadata_partition(B, None, offset / MIB, SCRATCH_BYTES / MIB)?;
    let bound_b = selection(B, None)?;
    let b_table = quick_partition_checks::table(B)?;
    let mut b_after = table_restore::archive_layout(&b_table)?;
    b_after
        .entries
        .retain(|e| (e.offset, e.size) != (offset, SCRATCH_BYTES));
    if b_after != b_before
        || quick_partition_checks::table(A)? != a_before
        || os_layout(&inventory()?)? != os_before
        || neighbors != neighbor_digests(&bound_b, offset)?
        || read_span(&bound_b, offset, SCRATCH_BYTES)?
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err("Existing disk layout or scratch data changed while preparing B".into());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(
        &mut file,
        &DeviceWitness {
            desktop,
            partition: part.number,
            offset,
            a_gpt: digest(&a_before.encode()?),
            b_gpt: digest(&b_table.encode()?),
            os_layout: os_before,
            neighbors,
            direct_sha256: digest(&direct),
            zero_filled_reference: digest(&reference),
            windows_code: code,
        },
    )
    .map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!("PASS: A offline. B scratch partition {} starts at LBA {}. Copy A LBA 0 for {FAULT_COUNT} sectors to B whole disk LBA {}. Then use A LBA {FAULT_START} and B partition {} relative LBA {PARTITION_OFFSET}, backward LOSS with 2 extra skips. Exactly one fault, {code} at A LBA {FAULT_LBA}.", part.number, offset / SECTOR, offset / SECTOR + DIRECT_OFFSET, part.number))
}

fn verify_device_fixture(reverse_written: bool) -> Result<(DeviceWitness, String), String> {
    let desktop = btrfs_checks::desktop()?;
    let receipt = witness_path()?.with_extension("device.json");
    let baseline: DeviceWitness =
        serde_json::from_reader(File::open(receipt).map_err(err)?.take(8192)).map_err(err)?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    state(&disks, B, true)?;
    let part = metadata_partition(B, None, baseline.offset / MIB, SCRATCH_BYTES / MIB)?;
    let bound_a = selection(A, None)?;
    let bound_b = selection(B, None)?;
    if desktop != baseline.desktop
        || part.number != baseline.partition
        || digest(&quick_partition_checks::table(A)?.encode()?) != baseline.a_gpt
        || digest(&quick_partition_checks::table(B)?.encode()?) != baseline.b_gpt
        || os_layout(&disks)? != baseline.os_layout
        || neighbor_digests(&bound_b, baseline.offset)? != baseline.neighbors
    {
        return Err("A/B GPT, OS layout or scratch neighboring sectors changed".into());
    }
    let direct = read_span(&bound_a, 0, FAULT_COUNT * SECTOR)?;
    let (mut reverse, code) = fault_reference(&bound_a)?;
    if digest(&direct) != baseline.direct_sha256
        || digest(&reverse) != baseline.zero_filled_reference
        || code != baseline.windows_code
    {
        return Err("A source bytes or independent read error changed".into());
    }
    for (byte, pattern) in reverse[SECTOR as usize..(4 * SECTOR) as usize]
        .iter_mut()
        .zip(b"LOSS".iter().cycle())
    {
        *byte = *pattern;
    }
    let mut expected = vec![0; SCRATCH_BYTES as usize];
    let start = (DIRECT_OFFSET * SECTOR) as usize;
    expected[start..start + direct.len()].copy_from_slice(&direct);
    if reverse_written {
        let start = (PARTITION_OFFSET * SECTOR) as usize;
        expected[start..start + reverse.len()].copy_from_slice(&reverse);
    }
    if read_span(&bound_b, baseline.offset, SCRATCH_BYTES)? != expected {
        return Err("B scratch changed outside the expected writes or copied bytes differ".into());
    }
    Ok((baseline, digest(&reverse)))
}

pub(super) fn device_continue() -> Result<String, String> {
    let (baseline, _) = verify_device_fixture(false)?;
    // A separate guest fixture action, after checking the first copy. Windows
    // may hide required partition protection metadata while B is offline.
    online(B)?;
    let disks = inventory()?;
    state(&disks, A, true)?;
    state(&disks, B, false)?;
    Ok(format!("PASS: first copy, entire 32 MiB scratch, both neighbors and A/B GPT verified. B online for the next isolated test, A offline. First SHA-256 {}.", baseline.direct_sha256))
}

pub(super) fn device_verify() -> Result<String, String> {
    let (baseline, reverse_sha) = verify_device_fixture(true)?;
    Ok(format!("PASS: strict disk addressing and reverse partition addressing match across the entire 32 MiB B scratch. Both neighboring sectors and A/B GPT and guest OS layout unchanged. Both disks offline. SHA-256 {} and {reverse_sha}.", baseline.direct_sha256))
}

const REREAD_FILE: &str = "ds-sector-reread-checked.bin";
const REREAD_REPORT: &str = "ds-sector-reread-checked.json";
const REREAD_SRC: u64 = FAULT_START - MIB / SECTOR;
const REREAD_SECTORS: u64 = 4 * MIB / SECTOR;
const REREAD_PARTITION: u32 = 7;
const REREAD_BASE: u64 = 3473664 * SECTOR;
const REREAD_DEST: u64 = 2048 * SECTOR;

#[derive(Deserialize, Serialize)]
struct RereadWitness {
    desktop: String,
    tables: [String; 3],
    scratch: String,
    neighbors: [String; 2],
    file: String,
    device: String,
    windows_error: i32,
}

fn reread_tables(disks: &[Disk]) -> Result<[String; 3], String> {
    let a = quick_partition_checks::table(A)?;
    let b = quick_partition_checks::table(B)?;
    let a_layout = table_restore::archive_layout(&a)?;
    let b_layout = table_restore::archive_layout(&b)?;
    if a_layout.guid == b_layout.guid
        || a_layout
            .entries
            .iter()
            .any(|a| b_layout.entries.iter().any(|b| a.guid == b.guid))
    {
        return Err("Test disk GPT identifiers are not distinct".into());
    }
    Ok([
        digest(&a.encode()?),
        digest(&b.encode()?),
        os_layout(disks)?,
    ])
}

fn reread_scratch() -> Result<Selection, String> {
    let part = metadata_partition(B, None, REREAD_BASE / MIB, SCRATCH_BYTES / MIB)?;
    if part.number != REREAD_PARTITION {
        return Err("Expected the retained B test partition 7".into());
    }
    selection(B, None)
}

fn mark_fault(bytes: &mut [u8], start: u64, marker: &[u8]) {
    let offset = ((FAULT_LBA - start) * SECTOR) as usize;
    for (byte, pattern) in bytes[offset..offset + SECTOR as usize]
        .iter_mut()
        .zip(marker.iter().cycle())
    {
        *byte = *pattern;
    }
}

fn reread_file_reference(bound: &Selection) -> Result<(Vec<u8>, i32), String> {
    let (mut full, code) = fault_reference(bound)?;
    mark_fault(&mut full, FAULT_START, b"LOSS");
    Ok((full, code))
}

fn reread_device_reference(bound: &Selection) -> Result<Vec<u8>, String> {
    let mut full = read_span(
        bound,
        REREAD_SRC * SECTOR,
        (FAULT_LBA - REREAD_SRC) * SECTOR,
    )?;
    full.resize(full.len() + SECTOR as usize, 0);
    full.extend(read_span(
        bound,
        (FAULT_LBA + 1) * SECTOR,
        (REREAD_SRC + REREAD_SECTORS - FAULT_LBA - 1) * SECTOR,
    )?);
    mark_fault(&mut full, REREAD_SRC, b"LOSS");
    Ok(full)
}

fn reread_receipt() -> Result<PathBuf, String> {
    Ok(witness_path()?.with_extension("reread.json"))
}

pub(super) fn reread_prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let receipt = reread_receipt()?;
    absent(&receipt)?;
    offline_fault_source()?;
    let disks = inventory()?;
    state(&disks, B, true).or_else(|_| state(&disks, B, false))?;
    let tables = reread_tables(&disks)?;
    let bound_b = reread_scratch()?;
    let scratch = read_span(&bound_b, REREAD_BASE, SCRATCH_BYTES)?;
    let start = REREAD_DEST as usize;
    if scratch[start..start + (REREAD_SECTORS * SECTOR) as usize]
        .iter()
        .any(|b| *b != 0)
    {
        return Err("The second test's B destination is not all zero".into());
    }
    let neighbors = neighbor_digests(&bound_b, REREAD_BASE)?;
    // Separate VM fixture action after validating the serials and both GPTs.
    online(B)?;
    let (folder, bound_a, _, _) = fixture()?;
    for name in [REREAD_FILE, REREAD_REPORT] {
        absent(&folder.join(name))?;
        absent(&folder.join(format!("{name}.partial")))?;
    }
    let (file, windows_error) = reread_file_reference(&bound_a)?;
    let device = reread_device_reference(&bound_a)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(
        &mut output,
        &RereadWitness {
            desktop,
            tables,
            scratch: digest(&scratch),
            neighbors,
            file: digest(&file),
            device: digest(&device),
            windows_error,
        },
    )
    .map_err(err)?;
    output.sync_all().map_err(err)?;
    Ok(format!("PASS: A offline, native error {windows_error} only at LBA {FAULT_LBA}. First copy 8 sectors at A LBA {FAULT_START} backward with 2 extra skips, LOSS and reread into {}. Then copy {REREAD_SECTORS} sectors at A LBA {REREAD_SRC} forward with large-range reread and LOSS into B/7 at relative LBA {}.", folder.display(), REREAD_DEST / SECTOR))
}

fn reread_prepared() -> Result<RereadWitness, String> {
    let desktop = btrfs_checks::desktop()?;
    let baseline: RereadWitness =
        serde_json::from_reader(File::open(reread_receipt()?).map_err(err)?.take(8192))
            .map_err(err)?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    if baseline.desktop != desktop || reread_tables(&disks)? != baseline.tables {
        return Err("Disk, desktop or GPT evidence differs from the reread baseline".into());
    }
    Ok(baseline)
}

pub(super) fn reread_file_verify() -> Result<String, String> {
    let baseline = reread_prepared()?;
    let (folder, bound_a, bound_b, _) = fixture()?;
    let (expected, code) = reread_file_reference(&bound_a)?;
    if digest(&expected) != baseline.file || code != baseline.windows_error {
        return Err("Reread source no longer matches the baseline".into());
    }
    let open = |name: &str| -> Result<File, String> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder.join(name))
            .map_err(err)?;
        data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
            &file,
            &bound_b.target,
        )?])?;
        Ok(file)
    };
    for name in [REREAD_FILE, REREAD_REPORT] {
        absent(&folder.join(format!("{name}.partial")))?;
    }
    let mut actual = Vec::new();
    open(REREAD_FILE)?
        .take(FAULT_COUNT * SECTOR + 1)
        .read_to_end(&mut actual)
        .map_err(err)?;
    let report: serde_json::Value =
        serde_json::from_reader(open(REREAD_REPORT)?.take(8192)).map_err(err)?;
    let witness = json!({
        "version": 1, "startLba": FAULT_START, "sectorSize": SECTOR, "sectorCount": FAULT_COUNT,
        "direction": "backward",
        "errors": {"kind": "fill", "skipSectors": 2, "marker": "LOSS", "reread": true},
        "outputName": REREAD_FILE, "sha256": baseline.file, "elapsedMs": report["elapsedMs"],
        "failures": {"filledSectors": 1, "unreadableSectors": 1, "omitted": 0, "ranges": [
            {"failedLba": FAULT_LBA, "firstFilledLba": FAULT_LBA, "filledSectors": 1, "windowsCode": code}
        ], "reread": {"initialReadFailures": 1, "queuedSectors": 3, "recoveredSectors": 2}}
    });
    if actual != expected || !report["elapsedMs"].is_u64() || report != witness {
        return Err(
            "File or report does not match the independently read sectors and exact retry counts"
                .into(),
        );
    }
    Ok(format!("PASS: exported file and JSON report match. 3 deferred, 2 recovered, one LOSS sector. SHA-256 {}.", baseline.file))
}

pub(super) fn reread_device_verify() -> Result<String, String> {
    let baseline = reread_prepared()?;
    state(&inventory()?, B, true)?;
    let bound_a = selection(A, None)?;
    let bound_b = reread_scratch()?;
    let (_, code) = fault_reference(&bound_a)?;
    let expected = reread_device_reference(&bound_a)?;
    let mut scratch = read_span(&bound_b, REREAD_BASE, SCRATCH_BYTES)?;
    let start = REREAD_DEST as usize;
    let range = &mut scratch[start..start + expected.len()];
    if *range != expected || digest(range) != baseline.device || code != baseline.windows_error {
        return Err("B target range differs from the recorded reread source".into());
    }
    range.fill(0);
    if digest(&scratch) != baseline.scratch
        || neighbor_digests(&bound_b, REREAD_BASE)? != baseline.neighbors
    {
        return Err("Bytes outside the confirmed target range changed".into());
    }
    Ok(format!("PASS: 4 MiB target matches, only one LOSS sector. Remaining 32 MiB scratch bytes, neighbors, A/B GPT and OS layout unchanged. Both disks offline. SHA-256 {}.", baseline.device))
}

pub(super) fn reread_probe() -> Result<String, String> {
    let baseline = reread_prepared()?;
    state(&inventory()?, B, true)?;
    let bound_b = reread_scratch()?;
    let scratch = read_span(&bound_b, REREAD_BASE, SCRATCH_BYTES)?;
    if digest(&scratch) != baseline.scratch
        || neighbor_digests(&bound_b, REREAD_BASE)? != baseline.neighbors
    {
        return Err("B bytes or adjacent sectors changed after the zero-only copy".into());
    }
    let source = selection(A, None)?;
    let disk = data_windows::resolve(&source, false, false)?;
    let mut ordinary = data_windows::device(&disk, false)?;
    let mut uncached = data_windows::surface_device(&disk)?;
    #[repr(align(4096))]
    struct Block([u8; MIB as usize]);
    let mut buffer = Box::<Block>::new_uninit();
    // SAFETY: the allocation respects Block's alignment and all-zero bytes
    // constitute a valid Block.
    let mut buffer = unsafe {
        buffer.as_mut_ptr().write_bytes(0, 1);
        buffer.assume_init()
    };
    let mut normal_buffer = vec![0; MIB as usize];
    let mut results = Vec::new();
    for (mode, file, storage) in [
        ("ordinary", &mut ordinary, normal_buffer.as_mut_slice()),
        ("uncached", &mut uncached, buffer.0.as_mut_slice()),
    ] {
        for lba in [FAULT_START, FAULT_LBA] {
            let mut values = Vec::new();
            for length in [8 * SECTOR, 32 * SECTOR, 128 * SECTOR, MIB] {
                let bytes = &mut storage[..length as usize];
                bytes.fill(0xa7);
                file.seek(SeekFrom::Start(lba * SECTOR)).map_err(err)?;
                values.push(match file.read_exact(bytes) {
                    Ok(()) if bytes.iter().all(|value| *value == 0) => "zero".into(),
                    Ok(()) => "data".into(),
                    Err(error) => error
                        .raw_os_error()
                        .map_or_else(|| error.to_string(), |code| code.to_string()),
                });
            }
            results.push(format!("{mode} at {lba}: {}", values.join(", ")));
        }
    }
    Ok(format!("B scratch and neighbors match the pre-write witness. A reads (32, 128, 512, 1024 KiB):\n{}", results.join("\n")))
}

const CONTINUE_FILE: &str = "ds-sector-resume.bin";
const CONTINUE_PREFIX: u64 = 16 * MIB;
const CONTINUE_LBA: u64 = FAULT_LBA - CONTINUE_PREFIX / SECTOR;
const CONTINUE_SIZE: u64 = CONTINUE_PREFIX + FAULT_COUNT * SECTOR;
const RECOPY_ORIGINAL: &str = "ds-sector-recopy-base.bin";
const RECOPY_RESULT: &str = "ds-sector-recopy-result.bin";
const RECOPY_REPORT: &str = "ds-sector-recopy-result.json";
const CONTINUE_WITNESS: &str = "ds-sector-continuation-witness.json";

fn continuation_file(folder: &Path, name: &str, disk: &Selection) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder.join(name))
        .map_err(err)?;
    if !file.metadata().map_err(err)?.is_file() {
        return Err("Expected a regular B test output file".into());
    }
    let binding = data_windows::FileBinding::new(&file, &disk.target)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    Ok(file)
}

pub(super) fn continuation_prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    offline_fault_source()?;
    let disks = inventory()?;
    state(&disks, B, true).or_else(|_| state(&disks, B, false))?;
    let _ = reread_scratch()?;
    let before = reread_tables(&disks)?;
    online(B)?;
    let (folder, source, _, snapshot) = fixture()?;
    if reread_tables(&inventory()?)? != before {
        return Err("Fixture GPT or OS layout changed on taking B online".into());
    }
    for name in [
        CONTINUE_FILE,
        RECOPY_ORIGINAL,
        RECOPY_RESULT,
        RECOPY_REPORT,
        CONTINUE_WITNESS,
    ] {
        absent(&folder.join(name))?;
        absent(&folder.join(format!("{name}.partial")))?;
        absent(&folder.join(format!("{name}.dscopy")))?;
    }
    let (_, error) = fault_reference(&source)?;
    let prefix = read_span(&source, CONTINUE_LBA * SECTOR, CONTINUE_PREFIX)?;
    let mut witness = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(folder.join(CONTINUE_WITNESS))
        .map_err(err)?;
    serde_json::to_writer(
        &mut witness,
        &json!({
            "desktop": desktop, "snapshot": snapshot, "error": error,
            "prefix": digest(&prefix),
        }),
    )
    .map_err(err)?;
    witness.sync_all().map_err(err)?;
    Ok(format!("PASS: A is offline and has error {error} at LBA {FAULT_LBA}. Export 4104 sectors forward from A LBA {CONTINUE_LBA}, stopping on error. For filled reread, export 8 sectors backward from LBA {FAULT_START}, skip 2 and use LOSS. Outputs belong to {}, on B.", folder.display()))
}

fn continuation_evidence() -> Result<(PathBuf, Selection, Selection, serde_json::Value), String> {
    let desktop = btrfs_checks::desktop()?;
    let (folder, source, output, snapshot) = fixture()?;
    let witness: serde_json::Value =
        serde_json::from_reader(continuation_file(&folder, CONTINUE_WITNESS, &output)?.take(8192))
            .map_err(err)?;
    if witness["desktop"] != desktop || witness["snapshot"] != snapshot {
        return Err("Desktop SHA-256, A/B GPT, or guest OS layout changed".into());
    }
    Ok((folder, source, output, witness))
}

fn continuation_bytes(
    folder: &Path,
    name: &str,
    disk: &Selection,
    bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    continuation_file(folder, name, disk)?
        .take(bytes + 1)
        .read_to_end(&mut data)
        .map_err(err)?;
    if data.len() as u64 != bytes {
        return Err(format!("{name} length differs from {bytes}"));
    }
    Ok(data)
}

pub(super) fn continuation_interrupted() -> Result<String, String> {
    let (folder, source, output, witness) = continuation_evidence()?;
    let (_, error) = fault_reference(&source)?;
    absent(&folder.join(CONTINUE_FILE))?;
    let partial = continuation_bytes(
        &folder,
        &format!("{CONTINUE_FILE}.partial"),
        &output,
        CONTINUE_PREFIX,
    )?;
    let mut checkpoint = continuation_file(&folder, &format!("{CONTINUE_FILE}.dscopy"), &output)?;
    for offset in [0, 4 * MIB] {
        let mut header = [0; 12];
        checkpoint.seek(SeekFrom::Start(offset)).map_err(err)?;
        checkpoint.read_exact(&mut header).map_err(err)?;
        let size = u32::from_le_bytes(header[8..].try_into().unwrap()) as u64;
        if &header[..8] != b"DSCOPY\0\x01" || size == 0 || size > 4 * MIB - 12 {
            return Err("Missing durable checkpoint slot".into());
        }
    }
    let hash = digest(&partial);
    if witness["error"] != error || witness["prefix"] != hash {
        return Err("Partial file or source fault changed".into());
    }
    Ok(format!("PASS: Native stop retained exactly 16 MiB on B, two durable checkpoint slots, and no final .bin. SHA-256 {hash}."))
}

pub(super) fn continuation_reread() -> Result<String, String> {
    let (folder, source, output, witness) = continuation_evidence()?;
    let (expected, code) = reread_file_reference(&source)?;
    let mut original = expected.clone();
    original[SECTOR as usize..(4 * SECTOR) as usize]
        .copy_from_slice(&b"LOSS".repeat((3 * SECTOR / 4) as usize));
    let before = continuation_bytes(&folder, RECOPY_ORIGINAL, &output, FAULT_COUNT * SECTOR)?;
    let after = continuation_bytes(&folder, RECOPY_RESULT, &output, FAULT_COUNT * SECTOR)?;
    let sha = digest(&after);
    let report: serde_json::Value =
        serde_json::from_reader(continuation_file(&folder, RECOPY_REPORT, &output)?.take(8192))
            .map_err(err)?;
    if witness["error"] != code
        || before != original
        || after != expected
        || report["sha256"] != sha
        || report["version"] != 1
        || report["failures"]["filledSectors"] != 1
        || report["failures"]["ranges"][0]["failedLba"] != FAULT_LBA
        || report["failures"]["reread"]["queuedSectors"] != 3
        || report["failures"]["reread"]["recoveredSectors"] != 2
    {
        return Err(
            "Completed-copy reread file, prior export, or JSON report does not match".into(),
        );
    }
    for name in [RECOPY_ORIGINAL, RECOPY_RESULT, RECOPY_REPORT] {
        absent(&folder.join(format!("{name}.partial")))?;
    }
    Ok(format!("PASS: original file intact, new export recovered only the two readable fills, and JSON matches. SHA-256 {sha}."))
}

pub(super) fn continuation_finish() -> Result<String, String> {
    let (folder, source, output, witness) = continuation_evidence()?;
    let expected = read_span(&source, CONTINUE_LBA * SECTOR, CONTINUE_SIZE)?;
    let actual = continuation_bytes(&folder, CONTINUE_FILE, &output, CONTINUE_SIZE)?;
    absent(&folder.join(format!("{CONTINUE_FILE}.partial")))?;
    if actual != expected || witness["prefix"] != digest(&actual[..CONTINUE_PREFIX as usize]) {
        return Err("Resumed file differs from the repaired read path or committed prefix".into());
    }
    Ok(format!("PASS: resumed and published all {} bytes; the 16 MiB committed prefix and both GPTs are intact. SHA-256 {}.", actual.len(), digest(&actual)))
}

const LEGACY_FILE: &str = "ds-sector-legacy-file.bin";
const LEGACY_DEVICE_REPORT: &str = "ds-sector-legacy-device.json";
const LEGACY_DEVICE_RELATIVE: u64 = 4096;
const LEGACY_DEVICE_LBA: u64 = REREAD_BASE / SECTOR + LEGACY_DEVICE_RELATIVE;

fn legacy_reference(bound_a: &Selection) -> Result<(Vec<u8>, Vec<u8>, i32), String> {
    let (new, error) = reread_file_reference(bound_a)?;
    let mut old = new.clone();
    old[SECTOR as usize..(4 * SECTOR) as usize]
        .copy_from_slice(&b"LOSS".repeat((3 * SECTOR / 4) as usize));
    Ok((old, new, error))
}

fn legacy_outside(expected: &[u8]) -> Result<String, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    let bound_b = reread_scratch()?;
    let mut data = read_span(&bound_b, REREAD_BASE, SCRATCH_BYTES)?;
    let start = (LEGACY_DEVICE_RELATIVE * SECTOR) as usize;
    let target = &mut data[start..start + expected.len()];
    if target != expected {
        return Err(
            "B/7 at relative LBA 4096 differs from the independently expected bytes".into(),
        );
    }
    target.fill(0);
    let items = (
        digest(&data),
        neighbor_digests(&bound_b, REREAD_BASE)?,
        reread_tables(&disks)?,
    );
    Ok(digest(&serde_json::to_vec(&items).map_err(err)?))
}

pub(super) fn legacy_prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    offline_fault_source()?;
    let disks = inventory()?;
    state(&disks, B, false)?;
    let (folder, source, output, _) = fixture()?;
    let (old, _, error) = legacy_reference(&source)?;
    let prior_export = continuation_bytes(&folder, REVERSE, &output, FAULT_COUNT * SECTOR)?;
    let first_report: serde_json::Value =
        serde_json::from_reader(continuation_file(&folder, FAULT_REPORT, &output)?.take(8192))
            .map_err(err)?;
    if prior_export != old
        || first_report["version"] != 1
        || first_report["startLba"] != FAULT_START
        || first_report["sha256"] != digest(&old)
    {
        return Err("The original file or archived v1 report changed".into());
    }
    let outside = legacy_outside(&vec![0; old.len()])?;
    for leaf in [LEGACY_FILE, LEGACY_DEVICE_REPORT] {
        absent(&folder.join(leaf))?;
        absent(&folder.join(format!("{leaf}.partial")))?;
        absent(&folder.join(format!("{leaf}.dscopy")))?;
    }
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(folder.join(LEGACY_DEVICE_REPORT))
        .map_err(err)?;
    serde_json::to_writer_pretty(&mut report, &json!({
        "version": 2, "startLba": FAULT_START, "sectorSize": SECTOR, "sectorCount": FAULT_COUNT,
        "direction": "backward", "errors": {"kind": "fill", "skipSectors": 2, "marker": "LOSS"},
        "outputName": "Test B partition 7", "destinationStartLba": LEGACY_DEVICE_LBA,
        "sha256": digest(&old), "elapsedMs": 0,
        "failures": {
            "filledSectors": 3, "unreadableSectors": 1, "omitted": 0,
            "ranges": [{"failedLba": FAULT_LBA, "firstFilledLba": FAULT_LBA - 2, "filledSectors": 3, "windowsCode": error}]
        }
    })).map_err(err)?;
    report.sync_all().map_err(err)?;
    Ok(format!("PASS: archived v1 report and original file unchanged. Fresh 32 KiB at B/7 relative LBA {LEGACY_DEVICE_RELATIVE} is zero. Independent v2 report saved to B. Outside-range, neighbors and layouts SHA-256 {outside}."))
}

pub(super) fn legacy_file() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (folder, source, output, _) = fixture()?;
    let (old, new, _) = legacy_reference(&source)?;
    let before = continuation_bytes(&folder, REVERSE, &output, FAULT_COUNT * SECTOR)?;
    let current = continuation_bytes(&folder, LEGACY_FILE, &output, FAULT_COUNT * SECTOR)?;
    absent(&folder.join(format!("{LEGACY_FILE}.partial")))?;
    if before != old || current != new {
        return Err("The archived source file or imported report result differs".into());
    }
    Ok(format!("PASS: legacy v1 JSON reread recovered exactly two sectors to a new file. The original file is unchanged. New file SHA-256 {}.", digest(&current)))
}

pub(super) fn legacy_rearm() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    state(&disks, B, true)?;
    let (old, _, _) = legacy_reference(&selection(A, None)?)?;
    let outside = legacy_outside(&old)?;
    let tables = reread_tables(&disks)?;
    // Separate fixture action. No second copy is submitted from this tool.
    online(B)?;
    if reread_tables(&inventory()?)? != tables {
        return Err("B GPT or other layout changed on fixture rearm".into());
    }
    Ok(format!("PASS: independent original B/7 bytes match the v2 report. B is online solely for the next confirmed test. Outside-range, neighbors and layouts SHA-256 {outside}."))
}

pub(super) fn legacy_device() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, true)?;
    state(&disks, B, true)?;
    let (_, new, _) = legacy_reference(&selection(A, None)?)?;
    let outside = legacy_outside(&new)?;
    Ok(format!("PASS: legacy v2 disk reread recovered two sectors and kept one LOSS sector. A/B remain offline. Target SHA-256 {}. Outside-range, neighbors and layouts SHA-256 {outside}.", digest(&new)))
}

const IMAGE_SOURCE: &str = "ds-sector-source-4kn.img";
const IMAGE_OUTPUT: &str = "ds-sector-image-reverse.bin";
const IMAGE_DESTINATION: u64 = 4608;
const IMAGE_START: u64 = MIB / SECTOR + 4;

fn image_fixture() -> Vec<u8> {
    let sector = SECTOR as usize;
    let mut image = vec![0; (4 * MIB) as usize];
    let last = image.len() as u64 / SECTOR - 1;
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    image[450] = 0xee;
    image[454..458].copy_from_slice(&1u32.to_le_bytes());
    image[458..462].copy_from_slice(&(last as u32).to_le_bytes());
    let mut entries = vec![0; 16384];
    entries[..16].fill(1);
    entries[16..32].fill(2);
    entries[32..40].copy_from_slice(&(MIB / SECTOR).to_le_bytes());
    entries[40..48].copy_from_slice(&(2 * MIB / SECTOR - 1).to_le_bytes());
    for (own, other, at) in [(1, last, 2), (last, 1, last - 4)] {
        let table = at as usize * sector;
        image[table..table + entries.len()].copy_from_slice(&entries);
        let header = &mut image[own as usize * sector..][..sector];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&own.to_le_bytes());
        header[32..40].copy_from_slice(&other.to_le_bytes());
        header[40..48].copy_from_slice(&6u64.to_le_bytes());
        header[48..56].copy_from_slice(&(last - 5).to_le_bytes());
        header[56..72].fill(3);
        header[72..80].copy_from_slice(&at.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        let crc = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    for (index, byte) in image[MIB as usize..(2 * MIB) as usize].iter_mut().enumerate() {
        *byte = (index.wrapping_mul(23).wrapping_add(index / 257 + index / sector)) as u8;
    }
    image
}

fn image_folder(serial: &str) -> Result<(PathBuf, Selection), String> {
    let (label, offset, size, fixture_letter) = match serial {
        A => ("DSGLEFTKEEP", 8704, 384, "B"),
        B => ("DS4KMOVEJRN", 32, 256, "I"),
        _ => return Err("Expected an authorized test volume".into()),
    };
    let part = metadata_partition(serial, Some(label), offset, size)?;
    let bound = selection(serial, Some(part.number))?;
    let letter = part.drive_letter.as_deref().unwrap_or(fixture_letter);
    let folder = PathBuf::from(format!("{letter}:\\"));
    let resolved = data_windows::resolve(&bound, true, false)?;
    let [volume] = resolved.volumes.as_slice() else {
        return Err(format!("Expected one volume for {label}"));
    };
    let root = cap_std::fs::Dir::open_ambient_dir(&folder, cap_std::ambient_authority())
        .map_err(err)?
        .into_std_file();
    data_windows::file_location(&root)?.confirm_partition(
        &bound.target,
        &bound.layout_key,
        volume,
        (resolved.disk.number, offset * MIB, size * MIB),
    )?;
    Ok((folder, bound))
}

fn image_reference() -> Result<Vec<u8>, String> {
    let (folder, bound) = image_folder(A)?;
    let image = image_fixture();
    if continuation_bytes(&folder, IMAGE_SOURCE, &bound, image.len() as u64)? != image {
        return Err("Source image differs from its synthetic reference".into());
    }
    Ok(image[(IMAGE_START * SECTOR) as usize..((IMAGE_START + FAULT_COUNT) * SECTOR) as usize].to_vec())
}

fn image_outside(expected: &[u8], offline: bool) -> Result<String, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, false)?;
    state(&disks, B, offline)?;
    let bound = reread_scratch()?;
    let mut bytes = read_span(&bound, REREAD_BASE, SCRATCH_BYTES)?;
    let offset = (IMAGE_DESTINATION * SECTOR) as usize;
    let target = &mut bytes[offset..offset + expected.len()];
    if target != expected {
        return Err("B/7 image copy range differs from the expected bytes".into());
    }
    target.fill(0);
    Ok(digest(&serde_json::to_vec(&(
        digest(&bytes), neighbor_digests(&bound, REREAD_BASE)?, reread_tables(&disks)?,
    )).map_err(err)?))
}

pub(super) fn image_prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let tables = reread_tables(&disks)?;
    online(A)?;
    online(B)?;
    if reread_tables(&inventory()?)? != tables {
        return Err("GPT or OS layout changed on fixture startup".into());
    }
    let (source, _) = image_folder(A)?;
    let (output, _) = image_folder(B)?;
    absent(&output.join(IMAGE_OUTPUT))?;
    absent(&output.join(format!("{IMAGE_OUTPUT}.partial")))?;
    let outside = image_outside(&vec![0; (FAULT_COUNT * SECTOR) as usize], false)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(source.join(IMAGE_SOURCE)).map_err(err)?;
    file.write_all(&image_fixture()).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    let expected = digest(&image_reference()?);
    Ok(format!("PASS: {} 4Kn image partition-relative LBA 4, 8 sectors. First export to {}. Then write B/7 relative LBA {IMAGE_DESTINATION}. Expected SHA-256 {expected}. Outside SHA-256 {outside}.", source.join(IMAGE_SOURCE).display(), output.join(IMAGE_OUTPUT).display()))
}

pub(super) fn image_file() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let expected = image_reference()?;
    let (folder, bound) = image_folder(B)?;
    let copied = continuation_bytes(&folder, IMAGE_OUTPUT, &bound, expected.len() as u64)?;
    absent(&folder.join(format!("{IMAGE_OUTPUT}.partial")))?;
    if copied != expected {
        return Err("Exported file differs from the image range".into());
    }
    let outside = image_outside(&vec![0; expected.len()], false)?;
    Ok(format!("PASS: source image unchanged, file bytes and exact length match, no partial left. SHA-256 {}. Outside SHA-256 {outside}.", digest(&copied)))
}

pub(super) fn image_device() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let expected = image_reference()?;
    let outside = image_outside(&expected, true)?;
    Ok(format!("PASS: source image unchanged. B is offline. B/7 exact target range SHA-256 {}. Entire other B/7 contents, neighbor sectors, A/B GPT and OS metadata outside SHA-256 {outside}.", digest(&expected)))
}

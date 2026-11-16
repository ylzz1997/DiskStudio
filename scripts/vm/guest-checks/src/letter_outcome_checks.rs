//! Independent fixed-letter acceptance on B/15 only. Never retry B/7's failed
//! request. No formatting, deletion, online transition, or fixture overwrite.
use super::*;
use allocation_batch::{record, root, tables};
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use partitions::{Action, DriveLetter};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const FILE: &str = "DiskStudio-letter-outcome-20261117.bin";
const BASELINE: &str = r"C:\ProgramData\DiskStudio-letter-outcome-20261117.json";
const ONCE: &str = r"C:\ProgramData\DiskStudio-letter-outcome-20261117.once";
type Layout = Vec<(u32, u32, u64, u64, String)>;
type Letters = Vec<(u32, u32, Option<String>)>;

#[derive(Serialize, Deserialize)]
struct Baseline {
    gpt: Vec<String>,
    layout: Layout,
    other_letters: Letters,
    root: PathBuf,
}
fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(disk_identity::clean)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn target(expected: Option<&str>) -> Result<(Selection, Dir, PathBuf), String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B absent")?;
    if b.is_offline != Some(false) || b.is_read_only != Some(false) {
        return Err("B must already be online and writable".into());
    }
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let frozen = parts
        .iter()
        .filter(|p| p.disk_number == b.number && p.partition_number == 7)
        .collect::<Vec<_>>();
    if frozen.len() != 1
        || frozen[0].size != 2 * 1024 * 1024
        || normalized(frozen[0].drive_letter.as_deref()).is_some()
    {
        return Err("Frozen failed B/7 state changed; stop without resubmission".into());
    }
    let found = parts
        .iter()
        .filter(|p| p.disk_number == b.number && p.partition_number == 15)
        .collect::<Vec<_>>();
    if found.len() != 1 {
        return Err("Expected unique B/15 fixture".into());
    }
    let p = found[0];
    if p.offset != 13_744_734_208
        || p.size != 134_217_728
        || !p
            .guid
            .as_deref()
            .is_some_and(|g| g.eq_ignore_ascii_case("{2c922dbe-e9d2-4582-af33-36e397065795}"))
        || !partitions::policy::formattable(p)
        || normalized(p.drive_letter.as_deref()).as_deref() != expected
    {
        return Err("B/15 identity/protection/letter mismatch".into());
    }
    let s = selection(B, Some(15))?;
    let r = data_windows::resolve(&s, true, false)?;
    if r.offset != p.offset || r.size != p.size || !r.ntfs_volume {
        return Err("B/15 resolved extent/filesystem differs".into());
    }
    let snapshot = storage::read_snapshot()?;
    let local = snapshot
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&s.target.0))
        .ok_or("B snapshot missing")?;
    let part = local
        .partitions
        .iter()
        .find(|p| p.number == 15)
        .ok_or("B/15 snapshot absent")?;
    if part.base.name != "DSFMT10" || part.base.fs != "NTFS" {
        return Err("Unexpected B/15 label/filesystem".into());
    }
    let path = root(&r)?;
    let dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(err)?;
    recovery_output_checks::bind(&dir, &s)?;
    Ok((s, dir, path))
}
fn other_letters() -> Result<Letters, String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B absent")?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let mut result = parts
        .into_iter()
        .filter(|p| p.disk_number != b.number || p.partition_number != 15)
        .map(|p| {
            (
                p.disk_number,
                p.partition_number,
                normalized(p.drive_letter.as_deref()),
            )
        })
        .collect::<Letters>();
    result.sort();
    Ok(result)
}
fn bytes() -> Vec<u8> {
    (0..65536)
        .map(|i| ((i * 37 + i / 251) % 253) as u8)
        .collect()
}
fn verify(base: &Baseline, expected: Option<&str>) -> Result<String, String> {
    let (s, dir, path) = target(expected)?;
    if path != base.root
        || tables()? != base.gpt
        || format_checks::layouts(&[])? != base.layout
        || other_letters()? != base.other_letters
    {
        return Err("Volume root, GPT, layout or unrelated letter changed".into());
    }
    let mut file = dir.open(FILE).map_err(err)?.into_std();
    let binding = data_windows::FileBinding::new(&file, &s.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    if file.metadata().map_err(err)?.len() != 65536 {
        return Err("Sentinel length changed".into());
    }
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).map_err(err)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    if actual != bytes() {
        return Err("Sentinel contents changed".into());
    }
    Ok(format!("{:x}", Sha256::digest(actual)))
}
pub fn check() -> Result<String, String> {
    let mut log = backup_protection::begin("letter-outcome")?;
    let result = (|| {
        let mut once = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(ONCE)
            .map_err(err)?;
        once.write_all(b"B/15 independent A -> B -> none; B/7 never resubmitted.")
            .map_err(err)?;
        once.sync_all().map_err(err)?;
        let (s, dir, path) = target(None)?;
        let status = partitions::status();
        let t = status
            .targets
            .iter()
            .find(|t| t.serial == B)
            .ok_or("B status absent")?;
        if !["A", "B", "Z"]
            .iter()
            .all(|l| t.available_drive_letters.iter().any(|v| v == l))
        {
            return Err("Independent A/B test letters and native Z must be free".into());
        }
        let baseline = Baseline {
            gpt: tables()?,
            layout: format_checks::layouts(&[])?,
            other_letters: other_letters()?,
            root: path,
        };
        let mut saved = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(BASELINE)
            .map_err(err)?;
        saved
            .write_all(&serde_json::to_vec(&baseline).map_err(err)?)
            .map_err(err)?;
        saved.sync_all().map_err(err)?;
        let mut opts = CapOptions::new();
        opts.write(true).create_new(true);
        let mut file = dir.open_with(FILE, &opts).map_err(err)?.into_std();
        let binding = data_windows::FileBinding::new(&file, &s.target)?;
        data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
        file.write_all(&bytes()).map_err(err)?;
        file.sync_all().map_err(err)?;
        data_windows::FileBinding::validate_all(&[binding])?;
        drop(file);
        drop(dir);
        record(
            &mut log,
            serde_json::json!({"event":"fixture-passed","sha256":verify(&baseline, None)?}),
        )?;
        for letter in [Some("A"), Some("B"), None] {
            record(
                &mut log,
                serde_json::json!({"event":"letter-begin","partition":15,"letter":letter}),
            )?;
            operation(
                B,
                Action::SetDriveLetter {
                    partition: 15,
                    letter: letter.map(|s| DriveLetter(s.into())),
                },
            )?;
            let hash = verify(&baseline, letter)?;
            record(
                &mut log,
                serde_json::json!({"event":"letter-passed","partition":15,"letter":letter,"sha256":hash}),
            )?;
        }
        Ok("PASS: B/15 none -> A -> B -> none, three production operations verified; 64 KiB content/SHA-256, complete A/B GPT and all layouts/other letters unchanged after each. Failed B/7 untouched. Native UI separate.".into())
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}
// Explicit read-only follow-up after a distinct native B/15 none -> Z case.
pub fn inspect_ui() -> Result<String, String> {
    let mut log = backup_protection::begin("letter-outcome-ui-inspect")?;
    let result = (|| {
        if fs::metadata(BASELINE).map_err(err)?.len() > 65536 {
            return Err("Baseline too large".into());
        }
        let baseline =
            serde_json::from_slice::<Baseline>(&fs::read(BASELINE).map_err(err)?).map_err(err)?;
        let hash = verify(&baseline, Some("Z"))?;
        Ok(format!("PASS: native B/15 Z assignment independently verified; sentinel SHA-256 {hash}; complete A/B GPT, all partition layouts, all other drive letters unchanged; failed B/7 untouched. Read-only inspection."))
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

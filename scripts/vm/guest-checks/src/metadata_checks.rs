//! Guest-only integration tests on six newly created A/B partitions.
use super::*;
use partitions::{Action, DriveLetter, FileSystem, PartitionKind};
const SIZE: u64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename = "MSFT_PartitionToVolume")]
struct Association {}
#[derive(Deserialize)]
#[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
struct Volume {
    path: String,
    drive_letter: Option<String>,
    file_system_label: Option<String>,
    file_system: Option<String>,
}
fn volume(serial: &str, number: u32, guid: &str, offset: u64) -> Result<Volume, String> {
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or("Disk missing")?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let found = parts
        .iter()
        .filter(|p| {
            p.disk_number == d.number
                && p.partition_number == number
                && p.guid.as_deref() == Some(guid)
                && p.offset == offset
                && p.size == SIZE
                && partitions::policy::formattable(p)
        })
        .collect::<Vec<_>>();
    if found.len() != 1 {
        return Err("Metadata fixture identity mismatch".into());
    }
    let mut volumes: Vec<Volume> = con
        .associators::<Volume, Association>(&found[0].path)
        .map_err(err)?;
    if volumes.len() != 1 {
        return Err("Ambiguous fixture volume".into());
    }
    let v = volumes.remove(0);
    if !v.path.starts_with("\\\\?\\Volume{")
        || !v.path.ends_with("}\\")
        || v.path.len() != 49
        || uuid::Uuid::parse_str(&v.path[11..47]).is_err()
        || v.drive_letter
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("C"))
    {
        return Err("Unexpected fixture volume path".into());
    }
    Ok(v)
}
fn run_checks(log: &mut fs::File) -> Result<String, String> {
    let baseline = format_checks::layouts(&[])?;
    let mut created = vec![];
    let mut changes = 0;
    for serial in [A, B] {
        online(serial)?;
        for fs in [FileSystem::Ntfs, FileSystem::Exfat, FileSystem::Fat32] {
            let status = partitions::status();
            let target = status
                .targets
                .iter()
                .find(|t| t.serial == serial)
                .ok_or("Target missing")?;
            let offset = target
                .free_ranges
                .iter()
                .find(|g| g.size >= SIZE)
                .ok_or("Need 64 MiB free")?
                .offset;
            writeln!(log, "BEGIN {serial:?} {} offset={offset}", fs.name()).map_err(err)?;
            log.sync_all().map_err(err)?;
            operation(
                serial,
                Action::Create {
                    offset,
                    size: SIZE,
                    kind: PartitionKind::Windows,
                },
            )?;
            let sel = selection(serial, None)?;
            let snapshot = storage::read_snapshot()?;
            let d = snapshot
                .disks
                .iter()
                .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
                .ok_or("Disk missing")?;
            let parts = d
                .partitions
                .iter()
                .filter(|p| p.base.start == offset && p.base.size == SIZE)
                .collect::<Vec<_>>();
            if parts.len() != 1 {
                return Err("New fixture ambiguous".into());
            }
            let number = parts[0].number;
            created.push((d.hardware.number, number));
            let con = WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
                .map_err(err)?;
            let rows: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
            let guid = rows
                .iter()
                .find(|p| {
                    p.disk_number == d.hardware.number
                        && p.partition_number == number
                        && p.offset == offset
                        && p.size == SIZE
                })
                .and_then(|p| p.guid.clone())
                .ok_or("GUID missing")?;
            operation(
                serial,
                Action::Format {
                    options: Default::default(),
                    partition: number,
                    label: "DSMETA".into(),
                    file_system: fs,
                    cluster_size: 0,
                },
            )?;
            let initial = volume(serial, number, &guid, offset)?;
            if !initial
                .file_system
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case(fs.windows_name()))
            {
                return Err("Fixture filesystem mismatch".into());
            }
            let original = initial
                .drive_letter
                .as_deref()
                .map(disk_identity::clean)
                .filter(|l| l.len() == 1 && l.as_bytes()[0].is_ascii_uppercase())
                .ok_or("Initial drive letter missing")?
                .to_string();
            let path = Path::new(&initial.path).join("metadata-sentinel.bin");
            let bytes = (0..65536)
                .map(|i| ((i * 37) % 251) as u8)
                .collect::<Vec<_>>();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(err)?;
            file.write_all(&bytes).map_err(err)?;
            file.sync_all().map_err(err)?;
            drop(file);
            let status = partitions::status();
            let target = status
                .targets
                .iter()
                .find(|t| t.serial == serial)
                .ok_or("Target missing")?;
            let alternate = target
                .available_drive_letters
                .iter()
                .rev()
                .find(|l| l.as_str() != "C")
                .ok_or("No free letter")?
                .clone();
            let new_label = if fs == FileSystem::Fat32 {
                "META-FAT"
            } else {
                "资料测试"
            };
            for (action, expected_label, expected_letter) in [
                (
                    Action::SetLabel {
                        partition: number,
                        label: new_label.into(),
                    },
                    new_label,
                    Some(original.clone()),
                ),
                (
                    Action::SetLabel {
                        partition: number,
                        label: String::new(),
                    },
                    "",
                    Some(original.clone()),
                ),
                (
                    Action::SetLabel {
                        partition: number,
                        label: "DSMETA".into(),
                    },
                    "DSMETA",
                    Some(original.clone()),
                ),
                (
                    Action::SetDriveLetter {
                        partition: number,
                        letter: Some(DriveLetter(alternate.clone())),
                    },
                    "DSMETA",
                    Some(alternate),
                ),
                (
                    Action::SetDriveLetter {
                        partition: number,
                        letter: None,
                    },
                    "DSMETA",
                    None,
                ),
                (
                    Action::SetDriveLetter {
                        partition: number,
                        letter: Some(DriveLetter(original.clone())),
                    },
                    "DSMETA",
                    Some(original),
                ),
            ] {
                writeln!(
                    log,
                    "BEGIN {} partition={number}: {}",
                    fs.name(),
                    serde_json::to_string(&action).map_err(err)?
                )
                .map_err(err)?;
                log.sync_all().map_err(err)?;
                operation(serial, action)?;
                let now = volume(serial, number, &guid, offset)?;
                let actual_letter = now
                    .drive_letter
                    .as_deref()
                    .map(disk_identity::clean)
                    .filter(|l| !l.is_empty());
                if now.path != initial.path
                    || now.file_system_label.as_deref().unwrap_or("") != expected_label
                    || actual_letter != expected_letter.as_deref()
                    || now.file_system != initial.file_system
                {
                    return Err("Metadata postconditions did not match".into());
                }
                if fs::read(&path).map_err(err)? != bytes {
                    return Err("File changed during metadata operation".into());
                }
                if format_checks::layouts(&created)? != baseline {
                    return Err("Unrelated partition layout changed".into());
                }
                changes += 1;
                writeln!(log, "PASS {changes}").map_err(err)?;
                log.sync_all().map_err(err)?;
            }
            // Occupied OS letter must be rejected at preview, with no mutation.
            let status = partitions::status();
            let target = status
                .targets
                .into_iter()
                .find(|t| t.serial == serial)
                .ok_or("Target missing")?;
            if partitions::prepare(partitions::Request {
                target: target.target,
                revision: target.revision,
                action: Action::SetDriveLetter {
                    partition: number,
                    letter: Some(DriveLetter("C".into())),
                },
            })
            .is_ok()
            {
                return Err("Occupied drive letter unexpectedly accepted".into());
            }
        }
    }
    Ok(format!("PASS: {changes} metadata changes on six NEW 64 MiB A/B partitions (NTFS/exFAT/FAT32). Unicode and empty labels; change/remove/assign drive letter; every 64 KiB sentinel unchanged; occupied C rejected; old partition and OS layouts unchanged."))
}
pub fn check() -> Result<String, String> {
    inventory()?;
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(r"C:\ProgramData\DiskStudio-metadata-checks.txt")
        .map_err(|e| format!("Refuse repeated/partial metadata test: {e}"))?;
    let result = run_checks(&mut log);
    let _ = writeln!(
        log,
        "{}",
        match &result {
            Ok(s) => s.clone(),
            Err(e) => format!("FAILED: {e}"),
        }
    );
    let _ = log.sync_all();
    result
}

//! Guest-only acceptance on newly created 512 MiB partitions on A/B.
//! Existing test partitions and the guest OS layout are preserved.
use super::*;
use partitions::{Action, FileSystem, PartitionKind};
const SIZE: u64 = 512 * 1024 * 1024;

// Recover a persisted result after a guest reboot. This entry point only
// reads our ordinary ProgramData log; it never dispatches a partition job.
pub fn saved_report() -> Result<String, String> {
    inventory()?;
    let prefix = "DiskStudio-format-checks-";
    let mut logs = Vec::new();
    for entry in fs::read_dir(r"C:\ProgramData").map_err(err)? {
        let entry = entry.map_err(err)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(hash) = name
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".txt"))
        else {
            continue;
        };
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || !entry.file_type().map_err(err)?.is_file()
        {
            continue;
        }
        let metadata = entry.metadata().map_err(err)?;
        if metadata.len() > 64 * 1024 {
            return Err("Saved format log exceeds limit".into());
        }
        logs.push((
            metadata.modified().map_err(err)?,
            entry.path(),
            hash.to_string(),
        ));
        if logs.len() > 128 {
            return Err("Too many format logs; inspect manually".into());
        }
    }
    logs.sort_by_key(|l| l.0);
    let (_, path, hash) = logs.last().ok_or("No saved format report")?;
    let log = fs::read_to_string(path).map_err(err)?;
    if !log.starts_with(&format!("START {hash}\n")) {
        return Err("Saved log run identity mismatch".into());
    }
    let summary = if let Some(at) = log.rfind("ALL FORMAT CHECKS PASSED") {
        log[at..].to_owned()
    } else {
        let tail = log
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        format!("NO COMPLETED PASS RECORD; last 20 log lines:\n{tail}")
    };
    Ok(format!(
        "SAVED REPORT ONLY — no test rerun\nRun: {hash}\n\n{summary}"
    ))
}

// Read-only diagnosis of the native FAT run. All mutation dispatch stays in
// separate explicitly launched test modes; this can inspect a partial run.
pub fn native_report() -> Result<String, String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B missing")?;
    let mut logs = Vec::new();
    for entry in fs::read_dir(r"C:\ProgramData").map_err(err)? {
        let entry = entry.map_err(err)?;
        let name = entry.file_name();
        let Some(hash) = name
            .to_str()
            .and_then(|s| s.strip_prefix("DiskStudio-native-fat-checks-"))
            .and_then(|s| s.strip_suffix(".txt"))
        else {
            continue;
        };
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || !entry.file_type().map_err(err)?.is_file()
        {
            continue;
        }
        let meta = entry.metadata().map_err(err)?;
        if meta.len() > 64 * 1024 {
            return Err("Native FAT log exceeds limit".into());
        }
        logs.push((meta.modified().map_err(err)?, entry.path(), hash.to_owned()));
        if logs.len() > 128 {
            return Err("Too many native FAT logs".into());
        }
    }
    logs.sort_by_key(|l| l.0);
    let (_, path, hash) = logs.last().ok_or("No saved native FAT run")?;
    let log = fs::read_to_string(path).map_err(err)?;
    let passed = log
        .lines()
        .filter(|l| l.starts_with("Disk ") && l.ends_with("PASS"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let last = log
        .lines()
        .rev()
        .find(|l| l.starts_with("FAILED:"))
        .unwrap_or("No FAILED record");
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Letter {
        #[serde(rename = "DeviceID")]
        id: String,
    }
    let con = WMIConnection::new().map_err(err)?;
    let letters: Vec<Letter> = con
        .raw_query("SELECT DeviceID FROM Win32_LogicalDisk")
        .map_err(err)?;
    let letters = letters
        .into_iter()
        .map(|l| l.id)
        .collect::<Vec<_>>()
        .join(",");
    let snapshot = storage::read_snapshot()?;
    let b_identity = identity(b).ok_or("B identity missing")?;
    let disk = snapshot
        .disks
        .iter()
        .find(|d| {
            d.hardware.number == b.number && d.hardware.identity.as_deref() == Some(&b_identity.0)
        })
        .ok_or("B snapshot identity mismatch")?;
    let tail = disk
        .partitions
        .iter()
        .filter(|p| p.number >= 6)
        .map(|p| {
            format!(
                "B/{}: offset {}, {} bytes, FS {}, letter {:?}",
                p.number, p.base.start, p.base.size, p.base.fs, p.drive_letter
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Protection {
        #[serde(rename = "DeviceID")]
        device_id: String,
        drive_letter: Option<String>,
        boot_volume: Option<bool>,
        system_volume: Option<bool>,
        page_file_present: Option<bool>,
        file_system: Option<String>,
    }
    let resolved = data_windows::resolve(&selection(B, Some(8))?, true, false)?;
    if resolved.offset != 12903776256 || resolved.size != 134217728 || resolved.volumes.len() != 1 {
        return Err("B/8 diagnostic extent changed".into());
    }
    let legacy:Vec<Protection>=WMIConnection::new().map_err(err)?.raw_query("SELECT DeviceID,DriveLetter,BootVolume,SystemVolume,PageFilePresent,FileSystem FROM Win32_Volume").map_err(err)?;
    let matches = legacy
        .iter()
        .filter(|v| {
            v.device_id
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(resolved.volumes[0].trim_end_matches('\\'))
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("B/8 Win32 metadata not unique".into());
    }
    let v = matches[0];
    let protection = format!(
        "B/8 Win32: letter={:?}, boot={:?}, system={:?}, paging={:?}, fs={:?}",
        v.drive_letter, v.boot_volume, v.system_volume, v.page_file_present, v.file_system
    );
    Ok(format!("READ-ONLY NATIVE FAT REPORT; no rerun\nRun: {hash}\nPassed recorded: {}\n{}\n{last}\nLogical letters: {letters}\nCurrent B partitions:\n{tail}\n{protection}",passed.len(),passed.join("\n")))
}

// Diagnostic only: no format, dismount, filesystem writes or partition changes.
// Targets the preserved failed A/13 partition, after exact lab and volume checks.
pub fn lock_probe() -> Result<String, String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{
            GetVolumeInformationByHandleW, FILE_SHARE_READ, FILE_SHARE_WRITE,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        },
        System::Ioctl::{
            FSCTL_ALLOW_EXTENDED_DASD_IO, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME,
            IOCTL_DISK_GET_PARTITION_INFO_EX, PARTITION_INFORMATION_EX,
        },
    };
    #[derive(Deserialize, Debug)]
    #[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
    struct V {
        #[serde(rename = "__PATH")]
        object_path: String,
        path: String,
        file_system: Option<String>,
        size: Option<u64>,
    }
    #[derive(Deserialize)]
    #[serde(rename = "MSFT_PartitionToVolume")]
    struct Association {}
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("A missing")?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts:Vec<partitions::policy::Partition>=con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let matches = parts
        .iter()
        .filter(|p| {
            p.disk_number == d.number
                && p.partition_number == 13
                && p.size == SIZE
                && partitions::policy::formattable(p)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("Expected preserved 512 MiB A partition 13".into());
    }
    let p = matches[0];
    let volumes: Vec<V> = con.associators::<V, Association>(&p.path).map_err(err)?;
    if volumes.len() != 1 {
        return Err(format!("Before lock: {} associated volumes", volumes.len()));
    }
    let v = &volumes[0];
    if v.file_system
        .as_deref()
        .is_some_and(|s| !s.is_empty() && !s.eq_ignore_ascii_case("RAW"))
    {
        return Err("Diagnostic target already contains a filesystem".into());
    }
    let reverse: Vec<partitions::policy::Partition> = con
        .associators::<partitions::policy::Partition, Association>(&v.object_path)
        .map_err(err)?;
    if reverse.len() != 1
        || reverse[0].guid != p.guid
        || reverse[0].offset != p.offset
        || reverse[0].size != p.size
        || reverse[0].disk_number != p.disk_number
    {
        return Err("Reverse volume mismatch".into());
    }
    let path = v.path.trim_end_matches('\\');
    if path.len() != 48
        || !path.starts_with("\\\\?\\Volume{")
        || !path.ends_with('}')
        || uuid::Uuid::parse_str(&path[11..47]).is_err()
    {
        return Err("Invalid volume GUID path".into());
    }
    let sel = selection(A, Some(p.partition_number))?;
    let resolved = data_windows::resolve(&sel, true, false)?;
    let _identity = data_windows::device(&resolved, false)?;
    let preview = || -> Result<String, String> {
        let t = partitions::status()
            .targets
            .into_iter()
            .find(|t| t.serial == A)
            .ok_or("Target missing")?;
        Ok(
            match partitions::prepare(partitions::Request {
                target: t.target,
                revision: t.revision,
                action: Action::Format {
                    options: Default::default(),
                    partition: p.partition_number,
                    label: "LOCKPROBE".into(),
                    file_system: FileSystem::Fat32,
                    cluster_size: 0,
                },
            }) {
                Ok(_) => "READY (token discarded)".into(),
                Err(e) => e,
            },
        )
    };
    let before = preview()?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .map_err(err)?;
    let mut ext = [0u8; 32];
    if data_windows::control(&f, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut ext)? != 32
        || u32::from_le_bytes(ext[..4].try_into().unwrap()) != 1
        || u32::from_le_bytes(ext[8..12].try_into().unwrap()) != d.number
        || u64::from_le_bytes(ext[16..24].try_into().unwrap()) != p.offset
        || u64::from_le_bytes(ext[24..32].try_into().unwrap()) != p.size
    {
        return Err("Native extent mismatch".into());
    }
    struct Lock(fs::File);
    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = data_windows::control(&self.0, FSCTL_UNLOCK_VOLUME, &[], &mut []);
        }
    }
    data_windows::control(&f, FSCTL_LOCK_VOLUME, &[], &mut [])?;
    let locked = Lock(f);
    let while_count = con
        .associators::<V, Association>(&p.path)
        .map_err(err)?
        .len();
    let during = preview()?;
    let mut info = vec![0u8; std::mem::size_of::<PARTITION_INFORMATION_EX>()];
    let partition_control =
        data_windows::control(&locked.0, IOCTL_DISK_GET_PARTITION_INFO_EX, &[], &mut info);
    let mut name = [0u16; 261];
    let recognized = unsafe {
        GetVolumeInformationByHandleW(
            locked.0.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    let native_fs = if recognized == 0 {
        format!("ERROR {:?}", std::io::Error::last_os_error().raw_os_error())
    } else {
        let len = name
            .iter()
            .position(|c| *c == 0)
            .ok_or("Unterminated native filesystem name")?;
        format!(
            "RECOGNIZED {:?}",
            String::from_utf16(&name[..len]).map_err(err)?
        )
    };
    let dasd_control = data_windows::control(&locked.0, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []);
    drop(locked);
    let after = con
        .associators::<V, Association>(&p.path)
        .map_err(err)?
        .len();
    let report=format!("LOCK DIAGNOSTIC ONLY; no data writes\nA partition {}, offset {}, size {}\nBefore: 1 association, RAW size {:?}\nPreview before: {before}\nWhile locked: {while_count} associations\nPreview while locked: {during}\nGET_PARTITION_INFO_EX: {partition_control:?}\nNative filesystem: {native_fs}\nALLOW_EXTENDED_DASD_IO: {dasd_control:?}\nAfter unlock: {after} associations\nNo format/dismount/partition change.",p.partition_number,p.offset,p.size,v.size);
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(format!(
            "C:\\ProgramData\\DiskStudio-lock-probe-{}.txt",
            uuid::Uuid::new_v4().simple()
        ))
        .map_err(err)?;
    log.write_all(report.as_bytes()).map_err(err)?;
    log.sync_all().map_err(err)?;
    Ok(report)
}

// Diagnose the failed 2 MiB FAT12 fixture using metadata only. This never
// executes a mutation, retries a format, opens a device, or changes the marker.
pub fn volume_probe() -> Result<String, String> {
    let disks = inventory()?;
    let a = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("A missing")?;
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Access {
        disk_number: u32,
        partition_number: u32,
        access_paths: Option<Vec<String>>,
        no_default_drive_letter: Option<bool>,
    }
    #[derive(Deserialize, Debug)]
    #[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
    struct V {
        path: Option<String>,
        drive_letter: Option<String>,
        file_system: Option<String>,
        size: Option<u64>,
    }
    #[derive(Deserialize)]
    #[serde(rename = "MSFT_PartitionToVolume")]
    struct Association {}
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts:Vec<partitions::policy::Partition>=con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let found = parts
        .iter()
        .filter(|p| {
            p.disk_number == a.number
                && p.size == 2 * 1024 * 1024
                && partitions::policy::formattable(p)
        })
        .collect::<Vec<_>>();
    if found.len() != 1 {
        return Err("Expected one 2 MiB ordinary A partition".into());
    }
    let p = found[0];
    let access:Vec<Access>=con.raw_query("SELECT DiskNumber,PartitionNumber,AccessPaths,NoDefaultDriveLetter FROM MSFT_Partition").map_err(err)?;
    let access = access
        .iter()
        .find(|q| q.disk_number == p.disk_number && q.partition_number == p.partition_number)
        .ok_or("Access metadata missing")?;
    let volumes: Vec<V> = con.associators::<V, Association>(&p.path).map_err(err)?;
    let status = partitions::status();
    let t = status
        .targets
        .into_iter()
        .find(|t| t.serial == A)
        .ok_or("Target missing")?;
    let preview = partitions::prepare(partitions::Request {
        target: t.target,
        revision: t.revision,
        action: Action::Format {
            options: Default::default(),
            partition: p.partition_number,
            label: "DSFAT12".into(),
            file_system: FileSystem::Fat12,
            cluster_size: 0,
        },
    });
    let result=format!("READ-ONLY 2 MiB volume probe\nA partition {}, offset={}, GUID={:?}\nDriveLetter={:?}, NoDefaultDriveLetter={:?}\nAccessPaths={:?}\nMSFT associated volumes: {volumes:?}\nFormat PREVIEW only: {}\nNo format execution or device I/O.",p.partition_number,p.offset,p.guid,p.drive_letter,access.no_default_drive_letter,access.access_paths,match preview { Ok(_)=>"READY now (token discarded)".into(),Err(e)=>e });
    let name = format!(
        "C:\\ProgramData\\DiskStudio-volume-probe-{}.txt",
        uuid::Uuid::new_v4().simple()
    );
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(name)
        .map_err(err)?;
    log.write_all(result.as_bytes()).map_err(err)?;
    log.sync_all().map_err(err)?;
    Ok(result)
}

// Guest-only diagnostics: fixed method names, no mutation and no raw I/O.
pub fn support() -> Result<String, String> {
    let disks = inventory()?;
    let a = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("A missing")?;
    #[derive(Deserialize)]
    #[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
    struct V {
        #[serde(rename = "__PATH")]
        path: String,
    }
    #[derive(Deserialize)]
    #[serde(rename = "MSFT_PartitionToVolume")]
    struct Association {}
    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Systems {
        return_value: u32,
        supported_file_systems: Option<Vec<String>>,
    }
    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Clusters {
        return_value: u32,
        supported_cluster_sizes: Option<Vec<u32>>,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Input<'a> {
        file_system: &'a str,
    }
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let p = parts
        .iter()
        .filter(|p| {
            p.disk_number == a.number
                && p.size == SIZE
                && p.is_boot == Some(false)
                && p.is_system == Some(false)
        })
        .max_by_key(|p| p.partition_number)
        .ok_or("No new test partition")?;
    let volumes: Vec<V> = con.associators::<V, Association>(&p.path).map_err(err)?;
    if volumes.len() != 1 {
        return Err("Ambiguous volume".into());
    }
    let v = &volumes[0];
    let systems: Systems = con
        .exec_instance_method::<V, _>(&v.path, "GetSupportedFileSystems", ())
        .map_err(err)?;
    let mut lines = vec![
        format!(
            "READ-ONLY provider diagnostics, A partition {}",
            p.partition_number
        ),
        format!("{systems:?}"),
    ];
    for name in ["exFAT", "ExFAT", "EXFAT", "FAT32", "NTFS"] {
        let clusters: Clusters = con
            .exec_instance_method::<V, _>(
                &v.path,
                "GetSupportedClusterSizes",
                Input { file_system: name },
            )
            .map_err(err)?;
        lines.push(format!("{name}: {clusters:?}"));
    }
    Ok(lines.join("\n\n"))
}

pub(super) fn layouts(exclude: &[(u32, u32)]) -> Result<Vec<(u32, u32, u64, u64, String)>, String> {
    inventory()?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)?;
    let mut keys = parts
        .into_iter()
        .filter(|p| !exclude.contains(&(p.disk_number, p.partition_number)))
        .map(|p| {
            (
                p.disk_number,
                p.partition_number,
                p.offset,
                p.size,
                p.guid.unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    keys.sort();
    if keys.is_empty() {
        return Err("Partition baseline missing".into());
    }
    Ok(keys)
}
fn test_volume(
    serial: &str,
    number: u32,
    offset: u64,
    fs: FileSystem,
    label: &str,
    size: u64,
) -> Result<PathBuf, String> {
    let sel = selection(serial, Some(number))?;
    let snapshot = storage::read_snapshot()?;
    let disk = snapshot
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
        .ok_or("Missing test disk")?;
    let p = disk
        .partitions
        .iter()
        .find(|p| p.number == number)
        .ok_or("Missing test partition")?;
    if p.base.start != offset
        || p.base.size != size
        || p.base.system
        || !p.base.fs.eq_ignore_ascii_case(fs.windows_name())
        || p.base.name != label
    {
        return Err("Post-format metadata mismatch".into());
    }
    // A growing set of preserved test partitions can exhaust drive letters.
    // Bind the ordinary file roundtrip to the provider's unique volume GUID,
    // with the production identity/layout and bidirectional association checks.
    let resolved = data_windows::resolve(&sel, true, false)?;
    if resolved
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(serial)
        || resolved.disk.number != disk.hardware.number
        || resolved.disk.is_system != Some(false)
        || resolved.disk.is_boot != Some(false)
        || resolved.disk.is_read_only != Some(false)
        || resolved.disk.is_offline != Some(false)
        || resolved.offset != offset
        || resolved.size != size
        || resolved.volumes.len() != 1
    {
        return Err("Test volume GUID binding changed or is protected".into());
    }
    Ok(PathBuf::from(format!("{}\\", resolved.volumes[0])))
}
pub fn check() -> Result<String, String> {
    inventory()?;
    use sha2::{Digest, Sha256};
    let hash = format!(
        "{:x}",
        Sha256::digest(fs::read(std::env::current_exe().map_err(err)?).map_err(err)?)
    );
    let name = format!("C:\\ProgramData\\DiskStudio-format-checks-{hash}.txt");
    let mut state = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&name)
        .map_err(|e| format!("Refuse repeat/partial format run of this build: {e}"))?;
    writeln!(
        state,
        "START {hash}\nOnly new partitions on A/B; no automatic retries."
    )
    .map_err(err)?;
    state.sync_all().map_err(err)?;
    let result = check_run(&mut state);
    let _ = writeln!(
        state,
        "{}",
        match &result {
            Ok(s) => s.clone(),
            Err(e) => format!("FAILED: {e}"),
        }
    );
    let _ = state.sync_all();
    result.map(|s| format!("{s}\n\nReport: {name}"))
}
fn check_run(state: &mut fs::File) -> Result<String, String> {
    inventory()?;
    let baseline = layouts(&[])?;
    let mut created = Vec::new();
    let mut report = Vec::new();
    for serial in [A, B] {
        online(serial)?;
        let status = partitions::status();
        let t = status
            .targets
            .iter()
            .find(|t| t.serial == serial)
            .ok_or("Missing A/B target")?;
        let gap = t
            .free_ranges
            .iter()
            .find(|g| g.size >= SIZE)
            .ok_or("Need 512 MiB of unallocated space")?;
        let offset = gap.offset;
        writeln!(state, "CREATE {serial}: offset={offset}, size={SIZE}").map_err(err)?;
        state.sync_all().map_err(err)?;
        operation(
            serial,
            Action::Create {
                offset,
                size: SIZE,
                kind: PartitionKind::Windows,
            },
        )?;
        let disks = inventory()?;
        let disk_number = disks
            .iter()
            .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
            .ok_or("Test disk missing")?
            .number;
        let snap = storage::read_snapshot()?;
        let d = snap
            .disks
            .iter()
            .find(|d| d.hardware.number == disk_number)
            .ok_or("Snapshot missing")?;
        let matches: Vec<_> = d
            .partitions
            .iter()
            .filter(|p| p.base.start == offset && p.base.size == SIZE)
            .collect();
        if matches.len() != 1 {
            return Err("Created partition identity is ambiguous".into());
        }
        let number = matches[0].number;
        created.push((disk_number, number));
        for (fs, cluster, label) in [
            (FileSystem::Fat32, 0, "DSFATDEF"),
            (FileSystem::Fat32, 4096, "DSFAT4K"),
            (FileSystem::Exfat, 0, "资料EXFAT"),
            (FileSystem::Exfat, 131072, "DSEX128K"),
            (FileSystem::Ntfs, 0, "DSNTFSDEF"),
            (FileSystem::Ntfs, 65536, "DSNTFS64K"),
        ] {
            writeln!(state, "FORMAT {serial} disk={disk_number}, partition={number}, offset={offset}, {} / {cluster} B", fs.name()).map_err(err)?;
            state.sync_all().map_err(err)?;
            operation(
                serial,
                Action::Format {
                    options: Default::default(),
                    partition: number,
                    label: label.into(),
                    file_system: fs,
                    cluster_size: cluster,
                },
            ).map_err(|e| format!("Disk {disk_number} partition {number}, offset {offset}, {} / {cluster} B: {e}\nPassed so far:\n{}", fs.name(), report.join("\n")))?;
            let root = test_volume(serial, number, offset, fs, label, SIZE)?;
            let path = root.join("diskstudio-format-check.bin");
            if path.exists() {
                return Err("Prior format sample survived formatting".into());
            }
            let bytes: Vec<u8> = (0..512 * 1024)
                .map(|i| ((i * 37 + cluster as usize) % 251) as u8)
                .collect();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(err)?;
            file.write_all(&bytes).map_err(err)?;
            file.sync_all().map_err(err)?;
            drop(file);
            if fs::read(&path).map_err(err)? != bytes {
                return Err("Formatted volume file round-trip mismatch".into());
            }
            if layouts(&created)? != baseline {
                return Err("An unrelated partition layout changed".into());
            }
            report.push(format!(
                "Disk {disk_number} partition {number}: {} / {cluster} B PASS",
                fs.name()
            ));
            writeln!(
                state,
                "{}; 512 KiB file matched; prior layouts unchanged",
                report.last().unwrap()
            )
            .map_err(err)?;
            state.sync_all().map_err(err)?;
        }
    }
    Ok(format!("ALL FORMAT CHECKS PASSED\n\n{}\n\n12 production format operations; default/custom clusters; UTF-16 exFAT label; every file round-trip matched. Guest OS and all pre-existing layouts unchanged. Two new 512 MiB NTFS partitions remain for production UI acceptance. No old A/B partition was formatted.", report.join("\n")))
}

// Explicit continuation after read-only diagnosis of the ce9824 run. B/8
// already exists: it is NEVER created again. Each format below is a new case.
pub fn native_finish() -> Result<String, String> {
    inventory()?;
    let previous = fs::read_to_string(r"C:\ProgramData\DiskStudio-native-fat-checks-ce9824a282d07e70c0244c1035d20a6e8e7bc0582d0c3d41b5e2a255aa9f83f7.txt").map_err(err)?;
    if previous
        .lines()
        .filter(|l| l.starts_with("Disk ") && l.ends_with("PASS"))
        .count()
        != 8
        || !previous.contains("offset=12903776256, size=134217728, intended=FAT16: EXECUTE:")
        || !previous.contains("40002")
        || previous.contains("ALL NATIVE FAT CHECKS PASSED")
    {
        return Err("Original partial run does not match the diagnosed state".into());
    }
    let baseline = layouts(&[])?;
    let sel = selection(B, Some(8))?;
    let snap = storage::read_snapshot()?;
    let disk = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
        .ok_or("B missing")?;
    let p = disk
        .partitions
        .iter()
        .find(|p| p.number == 8)
        .ok_or("B/8 missing")?;
    if p.base.start != 12903776256
        || p.base.size != 134217728
        || p.drive_letter.is_some()
        || p.base.system
    {
        return Err(format!("Preserved B/8 geometry/protection/letter changed: offset={}, size={}, fs={:?}, letter={:?}, system={}",p.base.start,p.base.size,p.base.fs,p.drive_letter,p.base.system));
    }
    // Snapshot display used to leave letterless filesystems unknown. Do not
    // interpret that as RAW: explicitly inspect the already bound guest volume.
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            GetVolumeInformationByHandleW, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let resolved = data_windows::resolve(&sel, true, false)?;
        if resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(B)
            || resolved.offset != 12903776256
            || resolved.size != 134217728
            || resolved.volumes.len() != 1
        {
            return Err("B/8 volume identity mismatch".into());
        }
        let f = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&resolved.volumes[0])
            .map_err(err)?;
        let mut fs_name = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                f.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        let fs_name = String::from_utf16_lossy(
            &fs_name[..fs_name
                .iter()
                .position(|x| *x == 0)
                .unwrap_or(fs_name.len())],
        );
        if (ok == 0 && std::io::Error::last_os_error().raw_os_error() != Some(1005))
            || (ok != 0 && !fs_name.eq_ignore_ascii_case("RAW"))
        {
            return Err(format!(
                "B/8 is not confirmed RAW: {fs_name:?}, {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let disk_number = disk.hardware.number;
    let refused = fs::read_to_string(r"C:\ProgramData\DiskStudio-native-fat-finish-20261112.txt")
        .map_err(err)?;
    if !refused.contains("FAILED: PREPARE: 目标卷可能承载分页文件，已拒绝操作。")
        || refused.contains("PASS")
        || refused.contains("BEGIN one new")
    {
        return Err(
            "Previous continuation is not the diagnosed pre-execution paging refusal".into(),
        );
    }
    let guarded =
        fs::read_to_string(r"C:\ProgramData\DiskStudio-native-fat-finish-guarded-20261112.txt")
            .map_err(err)?;
    if !guarded
        .contains("FAILED: PREPARE: 无盘符目标卷可能为系统、启动或分页文件卷，或其保护信息不完整。")
        || guarded.contains("PASS")
        || guarded.contains("BEGIN one new")
    {
        return Err(
            "Previous guarded continuation was not the diagnosed pre-execution refusal".into(),
        );
    }
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(r"C:\ProgramData\DiskStudio-native-fat-finish-resolved-20261112.txt")
        .map_err(|e| format!("Refuse repeated/partial continuation: {e}"))?;
    writeln!(log,"START explicit continuation: B/8 first formatting; one new B FAT32 partition; no repeat creation").map_err(err)?;
    log.sync_all().map_err(err)?;
    let result = (|| {
        let mut excluded = vec![];
        let mut report = vec![];
        for (system, size, explicit) in [
            (FileSystem::Fat16, 134217728, 4096),
            (FileSystem::Fat32, SIZE, 1024),
        ] {
            let (number, offset) = if system == FileSystem::Fat16 {
                (8, 12903776256)
            } else {
                let status = partitions::status();
                let t = status
                    .targets
                    .iter()
                    .find(|t| t.serial == B)
                    .ok_or("B target missing")?;
                if t.available_drive_letters
                    .iter()
                    .any(|l| l.len() == 1 && (b'C'..=b'Z').contains(&l.as_bytes()[0]))
                {
                    return Err("C-Z are no longer exhausted; separate acceptance needed".into());
                }
                let offset = t
                    .free_ranges
                    .iter()
                    .find(|r| r.size >= size)
                    .ok_or("B has no free extent")?
                    .offset;
                writeln!(log,"BEGIN one new FAT32 partition offset={offset}, size={size}; no automatic drive letter available").map_err(err)?;
                log.sync_all().map_err(err)?;
                operation(
                    B,
                    Action::Create {
                        offset,
                        size,
                        kind: PartitionKind::Windows,
                    },
                )?;
                let snap = storage::read_snapshot()?;
                let disk = snap
                    .disks
                    .iter()
                    .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
                    .ok_or("B missing after create")?;
                let parts = disk
                    .partitions
                    .iter()
                    .filter(|p| {
                        p.base.start == offset
                            && p.base.size == size
                            && !p.base.system
                            && p.drive_letter.is_none()
                    })
                    .collect::<Vec<_>>();
                if parts.len() != 1 {
                    return Err("New letterless FAT32 target ambiguous".into());
                }
                excluded.push((disk_number, parts[0].number));
                (parts[0].number, offset)
            };
            for cluster in [0, explicit] {
                writeln!(
                    log,
                    "BEGIN B/{number} {}, offset={offset}, size={size}, cluster={cluster}",
                    system.name()
                )
                .map_err(err)?;
                log.sync_all().map_err(err)?;
                let label = format!("DS{}", system.name());
                operation(
                    B,
                    Action::Format {
                        options: Default::default(),
                        partition: number,
                        label: label.clone(),
                        file_system: system,
                        cluster_size: cluster,
                    },
                )?;
                let root = test_volume(B, number, offset, system, &label, size)?;
                let path = root.join("native-fat-letterless.bin");
                if path.exists() {
                    return Err("Previous test file survived format".into());
                }
                let bytes = (0..512 * 1024)
                    .map(|i| ((i * 37 + cluster as usize) % 251) as u8)
                    .collect::<Vec<_>>();
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(err)?;
                f.write_all(&bytes).map_err(err)?;
                f.sync_all().map_err(err)?;
                drop(f);
                if fs::read(&path).map_err(err)? != bytes {
                    return Err("Letterless FAT file roundtrip mismatch".into());
                }
                if layouts(&excluded)? != baseline {
                    return Err("Pre-existing layout changed".into());
                }
                let line = format!(
                    "B/{number}: {} / {} MiB / {cluster} B: PASS",
                    system.name(),
                    size / 1024 / 1024
                );
                writeln!(log, "{line}").map_err(err)?;
                log.sync_all().map_err(err)?;
                report.push(line);
            }
        }
        Ok(format!("FOUR REMAINING NATIVE FAT CASES PASSED\n{}\nB/8 was not recreated. New FAT32 partition created without a drive letter. All old layouts and OS unchanged.",report.join("\n")))
    })();
    match &result {
        Ok(s) => writeln!(log, "{s}"),
        Err(e) => writeln!(log, "FAILED: {e}"),
    }
    .map_err(err)?;
    log.sync_all().map_err(err)?;
    result
}

pub fn native_check() -> Result<String, String> {
    inventory()?;
    // Keep historical failed runs intact. Each new build gets one explicitly
    // launched run on newly allocated extents, never a retry on a failed target.
    use sha2::{Digest, Sha256};
    let hash = format!(
        "{:x}",
        Sha256::digest(fs::read(std::env::current_exe().map_err(err)?).map_err(err)?)
    );
    let marker = format!("C:\\ProgramData\\DiskStudio-native-fat-checks-{hash}.txt");
    let mut state = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|e| format!("Refuse duplicate/partial native FAT run: {e}"))?;
    state
        .write_all(b"Started. Do not rerun automatically.\r\n")
        .map_err(err)?;
    state.sync_all().map_err(err)?;
    let result = native_run(&mut state);
    if let Err(e) = &result {
        // Preserve the original failure even when the diagnostic file cannot
        // be flushed. The marker still forbids silently rerunning the test.
        let _ = writeln!(state, "FAILED: {e}");
        let _ = state.sync_all();
    }
    result.map(|report| format!("{report}\n\nReport: {marker}"))
}

fn native_run(state: &mut fs::File) -> Result<String, String> {
    let baseline = layouts(&[])?;
    let mut created = vec![];
    let mut report = vec![];
    for serial in [A, B] {
        online(serial)?;
        for (system, size, explicit) in [
            (FileSystem::Fat12, 2 * 1024 * 1024, 4096),
            (FileSystem::Fat16, 128 * 1024 * 1024, 4096),
            (FileSystem::Fat32, SIZE, 1024),
        ] {
            let status = partitions::status();
            let t = status
                .targets
                .iter()
                .find(|t| t.serial == serial)
                .ok_or("Missing target")?;
            let offset = t
                .free_ranges
                .iter()
                .find(|r| r.size >= size)
                .ok_or("Insufficient free space")?
                .offset;
            let creation = format!(
                "CREATE serial={serial:?}, offset={offset}, size={size}, intended={}",
                system.name()
            );
            writeln!(state, "BEGIN {creation}").map_err(err)?;
            state.sync_all().map_err(err)?;
            operation(
                serial,
                Action::Create {
                    offset,
                    size,
                    kind: PartitionKind::Windows,
                },
            )
            .map_err(|e| format!("{creation}: {e}"))?;
            let disk_number = inventory()?
                .iter()
                .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
                .ok_or("Missing test disk")?
                .number;
            let snapshot = storage::read_snapshot()?;
            let disk = snapshot
                .disks
                .iter()
                .find(|d| d.hardware.number == disk_number)
                .ok_or("Missing snapshot")?;
            let matches = disk
                .partitions
                .iter()
                .filter(|p| p.base.start == offset && p.base.size == size)
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err("New native FAT partition ambiguous".into());
            }
            let number = matches[0].number;
            created.push((disk_number, number));
            for cluster in [0, explicit] {
                let label = format!("DS{}", system.name());
                let context = format!(
                    "FORMAT serial={serial:?}, disk={disk_number}, partition={number}, offset={offset}, size={size}, filesystem={}, cluster={cluster}",
                    system.name()
                );
                writeln!(state, "BEGIN {context}").map_err(err)?;
                state.sync_all().map_err(err)?;
                operation(
                    serial,
                    Action::Format {
                        options: Default::default(),
                        partition: number,
                        label: label.clone(),
                        file_system: system,
                        cluster_size: cluster,
                    },
                )
                .map_err(|e| format!("{context}: {e}"))?;
                let root = test_volume(serial, number, offset, system, &label, size)
                    .map_err(|e| format!("{context}: {e}"))?;
                let path = root.join("native-fat-cross-cluster.bin");
                if path.exists() {
                    return Err("Prior file survived native formatting".into());
                }
                let bytes = (0..512 * 1024)
                    .map(|i| ((i * 37 + cluster as usize) % 251) as u8)
                    .collect::<Vec<_>>();
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(err)?;
                f.write_all(&bytes).map_err(err)?;
                f.sync_all().map_err(err)?;
                drop(f);
                if fs::read(&path).map_err(err)? != bytes {
                    return Err("Native FAT file roundtrip mismatch".into());
                }
                if layouts(&created)? != baseline {
                    return Err("Unrelated partition layout changed".into());
                }
                let line = format!(
                    "Disk {disk_number} partition {number}, {} / {} MiB / {cluster} B: PASS",
                    system.name(),
                    size / 1024 / 1024
                );
                writeln!(state, "{line}").map_err(err)?;
                state.sync_all().map_err(err)?;
                report.push(line);
            }
        }
    }
    let result=format!("ALL NATIVE FAT CHECKS PASSED\n\n{}\n\n12 format/file roundtrips on six NEW partitions. Pre-existing layouts and OS unchanged. Larger-than-32-GiB FAT32 still requires separate volume acceptance; these 16 GiB disks cannot prove that boundary.",report.join("\n"));
    writeln!(state, "{result}").map_err(err)?;
    state.sync_all().map_err(err)?;
    Ok(result)
}

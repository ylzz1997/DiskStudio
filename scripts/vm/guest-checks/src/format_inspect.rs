//! Read-only diagnosis of the frozen format-options failure. No Format method,
//! volume lock, dismount, attribute change, file creation on B or raw reads.
use super::*;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::{
    Foundation::GENERIC_READ,
    Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    },
    System::Ioctl::{
        FSCTL_QUERY_PERSISTENT_VOLUME_STATE, IOCTL_DISK_GET_PARTITION_INFO_EX,
        PARTITION_INFORMATION_EX, PERSISTENT_VOLUME_STATE_SHORT_NAME_CREATION_DISABLED,
    },
};

const FAILED: &str = "752da6a3356d69ea0090c35d7ba2b08f57e53209ca05a384e8c672008670f6dc";
const DESKTOP: &str = "44bfbc2094dd39f4f258fbb5de9005a9a56dace5af5729b2b0a91bb7289473d8";

pub(super) fn frozen_fixture() -> Result<Fixture, String> {
    inventory()?;
    let path = format!(r"C:\ProgramData\DiskStudio-format-options-{FAILED}.jsonl");
    if fs::metadata(&path).map_err(err)?.len() > 512 * 1024 {
        return Err("Frozen report too large".into());
    }
    let text = fs::read_to_string(path).map_err(err)?;
    let events = text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let first = events.first().ok_or("Missing failure evidence")?;
    let last = events.last().ok_or("Missing failure evidence")?;
    let created: Vec<_> = events.iter().filter(|e| e["event"] == "created").collect();
    let begins: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "format-begin")
        .collect();
    if first["event"] != "start"
        || first["desktop"] != DESKTOP
        || first["helper"] != FAILED
        || last["event"] != "failed"
        || !last["error"]
            .as_str()
            .unwrap_or("")
            .ends_with("(os error 1)")
        || created.len() != 5
        || begins.len() != 9
        || events
            .iter()
            .filter(|e| e["event"] == "case-passed")
            .count()
            != 8
        || begins[8]["fs"] != "NTFS"
        || begins[8]["partition"] != 15
        || begins[8]["options"]["mode"] != "quick"
    {
        return Err("Frozen failure sequence differs; no device opened".into());
    }
    let e = created[4];
    let fixture = Fixture {
        number: 15,
        offset: 13_744_734_208,
        size: 128 * MIB,
        guid: "{2c922dbe-e9d2-4582-af33-36e397065795}".into(),
    };
    if e["partition"] != fixture.number
        || e["offset"] != fixture.offset
        || e["size"] != fixture.size
        || e["guid"] != fixture.guid
    {
        return Err("Frozen NTFS fixture differs".into());
    }
    Ok(fixture)
}
pub(super) fn check() -> Result<String, String> {
    let fixture = frozen_fixture()?;
    let mut log = backup_protection::begin("format-inspect")?;
    let result = (|| {
        let r = resolve(&fixture).map_err(|e| format!("resolve fixture: {e}"))?;
        if !r.ntfs_volume || r.volumes.len() != 1 {
            return Err("Fixture is not NTFS".into());
        }
        let mut lines = vec![
            "READ-ONLY: exact frozen B NTFS fixture; eight preceding cases passed in saved log."
                .into(),
        ];
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        for access in [FILE_READ_ATTRIBUTES, GENERIC_READ] {
            let root = OpenOptions::new()
                .access_mode(access)
                .share_mode(share)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(allocation_batch::root(&r)?)
                .map_err(|e| format!("root open {access}: {e}"))?;
            let location = data_windows::file_location(&root)
                .map_err(|e| format!("root identity {access}: {e}"))?;
            if location.target != selection(B, Some(fixture.number))?.target {
                return Err("Root disk differs".into());
            }
            let mut bytes = [0u8; 2];
            let response = data_windows::control(&root, FSCTL_GET_COMPRESSION, &[], &mut bytes);
            lines.push(format!(
                "root access={access:#x}: attrs={:#x}, compression={response:?}, bytes={bytes:?}",
                root.metadata().map_err(err)?.file_attributes()
            ));
            if data_windows::file_location(&root)? != location {
                return Err("Root binding changed".into());
            }
        }
        for access in [0, GENERIC_READ] {
            let volume = OpenOptions::new()
                .access_mode(access)
                .share_mode(share)
                .open(&r.volumes[0])
                .map_err(|e| format!("volume open {access}: {e}"))?;
            let mut extent = [0u8; 32];
            let n = data_windows::control(
                &volume,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                &[],
                &mut extent,
            )
            .map_err(|e| format!("volume extent {access}: {e}"))?;
            if n != 32
                || u32::from_le_bytes(extent[..4].try_into().unwrap()) != 1
                || u32::from_le_bytes(extent[8..12].try_into().unwrap()) != r.disk.number
                || u64::from_le_bytes(extent[16..24].try_into().unwrap()) != fixture.offset
                || u64::from_le_bytes(extent[24..32].try_into().unwrap()) != fixture.size
            {
                return Err("Native extent differs".into());
            }
            let mut partition = vec![0; std::mem::size_of::<PARTITION_INFORMATION_EX>()];
            let p = data_windows::control(
                &volume,
                IOCTL_DISK_GET_PARTITION_INFO_EX,
                &[],
                &mut partition,
            );
            let mut input = [0u8; 16];
            input[4..8].copy_from_slice(
                &PERSISTENT_VOLUME_STATE_SHORT_NAME_CREATION_DISABLED.to_le_bytes(),
            );
            input[8..12].copy_from_slice(&1u32.to_le_bytes());
            let mut state = [0u8; 16];
            let s = data_windows::control(
                &volume,
                FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
                &input,
                &mut state,
            );
            lines.push(format!("volume access={access:#x}: extent=matched, partition={p:?}, persistent={s:?}, state={state:?}"));
        }
        resolve(&fixture)?;
        Ok(lines.join("\n\n"))
    })();
    record(
        &mut log,
        serde_json::json!({"event":"read-only-result","result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

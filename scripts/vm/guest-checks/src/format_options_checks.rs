//! Guest-only consolidated formatting acceptance. Only NEW GPT data partitions
//! on B are formatted; existing fixtures and the OS layout are preserved.
use super::*;
use allocation_batch::{record, set_online, tables};
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use partitions::{
    format_options::{FormatMode, FormatOptions},
    Action, FileSystem, PartitionKind,
};
use std::{io::Read, os::windows::fs::MetadataExt};
use windows_sys::Win32::{
    Storage::FileSystem::FILE_ATTRIBUTE_COMPRESSED, System::Ioctl::FSCTL_GET_COMPRESSION,
};

const MIB: u64 = 1024 * 1024;
const SENTINEL: &str = "Long format acceptance content.bin";
#[path = "format_inspect.rs"]
mod diagnostics;
pub fn inspect() -> Result<String, String> {
    diagnostics::check()
}

// The failed Format was already submitted. First verify its existing result;
// never resubmit that request. Only then run the two remaining distinct cases.
pub fn continue_check() -> Result<String, String> {
    let fixture = diagnostics::frozen_fixture()?;
    let mut log = backup_protection::begin("format-options-continue")?;
    let result = (|| {
        let mut once = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(r"C:\ProgramData\DiskStudio-format-options-continue-752da6a3.once")
            .map_err(|e| format!("Continuation already dispatched or marker unavailable: {e}"))?;
        once.write_all(
            allocation_batch::hash_file(&std::env::current_exe().map_err(err)?)?.as_bytes(),
        )
        .map_err(err)?;
        once.sync_all().map_err(err)?;
        let gpt = tables()?;
        let layouts = format_checks::layouts(&[])?;
        verify_existing_ntfs(&fixture, FormatOptions::default())?;
        // Complete the independent file check that was never reached after the
        // original query failure. Its create_new sentinel must still be absent.
        let hash = roundtrip(&fixture, FileSystem::Ntfs, "DSFMT08", false)?;
        if tables()? != gpt || format_checks::layouts(&[])? != layouts {
            return Err("Layout changed while checking original NTFS result".into());
        }
        record(
            &mut log,
            serde_json::json!({"event":"original-result-verified","partition":fixture.number,"sha256":hash,"formatResubmitted":false,"previousHelper":"752da6a3356d69ea0090c35d7ba2b08f57e53209ca05a384e8c672008670f6dc"}),
        )?;
        for (mode, compressed, short_names, cluster, label) in [
            (FormatMode::Full, true, true, 4096, "DSFMT09"),
            (FormatMode::Quick, false, false, 65536, "DSFMT10"),
        ] {
            let options = FormatOptions {
                mode,
                ntfs_compression: compressed,
                ntfs_short_names: short_names,
            };
            resolve(&fixture)?;
            record(
                &mut log,
                serde_json::json!({"event":"format-begin","partition":fixture.number,"options":options,"cluster":cluster}),
            )?;
            let message = perform(Action::Format {
                partition: fixture.number,
                label: label.into(),
                file_system: FileSystem::Ntfs,
                cluster_size: cluster,
                options,
            })?;
            let hash = roundtrip(&fixture, FileSystem::Ntfs, label, compressed)?;
            if tables()? != gpt || format_checks::layouts(&[])? != layouts {
                return Err("GPT or layout changed during remaining NTFS case".into());
            }
            record(
                &mut log,
                serde_json::json!({"event":"case-passed","partition":fixture.number,"options":options,"cluster":cluster,"sha256":hash,"result":message}),
            )?;
        }
        Ok("PASS: previously submitted NTFS quick-format result verified without resubmission; two remaining full/compressed/short-name and quick/64KiB/uncompressed cases passed, inherited file compression checked, 524305-byte readbacks matched, GPT/all current layouts unchanged. Earlier eight FAT/exFAT cases remain evidence of original 44bfbc20 build. No retry or rollback. Native desktop UI acceptance separate.".into())
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

fn verify_existing_ntfs(fixture: &Fixture, options: FormatOptions) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let r = resolve(fixture)?;
    if !r.ntfs_volume || r.volumes.len() != 1 {
        return Err("Expected existing NTFS fixture".into());
    }
    let root = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(allocation_batch::root(&r)?)
        .map_err(err)?;
    let location = data_windows::file_location(&root)?;
    if location.target != selection(B, Some(fixture.number))?.target {
        return Err("Root disk differs".into());
    }
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&r.volumes[0])
        .map_err(err)?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query(partitions::policy::Partition::WMI_SELECT).map_err(err)?;
    let p = parts
        .iter()
        .find(|p| p.disk_number == r.disk.number && p.partition_number == fixture.number)
        .ok_or("Fixture missing")?;
    if p.offset != fixture.offset
        || p.size != fixture.size
        || p.guid.as_deref() != Some(&fixture.guid)
    {
        return Err("Fixture identity changed".into());
    }
    fat_volume::verify_extent(&volume, &r.disk, p)?;
    partitions::platform::format_windows::verify_handles(&root, &volume, options)?;
    fat_volume::verify_extent(&volume, &r.disk, p)?;
    if data_windows::file_location(&root)? != location {
        return Err("Root binding changed".into());
    }
    resolve(fixture)?;
    Ok(())
}
struct Fixture {
    number: u32,
    offset: u64,
    size: u64,
    guid: String,
}

fn perform(action: Action) -> Result<String, String> {
    inventory()?;
    let status = partitions::status();
    let target = status
        .targets
        .into_iter()
        .find(|t| t.serial == B)
        .ok_or("B target absent")?;
    let plan = partitions::prepare(partitions::Request {
        target: target.target,
        revision: target.revision,
        action,
    })?;
    let result = partitions::execute(plan.token)?;
    if !result.verified {
        return Err(result.message);
    }
    Ok(result.message)
}
fn resolve(fixture: &Fixture) -> Result<data_windows::Resolved, String> {
    inventory()?;
    let r = data_windows::resolve(&selection(B, Some(fixture.number))?, true, false)?;
    if r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(B)
        || r.disk.is_offline != Some(false)
        || r.offset != fixture.offset
        || r.size != fixture.size
    {
        return Err("New B fixture binding changed".into());
    }
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let parts: Vec<partitions::policy::Partition> = con.raw_query(partitions::policy::Partition::WMI_SELECT).map_err(err)?;
    let found: Vec<_> = parts
        .iter()
        .filter(|p| p.disk_number == r.disk.number && p.partition_number == fixture.number)
        .collect();
    if found.len() != 1
        || found[0].guid.as_deref() != Some(&fixture.guid)
        || found[0].offset != fixture.offset
        || found[0].size != fixture.size
    {
        return Err("New fixture GPT identity changed".into());
    }
    Ok(r)
}
fn new_fixture(log: &mut fs::File, size: u64) -> Result<Fixture, String> {
    inventory()?;
    let status = partitions::status();
    let target = status
        .targets
        .iter()
        .find(|t| t.serial == B)
        .ok_or("B target absent")?;
    let offset = target
        .free_ranges
        .iter()
        .find(|g| g.size >= size)
        .ok_or("Insufficient unallocated B space; preserve old fixtures")?
        .offset;
    record(
        log,
        serde_json::json!({"event":"create-begin","serial":B,"offset":offset,"size":size}),
    )?;
    perform(Action::Create {
        offset,
        size,
        kind: PartitionKind::Windows,
    })?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let disk_number = inventory()?
        .into_iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B absent")?
        .number;
    let parts: Vec<partitions::policy::Partition> = con.raw_query(partitions::policy::Partition::WMI_SELECT).map_err(err)?;
    let found: Vec<_> = parts
        .into_iter()
        .filter(|p| p.disk_number == disk_number && p.offset == offset && p.size == size)
        .collect();
    if found.len() != 1 {
        return Err("New partition absent or ambiguous; do not repeat create".into());
    }
    let p = &found[0];
    let fixture = Fixture {
        number: p.partition_number,
        offset,
        size,
        guid: p.guid.clone().ok_or("New GUID absent")?,
    };
    record(
        log,
        serde_json::json!({"event":"created","disk":disk_number,"partition":fixture.number,"offset":offset,"size":size,"guid":fixture.guid}),
    )?;
    resolve(&fixture)?;
    Ok(fixture)
}
fn roundtrip(
    fixture: &Fixture,
    fs: FileSystem,
    label: &str,
    compressed: bool,
) -> Result<String, String> {
    let r = resolve(fixture)?;
    let snapshot = storage::read_snapshot()?;
    let disk = snapshot
        .disks
        .iter()
        .find(|d| d.hardware.number == r.disk.number)
        .ok_or("B metadata absent")?;
    let p = disk
        .partitions
        .iter()
        .find(|p| p.number == fixture.number)
        .ok_or("Fixture metadata absent")?;
    if !p.base.fs.eq_ignore_ascii_case(fs.windows_name()) || p.base.name != label {
        return Err("Filesystem/label differs after format".into());
    }
    let root = allocation_batch::root(&r)?;
    let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(err)?;
    let binding = data_windows::FileBinding::new(
        &dir.try_clone().map_err(err)?.into_std_file(),
        &selection(B, Some(fixture.number))?.target,
    )?;
    data_windows::FileBinding::validate_all(&[binding])?;
    match dir.symlink_metadata(SENTINEL) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err("Old sentinel survived format".into()),
        Err(e) => return Err(err(e)),
    }
    // Compressible yet nonconstant, crossing cluster and compression-unit boundaries.
    let bytes: Vec<u8> = (0..524305)
        .map(|i| ((i / 8192 + i % 31) % 251) as u8)
        .collect();
    let mut f = dir
        .open_with(SENTINEL, CapOptions::new().write(true).create_new(true))
        .map_err(err)?;
    f.write_all(&bytes).map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    let mut f = dir.open(SENTINEL).map_err(err)?.into_std();
    let binding = data_windows::FileBinding::new(&f, &selection(B, Some(fixture.number))?.target)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    let mut actual = Vec::new();
    f.read_to_end(&mut actual).map_err(err)?;
    if actual != bytes {
        return Err("Independent ordinary-file readback differs".into());
    }
    if fs == FileSystem::Ntfs {
        let mut state = [0u8; 2];
        let n = data_windows::control(&f, FSCTL_GET_COMPRESSION, &[], &mut state)?;
        if n != 2
            || u16::from_le_bytes(state) != if compressed { 2 } else { 0 }
            || (f.metadata().map_err(err)?.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0)
                != compressed
        {
            return Err("New file did not inherit requested NTFS compression".into());
        }
    }
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(actual)))
}
pub fn check() -> Result<String, String> {
    let mut log = backup_protection::begin("format-options")?;
    let result = (|| {
        set_online(B)?;
        let baseline = format_checks::layouts(&[])?;
        let mut created = Vec::new();
        let mut passed = 0;
        for (fs, size, cluster) in [
            (FileSystem::Fat12, 2 * MIB, 512),
            (FileSystem::Fat16, 32 * MIB, 512),
            (FileSystem::Fat32, 64 * MIB, 512),
            (FileSystem::Exfat, 64 * MIB, 4096),
            (FileSystem::Ntfs, 128 * MIB, 0),
        ] {
            let fixture = new_fixture(&mut log, size)?;
            let disk_number = resolve(&fixture)?.disk.number;
            created.push((disk_number, fixture.number));
            if format_checks::layouts(&created)? != baseline {
                return Err("Prior layouts changed at create".into());
            }
            let gpt = tables()?;
            let layouts = format_checks::layouts(&[])?;
            let cases = if fs == FileSystem::Ntfs { 3 } else { 2 };
            for index in 0..cases {
                let options = FormatOptions {
                    mode: if index == 1 {
                        FormatMode::Full
                    } else {
                        FormatMode::Quick
                    },
                    ntfs_compression: fs == FileSystem::Ntfs && index == 1,
                    ntfs_short_names: fs == FileSystem::Ntfs && index == 1,
                };
                let cluster = if fs == FileSystem::Ntfs && index == 2 {
                    65536
                } else {
                    cluster
                };
                let label = format!("DSFMT{passed:02}");
                resolve(&fixture)?;
                record(
                    &mut log,
                    serde_json::json!({"event":"format-begin","fs":fs,"partition":fixture.number,"options":options,"cluster":cluster}),
                )?;
                let message = perform(Action::Format {
                    partition: fixture.number,
                    label: label.clone(),
                    file_system: fs,
                    cluster_size: cluster,
                    options,
                })?;
                if options.mode == FormatMode::Full
                    && fs.fat_type().is_some()
                    && !message.contains("SHA-256")
                {
                    return Err("Full FAT readback evidence missing".into());
                }
                let hash = roundtrip(&fixture, fs, &label, options.ntfs_compression)?;
                if tables()? != gpt
                    || format_checks::layouts(&[])? != layouts
                    || format_checks::layouts(&created)? != baseline
                {
                    return Err("GPT or layout changed during format".into());
                }
                passed += 1;
                record(
                    &mut log,
                    serde_json::json!({"event":"case-passed","fs":fs,"partition":fixture.number,"options":options,"cluster":cluster,"fileBytes":524305,"sha256":hash,"result":message}),
                )?;
            }
        }
        Ok(format!("PASS: {passed} quick/full format cases on five NEW B partitions; FAT12/16/32, exFAT and NTFS; NTFS compression/short-name options on/off and inherited file compression; 524305-byte file readbacks matched; existing layouts and OS unchanged. FAT full-volume readback verified by production engine. Windows full-format completion and metadata checked; no independent whole-volume zero guarantee. New fixtures retained; no retry or rollback. Desktop UI acceptance is separate."))
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

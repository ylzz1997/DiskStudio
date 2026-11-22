#![windows_subsystem = "windows"]
#![allow(dead_code)]
//! Guest-only acceptance driver. Compiles the production engines unchanged.
//! Never run on the host. UUID, exact disk inventory and serials fail closed.
mod advanced;
mod allocation_batch;
mod allocation_probe;
#[path = "../../../../src-tauri/src/backup_archive.rs"]
mod backup_archive;
#[path = "../../../../src-tauri/src/backup_browser.rs"]
mod backup_browser;
#[path = "../../../../src-tauri/src/backup_encryption.rs"]
mod backup_encryption;
#[path = "../../../../src-tauri/src/backup_files.rs"]
mod backup_files;
mod backup_protection;
#[path = "../../../../src-tauri/src/backup_secrets.rs"]
mod backup_secrets;
#[path = "../../../../src-tauri/src/bitlocker.rs"]
mod bitlocker;
mod bitlocker_checks;
mod btrfs_checks;
#[path = "../../../../src-tauri/src/btrfs_recovery.rs"]
mod btrfs_recovery;
#[path = "../../../../src-tauri/src/data_io.rs"]
mod data_io;
#[path = "../../../../src-tauri/src/data_tasks.rs"]
mod data_tasks;
#[path = "../../../../src-tauri/src/data_windows.rs"]
mod data_windows;
#[path = "../../../../src-tauri/src/device_state.rs"]
mod device_state;
#[path = "../../../../src-tauri/src/directory_metadata.rs"]
mod directory_metadata;
#[path = "../../../../src-tauri/src/directory_report.rs"]
mod directory_report;
#[path = "../../../../src-tauri/src/disk_identity.rs"]
mod disk_identity;
#[path = "../../../../src-tauri/src/export_metadata.rs"]
mod export_metadata;
mod ext4_checks;
#[path = "../../../../src-tauri/src/ext_allocation.rs"]
mod ext_allocation;
mod ext_checks;
mod ext_copy_checks;
mod signature_scope_checks;
#[path = "../../../../src-tauri/src/ext_recovery.rs"]
mod ext_recovery;
mod ext_reconstruct_checks;
mod ext_resume_checks;
#[path = "../../../../src-tauri/src/extfs.rs"]
mod extfs;
mod fat_reconstruction_checks;
#[path = "../../../../src-tauri/src/fat_recovery.rs"]
mod fat_recovery;
mod fat_resize_checks;
mod fat_resume_checks;
mod fat_sector_checks;
#[path = "../../../../src-tauri/src/fat_volume.rs"]
mod fat_volume;
mod file_clone_checks;
mod whole_file_clone_checks;
#[path = "../../../../src-tauri/src/file_metadata.rs"]
mod file_metadata;
mod format_checks;
mod format_options_checks;
#[path = "../../../../src-tauri/src/free_space_wipe.rs"]
mod free_space_wipe;
#[path = "../../../../src-tauri/src/image_bridge.rs"]
mod image_bridge;
#[path = "../../../../src-tauri/src/image_browser.rs"]
mod image_browser;
#[path = "../../../../src-tauri/src/image_files.rs"]
mod image_files;
mod letter_outcome_checks;
mod lost_backup_boot_checks;
mod lost_fixture;
#[cfg(test)]
#[path = "../../../../src-tauri/src/lost_partition_restore.rs"]
mod lost_partition_restore;
#[path = "../../../../src-tauri/src/lost_partition_tasks.rs"]
mod lost_partition_tasks;
#[path = "../../../../src-tauri/src/lost_partitions.rs"]
mod lost_partitions;
#[path = "../../../../src-tauri/src/lost_search_scope.rs"]
mod lost_search_scope;
#[path = "../../../../src-tauri/src/ldm_tasks.rs"]
mod ldm_tasks;
#[path = "../../../../src-tauri/src/ldm_basic.rs"]
mod ldm_basic;
#[path = "../../../../src-tauri/src/lvm_tasks.rs"]
mod lvm_tasks;
#[path = "../../../../src-tauri/src/maintenance_io.rs"]
mod maintenance_io;
#[path = "../../../../src-tauri/src/maintenance_tasks.rs"]
mod maintenance_tasks;
mod merge_checks;
mod metadata_checks;
mod move_checks;
#[path = "../../../../src-tauri/src/native_locale.rs"]
mod native_locale;
#[path = "../../../../src-tauri/src/ntfs_allocation.rs"]
mod ntfs_allocation;
#[path = "../../../../src-tauri/src/ntfs_checkpoint.rs"]
mod ntfs_checkpoint;
#[path = "../../../../src-tauri/src/ntfs_compression.rs"]
mod ntfs_compression;
#[path = "../../../../src-tauri/src/ntfs_file_clone.rs"]
mod ntfs_file_clone;
mod ntfs_file_clone_checks;
#[path = "../../../../src-tauri/src/ntfs_names.rs"]
mod ntfs_names;
mod ntfs_reconstruction_checks;
mod ntfs_resume_seed;
#[path = "../../../../src-tauri/src/ntfs_scan.rs"]
mod ntfs_scan;
mod ntfs_stream_checks;
mod ntfs_stream_evidence;
#[path = "../../../../src-tauri/src/ntfs_streams.rs"]
mod ntfs_streams;
#[path = "../../../../src-tauri/src/ntfs_wof.rs"]
mod ntfs_wof;
#[path = "../../../../src-tauri/src/partition_attributes.rs"]
mod partition_attributes;
#[path = "../../../../src-tauri/src/partition_move.rs"]
mod partition_move;
#[path = "../../../../src-tauri/src/partition_move_task.rs"]
mod partition_move_task;
#[path = "../../../../src-tauri/src/partitions.rs"]
mod partitions;
mod quick_partition_checks;
mod reallocation_checks;
#[path = "../../../../src-tauri/src/recovery.rs"]
mod recovery;
#[path = "../../../../src-tauri/src/recovery_checkpoint.rs"]
mod recovery_checkpoint;
#[path = "../../../../src-tauri/src/recovery_export.rs"]
mod recovery_export;
#[path = "../../../../src-tauri/src/recovery_output.rs"]
mod recovery_output;
mod recovery_output_checks;
#[path = "../../../../src-tauri/src/recovery_tree.rs"]
mod recovery_tree;
mod resize_clone_checks;
mod retained_restore_checks;
mod retained_partition_checks;
#[path = "../../../../src-tauri/src/image_convert.rs"]
mod image_convert;
mod unallocated_search_checks;
mod archive_restore_checks;
mod capacity_restore_checks;
mod fat_restore_checks;
#[path = "../../../../src-tauri/src/sector_copy.rs"]
mod sector_copy;
mod sector_copy_checks;
#[path = "../../../../src-tauri/src/signature_checkpoint.rs"]
mod signature_checkpoint;
#[path = "../../../../src-tauri/src/signature_scan.rs"]
mod signature_scan;
#[path = "../../../../src-tauri/src/signature_zip.rs"]
mod signature_zip;
#[path = "../../../../src-tauri/src/snapshot.rs"]
mod snapshot;
mod state_checks;
#[path = "../../../../src-tauri/src/storage.rs"]
mod storage;
#[path = "../../../../src-tauri/src/storage_spaces.rs"]
mod storage_spaces;
#[path = "../../../../src-tauri/src/surface_scan.rs"]
mod surface_scan;
#[path = "../../../../src-tauri/src/table_archive.rs"]
mod table_archive;
mod table_mbr_checks;
mod ntfs_wof_recovery_checks;
#[path = "../../../../src-tauri/src/table_restore.rs"]
mod table_restore;
#[path = "../../../../src-tauri/src/used_space.rs"]
mod used_space;
#[path = "../../../../src-tauri/src/windows_protection.rs"]
mod windows_protection;
use data_tasks::{Selection, Work};
use partitions::policy::{identity, Disk};
use serde::Deserialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, OnceLock},
    time::Duration,
};
use wmi::WMIConnection;
#[path = "../../../../src-tauri/src/operation_gate.rs"]
mod operation_gate;
static OPERATION: operation_gate::OperationGate = operation_gate::OperationGate::new();
static TEST_SECTOR: OnceLock<u64> = OnceLock::new();
// MSFT_Disk preserves six padding spaces before the NVMe namespace suffix.
const A: &str = "DS-TEST-A-0001      _00000001.";
const B: &str = "DS-TEST-B-0001      _00000001.";
const GIB: u64 = 1024 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Product {
    #[serde(rename = "UUID")]
    uuid: String,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn test_sector() -> u64 {
    TEST_SECTOR.get().copied().unwrap_or(512)
}
fn inventory() -> Result<Vec<Disk>, String> {
    let cim = WMIConnection::new().map_err(err)?;
    let products: Vec<Product> = cim
        .raw_query("SELECT UUID FROM Win32_ComputerSystemProduct")
        .map_err(err)?;
    if products.len() != 1
        || !products[0]
            .uuid
            .eq_ignore_ascii_case("e20adb9f-f665-432e-9a62-dd5d897b06a0")
    {
        return Err("STOP: not the authorized DiskStudio-Test VM".into());
    }
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let disks:Vec<Disk>=con.raw_query("SELECT __PATH,Number,SerialNumber,UniqueId,FriendlyName,Size,LogicalSectorSize,BusType,PartitionStyle,NumberOfPartitions,IsBoot,IsSystem,IsOffline,IsReadOnly,IsClustered FROM MSFT_Disk").map_err(err)?;
    if disks.len() != 3 {
        return Err("STOP: unexpected guest disk inventory".into());
    }
    for serial in [A, B] {
        let found: Vec<_> = disks
            .iter()
            .filter(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
            .collect();
        if found.len() != 1
            || found[0].size != 16 * GIB
            || found[0].is_boot != Some(false)
            || found[0].is_system != Some(false)
            || found[0].bus_type != Some(17)
            || found[0].logical_sector_size != Some(test_sector())
        {
            return Err(format!(
                "STOP: invalid test disk {serial}\n{}",
                disks
                    .iter()
                    .map(|d| format!(
                        "serial={:?}, size={}, sector={:?}, boot={:?}, system={:?}, bus={:?}",
                        d.serial_number.as_ref().map(|s| s.replace(' ', "[SP]")),
                        d.size,
                        d.logical_sector_size,
                        d.is_boot,
                        d.is_system,
                        d.bus_type
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }
    let os: Vec<_> = disks
        .iter()
        .filter(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some("DS-OS-0001"))
        .collect();
    if os.len() != 1 || os[0].size != 80 * GIB || os[0].is_system != Some(true) {
        return Err("STOP: guest OS identity mismatch".into());
    }
    Ok(disks)
}
fn selection(serial: &str, partition: Option<u32>) -> Result<Selection, String> {
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or("Test disk missing")?;
    let snap = storage::read_snapshot()?;
    let local = snap
        .disks
        .iter()
        .find(|l| l.hardware.number == d.number)
        .ok_or("Snapshot missing")?;
    Ok(Selection {
        target: identity(d).ok_or("Identity missing")?,
        layout_key: local.hardware.layout_key.clone().ok_or("Layout missing")?,
        partition,
    })
}
fn online(serial: &str) -> Result<(), String> {
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or("Disk missing")?;
    if d.is_offline == Some(true) {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Outcome {
            return_value: u32,
        }
        let con =
            WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
        let result: Outcome = con
            .exec_instance_method::<Disk, _>(&d.path, "Online", ())
            .map_err(err)?;
        if result.return_value != 0 {
            return Err(format!("Online failed {}", result.return_value));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}
fn operation(serial: &str, action: partitions::Action) -> Result<(), String> {
    inventory()?;
    let status = partitions::status();
    let t = status
        .targets
        .into_iter()
        .find(|t| t.serial.trim() == serial)
        .ok_or("Partition target unavailable")?;
    let plan = partitions::prepare(partitions::Request {
        target: t.target,
        revision: t.revision,
        action,
    })
    .map_err(|e| format!("PREPARE: {e}"))?;
    let out = partitions::execute(plan.token).map_err(|e| format!("EXECUTE: {e}"))?;
    if !out.verified {
        return Err(out.message);
    }
    Ok(())
}
fn part(serial: &str, size: u64) -> Result<(u32, PathBuf), String> {
    let sel = selection(serial, None)?;
    let snap = storage::read_snapshot()?;
    let d = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
        .ok_or("Disk missing")?;
    let matches: Vec<_> = d
        .partitions
        .iter()
        .filter(|p| {
            p.base.size == size
                && p.base.fs == "NTFS"
                && p.drive_letter.as_ref().is_some_and(|l| l.len() == 1)
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!("Expected one {size}-byte NTFS partition"));
    }
    Ok((
        matches[0].number,
        PathBuf::from(format!(
            "{}:\\",
            matches[0].drive_letter.as_deref().unwrap()
        )),
    ))
}
fn ensure_b(size: u64, label: &str) -> Result<(u32, PathBuf), String> {
    if let Ok(p) = part(B, size) {
        return Ok(p);
    }
    let status = partitions::status();
    let t = status
        .targets
        .iter()
        .find(|t| t.serial.trim() == B)
        .ok_or("B target missing")?;
    let gap = t
        .free_ranges
        .iter()
        .find(|g| g.size >= size)
        .ok_or("B has no suitable free range")?;
    let offset = gap.offset;
    operation(
        B,
        partitions::Action::Create {
            offset,
            size,
            kind: partitions::PartitionKind::Windows,
        },
    )?;
    let sel = selection(B, None)?;
    let snap = storage::read_snapshot()?;
    let d = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&sel.target.0))
        .ok_or("B missing")?;
    let p = d
        .partitions
        .iter()
        .find(|p| p.base.start == offset && p.base.size == size)
        .ok_or("Created partition missing")?;
    operation(
        B,
        partitions::Action::Format {
            options: Default::default(),
            partition: p.number,
            label: label.into(),
            file_system: partitions::FileSystem::Ntfs,
            cluster_size: 4096,
        },
    )?;
    std::thread::sleep(Duration::from_secs(2));
    part(B, size)
}
fn notice(message: &str) {
    rfd::MessageDialog::new()
        .set_title("DiskStudio VM acceptance")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}
fn run(work: Work) -> Result<(String, Option<String>), String> {
    inventory()?;
    data_tasks::run("guest-acceptance", &Arc::new(AtomicBool::new(false)), work)
}
fn find_output(folder: &Path, name: &str) -> Result<PathBuf, String> {
    for item in fs::read_dir(folder).map_err(err)? {
        let p = item.map_err(err)?.path();
        if p.is_dir() {
            if let Ok(found) = find_output(&p, name) {
                return Ok(found);
            }
        } else if p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(name))
        {
            return Ok(p);
        }
    }
    Err(format!("Recovered output not found: {name}"))
}
fn check() -> Result<String, String> {
    inventory()?;
    online(A)?;
    online(B)?;
    let os_before = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS missing")?
        .hardware
        .layout_key;
    notice("Preflight PASSED. Only DS-TEST-A/B (16 GiB each) will be tested.\n\nThis driver uses the production Rust engines. It will create B data volumes, generate fixtures, test recovery/backup/restore/clone, and keep the final B disk offline. The 80 GiB OS disk is excluded.\n\nClick OK to start.");
    let (a_number, a_root) = part(A, 2 * GIB)?;
    let (_, b_root) = ensure_b(6 * GIB, "DSBackup")?;
    let (clone_number, _) = ensure_b(2 * GIB, "DSClone")?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let source_folder = a_root.join(format!("DiskStudio-fixtures-{id}"));
    fs::create_dir(&source_folder).map_err(err)?;
    let output = b_root.join(format!("DiskStudio-acceptance-{id}"));
    fs::create_dir(&output).map_err(err)?;
    let resident_name = format!("deleted-{id}.txt");
    let resident = b"DiskStudio actual NTFS deleted resident file recovery. 2026-11-11.";
    let large_name = format!("existing-{id}.bin");
    let large: Vec<u8> = (0..1024 * 1024)
        .map(|i| ((i * 31 + i / 17) % 251) as u8)
        .collect();
    fs::write(source_folder.join(&large_name), &large).map_err(err)?;
    let pdf = b"%PDF-1.4\nDiskStudio signature fixture (not a full PDF document)\n%%EOF";
    fs::write(source_folder.join("signature.pdf"), pdf).map_err(err)?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(source_folder.join(&resident_name))
            .map_err(err)?;
        f.write_all(resident).map_err(err)?;
        f.sync_all().map_err(err)?;
    }
    fs::remove_file(source_folder.join(&resident_name)).map_err(err)?;
    let a = selection(A, Some(a_number))?;
    let resolved = data_windows::resolve(&a, false, false)?;
    let mut candidates = vec![];
    recovery::scan_ntfs(
        &mut data_windows::reader(&resolved)?,
        recovery::ScanMode::AllFiles,
        |_, _| Ok(()),
        |c| candidates.push(c),
    )?;
    let deleted = candidates
        .iter()
        .find(|c| c.name == resident_name && c.deleted)
        .ok_or("Deleted NTFS record not recovered")?
        .clone();
    let existing = candidates
        .iter()
        .find(|c| c.name == large_name && !c.deleted)
        .ok_or("Existing nonresident file not found")?
        .clone();
    let export = recovery_export::plan(
        &[deleted.clone(), existing.clone()],
        &recovery_tree::Catalog::default(),
        recovery_export::Layout::Flat,
    )?;
    run(Work::Recover {
        source: a.clone().into(),
        files: vec![deleted, existing],
        folder: output.clone(),
        export,
    })?;
    if fs::read(find_output(&output, &resident_name)?).map_err(err)? != resident {
        return Err("Resident recovered contents differ".into());
    }
    if fs::read(find_output(&output, &large_name)?).map_err(err)? != large {
        return Err("Nonresident recovered contents differ".into());
    }
    let mut signatures = vec![];
    recovery::scan_signatures(
        &mut data_windows::reader(&resolved)?,
        resolved.size,
        |_, _| Ok(()),
        |c| signatures.push(c),
    )?;
    let mut matched = false;
    for c in signatures.iter().filter(|c| c.size == pdf.len() as u64) {
        let mut bytes = vec![];
        recovery::extract(
            &mut data_windows::reader(&resolved)?,
            c,
            &mut bytes,
            u64::MAX,
            |_| Ok(()),
        )?;
        if bytes == pdf {
            matched = true;
            break;
        }
    }
    if !matched {
        return Err("Signature extraction mismatch".into());
    }
    notice("PASS: NTFS deleted resident recovery, existing nonresident recovery, PNG/JPEG/PDF engine PDF signature extraction. Exported bytes match fixtures.\n\nNext: 2 GiB partition backup, restore and partition clone, including full readback hashes.");
    let backup = output.join("partition.dsbackup");
    let (_, backup_hash) = run(Work::Backup {
        source: a.clone(),
        path: backup.clone(),
    })?;
    fs::write(
        source_folder.join(&large_name),
        b"Changed after backup; restore must recover original data",
    )
    .map_err(err)?;
    use std::os::windows::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(&backup)
        .map_err(err)?;
    let header = data_tasks::read_header(&mut file)?;
    let binding = data_windows::FileBinding::new(&file, &selection(B, None)?.target)?;
    let (_, restore_hash) = run(Work::Restore {
        destination: selection(A, Some(a_number))?,
        file,
        header,
        binding,
    })?;
    if backup_hash != restore_hash {
        return Err("Backup/restore hash mismatch".into());
    }
    online(A)?;
    if fs::read(source_folder.join(&large_name)).map_err(err)? != large {
        return Err("Restored file differs from original".into());
    }
    let (_, clone_hash) = run(Work::Clone {
        mode: crate::used_space::Mode::AllSectors,
        source: selection(A, Some(a_number))?,
        destination: selection(B, Some(clone_number))?,
    })?;
    online(B)?;
    let (_, clone_root) = part(B, 2 * GIB)?;
    if fs::read(
        clone_root
            .join(source_folder.file_name().unwrap())
            .join(&large_name),
    )
    .map_err(err)?
        != large
    {
        return Err("Partition clone file mismatch".into());
    }
    notice(&format!("PASS: 2 GiB backup + restore, original file restored, 2 GiB partition clone, destination file exact match.\n\nBackup SHA-256:\n{}\nClone SHA-256:\n{}\n\nNext: full 16 GiB A-to-B disk clone and readback verification. All B data, including these test outputs, will be replaced.",backup_hash.unwrap_or_default(),clone_hash.unwrap_or_default()));
    let (_, disk_hash) = run(Work::Clone {
        mode: crate::used_space::Mode::AllSectors,
        source: selection(A, None)?,
        destination: selection(B, None)?,
    })?;
    let os_after = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS missing after tests")?
        .hardware
        .layout_key;
    if os_before != os_after {
        return Err("Guest OS layout changed unexpectedly".into());
    }
    Ok(format!("ALL GUEST CHECKS PASSED\n\n- Exact VM UUID + 3-disk inventory\n- Deleted NTFS resident file recovered byte-for-byte\n- NTFS nonresident file exported byte-for-byte\n- PDF signature scan + extraction\n- 2 GiB backup + SHA-256 verification\n- Restore + readback hash + original file contents\n- 2 GiB partition clone + actual file contents\n- 16 GiB whole-disk clone + full readback hash\n- Guest OS layout unchanged\n\nB remains offline; A remains online.\nFull disk SHA-256:\n{}",disk_hash.unwrap_or_default()))
}
fn main() {
    let mode = std::env::args().nth(1).or_else(|| {
        std::env::current_exe()
            .ok()?
            .file_stem()?
            .to_str()
            .map(str::to_owned)
    });
    let reallocation = mode
        .as_deref()
        .and_then(reallocation_checks::EntryPoint::parse);
    let is_4kn = matches!(
        mode.as_deref(),
        Some(
            "Btrfs-Lab-4Kn-Create"
                | "Btrfs-Lab-4Kn-Inspect"
                | "Btrfs-Lab-4Kn-Online"
                | "File-Clone-Ntfs-4Kn-Prepare"
                | "File-Clone-Ntfs-4Kn-Baseline"
                | "File-Clone-Ntfs-4Kn-Verify"
                | "File-Clone-Ntfs-4Kn-Inspect-Source"
                | "File-Clone-Ntfs-4Kn-Settled-Baseline"
                | "File-Clone-Ntfs-4Kn-Verify-Settled"
                | "File-Clone-Ntfs-4Kn-Audit-Settled"
                | "File-Clone-Ntfs-4Kn-Metadata-Prepare"
                | "File-Clone-Ntfs-4Kn-Metadata-Continue"
                | "File-Clone-Ntfs-4Kn-Metadata-Inspect"
                | "File-Clone-Ntfs-4Kn-Metadata-Inspect-Sample"
                | "File-Clone-Ntfs-4Kn-Metadata-Baseline"
                | "File-Clone-Ntfs-4Kn-Metadata-Verify"
                | "File-Clone-Ntfs-4Kn-Storage-Prepare"
                | "File-Clone-Ntfs-4Kn-Storage-Baseline"
                | "File-Clone-Ntfs-4Kn-Storage-Verify"
                | "File-Clone-Ntfs-4Kn-Storage-Audit"
                | "File-Clone-Ntfs-4Kn-Links-Prepare"
                | "File-Clone-Ntfs-4Kn-Links-Baseline"
                | "File-Clone-Ntfs-4Kn-Links-Verify"
                | "File-Clone-Ntfs-4Kn-Links-Inspect"
                | "File-Clone-Ntfs-4Kn-Links-Resume-Baseline"
                | "File-Clone-Ntfs-4Kn-Links-Resume-Verify"
                | "File-Clone-Ntfs-4Kn-Links-Root-Audit"
                | "File-Clone-Ntfs-4Kn-Wof-Prepare"
                | "File-Clone-Ntfs-4Kn-Wof-Baseline"
                | "File-Clone-Ntfs-4Kn-Wof-Verify"
                | "File-Clone-Ntfs-4Kn-Wof-Continue"
                | "File-Clone-Ntfs-4Kn-Wof-Resume-Baseline"
                | "File-Clone-Ntfs-4Kn-Wof-Resume-Verify"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Prepare"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Baseline"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Verify"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Continue"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Resume-Baseline"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Resume-Verify"
                | "File-Clone-Ntfs-4Kn-Wof-Image-After-Online"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Settled-Baseline"
                | "File-Clone-Ntfs-4Kn-Wof-Image-Settled-Verify"
                | "File-Clone-Ntfs-4Kn-Efs-Prepare"
                | "File-Clone-Ntfs-4Kn-Efs-Baseline"
                | "File-Clone-Ntfs-4Kn-Efs-Verify"
                | "Whole-File-Clone-4Kn-Prepare"
                | "Whole-File-Clone-4Kn-Resume"
                | "Whole-File-Clone-4Kn-Rebind"
                | "Whole-File-Clone-4Kn-Verify"
                | "Mixed-File-Clone-4Kn-Prepare"
                | "Mixed-File-Clone-4Kn-Resume"
                | "Mixed-File-Clone-4Kn-Verify"
                | "Btrfs-Restore-4Kn-Prepare"
                | "Btrfs-Restore-4Kn-Resume"
                | "Btrfs-Restore-4Kn-Verify"
                | "Btrfs-Exact-Clone-4Kn-Prepare"
                | "Btrfs-Exact-Clone-4Kn-Verify"
                | "Signature-Scope-4Kn-Prepare"
                | "Signature-Scope-4Kn-Verify"
                | "Signature-Formats-4Kn-Prepare"
                | "Signature-Formats-4Kn-Verify"
                | "Signature-Media-4Kn-Prepare"
                | "Signature-Media-4Kn-Verify"
                | "Signature-Images-4Kn-Prepare"
                | "Signature-Images-4Kn-Verify"
                | "Table-MBR-4Kn-Prepare"
                | "Table-MBR-4Kn-Verify"
                | "Table-MBR-4Kn-Record"
                | "Ntfs-Wof-Recovery-4Kn-Baseline"
                | "Ntfs-Wof-Recovery-4Kn-Verify"
                | "Surface-Refinement-4Kn-Verify"
                | "Surface-Refinement-4Kn-Inspect"
                | "Surface-Refinement-4Kn-Prepare"
                | "Surface-Timeout-4Kn-Verify"
                | "Surface-Repair-4Kn-Baseline"
                | "Surface-Repair-4Kn-Verify"
                | "Surface-Repair-4Kn-Reports"
                | "Archive-Restore-4Kn-Prepare"
                | "Archive-Restore-4Kn-Continue"
                | "Archive-Restore-4Kn-Verify-Partition"
                | "Archive-Restore-4Kn-Verify-Disk"
                | "Capacity-Restore-4Kn-Prepare"
                | "Capacity-Restore-4Kn-Continue"
                | "Capacity-Restore-4Kn-After-Ntfs"
                | "Capacity-Restore-4Kn-After-Fat32"
                | "Capacity-Restore-4Kn-Verify"
                | "FAT-Restore-4Kn-Prepare"
                | "FAT-Restore-4Kn-Verify"
                | "Btrfs-Lab-4Kn-Move-Prepare"
                | "Btrfs-Lab-4Kn-Move-Inspect"
                | "Btrfs-Lab-4Kn-Move-Reuse"
                | "Btrfs-Lab-4Kn-Move-Reuse-Inspect"
                | "Btrfs-Lab-4Kn-Clone-Prepare"
                | "Btrfs-Lab-4Kn-Clone-Resume"
                | "Btrfs-Lab-4Kn-Clone-Diagnose"
                | "Btrfs-Lab-4Kn-Clone-Inspect-Shrink"
                | "Btrfs-Lab-4Kn-Clone-Inspect-Grow"
                | "Btrfs-Lab-4Kn-Snapshot-Prepare"
                | "Btrfs-Lab-4Kn-Snapshot-Inspect"
                | "Btrfs-Lab-4Kn-Retained-Prepare"
                | "Btrfs-Lab-4Kn-Retained-Inspect"
                | "Btrfs-Lab-4Kn-Sha256-Prepare"
                | "Btrfs-Lab-4Kn-Sha256-Inspect"
                | "Btrfs-Lab-4Kn-Members-Prepare"
                | "Btrfs-Lab-4Kn-Members-Inspect"
                | "Btrfs-Lab-4Kn-Parity-Prepare"
                | "Btrfs-Lab-4Kn-Parity-Inspect"
                | "Btrfs-Lab-4Kn-Allocation-Prepare"
                | "Btrfs-Lab-4Kn-Allocation-Archive"
                | "Btrfs-Lab-4Kn-Allocation-Inspect"
                | "Btrfs-Lab-4Kn-Lost-Prepare"
                | "Btrfs-Lab-4Kn-Lost-Inspect"
                | "Btrfs-Lab-4Kn-Lost-Group-Prepare"
                | "Btrfs-Lab-4Kn-Lost-Group-Inspect"
                | "Lvm-Lab-4Kn-Prepare"
                | "Lvm-Lab-4Kn-Inspect"
                | "Lvm-Images-4Kn-Prepare"
                | "Lvm-Images-4Kn-Inspect"
                | "Lvm-Raid-4Kn-Prepare"
                | "Lvm-Raid-4Kn-Inspect"
                | "Lvm-Thin-4Kn-Prepare"
                | "Lvm-Thin-4Kn-Inspect"
                | "Lvm-Cow-4Kn-Prepare"
                | "Lvm-Cow-4Kn-Inspect"
                | "Lvm-Degraded-4Kn-Prepare"
                | "Lvm-Degraded-4Kn-Inspect"
                | "Lvm-Cross-4Kn-Prepare"
                | "Lvm-Cross-4Kn-Inspect"
                | "Lvm-Raid10-4Kn-Prepare"
                | "Lvm-Raid10-4Kn-Inspect"
                | "Virtual-Raid-4Kn-Prepare"
                | "Virtual-Raid-4Kn-Inspect"
                | "Virtual-Raid-Md6-Prepare"
                | "Virtual-Raid-Md6-Inspect"
                | "Virtual-Raid-Md-Metadata-Prepare"
                | "Virtual-Raid-Md-Metadata-Inspect"
                | "Virtual-Raid-Md10-Stage"
                | "Virtual-Raid-Md10-Verify"
                | "Virtual-Raid-Md10-Advanced-Stage"
                | "Virtual-Raid-Md10-Advanced-Verify"
                | "Virtual-Raid-Device-Fresh-Prepare"
                | "Virtual-Raid-Device-Fresh-Inspect"
                | "Virtual-Raid-Md-Zones-Prepare"
                | "Virtual-Raid-Md-Zones-Inspect"
                | "Virtual-Raid-Md-Zones-Continue"
                | "Sector-Copy-4Kn-Baseline"
                | "Sector-Edit-4Kn-Baseline"
                | "Sector-Edit-4Kn-Verify"
                | "Sector-Copy-4Kn-Verify"
                | "Sector-Copy-4Kn-Verify-Report"
                | "Sector-Copy-Fault-4Kn-Prepare"
                | "Sector-Copy-Fault-4Kn-Verify"
                | "Sector-Copy-Device-4Kn-Prepare"
                | "Sector-Copy-Device-4Kn-Continue"
                | "Sector-Copy-Device-4Kn-Verify"
                | "Sector-Copy-Reread-4Kn-Prepare"
                | "Sector-Copy-Reread-4Kn-Verify-File"
                | "Sector-Copy-Reread-4Kn-Verify-Device"
                | "Sector-Copy-Reread-4Kn-Probe"
                | "Sector-Continuation-4Kn-Prepare"
                | "Sector-Continuation-4Kn-Interrupted"
                | "Sector-Continuation-4Kn-Reread"
                | "Sector-Continuation-4Kn-Finish"
                | "Sector-Legacy-4Kn-Prepare"
                | "Sector-Legacy-4Kn-File"
                | "Sector-Legacy-4Kn-Rearm"
                | "Sector-Legacy-4Kn-Device"
                | "Sector-Image-4Kn-Prepare"
                | "Sector-Image-4Kn-File"
                | "Sector-Image-4Kn-Device"
                | "Fat-Sectors-4Kn-Check"
                | "Fat-Sectors-4Kn-Report"
                | "Fat-Sectors-4Kn-Diagnose"
                | "Fat-Sectors-4Kn-Continue-Check"
                | "Fat-Sectors-4Kn-Continue-Report"
                | "Fat-Sectors-4Kn-Tail-Check"
                | "Fat-Sectors-4Kn-Tail-Report"
                | "Fat-Sectors-4Kn-Fat32-Diagnose"
                | "Fat-Sectors-4Kn-Fat32-Check"
                | "Fat-Sectors-4Kn-Fat32-Report"
                | "Partition-Move-4Kn-Exfat-Prepare"
                | "Partition-Move-4Kn-Exfat-Inspect"
                | "Partition-Move-4Kn-Ntfs-Prepare"
                | "Partition-Move-4Kn-Ntfs-Inspect"
                | "Partition-Move-4Kn-Ntfs-Diagnose"
                | "Partition-Move-4Kn-Fat12-Prepare"
                | "Partition-Move-4Kn-Fat12-Continue-Prepare"
                | "Partition-Move-4Kn-Fat12-Inspect"
                | "Partition-Move-4Kn-Fat32-Prepare"
                | "Partition-Move-4Kn-Fat32-Range-Prepare"
                | "Partition-Move-4Kn-Fat32-Interrupted"
                | "Partition-Move-4Kn-Fat32-Inspect"
                | "Partition-Merge-4Kn-Prepare"
                | "Partition-Merge-4Kn-Check"
                | "Partition-Merge-4Kn-Inspect"
                | "Partition-Merge-4Kn-Report"
                | "Partition-Merge-4Kn-Diagnose"
                | "Partition-Merge-4Kn-Continue"
                | "Partition-Merge-4Kn-Continue-Report"
                | "Partition-Merge-4Kn-Continue-Inspect"
                | "Partition-Merge-4Kn-Catalogue-Recheck"
                | "Partition-Merge-4Kn-Catalogue-Recheck-Report"
                | "Partition-Merge-4Kn-Corrected"
                | "Partition-Merge-4Kn-Corrected-Report"
                | "Partition-Merge-4Kn-Corrected-Inspect"
                | "Partition-Merge-4Kn-Owner-Inspect"
                | "Partition-Merge-4Kn-Fresh-Prepare"
                | "Partition-Merge-4Kn-Fresh-Check"
                | "Partition-Merge-4Kn-Fresh-Report"
                | "Partition-Merge-4Kn-Fresh-Inspect"
        )
    );
    if is_4kn || reallocation.is_some() {
        TEST_SECTOR.set(4096).expect("guest sector configured once");
    }
    let result = std::panic::catch_unwind(|| match reallocation {
        Some(entry) => entry.run(),
        None => dispatch(mode.as_deref()),
    })
    .unwrap_or_else(|_| Err("Guest test panicked; inspect state before retrying".into()));
    notice(&match result {
        Ok(text) => text,
        Err(error) => format!("GUEST CHECK FAILED\n\n{error}\n\nNo automatic rollback/retry. Partial targets remain offline. Host execution is forbidden."),
    });
}

fn dispatch(mode: Option<&str>) -> Result<String, String> {
    match mode {
        Some("Whole-File-Clone-4Kn-Prepare") => whole_file_clone_checks::prepare(false),
        Some("Whole-File-Clone-4Kn-Resume") => whole_file_clone_checks::prepare(true),
        Some("Whole-File-Clone-4Kn-Rebind") => whole_file_clone_checks::rebind(),
        Some("Whole-File-Clone-4Kn-Verify") => whole_file_clone_checks::verify(),
        Some("Mixed-File-Clone-4Kn-Prepare") => btrfs_checks::whole_clone::prepare(false),
        Some("Mixed-File-Clone-4Kn-Resume") => btrfs_checks::whole_clone::prepare(true),
        Some("Mixed-File-Clone-4Kn-Verify") => btrfs_checks::whole_clone::verify(),
        Some("Btrfs-Restore-4Kn-Prepare") => btrfs_checks::backup_restore::prepare(false),
        Some("Btrfs-Restore-4Kn-Resume") => btrfs_checks::backup_restore::prepare(true),
        Some("Btrfs-Restore-4Kn-Verify") => btrfs_checks::backup_restore::verify(),
        Some("Btrfs-Exact-Clone-4Kn-Prepare") => btrfs_checks::backup_restore::exact_clone::prepare(),
        Some("Btrfs-Exact-Clone-4Kn-Verify") => btrfs_checks::backup_restore::exact_clone::verify(),
        Some("Signature-Scope-4Kn-Prepare") => signature_scope_checks::prepare(signature_scope_checks::Sample::Pdf),
        Some("Signature-Scope-4Kn-Verify") => signature_scope_checks::verify(signature_scope_checks::Sample::Pdf),
        Some("Signature-Formats-4Kn-Prepare") => signature_scope_checks::prepare(signature_scope_checks::Sample::Formats),
        Some("Signature-Formats-4Kn-Verify") => signature_scope_checks::verify(signature_scope_checks::Sample::Formats),
        Some("Signature-Media-4Kn-Prepare") => signature_scope_checks::prepare(signature_scope_checks::Sample::Media),
        Some("Signature-Media-4Kn-Verify") => signature_scope_checks::verify(signature_scope_checks::Sample::Media),
        Some("Signature-Images-4Kn-Prepare") => signature_scope_checks::prepare(signature_scope_checks::Sample::Images),
        Some("Signature-Images-4Kn-Verify") => signature_scope_checks::verify(signature_scope_checks::Sample::Images),
        Some("Table-MBR-4Kn-Prepare") => table_mbr_checks::prepare(),
        Some("Table-MBR-4Kn-Verify") => table_mbr_checks::verify(),
        Some("Table-MBR-4Kn-Record") => table_mbr_checks::record(),
        Some("Ntfs-Wof-Recovery-4Kn-Baseline") => ntfs_wof_recovery_checks::baseline(),
        Some("Ntfs-Wof-Recovery-4Kn-Verify") => ntfs_wof_recovery_checks::verify(),
        Some("Surface-Refinement-4Kn-Verify") => sector_copy_checks::verify_surface_refinement(),
        Some("Surface-Refinement-4Kn-Inspect") => sector_copy_checks::inspect_surface_refinement(),
        Some("Surface-Refinement-4Kn-Prepare") => sector_copy_checks::prepare_surface_refinement(),
        Some("Surface-Timeout-4Kn-Verify") => sector_copy_checks::verify_surface_timeout(),
        Some("Sector-Edit-4Kn-Baseline") => sector_copy_checks::sector_edit_witness(false),
        Some("Sector-Edit-4Kn-Verify") => sector_copy_checks::sector_edit_witness(true),
        Some("Surface-Repair-4Kn-Baseline") => sector_copy_checks::surface_repair_witness(false),
        Some("Surface-Repair-4Kn-Verify") => sector_copy_checks::surface_repair_witness(true),
        Some("Surface-Repair-4Kn-Reports") => sector_copy_checks::verify_surface_reports(),
        Some("Surface-Parallel-512-Inspect") => sector_copy_checks::inspect_surface_parallel(),
        Some("Archive-Restore-4Kn-Prepare") => archive_restore_checks::prepare(false),
        Some("Archive-Restore-4Kn-Continue") => archive_restore_checks::prepare(true),
        Some("Archive-Restore-4Kn-Verify-Partition") => archive_restore_checks::verify(false),
        Some("Archive-Restore-4Kn-Verify-Disk") => archive_restore_checks::verify(true),
        Some("Capacity-Restore-4Kn-Prepare") => capacity_restore_checks::prepare(false),
        Some("Capacity-Restore-4Kn-Continue") => capacity_restore_checks::prepare(true),
        Some("Capacity-Restore-4Kn-After-Ntfs") => {
            capacity_restore_checks::verify(capacity_restore_checks::Stage::Ntfs)
        }
        Some("Capacity-Restore-4Kn-After-Fat32") => {
            capacity_restore_checks::verify(capacity_restore_checks::Stage::Fat32)
        }
        Some("Capacity-Restore-4Kn-Verify") => {
            capacity_restore_checks::verify(capacity_restore_checks::Stage::Exfat)
        }
        Some("FAT-Restore-4Kn-Prepare") => fat_restore_checks::prepare(),
        Some("FAT-Restore-4Kn-Verify") => fat_restore_checks::verify(),
        Some("Ext-Copy-Prepare") => ext_copy_checks::prepare(),
        Some("Ext-Copy-Verify-Restores") => ext_copy_checks::verify(false),
        Some("Ext-Copy-Verify-Clones") => ext_copy_checks::verify(true),
        Some("Btrfs-Lab-4Kn-Create") => btrfs_checks::create(),
        Some("Btrfs-Lab-4Kn-Inspect") => btrfs_checks::inspect(),
        Some("Btrfs-Lab-4Kn-Online") => btrfs_checks::online_fixture(),
        Some("Btrfs-Lab-4Kn-Move-Prepare") => btrfs_checks::prepare_move(false),
        Some("Btrfs-Lab-4Kn-Clone-Prepare") => btrfs_checks::capacity_clone::prepare(),
        Some("Btrfs-Lab-4Kn-Clone-Resume") => btrfs_checks::capacity_clone::resume(),
        Some("Btrfs-Lab-4Kn-Clone-Diagnose") => btrfs_checks::capacity_clone::diagnose(),
        Some("Btrfs-Lab-4Kn-Clone-Inspect-Shrink") => btrfs_checks::capacity_clone::verify(false),
        Some("Btrfs-Lab-4Kn-Clone-Inspect-Grow") => btrfs_checks::capacity_clone::verify(true),
        Some("Btrfs-Lab-4Kn-Move-Reuse") => btrfs_checks::prepare_move(true),
        Some("Btrfs-Lab-4Kn-Move-Inspect" | "Btrfs-Lab-4Kn-Move-Reuse-Inspect") => {
            btrfs_checks::inspect_move()
        }
        Some("Btrfs-Lab-4Kn-Snapshot-Prepare") => {
            btrfs_checks::prepare_history(btrfs_checks::SNAPSHOT)
        }
        Some("Btrfs-Lab-4Kn-Snapshot-Inspect") => {
            btrfs_checks::inspect_history(btrfs_checks::SNAPSHOT)
        }
        Some("Btrfs-Lab-4Kn-Retained-Prepare") => {
            btrfs_checks::prepare_history(btrfs_checks::RETAINED)
        }
        Some("Btrfs-Lab-4Kn-Retained-Inspect") => {
            btrfs_checks::inspect_history(btrfs_checks::RETAINED)
        }
        Some("Btrfs-Lab-4Kn-Sha256-Prepare") => btrfs_checks::prepare_checksum(),
        Some("Btrfs-Lab-4Kn-Sha256-Inspect") => btrfs_checks::inspect_checksum(),
        Some("Btrfs-Lab-4Kn-Members-Prepare") => btrfs_checks::prepare_members(),
        Some("Btrfs-Lab-4Kn-Members-Inspect") => btrfs_checks::inspect_members(),
        Some("Btrfs-Lab-4Kn-Parity-Prepare") => btrfs_checks::prepare_parity(),
        Some("Btrfs-Lab-4Kn-Parity-Inspect") => btrfs_checks::inspect_parity(),
        Some("Btrfs-Lab-4Kn-Allocation-Prepare") => btrfs_checks::allocation::prepare(),
        Some("Btrfs-Lab-4Kn-Allocation-Archive") => btrfs_checks::allocation::inspect_archive(),
        Some("Btrfs-Lab-4Kn-Allocation-Inspect") => btrfs_checks::allocation::inspect_clone(),
        Some("Btrfs-Lab-4Kn-Lost-Prepare") => btrfs_checks::lost::prepare(),
        Some("Btrfs-Lab-4Kn-Lost-Inspect") => btrfs_checks::lost::inspect(),
        Some("Btrfs-Lab-4Kn-Lost-Group-Prepare") => btrfs_checks::lost::prepare_group(),
        Some("Btrfs-Lab-4Kn-Lost-Group-Inspect") => btrfs_checks::lost::inspect_group(),
        Some("Lvm-Lab-4Kn-Prepare") => btrfs_checks::lvm::prepare(),
        Some("Lvm-Lab-4Kn-Inspect") => btrfs_checks::lvm::inspect(),
        Some("Lvm-Images-4Kn-Prepare") => btrfs_checks::lvm::images::prepare(),
        Some("Lvm-Images-4Kn-Inspect") => btrfs_checks::lvm::images::inspect(),
        Some("Lvm-Raid-4Kn-Prepare") => {
            btrfs_checks::lvm::group_images::prepare(btrfs_checks::lvm::group_images::RAID)
        }
        Some("Lvm-Raid-4Kn-Inspect") => {
            btrfs_checks::lvm::group_images::inspect(btrfs_checks::lvm::group_images::RAID)
        }
        Some("Lvm-Thin-4Kn-Prepare") => {
            btrfs_checks::lvm::group_images::prepare(btrfs_checks::lvm::group_images::THIN)
        }
        Some("Lvm-Thin-4Kn-Inspect") => {
            btrfs_checks::lvm::group_images::inspect(btrfs_checks::lvm::group_images::THIN)
        }
        Some("Lvm-Cow-4Kn-Prepare") => {
            btrfs_checks::lvm::group_images::prepare(btrfs_checks::lvm::group_images::COW)
        }
        Some("Lvm-Cow-4Kn-Inspect") => {
            btrfs_checks::lvm::group_images::inspect(btrfs_checks::lvm::group_images::COW)
        }
        Some("Lvm-Degraded-4Kn-Prepare") => {
            btrfs_checks::lvm::group_images::prepare(btrfs_checks::lvm::group_images::DEGRADED)
        }
        Some("Lvm-Degraded-4Kn-Inspect") => {
            btrfs_checks::lvm::group_images::inspect(btrfs_checks::lvm::group_images::DEGRADED)
        }
        Some("Lvm-Cross-4Kn-Prepare") => btrfs_checks::lvm::cross::prepare(),
        Some("Lvm-Cross-4Kn-Inspect") => btrfs_checks::lvm::cross::inspect(),
        Some("Lvm-Raid10-4Kn-Prepare") => {
            btrfs_checks::lvm::group_images::prepare(btrfs_checks::lvm::group_images::RAID10)
        }
        Some("Lvm-Raid10-4Kn-Inspect") => {
            btrfs_checks::lvm::group_images::inspect(btrfs_checks::lvm::group_images::RAID10)
        }
        Some("Virtual-Raid-4Kn-Prepare") => {
            btrfs_checks::virtual_raid::prepare(btrfs_checks::virtual_raid::Mode::Raid5)
        }
        Some("Virtual-Raid-4Kn-Inspect") => {
            btrfs_checks::virtual_raid::inspect(btrfs_checks::virtual_raid::Mode::Raid5)
        }
        Some("Virtual-Raid-Md6-Prepare") => {
            btrfs_checks::virtual_raid::prepare(btrfs_checks::virtual_raid::Mode::Md6)
        }
        Some("Virtual-Raid-Md6-Inspect") => {
            btrfs_checks::virtual_raid::inspect(btrfs_checks::virtual_raid::Mode::Md6)
        }
        Some("Virtual-Raid-Md-Metadata-Prepare") => {
            btrfs_checks::virtual_raid::prepare(btrfs_checks::virtual_raid::Mode::MdMetadata)
        }
        Some("Virtual-Raid-Md-Metadata-Inspect") => {
            btrfs_checks::virtual_raid::inspect(btrfs_checks::virtual_raid::Mode::MdMetadata)
        }
        Some("Virtual-Raid-Md10-Stage") => {
            btrfs_checks::virtual_raid::prepare(btrfs_checks::virtual_raid::Mode::Md10)
        }
        Some("Virtual-Raid-Md10-Verify") => {
            btrfs_checks::virtual_raid::inspect(btrfs_checks::virtual_raid::Mode::Md10)
        }
        Some("Virtual-Raid-Md10-Advanced-Stage") => {
            btrfs_checks::virtual_raid::prepare(btrfs_checks::virtual_raid::Mode::Md10Advanced)
        }
        Some("Virtual-Raid-Md10-Advanced-Verify") => {
            btrfs_checks::virtual_raid::inspect(btrfs_checks::virtual_raid::Mode::Md10Advanced)
        }
        Some("Virtual-Raid-Device-Fresh-Prepare") => btrfs_checks::virtual_raid::devices::prepare(
            btrfs_checks::virtual_raid::devices::Mode::Manual,
        ),
        Some("Virtual-Raid-Device-Fresh-Inspect") => btrfs_checks::virtual_raid::devices::inspect(
            btrfs_checks::virtual_raid::devices::Mode::Manual,
        ),
        Some("Virtual-Raid-Md-Zones-Prepare") => btrfs_checks::virtual_raid::devices::prepare(
            btrfs_checks::virtual_raid::devices::Mode::MdZones,
        ),
        Some("Virtual-Raid-Md-Zones-Inspect") => btrfs_checks::virtual_raid::devices::inspect(
            btrfs_checks::virtual_raid::devices::Mode::MdZones,
        ),
        Some("Virtual-Raid-Md-Zones-Continue") => {
            btrfs_checks::virtual_raid::devices::finish_zones()
        }
        Some("Sector-Copy-4Kn-Baseline") => sector_copy_checks::baseline(),
        Some("Sector-Copy-4Kn-Verify") => sector_copy_checks::verify(),
        Some("Sector-Copy-Fault-4Kn-Prepare") => sector_copy_checks::fault_prepare(),
        Some("Sector-Copy-Fault-4Kn-Verify") => sector_copy_checks::fault_verify(),
        Some("Sector-Copy-Device-4Kn-Prepare") => sector_copy_checks::device_prepare(),
        Some("Sector-Copy-Device-4Kn-Continue") => sector_copy_checks::device_continue(),
        Some("Sector-Copy-Device-4Kn-Verify") => sector_copy_checks::device_verify(),
        Some("Sector-Copy-Reread-4Kn-Prepare") => sector_copy_checks::reread_prepare(),
        Some("Sector-Copy-Reread-4Kn-Verify-File") => sector_copy_checks::reread_file_verify(),
        Some("Sector-Copy-Reread-4Kn-Verify-Device") => sector_copy_checks::reread_device_verify(),
        Some("Sector-Copy-Reread-4Kn-Probe") => sector_copy_checks::reread_probe(),
        Some("Sector-Continuation-4Kn-Prepare") => sector_copy_checks::continuation_prepare(),
        Some("Sector-Continuation-4Kn-Interrupted") => {
            sector_copy_checks::continuation_interrupted()
        }
        Some("Sector-Continuation-4Kn-Reread") => sector_copy_checks::continuation_reread(),
        Some("Sector-Continuation-4Kn-Finish") => sector_copy_checks::continuation_finish(),
        Some("Sector-Legacy-4Kn-Prepare") => sector_copy_checks::legacy_prepare(),
        Some("Sector-Legacy-4Kn-File") => sector_copy_checks::legacy_file(),
        Some("Sector-Legacy-4Kn-Rearm") => sector_copy_checks::legacy_rearm(),
        Some("Sector-Legacy-4Kn-Device") => sector_copy_checks::legacy_device(),
        Some("Sector-Image-4Kn-Prepare") => sector_copy_checks::image_prepare(),
        Some("Sector-Image-4Kn-File") => sector_copy_checks::image_file(),
        Some("Sector-Image-4Kn-Device") => sector_copy_checks::image_device(),
        Some("Sector-Copy-4Kn-Verify-Report") => {
            backup_protection::report(sector_copy_checks::VERIFY)
        }
        Some("Allocation-Probe") => allocation_probe::inspect(),
        Some("Letter-Outcome") => letter_outcome_checks::check(),
        Some("Partition-Merge-4Kn-Prepare") => merge_checks::prepare(),
        Some("Partition-Merge-4Kn-Check") => merge_checks::check(),
        Some("Partition-Merge-4Kn-Inspect") => merge_checks::inspect(),
        Some("Partition-Merge-4Kn-Report") => merge_checks::report(),
        Some("Partition-Merge-4Kn-Diagnose") => merge_checks::diagnose(),
        Some("Partition-Merge-4Kn-Continue") => merge_checks::continue_check(),
        Some("Partition-Merge-4Kn-Continue-Report") => merge_checks::continue_report(),
        Some("Partition-Merge-4Kn-Continue-Inspect") => merge_checks::continue_inspect(),
        Some("Partition-Merge-4Kn-Catalogue-Recheck") => merge_checks::catalogue_probe(),
        Some("Partition-Merge-4Kn-Catalogue-Recheck-Report") => {
            merge_checks::catalogue_probe_report()
        }
        Some("Partition-Merge-4Kn-Corrected") => merge_checks::corrected_check(),
        Some("Partition-Merge-4Kn-Corrected-Report") => merge_checks::corrected_report(),
        Some("Partition-Merge-4Kn-Corrected-Inspect") => merge_checks::corrected_inspect(),
        Some("Partition-Merge-4Kn-Owner-Inspect") => merge_checks::inspect_owner_failure(),
        Some("Partition-Merge-4Kn-Fresh-Prepare") => merge_checks::prepare_fresh(),
        Some("Partition-Merge-4Kn-Fresh-Check") => merge_checks::check_fresh(),
        Some("Partition-Merge-4Kn-Fresh-Report") => merge_checks::report_fresh(),
        Some("Partition-Merge-4Kn-Fresh-Inspect") => merge_checks::inspect_fresh(),
        Some("Partition-Move-Prepare") => move_checks::prepare(&move_checks::FIRST),
        Some("Partition-Move-Prepare-Report") => {
            backup_protection::report("partition-move-prepare")
        }
        Some("Partition-Move-Interrupted") => move_checks::inspect(&move_checks::FIRST, true),
        Some("Partition-Move-Inspect") => move_checks::inspect(&move_checks::FIRST, false),
        Some("Partition-Move-Restart-Diagnose") => move_checks::diagnose_restart(),
        Some("Partition-Move-Stop-Prepare") => move_checks::prepare(&move_checks::STOP),
        Some("Partition-Move-Stop-Interrupted") => move_checks::inspect(&move_checks::STOP, true),
        Some("Partition-Move-Stop-Inspect") => move_checks::inspect(&move_checks::STOP, false),
        Some("Partition-Move-4Kn-Exfat-Prepare") => {
            move_checks::prepare_4kn(&move_checks::EXFAT_4KN, true)
        }
        Some("Partition-Move-4Kn-Exfat-Inspect") => {
            move_checks::inspect(&move_checks::EXFAT_4KN, false)
        }
        Some("Partition-Move-4Kn-Ntfs-Prepare") => {
            move_checks::prepare_4kn(&move_checks::NTFS_4KN, false)
        }
        Some("Partition-Move-4Kn-Ntfs-Diagnose") => move_checks::diagnose_4kn_ntfs(),
        Some("Partition-Move-4Kn-Ntfs-Inspect") => {
            move_checks::inspect(&move_checks::NTFS_4KN, false)
        }
        Some("Partition-Move-4Kn-Fat12-Prepare") => move_checks::prepare_existing_4kn_fat(
            &move_checks::FAT12_4KN,
            &move_checks::NTFS_4KN,
            false,
        ),
        Some("Partition-Move-4Kn-Fat12-Continue-Prepare") => move_checks::prepare_existing_4kn_fat(
            &move_checks::FAT12_4KN,
            &move_checks::NTFS_4KN,
            true,
        ),
        Some("Partition-Move-4Kn-Fat12-Inspect") => {
            move_checks::inspect(&move_checks::FAT12_4KN, false)
        }
        Some("Partition-Move-4Kn-Fat32-Prepare" | "Partition-Move-4Kn-Fat32-Range-Prepare") => {
            move_checks::prepare_existing_4kn_fat(
                &move_checks::FAT32_4KN,
                &move_checks::FAT12_4KN,
                false,
            )
        }
        Some("Partition-Move-4Kn-Fat32-Interrupted") => {
            move_checks::inspect(&move_checks::FAT32_4KN, true)
        }
        Some("Partition-Move-4Kn-Fat32-Inspect") => {
            move_checks::inspect(&move_checks::FAT32_4KN, false)
        }
        Some("Quick-Partition") => quick_partition_checks::check(),
        Some("Fat-Resize-Check") => fat_resize_checks::check(),
        Some("Fat-Sectors-512-Check") => fat_sector_checks::check(),
        Some("Fat-Sectors-512-Report") => backup_protection::report("fat-sectors-512"),
        Some("Fat-Sectors-4Kn-Check") => fat_sector_checks::check(),
        Some("Fat-Sectors-4Kn-Diagnose") => fat_sector_checks::diagnose_4kn(),
        Some("Fat-Sectors-4Kn-Continue-Check") => fat_sector_checks::continue_4kn(),
        Some("Fat-Sectors-4Kn-Continue-Report") => {
            backup_protection::report("fat-sectors-4kn-continue")
        }
        Some("Fat-Sectors-4Kn-Tail-Check") => fat_sector_checks::tail_4kn(),
        Some("Fat-Sectors-4Kn-Tail-Report") => {
            backup_protection::report(fat_sector_checks::TAIL_4KN_KIND)
        }
        Some("Fat-Sectors-4Kn-Fat32-Diagnose") => fat_sector_checks::diagnose_4kn_fat32(),
        Some("Fat-Sectors-4Kn-Fat32-Check") => fat_sector_checks::resume_4kn_fat32(),
        Some("Fat-Sectors-4Kn-Fat32-Report") => {
            backup_protection::report(fat_sector_checks::FAT32_4KN_KIND)
        }
        Some("Fat-Sectors-4Kn-Report") => backup_protection::report("fat-sectors-4kn"),
        Some("Fat-Resize-Report") => backup_protection::report("fat-resize"),
        Some("Fat-Relocation-Check") => fat_resize_checks::check_relocation(),
        Some("Fat-Relocation-Report") => backup_protection::report("fat-relocation"),
        Some("Fat-Relocation-Remaining-Check") => fat_resize_checks::continue_relocation(),
        Some("Fat-Relocation-Remaining-Report") => {
            backup_protection::report("fat-relocation-remaining")
        }
        Some("Fat-Capacity-Check") => fat_resize_checks::check_capacity(),
        Some("Fat-Capacity-Report") => backup_protection::report("fat-capacity"),
        Some("Fat-Capacity-Remaining-Check") => fat_resize_checks::continue_capacity(),
        Some("Fat-Capacity-Remaining-Report") => {
            backup_protection::report("fat-capacity-remaining")
        }
        Some("Fat-Capacity-Fat12-Tail-Check") => fat_resize_checks::check_capacity_fat12_tail(),
        Some("Fat-Capacity-Fat12-Tail-Report") => {
            backup_protection::report("fat-capacity-fat12-tail")
        }
        Some("Quick-Progress-Prepare") => quick_partition_checks::prepare_progress_ui(),
        Some("Quick-Progress-Prepare-Report") => {
            backup_protection::report("quick-progress-prepare")
        }
        Some("Quick-Progress-Inspect") => quick_partition_checks::inspect_progress_ui(),
        Some("Quick-Progress-Inspect-Report") => {
            backup_protection::report("quick-progress-inspect")
        }
        Some("Quick-Progress-Recheck") => quick_partition_checks::recheck_progress_ui(),
        Some("Quick-Progress-Recheck-Report") => {
            backup_protection::report("quick-progress-recheck")
        }
        Some("Quick-Partition-Continue") => quick_partition_checks::continue_check(),
        Some("Quick-Partition-Continue-Report") => {
            backup_protection::report("quick-partition-continue")
        }
        Some("Quick-Partition-Report") => backup_protection::report("quick-partition"),
        Some("Quick-Partition-UI-Inspect") => quick_partition_checks::inspect_ui(),
        Some("Quick-Partition-UI-Report") => {
            backup_protection::report("quick-partition-ui-inspect")
        }
        Some("Letter-Outcome-Report") => backup_protection::report("letter-outcome"),
        Some("Letter-Outcome-UI-Inspect") => letter_outcome_checks::inspect_ui(),
        Some("Letter-Outcome-UI-Report") => backup_protection::report("letter-outcome-ui-inspect"),
        Some("Format-Options") => format_options_checks::check(),
        Some("Format-Options-Report") => backup_protection::report("format-options"),
        Some("Format-Inspect") => format_options_checks::inspect(),
        Some("Ntfs-Streams") => ntfs_stream_checks::check(),
        Some("Ntfs-Streams-Continue") => ntfs_stream_checks::continue_existing(),
        Some("Ntfs-Streams-Continue-Report") => backup_protection::report("ntfs-streams-continue"),
        Some("Ntfs-Streams-Inspect") => ntfs_stream_evidence::inspect(),
        Some("Ntfs-Streams-Inspect-Report") => backup_protection::report("ntfs-streams-inspect"),
        Some("Ntfs-Streams-Report") => backup_protection::report("ntfs-streams"),
        Some("Format-Inspect-Report") => backup_protection::report("format-inspect"),
        Some("Format-Options-Continue") => format_options_checks::continue_check(),
        Some("Format-Options-Continue-Report") => {
            backup_protection::report("format-options-continue")
        }
        Some("Backup-Protection") => backup_protection::check(),
        Some("Backup-Protection-Report") => backup_protection::report("protection"),
        Some("Recovery-Output") => recovery_output_checks::check(),
        Some("Recovery-Output-Report") => backup_protection::report("recovery-output"),
        Some("Ntfs-Fixture-Diagnose") => backup_protection::diagnose_ntfs(),
        Some("Ntfs-Fixture-Report") => backup_protection::report("ntfs-fixture"),
        Some("Allocation-Batch") => allocation_batch::check(),
        Some("Allocation-Batch-Report") => allocation_batch::saved_report(),
        Some("Allocation-Continue") => allocation_batch::continue_check(),
        Some("Allocation-Continue-Report") => allocation_batch::saved_report(),
        Some("Device-State-Baseline") => state_checks::baseline(),
        Some("Device-State-Verify") => state_checks::verify(),
        Some("Maintenance-Zero-Verify") => state_checks::verify_maintenance_zero(),
        Some("Maintenance-Inspect") => state_checks::inspect_maintenance(),
        Some("Lost-Fixture-Seed") => lost_fixture::seed(),
        Some("Lost-Fixture-Verify") => lost_fixture::verify(),
        Some("Lost-Fixture-Inspect") => lost_fixture::inspect_seed(),
        Some("Ntfs-Reconstruct-Prepare") => ntfs_reconstruction_checks::prepare(),
        Some("Ntfs-Reconstruct-Inspect") => ntfs_reconstruction_checks::inspect(),
        Some("Ntfs-Reconstruct-Seed") => ntfs_reconstruction_checks::seed_registered(),
        Some("Ntfs-Reconstruct-Verify") => ntfs_reconstruction_checks::verify(),
        Some("Fat-Reconstruct-Prepare") => fat_reconstruction_checks::prepare(),
        Some("Fat-Reconstruct-Verify") => fat_reconstruction_checks::verify(),
        Some("Ext-Reconstruct-Prepare") => ext_reconstruct_checks::prepare(),
        Some("Ext-Reconstruct-Verify") => ext_reconstruct_checks::verify(),
        Some("Lost-Backup-Boot-Prepare") => lost_backup_boot_checks::prepare(),
        Some("Lost-Backup-Boot-Verify") => lost_backup_boot_checks::verify(),
        Some("Lost-Ext4-Prepare") => lost_backup_boot_checks::prepare_ext(),
        Some("Lost-Ext4-Verify") => lost_backup_boot_checks::verify_ext(),
        Some("Retained-Lost-Prepare") => retained_partition_checks::prepare(),
        Some("Retained-Lost-Verify") => retained_partition_checks::verify(),
        Some("Unallocated-Search-Prepare") => unallocated_search_checks::prepare(),
        Some("Unallocated-Search-Verify") => unallocated_search_checks::verify(),
        Some("BitLocker-Marker-Prepare") => bitlocker_checks::prepare(),
        Some("BitLocker-Marker-Verify") => bitlocker_checks::verify(),
        Some("Resize-Clone-Prepare") => resize_clone_checks::prepare(),
        Some("Resize-Clone-Continue") => resize_clone_checks::continue_preparation(),
        Some("Resize-Clone-After-Growth") => resize_clone_checks::verify(true),
        Some("Resize-Clone-Verify") => resize_clone_checks::verify(false),
        Some("Retained-Restore-Prepare") => retained_restore_checks::prepare(),
        Some("Retained-Restore-Assess") => retained_restore_checks::assess_preparation(),
        Some("Retained-Restore-After-Exact") => retained_restore_checks::verify(true),
        Some("Retained-Restore-Verify") => retained_restore_checks::verify(false),
        Some("File-Clone-Prepare") => file_clone_checks::prepare(file_clone_checks::Case::Fat),
        Some("File-Clone-Verify") => file_clone_checks::verify(file_clone_checks::Case::Fat),
        Some("File-Clone-Exfat-Prepare") => {
            file_clone_checks::prepare(file_clone_checks::Case::Exfat)
        }
        Some("File-Clone-Exfat-Continue") => file_clone_checks::continue_exfat_preparation(),
        Some("File-Clone-Exfat-Verify") => {
            file_clone_checks::verify(file_clone_checks::Case::Exfat)
        }
        Some("File-Clone-Ntfs-Prepare") => ntfs_file_clone_checks::prepare(),
        Some("File-Clone-Ntfs-Continue") => ntfs_file_clone_checks::continue_preparation(),
        Some("File-Clone-Ntfs-Assess") => ntfs_file_clone_checks::prepare_seeded(),
        Some("File-Clone-Ntfs-Confirm-Prepared") => {
            ntfs_file_clone_checks::prepare_existing_target()
        }
        Some("File-Clone-Ntfs-Inspect") => ntfs_file_clone_checks::inspect_seeded(),
        Some("File-Clone-Ntfs-Source-Lock") => ntfs_file_clone_checks::source_lock::diagnose(),
        Some("File-Clone-Ntfs-Verify") => ntfs_file_clone_checks::verify(),
        Some("File-Clone-Ntfs-4Kn-Prepare" | "File-Clone-Ntfs-4Kn-Metadata-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(
                if mode.as_deref() == Some("File-Clone-Ntfs-4Kn-Metadata-Prepare") {
                    Case::Metadata
                } else {
                    Case::Standard
                },
            )
        }
        Some("File-Clone-Ntfs-4Kn-Metadata-Continue") => {
            ntfs_file_clone_checks::four_kn::continue_metadata_prepare()
        }
        Some("File-Clone-Ntfs-4Kn-Metadata-Inspect") => {
            ntfs_file_clone_checks::four_kn::inspect_metadata_record()
        }
        Some("File-Clone-Ntfs-4Kn-Metadata-Inspect-Sample") => {
            ntfs_file_clone_checks::four_kn::inspect_metadata_sample()
        }
        Some("File-Clone-Ntfs-4Kn-Storage-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(Case::Storage)
        }
        Some("File-Clone-Ntfs-4Kn-Storage-Baseline") => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(Case::Storage)
        }
        Some("File-Clone-Ntfs-4Kn-Storage-Verify" | "File-Clone-Ntfs-4Kn-Storage-Audit") => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(Case::Storage)
        }
        Some("File-Clone-Ntfs-4Kn-Links-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(Case::Links)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(Case::Wof)
        }
        Some("File-Clone-Ntfs-4Kn-Efs-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(Case::Efs)
        }
        Some("File-Clone-Ntfs-4Kn-Efs-Baseline") => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(Case::Efs)
        }
        Some("File-Clone-Ntfs-4Kn-Efs-Verify") => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(Case::Efs)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Image-Prepare") => {
            use ntfs_file_clone_checks::four_kn::{prepare, Case};
            prepare(Case::WofImage)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Image-Continue") => {
            ntfs_file_clone_checks::four_kn::continue_wof_image_prepare()
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Image-After-Online") => {
            ntfs_file_clone_checks::four_kn::after_wof_image_online()
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Image-Baseline" | "File-Clone-Ntfs-4Kn-Wof-Image-Resume-Baseline" | "File-Clone-Ntfs-4Kn-Wof-Image-Settled-Baseline") => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(Case::WofImage)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Image-Verify" | "File-Clone-Ntfs-4Kn-Wof-Image-Resume-Verify" | "File-Clone-Ntfs-4Kn-Wof-Image-Settled-Verify") => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(Case::WofImage)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Baseline" | "File-Clone-Ntfs-4Kn-Wof-Resume-Baseline") => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(Case::Wof)
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Continue") => {
            ntfs_file_clone_checks::four_kn::continue_wof_prepare()
        }
        Some("File-Clone-Ntfs-4Kn-Wof-Verify" | "File-Clone-Ntfs-4Kn-Wof-Resume-Verify") => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(Case::Wof)
        }
        Some(
            "File-Clone-Ntfs-4Kn-Links-Baseline" | "File-Clone-Ntfs-4Kn-Links-Resume-Baseline",
        ) => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(Case::Links)
        }
        Some(
            "File-Clone-Ntfs-4Kn-Links-Verify"
            | "File-Clone-Ntfs-4Kn-Links-Resume-Verify"
            | "File-Clone-Ntfs-4Kn-Links-Root-Audit",
        ) => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(Case::Links)
        }
        Some("File-Clone-Ntfs-4Kn-Links-Inspect") => {
            ntfs_file_clone_checks::four_kn::inspect_link_record()
        }
        Some("File-Clone-Ntfs-4Kn-Baseline" | "File-Clone-Ntfs-4Kn-Metadata-Baseline") => {
            use ntfs_file_clone_checks::four_kn::{baseline, Case};
            baseline(
                if mode.as_deref() == Some("File-Clone-Ntfs-4Kn-Metadata-Baseline") {
                    Case::Metadata
                } else {
                    Case::Standard
                },
            )
        }
        Some("File-Clone-Ntfs-4Kn-Verify" | "File-Clone-Ntfs-4Kn-Metadata-Verify") => {
            use ntfs_file_clone_checks::four_kn::{verify, Case};
            verify(
                if mode.as_deref() == Some("File-Clone-Ntfs-4Kn-Metadata-Verify") {
                    Case::Metadata
                } else {
                    Case::Standard
                },
            )
        }
        Some("File-Clone-Ntfs-4Kn-Inspect-Source") => {
            ntfs_file_clone_checks::four_kn::inspect_source()
        }
        Some("File-Clone-Ntfs-4Kn-Settled-Baseline") => {
            ntfs_file_clone_checks::four_kn::settled_baseline()
        }
        Some("File-Clone-Ntfs-4Kn-Verify-Settled" | "File-Clone-Ntfs-4Kn-Audit-Settled") => {
            ntfs_file_clone_checks::four_kn::verify_settled()
        }
        Some("--fat-resume-checks" | "Fat-Resume-Checks") => fat_resume_checks::check(),
        Some("--ext-resume-checks" | "Ext-Resume-Checks") => ext_resume_checks::check(),
        Some("--ntfs-resume-seed" | "Ntfs-Resume-Seed") => ntfs_resume_seed::create(),
        Some("--native-fat-finish" | "DiskStudio-Native-FAT-Finish") => {
            format_checks::native_finish()
        }
        Some("--native-fat-report" | "DiskStudio-Native-FAT-Report") => {
            format_checks::native_report()
        }
        Some("--format-report" | "DiskStudio-Format-Report") => format_checks::saved_report(),
        Some("--lock-probe" | "DiskStudio-Lock-Probe") => format_checks::lock_probe(),
        Some("--native-fat" | "DiskStudio-Native-FAT") => format_checks::native_check(),
        Some("--volume-probe" | "DiskStudio-Volume-Probe") => format_checks::volume_probe(),
        Some("--metadata-checks" | "DiskStudio-Metadata-Checks") => metadata_checks::check(),
        Some("--ext-format" | "DiskStudio-Ext-Format") => ext_checks::format(),
        Some("--ext-resize" | "DiskStudio-Ext-Resize") => ext_checks::resize(),
        Some("--format-support" | "DiskStudio-Format-Support") => format_checks::support(),
        Some("--format-checks" | "DiskStudio-Format-Checks") => format_checks::check(),
        Some("--ext4-format" | "DiskStudio-Ext4-Format") => ext4_checks::check(true, false, false),
        Some("--ext4-resume-format" | "DiskStudio-Ext4-Resume") => {
            ext4_checks::check(true, true, false)
        }
        Some("--ext4-resize" | "DiskStudio-Ext4-Resize") => ext4_checks::check(false, false, false),
        Some("--ext4-online" | "DiskStudio-Ext4-Online") => ext4_checks::online_for_ui(),
        Some("--ext4-after-ui" | "DiskStudio-Ext4-After-UI") => {
            ext4_checks::check(false, false, true)
        }
        Some("--advanced-checks" | "DiskStudio-Guest-Checks") => advanced::check(),
        _ => Err("Unknown guest-check mode; no test operation was started".into()),
    }
}

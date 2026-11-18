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
#[path = "../../../../src-tauri/src/ext_recovery.rs"]
mod ext_recovery;
mod ext_resume_checks;
#[path = "../../../../src-tauri/src/extfs.rs"]
mod extfs;
#[path = "../../../../src-tauri/src/fat_recovery.rs"]
mod fat_recovery;
mod fat_resize_checks;
mod fat_resume_checks;
mod fat_sector_checks;
#[path = "../../../../src-tauri/src/fat_volume.rs"]
mod fat_volume;
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
mod lost_fixture;
#[path = "../../../../src-tauri/src/lost_partition_tasks.rs"]
mod lost_partition_tasks;
#[path = "../../../../src-tauri/src/lost_partitions.rs"]
mod lost_partitions;
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
#[path = "../../../../src-tauri/src/ntfs_names.rs"]
mod ntfs_names;
mod ntfs_resume_seed;
#[path = "../../../../src-tauri/src/ntfs_scan.rs"]
mod ntfs_scan;
mod ntfs_stream_checks;
mod ntfs_stream_evidence;
#[path = "../../../../src-tauri/src/ntfs_streams.rs"]
mod ntfs_streams;
#[path = "../../../../src-tauri/src/partition_attributes.rs"]
mod partition_attributes;
#[path = "../../../../src-tauri/src/partition_move.rs"]
mod partition_move;
#[path = "../../../../src-tauri/src/partition_move_task.rs"]
mod partition_move_task;
#[path = "../../../../src-tauri/src/partitions.rs"]
mod partitions;
mod quick_partition_checks;
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
#[path = "../../../../src-tauri/src/surface_scan.rs"]
mod surface_scan;
#[path = "../../../../src-tauri/src/table_archive.rs"]
mod table_archive;
#[path = "../../../../src-tauri/src/table_restore.rs"]
mod table_restore;
#[path = "../../../../src-tauri/src/used_space.rs"]
mod used_space;
use data_tasks::{Selection, Work};
use partitions::policy::{identity, Disk};
use serde::Deserialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex, OnceLock},
    time::Duration,
};
use wmi::WMIConnection;
static OPERATION: Mutex<()> = Mutex::new(());
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
    if matches!(
        mode.as_deref(),
        Some(
            "Fat-Sectors-4Kn-Check"
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
    ) {
        TEST_SECTOR.set(4096).expect("guest sector configured once");
    }
    let result = std::panic::catch_unwind(|| match mode.as_deref() {
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
        Some("Partition-Merge-4Kn-Catalogue-Recheck-Report") => merge_checks::catalogue_probe_report(),
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
    })
    .unwrap_or_else(|_| Err("Guest test panicked; inspect state before retrying".into()));
    notice(&match result{Ok(text)=>text,Err(e)=>format!("GUEST CHECK FAILED\n\n{e}\n\nNo automatic rollback/retry. Partial targets remain offline. Host execution is forbidden.")});
}

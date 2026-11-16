//! Explicitly guest-only acceptance of advanced production engines.
use super::*;
use backup_archive::{Archive, Chain};
use data_tasks::BackupOptions;
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::windows::fs::OpenOptionsExt,
    time::Instant,
};
fn open(path: &Path) -> Result<Archive, String> {
    Archive::open(
        OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(path)
            .map_err(err)?,
    )
}
fn chain(path: &Path, parents: &[&Path]) -> Result<Chain, String> {
    let mut c = Chain::new(open(path)?);
    for p in parents {
        c.add_parent(open(p)?)?;
    }
    c.complete()?;
    Ok(c)
}
fn ensure_a(size: u64, label: &str) -> Result<(u32, PathBuf), String> {
    if let Ok(p) = part(A, size) {
        return Ok(p);
    }
    let status = partitions::status();
    let target = status
        .targets
        .iter()
        .find(|t| t.serial.trim() == A)
        .ok_or("A absent")?;
    let offset = target
        .free_ranges
        .iter()
        .find(|g| g.size >= size)
        .ok_or("No test space")?
        .offset;
    operation(
        A,
        partitions::Action::Create {
            offset,
            size,
            kind: partitions::PartitionKind::Windows,
        },
    )?;
    let snap = storage::read_snapshot()?;
    let s = selection(A, None)?;
    let d = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&s.target.0))
        .ok_or("A missing")?;
    let p = d
        .partitions
        .iter()
        .find(|p| p.base.start == offset && p.base.size == size)
        .ok_or("Created partition missing")?;
    operation(
        A,
        partitions::Action::Format {
            options: Default::default(),
            partition: p.number,
            label: label.into(),
            file_system: partitions::FileSystem::Ntfs,
            cluster_size: 4096,
        },
    )?;
    std::thread::sleep(Duration::from_secs(1));
    part(A, size)
}
fn fat_check(size: u64, filesystem: &str, output: &Path) -> Result<String, String> {
    inventory()?;
    let selected = selection(A, None)?;
    let current = storage::read_snapshot()?;
    let disk = current
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&selected.target.0))
        .ok_or("A missing")?;
    let existing: Vec<_> = disk
        .partitions
        .iter()
        .filter(|p| p.base.size == size && p.base.fs.eq_ignore_ascii_case(filesystem))
        .collect();
    if existing.len() > 1 {
        return Err("Ambiguous existing FAT test fixtures".into());
    }
    let reuse = !existing.is_empty();
    let (number, root) = if let Some(p) = existing.first() {
        let letter = p
            .drive_letter
            .as_deref()
            .filter(|l| l.len() == 1 && l.as_bytes()[0].is_ascii_alphabetic())
            .ok_or("Existing fixture has no drive letter")?;
        (p.number, PathBuf::from(format!("{letter}:\\")))
    } else {
        ensure_a(
            size,
            if filesystem == "FAT32" {
                "DSFAT"
            } else {
                "DSEXFAT"
            },
        )?
    };
    let selection = selection(A, Some(number))?;
    let source = data_windows::resolve(&selection, true, false)?;
    if source
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(A)
        || source.size != size
        || source.volumes.len() != 1
    {
        return Err("FAT fixture target guard failed".into());
    }
    #[derive(Deserialize)]
    #[serde(rename = "MSFT_Volume", rename_all = "PascalCase")]
    struct Volume {
        #[serde(rename = "__PATH")]
        path: String,
        unique_id: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ResultCode {
        return_value: u32,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Input {
        file_system: String,
        file_system_label: String,
        allocation_unit_size: u32,
        full: bool,
        force: bool,
    }
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let volumes: Vec<Volume> = con
        .raw_query("SELECT __PATH,UniqueId FROM MSFT_Volume")
        .map_err(err)?;
    let matches: Vec<_> = volumes
        .iter()
        .filter(|v| v.unique_id.trim_end_matches('\\') == source.volumes[0])
        .collect();
    if matches.len() != 1 || !matches!(filesystem, "FAT32" | "exFAT") {
        return Err("Unsafe fixture format request".into());
    }
    if !reuse {
        let result: ResultCode = con
            .exec_instance_method::<Volume, _>(
                &matches[0].path,
                "Format",
                Input {
                    file_system: filesystem.into(),
                    file_system_label: format!("DS{filesystem}"),
                    allocation_unit_size: if filesystem == "FAT32" { 1024 } else { 4096 },
                    full: false,
                    force: false,
                },
            )
            .map_err(err)?;
        if result.return_value != 0 {
            return Err(format!(
                "Fixture format {filesystem}: {}",
                result.return_value
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let name = format!("recovery-{}-long-name.txt", uuid::Uuid::new_v4().simple());
    let bytes = vec![b'Q'; 100_001];
    {
        let mut f = File::create(root.join(&name)).map_err(err)?;
        f.write_all(&bytes).map_err(err)?;
        f.sync_all().map_err(err)?;
    }
    for deleted in [false, true] {
        if deleted {
            fs::remove_file(root.join(&name)).map_err(err)?;
        }
        let s = super::selection(A, Some(number))?;
        let r = data_windows::resolve(&s, true, false)?;
        let _lock = data_windows::lock(&r)?;
        let mut found = vec![];
        recovery::scan_filesystem(
            &mut data_windows::reader(&r)?,
            recovery::ScanMode::AllFiles,
            |_, _| Ok(()),
            |c| found.push(c),
        )?;
        let c = found
            .iter()
            .find(|c| c.name == name && c.deleted == deleted)
            .ok_or(format!("{filesystem} deleted={deleted} candidate missing"))?;
        let mut actual = vec![];
        recovery::extract(
            &mut data_windows::reader(&r)?,
            c,
            &mut actual,
            u64::MAX,
            |_| Ok(()),
        )?;
        if actual != bytes {
            return Err(format!(
                "{filesystem} deleted={deleted} bytes differ (possibly TRIM)"
            ));
        }
        fs::write(
            output.join(format!("{filesystem}-{deleted}-export.txt")),
            &actual,
        )
        .map_err(err)?;
    }
    Ok(format!(
        "{filesystem}: live + deleted long-name file, 100001 bytes exact"
    ))
}
fn snapshot_count() -> Result<usize, String> {
    #[derive(Deserialize)]
    struct Shadow {
        #[serde(rename = "ID")]
        _id: String,
    }
    Ok(WMIConnection::new()
        .map_err(err)?
        .raw_query::<Shadow>("SELECT ID FROM Win32_ShadowCopy")
        .map_err(err)?
        .len())
}
pub(super) fn check() -> Result<String, String> {
    inventory()?;
    online(A)?;
    online(B)?;
    let before = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS absent")?
        .hardware
        .layout_key;
    notice("Advanced acceptance preflight PASSED. Only DS-TEST-A/B are allowed.\n\nTests: FAT32/exFAT file recovery, VSS with an open source file, parallel compressed full + two incremental backups, restore/readback, cancellation and partition clone.\n\nA and B are disposable. The OS disk is excluded. Click OK to begin.");
    let (a_number, a_root) = part(A, 2 * GIB)?;
    let (_, b_root) = ensure_b(6 * GIB, "DSBackup")?;
    let (b_number, _) = ensure_b(2 * GIB, "DSClone")?;
    let output = b_root.join(format!(
        "DiskStudio-advanced-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir(&output).map_err(err)?;
    let mut report = vec![];
    report.push(fat_check(256 * 1024 * 1024, "FAT32", &output)?);
    report.push(fat_check(512 * 1024 * 1024, "exFAT", &output)?);
    notice("PASS: FAT32 and exFAT live/deleted long-name files recovered with exact byte comparisons.\n\nNext: genuine VSS snapshot while a source file remains open, then compressed/incremental backups.");
    let name = format!("vss-{}.txt", uuid::Uuid::new_v4().simple());
    let file_path = a_root.join(&name);
    let original = vec![b'A'; 1024 * 1024];
    let changed = vec![b'B'; 1024 * 1024];
    let mut held = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&file_path)
        .map_err(err)?;
    held.write_all(&original).map_err(err)?;
    held.sync_all().map_err(err)?;
    let s = selection(A, Some(a_number))?;
    let resolved = data_windows::resolve(&s, true, false)?;
    let previous = snapshot_count()?;
    let snap = snapshot::create(&resolved, || Ok(())).map_err(|e| format!("VSS create: {e}"))?;
    held.seek(SeekFrom::Start(0)).map_err(err)?;
    held.write_all(&changed).map_err(err)?;
    held.sync_all().map_err(err)?;
    let mut candidates = vec![];
    recovery::scan_filesystem(
        &mut snap.reader()?,
        recovery::ScanMode::AllFiles,
        |_, _| Ok(()),
        |c| candidates.push(c),
    )
    .map_err(|e| format!("VSS filesystem scan: {e}"))?;
    let c = candidates
        .iter()
        .find(|c| c.name == name)
        .ok_or("VSS fixture absent")?;
    let mut bytes = vec![];
    recovery::extract(&mut snap.reader()?, c, &mut bytes, u64::MAX, |_| Ok(()))
        .map_err(|e| format!("VSS file extract: {e}"))?;
    if bytes != original {
        return Err("VSS did not retain original content".into());
    }
    drop(snap);
    std::thread::sleep(Duration::from_secs(1));
    if snapshot_count() != Ok(previous) {
        return Err("VSS snapshot was not autoreleased".into());
    }
    let hot = output.join("hot.dsbackup");
    run(Work::ArchiveBackup {
        source: selection(A, Some(a_number))?,
        path: hot.clone(),
        options: BackupOptions {
            compression: true,
            incremental: false,
            hot: true,
            mode: crate::used_space::Mode::AllSectors,
            ..Default::default()
        },
        parent: None,
    })
    .map_err(|e| format!("VSS compressed backup: {e}"))?;
    drop(held);
    report.push("VSS: source held open; snapshot retained old bytes after modification; snapshot autoreleased; hot backup verified".into());
    let full = output.join("full.dsbackup");
    let started = Instant::now();
    run(Work::ArchiveBackup {
        source: selection(A, Some(a_number))?,
        path: full.clone(),
        options: BackupOptions {
            compression: true,
            ..Default::default()
        },
        parent: None,
    })?;
    report.push(format!(
        "2 GiB compressed full: {} bytes, {} seconds incl readback",
        fs::metadata(&full).map_err(err)?.len(),
        started.elapsed().as_secs()
    ));
    fs::write(&file_path, vec![b'C'; 1024 * 1024]).map_err(err)?;
    let inc1 = output.join("incremental-1.dsbackup");
    run(Work::ArchiveBackup {
        source: selection(A, Some(a_number))?,
        path: inc1.clone(),
        options: BackupOptions {
            compression: true,
            incremental: true,
            hot: false,
            mode: crate::used_space::Mode::AllSectors,
            ..Default::default()
        },
        parent: Some(chain(&full, &[])?),
    })?;
    let final_data = vec![b'D'; 1024 * 1024];
    fs::write(&file_path, &final_data).map_err(err)?;
    let inc2 = output.join("incremental-2.dsbackup");
    let (_, expected) = run(Work::ArchiveBackup {
        source: selection(A, Some(a_number))?,
        path: inc2.clone(),
        options: BackupOptions {
            compression: true,
            incremental: true,
            hot: false,
            mode: crate::used_space::Mode::AllSectors,
            ..Default::default()
        },
        parent: Some(chain(&inc1, &[&full])?),
    })?;
    if fs::metadata(&inc1).map_err(err)?.len() >= fs::metadata(&full).map_err(err)?.len() {
        return Err("Incremental unexpectedly larger than full fixture backup".into());
    }
    // Backups live on B. Restore to A so the source file is not on the disk
    // that must be locked/offlined. Change the live file to prove rollback.
    fs::write(&file_path, vec![b'E'; 1024 * 1024]).map_err(err)?;
    let (_, actual) = run(Work::ArchiveRestore {
        destination: selection(A, Some(a_number))?,
        chain: chain(&inc2, &[&inc1, &full])?,
    })?;
    if expected != actual {
        return Err("Incremental restore hash differs".into());
    }
    online(A)?;
    if fs::read(&file_path).map_err(err)? != final_data {
        return Err("Restored file differs from latest incremental state".into());
    }
    report.push(format!("Two incremental layers: {} / {} bytes; 3-file chain restored from B to A, full SHA-256 + file contents match",fs::metadata(&inc1).map_err(err)?.len(),fs::metadata(&inc2).map_err(err)?.len()));
    notice("PASS: VSS consistency + cleanup, compressed full, two incremental layers, full chain restore from B to A and exact restored file.\n\nNext: 2 GiB pipeline clone and cancellation checks.");
    run(Work::Clone {
        mode: crate::used_space::Mode::AllSectors,
        source: selection(A, Some(a_number))?,
        destination: selection(B, Some(b_number))?,
    })?;
    report.push("2 GiB pipelined partition clone + full readback hash; B remains offline".into());
    let canceled = std::sync::Arc::new(AtomicBool::new(true));
    if data_tasks::run(
        "cancel",
        &canceled,
        Work::Clone {
            mode: crate::used_space::Mode::AllSectors,
            source: selection(A, Some(a_number))?,
            destination: selection(B, Some(b_number))?,
        },
    )
    .is_ok()
    {
        return Err("Canceled clone ran".into());
    }
    let after = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS absent")?
        .hardware
        .layout_key;
    if before != after {
        return Err("OS layout changed".into());
    }
    // B is offline now; show the complete report without writing to the OS disk.
    Ok(format!("ALL ADVANCED GUEST CHECKS PASSED\n\n{}\n\nGuest OS layout unchanged. A online, B offline.\nRecovery and backup artifacts are on B's 6 GiB test volume.",report.join("\n\n")))
}

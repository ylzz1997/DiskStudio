//! Consolidated acceptance over the unchanged production modules. Never execute
//! on the host. Synthetic fixture passwords here are not user credentials.
use super::*;
use allocation_batch::{hash_file, record, resolved, root, set_online, tables, write_fixture};
use backup_archive::{Chain, Compression};
use backup_secrets::Purpose;
use sha2::Digest;
use std::{
    fs::File,
    io::Read,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    sync::atomic::Ordering,
};
#[path = "allocation_evidence.rs"]
mod allocation_evidence;
use allocation_evidence::FAILED_HELPER;

const PASSWORD: &str = "DiskStudio synthetic backup fixture 2026";
const CHILD_PASSWORD: &str = "DiskStudio different incremental fixture 2026";
const SIZE: u64 = 2 * 1024 * 1024;
fn stage<T>(
    log: &mut File,
    name: &str,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    record(log, serde_json::json!({"event":"phase","phase":name}))?;
    action().map_err(|e| format!("{name}: {e}"))
}
fn log_path(kind: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-{kind}-{}.jsonl",
        hash_file(&std::env::current_exe().map_err(err)?)?
    )))
}
pub(super) fn begin(kind: &str) -> Result<File, String> {
    inventory()?;
    let desktop = option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        .ok_or("Build must bind the completed desktop SHA-256")?;
    let exe = std::env::current_exe().map_err(err)?;
    if hash_file(
        &exe.parent()
            .ok_or("Media folder absent")?
            .join("DiskStudio.exe"),
    )? != desktop
    {
        return Err("Desktop/payload mismatch; no test dispatched".into());
    }
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path(kind)?)
        .map_err(err)?;
    record(
        &mut log,
        serde_json::json!({"event":"start","desktop":desktop,"helper":hash_file(&exe)?,"kind":kind}),
    )?;
    Ok(log)
}
pub fn report(kind: &str) -> Result<String, String> {
    inventory()?;
    let path = log_path(kind)?;
    if fs::metadata(&path).map_err(err)?.len() > 512 * 1024 {
        return Err("Report too large".into());
    }
    let text = fs::read_to_string(path).map_err(err)?;
    let lines = text
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!("SAVED REPORT ONLY\n\n{lines}"))
}
fn answer<T>(
    purpose: Purpose,
    name: &str,
    password: Option<&str>,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    inventory()?;
    if backup_secrets::status().is_some() {
        return Err("Unexpected pending password request".into());
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let responder = scope.spawn(|| -> Result<(), String> {
            let start = std::time::Instant::now();
            while !done.load(Ordering::SeqCst) {
                if let Some(prompt) = backup_secrets::status() {
                    if prompt.purpose != purpose || prompt.filename != name {
                        let reply = serde_json::from_value(serde_json::json!({"id":prompt.id,"answer":{"kind":"cancel"}})).map_err(err)?;
                        backup_secrets::submit(reply)?;
                        return Err("Unexpected password challenge cancelled".into());
                    }
                    let response = match password {
                        None => serde_json::json!({"kind":"cancel"}),
                        Some(p) => serde_json::json!({"kind":"password","value":p,"confirmation":if purpose==Purpose::Create {Some(p)} else {None}}),
                    };
                    backup_secrets::submit(serde_json::from_value(serde_json::json!({"id":prompt.id,"answer":response})).map_err(err)?)?;
                    return Ok(());
                }
                if start.elapsed() > Duration::from_secs(180) { return Err("No expected password challenge".into()); }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err("Operation ended before expected password challenge".into())
        });
        let result = action();
        done.store(true, Ordering::SeqCst);
        let response = responder
            .join()
            .map_err(|_| "Password responder failed".to_string())
            .and_then(|r| r);
        // Preserve an operation's real failure if it happened before a prompt;
        // a responder timeout must not erase its original phase/cause.
        match (result, response) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    })
}
fn open(
    path: &Path,
    other: &partitions::Target,
    password: &str,
) -> Result<backup_archive::Archive, String> {
    let input = backup_files::Input::open(path, Some(other))?;
    if &input.magic != backup_encryption::MAGIC {
        return Err("Expected encrypted envelope".into());
    }
    let name = input.name.clone();
    answer(Purpose::Unlock, &name, Some(password), || {
        input.archive(|| Ok(()))
    })?
    .ok_or("Unexpected cancellation".into())
}
fn output() -> Result<PathBuf, String> {
    let (number, _) = part(B, 6 * GIB)?;
    let s = selection(B, Some(number))?;
    let r = data_windows::resolve(&s, true, false)?;
    if r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(B) || r.size != 6 * GIB {
        return Err("Output is not B 6 GiB".into());
    }
    let base = root(&r)?;
    let dir =
        cap_std::fs::Dir::open_ambient_dir(&base, cap_std::ambient_authority()).map_err(err)?;
    let binding =
        data_windows::FileBinding::new(&dir.try_clone().map_err(err)?.into_std_file(), &s.target)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    let name = format!("Backup-Protection-{}", uuid::Uuid::new_v4().simple());
    dir.create_dir(&name).map_err(err)?;
    let child = dir.open_dir(&name).map_err(err)?.into_std_file();
    data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(&child, &s.target)?])?;
    Ok(base.join(name))
}
fn backup(
    s: &Selection,
    path: &Path,
    compression: Compression,
    parent: Option<Chain>,
    password: &str,
) -> Result<String, String> {
    let options = data_tasks::BackupOptions {
        encryption: true,
        compression: compression != Compression::None,
        compression_level: compression,
        incremental: parent.is_some(),
        ..Default::default()
    };
    let (_, hash) = answer(
        Purpose::Create,
        &path.file_name().unwrap().to_string_lossy(),
        Some(password),
        || {
            run(Work::ArchiveBackup {
                source: s.clone(),
                path: path.to_path_buf(),
                options,
                parent,
            })
        },
    )?;
    if path.with_extension("dsbackup.partial").exists() || !path.exists() {
        return Err("Unexpected final/partial publication state".into());
    }
    hash.ok_or("Backup digest absent".into())
}
pub fn check() -> Result<String, String> {
    let mut log = begin("protection")?;
    notice("Backup protection batch on isolated A/B only.\n\nEncrypted full/high, incremental/fast, uncompressed; password cancellation and rejection; complete chain verification; restored bytes; unchanged GPT/layout. No intermediate prompts. Any error stops, no retry or rollback.\n\nSynthetic passwords belong only to these test files.");
    let result = (|| -> Result<String, String> {
        let before_layouts = stage(&mut log, "baseline-layouts", || format_checks::layouts(&[]))?;
        let before_tables = stage(&mut log, "baseline-gpt", tables)?;
        stage(&mut log, "prepare-A-online", || set_online(A))?;
        stage(&mut log, "prepare-B-online", || set_online(B))?;
        let (s, offset) = stage(
            &mut log,
            "select-FAT12-source",
            allocation_batch::fat12_selection,
        )?;
        let output = stage(&mut log, "create-fresh-B-output-folder", output)?;
        let name = format!(
            "{}.BIN",
            &uuid::Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase()
        );
        let first = (0..65553)
            .map(|i| ((i * 37 + i / 19) % 251) as u8)
            .collect::<Vec<_>>();
        let second = first.iter().map(|b| b ^ 0x67).collect::<Vec<_>>();
        record(
            &mut log,
            serde_json::json!({"event":"fixture-begin","sentinel":name,"partition":s.partition,"offset":offset,"size":SIZE,"output":output}),
        )?;
        stage(&mut log, "create-fixture", || {
            write_fixture(&s, offset, SIZE, &name, &first, true)
        })?;
        let full = output.join("protected-full.dsbackup");
        let full_hash = stage(&mut log, "encrypted-full-high", || {
            backup(&s, &full, Compression::High, None, PASSWORD)
        })?;
        record(
            &mut log,
            serde_json::json!({"event":"full-pass","sha256":full_hash}),
        )?;
        record(
            &mut log,
            serde_json::json!({"event":"phase","phase":"wrong-password"}),
        )?;
        match open(&full, &s.target, "synthetic wrong password") {
            Err(error) if error.contains("备份密码不正确") => (),
            _ => return Err("Wrong-password rejection did not reach authentication check".into()),
        }
        let cancelled = output.join("cancelled.dsbackup");
        record(
            &mut log,
            serde_json::json!({"event":"phase","phase":"password-cancellation"}),
        )?;
        let cancelled_result = answer(Purpose::Create, "cancelled.dsbackup", None, || {
            run(Work::ArchiveBackup {
                source: s.clone(),
                path: cancelled.clone(),
                options: data_tasks::BackupOptions {
                    encryption: true,
                    ..Default::default()
                },
                parent: None,
            })
        });
        if cancelled_result
            .as_ref()
            .err()
            .is_none_or(|e| !e.contains("已取消加密备份"))
            || cancelled.exists()
            || cancelled.with_extension("dsbackup.partial").exists()
        {
            return Err("Cancelled password did not stop before output creation".into());
        }
        record(
            &mut log,
            serde_json::json!({"event":"password-rejection-cancellation-pass","noRestoreDispatched":true}),
        )?;
        stage(&mut log, "update-fixture-for-incremental", || {
            write_fixture(&s, offset, SIZE, &name, &second, false)
        })?;
        let parent = Chain::new(open(&full, &s.target, PASSWORD)?);
        let inc = output.join("protected-incremental.dsbackup");
        let expected = stage(&mut log, "encrypted-incremental-fast", || {
            backup(&s, &inc, Compression::Fast, Some(parent), CHILD_PASSWORD)
        })?;
        let raw = output.join("protected-uncompressed.dsbackup");
        stage(&mut log, "encrypted-uncompressed-full", || {
            backup(&s, &raw, Compression::None, None, PASSWORD)
        })?;
        record(
            &mut log,
            serde_json::json!({"event":"phase","phase":"chain-browser-verification"}),
        )?;
        let mut chain = Chain::new(open(&inc, &s.target, CHILD_PASSWORD)?);
        chain.add_parent(open(&full, &s.target, PASSWORD)?)?;
        chain.verify(|_| Ok(()))?;
        let browser = backup_browser::Browser::from_chain(
            chain,
            "protected-incremental.dsbackup".into(),
            |_, _| Ok(()),
        )?;
        // Browser parsing and complete chain verification use production code.
        if browser.info.partitions.len() != 1 {
            return Err("Partition backup browser scope differs".into());
        }
        let mut reader = browser.reader(browser.info.partitions[0].id)?;
        if data_io::hash_exact(&mut reader, SIZE, |_| Ok(()))? != expected {
            return Err("Browser logical bytes differ".into());
        }
        drop(reader);
        drop(browser);
        record(
            &mut log,
            serde_json::json!({"event":"incremental-uncompressed-browser-pass","sha256":expected}),
        )?;
        stage(&mut log, "prepare-restore-fixture", || {
            write_fixture(&s, offset, SIZE, &name, &first, false)
        })?;
        let mut chain = Chain::new(open(&inc, &s.target, CHILD_PASSWORD)?);
        chain.add_parent(open(&full, &s.target, PASSWORD)?)?;
        let (_, actual) = stage(&mut log, "encrypted-incremental-restore", || {
            run(Work::ArchiveRestore {
                destination: s.clone(),
                chain,
            })
        })?;
        if actual.as_deref() != Some(&expected) {
            return Err("Encrypted restore readback hash differs".into());
        }
        let sentinel_hash = stage(&mut log, "independent-restored-file-verification", || {
            allocation_batch::check_file(&selection(A, s.partition)?, offset, SIZE, &name, &second)
        })?;
        record(
            &mut log,
            serde_json::json!({"event":"phase","phase":"final-layout-GPT-offline-verification"}),
        )?;
        if format_checks::layouts(&[])? != before_layouts || tables()? != before_tables {
            return Err("GPT/provider layout changed".into());
        }
        let disk = inventory()?
            .into_iter()
            .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
            .ok_or("A absent")?;
        if disk.is_offline != Some(true) {
            return Err("Restored disk did not remain offline".into());
        }
        record(
            &mut log,
            serde_json::json!({"event":"passed","sentinel":name,"sentinelSha256":sentinel_hash,"restoreSha256":actual,"allLayoutsUnchanged":true,"gptUnchanged":true,"diskLeftOffline":true,"output":output}),
        )?;
        Ok("BACKUP PROTECTION BATCH PASSED\n\nEncrypted full/high, incremental/fast, uncompressed; wrong password and cancellation; browser/complete chain; restore SHA-256 and 65,553 sentinel bytes; all GPT/layouts unchanged. A remains offline.\n\nProduction-source integration evidence; native desktop UI evidence is separate.".into())
    })();
    if let Err(error) = &result {
        record(
            &mut log,
            serde_json::json!({"event":"failed","error":error,"retry":false,"rollback":false}),
        )?;
    }
    result
}

fn ntfs_candidates(
    r: &data_windows::Resolved,
    name: &str,
    expected: &[u8],
) -> Result<serde_json::Value, String> {
    if r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(A)
        || r.offset != 16 * 1024 * 1024
        || r.size != 2 * GIB
    {
        return Err("Diagnostic target differs".into());
    }
    let mut reader = data_windows::reader(r)?;
    let mut found = vec![];
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |f| {
            if !f.deleted && f.name.eq_ignore_ascii_case(name) {
                found.push(f);
            }
        },
        |_| Ok(()),
    )?;
    let mut reports = vec![];
    for candidate in found {
        let identity = match &candidate.locator {
            recovery::Locator::Ntfs {
                record,
                sequence,
                link,
                ..
            } => serde_json::json!({"record":record,"sequence":sequence,"link":link}),
            _ => return Err("Expected NTFS locator".into()),
        };
        let mut bytes = vec![];
        let extracted = recovery::extract(&mut reader, &candidate, &mut bytes, 65554, |_| Ok(()));
        reports.push(serde_json::json!({"size":candidate.size,"identity":identity,"allBytesMatch":extracted.is_ok() && bytes==expected,"error":extracted.err()}));
    }
    Ok(serde_json::json!({"count":reports.len(),"candidates":reports}))
}
pub fn diagnose_ntfs() -> Result<String, String> {
    let mut log = begin("ntfs-fixture")?;
    let result = (|| -> Result<String, String> {
        let path = PathBuf::from(format!(
            r"C:\ProgramData\DiskStudio-allocation-batch-{FAILED_HELPER}.jsonl"
        ));
        let mut evidence = OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        let metadata = evidence.metadata().map_err(err)?;
        if !metadata.is_file()
            || metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            || metadata.len() > 512 * 1024
        {
            return Err("Previous log too large".into());
        }
        let mut text = String::new();
        (&mut evidence)
            .take(512 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(err)?;
        let name = allocation_evidence::fixture_name(&text)?;
        record(
            &mut log,
            serde_json::json!({"event":"saved-fixture-bound","sentinel":name,"sourceHelper":FAILED_HELPER,"sourceLogSha256":format!("{:x}",sha2::Sha256::digest(text.as_bytes()))}),
        )?;
        let s = selection(A, Some(2))?;
        let r = resolved(&s, 16 * 1024 * 1024, 2 * GIB, true)?;
        let expected = (0..65553)
            .map(|i| ((i * 31 + i / 17 + 4) % 251) as u8)
            .collect::<Vec<_>>();
        {
            let dir = cap_std::fs::Dir::open_ambient_dir(root(&r)?, cap_std::ambient_authority())
                .map_err(err)?;
            let parent = data_windows::FileBinding::new(
                &dir.try_clone().map_err(err)?.into_std_file(),
                &s.target,
            )?;
            data_windows::FileBinding::validate_all(&[parent])?;
            let mut f = dir.open(&name).map_err(err)?.into_std();
            let binding = data_windows::FileBinding::new(&f, &s.target)?;
            data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
            if f.metadata().map_err(err)?.len() != 65553 {
                return Err("OS file size differs".into());
            }
            let mut bytes = vec![0; 65553];
            f.read_exact(&mut bytes).map_err(err)?;
            if bytes != expected {
                return Err("OS file bytes differ".into());
            }
            data_windows::FileBinding::validate_all(&[binding])?;
        }
        let before = ntfs_candidates(&r, &name, &expected)?;
        record(
            &mut log,
            serde_json::json!({"event":"before-volume-lock","osFileAllBytesMatch":true,"raw":before}),
        )?;
        // FSCTL_LOCK_VOLUME flushes filesystem metadata. This is explicitly a
        // guest A-only cache/identity diagnostic, not a claim of zero writes.
        let locks = data_windows::lock(&r)?;
        let fresh = data_windows::resolve_locked(&s, false, &r, &locks)?;
        let after = ntfs_candidates(&fresh, &name, &expected)?;
        drop(locks);
        record(
            &mut log,
            serde_json::json!({"event":"diagnosed","afterVolumeLock":after,"noBackupWipeRestoreDispatched":true,"fixtureNotRewritten":true}),
        )?;
        drop(evidence);
        Ok(format!("NTFS fixture diagnostic only\n\nOS file: all 65,553 bytes match.\nBefore lock: {before}\nAfter lock/metadata flush: {after}\n\nNo fixture rewrite, backup, wipe or restore dispatched. Candidate identities remain separate."))
    })();
    if let Err(error) = &result {
        record(
            &mut log,
            serde_json::json!({"event":"failed","error":error,"retry":false}),
        )?;
    }
    result
}

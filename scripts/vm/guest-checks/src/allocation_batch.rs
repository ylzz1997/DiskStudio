//! Guest-only matrix acceptance. Production modules above are compiled without
//! edits. The production desktop executable is separately hash-bound and its
//! native UI acceptance remains a distinct record. Never run this on the host.
use super::*;
use backup_archive::{Archive, Chain};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::windows::fs::OpenOptionsExt,
};

const DESKTOP: &str = "538afa2b0066c9bfce8436d403e0bbdbfd495b518fd39cbb5b7680f26824bfca";
const PREVIOUS: &str = "e363a460de281ab85d5769a50785ff1a34a282313c1409f389de0ea0e034f2d3";
const MIB: u64 = 1024 * 1024;
struct Case {
    fs: &'static str,
    label: Option<&'static str>,
    size: u64,
    random: bool,
    passes: u8,
}
const CASES: [Case; 5] = [
    Case {
        fs: "FAT12",
        label: Some("DSFAT12"),
        size: 2 * MIB,
        random: false,
        passes: 2,
    },
    Case {
        fs: "FAT16",
        label: Some("DSFAT16"),
        size: 128 * MIB,
        random: true,
        passes: 2,
    },
    Case {
        fs: "FAT32",
        label: Some("DSFAT32"),
        size: 256 * MIB,
        random: false,
        passes: 1,
    },
    Case {
        fs: "exFAT",
        label: Some("DSexFAT"),
        size: 512 * MIB,
        random: true,
        passes: 1,
    },
    Case {
        fs: "NTFS",
        label: None,
        size: 2 * GIB,
        random: false,
        passes: 1,
    },
];

pub(super) fn hash_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(err)?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0; 1024 * 1024];
    loop {
        let n = f.read(&mut bytes).map_err(err)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn log_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(err)?;
    Ok(PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-allocation-batch-{}.jsonl",
        hash_file(&exe)?
    )))
}
pub(super) fn record(log: &mut File, mut event: serde_json::Value) -> Result<(), String> {
    event["recordedAtUnixMs"] = serde_json::json!(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(err)?
        .as_millis());
    serde_json::to_writer(&mut *log, &event).map_err(err)?;
    log.write_all(b"\n").map_err(err)?;
    log.sync_all().map_err(err)
}
pub fn saved_report() -> Result<String, String> {
    inventory()?;
    let file = log_path()?;
    if fs::metadata(&file).map_err(err)?.len() > 512 * 1024 {
        return Err("Log too large".into());
    }
    let text = fs::read_to_string(file).map_err(err)?;
    let lines = text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let passed = lines
        .iter()
        .filter(|v| matches!(v["event"].as_str(), Some("case-pass" | "prior-pass")))
        .map(|v| {
            format!(
                "{}: {}",
                v["fs"].as_str().unwrap_or("?"),
                v["event"].as_str().unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tail = lines
        .iter()
        .filter(|v| v["event"] != "free-wipe-result")
        .map(|v| serde_json::to_string(v).map_err(err))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!(
        "SAVED REPORT ONLY — no device job dispatched\n\nCompleted records:\n{passed}\n\nLatest events:\n{tail}"
    ))
}
fn phase(log: &mut File, fs: &str, phase: &str) -> Result<(), String> {
    record(
        log,
        serde_json::json!({"event":"phase","fs":fs,"phase":phase}),
    )
}

// A narrowly defined continuation of an observed terminal success checkpoint.
// A failure, interrupted case, unexpected event or other source binary refuses
// continuation. No failed mutation is repeated and no old log is overwritten.
fn previous_pass() -> Result<(serde_json::Value, serde_json::Value, String), String> {
    inventory()?;
    let path = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-allocation-batch-{PREVIOUS}.jsonl"
    ));
    let mut f = OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(path)
        .map_err(err)?;
    if f.metadata().map_err(err)?.len() > 512 * 1024 {
        return Err("Previous log too large".into());
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(err)?;
    let lines = std::str::from_utf8(&bytes)
        .map_err(err)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let expected = [
        "start",
        "case-begin",
        "full-backup-pass",
        "free-wipe-result",
        "case-pass",
    ];
    if lines.len() != expected.len()
        || lines.iter().zip(expected).any(|(v, e)| v["event"] != e)
        || lines[0]["desktop"] != DESKTOP
        || lines[0]["helper"] != PREVIOUS
        || lines[1..].iter().any(|v| v["fs"] != "FAT12")
    {
        return Err(
            "Previous batch did not stop exactly after its FAT12 pass; refuse continuation".into(),
        );
    }
    let pass = &lines[4];
    let report = &lines[3]["report"];
    let hash = |v: &serde_json::Value| {
        v.as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    if pass["gptUnchanged"] != true
        || pass["allLayoutsUnchanged"] != true
        || pass["sentinelBytes"] != 65553
        || pass["passes"] != 2
        || pass["random"] != false
        || !hash(&pass["sentinelSha256"])
        || !hash(&pass["restoreSha256"])
        || report["state"] != "completed"
        || report["diskLeftOffline"] != true
        || report["size"] != 2 * MIB
        || report["freeSpace"]["filesystem"] != "FAT12"
        || report["wipe"]["verifiedPasses"] != 2
        || !hash(&report["freeSpace"]["preservedSha256"])
        || report["wipe"]["writtenBytes"] != report["wipe"]["readbackBytes"]
    {
        return Err("Previous FAT12 success proof incomplete".into());
    }
    let name = pass["sentinel"]
        .as_str()
        .ok_or("Previous sentinel absent")?;
    if name.len() != 12
        || !name.ends_with(".BIN")
        || !name.as_bytes()[..8].iter().all(u8::is_ascii_hexdigit)
    {
        return Err("Previous sentinel name invalid".into());
    }
    let number = u32::try_from(
        report["source"]["partition"]
            .as_u64()
            .ok_or("Partition absent")?,
    )
    .map_err(err)?;
    let offset = report["offset"].as_u64().ok_or("Offset absent")?;
    let expected = (0..65553)
        .map(|i| (((i * 31 + i / 17) % 251) as u8) ^ 0x5a)
        .collect::<Vec<_>>();
    let actual = check_file(
        &selection(A, Some(number))?,
        offset,
        2 * MIB,
        name,
        &expected,
    )?;
    if pass["sentinelSha256"] != actual {
        return Err("Previous FAT12 sentinel changed after restart".into());
    }
    Ok((
        pass.clone(),
        report.clone(),
        format!("{:x}", Sha256::digest(&bytes)),
    ))
}
pub(super) fn set_online(serial: &str) -> Result<(), String> {
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or("Disk absent")?;
    if d.is_offline == Some(false) {
        return Ok(());
    }
    let p = device_state::prepare(device_state::Request {
        source: selection(serial, None)?,
        action: device_state::Action::Online,
    })?;
    device_state::execute(p.token)?;
    let after = inventory()?;
    if !after.iter().any(|d| {
        d.serial_number.as_deref().map(disk_identity::clean) == Some(serial)
            && d.is_offline == Some(false)
    }) {
        return Err("Online postcondition missing".into());
    }
    Ok(())
}
fn selected(case: &Case) -> Result<(Selection, u64), String> {
    fixture_selection(case.fs, case.label, case.size)
}

pub(super) fn fixture_selection(
    filesystem: &str,
    label: Option<&str>,
    size: u64,
) -> Result<(Selection, u64), String> {
    inventory()?;
    let s = selection(A, None)?;
    let snap = storage::read_snapshot()?;
    let disk = snap
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&s.target.0))
        .ok_or("A absent")?;
    let candidates = disk
        .partitions
        .iter()
        .filter(|p| {
            p.base.size == size
                && !p.base.system
                && label.is_none_or(|name| {
                    p.volume_label
                        .as_deref()
                        .is_some_and(|v| v.eq_ignore_ascii_case(name))
                })
                && (p.base.fs.eq_ignore_ascii_case(filesystem)
                    || matches!(filesystem, "FAT12" | "FAT16")
                        && p.base.fs.eq_ignore_ascii_case("FAT"))
        })
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err(format!(
            "Expected one {} fixture with label {:?}, found {}",
            filesystem,
            label,
            candidates.len()
        ));
    }
    let p = candidates[0];
    Ok((selection(A, Some(p.number))?, p.base.start))
}
pub(super) fn fat12_selection() -> Result<(Selection, u64), String> {
    selected(&CASES[0])
}
pub(super) fn resolved(
    s: &Selection,
    offset: u64,
    size: u64,
    stable: bool,
) -> Result<data_windows::Resolved, String> {
    inventory()?;
    let r = data_windows::resolve(s, stable, false)?;
    if r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(A)
        || r.offset != offset
        || r.size != size
        || r.disk.logical_sector_size != Some(test_sector())
        || r.disk.is_boot != Some(false)
        || r.disk.is_system != Some(false)
    {
        return Err("A fixture identity changed".into());
    }
    Ok(r)
}
pub(super) fn root(r: &data_windows::Resolved) -> Result<PathBuf, String> {
    if r.volumes.len() != 1
        || r.disk.is_offline != Some(false)
        || r.disk.is_read_only != Some(false)
    {
        return Err("Fixture must have one writable online volume".into());
    }
    Ok(PathBuf::from(format!("{}\\", r.volumes[0])))
}
pub(super) fn write_fixture(
    s: &Selection,
    offset: u64,
    size: u64,
    name: &str,
    bytes: &[u8],
    new: bool,
) -> Result<(), String> {
    let r = resolved(s, offset, size, true)?;
    let directory =
        cap_std::fs::Dir::open_ambient_dir(root(&r)?, cap_std::ambient_authority()).map_err(err)?;
    let parent = data_windows::FileBinding::new(
        &directory.try_clone().map_err(err)?.into_std_file(),
        &s.target,
    )?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&parent))?;
    use cap_std::fs::OpenOptionsExt;
    let mut opts = cap_std::fs::OpenOptions::new();
    opts.read(true)
        .write(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    if new {
        opts.create_new(true);
    }
    let mut f = directory.open_with(name, &opts).map_err(err)?.into_std();
    let binding = data_windows::FileBinding::new(&f, &s.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    if !new && f.metadata().map_err(err)?.len() != bytes.len() as u64 {
        return Err("Fixture size changed before update".into());
    }
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    f.write_all(bytes).map_err(err)?;
    f.sync_all().map_err(err)?;
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut check = vec![0; bytes.len()];
    f.read_exact(&mut check).map_err(err)?;
    if check != bytes {
        return Err("Fixture write/readback differs".into());
    }
    data_windows::FileBinding::validate_all(&[parent, binding])
}
pub(super) fn check_file(
    s: &Selection,
    offset: u64,
    size: u64,
    name: &str,
    expected: &[u8],
) -> Result<String, String> {
    // Separate directory/file extraction engine compares every known byte,
    // including the partial final cluster; it never reads via the wipe plan.
    let r = resolved(s, offset, size, false)?;
    let mut reader = data_windows::reader(&r)?;
    let mut files = vec![];
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |f| files.push(f),
        |_| Ok(()),
    )?;
    let found = files
        .iter()
        .filter(|f| !f.deleted && f.name.eq_ignore_ascii_case(name))
        .collect::<Vec<_>>();
    if found.len() != 1 || found[0].size != expected.len() as u64 {
        return Err("Sentinel file missing or changed size".into());
    }
    let mut actual = vec![];
    recovery::extract(&mut reader, found[0], &mut actual, u64::MAX, |_| Ok(()))?;
    if actual != expected {
        return Err("Independent full sentinel contents differ".into());
    }
    Ok(format!("{:x}", Sha256::digest(actual)))
}
fn chain(path: &Path, parent: Option<&Path>, b: &partitions::Target) -> Result<Chain, String> {
    let open = |p: &Path| -> Result<Archive, String> {
        let f = OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(p)
            .map_err(err)?;
        let binding = data_windows::FileBinding::new(&f, b)?;
        data_windows::FileBinding::validate_all(&[binding])?;
        Archive::open(f)
    };
    let mut c = Chain::new(open(path)?);
    if let Some(p) = parent {
        c.add_parent(open(p)?)?;
    }
    c.complete()?;
    if c.header().version != 3
        || c.header().allocation.is_none()
        || c.header().parent.is_some() != parent.is_some()
    {
        return Err("Expected an allocation-aware v3 archive with exact parent scope".into());
    }
    Ok(c)
}
pub(super) fn tables() -> Result<Vec<String>, String> {
    let mut hashes = vec![];
    for serial in [A, B] {
        inventory()?;
        let s = selection(serial, None)?;
        let ctx = partitions::platform::table_inspect_context(&s)?;
        let mut file = data_windows::device(&ctx.resolved, false)?;
        let table =
            table_archive::capture(&mut file, 16 * GIB, 512, s.target.0.clone(), false, || {
                Ok(())
            })?;
        if !table_restore::archive_layout(&table)?.agrees(
            &extfs::restore_native(&file, Some(&ctx.parts.iter().collect::<Vec<_>>()), 512)?.0,
        ) {
            return Err("Raw/native GPT mismatch".into());
        }
        hashes.push(format!("{:x}", Sha256::digest(table.encode()?)));
    }
    Ok(hashes)
}

pub fn check() -> Result<String, String> {
    check_impl(false)
}
pub fn continue_check() -> Result<String, String> {
    check_impl(true)
}
fn check_impl(continuation: bool) -> Result<String, String> {
    inventory()?;
    let exe = std::env::current_exe().map_err(err)?;
    if hash_file(
        &exe.parent()
            .ok_or("No media folder")?
            .join("DiskStudio.exe"),
    )? != DESKTOP
    {
        return Err("Desktop executable differs from this batch's source revision".into());
    }
    let prior = if continuation {
        Some(previous_pass()?)
    } else {
        None
    };
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path()?)
        .map_err(|e| format!("Run record must be new; do not rerun a partial batch: {e}"))?;
    record(
        &mut log,
        serde_json::json!({"event":"start","desktop":DESKTOP,"helper":hash_file(&exe)?}),
    )?;
    if let Some((pass, report, hash)) = &prior {
        record(
            &mut log,
            serde_json::json!({"event":"prior-pass","fs":"FAT12","previousHelper":PREVIOUS,"previousLogSha256":hash,"pass":pass,"report":report,"sentinelRecheckedAfterRestart":true}),
        )?;
    }
    notice(&format!("Allocation matrix acceptance: isolated DS-TEST-A/B only.\n\n{}\n\nEach case: compressed used-space full + incremental backup, free wipe/readback, exact sentinel comparison, incremental restore/readback, unchanged GPT and layouts. Fresh files only; no partition creation/format/delete.\n\nCases continue without intermediate prompts. Any error stops the batch without retry/rollback. A stays offline at completion. Production UI and source-integration results are separate evidence.\n\nClick OK to begin.",if continuation {"Previous FAT12 pass and every sentinel byte revalidated after restart. Continue FAT16, FAT32, exFAT, NTFS only; do not repeat FAT12 mutations."} else {"New FAT12, FAT16, FAT32, exFAT and NTFS cases."}));
    let result = (|| -> Result<String, String> {
        let baseline = format_checks::layouts(&[])?;
        let before = tables()?;
        record(
            &mut log,
            serde_json::json!({"event":"baseline","gptSha256":before,"layouts":baseline}),
        )?;
        set_online(A)?;
        set_online(B)?;
        let (output_number, _) = part(B, 6 * GIB)?;
        let bs = selection(B, Some(output_number))?;
        let br = data_windows::resolve(&bs, true, false)?;
        if br.disk.serial_number.as_deref().map(disk_identity::clean) != Some(B)
            || br.size != 6 * GIB
        {
            return Err("Output must be B 6 GiB volume".into());
        }
        let base = root(&br)?;
        let parent =
            cap_std::fs::Dir::open_ambient_dir(&base, cap_std::ambient_authority()).map_err(err)?;
        let parent_binding = data_windows::FileBinding::new(
            &parent.try_clone().map_err(err)?.into_std_file(),
            &bs.target,
        )?;
        data_windows::FileBinding::validate_all(std::slice::from_ref(&parent_binding))?;
        let folder = format!("Allocation-{}", uuid::Uuid::new_v4().simple());
        parent.create_dir(&folder).map_err(err)?;
        let output = base.join(&folder);
        let directory = parent.open_dir(&folder).map_err(err)?.into_std_file();
        let binding = data_windows::FileBinding::new(&directory, &bs.target)?;
        data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
        let mut successes = if continuation {
            vec!["FAT12 (prior run; sentinel rechecked)"]
        } else {
            vec![]
        };
        for (index, case) in CASES.iter().enumerate() {
            if continuation && index == 0 {
                continue;
            }
            record(
                &mut log,
                serde_json::json!({"event":"case-begin","fs":case.fs}),
            )?;
            set_online(A)?;
            let (s, offset) = selected(case)?;
            let number = s.partition.ok_or("Partition required")?;
            let name = format!("{}.BIN", &uuid::Uuid::new_v4().simple().to_string()[..8])
                .to_ascii_uppercase();
            let first = (0..65553)
                .map(|i| ((i * 31 + i / 17 + index) % 251) as u8)
                .collect::<Vec<_>>();
            let second = first.iter().map(|b| b ^ 0x5a).collect::<Vec<_>>();
            record(
                &mut log,
                serde_json::json!({"event":"fixture-begin","fs":case.fs,"partition":number,"offset":offset,"size":case.size,"sentinel":name}),
            )?;
            write_fixture(&s, offset, case.size, &name, &first, true)?;
            check_file(&s, offset, case.size, &name, &first)?;
            let full = output.join(format!("{}-full.dsbackup", case.fs));
            let inc = output.join(format!("{}-incremental.dsbackup", case.fs));
            let r = resolved(&s, offset, case.size, true)?;
            let locks = data_windows::lock(&r)?;
            let fresh = resolved(&s, offset, case.size, true)?;
            let device = data_windows::device(&fresh, false)?;
            let mut view =
                used_space::View::bounded(device, offset, case.size, 16 * GIB).map_err(err)?;
            let plan =
                used_space::inspect(&mut view, case.size, 512, None, &AtomicBool::new(false))?;
            if plan.volumes.len() != 1 || plan.volumes[0].filesystem != case.fs || plan.skipped == 0
            {
                return Err("Actual allocation filesystem differs or has no free clusters".into());
            }
            plan.proof.verify(&mut view, &AtomicBool::new(false))?;
            drop(view);
            drop(locks);
            let options = data_tasks::BackupOptions {
                compression: true,
                incremental: false,
                hot: false,
                mode: used_space::Mode::UsedSpace,
                ..Default::default()
            };
            phase(&mut log, case.fs, "full-backup")?;
            let (_, full_hash) = run(Work::ArchiveBackup {
                source: s.clone(),
                path: full.clone(),
                options: options.clone(),
                parent: None,
            })?;
            record(
                &mut log,
                serde_json::json!({"event":"full-backup-pass","fs":case.fs,"sha256":full_hash,"fileBytes":fs::metadata(&full).map_err(err)?.len()}),
            )?;
            write_fixture(&s, offset, case.size, &name, &second, false)?;
            phase(&mut log, case.fs, "incremental-backup")?;
            let (_, incremental_hash) = run(Work::ArchiveBackup {
                source: s.clone(),
                path: inc.clone(),
                options: data_tasks::BackupOptions {
                    incremental: true,
                    ..options
                },
                parent: Some(chain(&full, None, &bs.target)?),
            })?;
            record(
                &mut log,
                serde_json::json!({"event":"incremental-backup-pass","fs":case.fs,"sha256":incremental_hash,"fileBytes":fs::metadata(&inc).map_err(err)?.len()}),
            )?;
            phase(&mut log, case.fs, "free-wipe")?;
            let preview = maintenance_tasks::prepare(maintenance_tasks::Request {
                source: s.clone(),
                range: surface_scan::Options {
                    start_sector: 0,
                    sector_count: case.size / 512,
                },
                operation: maintenance_tasks::Operation::WipeFree {
                    options: maintenance_io::WipeOptions {
                        passes: case.passes,
                        pattern: if case.random {
                            maintenance_io::Pattern::Random
                        } else {
                            maintenance_io::Pattern::Bytes { hex: "00".into() }
                        },
                    },
                },
            })?;
            inventory()?;
            let report = maintenance_tasks::execute(preview.token)?;
            record(
                &mut log,
                serde_json::json!({"event":"free-wipe-result","fs":case.fs,"report":report}),
            )?;
            if report.state != "completed"
                || !report.disk_left_offline
                || report
                    .free_space
                    .as_ref()
                    .is_none_or(|f| f.filesystem != case.fs || f.preserved_sha256.is_none())
            {
                return Err(format!("{} free wipe failed: {}", case.fs, report.message));
            }
            let file_hash = check_file(
                &selection(A, Some(number))?,
                offset,
                case.size,
                &name,
                &second,
            )?;
            phase(&mut log, case.fs, "incremental-restore")?;
            // Explicit next test preparation, only after all prior checks pass.
            set_online(A)?;
            let target = selection(A, Some(number))?;
            resolved(&target, offset, case.size, true)?;
            let (_, restored_hash) = run(Work::ArchiveRestore {
                destination: target,
                chain: chain(&inc, Some(&full), &bs.target)?,
            })?;
            if restored_hash != incremental_hash || restored_hash.is_none() {
                return Err("Restored normalized digest differs from incremental archive".into());
            }
            if check_file(
                &selection(A, Some(number))?,
                offset,
                case.size,
                &name,
                &second,
            )? != file_hash
            {
                return Err("Restored sentinel full digest differs".into());
            }
            if format_checks::layouts(&[])? != baseline || tables()? != before {
                return Err("Partition layout or raw GPT changed".into());
            }
            data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
            record(
                &mut log,
                serde_json::json!({"event":"case-pass","fs":case.fs,"sentinel":name,"sentinelBytes":second.len(),"sentinelSha256":file_hash,"restoreSha256":restored_hash,"fullBackupBytes":fs::metadata(&full).map_err(err)?.len(),"incrementalBytes":fs::metadata(&inc).map_err(err)?.len(),"passes":case.passes,"random":case.random,"gptUnchanged":true,"allLayoutsUnchanged":true}),
            )?;
            successes.push(case.fs);
        }
        record(
            &mut log,
            serde_json::json!({"event":"all-pass","filesystems":successes,"desktop":DESKTOP,"source":"unchanged production Rust modules; separate integration driver","a":"offline","b":"online; new backup files only"}),
        )?;
        Ok(format!("ALL ALLOCATION MATRIX CASES PASSED\n\n{}\n\nEach: compressed used-space full + changed-file incremental, free wipe/readback, exact 65,553-byte sentinel comparison, incremental chain restore/readback, unchanged raw GPT and all provider layouts.\n\nA stays offline; B online with new backups. This is production-source integration evidence, separate from desktop UI evidence.\nSaved log: {}",successes.join(", "),log_path()?.display()))
    })();
    if let Err(error) = &result {
        let _ = record(
            &mut log,
            serde_json::json!({"event":"failed","error":error,"automaticRetry":false,"rollback":false}),
        );
    }
    result
}

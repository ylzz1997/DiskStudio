//! Explicit guest-only batch acceptance. Original A/B partitions are retained;
//! the replacement phase deletes only the four partitions created by this run.
use super::*;
use allocation_batch::{record, root};
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use partitions::{
    format_options::FormatOptions,
    quick::{self, Entry, Format, Role},
    FileSystem,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const MIB: u64 = 1024 * 1024;
const ONCE: &str = r"C:\ProgramData\DiskStudio-quick-partition-20261117.once";
const BASELINE: &str = r"C:\ProgramData\DiskStudio-quick-partition-20261117.json";
const NATIVE: &str = r"C:\ProgramData\DiskStudio-quick-partition-native-20261117.json";

#[derive(Serialize, Deserialize)]
struct Baseline {
    parts: Vec<partitions::policy::Partition>,
    a: table_archive::Archive,
    b: table_archive::Archive,
}
#[derive(Serialize, Deserialize)]
struct Native {
    baseline: Baseline,
    entries: Vec<Entry>,
}
pub(super) fn parts() -> Result<Vec<partitions::policy::Partition>, String> {
    inventory()?;
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let mut p: Vec<partitions::policy::Partition> = con.raw_query(partitions::policy::Partition::WMI_SELECT).map_err(err)?;
    p.sort_by_key(|p| (p.disk_number, p.partition_number));
    Ok(p)
}
pub(super) fn table(serial: &str) -> Result<table_archive::Archive, String> {
    inventory()?;
    let s = selection(serial, None)?;
    let ctx = partitions::platform::table_inspect_context(&s)?;
    let mut f = data_windows::device(&ctx.resolved, false)?;
    let sector = test_sector();
    let archive = table_archive::capture(&mut f, 16 * GIB, sector, s.target.0, false, || Ok(()))?;
    if !table_restore::archive_layout(&archive)?
        .agrees(&extfs::restore_native(&f, Some(&ctx.parts.iter().collect::<Vec<_>>()), sector)?.0)
    {
        return Err("Raw/native GPT disagrees".into());
    }
    Ok(archive)
}
fn baseline() -> Result<Baseline, String> {
    Ok(Baseline {
        parts: parts()?,
        a: table(A)?,
        b: table(B)?,
    })
}
fn retain(base: &Baseline, additions: &[partitions::policy::Partition]) -> Result<(), String> {
    let new = parts()?;
    let keep = new
        .into_iter()
        .filter(|p| {
            !additions
                .iter()
                .any(|a| a.disk_number == p.disk_number && a.guid == p.guid)
        })
        .collect::<Vec<_>>();
    if keep != base.parts || table(A)?.encode()? != base.a.encode()? {
        return Err("Original partition metadata or A GPT changed".into());
    }
    let b = table(B)?;
    let before = table_restore::archive_layout(&base.b)?;
    let after = table_restore::archive_layout(&b)?;
    if before.guid != after.guid
        || before.first != after.first
        || before.last != after.last
        || before.count != after.count
        || after.entries.len() != before.entries.len() + additions.len()
        || before.entries.iter().any(|e| !after.entries.contains(e))
    {
        return Err("Retained B GPT fields/entries changed".into());
    }
    retained_file()?;
    Ok(())
}
fn retained_file() -> Result<(), String> {
    let s = selection(B, Some(15))?;
    // `stable` requests complete volume associations; `writing` stays false.
    // No lock or writable device is opened by this metadata resolution.
    let r = data_windows::resolve(&s, true, false)?;
    if r.offset != 13_744_734_208 || r.size != 128 * MIB || !r.ntfs_volume {
        return Err("B/15 sentinel scope differs".into());
    }
    let dir = Dir::open_ambient_dir(root(&r)?, cap_std::ambient_authority()).map_err(err)?;
    recovery_output_checks::bind(&dir, &s)?;
    let mut f = dir
        .open("DiskStudio-letter-outcome-20261117.bin")
        .map_err(err)?
        .into_std();
    let binding = data_windows::FileBinding::new(&f, &s.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    if f.metadata().map_err(err)?.len() != 65536 {
        return Err("B/15 sentinel size differs".into());
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(err)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    if format!("{:x}", Sha256::digest(bytes))
        != "ed824a9bd7f414d97ec7ee436178fcfac234dc332018260e099bcb979fc4f53e"
    {
        return Err("B/15 sentinel digest differs".into());
    }
    Ok(())
}
fn target() -> Result<partitions::TargetInfo, String> {
    let ds = inventory()?;
    let b = ds
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B absent")?;
    if b.is_offline != Some(false) || b.is_read_only != Some(false) {
        return Err(
            "B must already be online and writable; driver never changes that state".into(),
        );
    }
    partitions::status()
        .targets
        .into_iter()
        .find(|t| t.serial == B && t.available)
        .ok_or("B unavailable".into())
}
fn entries_at(start: u64) -> Vec<Entry> {
    [
        (
            0,
            128,
            Role::Windows,
            Some(FileSystem::Ntfs),
            "QBNTFS",
            4096,
        ),
        (
            144,
            128,
            Role::Windows,
            Some(FileSystem::Fat32),
            "QBFAT32",
            512,
        ),
        (
            288,
            128,
            Role::Windows,
            Some(FileSystem::Exfat),
            "QBEXFAT",
            4096,
        ),
        (432, 64, Role::Linux, None, "", 0),
    ]
    .into_iter()
    .map(|(offset, size, role, fs, label, cluster)| Entry {
        offset: start + offset * MIB,
        size: size * MIB,
        role,
        format: fs.map(|fs| Format {
            file_system: fs,
            label: label.into(),
            cluster_size: cluster,
            options: FormatOptions::default(),
        }),
    })
    .collect()
}
fn dispatch(
    delete: Vec<u32>,
    entries: Vec<Entry>,
    log: &mut fs::File,
    phase: &str,
) -> Result<Vec<partitions::policy::Partition>, String> {
    let t = target()?;
    let request = quick::Request {
        target: t.target,
        revision: t.revision,
        alignment: 16 * MIB as u32,
        delete_partitions: delete,
        entries: entries.clone(),
    };
    let plan = quick::prepare(request)?;
    record(
        log,
        serde_json::json!({"event":"batch-preview","phase":phase,"plan":plan}),
    )?;
    let result = quick::execute(plan.plan.token)?;
    record(
        log,
        serde_json::json!({"event":"batch-result","phase":phase,"result":result}),
    )?;
    if !result.verified {
        return Err(result.message);
    }
    find_entries(&entries)
}
fn find_entries(entries: &[Entry]) -> Result<Vec<partitions::policy::Partition>, String> {
    let t = target()?;
    let ds = inventory()?;
    let d = ds
        .iter()
        .find(|d| identity(d).as_ref() == Some(&t.target))
        .ok_or("B identity differs")?;
    let list = parts()?;
    let snapshot = storage::read_snapshot()?;
    let view = snapshot
        .disks
        .iter()
        .find(|s| s.hardware.identity.as_deref() == Some(&t.target.0))
        .ok_or("B view absent")?;
    entries
        .iter()
        .map(|e| {
            let found = list
                .iter()
                .filter(|p| {
                    p.disk_number == d.number
                        && p.offset == e.offset
                        && p.size == e.size
                        && p.gpt_type
                            .as_deref()
                            .map(partitions::policy::guid)
                            .as_deref()
                            == Some(e.role.gpt_type())
                })
                .collect::<Vec<_>>();
            if found.len() != 1 {
                return Err("New entry identity/extent/type mismatch".into());
            }
            let p = found[0];
            if let Some(f) = &e.format {
                let v = view
                    .partitions
                    .iter()
                    .find(|v| v.number == p.partition_number)
                    .ok_or("Formatted view absent")?;
                if v.base.name != f.label
                    || !v.base.fs.eq_ignore_ascii_case(f.file_system.windows_name())
                {
                    return Err("Formatted filesystem/label differs".into());
                }
                let s = selection(B, Some(p.partition_number))?;
                let r = data_windows::resolve(&s, false, false)?;
                if r.offset != e.offset || r.size != e.size {
                    return Err("Native opened extent differs".into());
                }
                // Independent raw boot recognition. This never opens a writable device.
                let mut reader = data_windows::reader(&r)?;
                let mut boot = [0u8; 512];
                reader.read_exact(&mut boot).map_err(err)?;
                let expected = match f.file_system {
                    FileSystem::Ntfs => &boot[3..11] == b"NTFS    ",
                    FileSystem::Exfat => &boot[3..11] == b"EXFAT   ",
                    FileSystem::Fat32 => &boot[82..90] == b"FAT32   ",
                    FileSystem::Fat16 => &boot[54..62] == b"FAT16   ",
                    FileSystem::Fat12 => &boot[54..62] == b"FAT12   ",
                };
                if !expected || boot[510..512] != [0x55, 0xaa] {
                    return Err("Independent boot signature mismatch".into());
                }
            }
            Ok(p.clone())
        })
        .collect()
}
fn roundtrip(part: &partitions::policy::Partition) -> Result<String, String> {
    let s = selection(B, Some(part.partition_number))?;
    let r = data_windows::resolve(&s, true, false)?;
    if r.offset != part.offset || r.size != part.size {
        return Err("New fixture extent mismatch".into());
    }
    let dir = Dir::open_ambient_dir(root(&r)?, cap_std::ambient_authority()).map_err(err)?;
    recovery_output_checks::bind(&dir, &s)?;
    let expected = (0..65553)
        .map(|i| ((i * 43 + i / 127) % 251) as u8)
        .collect::<Vec<_>>();
    let mut opts = CapOptions::new();
    opts.write(true).read(true).create_new(true);
    let mut file = dir.open_with("DSQPART.BIN", &opts).map_err(err)?.into_std();
    let binding = data_windows::FileBinding::new(&file, &s.target)?;
    data_windows::FileBinding::validate_all(std::slice::from_ref(&binding))?;
    file.write_all(&expected).map_err(err)?;
    file.sync_all().map_err(err)?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).map_err(err)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    if actual != expected {
        return Err("Independent file contents differ".into());
    }
    Ok(format!("{:x}", Sha256::digest(actual)))
}
fn save(path: &str, value: &impl Serialize) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    f.write_all(&serde_json::to_vec(value).map_err(err)?)
        .map_err(err)?;
    f.sync_all().map_err(err)
}
pub fn check() -> Result<String, String> {
    run_check(false)
}
pub fn continue_check() -> Result<String, String> {
    run_check(true)
}
const FAILED_HELPER: &str = "ba362a2e3a39deea99d021a5d682fe368f98c3e699953dc6285f0041327d8d93";
fn frozen_events(events: &[serde_json::Value]) -> Result<(), String> {
    if events.len() != 2
        || events[0]["event"] != "start"
        || events[0]["kind"] != "quick-partition"
        || events[0]["helper"] != FAILED_HELPER
        || events[0]["desktop"]
            != "266af3c12b541f66b5a754589ac95f118a3f306ebda169a3ca7dd31cb588cfe4"
        || events[0]["recordedAtUnixMs"] != 1794850076012u64
        || events[1]["event"] != "failed"
        || events[1]["error"] != "Fixture must have one writable online volume"
        || events[1]["recordedAtUnixMs"] != 1794850089189u64
    {
        return Err(
            "Previous run was not the exact pre-dispatch fixture failure; refuse continuation"
                .into(),
        );
    }
    Ok(())
}
fn authorize_unstarted() -> Result<(), String> {
    inventory()?;
    let path = format!(r"C:\ProgramData\DiskStudio-quick-partition-{FAILED_HELPER}.jsonl");
    if fs::metadata(&path).map_err(err)?.len() > 65536 {
        return Err("Frozen failure log too large".into());
    }
    let text = fs::read_to_string(path).map_err(err)?;
    let events = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    frozen_events(&events)?;
    if !fs::metadata(ONCE).map_err(err)?.is_file()
        || Path::new(BASELINE).try_exists().map_err(err)?
        || Path::new(NATIVE).try_exists().map_err(err)?
    {
        return Err("Previous run advanced past the known stop or missing once marker".into());
    }
    save(
        r"C:\ProgramData\DiskStudio-quick-partition-continue-20261117.once",
        &"Verified exact original pre-dispatch stop; no partition mutation is repeated",
    )
}
fn run_check(continued: bool) -> Result<String, String> {
    let mut log = backup_protection::begin(if continued {
        "quick-partition-continue"
    } else {
        "quick-partition"
    })?;
    let result = (|| {
        if continued {
            authorize_unstarted()?;
        } else {
            save(
                ONCE,
                &"No retries; deletion only of this run's new B partitions",
            )?;
        }
        let t = target()?;
        let base = baseline()?;
        retained_file()?;
        let b_number = inventory()?
            .into_iter()
            .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
            .ok_or("B absent")?
            .number;
        let frozen = base
            .parts
            .iter()
            .filter(|p| p.disk_number == b_number && p.partition_number == 7)
            .collect::<Vec<_>>();
        if frozen.len() != 1
            || frozen[0].size != 2 * MIB
            || frozen[0]
                .drive_letter
                .as_deref()
                .is_some_and(|s| !disk_identity::clean(s).is_empty())
        {
            return Err("Frozen B/7 state differs".into());
        }
        save(BASELINE, &base)?;
        let free = t
            .free_ranges
            .iter()
            .filter_map(|r| {
                let start = r.offset.div_ceil(16 * MIB) * (16 * MIB);
                (start.checked_add(768 * MIB)? <= r.offset + r.size).then_some(start)
            })
            .max()
            .ok_or("Need at least 768 MiB of aligned free B space")?;
        let first_entries = entries_at(free);
        let first = dispatch(vec![], first_entries.clone(), &mut log, "create-mixed")?;
        for (p, e) in first.iter().zip(&first_entries) {
            if e.format.is_some() {
                record(
                    &mut log,
                    serde_json::json!({"event":"file-passed","phase":"create-mixed","partition":p.partition_number,"sha256":roundtrip(p)?}),
                )?;
            }
        }
        retain(&base, &first)?;
        let second = vec![
            Entry {
                offset: free,
                size: 256 * MIB,
                role: Role::Windows,
                format: Some(Format {
                    file_system: FileSystem::Ntfs,
                    label: "QBREPLACE".into(),
                    cluster_size: 4096,
                    options: FormatOptions {
                        ntfs_compression: true,
                        ..Default::default()
                    },
                }),
            },
            Entry {
                offset: free + 288 * MIB,
                size: 128 * MIB,
                role: Role::Windows,
                format: Some(Format {
                    file_system: FileSystem::Fat16,
                    label: "QBFAT16".into(),
                    cluster_size: 2048,
                    options: FormatOptions::default(),
                }),
            },
        ];
        let replaced = dispatch(
            first.iter().map(|p| p.partition_number).collect(),
            second,
            &mut log,
            "replace-owned",
        )?;
        for p in &replaced {
            record(
                &mut log,
                serde_json::json!({"event":"file-passed","phase":"replace-owned","partition":p.partition_number,"sha256":roundtrip(p)?}),
            )?;
        }
        retain(&base, &replaced)?;
        if parts()?
            .iter()
            .any(|p| first.iter().any(|old| old.guid == p.guid))
        {
            return Err("Replaced GUID still present".into());
        }
        // Native UI is a separate new batch in the largest remaining free range.
        let t = target()?;
        let r = t
            .free_ranges
            .into_iter()
            .max_by_key(|r| r.size)
            .ok_or("No native UI free range")?;
        let size = (r.size / 2 / MIB) * MIB;
        if size < 64 * MIB {
            return Err("Native UI needs two >=64 MiB partitions".into());
        }
        let entries = (0..2)
            .map(|i| Entry {
                offset: r.offset + i * size,
                size: if i == 1 { r.size - size } else { size },
                role: Role::Windows,
                format: Some(Format {
                    file_system: FileSystem::Ntfs,
                    label: format!("DATA{}", i + 1),
                    cluster_size: 0,
                    options: FormatOptions::default(),
                }),
            })
            .collect::<Vec<_>>();
        save(
            NATIVE,
            &Native {
                baseline: baseline()?,
                entries: entries.clone(),
            },
        )?;
        record(
            &mut log,
            serde_json::json!({"event":"native-plan","entries":entries}),
        )?;
        Ok("PASS: mixed NTFS/FAT32/exFAT/Linux creation, then explicit replacement of only this run's four new entries with NTFS/FAT16; five 65553-byte file roundtrips; original A/B GPT entries, all original partition metadata and B/15 sentinel preserved. Native UI separate. ESP/MSR are synthetic-only in this run.".into())
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}
pub fn inspect_ui() -> Result<String, String> {
    let mut log = backup_protection::begin("quick-partition-ui-inspect")?;
    let result = (|| {
        if fs::metadata(NATIVE).map_err(err)?.len() > 512 * 1024 {
            return Err("Native baseline too large".into());
        }
        let base: Native = serde_json::from_slice(&fs::read(NATIVE).map_err(err)?).map_err(err)?;
        let added = find_entries(&base.entries)?;
        retain(&base.baseline, &added)?;
        Ok("PASS: native two-entry quick partition independently verified: exact offsets/sizes/types, NTFS labels and raw boot signatures; A GPT and all retained B GPT entries/partition metadata, B/15 sentinel unchanged. Read-only postcheck; no mutation or file seeding.".into())
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

const PROGRESS_PLAN: &str = r"C:\ProgramData\DiskStudio-quick-progress-native-20261117.json";
const PROGRESS_INSPECT_ONCE: &str =
    r"C:\ProgramData\DiskStudio-quick-progress-inspect-20261117.once";
const PROGRESS_FAILED_HELPER: &str =
    "c8dccc4c1b15932f1876e2b6195f84ed55c90387485038695ef6b089c0a3c707";
const PROGRESS_GPT_ERROR: &str = "Retained B GPT fields/entries changed or removed GUID remains";
#[derive(Serialize, Deserialize)]
struct ProgressNative {
    desktop: String,
    baseline: Baseline,
    removed: partitions::policy::Partition,
    entries: Vec<Entry>,
}
fn progress_entries() -> Vec<Entry> {
    [(14768, 403, "DATA1"), (15171, 404, "DATA2")]
        .into_iter()
        .map(|(offset, size, label)| Entry {
            offset: offset * MIB,
            size: size * MIB,
            role: Role::Windows,
            format: Some(Format {
                file_system: FileSystem::Ntfs,
                label: label.into(),
                cluster_size: 0,
                options: FormatOptions::default(),
            }),
        })
        .collect()
}
fn previous_native_fixture(entries: &[Entry]) -> Result<(), String> {
    if entries.len() != 2
        || entries
            .iter()
            .zip([(14768, 807, "DATA1"), (15575, 808, "DATA2")])
            .any(|(entry, (offset, size, label))| {
                entry.offset != offset * MIB
                    || entry.size != size * MIB
                    || entry.role != Role::Windows
                    || !entry
                        .format
                        .as_ref()
                        .is_some_and(|f| f.file_system == FileSystem::Ntfs && f.label == label)
            })
    {
        return Err("Previous accepted native fixture differs; refuse replacement".into());
    }
    Ok(())
}
// Read-only disk work. The desktop performs the separately observed, single
// confirmation batch. Only DATA1 created by the preceding accepted UI run may
// be replaced; no old helper or previously dispatched mutation is rerun.
pub fn prepare_progress_ui() -> Result<String, String> {
    let mut log = backup_protection::begin("quick-progress-prepare")?;
    let result = (|| {
        save(
            r"C:\ProgramData\DiskStudio-quick-progress-prepare-20261117.once",
            &"One read-only baseline/preview; native UI execution is separate",
        )?;
        if fs::metadata(NATIVE).map_err(err)?.len() > 512 * 1024 {
            return Err("Previous baseline too large".into());
        }
        let previous: Native =
            serde_json::from_slice(&fs::read(NATIVE).map_err(err)?).map_err(err)?;
        previous_native_fixture(&previous.entries)?;
        let old = find_entries(&previous.entries)?;
        retain(&previous.baseline, &old)?;
        let t = target()?;
        let base = baseline()?;
        let removed = old[0].clone();
        if !t.delete_partitions.contains(&removed.partition_number) {
            return Err("Owned DATA1 is not deletable".into());
        }
        let entries = progress_entries();
        let started = std::time::Instant::now();
        let plan = quick::prepare(quick::Request {
            target: t.target,
            revision: t.revision,
            alignment: MIB as u32,
            delete_partitions: vec![removed.partition_number],
            entries: entries.clone(),
        })?;
        quick::discard(plan.plan.token.clone());
        if plan.steps.len() != 5 || plan.retained_partitions.len() != 18 {
            return Err("Expected five-step/18-retained plan differs".into());
        }
        let millis = started.elapsed().as_millis();
        save(
            PROGRESS_PLAN,
            &ProgressNative {
                desktop: option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
                    .ok_or("Missing desktop binding")?
                    .into(),
                baseline: base,
                removed: removed.clone(),
                entries: entries.clone(),
            },
        )?;
        record(
            &mut log,
            serde_json::json!({"event":"native-plan","delete":removed.partition_number,"deleteGuid":removed.guid,"entries":entries,"steps":5,"retained":18,"previewMilliseconds":millis,"previewDiscarded":true}),
        )?;
        Ok(format!("PASS read-only preparation: replace only owned B partition {} DATA1 (14768 MiB/807 MiB), retaining all other 18 entries. Native UI: select that one deletion, 1 MiB alignment, its 807 MiB free range, generate two default NTFS entries (403/404 MiB). Five steps. Preview discarded; no disk mutation. Snapshot preview took {millis} ms.", removed.partition_number))
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}
fn retained_replacement_metadata(
    before: &[partitions::policy::Partition],
    after: &[partitions::policy::Partition],
    removed: &partitions::policy::Partition,
    added: &[partitions::policy::Partition],
) -> bool {
    let kept_before = before.iter().filter(|p| *p != removed).collect::<Vec<_>>();
    let kept_after = after
        .iter()
        .filter(|p| !added.contains(p))
        .collect::<Vec<_>>();
    before.iter().filter(|p| *p == removed).count() == 1
        && !after
            .iter()
            .any(|p| p.disk_number == removed.disk_number && p.guid == removed.guid)
        && added.iter().all(|p| {
            after.iter().filter(|q| *q == p).count() == 1 && p.disk_number == removed.disk_number
        })
        && kept_before == kept_after
}
pub fn inspect_progress_ui() -> Result<String, String> {
    inspect_progress(false)
}

pub fn recheck_progress_ui() -> Result<String, String> {
    inspect_progress(true)
}

fn inspect_progress(recheck: bool) -> Result<String, String> {
    let mut log = backup_protection::begin(if recheck {
        "quick-progress-recheck"
    } else {
        "quick-progress-inspect"
    })?;
    let result = (|| {
        if recheck {
            verify_original_progress_failure()?;
        }
        save(
            if recheck {
                r"C:\ProgramData\DiskStudio-quick-progress-recheck-20261117.once"
            } else {
                PROGRESS_INSPECT_ONCE
            },
            &"Read-only independent postcheck; no seeding or retries",
        )?;
        if fs::metadata(PROGRESS_PLAN).map_err(err)?.len() > 512 * 1024 {
            return Err("Progress baseline too large".into());
        }
        let plan: ProgressNative =
            serde_json::from_slice(&fs::read(PROGRESS_PLAN).map_err(err)?).map_err(err)?;
        if Some(plan.desktop.as_str()) != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256") {
            return Err("Progress baseline belongs to another desktop".into());
        }
        let added = find_entries(&plan.entries)?;
        if !retained_replacement_metadata(&plan.baseline.parts, &parts()?, &plan.removed, &added)
            || table(A)?.encode()? != plan.baseline.a.encode()?
        {
            return Err("Retained metadata or A GPT changed".into());
        }
        let before = table_restore::archive_layout(&plan.baseline.b)?;
        let after = table_restore::archive_layout(&table(B)?)?;
        let moved = verify_replacement_gpt(&before, &after, &plan.removed, &added)?;
        record(
            &mut log,
            serde_json::json!({"event":"gpt-verified","retainedSlotMoves":moved}),
        )?;
        retained_file()?;
        Ok(format!("PASS: both new NTFS extents, labels and boot sectors verified. Only owned DATA1 removed. A GPT, all 18 retained B partition records and GPT entry bytes, and B/15 sentinel preserved.\n\nRetained GPT slot moves: {}.\nRead-only postcheck.", if moved.is_empty() { "none".into() } else { moved.iter().map(|(from, to)| format!("{from} -> {to}")).collect::<Vec<_>>().join(", ") }))
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

fn verify_original_progress_failure() -> Result<(), String> {
    let path =
        format!(r"C:\ProgramData\DiskStudio-quick-progress-inspect-{PROGRESS_FAILED_HELPER}.jsonl");
    if !fs::metadata(PROGRESS_INSPECT_ONCE).map_err(err)?.is_file()
        || fs::metadata(&path).map_err(err)?.len() > 65536
    {
        return Err("Original postcheck evidence missing or oversized".into());
    }
    let events = fs::read_to_string(path)
        .map_err(err)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let desktop =
        option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256").ok_or("Missing desktop binding")?;
    if events.len() != 2
        || events[0]["event"] != "start"
        || events[0]["kind"] != "quick-progress-inspect"
        || events[0]["desktop"] != desktop
        || events[0]["helper"] != PROGRESS_FAILED_HELPER
        || events[1]["event"] != "failed"
        || events[1]["error"] != PROGRESS_GPT_ERROR
    {
        return Err("Original postcheck evidence differs".into());
    }
    Ok(())
}

fn verify_replacement_gpt(
    before: &table_restore::Layout,
    after: &table_restore::Layout,
    removed: &partitions::policy::Partition,
    added: &[partitions::policy::Partition],
) -> Result<Vec<(u32, u32)>, String> {
    if before.guid != after.guid
        || before.first != after.first
        || before.last != after.last
        || before.count != after.count
    {
        return Err(format!("B GPT header differs: disk GUID {} -> {}, usable LBAs {}..{} -> {}..{}, entry capacity {} -> {}", before.guid, after.guid, before.first, before.last, after.first, after.last, before.count, after.count));
    }
    if after.entries.len() + 1 != before.entries.len() + added.len() {
        return Err(format!(
            "B GPT entry count differs: {} before, {} after, {} added",
            before.entries.len(),
            after.entries.len(),
            added.len()
        ));
    }
    let unique = |entries: &[table_restore::Entry]| {
        entries
            .iter()
            .all(|entry| entries.iter().filter(|e| e.guid == entry.guid).count() == 1)
    };
    if !unique(&before.entries) || !unique(&after.entries) {
        return Err("Duplicate B GPT partition GUID".into());
    }
    let removed_guid =
        partitions::policy::guid(removed.guid.as_deref().ok_or("Removed GUID absent")?);
    if !before
        .entries
        .iter()
        .any(|e| e.guid == removed_guid && e.offset == removed.offset && e.size == removed.size)
        || after.entries.iter().any(|e| e.guid == removed_guid)
    {
        return Err("Removed GPT entry does not match baseline or is still present".into());
    }
    let mut new_guids = std::collections::BTreeSet::new();
    for p in added {
        let guid = partitions::policy::guid(p.guid.as_deref().ok_or("New partition GUID absent")?);
        let kind =
            partitions::policy::guid(p.gpt_type.as_deref().ok_or("New partition type absent")?);
        if before.entries.iter().any(|e| e.guid == guid)
            || !after.entries.iter().any(|e| {
                e.guid == guid && e.kind == kind && e.offset == p.offset && e.size == p.size
            })
            || !new_guids.insert(guid)
        {
            return Err("New GPT entry does not match a unique verified partition".into());
        }
    }
    let mut moved = Vec::new();
    for old in before.entries.iter().filter(|e| e.guid != removed_guid) {
        let new = after
            .entries
            .iter()
            .find(|e| e.guid == old.guid)
            .ok_or_else(|| format!("Retained GPT entry missing from old slot {}", old.slot))?;
        // Windows can reorder the GPT array during partition creation. Each
        // 128-byte record is compared by its unique GUID, and the slot change is
        // recorded separately. The provider partition records are checked above.
        if old.kind != new.kind
            || old.offset != new.offset
            || old.size != new.size
            || old.attributes != new.attributes
            || old.name != new.name
        {
            return Err(format!("Retained GPT entry changed (slot {} -> {}): type {} -> {}, offset {} -> {}, size {} -> {}, attributes {:#x} -> {:#x}, name changed: {}", old.slot, new.slot, old.kind, new.kind, old.offset, new.offset, old.size, new.size, old.attributes, new.attributes, old.name != new.name));
        }
        if old.slot != new.slot {
            moved.push((old.slot, new.slot));
        }
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_allows_gpt_slot_moves_but_rejects_record_changes() {
        use partitions::policy::{Partition, BASIC_DATA};
        let entry = |slot, guid: &str, offset, size| table_restore::Entry {
            slot,
            guid: guid.into(),
            kind: BASIC_DATA.into(),
            offset,
            size,
            attributes: 0,
            name: vec![65, 0],
        };
        let parts = |entry: &table_restore::Entry| Partition {
            guid: Some(format!("{{{}}}", entry.guid.to_uppercase())),
            gpt_type: Some(entry.kind.clone()),
            mbr_type: None,
            offset: entry.offset,
            size: entry.size,
            ..Default::default()
        };
        let removed = entry(18, "old", 1024, 1024);
        let kept = entry(19, "kept", 2048, 512);
        let before = table_restore::Layout {
            guid: "disk".into(),
            first: 1,
            last: 100,
            count: 128,
            entries: vec![removed.clone(), kept.clone()],
        };
        let first = entry(18, "first", 1024, 512);
        let second = entry(19, "second", 1536, 512);
        let after = table_restore::Layout {
            entries: vec![
                first.clone(),
                second.clone(),
                table_restore::Entry { slot: 20, ..kept },
            ],
            ..before.clone()
        };
        let removed = parts(&removed);
        let added = [parts(&first), parts(&second)];
        assert_eq!(
            verify_replacement_gpt(&before, &after, &removed, &added).unwrap(),
            vec![(19, 20)]
        );
        let changes: [fn(&mut table_restore::Entry); 6] = [
            |e| e.guid = "unexpected".into(),
            |e| e.kind = "unexpected".into(),
            |e| e.offset += 512,
            |e| e.size += 512,
            |e| e.attributes = 1,
            |e| e.name[0] = 66,
        ];
        for change in changes {
            let mut changed = after.clone();
            change(&mut changed.entries[2]);
            assert!(verify_replacement_gpt(&before, &changed, &removed, &added).is_err());
        }
        let mut wrong_new = after.clone();
        wrong_new.entries[0].guid = second.guid;
        assert!(verify_replacement_gpt(&before, &wrong_new, &removed, &added).is_err());
    }
    #[test]
    fn progress_fixture_is_only_the_two_previously_accepted_native_extents() {
        let mut entries = progress_entries();
        assert!(previous_native_fixture(&entries).is_err());
        entries[0].size = 807 * MIB;
        entries[1].offset = 15575 * MIB;
        entries[1].size = 808 * MIB;
        previous_native_fixture(&entries).unwrap();
        entries[0].format.as_mut().unwrap().label = "other".into();
        assert!(previous_native_fixture(&entries).is_err());
    }
    #[test]
    fn progress_retention_rejects_other_removals_changes_and_reused_guid() {
        use partitions::policy::Partition;
        let retained = Partition {
            guid: Some("retained".into()),
            ..Default::default()
        };
        let removed = Partition {
            partition_number: 18,
            guid: Some("removed".into()),
            ..Default::default()
        };
        let added = Partition {
            partition_number: 18,
            guid: Some("new".into()),
            ..Default::default()
        };
        let before = vec![retained.clone(), removed.clone()];
        let after = vec![retained.clone(), added.clone()];
        assert!(retained_replacement_metadata(
            &before,
            &after,
            &removed,
            std::slice::from_ref(&added)
        ));
        assert!(!retained_replacement_metadata(
            &before,
            &[added.clone()],
            &removed,
            std::slice::from_ref(&added)
        ));
        let mut changed = after.clone();
        changed[0].is_hidden = Some(true);
        assert!(!retained_replacement_metadata(
            &before,
            &changed,
            &removed,
            std::slice::from_ref(&added)
        ));
        assert!(!retained_replacement_metadata(
            &before,
            &before,
            &removed,
            &[removed.clone()]
        ));
    }
    fn events() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"event":"start","kind":"quick-partition","helper":FAILED_HELPER,"desktop":"266af3c12b541f66b5a754589ac95f118a3f306ebda169a3ca7dd31cb588cfe4","recordedAtUnixMs":1794850076012u64}),
            serde_json::json!({"event":"failed","error":"Fixture must have one writable online volume","recordedAtUnixMs":1794850089189u64}),
        ]
    }
    #[test]
    fn accepts_only_the_recorded_failure_before_any_batch_preview() {
        let e = events();
        frozen_events(&e).unwrap();
        let mut bad = e.clone();
        bad.insert(1, serde_json::json!({"event":"batch-preview"}));
        assert!(frozen_events(&bad).is_err());
        let mut bad = e;
        bad[1]["error"] = "another failure".into();
        assert!(frozen_events(&bad).is_err());
    }
    #[test]
    fn rejects_different_builds_and_run_timestamps() {
        let mut bad = events();
        bad[0]["helper"] = "other".into();
        assert!(frozen_events(&bad).is_err());
        let mut bad = events();
        bad[0]["desktop"] = "other".into();
        assert!(frozen_events(&bad).is_err());
        let mut bad = events();
        bad[1]["recordedAtUnixMs"] = 1.into();
        assert!(frozen_events(&bad).is_err());
    }
}

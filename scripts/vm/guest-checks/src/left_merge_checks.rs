//! Two fresh 4Kn left-merge pairs. Only the desktop submits a merge.
use super::*;
use std::{
    os::windows::fs::OpenOptionsExt,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

const PREPARE_KIND: &str = "partition-reallocation-4kn-left-prepare";
const HOLD_KIND: &str = "partition-reallocation-4kn-left-hold";
const BUSY_KIND: &str = "partition-reallocation-4kn-left-busy";
const SUCCESS_KIND: &str = "partition-reallocation-4kn-left-success";
const STOPPED_KIND: &str = "partition-reallocation-4kn-left-clean-stopped";
const LOCK_PROBE_KIND: &str = "partition-reallocation-4kn-left-revised-lock-probe";
const COPY_LOCK_PROBE_KIND: &str = "partition-reallocation-4kn-left-revised-copy-lock-probe";
const INTERRUPTED_KIND: &str = "partition-reallocation-4kn-left-interrupted";
const RESUME_KIND: &str = "partition-reallocation-4kn-left-resume";
const INTERRUPTED_HELPER: &str = "d678dadb5c91264197c1f3444eea4761c647314bcae97b8b31b01eba7335f8f2";
const ONCE: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.once";
const BASELINE: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.json";
const RELEASED: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.released.json";
const BUSY_VERIFIED: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.busy.json";
const INTERRUPTED: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.interrupted.json";
const RESUME_ONCE: &str = r"C:\ProgramData\DiskStudio-left-merge-v1.resume.once";
const ALPHA_DESKTOP: &str = "131b51abf8d9359fadc76adf385c38817986c0f914d5e143fcf15a83cf1ab7ac";
const REVISED_DESKTOP: &str = "71f10fa9428ef11706695f804de4d713d8857c3a748cba135ea757c80d73fcaf";

const FIXTURES: [VolumeSpec; 4] = [
    VolumeSpec {
        label: "DSBUSYFROM",
        offset: 5632,
        size: 128,
        file: "BUSYFROM.BIN",
        seed: 139,
    },
    VolumeSpec {
        label: "DSBUSYKEEP",
        offset: 5760,
        size: 256,
        file: "BUSYKEEP.BIN",
        seed: 157,
    },
    VolumeSpec {
        label: "DSLEFTFROM",
        offset: 6144,
        size: 128,
        file: "LEFTFROM.BIN",
        seed: 173,
    },
    VolumeSpec {
        label: "DSLEFTKEEP",
        offset: 6272,
        size: 256,
        file: "LEFTKEEP.BIN",
        seed: 199,
    },
];

const REVISED_FIXTURES: [VolumeSpec; 4] = [
    VolumeSpec {
        label: "DSRVBUSYFROM",
        offset: 6656,
        size: 128,
        file: "RVBFROM.BIN",
        seed: 211,
    },
    VolumeSpec {
        label: "DSRVBUSYKEEP",
        offset: 6784,
        size: 256,
        file: "RVBKEEP.BIN",
        seed: 223,
    },
    VolumeSpec {
        label: "DSRVLEFTFROM",
        offset: 7168,
        size: 128,
        file: "RVLFROM.BIN",
        seed: 227,
    },
    VolumeSpec {
        label: "DSRVLEFTKEEP",
        offset: 7296,
        size: 256,
        file: "RVLKEEP.BIN",
        seed: 239,
    },
];

const COPY_PROBE_FIXTURES: [VolumeSpec; 2] = [
    VolumeSpec {
        label: "DSLOCKFROM",
        offset: 7680,
        size: 128,
        file: "LOCKFROM.BIN",
        seed: 241,
    },
    VolumeSpec {
        label: "DSLOCKKEEP",
        offset: 7808,
        size: 256,
        file: "LOCKKEEP.BIN",
        seed: 251,
    },
];

const GRACE_FIXTURES: [VolumeSpec; 4] = [
    VolumeSpec {
        label: "DSGBUSYFROM",
        offset: 8192,
        size: 128,
        file: "GBFROM.BIN",
        seed: 197,
    },
    VolumeSpec {
        label: "DSGBUSYKEEP",
        offset: 8320,
        size: 256,
        file: "GBKEEP.BIN",
        seed: 217,
    },
    VolumeSpec {
        label: "DSGLEFTFROM",
        offset: 8704,
        size: 128,
        file: "GLFROM.BIN",
        seed: 221,
    },
    VolumeSpec {
        label: "DSGLEFTKEEP",
        offset: 8832,
        size: 256,
        file: "GLKEEP.BIN",
        seed: 229,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Previous {
    Transfer,
    Alpha,
    Revised,
}

pub(crate) struct Set {
    fixtures: &'static [VolumeSpec; 4],
    previous: Previous,
    prepare_kind: &'static str,
    hold_kind: &'static str,
    busy_kind: &'static str,
    success_kind: &'static str,
    stopped_kind: &'static str,
    once: &'static str,
    baseline: &'static str,
    released: &'static str,
    busy_verified: &'static str,
}

const ORIGINAL: Set = Set {
    fixtures: &FIXTURES,
    previous: Previous::Transfer,
    prepare_kind: PREPARE_KIND,
    hold_kind: HOLD_KIND,
    busy_kind: BUSY_KIND,
    success_kind: SUCCESS_KIND,
    stopped_kind: STOPPED_KIND,
    once: ONCE,
    baseline: BASELINE,
    released: RELEASED,
    busy_verified: BUSY_VERIFIED,
};

const REVISED: Set = Set {
    fixtures: &REVISED_FIXTURES,
    previous: Previous::Alpha,
    prepare_kind: "partition-reallocation-4kn-left-revised-prepare",
    hold_kind: "partition-reallocation-4kn-left-revised-hold",
    busy_kind: "partition-reallocation-4kn-left-revised-busy",
    success_kind: "partition-reallocation-4kn-left-revised-success",
    stopped_kind: "partition-reallocation-4kn-left-revised-clean-stopped",
    once: r"C:\ProgramData\DiskStudio-left-merge-v2.once",
    baseline: r"C:\ProgramData\DiskStudio-left-merge-v2.json",
    released: r"C:\ProgramData\DiskStudio-left-merge-v2.released.json",
    busy_verified: r"C:\ProgramData\DiskStudio-left-merge-v2.busy.json",
};

const GRACE: Set = Set {
    fixtures: &GRACE_FIXTURES,
    previous: Previous::Revised,
    prepare_kind: "partition-reallocation-4kn-left-grace-prepare",
    hold_kind: "partition-reallocation-4kn-left-grace-hold",
    busy_kind: "partition-reallocation-4kn-left-grace-busy",
    success_kind: "partition-reallocation-4kn-left-grace-success",
    stopped_kind: "partition-reallocation-4kn-left-grace-clean-stopped",
    once: r"C:\ProgramData\DiskStudio-left-merge-v3.once",
    baseline: r"C:\ProgramData\DiskStudio-left-merge-v3.json",
    released: r"C:\ProgramData\DiskStudio-left-merge-v3.released.json",
    busy_verified: r"C:\ProgramData\DiskStudio-left-merge-v3.busy.json",
};

#[derive(Clone, Copy)]
pub(crate) enum EntryPoint {
    Prepare(&'static Set),
    Hold(&'static Set),
    VerifyBusy(&'static Set),
    VerifySuccess(&'static Set),
    VerifyCleanStopped(&'static Set),
    ProbeRevisedLock,
    ProbeLockAfterCopy,
    InspectInterrupted,
    Resume,
    Report(&'static str),
}

impl EntryPoint {
    pub(super) fn parse(name: &str) -> Option<Self> {
        let (set, name) = if let Some(name) = name.strip_prefix("Grace-") {
            (&GRACE, name)
        } else if let Some(name) = name.strip_prefix("Revised-") {
            (&REVISED, name)
        } else {
            (&ORIGINAL, name)
        };
        Some(match name {
            "Prepare" => Self::Prepare(set),
            "Hold" => Self::Hold(set),
            "Busy" => Self::VerifyBusy(set),
            "Success" => Self::VerifySuccess(set),
            "Clean-Stopped" => Self::VerifyCleanStopped(set),
            "Lock-Probe" if set.previous == Previous::Alpha => Self::ProbeRevisedLock,
            "Lock-Probe-Report" if set.previous == Previous::Alpha => Self::Report(LOCK_PROBE_KIND),
            "Copy-Lock-Probe" if set.previous == Previous::Alpha => Self::ProbeLockAfterCopy,
            "Copy-Lock-Probe-Report" if set.previous == Previous::Alpha => {
                Self::Report(COPY_LOCK_PROBE_KIND)
            }
            "Prepare-Report" => Self::Report(set.prepare_kind),
            "Hold-Report" => Self::Report(set.hold_kind),
            "Busy-Report" => Self::Report(set.busy_kind),
            "Success-Report" => Self::Report(set.success_kind),
            "Clean-Stopped-Report" => Self::Report(set.stopped_kind),
            "Interrupted" if set.previous == Previous::Transfer => Self::InspectInterrupted,
            "Resume" if set.previous == Previous::Transfer => Self::Resume,
            "Interrupted-Report" if set.previous == Previous::Transfer => {
                Self::Report(INTERRUPTED_KIND)
            }
            "Resume-Report" if set.previous == Previous::Transfer => Self::Report(RESUME_KIND),
            _ => return None,
        })
    }

    pub(super) fn run(self) -> Result<String, String> {
        match self {
            Self::Prepare(set) => prepare(set),
            Self::Hold(set) => hold(set),
            Self::VerifyBusy(set) => inspect(set, Inspection::Busy),
            Self::VerifySuccess(set) => inspect(set, Inspection::Success),
            Self::VerifyCleanStopped(set) => inspect(set, Inspection::CleanStopped),
            Self::ProbeRevisedLock => probe_revised_lock(),
            Self::ProbeLockAfterCopy => probe_lock_after_copy(),
            Self::InspectInterrupted => inspect_interrupted(),
            Self::Resume => resume(),
            Self::Report(kind) => backup_protection::report(kind),
        }
    }
}

fn checked(offline: bool) -> Result<Vec<Disk>, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, offline)?;
    state(&disks, B, false)?;
    Ok(disks)
}

fn prepare(set: &Set) -> Result<String, String> {
    let mut log = backup_protection::begin(set.prepare_kind)?;
    let disks = checked(false)?;
    let a = table(A)?;
    let before = table_restore::archive_layout(&a)?;
    let previous = match set.previous {
        Previous::Transfer => {
            let previous = witness()?;
            let tail = Tail::from(entry_at(&before, TAIL_OFFSET, TAIL_SIZE)?);
            verify_previous(&previous, &before, &tail, &disks)?;
            previous
        }
        Previous::Alpha => {
            if ALPHA_DESKTOP == checked_desktop()? {
                return Err("Fresh revised fixtures require the new production build".into());
            }
            let previous = baseline(&ORIGINAL, ALPHA_DESKTOP)?;
            if a != previous.a
                || table(B)? != previous.b
                || os_layout(&disks)? != previous.os_layout
            {
                return Err(
                    "GPT or guest OS metadata no longer matches the original pair baseline".into(),
                );
            }
            verify_alpha_samples(false)?;
            previous
        }
        Previous::Revised => {
            if REVISED_DESKTOP == checked_desktop()? {
                return Err("The lock-grace fixtures require the new production build".into());
            }
            let previous = baseline(&REVISED, REVISED_DESKTOP)?;
            let mut retained = before.clone();
            for spec in &COPY_PROBE_FIXTURES {
                let guid = &entry_at(&before, spec.offset, spec.size)?.guid;
                retained.entries.retain(|entry| &entry.guid != guid);
            }
            if !retained.agrees(&table_restore::archive_layout(&previous.a)?)
                || table(B)? != previous.b
                || os_layout(&disks)? != previous.os_layout
            {
                return Err(
                    "The current GPT does not match the stopped cases and copy probe".into(),
                );
            }
            verify_revised_and_probe_samples(false)?;
            previous
        }
    };
    require_unused(&before, set.fixtures)?;
    write_new(set.once, b"Create the two new left-merge pairs once.")?;
    let original_guids = create_fixtures(&mut log, set.fixtures)?;
    let a = table(A)?;
    let mut retained = table_restore::archive_layout(&a)?;
    retained
        .entries
        .retain(|entry| !original_guids.contains(&entry.guid));
    if !retained.agrees(&before)
        || table(B)? != previous.b
        || os_layout(&checked(false)?)? != previous.os_layout
    {
        return Err("A historical entry, B GPT, or the guest OS layout changed".into());
    }
    save_baseline(set, &mut log, a, previous, original_guids)?;
    Ok("PASS: Created both fresh left-merge pairs. Start the busy-case holder before the busy merge.".into())
}

fn verify_previous(
    previous: &Witness,
    retained: &Layout,
    tail: &Tail,
    disks: &[Disk],
) -> Result<(), String> {
    let expected = expected_layout(
        table_restore::archive_layout(&previous.a)?,
        &previous.original_guids,
        retained,
        Phase::Transfer,
    )?;
    if !retained.agrees(&expected)
        || tail != &read_evidence::<Tail>(SPLIT_TAIL, 2048)?
        || table(B)? != previous.b
        || os_layout(disks)? != previous.os_layout
    {
        return Err("The previously verified 4Kn layout changed".into());
    }
    Ok(())
}

fn require_unused(layout: &Layout, fixtures: &[VolumeSpec]) -> Result<(), String> {
    let first = fixtures.first().ok_or("Missing fixture scope")?;
    let last = fixtures.last().ok_or("Missing fixture scope")?;
    let target = partitions::status()
        .targets
        .into_iter()
        .find(|target| target.serial == A && target.available)
        .ok_or("A target unavailable")?;
    let end_mib = last.offset + last.size;
    if !target.free_ranges.iter().any(|range| {
        range.offset <= first.offset * MIB
            && range.offset.saturating_add(range.size) >= end_mib * MIB
    }) || layout.entries.len() + fixtures.len() > layout.count as usize
    {
        return Err(format!(
            "A reservation {}–{end_mib} MiB must be completely free",
            first.offset
        ));
    }
    Ok(())
}

fn save_baseline(
    set: &Set,
    log: &mut File,
    a: Archive,
    previous: Witness,
    original_guids: Vec<String>,
) -> Result<(), String> {
    let witness = Witness {
        desktop: checked_desktop()?.into(),
        a,
        b: previous.b,
        original_guids,
        os_layout: previous.os_layout,
    };
    write_new(set.baseline, &serde_json::to_vec(&witness).map_err(err)?)?;
    record(
        log,
        json!({"event":"prepared","fixtureCount":set.fixtures.len()}),
    )
}

fn baseline(set: &Set, desktop: &str) -> Result<Witness, String> {
    let witness: Witness = read_evidence(set.baseline, 8 * 1024 * 1024)?;
    if witness.desktop != desktop
        || witness.original_guids.len() != set.fixtures.len()
        || witness.a.sector != 4096
        || witness.b.sector != 4096
    {
        return Err("Fresh left-merge witness does not match this payload".into());
    }
    Ok(witness)
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InterruptedState {
    a: Archive,
    guids: Vec<String>,
    keep_sample: Option<String>,
}

fn verify_interrupted_log(events: &[serde_json::Value], desktop: &str) -> Result<(), String> {
    let [start, create_first, format_first, first, create_second, format_second] = events else {
        return Err("Unexpected interrupted preparation sequence".into());
    };
    let create_matches = |event: &serde_json::Value, spec: &VolumeSpec| {
        event["event"] == "create-fixture-once"
            && event["filesystem"] == "NTFS"
            && event["offset"] == spec.offset * MIB
            && event["size"] == spec.size * MIB
    };
    let format_matches = |event: &serde_json::Value, partition: u32| {
        event["event"] == "format-fixture-once"
            && event["filesystem"] == "NTFS"
            && event["partition"] == partition
    };
    if start["event"] != "start"
        || start["desktop"] != desktop
        || start["helper"] != INTERRUPTED_HELPER
        || start["kind"] != PREPARE_KIND
        || !create_matches(create_first, &FIXTURES[0])
        || !format_matches(format_first, 20)
        || first["event"] != "fixture"
        || first["label"] != FIXTURES[0].label
        || first["partition"] != 20
        || first["offsetMiB"] != FIXTURES[0].offset
        || first["sizeMiB"] != FIXTURES[0].size
        || !create_matches(create_second, &FIXTURES[1])
        || !format_matches(format_second, 21)
    {
        return Err("Saved log does not describe this interrupted build and exact pair".into());
    }
    Ok(())
}

fn read_interrupted_log() -> Result<(), String> {
    if !Path::new(ONCE).try_exists().map_err(err)?
        || Path::new(BASELINE).try_exists().map_err(err)?
    {
        return Err("Expected one interrupted preparation without a finished witness".into());
    }
    let path = format!(r"C:\ProgramData\DiskStudio-{PREPARE_KIND}-{INTERRUPTED_HELPER}.jsonl");
    let mut text = String::new();
    File::open(path)
        .map_err(err)?
        .take(8193)
        .read_to_string(&mut text)
        .map_err(err)?;
    if text.len() > 8192 {
        return Err("Interrupted preparation log is too large".into());
    }
    let events = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    verify_interrupted_log(&events, checked_desktop()?)
}

fn check_keep_after_restart() -> Result<Option<String>, String> {
    let spec = &FIXTURES[1];
    let part = metadata_partition(A, Some(spec.label), spec.offset, spec.size)?;
    let selected = selection(A, Some(part.number))?;
    let size = spec.size * MIB;
    let resolved = resolved(&selected, spec.offset * MIB, size, false)?;
    let mut reader = data_windows::reader(&resolved)?;
    let ntfs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    if ntfs.sector_size() != 4096
        || ntfs.cluster_size() != 4096
        || ntfs.size().checked_add(4096) != Some(size)
    {
        return Err("The interrupted format did not leave the expected NTFS geometry".into());
    }
    ntfs_allocation::inspect(&mut reader, &AtomicBool::new(false))?;
    let mut found = false;
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| found |= !file.deleted && file.name.eq_ignore_ascii_case(spec.file),
        |_| Ok(()),
    )?;
    found
        .then(|| original_sample(spec, spec.offset, spec.size, false))
        .transpose()
}

fn check_interrupted() -> Result<(InterruptedState, Witness), String> {
    let disks = checked(false)?;
    read_interrupted_log()?;
    let previous = witness()?;
    let a = table(A)?;
    let layout = table_restore::archive_layout(&a)?;
    let tail = Tail::from(entry_at(&layout, TAIL_OFFSET, TAIL_SIZE)?);
    let guids = FIXTURES[..2]
        .iter()
        .map(|spec| entry_at(&layout, spec.offset, spec.size).map(|entry| entry.guid.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut retained = layout.clone();
    retained
        .entries
        .retain(|entry| !guids.contains(&entry.guid));
    verify_previous(&previous, &retained, &tail, &disks)?;
    require_unused(&layout, &FIXTURES[2..])?;
    let first = &FIXTURES[0];
    original_sample(first, first.offset, first.size, false)?;
    let keep_sample = check_keep_after_restart()?;
    if table(A)? != a || table(B)? != previous.b {
        return Err("GPT changed during interrupted preparation readback".into());
    }
    Ok((
        InterruptedState {
            a,
            guids,
            keep_sample,
        },
        previous,
    ))
}

fn inspect_interrupted() -> Result<String, String> {
    let mut log = backup_protection::begin(INTERRUPTED_KIND)?;
    let (state, _) = check_interrupted()?;
    write_new(INTERRUPTED, &serde_json::to_vec(&state).map_err(err)?)?;
    record(
        &mut log,
        json!({"event":"passed","keepNtfsClean":true,"keepSample":state.keep_sample,
        "originalSampleVerified":true,"cleanPairUnused":true,"bGptUnchanged":true}),
    )?;
    Ok("PASS: Found both retained NTFS volumes. The interrupted format has a clean, consistent NTFS filesystem. The clean pair is still entirely unallocated.".into())
}

fn resume() -> Result<String, String> {
    let mut log = backup_protection::begin(RESUME_KIND)?;
    let (state, previous) = check_interrupted()?;
    if state != read_evidence::<InterruptedState>(INTERRUPTED, 8 * 1024 * 1024)? {
        return Err("Interrupted disk or sample changed since the independent readback".into());
    }
    write_new(RESUME_ONCE, b"Only untouched fixture steps may run once.")?;
    if state.keep_sample.is_none() {
        let spec = &FIXTURES[1];
        let part = metadata_partition(A, Some(spec.label), spec.offset, spec.size)?;
        let selected = selection(A, Some(part.number))?;
        write_fixture(
            &selected,
            spec.offset * MIB,
            spec.size * MIB,
            spec.file,
            &sample(spec.seed),
            true,
        )?;
        record(&mut log, json!({"event":"new-sample","label":spec.label}))?;
    }
    let new_guids = create_fixtures(&mut log, &FIXTURES[2..])?;
    let a = table(A)?;
    let mut retained = table_restore::archive_layout(&a)?;
    retained
        .entries
        .retain(|entry| !new_guids.contains(&entry.guid));
    if !retained.agrees(&table_restore::archive_layout(&state.a)?)
        || table(B)? != previous.b
        || os_layout(&checked(false)?)? != previous.os_layout
    {
        return Err("Continuing preparation changed an existing GPT entry or OS layout".into());
    }
    let guids = state.guids.into_iter().chain(new_guids).collect();
    save_baseline(&ORIGINAL, &mut log, a, previous, guids)?;
    Ok("PASS: Finished only the untouched preparation steps. The two original left-merge fixture volumes were kept.".into())
}

fn hold(set: &Set) -> Result<String, String> {
    let mut log = backup_protection::begin(set.hold_kind)?;
    let disks = checked(false)?;
    let base = baseline(set, checked_desktop()?)?;
    if table(A)? != base.a || table(B)? != base.b || os_layout(&disks)? != base.os_layout {
        return Err("Both pairs must be fresh before holding the busy fixture".into());
    }
    let [source, spec, ..] = set.fixtures;
    let part = metadata_partition(A, Some(spec.label), spec.offset, spec.size)?;
    let selection = selection(A, Some(part.number))?;
    let offset = spec.offset * MIB;
    let size = spec.size * MIB;
    let resolved = resolved(&selection, offset, size, true)?;
    let directory = allocation_batch::root(&resolved)?;
    let mut sample_file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(directory.join(spec.file))
        .map_err(err)?;
    data_windows::file_location(&sample_file)?.confirm_partition(
        &selection.target,
        &selection.layout_key,
        &resolved.volumes[0],
        (resolved.disk.number, offset, size),
    )?;
    let expected = sample(spec.seed);
    let mut bytes = Vec::with_capacity(expected.len());
    (&mut sample_file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes != expected {
        return Err("Open handle does not contain the expected sample".into());
    }
    record(
        &mut log,
        json!({"event":"holding-read-handle","label":spec.label}),
    )?;
    notice(&format!("HOLDING the {} read handle on {}. Keep this window open and submit the {} → {} test once in DiskStudio. Close this dialog after recording the result.", spec.file, spec.label, source.label, spec.label));
    drop(sample_file);
    write_new(
        set.released,
        &serde_json::to_vec(&base.original_guids[1]).map_err(err)?,
    )?;
    record(&mut log, json!({"event":"released-read-handle"}))?;
    Ok(format!(
        "Released the {} read handle. Run the independent busy check before the clean merge.",
        spec.label
    ))
}

fn success_layout(set: &Set, mut layout: Layout, witness: &[String]) -> Result<Layout, String> {
    let [_, _, absorbed, kept] = witness else {
        return Err("Expected the four prepared fixture identities".into());
    };
    let [_, _, source, target] = set.fixtures;
    if &entry_at(&layout, source.offset, source.size)?.guid != absorbed
        || &entry_at(&layout, target.offset, target.size)?.guid != kept
    {
        return Err("Left-merge identity does not match the prepared extent".into());
    }
    layout.entries.retain(|entry| &entry.guid != absorbed);
    let keep = layout
        .entries
        .iter_mut()
        .find(|entry| &entry.guid == kept)
        .ok_or("Missing kept GPT identity")?;
    keep.offset = source.offset * MIB;
    keep.size = (source.size + target.size) * MIB;
    Ok(layout)
}

fn original_sample(
    spec: &VolumeSpec,
    offset: u64,
    size: u64,
    offline: bool,
) -> Result<String, String> {
    let part = metadata_partition(A, (!offline).then_some(spec.label), offset, size)?;
    check_file(
        &selection(A, Some(part.number))?,
        offset * MIB,
        size * MIB,
        spec.file,
        &sample(spec.seed),
    )
}

fn verify_alpha_samples(offline: bool) -> Result<(), String> {
    for spec in &FIXTURES {
        original_sample(spec, spec.offset, spec.size, offline)?;
    }
    let busy = merged_sample(FIXTURES[1].offset, FIXTURES[1].size, &FIXTURES[0])?;
    if busy != read_evidence::<(String, String)>(BUSY_VERIFIED, 2048)? {
        return Err("The Alpha busy-case copy changed".into());
    }
    merged_sample(FIXTURES[3].offset, FIXTURES[3].size, &FIXTURES[2])?;
    Ok(())
}

fn verify_revised_and_probe_samples(offline: bool) -> Result<(), String> {
    verify_alpha_samples(offline)?;
    for spec in REVISED_FIXTURES.iter().chain(&COPY_PROBE_FIXTURES) {
        original_sample(spec, spec.offset, spec.size, offline)?;
    }
    let [busy_from, busy_keep, clean_from, clean_keep] = &REVISED_FIXTURES;
    if merged_sample(busy_keep.offset, busy_keep.size, busy_from)?
        != read_evidence::<(String, String)>(REVISED.busy_verified, 2048)?
    {
        return Err("The revised busy-case copy changed".into());
    }
    merged_sample(clean_keep.offset, clean_keep.size, clean_from)?;
    let [probe_from, probe_keep] = &COPY_PROBE_FIXTURES;
    merged_sample(probe_keep.offset, probe_keep.size, probe_from)?;
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Inspection {
    Busy,
    Success,
    CleanStopped,
}

fn inspect(set: &Set, kind: Inspection) -> Result<String, String> {
    let mut log = backup_protection::begin(match kind {
        Inspection::Busy => set.busy_kind,
        Inspection::Success => set.success_kind,
        Inspection::CleanStopped => set.stopped_kind,
    })?;
    let success = kind == Inspection::Success;
    let disks = checked(success)?;
    let base = baseline(set, checked_desktop()?)?;
    if read_evidence::<String>(set.released, 256)? != base.original_guids[1]
        || table(B)? != base.b
        || os_layout(&disks)? != base.os_layout
    {
        return Err("Missing busy-handle evidence or an unrelated layout changed".into());
    }
    let actual = table(A)?;
    let baseline = table_restore::archive_layout(&base.a)?;
    let expected = if success {
        success_layout(set, baseline, &base.original_guids)?
    } else {
        baseline
    };
    if !table_restore::archive_layout(&actual)?.agrees(&expected) {
        return Err("The complete GPT does not match the expected result".into());
    }

    match set.previous {
        Previous::Transfer => {}
        Previous::Alpha => verify_alpha_samples(success)?,
        Previous::Revised => verify_revised_and_probe_samples(success)?,
    }
    let [busy_from, busy_keep, clean_from, clean_keep] = set.fixtures;
    let (busy_folder, busy_hash) = merged_sample(busy_keep.offset, busy_keep.size, busy_from)?;
    if kind != Inspection::Busy
        && read_evidence::<(String, String)>(set.busy_verified, 2048)?
            != (busy_folder.clone(), busy_hash.clone())
    {
        return Err("The independently verified busy-case copy changed".into());
    }
    let mut digests = Vec::new();
    for spec in [busy_from, busy_keep, &VOLUMES[5], &VOLUMES[6]] {
        digests.push((
            spec.label,
            original_sample(spec, spec.offset, spec.size, success)?,
        ));
    }
    if success {
        let size = clean_from.size + clean_keep.size;
        let (folder, hash) = merged_sample(clean_from.offset, size, clean_from)?;
        record(
            &mut log,
            json!({"event":"clean-copy","directory":folder,"sha256":hash}),
        )?;
        digests.push((
            clean_keep.label,
            original_sample(clean_keep, clean_from.offset, size, true)?,
        ));
    } else {
        for spec in [clean_from, clean_keep] {
            digests.push((
                spec.label,
                original_sample(spec, spec.offset, spec.size, false)?,
            ));
        }
        if kind == Inspection::CleanStopped {
            let (folder, hash) = merged_sample(clean_keep.offset, clean_keep.size, clean_from)?;
            record(
                &mut log,
                json!({"event":"clean-copy","directory":folder,"sha256":hash}),
            )?;
        }
    }
    checked(success)?;
    if table(A)? != actual || table(B)? != base.b {
        return Err("GPT changed during the independent readback".into());
    }
    record(
        &mut log,
        json!({"event":"busy-copy","directory":busy_folder,"sha256":busy_hash}),
    )?;
    if kind == Inspection::Busy {
        write_new(
            set.busy_verified,
            &serde_json::to_vec(&(busy_folder, busy_hash)).map_err(err)?,
        )?;
    }
    record(
        &mut log,
        json!({"event":"passed","aOffline":success,
        "bGptUnchanged":true,"guestOsLayoutUnchanged":true,"fileSha256":digests}),
    )?;
    Ok(match kind {
        Inspection::Success => "PASS: Fresh left merge moved and grew the kept GPT entry. Both originals from the busy pair, both staged copies, the kept sample and the historical originals match. A is offline.",
        Inspection::Busy => "PASS: Busy merge preserved the complete GPT, both source partitions and their samples. The staged copy matches byte for byte. A is online.",
        Inspection::CleanStopped => "PASS: Clean case stopped with both GPT entries and originals preserved. Both staged copies match byte for byte. A is online.",
    }.into())
}

fn revised_stopped_baseline() -> Result<Witness, String> {
    let disks = checked(false)?;
    let base = baseline(&REVISED, checked_desktop()?)?;
    if read_evidence::<String>(REVISED.released, 256)? != base.original_guids[1]
        || table(A)? != base.a
        || table(B)? != base.b
        || os_layout(&disks)? != base.os_layout
    {
        return Err("The revised stopped fixture no longer matches its baseline".into());
    }
    Ok(base)
}

fn fixture_binding(spec: &VolumeSpec) -> Result<(Selection, data_windows::Resolved), String> {
    let part = metadata_partition(A, Some(spec.label), spec.offset, spec.size)?;
    let selection = selection(A, Some(part.number))?;
    let resolved = resolved(&selection, spec.offset * MIB, spec.size * MIB, true)?;
    if resolved.volumes.len() != 1 || !resolved.ntfs_volume {
        return Err("Expected exactly one verified NTFS volume in the fixture".into());
    }
    Ok((selection, resolved))
}

fn lock_fixture(
    (selection, resolved): &(Selection, data_windows::Resolved),
) -> Result<data_windows::Locks, String> {
    let locks = data_windows::lock(resolved)?;
    data_windows::resolve_locked(selection, false, resolved, &locks)?;
    Ok(locks)
}

fn probe_revised_lock() -> Result<String, String> {
    let mut log = backup_protection::begin(LOCK_PROBE_KIND)?;
    let base = revised_stopped_baseline()?;
    let [_, busy_keep, clean_from, clean_keep] = &REVISED_FIXTURES;
    let (folder, hash) = merged_sample(clean_keep.offset, clean_keep.size, clean_from)?;
    record(
        &mut log,
        json!({"event":"staged-copy","directory":folder,"sha256":hash}),
    )?;

    let clean_keep = fixture_binding(clean_keep)?;
    let clean_from = fixture_binding(clean_from)?;
    let busy_keep = fixture_binding(busy_keep)?;
    let mut result = Vec::new();
    let mut observe = |label, outcome: Result<(), String>| {
        let summary = outcome.err().unwrap_or_else(|| "verified".into());
        record(
            &mut log,
            json!({"event":"lock-probe","case":label,"result":summary}),
        )?;
        result.push(format!("{label}: {summary}"));
        Ok::<_, String>(())
    };
    observe(
        "Formerly occupied target alone",
        lock_fixture(&busy_keep).map(drop),
    )?;
    observe("Clean target alone", lock_fixture(&clean_keep).map(drop))?;
    let both = (|| {
        let _source_lock = lock_fixture(&clean_from).map_err(|e| format!("Source lock: {e}"))?;
        lock_fixture(&clean_keep).map(drop)
    })();
    observe("Clean target with source locked", both)?;

    let disks = checked(false)?;
    if table(A)? != base.a || table(B)? != base.b || os_layout(&disks)? != base.os_layout {
        return Err("A layout changed while probing locks".into());
    }
    record(&mut log, json!({"event":"layout-verified"}))?;
    Ok(result.join("\n"))
}

fn probe_lock_after_copy() -> Result<String, String> {
    use partitions::merge;

    let mut log = backup_protection::begin(COPY_LOCK_PROBE_KIND)?;
    let base = revised_stopped_baseline()?;
    let original = table_restore::archive_layout(&base.a)?;
    require_unused(&original, &COPY_PROBE_FIXTURES)?;
    write_new(
        r"C:\ProgramData\DiskStudio-left-copy-lock-probe.once",
        b"Create the copy-and-lock probe partitions once.",
    )?;
    let guids = create_fixtures(&mut log, &COPY_PROBE_FIXTURES)?;
    let prepared = table(A)?;
    let mut retained = table_restore::archive_layout(&prepared)?;
    retained
        .entries
        .retain(|entry| !guids.contains(&entry.guid));
    if !retained.agrees(&original)
        || table(B)? != base.b
        || os_layout(&checked(false)?)? != base.os_layout
    {
        return Err("Preparing the probe changed an unrelated layout".into());
    }

    let [from, keep] = &COPY_PROBE_FIXTURES;
    let source = fixture_binding(from)?;
    let target = fixture_binding(keep)?;
    let parts = quick_partition_checks::parts()?;
    let layout = partitions::policy::layout(&target.1.disk, &parts)?;
    target
        .1
        .verify_partition_snapshot(&target.1.disk, &layout)?;
    let target_partition = layout
        .into_iter()
        .find(|part| {
            Some(part.partition_number) == target.0.partition
                && (part.offset, part.size) == (keep.offset * MIB, keep.size * MIB)
        })
        .ok_or("Cannot resolve the fixed copy probe target")?;

    let source_lock = lock_fixture(&source)?;
    let mut reader = data_windows::reader(&source.1)?;
    let cancel = AtomicBool::new(false);
    let catalogue = merge::catalogue(&mut reader, None, &cancel)?;
    let copied = merge::output::copy(
        &target.0,
        &target.1.disk,
        target_partition,
        &target.1.volumes[0],
        uuid::Uuid::new_v4(),
        &catalogue,
        &mut reader,
        &cancel,
    )?;
    drop(reader);
    let copied_at = Instant::now();
    let mut observed = None;
    let mut observations = Vec::new();
    for milliseconds in [0, 250, 2_000, 10_000] {
        std::thread::sleep(Duration::from_millis(milliseconds).saturating_sub(copied_at.elapsed()));
        let elapsed = copied_at.elapsed().as_millis() as u64;
        match data_windows::lock(&target.1) {
            Ok(locks) => {
                observed = Some((elapsed, locks));
                break;
            }
            Err(error) => {
                record(
                    &mut log,
                    json!({"event":"lock-denied","elapsedMs":elapsed,"error":error}),
                )?;
                observations.push(format!("{elapsed} ms: {error}"));
            }
        }
    }
    if let Some((elapsed, locks)) = observed {
        data_windows::resolve_locked(&target.0, false, &target.1, &locks)?;
        let mut target_reader = data_windows::reader(&target.1)?;
        copied.verify(&catalogue, &mut target_reader, &cancel)?;
        record(
            &mut log,
            json!({"event":"lock-and-copy-verified","elapsedMs":elapsed}),
        )?;
        observations.push(format!("{elapsed} ms: locked and verified"));
    }
    drop(source_lock);
    if table(A)? != prepared
        || table(B)? != base.b
        || os_layout(&checked(false)?)? != base.os_layout
    {
        return Err("Copy and lock probes changed a partition layout".into());
    }
    merged_sample(keep.offset, keep.size, from)?;
    record(&mut log, json!({"event":"layout-and-sample-verified"}))?;
    Ok(observations.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_rejects_a_log_from_a_different_dispatch_or_build() {
        let desktop = "desktop-hash";
        let a = &FIXTURES[0];
        let b = &FIXTURES[1];
        let events = [
            json!({"event":"start","desktop":desktop,"helper":INTERRUPTED_HELPER,"kind":PREPARE_KIND}),
            json!({"event":"create-fixture-once","filesystem":"NTFS","offset":a.offset * MIB,"size":a.size * MIB}),
            json!({"event":"format-fixture-once","filesystem":"NTFS","partition":20}),
            json!({"event":"fixture","label":a.label,"partition":20,"offsetMiB":a.offset,"sizeMiB":a.size}),
            json!({"event":"create-fixture-once","filesystem":"NTFS","offset":b.offset * MIB,"size":b.size * MIB}),
            json!({"event":"format-fixture-once","filesystem":"NTFS","partition":21}),
        ];
        assert!(verify_interrupted_log(&events, desktop).is_ok());
        assert!(verify_interrupted_log(&events, "another-desktop").is_err());
        assert!(verify_interrupted_log(&events[..5], desktop).is_err());
        let mut changed = events;
        changed[5]["partition"] = json!(22);
        assert!(verify_interrupted_log(&changed, desktop).is_err());
    }

    #[test]
    fn expected_success_only_removes_the_clean_source_and_changes_the_kept_extent() {
        let entries: Vec<_> = FIXTURES
            .iter()
            .enumerate()
            .map(|(index, spec)| Entry {
                slot: index as u32 + 11,
                guid: uuid::Uuid::from_u128(1 + index as u128).to_string(),
                kind: partitions::policy::BASIC_DATA.into(),
                offset: spec.offset * MIB,
                size: spec.size * MIB,
                attributes: 1 << 63,
                name: spec.label.encode_utf16().collect(),
            })
            .collect();
        let guids: Vec<_> = entries.iter().map(|entry| entry.guid.clone()).collect();
        let base = Layout {
            guid: "test-disk".into(),
            first: 6,
            last: 4_194_299,
            count: 128,
            entries: entries.clone(),
        };
        let actual = success_layout(&ORIGINAL, base.clone(), &guids).unwrap();
        assert_eq!(&actual.entries[..2], &entries[..2]);
        let mut kept = entries[3].clone();
        kept.offset = entries[2].offset;
        kept.size += entries[2].size;
        assert_eq!(&actual.entries[2..], &[kept]);
        let mut stale = guids;
        stale.swap(2, 3);
        assert!(success_layout(&ORIGINAL, base, &stale).is_err());
    }
}

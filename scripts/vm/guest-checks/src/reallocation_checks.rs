//! Guest-only preparation and independent readback of five native UI cases.
//! Inspectors never submit a partition operation or bring a disk online.
use super::*;
use allocation_batch::{check_file, record, resolved, write_fixture};
use disk_core::MIB;
use partitions::FileSystem;
use quick_partition_checks::table;
use recovery_tree::{Catalog, ROOT};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};
use table_archive::Archive;
use table_restore::{Entry, Layout};

#[path = "left_merge_checks.rs"]
mod left_merge;

const PREPARE: &str = "partition-reallocation-4kn-prepare";
const ONCE: &str = r"C:\ProgramData\DiskStudio-reallocation-4kn-v1.once";
const WITNESS: &str = r"C:\ProgramData\DiskStudio-reallocation-4kn-v1.json";
const SPLIT_TAIL: &str = r"C:\ProgramData\DiskStudio-reallocation-4kn-v1.tail.json";
const TAIL_LABEL: &str = "NEWVOLUME";
const TAIL_OFFSET: u64 = 2368;
const TAIL_SIZE: u64 = 64;
const ADJACENT_GROW: u64 = 32;
const MOVE_AMOUNT: u64 = 64;
const JOURNAL_LABEL: &str = "DS4KMOVEJRN";
const JOURNAL_OFFSET: u64 = 32 * MIB;
const JOURNAL_SIZE: u64 = 256 * MIB;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Split,
    Adjacent,
    Across,
    Transfer,
    MergeLeft,
    MergeLockFailure,
}

#[derive(Clone, Copy)]
pub(super) enum EntryPoint {
    Prepare,
    Inspect(Phase),
    Report(Option<Phase>),
    Left(left_merge::EntryPoint),
}

impl EntryPoint {
    pub(super) fn parse(mode: &str) -> Option<Self> {
        let mode = mode.strip_prefix("Partition-Reallocation-4Kn-")?;
        if let Some(mode) = mode.strip_prefix("Left-") {
            return left_merge::EntryPoint::parse(mode).map(Self::Left);
        }
        if mode == "Prepare" {
            return Some(Self::Prepare);
        }
        if mode == "Prepare-Report" {
            return Some(Self::Report(None));
        }
        let (phase, report) = mode
            .strip_suffix("-Report")
            .map_or((mode, false), |phase| (phase, true));
        let phase = match phase {
            "Split" => Phase::Split,
            "Adjacent" => Phase::Adjacent,
            "Across" => Phase::Across,
            "Transfer" => Phase::Transfer,
            "Merge-Left" => Phase::MergeLeft,
            "Merge-Lock-Failure" => Phase::MergeLockFailure,
            _ => return None,
        };
        Some(if report {
            Self::Report(Some(phase))
        } else {
            Self::Inspect(phase)
        })
    }

    pub(super) fn run(self) -> Result<String, String> {
        match self {
            Self::Prepare => prepare(),
            Self::Inspect(phase) => inspect(phase),
            Self::Report(phase) => backup_protection::report(phase.map_or(PREPARE, Phase::log)),
            Self::Left(entry) => entry.run(),
        }
    }
}

impl Phase {
    fn log(self) -> &'static str {
        match self {
            Self::Split => "partition-reallocation-4kn-split",
            Self::Adjacent => "partition-reallocation-4kn-adjacent",
            Self::Across => "partition-reallocation-4kn-across",
            Self::Transfer => "partition-reallocation-4kn-transfer",
            Self::MergeLeft => "partition-reallocation-4kn-merge-left",
            Self::MergeLockFailure => "partition-reallocation-4kn-merge-lock-failure",
        }
    }

    fn offline(self) -> bool {
        matches!(self, Self::Across | Self::Transfer | Self::MergeLeft)
    }
}

struct VolumeSpec {
    label: &'static str,
    offset: u64,
    size: u64,
    file: &'static str,
    seed: u8,
}

const VOLUMES: [VolumeSpec; 8] = [
    VolumeSpec {
        label: "DSREALLOCSPLIT",
        offset: 2048,
        size: 384,
        file: "SPLIT.BIN",
        seed: 7,
    },
    VolumeSpec {
        label: "DSREALLOCGROW",
        offset: 3072,
        size: 128,
        file: "GROW.BIN",
        seed: 13,
    },
    VolumeSpec {
        label: "DSREALLOCMOVE",
        offset: 3200,
        size: 128,
        file: "MOVE.BIN",
        seed: 29,
    },
    VolumeSpec {
        label: "DSREALLOCDONOR",
        offset: 4096,
        size: 384,
        file: "DONOR.BIN",
        seed: 43,
    },
    VolumeSpec {
        label: "DSREALLOCRECV",
        offset: 4480,
        size: 128,
        file: "RECEIVER.BIN",
        seed: 61,
    },
    VolumeSpec {
        label: "DSREALLOCLEFT",
        offset: 5120,
        size: 128,
        file: "ABSORBED.BIN",
        seed: 83,
    },
    VolumeSpec {
        label: "DSREALLOCKEEP",
        offset: 5248,
        size: 256,
        file: "KEEP.BIN",
        seed: 101,
    },
    VolumeSpec {
        label: "DSREALLOCADJ",
        offset: 2560,
        size: 128,
        file: "ADJACENT.BIN",
        seed: 113,
    },
];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    desktop: String,
    a: Archive,
    b: Archive,
    original_guids: Vec<String>,
    os_layout: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Tail {
    guid: String,
    attributes: u64,
    name: Vec<u16>,
}

impl From<&Entry> for Tail {
    fn from(entry: &Entry) -> Self {
        Self {
            guid: entry.guid.clone(),
            attributes: entry.attributes,
            name: entry.name.clone(),
        }
    }
}

fn checked_desktop() -> Result<&'static str, String> {
    option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        .ok_or_else(|| "Build must bind the verified desktop payload".into())
}

fn write_new(path: &str, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(err)
}

fn read_evidence<T: serde::de::DeserializeOwned>(path: &str, limit: usize) -> Result<T, String> {
    let file = File::open(path).map_err(err)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() > limit {
        return Err("Saved evidence is over the expected size".into());
    }
    serde_json::from_slice(&bytes).map_err(err)
}

fn witness() -> Result<Witness, String> {
    let witness: Witness = read_evidence(WITNESS, 8 * 1024 * 1024)?;
    if witness.desktop != checked_desktop()? || witness.a.sector != 4096 || witness.b.sector != 4096
    {
        return Err("Unexpected witness build or sector size".into());
    }
    Ok(witness)
}

pub(super) fn state(disks: &[Disk], serial: &str, offline: bool) -> Result<(), String> {
    // Callers first run inventory(), which validates the guest UUID, exact disk
    // count, capacities, and the full namespace-1 serials.
    disks
        .iter()
        .find(|d| {
            d.serial_number.as_deref().map(disk_identity::clean) == Some(serial)
                && d.is_boot == Some(false)
                && d.is_system == Some(false)
                && d.is_read_only == Some(false)
                && d.is_offline == Some(offline)
                && d.partition_style == Some(2)
        })
        .map(|_| ())
        .ok_or_else(|| format!("Unexpected protection or online state for {serial}"))
}

pub(super) fn os_layout(disks: &[Disk]) -> Result<String, String> {
    let os = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some("DS-OS-0001"))
        .ok_or("Guest OS disk absent")?;
    storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.number == os.number && d.hardware.is_system)
        .and_then(|d| d.hardware.layout_key)
        .ok_or_else(|| "Guest OS metadata layout key unavailable".into())
}

fn entry_at(layout: &Layout, offset: u64, size: u64) -> Result<&Entry, String> {
    let matching: Vec<_> = layout
        .entries
        .iter()
        .filter(|e| {
            (e.offset, e.size) == (offset * MIB, size * MIB)
                && e.kind == partitions::policy::BASIC_DATA
                && e.attributes & !(1 << 63) == 0
        })
        .collect();
    if matching.len() != 1 {
        return Err(format!(
            "Expected unique basic data entry at {offset} MiB of {size} MiB"
        ));
    }
    Ok(matching[0])
}

fn sample(seed: u8) -> Vec<u8> {
    (0..131_089usize)
        .map(|index| ((index * usize::from(seed) + index / 131) % 251) as u8)
        .collect()
}

pub(super) fn metadata_partition(
    serial: &str,
    label: Option<&str>,
    offset: u64,
    size: u64,
) -> Result<storage::LocalPartition, String> {
    let bound = selection(serial, None)?;
    let mut found = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.identity.as_deref() == Some(bound.target.0.as_str()))
        .ok_or("Bound partition disk absent")?
        .partitions
        .into_iter()
        .filter(|p| p.base.start == offset * MIB && p.base.size == size * MIB && !p.base.system);
    let part = found.next().ok_or("Partition range not found")?;
    if found.next().is_some()
        || label.is_some_and(|label| {
            part.volume_label.as_deref() != Some(label) || part.base.fs != "NTFS"
        })
    {
        return Err("Partition range, NTFS, or label disagree".into());
    }
    Ok(part)
}

fn prepare() -> Result<String, String> {
    let mut log = backup_protection::begin(PREPARE)?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, false)?;
    state(&disks, B, false)?;
    let os = os_layout(&disks)?;
    let base_a = table_restore::archive_layout(&table(A)?)?;
    let b = table(B)?;
    entry_at(
        &table_restore::archive_layout(&b)?,
        JOURNAL_OFFSET / MIB,
        JOURNAL_SIZE / MIB,
    )?;
    let journal = metadata_partition(
        B,
        Some(JOURNAL_LABEL),
        JOURNAL_OFFSET / MIB,
        JOURNAL_SIZE / MIB,
    )?;
    let letter = journal
        .drive_letter
        .ok_or("B journal volume must have a drive letter")?;
    let start = VOLUMES[0].offset * MIB;
    let stop = (VOLUMES[6].offset + VOLUMES[6].size) * MIB;
    let target = partitions::status()
        .targets
        .into_iter()
        .find(|t| t.serial == A && t.available)
        .ok_or("A target not ready")?;
    if !target
        .free_ranges
        .iter()
        .any(|r| r.offset <= start && r.offset.saturating_add(r.size) >= stop)
        || base_a.entries.len() + VOLUMES.len() + 1 > base_a.count as usize
    {
        return Err("Fixed A test reservation must be entirely unused before preparation".into());
    }
    write_new(ONCE, b"Do not repeat preparation for this fixture set.")?;
    let original_guids = create_fixtures(&mut log, &VOLUMES)?;
    let a = table(A)?;
    let mut retained = table_restore::archive_layout(&a)?;
    retained
        .entries
        .retain(|entry| !original_guids.contains(&entry.guid));
    if !retained.agrees(&base_a) || table(B)? != b || os_layout(&inventory()?)? != os {
        return Err("Fixture setup changed a pre-existing partition table entry".into());
    }
    write_new(
        WITNESS,
        &serde_json::to_vec(&Witness {
            desktop: checked_desktop()?.into(),
            a,
            b,
            original_guids,
            os_layout: os,
        })
        .map_err(err)?,
    )?;
    record(
        &mut log,
        json!({"event":"passed","fixtureCount":VOLUMES.len(),"journalDrive":letter}),
    )?;
    Ok(format!("Prepared {} 4Kn fixtures on A. Use the verified B volume {JOURNAL_LABEL} ({letter}) for native move journals.", VOLUMES.len()))
}

fn create_fixtures(log: &mut File, specs: &[VolumeSpec]) -> Result<Vec<String>, String> {
    let mut guids = Vec::with_capacity(specs.len());
    for spec in specs {
        state(&inventory()?, A, false)?;
        let (selected, after) = fat_resize_checks::create_capacity_fixture(
            log,
            (FileSystem::Ntfs, spec.label, 4096),
            spec.offset * MIB,
            spec.size * MIB,
        )?;
        guids.push(entry_at(&after, spec.offset, spec.size)?.guid.clone());
        write_fixture(
            &selected,
            spec.offset * MIB,
            spec.size * MIB,
            spec.file,
            &sample(spec.seed),
            true,
        )?;
        record(
            log,
            json!({"event":"fixture", "label":spec.label, "partition":selected.partition,
            "offsetMiB":spec.offset, "sizeMiB":spec.size}),
        )?;
    }
    Ok(guids)
}

/// Expected geometry comes from the fixture specification, not the product planner.
fn final_scope(index: usize, phase: Phase) -> Option<(u64, u64)> {
    let spec = &VOLUMES[index];
    match (index, phase) {
        (0, _) => Some((spec.offset, TAIL_OFFSET - spec.offset)),
        (1, Phase::Across | Phase::Transfer | Phase::MergeLeft) => {
            Some((spec.offset, spec.size + MOVE_AMOUNT))
        }
        (2, Phase::Across | Phase::Transfer | Phase::MergeLeft) => {
            Some((spec.offset + MOVE_AMOUNT, spec.size))
        }
        (3, Phase::Transfer | Phase::MergeLeft) => Some((spec.offset, spec.size - MOVE_AMOUNT)),
        (4, Phase::Transfer | Phase::MergeLeft) => {
            Some((spec.offset - MOVE_AMOUNT, spec.size + MOVE_AMOUNT))
        }
        (5, Phase::MergeLeft) => None,
        (6, Phase::MergeLeft) => Some((VOLUMES[5].offset, spec.size + VOLUMES[5].size)),
        (7, Phase::Adjacent | Phase::Across | Phase::Transfer | Phase::MergeLeft) => {
            Some((spec.offset, spec.size + ADJACENT_GROW))
        }
        _ => Some((spec.offset, spec.size)),
    }
}

fn expected_layout(
    mut base: Layout,
    witness: &[String],
    actual: &Layout,
    phase: Phase,
) -> Result<Layout, String> {
    let tail = entry_at(actual, TAIL_OFFSET, TAIL_SIZE)?;
    if witness.len() != VOLUMES.len() || base.entries.iter().any(|e| e.guid == tail.guid) {
        return Err("Fixture witness or fresh tail GUID is invalid".into());
    }
    for (index, guid) in witness.iter().enumerate() {
        let spec = &VOLUMES[index];
        if entry_at(&base, spec.offset, spec.size)?.guid != *guid {
            return Err("Fixture GUID does not match its frozen extent".into());
        }
        if let Some((offset, size)) = final_scope(index, phase) {
            let entry = base
                .entries
                .iter_mut()
                .find(|entry| entry.guid == *guid)
                .ok_or("Fixture GUID missing")?;
            entry.offset = offset * MIB;
            entry.size = size * MIB;
        } else {
            base.entries.retain(|entry| entry.guid != *guid);
        }
    }
    base.entries.push(tail.clone());
    Ok(base)
}

fn merged_sample(offset: u64, size: u64, spec: &VolumeSpec) -> Result<(String, String), String> {
    let part = metadata_partition(A, None, offset, size)?;
    let bound = selection(A, Some(part.number))?;
    let device = resolved(&bound, offset * MIB, size * MIB, false)?;
    let mut reader = data_windows::reader(&device)?;
    let mut files = Vec::new();
    let mut dirs = Catalog::default();
    recovery::scan_filesystem_with_directories(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |file| {
            if file.name == spec.file {
                files.push(file);
            }
        },
        |directory| dirs.insert(directory),
    )?;
    let [file] = files.as_slice() else {
        return Err("Merged sentinel missing or ambiguous".into());
    };
    let path = dirs.path(
        file.directory
            .as_deref()
            .ok_or("Missing merge directory identity")?,
    )?;
    let [root, folder] = path.as_slice() else {
        return Err("Merged file is not inside a new root-level directory".into());
    };
    if root.id != ROOT
        || folder.deleted
        || !folder
            .name
            .strip_prefix("DiskStudio-Merge-")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    {
        return Err("Unexpected native merge output directory".into());
    }
    let expected = sample(spec.seed);
    if file.size != expected.len() as u64 {
        return Err("Merged sentinel size differs".into());
    }
    let mut bytes = Vec::with_capacity(expected.len());
    recovery::extract(&mut reader, file, &mut bytes, expected.len() as u64, |_| {
        Ok(())
    })?;
    if bytes != expected {
        return Err("Merged sentinel differs byte for byte".into());
    }
    Ok((folder.name.clone(), format!("{:x}", Sha256::digest(bytes))))
}

fn inspect(phase: Phase) -> Result<String, String> {
    let layout_phase = if phase == Phase::MergeLockFailure {
        Phase::Transfer
    } else {
        phase
    };
    let mut log = backup_protection::begin(phase.log())?;
    let witness = witness()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    state(&disks, A, phase.offline())?;
    state(&disks, B, false)?;
    if os_layout(&disks)? != witness.os_layout || table(B)? != witness.b {
        return Err("B GPT or guest OS partition metadata differs".into());
    }
    let actual_a = table(A)?;
    let actual = table_restore::archive_layout(&actual_a)?;
    let expected = expected_layout(
        table_restore::archive_layout(&witness.a)?,
        &witness.original_guids,
        &actual,
        layout_phase,
    )?;
    if !actual.agrees(&expected) {
        return Err("A GPT has a missing, stale or unexpected field or entry".into());
    }
    let tail = Tail::from(entry_at(&actual, TAIL_OFFSET, TAIL_SIZE)?);
    if phase != Phase::Split && read_evidence::<Tail>(SPLIT_TAIL, 2048)? != tail {
        return Err("The verified split tail's GUID, flags, or name changed".into());
    }
    if !phase.offline() {
        metadata_partition(A, Some(TAIL_LABEL), TAIL_OFFSET, TAIL_SIZE)?;
    }
    let mut digests = Vec::new();
    for (index, spec) in VOLUMES.iter().enumerate() {
        let digest = match final_scope(index, layout_phase) {
            Some((offset, size)) => {
                let part =
                    metadata_partition(A, (!phase.offline()).then_some(spec.label), offset, size)?;
                check_file(
                    &selection(A, Some(part.number))?,
                    offset * MIB,
                    size * MIB,
                    spec.file,
                    &sample(spec.seed),
                )?
            }
            None => {
                let (offset, size) =
                    final_scope(6, layout_phase).ok_or("Merge keep extent missing")?;
                merged_sample(offset, size, spec)?.1
            }
        };
        digests.push((spec.label, digest));
    }
    if phase == Phase::MergeLockFailure {
        let (offset, size) = final_scope(6, layout_phase).ok_or("Merge keep extent missing")?;
        let (directory, digest) = merged_sample(offset, size, &VOLUMES[5])?;
        if digest != digests[5].1 {
            return Err("Staged merge sample and retained source digest differ".into());
        }
        record(
            &mut log,
            json!({"event":"staged-copy", "directory":directory, "sha256":digest}),
        )?;
    }
    state(&inventory()?, A, phase.offline())?;
    if table(A)? != actual_a || table(B)? != witness.b {
        return Err("GPT differed after readback".into());
    }
    if phase == Phase::Split {
        write_new(SPLIT_TAIL, &serde_json::to_vec(&tail).map_err(err)?)?;
    }
    record(
        &mut log,
        json!({"event":"passed","aOffline":phase.offline(),"bGptUnchanged":true,
        "guestOsLayoutUnchanged":true,"fileSha256":digests}),
    )?;
    Ok(format!(
        "PASS: {}. A GPT, B GPT, guest OS metadata and {} complete sample files verified.",
        phase.log(),
        VOLUMES.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slot: u32, offset: u64, size: u64) -> Entry {
        Entry {
            slot,
            guid: uuid::Uuid::from_u128(u128::from(slot)).to_string(),
            kind: partitions::policy::BASIC_DATA.into(),
            offset: offset * MIB,
            size: size * MIB,
            attributes: 0,
            name: vec![],
        }
    }

    #[test]
    fn split_witness_rejects_reused_tail_identity_and_ignores_slot_renumbering() {
        let mut entries = VOLUMES
            .iter()
            .enumerate()
            .map(|(i, spec)| row(i as u32 + 1, spec.offset, spec.size))
            .collect::<Vec<_>>();
        let witness = entries
            .iter()
            .map(|entry| entry.guid.clone())
            .collect::<Vec<_>>();
        entries.push(row(9, 32, 256));
        let base = Layout {
            guid: "disk".into(),
            first: 6,
            last: 4_194_299,
            count: 128,
            entries,
        };
        let mut actual = base.clone();
        actual.entries[0].size = 320 * MIB;
        actual.entries[1].slot = 50;
        actual.entries.push(row(10, TAIL_OFFSET, TAIL_SIZE));
        let expected = expected_layout(base.clone(), &witness, &actual, Phase::Split).unwrap();
        assert!(actual.agrees(&expected));
        actual.entries[8].attributes = 1;
        assert!(!actual.agrees(&expected));
        actual.entries[9].guid.clone_from(&witness[0]);
        assert!(expected_layout(base, &witness, &actual, Phase::Split).is_err());
    }
}

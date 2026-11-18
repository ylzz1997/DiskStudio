//! Isolated fixture preparation and independent readback for the native NTFS merge.
//! All partition and file mutations are confined to the verified 4Kn test disk A.
use super::*;
use allocation_batch::{fixture_selection, record, resolved, root, write_fixture};
use disk_core::MIB;
use partitions::{reallocation, FileSystem};
use quick_partition_checks::table;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
};
use windows_sys::Win32::{
    Security::{
        GetKernelObjectSecurity, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
};

const PREPARE_LOG: &str = "partition-merge-4kn-prepare";
const MERGE_LOG: &str = "partition-merge-4kn-check";
const INSPECT_LOG: &str = "partition-merge-4kn-inspect";
const CONTINUE_LOG: &str = "partition-merge-4kn-continue";
const PROBE_LOG: &str = "partition-merge-4kn-catalogue-probe";
const CORRECTED_LOG: &str = "partition-merge-4kn-corrected";
const OWNER_INSPECT_LOG: &str = "partition-merge-4kn-owner-inspect";
const FRESH_PREPARE_LOG: &str = "partition-merge-4kn-fresh-prepare";
const FRESH_CHECK_LOG: &str = "partition-merge-4kn-fresh-check";
const FIRST_DESKTOP: &str = "d39743b2f2e22a21c292e005ec6607e64248caf7d37842bf4114dc39057b8731";
const FIRST_HELPER: &str = "b75b68e32ad8ce7e9b083150f5a0a50f88bfe84d256b23dbb7a127b92308a53c";
const DIAGNOSTIC_HELPER: &str = "3dec77e3108ad6f766b2225bd10bb7d8207abcfe8029120bafbf4cc5a6da6ef4";
const TXF_DESKTOP: &str = "2813977391ff606f1595312d00272ac152a87d410881ed3b967131bbaa08b8fd";
const TXF_HELPER: &str = "1db3005b5ea49e66a557425f6ce8d3283944ce9b26294c5b4ce4036896d9274b";
const CATALOGUE_HELPER: &str = "37b37d5d8586a8acf130862e359a7b9afa00254e38204d5a1e97d2e3e034e219";
const CORRECTED_DESKTOP: &str = "d2e2080f260748ccdb206f958a57eec049fe98293ba136bc1d718e8ea3b2645d";
const CORRECTED_HELPER: &str = "a6863eb7bba506406d8ee45f2a54aa00208b52787c518f57bb17cde22ec23dcc";
const OWNER_INSPECT_HELPER: &str =
    "78fb55e1295b8d43392511af45ed3cfc56d7679889a02d0c6185a6eff46dd28a";
const BASE: &str = r"C:\ProgramData\DiskStudio-partition-merge-4kn-20261117";
const KEEP_LABEL: &str = "DSMERGEKEEP";
const ABSORB_LABEL: &str = "DSMERGEFROM";
const KEEP_SIZE: u64 = 256 * MIB;
const ABSORB_SIZE: u64 = 128 * MIB;
const KEEP_FILE: &str = "DSKEEP.BIN";
const ABSORB_FILE: &str = "DSMFILE.BIN";
const HARDLINK: &str = "DSMLINK.BIN";
const STREAM: &str = "DSMFILE.BIN:MergeSample:$DATA";
const LONG_FILE: &str = "合并样本 file.txt";

#[derive(Clone, Copy)]
struct FixtureCase {
    keep: &'static str,
    absorb: &'static str,
    witness: &'static str,
    prepare_once: &'static str,
}

const ORIGINAL: FixtureCase = FixtureCase {
    keep: KEEP_LABEL,
    absorb: ABSORB_LABEL,
    witness: "fixture",
    prepare_once: "prepare-once",
};

const FRESH: FixtureCase = FixtureCase {
    keep: "DSMERGEKEEP2",
    absorb: "DSMERGEFROM2",
    witness: "fresh-fixture",
    prepare_once: "fresh-prepare-once",
};

#[derive(Serialize, Deserialize)]
struct Fixture {
    desktop: String,
    offset: u64,
    keep_guid: String,
    absorb_guid: String,
    base_a: table_archive::Archive,
    base_b: table_archive::Archive,
    security: String,
    written: u64,
}

fn witness(ext: &str) -> PathBuf {
    PathBuf::from(format!("{BASE}.{ext}"))
}

fn write_witness(ext: &str, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(witness(ext))
        .map_err(err)?;
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(err)
}

fn read_witness(ext: &str, max: u64) -> Result<Vec<u8>, String> {
    read_bounded(&witness(ext), max)
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(err)?;
    let mut bytes = vec![];
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() as u64 > max {
        return Err("Merge fixture witness too large".into());
    }
    Ok(bytes)
}

fn sample(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * seed + i / 127) % 251) as u8)
        .collect()
}

fn desktop() -> Result<&'static str, String> {
    option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        .ok_or_else(|| "Missing desktop build binding".into())
}

fn selection_for(label: &str, offset: u64, size: u64) -> Result<Selection, String> {
    let (selected, actual) = fixture_selection("NTFS", Some(label), size)?;
    if actual != offset {
        return Err("NTFS merge fixture location changed".into());
    }
    Ok(selected)
}

fn verify_file(
    file: &File,
    selected: &Selection,
    volume: &data_windows::Resolved,
) -> Result<(), String> {
    data_windows::file_location(file)?.confirm_partition(
        &selected.target,
        &selected.layout_key,
        &volume.volumes[0],
        (volume.disk.number, volume.offset, volume.size),
    )?;
    if file.metadata().map_err(err)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("Merge sample unexpectedly resolves to a reparse point".into());
    }
    Ok(())
}

fn open_sample(
    path: &Path,
    selected: &Selection,
    volume: &data_windows::Resolved,
) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(err)?;
    verify_file(&file, selected, volume)?;
    Ok(file)
}

fn native_data(mut file: &File, expected: &[u8]) -> Result<String, String> {
    let mut seen = Vec::with_capacity(expected.len());
    Read::by_ref(&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut seen)
        .map_err(err)?;
    if seen != expected {
        return Err("Independent Windows file bytes differ".into());
    }
    Ok(format!("{:x}", Sha256::digest(seen)))
}

fn security(file: &File) -> Result<String, String> {
    let parts = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0;
    unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            parts,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if !(20..=65536).contains(&needed) {
        return Err("Native security descriptor size invalid".into());
    }
    let mut data = vec![0u32; (needed as usize).div_ceil(4)];
    let capacity = needed;
    if unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            parts,
            data.as_mut_ptr().cast(),
            capacity,
            &mut needed,
        )
    } == 0
        || needed != capacity
    {
        return Err(format!(
            "Native security descriptor read failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let raw = data
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .take(needed as usize)
        .collect::<Vec<_>>();
    Ok(format!("{:x}", Sha256::digest(raw)))
}

fn file_id(file: &File) -> Result<(u32, u64), String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
        || info.nNumberOfLinks != 2
    {
        return Err("Native hard-link identity or link count mismatch".into());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

pub(super) fn prepare() -> Result<String, String> {
    let mut log = backup_protection::begin(PREPARE_LOG)?;
    prepare_case(ORIGINAL, &mut log)
}

fn prepare_case(case: FixtureCase, log: &mut File) -> Result<String, String> {
    write_witness(case.prepare_once, b"single preparation")?;
    allocation_batch::set_online(A)?;
    let base_b = table(B)?;
    let target = selection(A, None)?.target;
    let item = partitions::status()
        .targets
        .into_iter()
        .find(|t| t.target == target && t.available)
        .ok_or("A fixture target unavailable")?;
    let offset = item
        .free_ranges
        .iter()
        .find(|gap| gap.size >= KEEP_SIZE + ABSORB_SIZE)
        .ok_or("A lacks room for two new adjacent merge fixtures")?
        .offset;
    fat_resize_checks::create_capacity_fixture(
        log,
        (FileSystem::Ntfs, case.keep, 4096),
        offset,
        KEEP_SIZE,
    )?;
    fat_resize_checks::create_capacity_fixture(
        log,
        (FileSystem::Ntfs, case.absorb, 4096),
        offset + KEEP_SIZE,
        ABSORB_SIZE,
    )?;
    let base_a = table(A)?;
    let layout = table_restore::archive_layout(&base_a)?;
    let guid = |offset, size| {
        layout
            .entries
            .iter()
            .find(|p| (p.offset, p.size) == (offset, size))
            .map(|p| p.guid.clone())
            .ok_or("Fixture entry missing")
    };
    let keep = selection_for(case.keep, offset, KEEP_SIZE)?;
    let absorb = selection_for(case.absorb, offset + KEEP_SIZE, ABSORB_SIZE)?;
    write_fixture(
        &keep,
        offset,
        KEEP_SIZE,
        KEEP_FILE,
        &sample(19, 65_553),
        true,
    )?;
    write_fixture(
        &absorb,
        offset + KEEP_SIZE,
        ABSORB_SIZE,
        ABSORB_FILE,
        &sample(31, 131_089),
        true,
    )?;
    write_fixture(
        &absorb,
        offset + KEEP_SIZE,
        ABSORB_SIZE,
        LONG_FILE,
        b"DiskStudio NTFS merge unicode fixture\n",
        true,
    )?;
    let volume = resolved(&absorb, offset + KEEP_SIZE, ABSORB_SIZE, true)?;
    let root = root(&volume)?;
    let mut alternate = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root.join(STREAM))
        .map_err(err)?;
    verify_file(&alternate, &absorb, &volume)?;
    alternate
        .write_all(&sample(47, 1025))
        .and_then(|_| alternate.sync_all())
        .map_err(err)?;
    drop(alternate);
    fs::hard_link(root.join(ABSORB_FILE), root.join(HARDLINK)).map_err(err)?;
    let base_file = open_sample(&root.join(ABSORB_FILE), &absorb, &volume)?;
    let hardlink = open_sample(&root.join(HARDLINK), &absorb, &volume)?;
    if file_id(&base_file)? != file_id(&hardlink)? || table(A)? != base_a || table(B)? != base_b {
        return Err("Hard-link creation or fixture GPT verification failed".into());
    }
    let witness_data = Fixture {
        desktop: desktop()?.to_owned(),
        offset,
        keep_guid: guid(offset, KEEP_SIZE)?,
        absorb_guid: guid(offset + KEEP_SIZE, ABSORB_SIZE)?,
        base_a,
        base_b,
        security: security(&base_file)?,
        written: base_file.metadata().map_err(err)?.last_write_time(),
    };
    write_witness(
        case.witness,
        &serde_json::to_vec(&witness_data).map_err(err)?,
    )?;
    record(
        log,
        json!({"event":"fixture-ready","keep":keep.partition,"absorb":absorb.partition,"offsetMiB":offset/MIB}),
    )?;
    Ok(format!(
        "Fixtures ready on 4Kn A. Keep {} ({}), absorb {} ({}), start {} MiB.",
        keep.partition.unwrap(),
        case.keep,
        absorb.partition.unwrap(),
        case.absorb,
        offset / MIB
    ))
}

fn fixture(expected_desktop: &str) -> Result<Fixture, String> {
    read_fixture(ORIGINAL, expected_desktop)
}

fn read_fixture(case: FixtureCase, expected_desktop: &str) -> Result<Fixture, String> {
    let fixture: Fixture =
        serde_json::from_slice(&read_witness(case.witness, 1024 * 1024)?).map_err(err)?;
    if fixture.desktop != expected_desktop {
        return Err("Fixture bound to a different desktop".into());
    }
    Ok(fixture)
}

pub(super) fn check() -> Result<String, String> {
    let mut log = backup_protection::begin(MERGE_LOG)?;
    let fixture = fixture(desktop()?)?;
    dispatch(ORIGINAL, &fixture, "execute-once", &mut log)
}

fn dispatch(
    case: FixtureCase,
    fixture: &Fixture,
    marker: &str,
    log: &mut File,
) -> Result<String, String> {
    if table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("Pre-merge GPT differs from frozen witnesses".into());
    }
    let keep = selection_for(case.keep, fixture.offset, KEEP_SIZE)?;
    let absorb = selection_for(case.absorb, fixture.offset + KEEP_SIZE, ABSORB_SIZE)?;
    let target = partitions::status()
        .targets
        .into_iter()
        .find(|t| t.target == keep.target && t.available)
        .ok_or("A merge target unavailable")?;
    let preview = reallocation::prepare(reallocation::Request {
        target: target.target,
        revision: target.revision,
        operation: reallocation::Operation::Merge {
            keep: keep.partition.unwrap(),
            absorb: absorb.partition.unwrap(),
        },
    })?
    .ok_or("Unexpected native dialog during rightward merge")?;
    let folder = uuid::Uuid::parse_str(&preview.confirmation.token).map_err(err)?;
    if preview.steps.len() != 2
        || !preview.journals.is_empty()
        || !matches!(&preview.steps[0], reallocation::Step::CopyAndRemove { keep, absorb } if keep.guid == fixture.keep_guid && absorb.guid == fixture.absorb_guid)
        || !matches!(&preview.steps[1], reallocation::Step::Resize { partition, new_size } if partition.guid == fixture.keep_guid && *new_size == KEEP_SIZE + ABSORB_SIZE)
    {
        return Err("Merge plan differs from the exact source and keep fixture identities".into());
    }
    write_witness(marker, folder.to_string().as_bytes())?;
    record(
        log,
        json!({"event":"dispatch-once","folder":folder.to_string(),"steps":preview.steps}),
    )?;
    let result = reallocation::execute(preview.confirmation.token, |progress| {
        let _ = record(log, json!({"event":"progress","progress":progress}));
    });
    record(log, json!({"event":"merge-result","outcome":result}))?;
    let result = result?;
    if !result.batch.verified {
        return Err(result.batch.message);
    }
    inspect_files(case, fixture, folder, log)
}

pub(super) fn inspect() -> Result<String, String> {
    let mut log = backup_protection::begin(INSPECT_LOG)?;
    inspect_files(
        ORIGINAL,
        &fixture(desktop()?)?,
        attempt_id("execute-once")?,
        &mut log,
    )
}

fn attempt_id(marker: &str) -> Result<uuid::Uuid, String> {
    uuid::Uuid::parse_str(std::str::from_utf8(&read_witness(marker, 36)?).map_err(err)?)
        .map_err(err)
}

fn prior_events(kind: &str, helper: &str, desktop: &str) -> Result<Vec<serde_json::Value>, String> {
    let path = PathBuf::from(format!(r"C:\ProgramData\DiskStudio-{kind}-{helper}.jsonl"));
    let bytes = read_bounded(&path, 64 * 1024)?;
    let events = std::str::from_utf8(&bytes)
        .map_err(err)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    if events.first().is_none_or(|event| {
        event["event"] != "start"
            || event["desktop"] != desktop
            || event["helper"] != helper
            || event["kind"] != kind
    }) {
        return Err("Saved merge record differs from the expected lab run".into());
    }
    Ok(events)
}

pub(super) fn continue_check() -> Result<String, String> {
    let mut log = backup_protection::begin(CONTINUE_LOG)?;
    if desktop()? == FIRST_DESKTOP {
        return Err("Continuation requires the corrected desktop".into());
    }
    let fixture = fixture(FIRST_DESKTOP)?;
    let previous = prior_events(MERGE_LOG, FIRST_HELPER, FIRST_DESKTOP)?;
    let message = previous
        .last()
        .and_then(|event| {
            (event["event"] == "merge-result" && event["outcome"]["Ok"]["verified"] == false)
                .then(|| event["outcome"]["Ok"]["message"].as_str())
                .flatten()
        })
        .ok_or("Unconfirmed initial merge outcome")?;
    if !message.contains("存在无法由卷根目录完整核验的活动 NTFS 文件") {
        return Err("Initial merge stopped at a different phase".into());
    }
    let diagnosis = prior_events(
        "partition-merge-4kn-diagnose",
        DIAGNOSTIC_HELPER,
        FIRST_DESKTOP,
    )?;
    if diagnosis.last().is_none_or(|event| {
        event["event"] != "diagnosed"
            || event["gptUnchanged"] != true
            || event["destinationAbsent"] != true
            || event["sourceFileSha256"] != format!("{:x}", Sha256::digest(sample(31, 131_089)))
    }) {
        return Err("Initial independent verification is missing".into());
    }
    let keep = selection_for(KEEP_LABEL, fixture.offset, KEEP_SIZE)?;
    let kept = resolved(&keep, fixture.offset, KEEP_SIZE, true)?;
    let previous_id = attempt_id("execute-once")?;
    if root(&kept)?
        .join(format!("DiskStudio-Merge-{previous_id}"))
        .try_exists()
        .map_err(err)?
    {
        return Err("A directory exists from the first attempt".into());
    }
    dispatch(ORIGINAL, &fixture, "continue-once", &mut log)
}

pub(super) fn continue_report() -> Result<String, String> {
    backup_protection::report(CONTINUE_LOG)
}

pub(super) fn continue_inspect() -> Result<String, String> {
    let mut log = backup_protection::begin("partition-merge-4kn-continue-inspect")?;
    inspect_files(
        ORIGINAL,
        &fixture(FIRST_DESKTOP)?,
        attempt_id("continue-once")?,
        &mut log,
    )
}

fn verify_pre_catalogue_fixture() -> Result<Fixture, String> {
    let previous = prior_events(CONTINUE_LOG, TXF_HELPER, TXF_DESKTOP)?;
    if previous.last().is_none_or(|event| {
        event["event"] != "merge-result"
            || event["outcome"]["Ok"]["verified"] != false
            || !event["outcome"]["Ok"]["message"]
                .as_str()
                .is_some_and(|message| {
                    message.contains(
                        "NTFS 文件记录 5：合并无法完整核对 NTFS 文件属性 LoggedUtilityStream",
                    )
                })
    }) {
        return Err("Unexpected preceding merge outcome".into());
    }
    let fixture = fixture(FIRST_DESKTOP)?;
    if table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("Post-attempt GPT differs from the prepared pair".into());
    }
    let keep = selection_for(KEEP_LABEL, fixture.offset, KEEP_SIZE)?;
    let kept = resolved(&keep, fixture.offset, KEEP_SIZE, true)?;
    let keep_root = root(&kept)?;
    for marker in ["execute-once", "continue-once"] {
        let id = attempt_id(marker)?;
        if keep_root
            .join(format!("DiskStudio-Merge-{id}"))
            .try_exists()
            .map_err(err)?
        {
            return Err(format!(
                "An earlier attempt created a destination ({marker})"
            ));
        }
    }
    Ok(fixture)
}

pub(super) fn catalogue_probe() -> Result<String, String> {
    use ntfs::{Ntfs, NtfsAttributeType};

    let mut log = backup_protection::begin(PROBE_LOG)?;
    let fixture = verify_pre_catalogue_fixture()?;
    let offset = fixture.offset + KEEP_SIZE;
    let source = selection_for(ABSORB_LABEL, offset, ABSORB_SIZE)?;
    let volume = resolved(&source, offset, ABSORB_SIZE, true)?;
    let base = open_sample(&root(&volume)?.join(ABSORB_FILE), &source, &volume)?;
    let hash = native_data(&base, &sample(31, 131_089))?;
    drop(base);
    let mut reader = data_windows::reader(&volume)?;
    let ntfs = Ntfs::new(&mut reader).map_err(err)?;
    let root_record = ntfs.root_directory(&mut reader).map_err(err)?;
    let mut attributes = root_record.attributes();
    let mut logged = Vec::new();
    while let Some(item) = attributes.next(&mut reader) {
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.ty().map_err(err)? == NtfsAttributeType::LoggedUtilityStream {
            logged.push(json!({"name":attr.name().map_err(err)?.to_string_lossy(),"size":attr.value_length(),"flags":attr.flags().bits(),"resident":attr.is_resident()}));
        }
    }
    record(
        &mut log,
        json!({"event":"fixture-rechecked","gptUnchanged":true,"bothDestinationsAbsent":true,"sourceFileSha256":hash,"rootLoggedUtilityStreams":logged}),
    )?;
    let panic = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed = panic.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut observed) = observed.lock() {
            *observed = Some(info.to_string());
        }
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        partitions::merge::catalogue(&mut reader, None, &AtomicBool::new(false))
    }));
    std::panic::set_hook(previous_hook);
    let result = result.unwrap_or_else(|_| {
        Err(panic
            .lock()
            .ok()
            .and_then(|mut reason| reason.take())
            .unwrap_or_else(|| "Catalogue panicked with no location".into()))
    });
    if table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("GPT changed during the isolated catalogue probe".into());
    }
    let summary = result.as_ref().map(|catalogue| {
        json!({"names":catalogue.names.len(),"nodes":catalogue.nodes.len(),"streams":catalogue.nodes.values().map(|node| node.streams.len()).sum::<usize>()})
    });
    record(
        &mut log,
        json!({"event":"catalogue-probed","gptUnchanged":true,"outcome":summary}),
    )?;
    let summary = summary.map_err(Clone::clone)?;
    Ok(format!("Read-only source catalogue passed. Both GPTs match. Both prior destinations are absent.\nRoot logged streams: {logged:?}\nCatalogue: {summary}"))
}

pub(super) fn catalogue_probe_report() -> Result<String, String> {
    backup_protection::report(PROBE_LOG)
}

pub(super) fn corrected_check() -> Result<String, String> {
    if [FIRST_DESKTOP, TXF_DESKTOP].contains(&desktop()?) {
        return Err("Completion requires the corrected desktop".into());
    }
    let mut log = backup_protection::begin(CORRECTED_LOG)?;
    let probe = prior_events(PROBE_LOG, CATALOGUE_HELPER, TXF_DESKTOP)?;
    if probe.last().is_none_or(|event| {
        event["event"] != "catalogue-probed"
            || event["gptUnchanged"] != true
            || !event["outcome"]["Ok"].is_object()
    }) {
        return Err("No successful independent catalogue probe".into());
    }
    let fixture = verify_pre_catalogue_fixture()?;
    dispatch(ORIGINAL, &fixture, "corrected-once", &mut log)
}

pub(super) fn corrected_report() -> Result<String, String> {
    backup_protection::report(CORRECTED_LOG)
}

pub(super) fn corrected_inspect() -> Result<String, String> {
    let mut log = backup_protection::begin("partition-merge-4kn-corrected-inspect")?;
    inspect_files(
        ORIGINAL,
        &fixture(FIRST_DESKTOP)?,
        attempt_id("corrected-once")?,
        &mut log,
    )
}

pub(super) fn inspect_owner_failure() -> Result<String, String> {
    let mut log = backup_protection::begin(OWNER_INSPECT_LOG)?;
    let events = prior_events(CORRECTED_LOG, CORRECTED_HELPER, CORRECTED_DESKTOP)?;
    let id = attempt_id("corrected-once")?;
    if !events
        .iter()
        .any(|event| event["event"] == "dispatch-once" && event["folder"] == id.to_string())
        || events.last().is_none_or(|event| {
            event["event"] != "merge-result"
                || event["outcome"]["Ok"]["verified"] != false
                || !event["outcome"]["Ok"]["message"]
                    .as_str()
                    .is_some_and(|message| {
                        message.contains(&id.to_string()) && message.contains("os error 1307")
                    })
        })
    {
        return Err("Unexpected previous ownership failure witness".into());
    }
    let fixture = verify_pre_catalogue_fixture()?;
    let offset = fixture.offset + KEEP_SIZE;
    let absorb = selection_for(ABSORB_LABEL, offset, ABSORB_SIZE)?;
    let absorbed = resolved(&absorb, offset, ABSORB_SIZE, true)?;
    let original = root(&absorbed)?;
    let base = open_sample(&original.join(ABSORB_FILE), &absorb, &absorbed)?;
    if security(&base)? != fixture.security
        || base.metadata().map_err(err)?.last_write_time() != fixture.written
    {
        return Err("Source ownership, DACL or write time changed".into());
    }
    let original_sha = native_merge_payload(&original, &absorb, &absorbed, &base)?;
    let keep = selection_for(KEEP_LABEL, fixture.offset, KEEP_SIZE)?;
    let kept = resolved(&keep, fixture.offset, KEEP_SIZE, true)?;
    let path = root(&kept)?;
    let staged = path.join(format!("DiskStudio-Merge-{id}"));
    let attrs = fs::symlink_metadata(&staged).map_err(err)?;
    if !attrs.is_dir() || attrs.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("The staged path is not a regular directory".into());
    }
    let copy = open_sample(&staged.join(ABSORB_FILE), &keep, &kept)?;
    let copied_sha = native_merge_payload(&staged, &keep, &kept, &copy)?;
    let kept_sha = native_data(
        &open_sample(&path.join(KEEP_FILE), &keep, &kept)?,
        &sample(19, 65_553),
    )?;
    if original_sha != copied_sha || table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("Target bytes or original GPT changed during ownership diagnosis".into());
    }
    record(
        &mut log,
        json!({"event":"ownership-failure-verified","gptsUnchanged":true,"stagedFolder":id.to_string(),"sourceAndCopySha256":copied_sha,"keeperSha256":kept_sha,"sourceSecurityUnchanged":true}),
    )?;
    Ok(format!("GPTs, original NTFS files and owner/DACL match. Partial folder DiskStudio-Merge-{id} also contains the three expected byte samples and both hard links."))
}

fn native_merge_payload(
    folder: &Path,
    selected: &Selection,
    volume: &data_windows::Resolved,
    base: &File,
) -> Result<[String; 3], String> {
    let link = open_sample(&folder.join(HARDLINK), selected, volume)?;
    if file_id(base)? != file_id(&link)? {
        return Err("The two native hard links refer to different files".into());
    }
    Ok([
        native_data(base, &sample(31, 131_089))?,
        native_data(
            &open_sample(&folder.join(STREAM), selected, volume)?,
            &sample(47, 1025),
        )?,
        native_data(
            &open_sample(&folder.join(LONG_FILE), selected, volume)?,
            b"DiskStudio NTFS merge unicode fixture\n",
        )?,
    ])
}

pub(super) fn prepare_fresh() -> Result<String, String> {
    if [FIRST_DESKTOP, TXF_DESKTOP, CORRECTED_DESKTOP].contains(&desktop()?) {
        return Err(
            "Fresh fixture requires the desktop with corrected ownership restoration".into(),
        );
    }
    let mut log = backup_protection::begin(FRESH_PREPARE_LOG)?;
    let baseline = prior_events(OWNER_INSPECT_LOG, OWNER_INSPECT_HELPER, CORRECTED_DESKTOP)?;
    let staged = attempt_id("corrected-once")?.to_string();
    if baseline.last().is_none_or(|event| {
        event["event"] != "ownership-failure-verified"
            || event["gptsUnchanged"] != true
            || event["sourceSecurityUnchanged"] != true
            || event["stagedFolder"] != staged
    }) {
        return Err("No independent verification of the previous source and partial copy".into());
    }
    verify_pre_catalogue_fixture()?;
    prepare_case(FRESH, &mut log)
}

pub(super) fn check_fresh() -> Result<String, String> {
    let mut log = backup_protection::begin(FRESH_CHECK_LOG)?;
    dispatch(
        FRESH,
        &read_fixture(FRESH, desktop()?)?,
        "fresh-execute-once",
        &mut log,
    )
}

pub(super) fn report_fresh() -> Result<String, String> {
    backup_protection::report(FRESH_CHECK_LOG)
}

pub(super) fn inspect_fresh() -> Result<String, String> {
    let mut log = backup_protection::begin("partition-merge-4kn-fresh-inspect")?;
    inspect_files(
        FRESH,
        &read_fixture(FRESH, desktop()?)?,
        attempt_id("fresh-execute-once")?,
        &mut log,
    )
}

fn inspect_files(
    case: FixtureCase,
    fixture: &Fixture,
    id: uuid::Uuid,
    log: &mut File,
) -> Result<String, String> {
    let mut layout = table_restore::archive_layout(&fixture.base_a)?;
    layout
        .entries
        .retain(|entry| entry.guid != fixture.absorb_guid);
    layout
        .entries
        .iter_mut()
        .find(|p| p.guid == fixture.keep_guid)
        .ok_or("Keep entry missing")?
        .size = KEEP_SIZE + ABSORB_SIZE;
    let after = table(A)?;
    if !table_restore::archive_layout(&after)?.agrees(&layout) || table(B)? != fixture.base_b {
        return Err("Final GPT geometry differs or B changed".into());
    }
    let keep = selection_for(case.keep, fixture.offset, KEEP_SIZE + ABSORB_SIZE)?;
    let volume = resolved(&keep, fixture.offset, KEEP_SIZE + ABSORB_SIZE, true)?;
    let root = root(&volume)?;
    let folder = root.join(format!("DiskStudio-Merge-{id}"));
    let base = open_sample(&folder.join(ABSORB_FILE), &keep, &volume)?;
    if security(&base)? != fixture.security
        || base.metadata().map_err(err)?.last_write_time() != fixture.written
    {
        return Err("Final owner/group/DACL or write time changed".into());
    }
    let [payload, stream, long_file] = native_merge_payload(&folder, &keep, &volume, &base)?;
    let original = native_data(
        &open_sample(&root.join(KEEP_FILE), &keep, &volume)?,
        &sample(19, 65_553),
    )?;
    if table(A)? != after || table(B)? != fixture.base_b {
        return Err("GPT changed during readback".into());
    }
    record(
        log,
        json!({"event":"passed","bytesAndSha256":[payload,stream,original,long_file],"hardlinks":2,"securityAndWriteTime":true,"aMiB":384,"bGptUnchanged":true}),
    )?;
    Ok("NTFS merge passed on 4Kn A. Windows read both hard links, all four byte samples, the named stream, preserved ACL/time, and unchanged B GPT.".into())
}

pub(super) fn report() -> Result<String, String> {
    backup_protection::report(MERGE_LOG)
}

pub(super) fn diagnose() -> Result<String, String> {
    use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
    use std::collections::{BTreeMap, BTreeSet};

    let mut log = backup_protection::begin("partition-merge-4kn-diagnose")?;
    let fixture = fixture(desktop()?)?;
    if table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("Post-attempt GPT differs from the prepared pair".into());
    }
    let keep = selection_for(KEEP_LABEL, fixture.offset, KEEP_SIZE)?;
    let kept = resolved(&keep, fixture.offset, KEEP_SIZE, true)?;
    let attempt = attempt_id("execute-once")?;
    if root(&kept)?
        .join(format!("DiskStudio-Merge-{attempt}"))
        .try_exists()
        .map_err(err)?
    {
        return Err("First attempt had already entered destination creation".into());
    }
    let offset = fixture.offset + KEEP_SIZE;
    let source = selection_for(ABSORB_LABEL, offset, ABSORB_SIZE)?;
    let volume = resolved(&source, offset, ABSORB_SIZE, true)?;
    let base = open_sample(&root(&volume)?.join(ABSORB_FILE), &source, &volume)?;
    let hash = native_data(&base, &sample(31, 131_089))?;
    drop(base);
    let mut reader = data_windows::reader(&volume)?;
    let ntfs = Ntfs::new(&mut reader).map_err(err)?;
    let root_record = ntfs.root_directory(&mut reader).map_err(err)?;
    let root_id = (
        root_record.file_record_number(),
        root_record.sequence_number(),
    );
    let mft = ntfs.file(&mut reader, 0).map_err(err)?;
    let mut bitmap = None;
    for item in mft.attributes_raw() {
        let attribute = item.map_err(err)?;
        if attribute.ty().map_err(err)? == NtfsAttributeType::Bitmap && attribute.name_length() == 0
        {
            if attribute.value_length() > 512 * 1024 || bitmap.is_some() {
                return Err(format!(
                    "Diagnostic MFT bitmap exceeds its bound or is duplicated ({} bytes)",
                    attribute.value_length()
                ));
            }
            let mut bytes = vec![0; attribute.value_length() as usize];
            attribute
                .value(&mut reader)
                .map_err(err)?
                .read_exact(&mut reader, &mut bytes)
                .map_err(err)?;
            bitmap = Some(bytes);
        }
    }
    let mut records = BTreeMap::new();
    for (index, octet) in bitmap
        .ok_or("Missing diagnostic MFT bitmap")?
        .into_iter()
        .enumerate()
    {
        if octet == 0 {
            continue;
        }
        for bit in 0..8 {
            let number = (index * 8 + bit) as u64;
            if number < 16 || octet & (1 << bit) == 0 {
                continue;
            }
            if records.len() >= 512 {
                return Err("Too many active records for the dedicated merge fixture".into());
            }
            let record = ntfs.file(&mut reader, number).map_err(err)?;
            if ntfs_names::merge_base_reference(&mut reader, &record)? != 0 {
                continue;
            }
            let id = (number, record.sequence_number());
            let names = ntfs_names::read_for_merge(&mut reader, &record)?;
            records.insert(
                id,
                (
                    record.raw_flags(),
                    record.info().map_err(err)?.file_attributes().bits(),
                    names,
                ),
            );
        }
    }
    let mut reachable = BTreeSet::from([root_id]);
    loop {
        let count = reachable.len();
        for (&id, (_, _, names)) in &records {
            if names
                .iter()
                .any(|name| name.namespace != 2 && reachable.contains(&name.parent))
            {
                reachable.insert(id);
            }
        }
        if count == reachable.len() {
            break;
        }
    }
    let outside = records
        .iter()
        .filter(|(id, _)| !reachable.contains(id))
        .map(|(id, (record_flags, attributes, names))| {
            let names = names
                .iter()
                .map(|n| format!("{}→{:?}", n.display(), n.parent))
                .collect::<Vec<_>>()
                .join("|");
            format!(
                "{}:{} flags={record_flags:x} attr={attributes:x} {names}",
                id.0, id.1
            )
        })
        .collect::<Vec<_>>();
    if table(A)? != fixture.base_a || table(B)? != fixture.base_b {
        return Err("GPT changed during the isolated diagnosis".into());
    }
    record(
        &mut log,
        json!({"event":"diagnosed","gptUnchanged":true,"destinationAbsent":true,"sourceFileSha256":hash,"outside":outside}),
    )?;
    let excerpt = outside
        .iter()
        .take(20)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "GPT exact. Destination absent. Source sample intact.\n{} records outside root:\n{}",
        outside.len(),
        excerpt
    ))
}

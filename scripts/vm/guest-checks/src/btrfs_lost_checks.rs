//! Real lost-partition acceptance restricted to fresh ranges on isolated A.
use super::*;
use crate::{lost_partitions, surface_scan::Range};

const START: u64 = 14 * GIB;
const SECOND: u64 = START + 512 * MIB;
const SEARCH: Range = Range {
    offset: START,
    size: 768 * MIB,
    sector: 4096,
};
const MEDIA: [&str; 2] = ["Btrfs-Lost-Primary.raw", "Btrfs-Lost-Mirror.raw"];
const OUTPUT: &str = "DSBTRFS-Lost-Recovery";
const GROUP_START: u64 = 12 * GIB;
const GROUP_SECOND: u64 = GROUP_START + 512 * MIB;
const GROUP_SEARCH: Range = Range {
    offset: GROUP_START,
    ..SEARCH
};
const GROUP_OUTPUT: &str = "DSBTRFS-Lost-Group-Recovery";
const GROUP_MEDIA: [&str; 2] = ["Btrfs-Lost-Member-1.raw", "Btrfs-Lost-Member-2.raw"];
const GROUP_FILES: [(&str, u64, &str); 1] = [(
    "striped.bin",
    655360,
    "a7b17ca58cd2d07524ab80dfa5cc599431dfbb6f34a4dea62756bc84f3af9734",
)];
const FILES: [(&str, u64, &str); 2] = [
    (
        "checksum-inline.txt",
        34,
        "ecd1c486cbdd94dc81da14877aef40b5b1974c7f17cd90cc40cdb9827513674b",
    ),
    (
        "checksum-compressed.bin",
        249257,
        "ae8bbe2a5fea29843fb622e18f84e4470049d85762560aaf8ab74a4f1c61d6c2",
    ),
];

#[derive(Serialize, Deserialize)]
struct LostWitness {
    initial_a: table_archive::Archive,
    b_table: String,
    os_layout: String,
    destination: PathBuf,
}

fn digests() -> Result<[&'static str; 2], String> {
    option_env!("DISKSTUDIO_BTRFS_LOST_SHA256")
        .ok_or("Missing Btrfs lost image digests")?
        .split(',')
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "Expected two Btrfs lost images".into())
}

fn verify_member(mut reader: impl Read + Seek, primary: bool) -> Result<(), String> {
    let mut volume = disk_core::btrfs::open_recovered(&mut reader, || Ok(()))?;
    if volume.has_current_primary()? != primary
        || volume.member_size() != HISTORY_SIZE
        || volume.missing_members() != 0
    {
        return Err("Unexpected Btrfs fixture recovery geometry".into());
    }
    checksum_volume(volume)
}

fn scan_candidates(
    reader: impl Read + Seek + 'static,
    local: Range,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<lost_partitions::Candidate>, String> {
    let mut found = Vec::new();
    lost_partitions::scan(reader, local, cancel, |_, item| {
        found.extend(item);
        Ok(())
    })?;
    if found.len() != 2 {
        return Err(format!(
            "Expected two independently checked Btrfs candidates, found {}",
            found.len()
        ));
    }
    Ok(found)
}

fn check_candidates(file: &mut File) -> Result<(), String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let read_window = || {
        data_io::Region::new(file.try_clone().map_err(err)?, SEARCH.offset, SEARCH.size).map_err(err)
    };
    let local = Range {
        offset: 0,
        ..SEARCH
    };
    let found = scan_candidates(read_window()?, local, &cancel)?;
    for (offset, primary) in [(0, true), (SECOND - START, false)] {
        let candidate = found
            .iter()
            .find(|candidate| {
                candidate.filesystem == "Btrfs"
                    && candidate.offset == offset
                    && candidate.size == HISTORY_SIZE
                    && candidate.can_restore == primary
            })
            .ok_or("Btrfs candidate bounds or write eligibility differ")?;
        let member = lost_partitions::candidate_reader(
            read_window()?,
            local.size,
            local.sector,
            local,
            candidate,
            &cancel,
        )?;
        verify_member(member, primary)?;
    }
    Ok(())
}

fn verified_table(
    file: &mut File,
    context: &partitions::platform::TableReadContext,
) -> Result<table_archive::Archive, String> {
    let table = table_archive::capture(file, 16 * GIB, 4096, "a".repeat(64), false, || Ok(()))?;
    let parts = context.parts.iter().collect::<Vec<_>>();
    if !table_restore::archive_layout(&table)?
        .agrees(&extfs::restore_native(file, Some(&parts), 4096)?.0)
    {
        return Err("A native geometry differs from the verified GPT".into());
    }
    Ok(table)
}

fn prepare_fixture(
    kind: &str,
    search: Range,
    offsets: [u64; 2],
    output: &str,
    expected: [&str; 2],
    mut payload: Vec<File>,
    check: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let sha = desktop()?;
    let report = history_report(kind, &sha);
    if report.exists() {
        return Err("A Btrfs lost-partition preparation receipt already exists".into());
    }
    let mut log = backup_protection::begin(&format!("btrfs-4kn-{kind}-prepare"))?;
    online(B)?;
    let destination = history_destination(output)?;
    if destination.try_exists().map_err(err)? {
        return Err("The B recovery folder already exists".into());
    }
    let [_, b_table] = history_tables()?;
    let os_layout = reallocation_checks::os_layout(&inventory()?)?;
    let _guard = OPERATION.try_lock().map_err(err)?;
    let source = selection(A, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    let locks = data_windows::lock(&ctx.resolved)?;
    let (fresh, _, _) = partitions::platform::table_restore_context(&source, Some((&ctx, &locks)))?;
    if fresh.fingerprint != ctx.fingerprint {
        return Err("A identity changed while locking".into());
    }
    let parts = fresh.parts.iter().collect::<Vec<_>>();
    data_windows::verify_locks(&locks, &fresh.resolved, &parts)?;
    let mut file = data_windows::exclusive_device(&fresh.resolved)?;
    let before = verified_table(&mut file, &fresh)?;
    let layout = table_restore::archive_layout(&before)?;
    if search.offset / 4096 < layout.first
        || (search.offset + search.size) / 4096 > layout.last + 1
        || offsets.iter().any(|&offset| {
            offset < search.offset || offset + HISTORY_SIZE > search.offset + search.size
        })
        || layout.entries.iter().any(|entry| {
            entry.offset < search.offset + search.size && search.offset < entry.offset + entry.size
        })
    {
        return Err("The fixed A fixture scope is not unallocated".into());
    }
    data_windows::offline(&file)?;
    if verified_table(&mut file, &fresh)? != before {
        return Err("A GPT changed before seeding".into());
    }
    let mut block = vec![0; MIB as usize];
    file.seek(SeekFrom::Start(search.offset)).map_err(err)?;
    for _ in 0..search.size / MIB {
        file.read_exact(&mut block).map_err(err)?;
        if block.iter().any(|&byte| byte != 0) {
            return Err("A fixture scope contains existing bytes; no fixture was written".into());
        }
    }
    for ((offset, image), digest) in offsets.into_iter().zip(&mut payload).zip(expected) {
        let mut region = data_io::Region::new(file.try_clone().map_err(err)?, offset, HISTORY_SIZE)
            .map_err(err)?;
        image.rewind().map_err(err)?;
        for _ in 0..HISTORY_SIZE / MIB {
            image.read_exact(&mut block).map_err(err)?;
            region.write_all(&block).map_err(err)?;
        }
        region.flush().map_err(err)?;
        region.rewind().map_err(err)?;
        if data_io::hash_exact(&mut region, HISTORY_SIZE, |_| Ok(()))? != digest {
            return Err("Btrfs fixture SHA-256 readback differs".into());
        }
    }
    if verified_table(&mut file, &fresh)? != before {
        return Err("A GPT changed while writing new fixture bytes".into());
    }
    check(&mut file)?;
    drop(file);
    drop(locks);
    if test_a()?.is_offline != Some(true)
        || reallocation_checks::os_layout(&inventory()?)? != os_layout
    {
        return Err("A must be offline and guest OS layout must match".into());
    }
    fs::create_dir(&destination).map_err(err)?;
    let mut saved = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(
        &mut saved,
        &LostWitness {
            initial_a: before,
            b_table,
            os_layout,
            destination: destination.clone(),
        },
    )
    .map_err(err)?;
    saved.sync_all().map_err(err)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass","fixtureSha256":expected}),
    )?;
    Ok(destination)
}

pub(crate) fn prepare() -> Result<String, String> {
    let executable = std::env::current_exe().map_err(err)?;
    let expected = digests()?;
    let mut payload = Vec::new();
    for ((name, digest), primary) in MEDIA.into_iter().zip(expected).zip([true, false]) {
        let path = executable.with_file_name(name);
        let mut image = File::open(&path).map_err(err)?;
        if image.metadata().map_err(err)?.len() != HISTORY_SIZE
            || allocation_batch::hash_file(&path)? != digest
        {
            return Err("Unexpected Btrfs lost fixture media".into());
        }
        verify_member(&mut image, primary)?;
        payload.push(image);
    }
    let destination = prepare_fixture(
        "lost",
        SEARCH,
        [START, SECOND],
        OUTPUT,
        expected,
        payload,
        check_candidates,
    )?;
    Ok(format!(
        "PASS. A is offline. Search custom start LBA {} and count {}. Primary candidate LBA {} permits GPT restore. Mirror-only candidate LBA {} permits browsing and extracting both files to {} with folders preserved. Restore only the primary candidate after export.",
        START / 4096, SEARCH.size / 4096, START / 4096, SECOND / 4096, destination.display(),
    ))
}

pub(crate) fn inspect() -> Result<String, String> {
    let sha = desktop()?;
    let saved: LostWitness =
        serde_json::from_reader(File::open(history_report("lost", &sha)).map_err(err)?)
            .map_err(err)?;
    if history_tables()?[1] != saved.b_table
        || reallocation_checks::os_layout(&inventory()?)? != saved.os_layout
        || test_a()?.is_offline != Some(true)
        || history_destination(OUTPUT)? != saved.destination
    {
        return Err(
            "The protected layouts, destination binding, or A offline state changed".into(),
        );
    }
    let source = selection(A, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    let mut file = data_windows::device(&ctx.resolved, false)?;
    let table = verified_table(&mut file, &ctx)?;
    let before = table_restore::archive_layout(&saved.initial_a)?;
    let after = table_restore::archive_layout(&table)?;
    if after.guid != before.guid
        || after.count != before.count
        || after.first != before.first
        || after.last != before.last
        || after.entries.len() != before.entries.len() + 1
        || before
            .entries
            .iter()
            .any(|entry| !after.entries.contains(entry))
        || !after.entries.iter().any(|entry| {
            entry.offset == START
                && entry.size == HISTORY_SIZE
                && entry.attributes == 0
                && entry.kind == partitions::policy::LINUX_DATA
        })
        || table
            .extents
            .iter()
            .find(|e| e.kind == table_archive::Kind::Mbr)
            != saved
                .initial_a
                .extents
                .iter()
                .find(|e| e.kind == table_archive::Kind::Mbr)
    {
        return Err("GPT result must add only the bounded primary Linux data entry".into());
    }
    for (offset, digest) in [START, SECOND].into_iter().zip(digests()?) {
        let mut reader = data_io::Region::new(&mut file, offset, HISTORY_SIZE).map_err(err)?;
        if data_io::hash_exact(&mut reader, HISTORY_SIZE, |_| Ok(()))? != digest {
            return Err("A recovered source filesystem bytes changed".into());
        }
    }
    verify_recovered(&saved.destination, &FILES)?;
    Ok("PASS. Exactly one Linux data GPT entry was registered and its range matches the primary. Both full source images match, A stayed offline, prior GPT entries and B/OS layouts match, and both mirror-only files exported to B match Linux SHA-256.".into())
}

fn group_readback(file: &mut File) -> Result<(), String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let local = Range {
        offset: 0,
        ..GROUP_SEARCH
    };
    let found = scan_candidates(
        data_io::Region::new(file.try_clone().map_err(err)?, GROUP_SEARCH.offset, local.size)
            .map_err(err)?,
        local,
        &cancel,
    )?;
    let mut readers = Vec::new();
    for (candidate, offset) in found.iter().zip([GROUP_START, GROUP_SECOND]) {
        if candidate.filesystem != "Btrfs"
            || candidate.offset != offset - GROUP_START
            || candidate.size != HISTORY_SIZE
            || candidate.btrfs_member_count != Some(2)
        {
            return Err("Unexpected Btrfs group candidate geometry".into());
        }
        let reader = lost_partitions::candidate_reader(
            data_io::Region::new(file.try_clone().map_err(err)?, GROUP_START, local.size)
                .map_err(err)?,
            local.size,
            local.sector,
            local,
            candidate,
            &cancel,
        )?;
        readers.push(reader);
    }
    let volume = disk_core::btrfs::open_recovered_members(readers, false, || Ok(()))?;
    let mut candidates = Vec::new();
    btrfs_recovery::scan_volume(
        volume,
        recovery::ScanMode::AllFiles,
        &cancel,
        |_, _| Ok(()),
        |file| candidates.push(file),
        |_| Ok(()),
    )?;
    if !candidates.iter().any(|file| {
        file.name == GROUP_FILES[0].0 && file.size == GROUP_FILES[0].1 && file.supported
    }) {
        return Err("The joint directory does not contain the verified striped file".into());
    }
    Ok(())
}

pub(crate) fn prepare_group() -> Result<String, String> {
    let (mut payload, expected) = member_images(GROUP_MEDIA)?;
    member_contents(&mut payload)?;
    let destination = prepare_fixture(
        "lost-group",
        GROUP_SEARCH,
        [GROUP_START, GROUP_SECOND],
        GROUP_OUTPUT,
        expected,
        payload,
        group_readback,
    )?;
    Ok(format!(
        "PASS. A is offline. Search A twice, starting at LBA {} and {}, each with {} sectors. Join these two Btrfs members from the retained searches. Preview and export striped.bin to {} with its folders.",
        GROUP_START / 4096, GROUP_SECOND / 4096, HISTORY_SIZE / 4096, destination.display(),
    ))
}

pub(crate) fn inspect_group() -> Result<String, String> {
    let sha = desktop()?;
    let saved: LostWitness =
        serde_json::from_reader(File::open(history_report("lost-group", &sha)).map_err(err)?)
            .map_err(err)?;
    if history_tables()?[1] != saved.b_table
        || reallocation_checks::os_layout(&inventory()?)? != saved.os_layout
        || test_a()?.is_offline != Some(true)
        || history_destination(GROUP_OUTPUT)? != saved.destination
    {
        return Err(
            "The protected layouts, destination binding, or A offline state changed".into(),
        );
    }
    let (_, expected) = member_images(GROUP_MEDIA)?;
    let source = selection(A, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    let mut file = data_windows::device(&ctx.resolved, false)?;
    if verified_table(&mut file, &ctx)? != saved.initial_a {
        return Err("A GPT changed during joint recovery".into());
    }
    for (offset, digest) in [GROUP_START, GROUP_SECOND].into_iter().zip(expected) {
        let mut reader = data_io::Region::new(&mut file, offset, HISTORY_SIZE).map_err(err)?;
        if data_io::hash_exact(&mut reader, HISTORY_SIZE, |_| Ok(()))? != digest {
            return Err("A Btrfs member bytes changed".into());
        }
    }
    verify_recovered(&saved.destination, &GROUP_FILES)?;
    Ok("PASS. The striped file exported to B matches the Linux SHA-256 and retained its directory. A stayed offline. Both source member images, both GPTs and the guest OS layout match the baseline.".into())
}

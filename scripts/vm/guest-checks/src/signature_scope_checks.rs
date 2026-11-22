//! Native 4Kn signature scan, progress and export fixture.
use super::*;
use capacity_restore_checks::{b_device, hash, region};
use disk_core::{fat_format, MIB};
use partitions::{policy, Action, FileSystem, PartitionKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Cursor, Read, Write};

const SECTOR: u64 = 4096;
const START: u64 = 32 * MIB;
const SIZE: u64 = 8 * MIB;
const EXT_JOURNAL_SIZE: u64 = 32 * MIB;
const EXT_JOURNAL_PROFILE: &str = "ext-journal-4kn";
const EXT_JOURNAL_FOLDER: &str = "DS-ExtJournal";
const EXT_JOURNAL_TREE_PROFILE: &str = "ext-journal-tree-4kn";
const EXT_JOURNAL_TREE_FOLDER: &str = "DS-ExtJournalTree";
const FOLDER: &str = "DS-SignatureScope";
const FREE_PDF: &[u8] = b"%PDF-1.4\nDiskStudio aligned free fixture\n%%EOF";
const LIVE_PDF: &[u8] = b"%PDF-1.4\nDiskStudio allocated control\n%%EOF";
const UNALIGNED: &[u8] = b"%PDF-1.4\nDiskStudio unaligned free control\n%%EOF";
const SEVEN_ZIP: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample.7z");
const SQLITE: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample.sqlite");
const MP4: &[u8] = include_bytes!("../../../../tests/fixtures/signature-bmff-faststart.mp4");
const MOV: &[u8] = include_bytes!("../../../../tests/fixtures/signature-bmff-tail.mov");
const OGG_MUX: &[u8] = include_bytes!("../../../../tests/fixtures/signature-ogg-multiplex.ogg");
const OGG_OPUS: &[u8] = include_bytes!("../../../../tests/fixtures/signature-ogg-opus.ogg");
const TIFF_PAGES: &[u8] = include_bytes!("../../../../tests/fixtures/signature-tiff-pages.tif");
const BIG_TIFF: &[u8] = include_bytes!("../../../../tests/fixtures/signature-bigtiff-tiles.tif");
const ICON: &[u8] = include_bytes!("../../../../tests/fixtures/signature-icon-png.ico");
const CURSOR: &[u8] = include_bytes!("../../../../tests/fixtures/signature-cursor-dib.cur");
const RAR4: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample-rar4.rar");
const RAR5: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample-rar5.rar");
const AVI: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample.avi");
const AIFF: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample.aiff");
const AIFC: &[u8] = include_bytes!("../../../../tests/fixtures/signature-sample.aifc");
const FLAC: &[u8] = include_bytes!("../../../../tests/fixtures/signature-audio.flac");
const MIDI: &[u8] = include_bytes!("../../../../tests/fixtures/signature-audio.mid");
const DSF: &[u8] = include_bytes!("../../../../tests/fixtures/signature-audio.dsf");
const DFF: &[u8] = include_bytes!("../../../../tests/fixtures/signature-audio.dff");
const DOCM: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.docm");
const XLSM: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.xlsm");
const PPTM: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.pptm");
const ODT: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.odt");
const ODS: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.ods");
const ODP: &[u8] = include_bytes!("../../../../tests/fixtures/signature-document.odp");

#[derive(Clone, Copy)]
pub(super) enum Sample {
    Pdf,
    Formats,
    Media,
    Images,
    Containers,
    Audio,
    Documents,
}

impl Sample {
    fn folder(self) -> &'static str {
        match self {
            Self::Pdf => FOLDER,
            Self::Formats => "DS-SignatureFormats",
            Self::Media => "DS-SignatureMedia",
            Self::Images => "DS-SignatureImages",
            Self::Containers => "DS-SignatureContainers",
            Self::Audio => "DS-SignatureAudio",
            Self::Documents => "DS-SignatureDocuments",
        }
    }

    fn profile(self) -> &'static str {
        match self {
            Self::Pdf => "signature-scope-4kn",
            Self::Formats => "signature-formats-4kn",
            Self::Media => "signature-media-4kn",
            Self::Images => "signature-images-4kn",
            Self::Containers => "signature-containers-4kn",
            Self::Audio => "signature-audio-4kn",
            Self::Documents => "signature-documents-4kn",
        }
    }

    fn expected(self, offset: u64) -> Vec<(u64, &'static str, &'static [u8])> {
        match self {
            Self::Pdf => vec![(offset, "pdf", FREE_PDF)],
            Self::Formats => vec![
                (offset + 2 * SECTOR, "7z", SEVEN_ZIP),
                (offset + 4 * SECTOR, "sqlite", SQLITE),
            ],
            Self::Media => vec![
                (offset + 2 * SECTOR, "mp4", MP4),
                (offset + 6 * SECTOR, "mov", MOV),
                (offset + 8 * SECTOR, "ogg", OGG_MUX),
                (offset + 12 * SECTOR, "ogg", OGG_OPUS),
            ],
            Self::Images => vec![
                (offset + 2 * SECTOR, "tif", TIFF_PAGES),
                (offset + 4 * SECTOR, "tif", BIG_TIFF),
                (offset + 6 * SECTOR, "ico", ICON),
                (offset + 8 * SECTOR, "cur", CURSOR),
            ],
            Self::Containers => vec![
                (offset + 2 * SECTOR, "rar", RAR4),
                (offset + 4 * SECTOR, "rar", RAR5),
                (offset + 6 * SECTOR, "avi", AVI),
                (offset + 10 * SECTOR, "aiff", AIFF),
                (offset + 12 * SECTOR, "aifc", AIFC),
            ],
            Self::Audio => vec![
                (offset + 2 * SECTOR, "flac", FLAC),
                (offset + 8 * SECTOR, "mid", MIDI),
                (offset + 10 * SECTOR, "dsf", DSF),
                (offset + 14 * SECTOR, "dff", DFF),
            ],
            Self::Documents => vec![
                (offset + 2 * SECTOR, "docm", DOCM),
                (offset + 4 * SECTOR, "xlsm", XLSM),
                (offset + 6 * SECTOR, "pptm", PPTM),
                (offset + 8 * SECTOR, "odt", ODT),
                (offset + 10 * SECTOR, "ods", ODS),
                (offset + 12 * SECTOR, "odp", ODP),
            ],
        }
    }

    fn kinds(self) -> &'static [signature_scan::Kind] {
        use signature_scan::Kind;
        match self {
            Self::Pdf => &[Kind::Pdf],
            Self::Formats => &[Kind::SevenZip, Kind::Sqlite],
            Self::Media => &[Kind::Mp4, Kind::Mov, Kind::Ogg],
            Self::Images => &[Kind::Tiff, Kind::Ico, Kind::Cur],
            Self::Containers => &[Kind::Rar, Kind::Avi, Kind::Aiff, Kind::Aifc],
            Self::Audio => &[Kind::Flac, Kind::Midi, Kind::Dsf, Kind::Dff],
            Self::Documents => &[Kind::Docm, Kind::Xlsm, Kind::Pptm, Kind::Odt, Kind::Ods, Kind::Odp],
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Baseline {
    tables: [String; 3],
    edges: [String; 2],
    partition: u32,
}

fn receipt(desktop: &str, profile: &str) -> PathBuf {
    let profile = profile.trim_end_matches("-4kn");
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-{profile}-{desktop}.json"
    ))
}

fn fixture(sample: Sample) -> Result<(Vec<u8>, u64), String> {
    let format = fat_format::Plan::new(
        fat_format::FatType::Fat12,
        SIZE,
        SECTOR as u32,
        SECTOR as u32,
        "SIGSCOPE",
        0x47979328,
        (START / SECTOR) as u32,
    )
    .map_err(err)?;
    let mut image = Cursor::new(vec![0; SIZE as usize]);
    format.format(&mut image, |_, _| Ok(())).map_err(err)?;
    let mut bytes = image.into_inner();
    let word = |i| u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
    let sector = SECTOR as usize;
    let reserved = word(14);
    let fat = word(22);
    let root = reserved + 2 * fat;
    let heap = (root + (word(17) * 32).div_ceil(sector)) * sector;
    for copy in [reserved, reserved + fat] {
        bytes[copy * sector + 3..copy * sector + 5].copy_from_slice(&[0xff, 0x0f]);
    }
    let entry = &mut bytes[root * sector + 32..root * sector + 64];
    entry[..11].copy_from_slice(b"LIVE    PDF");
    entry[11] = 0x20;
    entry[26..28].copy_from_slice(&2u16.to_le_bytes());
    entry[28..32].copy_from_slice(&(LIVE_PDF.len() as u32).to_le_bytes());
    let aligned = heap + 2 * sector;
    for (offset, pdf) in [
        (heap, LIVE_PDF),
        (heap + sector + 37, UNALIGNED),
        (aligned, FREE_PDF),
    ] {
        bytes[offset..][..pdf.len()].copy_from_slice(pdf);
    }
    if !matches!(sample, Sample::Pdf) {
        for (offset, _, contents) in sample.expected(aligned as u64) {
            let start = usize::try_from(offset).map_err(err)?;
            let end = start
                .checked_add(contents.len())
                .ok_or("Test sample range overflow")?;
            bytes
                .get_mut(start..end)
                .ok_or("Test sample falls outside its fixture")?
                .copy_from_slice(contents);
        }
    }
    Ok((bytes, aligned as u64))
}

fn edges(file: &File, size: u64) -> Result<[String; 2], String> {
    Ok([
        hash(file, START - SECTOR, SECTOR)?,
        hash(file, START + size, SECTOR)?,
    ])
}

fn part(size: u64) -> Result<u32, String> {
    let table = fat_resize_checks::model(B)?;
    if table.entries.len() != 2
        || !table.entries.iter().any(|entry| {
            entry.offset == START
                && entry.size == size
                && entry.kind == policy::LINUX_DATA
                && entry.attributes == 0
        })
    {
        return Err("Expected the single exact unprotected signature fixture on B".into());
    }
    Ok(reallocation_checks::metadata_partition(B, None, START / MIB, size / MIB)?.number)
}

fn write_fixture(bytes: &[u8], partition: u32) -> Result<[String; 2], String> {
    use crate::{partitions::platform::maintenance, surface_scan::Range};
    let size = bytes.len() as u64;
    let selected = selection(B, None)?;
    let before = maintenance::write_context(&selected, None)?;
    let locks = data_windows::lock(&before.resolved)?;
    let fresh = maintenance::write_context(&selected, Some((&before, &locks)))?;
    let parts = fresh.parts.iter().collect::<Vec<_>>();
    if fresh
        .resolved
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean)
        != Some(B)
        || fresh.resolved.size != 16 * GIB
        || fresh.resolved.disk.logical_sector_size != Some(SECTOR)
        || fresh.has_volume(partition)
        || !parts.iter().any(|p| {
            p.partition_number == partition
                && p.offset == START
                && p.size == size
                && policy::linux_clone_candidate(&fresh.resolved.disk, &parts, p)
        })
        || ![SIZE, EXT_JOURNAL_SIZE].contains(&size)
    {
        return Err("Bound B signature fixture or protection changed".into());
    }
    let mut file = data_windows::exclusive_device(&fresh.resolved)?;
    let range = Range {
        offset: START,
        size,
        sector: SECTOR,
    };
    let capture = |file: &mut File| {
        maintenance::capture_write_table(&fresh, file, &selected.target, range, true)
    };
    let table = capture(&mut file)?.ok_or("Missing B GPT")?;
    data_windows::persist_offline(&file)?;
    data_windows::confirm_writable_offline(&file)?;
    let surrounding = edges(&file, size)?;
    if capture(&mut file)? != Some(table.clone())
        || hash(&file, START, size)? != format!("{:x}", Sha256::digest(vec![0; size as usize]))
    {
        return Err("Expected zero fixture extent and unchanged B GPT before write".into());
    }
    let mut output =
        data_io::Region::new(file.try_clone().map_err(err)?, START, size).map_err(err)?;
    output.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    if hash(&file, START, size)? != format!("{:x}", Sha256::digest(bytes))
        || capture(&mut file)? != Some(table)
        || edges(&file, size)? != surrounding
    {
        return Err("B fixture readback, GPT or boundary differs".into());
    }
    data_windows::confirm_writable_offline(&file)?;
    Ok(surrounding)
}

fn prepare_source(bytes: &[u8], profile: &str, folder_name: &str) -> Result<(u32, PathBuf), String> {
    let size = bytes.len() as u64;
    if ![SIZE, EXT_JOURNAL_SIZE].contains(&size) {
        return Err("Unsupported fixture size".into());
    }
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    for serial in [A, B] {
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let path = receipt(&desktop, profile);
    if path.try_exists().map_err(err)? {
        return Err("Signature fixture already prepared".into());
    }
    let mut log = backup_protection::begin(profile)?;
    online(A)?;
    operation(A, Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (FileSystem::Ntfs, "DSSIGSCOPE", 4096),
        START,
        2 * GIB,
    )?;
    let (root, folder) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(folder_name).map_err(err)?;
    online(B)?;
    operation(B, Action::Initialize {})?;
    operation(
        B,
        Action::Create {
            offset: START,
            size,
            kind: PartitionKind::Linux,
        },
    )?;
    let partition = part(size)?;
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    if tables[2] != os {
        return Err("Guest OS layout changed".into());
    }
    let edges = write_fixture(bytes, partition)?;
    let saved = Baseline {
        tables,
        edges,
        partition,
    };
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    serde_json::to_writer(&file, &saved).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok((partition, folder.join(folder_name)))
}

pub(super) fn prepare(sample: Sample) -> Result<String, String> {
    let (bytes, aligned) = fixture(sample)?;
    let (partition, folder) = prepare_source(&bytes, sample.profile(), sample.folder())?;
    let scan = match sample {
        Sample::Pdf => format!("Scan PDF only. Expect 3 full, 2 aligned, 1 free+aligned. Free PDF byte offset {aligned:#x}."),
        Sample::Formats => "Scan 7z and SQLite only with both free clusters and sector alignment enabled. Expect one candidate of each type.".into(),
        Sample::Media => "Scan MP4, MOV and Ogg with free clusters and sector alignment enabled. Expect one MP4, one MOV and two Ogg files.".into(),
        Sample::Images => "Scan TIFF, ICO and CUR with free clusters and sector alignment enabled. Expect two TIFF files, one icon and one cursor.".into(),
        Sample::Containers => "Scan RAR, AVI, AIFF and AIFF-C with free clusters and sector alignment enabled. Expect two RAR files and one AVI, one AIFF and one AIFC file.".into(),
        Sample::Audio => "Scan FLAC, MIDI, DSF and DFF with free clusters and sector alignment enabled. Expect one candidate of each type.".into(),
        Sample::Documents => "Scan DOCM, XLSM, PPTM, ODT, ODS and ODP with free clusters and sector alignment enabled. Expect one candidate of each type.".into(),
    };
    Ok(format!(
        "PASS: B/{partition} 8 MiB is offline. {scan} Save Scope.dsrecover and export to {}.",
        folder.display()
    ))
}

pub(super) fn verify(sample: Sample) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(&desktop, sample.profile()))
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let (bytes, aligned) = fixture(sample)?;
    let b = b_device(true)?;
    if part(SIZE)? != baseline.partition
        || ntfs_reconstruction_checks::table_hashes()? != baseline.tables
        || edges(&b, SIZE)? != baseline.edges
        || hash(&b, START, SIZE)? != format!("{:x}", Sha256::digest(&bytes))
    {
        return Err("Source bytes, neighbors, GPTs, or OS layout changed".into());
    }
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(sample.folder()).map_err(err)?;
    let mut progress = Vec::new();
    folder
        .open("Scope.dsrecover")
        .map_err(err)?
        .take(1024 * 1024)
        .read_to_end(&mut progress)
        .map_err(err)?;
    let sel = selection(B, Some(baseline.partition))?;
    let progress = recovery_checkpoint::decode(&progress, &sel)?;
    let recovery_checkpoint::Cursor::Signatures { options, cursor } = &progress.cursor else {
        return Err("Expected native signature progress".into());
    };
    let mut reader = region(&b, START, SIZE)?;
    let plan = used_space::inspect(&mut reader, SIZE, SECTOR, None, &AtomicBool::new(false))?;
    cursor.verify_scope(options, &plan)?;
    let (found, _) = progress.validate(&sel)?;
    signature_scan::verify_free_candidates(&plan, &found)?;
    let expected = sample.expected(aligned);
    if !options.free_only
        || !options.sector_aligned
        || options.kinds.len() != sample.kinds().len()
        || sample
            .kinds()
            .iter()
            .any(|kind| !options.kinds.contains(kind))
        || !cursor.sealed
        || found.len() != expected.len()
        || found
            .iter()
            .zip(&expected)
            .any(|(c, (offset, ext, bytes))| {
                c.name != format!("recovered_{offset:016x}.{ext}") || c.size != bytes.len() as u64
            })
    {
        return Err("Native progress did not retain the exact aligned free candidates".into());
    }
    let mut exports = 0;
    for entry in folder.entries().map_err(err)? {
        let entry = entry.map_err(err)?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("DiskStudio-recovered-")
        {
            continue;
        }
        let output = folder.open_dir(entry.file_name()).map_err(err)?;
        let report: serde_json::Value = serde_json::from_reader(
            output
                .open(recovery_export::REPORT)
                .map_err(err)?
                .take(128 * 1024),
        )
        .map_err(err)?;
        if report["completed"] != true
            || report["savedFiles"] != expected.len()
            || report["failedFiles"] != 0
            || report["layout"] != "flat"
        {
            return Err("Unexpected native export completion or layout".into());
        }
        for (index, (candidate, (_, _, bytes))) in found.iter().zip(&expected).enumerate() {
            let name = &candidate.name;
            let leaf = format!("{:06}_{name}", candidate.id);
            if report["entries"][index]["originalName"] != *name
                || report["entries"][index]["name"] != leaf
                || report["entries"][index]["folder"] != 0
                || output.read(&leaf).map_err(err)? != *bytes
                || report["outcomes"][index]["sha256"] != format!("{:x}", Sha256::digest(bytes))
            {
                return Err(format!(
                    "Native export or verified SHA-256 differs for {name}"
                ));
            }
        }
        exports += 1;
    }
    if exports != 1 {
        return Err("Expected exactly one native export".into());
    }
    Ok(format!("PASS: Saved settings, {} candidates, original B extent, both GPTs, adjacent sectors, guest OS layout and exported SHA-256 verified. B remains offline.", expected.len()))
}

struct ExtJournalFixture {
    profile: &'static str,
    folder: &'static str,
    compressed: &'static [u8],
    sha256: &'static str,
    expected: &'static [(&'static str, usize, &'static str)],
    first_file_id: usize,
}

const EXT_JOURNAL_DIGESTS: [(&str, usize, &str); 2] = [
    ("removed.txt", 39, "e3db9e1888e849f3d5b3d5347ecbb7b2ce0bd2dd93f7f73dd117d2d93c53b48e"),
    ("fragment.bin", 15668, "e66791aecd54ff8a0d6cafdab9cf46d98c22a965211b615831c8065806ee2069"),
];
const EXT_TREE_FILE: (&str, usize, &str) = (
    "extent-tree.bin", 12_583_631, "a9f32b65f4b5eb0ffe4fcef16546926f76b7cc384e667e09f5846708102ab497",
);
const EXT_COMPLETE_PDF: &[u8] = include_bytes!("../../../../tests/fixtures/ext-complete-sample.pdf");

fn ext_journal_fixture(tree: bool) -> ExtJournalFixture {
    if tree {
        ExtJournalFixture {
            profile: EXT_JOURNAL_TREE_PROFILE,
            folder: EXT_JOURNAL_TREE_FOLDER,
            compressed: include_bytes!("../../../../tests/fixtures/ext-journal-tree-linux.raw.gz"),
            sha256: "8592d8add23c97ef7954970accff024e7cb61c14682a2872b282d8179a26d599",
            expected: &[
                EXT_JOURNAL_DIGESTS[0], EXT_JOURNAL_DIGESTS[1],
                EXT_TREE_FILE,
            ],
            first_file_id: 1,
        }
    } else {
        ExtJournalFixture {
            profile: EXT_JOURNAL_PROFILE,
            folder: EXT_JOURNAL_FOLDER,
            compressed: include_bytes!("../../../../tests/fixtures/ext-journal-linux.raw.gz"),
            sha256: "fd44edb0ba66cad04f8e442b568755c5fffc75e210aea796af66e7b73291fe5d",
            expected: &EXT_JOURNAL_DIGESTS,
            first_file_id: 1,
        }
    }
}

fn decompress_ext_journal(fixture: &ExtJournalFixture) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(fixture.compressed)
        .take(EXT_JOURNAL_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() as u64 != EXT_JOURNAL_SIZE
        || format!("{:x}", Sha256::digest(&bytes)) != fixture.sha256
    {
        return Err("Ext4 Linux journal fixture does not match the independently verified image".into());
    }
    Ok(bytes)
}

pub(super) fn prepare_ext_journal(tree: bool) -> Result<String, String> {
    let fixture = ext_journal_fixture(tree);
    let bytes = decompress_ext_journal(&fixture)?;
    let (partition, output) = prepare_source(&bytes, fixture.profile, fixture.folder)?;
    Ok(format!(
        "PASS: B/{partition} 32 MiB Ext4 is offline. Expect {} deleted files. Preview removed.txt and export all matches to {} with Save in one folder.",
        fixture.expected.len(), output.display()
    ))
}

fn ext_complete_fixture() -> ExtJournalFixture {
    ExtJournalFixture {
        profile: "ext-complete-4kn",
        folder: "DS-ExtComplete",
        sha256: "2e86ff75b0a7fcb3fb7d292d8043a710be39ab522d1cd973fc05141f7beecd80",
        expected: &[
            ("retained.txt", 43, "0c53dd40af040f36d4bc138dfda8efbe1cef4f5311bfc96bf41bba8e9b77866c"),
            EXT_JOURNAL_DIGESTS[0],
            EXT_JOURNAL_DIGESTS[1],
            EXT_TREE_FILE,
            ("recovered_0000000001fff000.pdf", 605, "3c996da0648ac11b752976a80dd58d20fa1ef6ea760b3150417d4111eacfffca"),
        ],
        first_file_id: 0,
        ..ext_journal_fixture(true)
    }
}

pub(super) fn prepare_ext_complete() -> Result<String, String> {
    let fixture = ext_complete_fixture();
    let mut bytes = decompress_ext_journal(&ext_journal_fixture(true))?;
    let offset = EXT_JOURNAL_SIZE - SECTOR;
    let block = &mut bytes[offset as usize..];
    if block.iter().any(|&byte| byte != 0) || EXT_COMPLETE_PDF.len() > block.len() {
        return Err("The Ext4 signature fixture must start in a blank final block".into());
    }
    block[..EXT_COMPLETE_PDF.len()].copy_from_slice(EXT_COMPLETE_PDF);
    if format!("{:x}", Sha256::digest(&bytes)) != fixture.sha256 {
        return Err("The combined Ext4 fixture digest differs".into());
    }
    let (partition, output) = prepare_source(&bytes, fixture.profile, fixture.folder)?;
    Ok(format!(
        "PASS: B/{partition} is offline. Choose Complete Recovery, PDF only, free clusters, sector aligned. Save a paused journal checkpoint to the root of Disk A. Export five results to {} in one folder.",
        output.display()
    ))
}

pub(super) fn verify_ext_complete() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let summary = verify_ext_journal_output(ext_complete_fixture(), &desktop, false)?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let mut progress = None;
    for entry in root.entries().map_err(err)? {
        let entry = entry.map_err(err)?;
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".dsrecover") {
            continue;
        }
        if progress.is_some() {
            return Err("Expected exactly one native combined Ext4 progress file".into());
        }
        let mut bytes = Vec::new();
        root.open(&name).map_err(err)?.take(1024 * 1024).read_to_end(&mut bytes).map_err(err)?;
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(err)?;
        if envelope["version"] != 5 {
            return Err("Ext4 Complete Recovery did not save a version 5 checkpoint".into());
        }
        let selected: data_tasks::Selection =
            serde_json::from_value(envelope["progress"]["source"].clone()).map_err(err)?;
        let saved = recovery_checkpoint::decode(&bytes, &selected)?;
        let options = saved.cursor.options().ok_or("Combined signature options are missing")?;
        if selected.partition != Some(2) || saved.binding.size != EXT_JOURNAL_SIZE
            || saved.binding.offset != START || saved.binding.sector != SECTOR
            || saved.cursor.kind() != "extJournal" || saved.cursor.sealed()
            || saved.cursor.done() != 2 || saved.cursor.count() != 3
            || options.kinds != [signature_scan::Kind::Pdf] || !options.free_only || !options.sector_aligned
            || options.start != 0 || options.length.is_some()
        {
            return Err("Saved Ext4 journal position, directory count, source or signature scope differs".into());
        }
        progress = Some(());
    }
    progress.ok_or("The native combined checkpoint is missing")?;
    Ok(format!("Version 5 journal checkpoint and signature scope verified. {summary}"))
}

pub(super) fn verify_ext_journal(tree: bool) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    verify_ext_journal_output(ext_journal_fixture(tree), &desktop, false)
}

pub(super) fn verify_ext_journal_resume() -> Result<String, String> {
    // Use the retained baseline from the original preparation of this A/B
    // pair. The current optical desktop and this verifier are checked above it.
    const PREPARED_DESKTOP: &str =
        "c3bb0e4c318dba0211f21cccf6da0df2ca30416d27c48e21547c101960e9d726";
    btrfs_checks::desktop()?;
    verify_ext_journal_output(ext_journal_fixture(true), PREPARED_DESKTOP, true)
}

fn verify_ext_journal_output(fixture: ExtJournalFixture, baseline_desktop: &str, at_root: bool) -> Result<String, String> {
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(baseline_desktop, fixture.profile)).map_err(err)?.take(4096),
    ).map_err(err)?;
    let b = b_device(true)?;
    if part(EXT_JOURNAL_SIZE)? != baseline.partition
        || ntfs_reconstruction_checks::table_hashes()? != baseline.tables
        || edges(&b, EXT_JOURNAL_SIZE)? != baseline.edges
        || hash(&b, START, EXT_JOURNAL_SIZE)? != fixture.sha256
    {
        return Err("The Ext4 source, neighboring sectors or a disk layout changed".into());
    }
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = if at_root { root } else { root.open_dir(fixture.folder).map_err(err)? };
    let mut exports = 0;
    for entry in folder.entries().map_err(err)? {
        let entry = entry.map_err(err)?;
        if !entry.file_name().to_string_lossy().starts_with("DiskStudio-recovered-") {
            if at_root {
                continue;
            }
            return Err("Unexpected item in Ext4 output directory".into());
        }
        exports += 1;
        let output = folder.open_dir(entry.file_name()).map_err(err)?;
        let report: serde_json::Value = serde_json::from_reader(
            output.open(recovery_export::REPORT).map_err(err)?.take(128 * 1024),
        ).map_err(err)?;
        if report["completed"] != true || report["layout"] != "flat"
            || report["savedFiles"] != fixture.expected.len() || report["failedFiles"] != 0
        {
            return Err("Ext4 native export is incomplete".into());
        }
        for (i, &(name, count, sha256)) in fixture.expected.iter().enumerate() {
            let leaf = format!("{:06}_{name}", i + fixture.first_file_id);
            let mut bytes = Vec::with_capacity(count);
            output.open(&leaf).map_err(err)?.take(count as u64 + 1)
                .read_to_end(&mut bytes).map_err(err)?;
            if report["entries"][i]["originalName"] != name
                || report["entries"][i]["name"] != leaf
                || report["entries"][i]["folder"] != 0
                || report["outcomes"][i]["sha256"] != sha256
                || bytes.len() != count
                || format!("{:x}", Sha256::digest(&bytes)) != sha256
            {
                return Err(format!("Native export differs from the independent Linux original: {name}"));
            }
        }
    }
    if exports != 1 {
        return Err("Expected exactly one native Ext4 export".into());
    }
    Ok(format!("PASS: All {} SHA-256 values match the original fixtures. B remains offline. Complete B source extent, neighbors, both GPTs and guest OS layout match.", fixture.expected.len()))
}

const COMPLETE_FOLDER: &str = "DS-Complete-Recovery";
// The last independently verified preparation of the isolated media pair.
const MEDIA_DESKTOP: &str = "1cea3f4c51ade6dc3b473461f1f460581885924daa03fed0feb91a2985a135f2";

fn complete_fixture() -> Result<(File, u32, u64), String> {
    btrfs_checks::desktop()?;
    let sample = Sample::Media;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(MEDIA_DESKTOP, sample.profile()))
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let (bytes, aligned) = fixture(sample)?;
    let b = b_device(true)?;
    if part(SIZE)? != baseline.partition
        || ntfs_reconstruction_checks::table_hashes()? != baseline.tables
        || edges(&b, SIZE)? != baseline.edges
        || hash(&b, START, SIZE)? != format!("{:x}", Sha256::digest(&bytes))
    {
        return Err("Existing offline media fixture or its guest layout does not match".into());
    }
    Ok((b, baseline.partition, aligned))
}

pub(super) fn prepare_complete() -> Result<String, String> {
    use recovery_checkpoint::{Cursor as ScanCursor, Progress};
    let (b, partition, _) = complete_fixture()?;
    let mut reader = region(&b, START, SIZE)?;
    let mut metadata = fat_recovery::scan_progress::Checkpoint::default();
    let mut files = Vec::new();
    let mut tree = recovery_tree::Catalog::default();
    fat_recovery::scan_progress::continue_scan(
        &mut reader,
        recovery::ScanMode::AllFiles,
        &mut metadata,
        |_, _| Ok(()),
        |candidate| files.push(candidate),
        |directory| tree.insert(directory),
    )?;
    if !metadata.sealed || files.len() != 1 || !files[0].name.eq_ignore_ascii_case("LIVE.PDF") {
        return Err(
            "The complete recovery fixture should contain one recoverable file record".into(),
        );
    }
    let selection = selection(B, Some(partition))?;
    let binding = signature_checkpoint::Binding::capture(&mut reader, SIZE, START, SECTOR)?;
    let cursor = ScanCursor::Combined {
        metadata: Some(Box::new(ScanCursor::Fat {
            mode: recovery::ScanMode::AllFiles,
            cursor: metadata,
        })),
        journal: None,
        signature_phase: false,
        options: signature_scan::Options {
            kinds: Sample::Media.kinds().to_vec(),
            free_only: true,
            sector_aligned: true,
            ..Default::default()
        },
        signature_cursor: Default::default(),
    };
    let progress = Progress::capture(selection, binding, cursor, &files, &tree)?;
    let encoded = recovery_checkpoint::encode(progress)?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(COMPLETE_FOLDER).map_err(err)?;
    let folder = root.open_dir(COMPLETE_FOLDER).map_err(err)?;
    let mut file = folder
        .open_with(
            "Resume.dsrecover",
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .map_err(err)?;
    file.write_all(&encoded).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!("PASS: existing offline B/{partition} media fixture matches. On A, {} contains Resume.dsrecover at the stage boundary. Complete recovery should retain LIVE.PDF and four aligned free-cluster files.", path.join(COMPLETE_FOLDER).display()))
}

pub(super) fn verify_complete() -> Result<String, String> {
    use recovery_checkpoint::Cursor as ScanCursor;
    let (b, partition, aligned) = complete_fixture()?;
    let selected = selection(B, Some(partition))?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let folder = root.open_dir(COMPLETE_FOLDER).map_err(err)?;
    let mut bytes = Vec::new();
    folder
        .open("Complete.dsrecover")
        .map_err(err)?
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    let progress = recovery_checkpoint::decode(&bytes, &selected)?;
    let (files, catalog) = progress.validate(&selected)?;
    let ScanCursor::Combined {
        metadata: Some(metadata),
        journal: None,
        signature_phase: true,
        options,
        signature_cursor,
    } = &progress.cursor
    else {
        return Err("Expected saved complete recovery progress".into());
    };
    let mut reader = region(&b, START, SIZE)?;
    progress.binding.verify(&mut reader, SIZE, START, SECTOR)?;
    let owned = data_io::Region::new(b.try_clone().map_err(err)?, START, SIZE).map_err(err)?;
    progress.verify_owned(owned, SIZE)?;
    let plan = used_space::inspect(&mut reader, SIZE, SECTOR, None, &AtomicBool::new(false))?;
    signature_cursor.verify_scope(options, &plan)?;
    signature_scan::verify_free_candidates(&plan, &files)?;
    let signatures = Sample::Media.expected(aligned);
    if !metadata.sealed()
        || metadata.count() != 1
        || !signature_cursor.sealed
        || !options.free_only
        || !options.sector_aligned
        || options.kinds != Sample::Media.kinds()
        || files.len() != signatures.len() + 1
        || catalog.folders(None, 0)?.signature_files != signatures.len()
        || !files[0].name.eq_ignore_ascii_case("LIVE.PDF")
        || files[0].size != LIVE_PDF.len() as u64
        || files[1..]
            .iter()
            .zip(&signatures)
            .any(|(candidate, (at, ext, data))| {
                candidate.name != format!("recovered_{at:016x}.{ext}")
                    || candidate.size != data.len() as u64
            })
    {
        return Err("Saved complete recovery does not match both sets of candidates".into());
    }
    let exports = folder
        .entries()
        .map_err(err)?
        .map(|entry| entry.map(|entry| entry.file_name()).map_err(err))
        .collect::<Result<Vec<_>, _>>()?;
    let output_dirs = exports
        .iter()
        .filter(|name| name.to_string_lossy().starts_with("DiskStudio-recovered-"))
        .collect::<Vec<_>>();
    let [name] = output_dirs.as_slice() else {
        return Err("Expected one complete recovery export".into());
    };
    let output = folder.open_dir(name).map_err(err)?;
    let report: serde_json::Value = serde_json::from_reader(
        output
            .open(recovery_export::REPORT)
            .map_err(err)?
            .take(128 * 1024),
    )
    .map_err(err)?;
    if report["completed"] != true
        || report["savedFiles"] != files.len()
        || report["failedFiles"] != 0
        || report["layout"] != "flat"
    {
        return Err("Complete recovery export was incomplete or selected the wrong layout".into());
    }
    for (i, (file, data)) in files
        .iter()
        .zip(std::iter::once(LIVE_PDF).chain(signatures.iter().map(|(_, _, data)| *data)))
        .enumerate()
    {
        let leaf = format!("{:06}_{}", file.id, file.name);
        if file.id != i as u32
            || output.read(&leaf).map_err(err)? != data
            || !report["outcomes"].as_array().is_some_and(|outcomes| {
                outcomes
                    .iter()
                    .any(|outcome| outcome["sha256"] == format!("{:x}", Sha256::digest(data)))
            })
        {
            return Err(format!("Wrong bytes or SHA-256 for {}", file.name));
        }
    }
    Ok("PASS: both stages, five stable candidate IDs, five exported SHA-256 values, source bytes and boundaries, both GPTs and the guest OS layout match. B stayed offline.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_free_media_fixture_has_four_independently_verified_candidates() {
        check_aligned_fixture(Sample::Media);
    }

    #[test]
    fn aligned_free_image_fixture_has_four_independently_verified_candidates() {
        check_aligned_fixture(Sample::Images);
    }

    #[test]
    fn aligned_free_document_fixture_has_six_verified_candidates() {
        check_aligned_fixture(Sample::Documents);
    }

    fn check_aligned_fixture(sample: Sample) {
        use signature_scan::{Checkpoint, Options};

        let (bytes, aligned) = fixture(sample).unwrap();
        let mut reader = Cursor::new(&bytes);
        let plan =
            used_space::inspect(&mut reader, SIZE, SECTOR, None, &AtomicBool::new(false)).unwrap();
        let options = Options {
            kinds: sample.kinds().to_vec(),
            free_only: true,
            sector_aligned: true,
            ..Options::default()
        };
        let mut found = Vec::new();
        signature_scan::continue_scan_scoped(
            &mut reader,
            SIZE,
            &options,
            &mut Checkpoint::default(),
            SECTOR,
            Some(&plan),
            |_, _| Ok(()),
            |candidate| found.push(candidate),
        )
        .unwrap();
        let expected = sample.expected(aligned);
        assert_eq!(found.len(), expected.len());
        signature_scan::verify_free_candidates(&plan, &found).unwrap();
        for (candidate, (offset, ext, bytes)) in found.iter().zip(expected) {
            assert_eq!(candidate.name, format!("recovered_{offset:016x}.{ext}"));
            let mut output = Vec::new();
            recovery::extract(&mut reader, candidate, &mut output, SIZE, |_| Ok(())).unwrap();
            assert_eq!(output, bytes);
        }
    }
}

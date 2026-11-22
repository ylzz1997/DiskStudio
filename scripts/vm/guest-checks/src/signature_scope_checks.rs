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

#[derive(Clone, Copy)]
pub(super) enum Sample {
    Pdf,
    Formats,
    Media,
    Images,
}

impl Sample {
    fn folder(self) -> &'static str {
        match self {
            Self::Pdf => FOLDER,
            Self::Formats => "DS-SignatureFormats",
            Self::Media => "DS-SignatureMedia",
            Self::Images => "DS-SignatureImages",
        }
    }

    fn profile(self) -> &'static str {
        match self {
            Self::Pdf => "signature-scope-4kn",
            Self::Formats => "signature-formats-4kn",
            Self::Media => "signature-media-4kn",
            Self::Images => "signature-images-4kn",
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
        }
    }

    fn kinds(self) -> &'static [signature_scan::Kind] {
        use signature_scan::Kind;
        match self {
            Self::Pdf => &[Kind::Pdf],
            Self::Formats => &[Kind::SevenZip, Kind::Sqlite],
            Self::Media => &[Kind::Mp4, Kind::Mov, Kind::Ogg],
            Self::Images => &[Kind::Tiff, Kind::Ico, Kind::Cur],
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Baseline {
    tables: [String; 3],
    edges: [String; 2],
    partition: u32,
}

fn receipt(desktop: &str, sample: Sample) -> PathBuf {
    let profile = sample.profile().trim_end_matches("-4kn");
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

fn edges(file: &File) -> Result<[String; 2], String> {
    Ok([
        hash(file, START - SECTOR, SECTOR)?,
        hash(file, START + SIZE, SECTOR)?,
    ])
}

fn part() -> Result<u32, String> {
    let table = fat_resize_checks::model(B)?;
    if table.entries.len() != 2
        || !table.entries.iter().any(|entry| {
            entry.offset == START
                && entry.size == SIZE
                && entry.kind == policy::LINUX_DATA
                && entry.attributes == 0
        })
    {
        return Err("Expected the single exact unprotected signature fixture on B".into());
    }
    Ok(reallocation_checks::metadata_partition(B, None, START / MIB, SIZE / MIB)?.number)
}

fn write_fixture(bytes: &[u8], partition: u32) -> Result<[String; 2], String> {
    use crate::{partitions::platform::maintenance, surface_scan::Range};
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
                && p.size == SIZE
                && policy::linux_clone_candidate(&fresh.resolved.disk, &parts, p)
        })
        || bytes.len() as u64 != SIZE
    {
        return Err("Bound B signature fixture or protection changed".into());
    }
    let mut file = data_windows::exclusive_device(&fresh.resolved)?;
    let range = Range {
        offset: START,
        size: SIZE,
        sector: SECTOR,
    };
    let capture = |file: &mut File| {
        maintenance::capture_write_table(&fresh, file, &selected.target, range, true)
    };
    let table = capture(&mut file)?.ok_or("Missing B GPT")?;
    data_windows::persist_offline(&file)?;
    data_windows::confirm_writable_offline(&file)?;
    let surrounding = edges(&file)?;
    if capture(&mut file)? != Some(table.clone())
        || hash(&file, START, SIZE)? != format!("{:x}", Sha256::digest(vec![0; SIZE as usize]))
    {
        return Err("Expected zero fixture extent and unchanged B GPT before write".into());
    }
    let mut output =
        data_io::Region::new(file.try_clone().map_err(err)?, START, SIZE).map_err(err)?;
    output.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    if hash(&file, START, SIZE)? != format!("{:x}", Sha256::digest(bytes))
        || capture(&mut file)? != Some(table)
        || edges(&file)? != surrounding
    {
        return Err("B fixture readback, GPT or boundary differs".into());
    }
    data_windows::confirm_writable_offline(&file)?;
    Ok(surrounding)
}

pub(super) fn prepare(sample: Sample) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    for serial in [A, B] {
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let path = receipt(&desktop, sample);
    if path.try_exists().map_err(err)? {
        return Err("Signature fixture already prepared".into());
    }
    let (bytes, aligned) = fixture(sample)?;
    let mut log = backup_protection::begin(sample.profile())?;
    online(A)?;
    operation(A, Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (FileSystem::Ntfs, "DSSIGSCOPE", 4096),
        START,
        2 * GIB,
    )?;
    let (root, folder) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(sample.folder()).map_err(err)?;
    online(B)?;
    operation(B, Action::Initialize {})?;
    operation(
        B,
        Action::Create {
            offset: START,
            size: SIZE,
            kind: PartitionKind::Linux,
        },
    )?;
    let partition = part()?;
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    if tables[2] != os {
        return Err("Guest OS layout changed".into());
    }
    let edges = write_fixture(&bytes, partition)?;
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
    let scan = match sample {
        Sample::Pdf => format!("Scan PDF only. Expect 3 full, 2 aligned, 1 free+aligned. Free PDF byte offset {aligned:#x}."),
        Sample::Formats => "Scan 7z and SQLite only with both free clusters and sector alignment enabled. Expect one candidate of each type.".into(),
        Sample::Media => "Scan MP4, MOV and Ogg with free clusters and sector alignment enabled. Expect one MP4, one MOV and two Ogg files.".into(),
        Sample::Images => "Scan TIFF, ICO and CUR with free clusters and sector alignment enabled. Expect two TIFF files, one icon and one cursor.".into(),
    };
    Ok(format!(
        "PASS: B/{partition} 8 MiB is offline. {scan} Save Scope.dsrecover and export to {}.",
        folder.join(sample.folder()).display()
    ))
}

pub(super) fn verify(sample: Sample) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(&desktop, sample))
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let (bytes, aligned) = fixture(sample)?;
    let b = b_device(true)?;
    if part()? != baseline.partition
        || ntfs_reconstruction_checks::table_hashes()? != baseline.tables
        || edges(&b)? != baseline.edges
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

const COMPLETE_FOLDER: &str = "DS-Complete-Recovery";
// The last independently verified preparation of the isolated media pair.
const MEDIA_DESKTOP: &str = "1cea3f4c51ade6dc3b473461f1f460581885924daa03fed0feb91a2985a135f2";

fn complete_fixture() -> Result<(File, u32, u64), String> {
    btrfs_checks::desktop()?;
    let sample = Sample::Media;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(MEDIA_DESKTOP, sample))
            .map_err(err)?
            .take(4096),
    )
    .map_err(err)?;
    let (bytes, aligned) = fixture(sample)?;
    let b = b_device(true)?;
    if part()? != baseline.partition
        || ntfs_reconstruction_checks::table_hashes()? != baseline.tables
        || edges(&b)? != baseline.edges
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

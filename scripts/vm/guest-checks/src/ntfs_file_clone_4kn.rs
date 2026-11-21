//! 4Kn desktop file-clone fixtures and before/after verification.
use super::*;
use std::{io::Cursor, path::PathBuf};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_COMPRESSION, FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE,
};

#[path = "ntfs_file_clone_efs.rs"]
mod efs;
#[path = "ntfs_file_clone_links.rs"]
mod links;
#[path = "ntfs_file_clone_wof.rs"]
mod wof;
#[path = "ntfs_file_clone_wof_image.rs"]
mod wof_image;

const SECTOR: u64 = 4096;
const FIRST: u64 = 12 * GIB;
const WITNESS_LABEL: &str = "DSMOVENT4K";
const WITNESS_OFFSET: u64 = 792 * MIB;
const WITNESS_SIZE: u64 = 128 * MIB;
const PLAN: &str = "layout.json";
const BASELINE: &str = "baseline.json";
const IMAGE: &str = "source-before.zst";
const LINKS_EVIDENCE: &str = "links-native.json";
const WOF_EVIDENCE: &str = "wof-native.json";
const SETTLED_BASELINE: &str = "baseline-after-lock.json";
const SETTLED_IMAGE: &str = "source-after-lock.zst";
const SPARSE_HEAD: u64 = 256 * 1024;
const SPARSE_TAIL: u64 = 64 * MIB;
const SPARSE_BYTES: u64 = SPARSE_TAIL + 113;
// The metadata fixture was formatted before the catalogue update for TxF aliases.
const METADATA_FIXTURE_DESKTOP: &str =
    "dd6359eadcbba0d50a855b83b2451b15b194b872d64c9c24ddef7a75d4023ffb";
const LINKS_FIXTURE_DESKTOP: &str =
    "6355f5324a4dccdbfdc6f1abbf75b42ef331ccb78fe2fc1dbd726c2ca1b4f87d";

#[derive(Clone, Copy)]
pub(in super::super) enum Case {
    Standard,
    Metadata,
    Storage,
    Links,
    Wof,
    WofImage,
    Efs,
}

impl Case {
    fn labels(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Standard => ("DSNT4KCOPY", "NT4KDEST", "DS-NTFS-FileClone-4Kn"),
            Self::Metadata => ("DSNT4KMETA", "NT4KMETA", "DS-NTFS-FileClone-4Kn-Metadata"),
            Self::Storage => ("DSNT4KSTOR", "NT4KSTOR", "DS-NTFS-FileClone-4Kn-Storage"),
            Self::Links => ("DSNT4KLINK", "NT4KLINK", "DS-NTFS-FileClone-4Kn-Links"),
            Self::Wof => ("DSNT4KWOF", "NT4KWOF", "DS-NTFS-FileClone-4Kn-Wof"),
            Self::WofImage => ("DISKCLONE", "NT4KWOFIMG", "DS-NTFS-FileClone-4Kn-WofImage"),
            Self::Efs => ("DSNT4KEFS", "NT4KEFS", "DS-NTFS-FileClone-4Kn-Efs"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Fixture {
    desktop: String,
    source_at: u64,
    target_at: u64,
    prior: [String; 3],
    source_edges: [String; 2],
    target_edges: [String; 2],
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Baseline {
    source: u32,
    target: u32,
    source_digest: String,
    layouts: [String; 3],
}

fn profile() -> Result<(), String> {
    if test_sector() != SECTOR {
        return Err("This fixture requires the isolated 4Kn pair".into());
    }
    fat_sector_checks::confirm_4kn(&inventory()?)
}

fn witness_root() -> Result<Dir, String> {
    witness_volume().map(|(root, _)| root)
}

pub(in super::super) fn witness_volume() -> Result<(Dir, PathBuf), String> {
    let part = reallocation_checks::metadata_partition(
        A,
        Some(WITNESS_LABEL),
        WITNESS_OFFSET / MIB,
        WITNESS_SIZE / MIB,
    )?;
    let (selection, bound) = resolved(A, part.number, WITNESS_OFFSET, WITNESS_SIZE, true)?;
    let letter = part
        .drive_letter
        .filter(|letter| letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic())
        .ok_or("The verified A witness volume has no drive letter")?;
    let [volume] = bound.volumes.as_slice() else {
        return Err("Expected exactly one A witness volume".into());
    };
    let path = PathBuf::from(format!("{letter}:\\"));
    let root = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(err)?;
    data_windows::file_location(&root.try_clone().map_err(err)?.into_std_file())?
        .confirm_partition(
            &selection.target,
            &selection.layout_key,
            volume,
            (bound.disk.number, WITNESS_OFFSET, WITNESS_SIZE),
        )?;
    Ok((root, path))
}

fn read_record<T: serde::de::DeserializeOwned>(dir: &Dir, name: &str) -> Result<T, String> {
    let file = dir.open(name).map_err(err)?;
    if file.metadata().map_err(err)?.len() > 4096 {
        return Err("The 4Kn fixture record exceeds its limit".into());
    }
    serde_json::from_reader(file.take(4096)).map_err(err)
}

fn write_receipt(dir: &Dir, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut file = dir
        .open_with(name, CapOptions::new().create_new(true).write(true))
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}

fn read_fixture(case: Case) -> Result<(Dir, Fixture), String> {
    profile()?;
    let desktop = btrfs_checks::desktop()?;
    let dir = witness_root()?.open_dir(case.labels().2).map_err(err)?;
    let fixture: Fixture = read_record(&dir, PLAN)?;
    let matching_build = fixture.desktop == desktop
        || matches!(case, Case::Metadata) && fixture.desktop == METADATA_FIXTURE_DESKTOP
        || matches!(case, Case::Links) && fixture.desktop == LINKS_FIXTURE_DESKTOP;
    if !matching_build
        || !(FIRST..=TARGET_END - SOURCE_BYTES).contains(&fixture.source_at)
        || !(FIRST..=TARGET_END - TARGET_BYTES).contains(&fixture.target_at)
        || fixture.source_at % MIB != 0
        || fixture.target_at % MIB != 0
    {
        return Err("The planned 4Kn fixture does not match the desktop or safe ranges".into());
    }
    Ok((dir, fixture))
}

fn scope(fixture: &Fixture) -> Result<[String; 3], String> {
    scope_with_target(fixture, true)
}

fn scope_with_target(fixture: &Fixture, target_exists: bool) -> Result<[String; 3], String> {
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    if !retained_restore_checks::original_gpt_matches(
        &quick_partition_checks::table(A)?,
        &fixture.prior[0],
        &[(fixture.source_at, SOURCE_BYTES)],
    )? {
        return Err("A pre-existing A GPT entry changed".into());
    }
    let target_matches = if target_exists {
        retained_restore_checks::original_gpt_matches(
            &quick_partition_checks::table(B)?,
            &fixture.prior[1],
            &[(fixture.target_at, TARGET_BYTES)],
        )?
    } else {
        layouts[1] == fixture.prior[1]
    };
    if !target_matches {
        return Err("A pre-existing B GPT entry changed".into());
    }
    if layouts[2] != fixture.prior[2]
        || edges(A, fixture.source_at, SOURCE_BYTES)? != fixture.source_edges
        || edges(B, fixture.target_at, TARGET_BYTES)? != fixture.target_edges
    {
        return Err("An adjacent sector or the guest OS layout changed".into());
    }
    Ok(layouts)
}

pub(in super::super) fn prepare(case: Case) -> Result<String, String> {
    profile()?;
    let (source_label, _, directory) = case.labels();
    let rich_metadata = matches!(case, Case::Metadata);
    let desktop = btrfs_checks::desktop()?;
    let wof_source = matches!(case, Case::WofImage)
        .then(wof_image::load)
        .transpose()?;
    let a = fat_resize_checks::model(A)?;
    let b = fat_resize_checks::model(B)?;
    if a.guid == b.guid
        || a.entries
            .iter()
            .any(|a| b.entries.iter().any(|b| a.guid == b.guid))
    {
        return Err("Fixture disk or partition identities overlap".into());
    }
    let src_at = blank_window(A, &a, FIRST, TARGET_END, SOURCE_BYTES)?;
    let dst_at = blank_window(B, &b, FIRST, TARGET_END, TARGET_BYTES)?;
    let fixture = Fixture {
        desktop,
        source_at: src_at,
        target_at: dst_at,
        prior: ntfs_reconstruction_checks::table_hashes()?,
        source_edges: edges(A, src_at, SOURCE_BYTES)?,
        target_edges: edges(B, dst_at, TARGET_BYTES)?,
    };
    allocation_batch::set_online(A)?;
    {
        let root = witness_root()?;
        root.create_dir(directory).map_err(err)?;
        let dir = root.open_dir(directory).map_err(err)?;
        write_receipt(&dir, PLAN, &serde_json::to_vec(&fixture).map_err(err)?)?;
    }
    if let Some(image) = wof_source {
        return wof_image::prepare(&fixture, image, true);
    }
    let source = create(
        A,
        src_at,
        SOURCE_BYTES,
        FileSystem::Ntfs,
        source_label,
        4096,
        rich_metadata,
    )?;
    finish_prepare(&fixture, source, case)
}

pub(in super::super) fn continue_wof_prepare() -> Result<String, String> {
    let case = Case::Wof;
    let (dir, fixture) = read_fixture(case)?;
    scope_with_target(&fixture, false)?;
    if !available(
        &fat_resize_checks::model(B)?,
        fixture.target_at,
        TARGET_BYTES,
    ) || first_nonzero(&mut raw(B)?, fixture.target_at, TARGET_BYTES)?.is_some()
        || dir.symlink_metadata(WOF_EVIDENCE).is_ok()
        || dir.symlink_metadata(BASELINE).is_ok()
        || dir.symlink_metadata(IMAGE).is_ok()
    {
        return Err("The planned B window or WOF witness differs after the fixture stopped".into());
    }
    let (source_label, target_label, _) = case.labels();
    let source = reallocation_checks::metadata_partition(
        A,
        Some(source_label),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (selection, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let fresh = catalogue(&mut data_windows::reader(&bound)?, &AtomicBool::new(false))?;
    sample(&fresh)?;
    // Preserve the interrupted fixture's files. WOF seed creates only new names.
    let evidence = wof::seed(&selection, &bound)?;
    write_receipt(
        &dir,
        WOF_EVIDENCE,
        &serde_json::to_vec(&evidence).map_err(err)?,
    )?;
    let target = create(
        B,
        fixture.target_at,
        TARGET_BYTES,
        FileSystem::Fat16,
        target_label,
        4096,
        false,
    )?;
    scope(&fixture)?;
    Ok(format!(
        "Native WOF samples prepared. A/{} {source_label} 96 MiB to B/{target} {target_label} 48 MiB.",
        source.number
    ))
}

pub(in super::super) fn continue_wof_image_prepare() -> Result<String, String> {
    let (dir, fixture) = read_fixture(Case::WofImage)?;
    if dir.symlink_metadata(BASELINE).is_ok() || dir.symlink_metadata(IMAGE).is_ok() {
        return Err("The WOF image fixture already has a source baseline".into());
    }
    drop(dir);
    scope_with_target(&fixture, false)?;
    if !available(
        &fat_resize_checks::model(B)?,
        fixture.target_at,
        TARGET_BYTES,
    ) || first_nonzero(&mut raw(B)?, fixture.target_at, TARGET_BYTES)?.is_some()
        || first_nonzero(&mut raw(A)?, fixture.source_at, SOURCE_BYTES)?.is_some()
    {
        return Err("A or B changed after the interrupted WOF fixture preparation".into());
    }
    wof_image::prepare(&fixture, wof_image::load()?, false)
}

pub(in super::super) fn after_wof_image_online() -> Result<String, String> {
    let (dir, fixture) = read_fixture(Case::WofImage)?;
    if dir.symlink_metadata(BASELINE).is_ok() || dir.symlink_metadata(IMAGE).is_ok() {
        return Err("The WOF image fixture already has a source baseline".into());
    }
    drop(dir);
    wof_image::prepare_target(&fixture)
}

pub(in super::super) fn continue_metadata_prepare() -> Result<String, String> {
    let case = Case::Metadata;
    let (dir, fixture) = read_fixture(case)?;
    scope_with_target(&fixture, false)?;
    if !available(
        &fat_resize_checks::model(B)?,
        fixture.target_at,
        TARGET_BYTES,
    ) || first_nonzero(&mut raw(B)?, fixture.target_at, TARGET_BYTES)?.is_some()
        || dir.symlink_metadata(BASELINE).is_ok()
        || dir.symlink_metadata(IMAGE).is_ok()
    {
        return Err("The original B window or witness changed after preparation stopped".into());
    }
    let source = reallocation_checks::metadata_partition(
        A,
        Some(case.labels().0),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (_, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let mut reader = data_windows::reader(&bound)?;
    let cancel = AtomicBool::new(false);
    let fresh = catalogue(&mut reader, &cancel)?;
    let system = "System Volume Information"
        .encode_utf16()
        .collect::<Vec<_>>();
    let system_id = fresh.names.get(std::slice::from_ref(&system));
    let system_path = |path: &[Vec<u16>]| path.len() <= 2 && path.first() == Some(&system);
    if fresh
        .names
        .keys()
        .any(|path| !path.is_empty() && !system_path(path))
        || fresh
            .short_names
            .iter()
            .any(|(path, id)| !system_path(path) && !(path.len() == 1 && Some(id) == system_id))
    {
        return Err("The interrupted A fixture contains existing sample or unknown files".into());
    }
    drop(reader);
    write_receipt(
        &dir,
        "finish-prepare-by-absolute-volume.once",
        b"Complete the unstarted seed by verified volume identity and prepare the original blank target.",
    )?;
    finish_prepare(&fixture, source.number, case)
}

pub(in super::super) fn inspect_metadata_record() -> Result<String, String> {
    let case = Case::Metadata;
    let (_, fixture) = read_fixture(case)?;
    scope_with_target(&fixture, false)?;
    let source = reallocation_checks::metadata_partition(
        A,
        Some(case.labels().0),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (_, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let mut reader = data_windows::reader(&bound)?;
    let ntfs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    let mut record = 34u64;
    let mut report = Vec::new();
    for _ in 0..5 {
        let file = ntfs.file(&mut reader, record).map_err(err)?;
        let names = ntfs_names::read_for_merge(&mut reader, &file)?;
        if names.len() > 4 {
            return Err("The fixture record exceeds the diagnostic name budget".into());
        }
        let fields = names
            .iter()
            .map(|name| {
                format!(
                    "{} [{}] parent {}:{}",
                    name.display(),
                    name.namespace,
                    name.parent.0,
                    name.parent.1
                )
            })
            .collect::<Vec<_>>();
        let flags = file.info().map_err(err)?.file_attributes().bits();
        report.push(format!(
            "Record {record}:{} flags {flags:#x}, [{}]",
            file.sequence_number(),
            fields.join("; ")
        ));
        let Some(parent) = names.first().map(|name| name.parent) else {
            break;
        };
        if record == 5 || names.iter().any(|name| name.parent != parent) {
            break;
        }
        record = parent.0;
    }
    Ok(format!(
        "Read-only A/{} NTFS name diagnostic\n{}",
        source.number,
        report.join("\n")
    ))
}

pub(in super::super) fn inspect_metadata_sample() -> Result<String, String> {
    let case = Case::Metadata;
    let (_, fixture) = read_fixture(case)?;
    scope(&fixture)?;
    let source = reallocation_checks::metadata_partition(
        A,
        Some(case.labels().0),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let target = reallocation_checks::metadata_partition(
        B,
        None,
        fixture.target_at / MIB,
        TARGET_BYTES / MIB,
    )?;
    if target.volume_label.as_deref() != Some(case.labels().1) {
        return Err("The target is not the prepared metadata fixture".into());
    }
    let (_, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let (_, destination) = resolved(B, target.number, fixture.target_at, TARGET_BYTES, true)?;
    let cancel = AtomicBool::new(false);
    confirm_prepared_target(&destination, &cancel)?;
    let found = catalogue(&mut data_windows::reader(&bound)?, &cancel)?;
    sample(&found)?;
    metadata_sample(&found)?;
    if found.nodes.len() > 16 {
        return Err("Too many nodes for the metadata fixture diagnostic".into());
    }
    let mut details = Vec::new();
    for (&id, node) in &found.nodes {
        let attrs = node
            .metadata
            .keys()
            .map(|code| format!("{code:#x}"))
            .collect::<Vec<_>>();
        let streams = node
            .streams
            .iter()
            .map(|stream| format!("{:#x}", stream.storage_flags))
            .collect::<Vec<_>>();
        details.push(format!(
            "#{} {} {:#010x} metadata [{}], streams [{}]",
            id.0,
            if node.directory { "directory" } else { "file" },
            node.attributes,
            attrs.join(","),
            streams.join(",")
        ));
    }
    Ok(format!(
        "A/{} native short name and EA verified. B/{} still prepared.\n{}",
        source.number,
        target.number,
        details.join("\n")
    ))
}

pub(in super::super) fn inspect_link_record() -> Result<String, String> {
    let case = Case::Links;
    let (_, fixture) = read_fixture(case)?;
    scope(&fixture)?;
    let source = reallocation_checks::metadata_partition(
        A,
        Some(case.labels().0),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let (_, bound) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let mut reader = data_windows::reader(&bound)?;
    let fs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    let file = fs.file(&mut reader, 42).map_err(err)?;
    let names = ntfs_names::read_for_merge(&mut reader, &file)?;
    let mut report = vec![format!(
        "Read-only A/{} record 42:{} [{}]",
        source.number,
        file.sequence_number(),
        names
            .iter()
            .map(|name| name.display())
            .collect::<Vec<_>>()
            .join(", ")
    )];
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        if report.len() > 16 {
            return Err("Too many attributes in the fixture diagnostic".into());
        }
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        let ty = attr.ty().map_err(err)?;
        let mut message = format!("{ty} {} B", attr.value_length());
        if ty == ntfs::NtfsAttributeType::StandardInformation && attr.value_length() <= 72 {
            use ntfs::NtfsReadSeek;
            let mut bytes = vec![0; attr.value_length() as usize];
            attr.value(&mut reader)
                .map_err(err)?
                .read_exact(&mut reader, &mut bytes)
                .map_err(err)?;
            for (offset, value) in bytes
                .get(32..)
                .ok_or("Short standard information")?
                .chunks_exact(4)
                .enumerate()
            {
                if offset < 6 {
                    message.push_str(&format!(
                        " @{:#x}={:#010x}",
                        32 + offset * 4,
                        u32::from_le_bytes(value.try_into().unwrap())
                    ));
                }
            }
        }
        report.push(message);
    }
    Ok(report.join("\n"))
}

fn finish_prepare(fixture: &Fixture, source: u32, case: Case) -> Result<String, String> {
    let (source_label, target_label, _) = case.labels();
    let part = reallocation_checks::metadata_partition(
        A,
        Some(source_label),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    if part.number != source {
        return Err("Windows reported a different source partition after formatting".into());
    }
    let letter = match (part.drive_letter, case) {
        (
            _,
            Case::Metadata | Case::Storage | Case::Links | Case::Wof | Case::WofImage | Case::Efs,
        ) => None,
        (Some(letter), Case::Standard) => Some(letter),
        (None, Case::Standard) => {
            operation(
                A,
                Action::SetDriveLetter {
                    partition: source,
                    letter: Some(DriveLetter("A".into())),
                },
            )?;
            Some("A".into())
        }
    };
    let (selection, bound) = resolved(A, source, fixture.source_at, SOURCE_BYTES, true)?;
    seed(&selection, &bound, letter.as_deref())?;
    if matches!(case, Case::Storage) {
        seed_storage(&selection, &bound)?;
    }
    if matches!(case, Case::Efs) {
        efs::seed(&selection, &bound)?;
    }
    if matches!(case, Case::Links) {
        let evidence = links::seed(&selection, &bound)?;
        let dir = witness_root()?.open_dir(case.labels().2).map_err(err)?;
        write_receipt(
            &dir,
            LINKS_EVIDENCE,
            &serde_json::to_vec(&evidence).map_err(err)?,
        )?;
    }
    if matches!(case, Case::Wof) {
        let evidence = wof::seed(&selection, &bound)?;
        let dir = witness_root()?.open_dir(case.labels().2).map_err(err)?;
        write_receipt(
            &dir,
            WOF_EVIDENCE,
            &serde_json::to_vec(&evidence).map_err(err)?,
        )?;
    }
    let target = create(
        B,
        fixture.target_at,
        TARGET_BYTES,
        FileSystem::Fat16,
        target_label,
        4096,
        false,
    )?;
    scope(fixture)?;
    Ok(format!("4Kn fixtures prepared. Source A/{source} {source_label} 96 MiB. Target B/{target} {target_label} 48 MiB. Capture the source baseline before starting the desktop clone."))
}

fn storage_digest() -> [u8; 32] {
    let mut hash = Sha256::new();
    let zeros = [0; 65536];
    for offset in (0..SPARSE_TAIL).step_by(zeros.len()) {
        if offset == SPARSE_HEAD {
            hash.update([0x29; 65536]);
        } else {
            hash.update(zeros);
        }
    }
    hash.update([0x73; 113]);
    hash.finalize().into()
}

fn packed_contents() -> Vec<u8> {
    (0..196_621).map(|i| b'A' + (i / 1013 % 24) as u8).collect()
}

fn seed_storage(selected: &Selection, source: &data_windows::Resolved) -> Result<(), String> {
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected one A fixture volume".into());
    };
    let root = PathBuf::from(format!("{}\\", data_windows::volume_root(volume)?));
    let confirm = |file: &fs::File| {
        data_windows::file_location(file)?.confirm_partition(
            &selected.target,
            &selected.layout_key,
            volume,
            (source.disk.number, source.offset, source.size),
        )
    };
    let open_dir = |path: &Path, writable: bool| {
        let directory = OpenOptions::new()
            .read(true)
            .write(writable)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&directory)?;
        Ok::<_, String>(directory)
    };
    let _root = open_dir(&root, false)?;
    let create = |path: &Path| {
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(err)?;
        confirm(&file)?;
        Ok::<_, String>(file)
    };
    let mut sparse = create(&root.join("sparse.bin"))?;
    data_windows::control(&sparse, FSCTL_SET_SPARSE, &[], &mut [])?;
    sparse.set_len(SPARSE_BYTES).map_err(err)?;
    sparse.seek(SeekFrom::Start(SPARSE_HEAD)).map_err(err)?;
    sparse.write_all(&[0x29; 65536]).map_err(err)?;
    sparse.seek(SeekFrom::Start(SPARSE_TAIL)).map_err(err)?;
    sparse.write_all(&[0x73; 113]).map_err(err)?;
    sparse.sync_all().map_err(err)?;
    sparse.rewind().map_err(err)?;
    let mut hash = Sha256::new();
    if std::io::copy(&mut sparse.take(SPARSE_BYTES + 1), &mut hash).map_err(err)? != SPARSE_BYTES
        || <[u8; 32]>::from(hash.finalize()) != storage_digest()
    {
        return Err("Windows sparse source readback differs".into());
    }
    let folder = root.join("packed");
    fs::create_dir(&folder).map_err(err)?;
    let compress = |file: &fs::File| {
        data_windows::control(file, FSCTL_SET_COMPRESSION, &[2, 0], &mut [])?;
        let mut format = [0; 2];
        if data_windows::control(file, FSCTL_GET_COMPRESSION, &[], &mut format)? != 2
            || format != [2, 0]
        {
            return Err("Windows did not set LZNT1 compression".into());
        }
        Ok::<_, String>(())
    };
    compress(&open_dir(&folder, true)?)?;
    let mut packed = create(&folder.join("sample.bin"))?;
    compress(&packed)?;
    let mut stream = create(&folder.join("sample.bin:packed:$DATA"))?;
    compress(&stream)?;
    let expected = packed_contents();
    for (file, bytes) in [
        (&mut packed, expected.as_slice()),
        (&mut stream, b"packed ADS"),
    ] {
        file.write_all(bytes).map_err(err)?;
        file.sync_all().map_err(err)?;
        file.rewind().map_err(err)?;
        let mut actual = Vec::new();
        file.take(bytes.len() as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(err)?;
        if actual != bytes {
            return Err("Windows compressed source readback differs".into());
        }
    }
    Ok(())
}

fn storage_sample(catalogue: &partitions::merge::Catalogue) -> Result<(), String> {
    let node = |parts: &[&str]| {
        let path = parts
            .iter()
            .map(|part| part.encode_utf16().collect())
            .collect::<Vec<Vec<u16>>>();
        catalogue
            .names
            .get(&path)
            .and_then(|id| catalogue.nodes.get(id))
            .ok_or("Missing NTFS storage fixture")
    };
    let sparse = node(&["sparse.bin"])?;
    let [stream] = sparse.streams.as_slice() else {
        return Err("Expected one sparse stream".into());
    };
    if sparse.attributes & 0x200 == 0
        || stream.storage_flags != crate::ntfs_streams::SPARSE
        || stream.size != SPARSE_BYTES
        || stream.sha256 != storage_digest()
        || !stream
            .allocated_ranges
            .iter()
            .any(|range| range.start <= SPARSE_HEAD && range.end >= SPARSE_HEAD + 65536)
        || !stream
            .allocated_ranges
            .iter()
            .any(|range| range.start <= SPARSE_TAIL && range.end == SPARSE_BYTES)
        || stream
            .allocated_ranges
            .iter()
            .map(|range| range.end - range.start)
            .sum::<u64>()
            > MIB
    {
        return Err("Sparse source content or allocation map differs".into());
    }
    let packed = node(&["packed", "sample.bin"])?;
    let [main, ads] = packed.streams.as_slice() else {
        return Err("Expected unnamed and named compressed streams".into());
    };
    if node(&["packed"])?.attributes & 0x800 == 0
        || packed.attributes & 0x800 == 0
        || main.storage_flags != crate::ntfs_streams::LZNT1
        || main.sha256 != <[u8; 32]>::from(Sha256::digest(packed_contents()))
        || ads.sha256 != <[u8; 32]>::from(Sha256::digest(b"packed ADS"))
    {
        return Err("Compressed source content or flags differ".into());
    }
    Ok(())
}

fn source_bytes(bound: &data_windows::Resolved) -> Result<Vec<u8>, String> {
    if bound.size != SOURCE_BYTES {
        return Err("The source snapshot exceeds the fixture range".into());
    }
    let mut bytes = vec![0; SOURCE_BYTES as usize];
    data_windows::reader(bound)?
        .read_exact(&mut bytes)
        .map_err(err)?;
    Ok(bytes)
}

pub(in super::super) fn baseline(case: Case) -> Result<String, String> {
    let (dir, fixture) = read_fixture(case)?;
    let (source_label, target_label, _) = case.labels();
    let layouts = scope(&fixture)?;
    if dir.symlink_metadata(BASELINE).is_ok() || dir.symlink_metadata(IMAGE).is_ok() {
        return Err("A source baseline already exists".into());
    }
    let source = reallocation_checks::metadata_partition(
        A,
        Some(source_label),
        fixture.source_at / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let target = reallocation_checks::metadata_partition(
        B,
        None,
        fixture.target_at / MIB,
        TARGET_BYTES / MIB,
    )?;
    if target.volume_label.as_deref() != Some(target_label) {
        return Err("The new B volume has an unexpected label".into());
    }
    let (selected, src) = resolved(A, source.number, fixture.source_at, SOURCE_BYTES, true)?;
    let (_, dst) = resolved(B, target.number, fixture.target_at, TARGET_BYTES, true)?;
    let cancel = AtomicBool::new(false);
    confirm_prepared_target(&dst, &cancel)?;
    let locked = if matches!(
        case,
        Case::Metadata | Case::Storage | Case::Links | Case::Wof | Case::WofImage | Case::Efs
    ) {
        let locks = data_windows::state_locks(&src)?;
        let bound = data_windows::resolve_locked(&selected, false, &src, &locks)?;
        let snapshot = source_bytes(&bound)?;
        if snapshot != source_bytes(&bound)? {
            return Err("The fixture source changed while locked".into());
        }
        Some(snapshot)
    } else {
        None
    };
    let before = source_bytes(&src)?;
    if before != source_bytes(&src)? {
        return Err("The source changed during the two control reads".into());
    }
    if let Some(locked) = locked {
        source_lock::verify_clone_source(&locked, &before, &cancel)?;
    }
    validate_snapshot(&before, &fixture, &dir, case, &cancel)?;
    save_baseline(
        &dir,
        (BASELINE, IMAGE),
        &before,
        source.number,
        target.number,
        layouts,
        case,
    )
}

fn validate_snapshot(
    before: &[u8],
    fixture: &Fixture,
    dir: &Dir,
    case: Case,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let found = catalogue(&mut Cursor::new(before), cancel)?;
    if matches!(case, Case::WofImage) {
        wof_image::sample(&found, cancel)?;
    } else {
        sample(&found)?;
    }
    if matches!(case, Case::Metadata) {
        metadata_sample(&found)?;
    }
    if matches!(case, Case::Storage) {
        storage_sample(&found)?;
    }
    if matches!(case, Case::Efs) {
        efs::sample(&found)?;
    }
    if matches!(case, Case::Links) {
        links::sample(&found, &read_record(dir, LINKS_EVIDENCE)?)?;
    }
    if matches!(case, Case::Wof) {
        let evidence: Vec<wof::Evidence> = read_record(dir, WOF_EVIDENCE)?;
        wof::sample(&found, &evidence)?;
    }
    crate::ntfs_file_clone::Plan::prepare(
        &mut Cursor::new(before),
        surface_scan::Range {
            offset: fixture.target_at,
            size: TARGET_BYTES,
            sector: SECTOR,
        },
        0x3910_8133_4579_4021,
        cancel,
    )?;
    source_lock::verify_clone_source(before, before, cancel)?;
    Ok(())
}

fn save_baseline(
    dir: &Dir,
    (receipt_name, image): (&str, &str),
    before: &[u8],
    source: u32,
    target: u32,
    layouts: [String; 3],
    case: Case,
) -> Result<String, String> {
    let encoded = zstd::bulk::compress(before, 1).map_err(err)?;
    write_receipt(dir, image, &encoded)?;
    let receipt = Baseline {
        source,
        target,
        source_digest: format!("{:x}", Sha256::digest(before)),
        layouts,
    };
    write_receipt(
        dir,
        receipt_name,
        &serde_json::to_vec(&receipt).map_err(err)?,
    )?;
    let (source_label, target_label, _) = case.labels();
    Ok(format!("4Kn baseline ready. A/{source} {source_label} -> B/{target} {target_label}. Two matching reads saved all 96 MiB of source bytes. SHA-256 {}.", receipt.source_digest))
}

pub(in super::super) fn settled_baseline() -> Result<String, String> {
    let (dir, fixture) = read_fixture(Case::Standard)?;
    if dir.symlink_metadata(SETTLED_BASELINE).is_ok() || dir.symlink_metadata(SETTLED_IMAGE).is_ok()
    {
        return Err("The post-lock source baseline already exists".into());
    }
    let original: Baseline = read_record(&dir, BASELINE)?;
    let previous = load_baseline(&dir, IMAGE, &original)?;
    let layouts = scope(&fixture)?;
    if layouts != original.layouts {
        return Err("A/B GPT or guest OS layout differs from the first baseline".into());
    }
    let (selected, src) = resolved(A, original.source, fixture.source_at, SOURCE_BYTES, true)?;
    let (_, dst) = resolved(B, original.target, fixture.target_at, TARGET_BYTES, true)?;
    let cancel = AtomicBool::new(false);
    confirm_prepared_target(&dst, &cancel)?;
    let target_digest = resize_clone_checks::hash(&dst)?;
    let mut prior_catalogue = catalogue(&mut Cursor::new(previous), &cancel)?;
    sample(&prior_catalogue)?;
    let locks = data_windows::state_locks(&src)?;
    let bound = data_windows::resolve_locked(&selected, false, &src, &locks)?;
    let locked = source_bytes(&bound)?;
    if locked != source_bytes(&bound)? {
        return Err("The source changed while the volume was locked".into());
    }
    drop(locks);
    let before = source_bytes(&src)?;
    if before != source_bytes(&src)? {
        return Err("The source changed during the control reads after unlocking".into());
    }
    let current = catalogue(&mut Cursor::new(&before), &cancel)?;
    if current.names != prior_catalogue.names || current.short_names != prior_catalogue.short_names
    {
        return Err("The prepared source names differ from the first baseline".into());
    }
    let system_directory = vec!["System Volume Information".encode_utf16().collect()];
    if let Some(id) = current.names.get(&system_directory) {
        let old = prior_catalogue
            .nodes
            .get_mut(id)
            .ok_or("Missing original system directory")?;
        let new = current
            .nodes
            .get(id)
            .ok_or("Missing current system directory")?;
        if old.directory
            && new.directory
            && new.times[1] >= old.times[1]
            && new.times[2] >= old.times[2]
        {
            old.times[1..].copy_from_slice(&new.times[1..]);
        }
    }
    prior_catalogue.verify_copy(&current)?;
    source_lock::verify_clone_source(&locked, &before, &cancel)?;
    validate_snapshot(&before, &fixture, &dir, Case::Standard, &cancel)?;
    if scope(&fixture)? != layouts || resize_clone_checks::hash(&dst)? != target_digest {
        return Err(
            "The prepared target or its protected state changed settling the source".into(),
        );
    }
    save_baseline(
        &dir,
        (SETTLED_BASELINE, SETTLED_IMAGE),
        &before,
        original.source,
        original.target,
        layouts,
        Case::Standard,
    )
}

pub(in super::super) fn verify(case: Case) -> Result<String, String> {
    verify_baseline(BASELINE, IMAGE, case)
}

pub(in super::super) fn verify_settled() -> Result<String, String> {
    verify_baseline(SETTLED_BASELINE, SETTLED_IMAGE, Case::Standard)
}

fn verify_baseline(receipt: &str, image: &str, case: Case) -> Result<String, String> {
    let (dir, fixture) = read_fixture(case)?;
    let baseline: Baseline = read_record(&dir, receipt)?;
    if scope(&fixture)? != baseline.layouts {
        return Err("The 4Kn GPTs or the OS layout differ from the clone baseline".into());
    }
    let (_, src) = resolved(A, baseline.source, fixture.source_at, SOURCE_BYTES, true)?;
    let (_, dst) = resolved(B, baseline.target, fixture.target_at, TARGET_BYTES, false)?;
    data_windows::verify_offline_device(&dst)?;
    let before = load_baseline(&dir, image, &baseline)?;
    let cancel = AtomicBool::new(false);
    match case {
        Case::Metadata => metadata_sample(&catalogue(&mut Cursor::new(&before), &cancel)?)?,
        Case::Storage => storage_sample(&catalogue(&mut Cursor::new(&before), &cancel)?)?,
        Case::Links => links::sample(
            &catalogue(&mut Cursor::new(&before), &cancel)?,
            &read_record(&dir, LINKS_EVIDENCE)?,
        )?,
        Case::Wof => {
            let evidence: Vec<wof::Evidence> = read_record(&dir, WOF_EVIDENCE)?;
            wof::sample(&catalogue(&mut Cursor::new(&before), &cancel)?, &evidence)?;
        }
        Case::Efs => efs::sample(&catalogue(&mut Cursor::new(&before), &cancel)?)?,
        Case::WofImage | Case::Standard => (),
    }
    let after = source_bytes(&src)?;
    let digest = verify_target(&src, &dst, fixture.target_at, |target| {
        if matches!(case, Case::WofImage) {
            wof_image::sample(target, &cancel)
        } else {
            sample(target)
        }
    })?;
    let delta = source_lock::verify_clone_source(&before, &after, &AtomicBool::new(false))
        .map_err(|error| format!("Offline target files and NTFS metadata verified. Both GPTs, edges and guest OS layout match.\nTarget SHA-256\n{digest}\nSource audit stopped: {error}"))?;
    let root_caches = if delta.root_cache_names.is_empty() {
        String::new()
    } else {
        format!(
            "\nRoot index time caches matched live records: {}.",
            delta.root_cache_names.join(", ")
        )
    };
    Ok(format!("4Kn NTFS file clone verified. A/{} -> B/{}.\nSource catalogue agrees. {} changed source sectors accounted for. Checked {} MFT and {} directory index records. TxF index write/change times advanced {}/{}; verified cached lengths {}.{root_caches}\nGPTs, neighboring sectors and guest OS layout agree. B remains offline.\nTarget SHA-256\n{digest}", baseline.source, baseline.target, delta.sectors, delta.mft_records, delta.index_records, delta.txf_index_fields[0], delta.txf_index_fields[1], delta.txf_index_fields[2]))
}

fn load_baseline(dir: &Dir, image: &str, baseline: &Baseline) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    dir.open(image)
        .map_err(err)?
        .take(SOURCE_BYTES + MIB + 1)
        .read_to_end(&mut encoded)
        .map_err(err)?;
    if encoded.len() as u64 > SOURCE_BYTES + MIB {
        return Err("The compressed baseline exceeds its fixture limit".into());
    }
    let before = zstd::bulk::decompress(&encoded, SOURCE_BYTES as usize).map_err(err)?;
    if before.len() as u64 != SOURCE_BYTES
        || format!("{:x}", Sha256::digest(&before)) != baseline.source_digest
    {
        return Err("The stored source baseline does not match its length or SHA-256".into());
    }
    Ok(before)
}

pub(in super::super) fn inspect_source() -> Result<String, String> {
    let (dir, fixture) = read_fixture(Case::Standard)?;
    let baseline: Baseline = read_record(&dir, BASELINE)?;
    if scope(&fixture)? != baseline.layouts {
        return Err("A/B GPT or guest OS layout differs from the baseline".into());
    }
    let (_, src) = resolved(A, baseline.source, fixture.source_at, SOURCE_BYTES, true)?;
    let (_, dst) = resolved(B, baseline.target, fixture.target_at, TARGET_BYTES, true)?;
    let cancel = AtomicBool::new(false);
    confirm_prepared_target(&dst, &cancel)?;
    let before = load_baseline(&dir, IMAGE, &baseline)?;
    let after = source_bytes(&src)?;
    let original = catalogue(&mut Cursor::new(&before), &cancel)?;
    let current = catalogue(&mut Cursor::new(&after), &cancel)?;
    if original.names != current.names || original.short_names != current.short_names {
        return Err("Source directory names or record identities differ".into());
    }
    let mut differences = Vec::new();
    let changed_sectors = before
        .chunks_exact(SECTOR as usize)
        .zip(after.chunks_exact(SECTOR as usize))
        .filter(|(a, b)| a != b)
        .count();
    for (&id, node) in &original.nodes {
        let now = current.nodes.get(&id).ok_or("Source record is missing")?;
        let mut fields = Vec::new();
        for (position, label) in ["created", "modified", "accessed"].into_iter().enumerate() {
            if node.times[position] != now.times[position] {
                fields.push(format!(
                    "{label} {} -> {}",
                    node.times[position], now.times[position]
                ));
            }
        }
        if node.attributes != now.attributes || node.directory != now.directory {
            fields.push(format!(
                "flags {:#x} -> {:#x}",
                node.attributes, now.attributes
            ));
        }
        if node.metadata != now.metadata || original.descriptor(id)? != current.descriptor(id)? {
            fields.push("metadata or ACL changed".into());
        }
        let content = |s: &partitions::merge::Stream| {
            (s.locator.name.clone(), s.size, s.storage_flags, s.sha256)
        };
        if !node
            .streams
            .iter()
            .map(content)
            .eq(now.streams.iter().map(content))
        {
            fields.push("data streams changed".into());
        }
        if !fields.is_empty() {
            differences.push(format!("MFT #{} {}", id.0, fields.join(", ")));
        }
    }
    if differences.is_empty() {
        differences.push(match original.same_source(&current) {
            Ok(()) => "Source catalogue still matches".into(),
            Err(error) => error,
        });
    }
    Ok(format!("A/{} comparison with saved baseline: {changed_sectors} changed sectors.\n{}\nB/{} is still the prepared FAT16 fixture. Protected layouts and boundaries agree.", baseline.source, differences.join("\n"), baseline.target))
}

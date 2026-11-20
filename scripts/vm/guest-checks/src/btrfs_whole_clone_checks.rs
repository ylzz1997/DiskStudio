//! A mixed GPT fixture with resizable Btrfs, NTFS and a fixed sector copy.
use super::*;
use crate::whole_file_clone_checks as whole;
use std::io::Cursor;

const MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
const RAW_SIZE: u64 = 32 * MIB;
const NTFS_START: u64 = 512 * MIB;
const NTFS_SIZE: u64 = 96 * MIB;
const NTFS_TARGET: u64 = 48 * MIB;
const LABEL: &str = "DSMIXED_N";

#[derive(Serialize, Deserialize)]
struct Baseline {
    source: table_archive::Archive,
    os: String,
    files: String,
    ntfs: String,
}

fn receipt(desktop: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-mixed-clone-{desktop}.json"
    ))
}

fn image_sha256() -> Result<&'static str, String> {
    option_env!("DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256")
        .ok_or("Missing bound Btrfs relocation fixture digest".into())
}

fn opaque_fixture() -> Vec<u8> {
    (0..RAW_SIZE)
        .map(|n| ((n * 109 + n / 137 + n / 4096) % 251) as u8)
        .collect()
}

fn entries(layout: &table_restore::Layout) -> Result<[&table_restore::Entry; 4], String> {
    use partitions::policy::{BASIC_DATA, LINUX_DATA};
    if layout.entries.len() != 4 || layout.entries.iter().any(|entry| entry.attributes != 0) {
        return Err("Unexpected mixed source GPT contents".into());
    }
    let required = |kind: &str, size: Option<u64>| {
        let found = layout
            .entries
            .iter()
            .filter(|entry| entry.kind == kind && size.is_none_or(|size| entry.size == size))
            .collect::<Vec<_>>();
        match found.as_slice() {
            [entry] => Ok(*entry),
            _ => Err("Missing or ambiguous mixed source entry".to_owned()),
        }
    };
    let ntfs = required(BASIC_DATA, Some(NTFS_SIZE))?;
    if ntfs.offset != NTFS_START {
        return Err("Unexpected NTFS source offset".into());
    }
    Ok([
        required(MSR, None)?,
        required(LINUX_DATA, Some(MOVE_SIZE))?,
        required(LINUX_DATA, Some(RAW_SIZE))?,
        ntfs,
    ])
}

fn reader<'a>(
    file: &'a File,
    entry: &table_restore::Entry,
) -> Result<data_io::Region<&'a File>, String> {
    data_io::Region::new(file, entry.offset, entry.size).map_err(err)
}

fn fsid(input: &mut (impl Read + Seek)) -> Result<[u8; 16], String> {
    input.seek(SeekFrom::Start(65536 + 32)).map_err(err)?;
    let mut id = [0; 16];
    input.read_exact(&mut id).map_err(err)?;
    Ok(id)
}

fn verify_source(
    file: &File,
    btrfs: &table_restore::Entry,
    raw: &table_restore::Entry,
    ntfs: &table_restore::Entry,
) -> Result<(String, String), String> {
    let mut view = reader(file, btrfs)?;
    if data_io::hash_exact(&mut view, btrfs.size, |_| Ok(()))? != image_sha256()? {
        return Err("The original Btrfs partition digest differs".into());
    }
    let raw_hash = format!("{:x}", Sha256::digest(opaque_fixture()));
    if data_io::hash_exact(&mut reader(file, raw)?, raw.size, |_| Ok(()))? != raw_hash {
        return Err("The original opaque partition digest differs".into());
    }
    Ok((
        move_contents(&mut view)?,
        whole::sample_hash(&mut reader(file, ntfs)?, &whole::sample(0))?,
    ))
}

pub(in super::super) fn prepare(resume: bool) -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    whole::empty_raw(&disks, B)?;
    let os = reallocation_checks::os_layout(&disks)?;
    if receipt(&desktop).try_exists().map_err(err)? {
        return Err("The mixed-clone baseline already exists".into());
    }
    if !resume {
        whole::empty_raw(&disks, A)?;
        let image_path = std::env::current_exe()
            .map_err(err)?
            .with_file_name("Btrfs-Clone-Source.raw");
        let mut image = File::open(&image_path).map_err(err)?;
        let expected = image_sha256()?;
        if image.metadata().map_err(err)?.len() != MOVE_SIZE
            || allocation_batch::hash_file(&image_path)? != expected
            || disk_core::btrfs::plan_resize(&mut image, MOVE_TARGET)?.relocation_bytes()
                != MOVE_BYTES
        {
            return Err("Unexpected Btrfs relocation fixture".into());
        }
        let mut log = backup_protection::begin("mixed-file-clone-4kn")?;
        online(A)?;
        operation(A, partitions::Action::Initialize {})?;
        let btrfs = create_linux_fixture(A, MOVE_SIZE)?;
        seed_linux_fixture(
            A,
            &btrfs,
            MOVE_SIZE,
            &mut image,
            expected,
            |_| Ok(()),
            |_| Ok(()),
        )?;
        let raw = create_linux_fixture(A, RAW_SIZE)?;
        let mut opaque = Cursor::new(opaque_fixture());
        let expected = format!("{:x}", Sha256::digest(opaque.get_ref()));
        seed_linux_fixture(
            A,
            &raw,
            RAW_SIZE,
            &mut opaque,
            &expected,
            |_| Ok(()),
            |_| Ok(()),
        )?;
        let (ntfs, _) = fat_resize_checks::create_capacity_fixture(
            &mut log,
            (partitions::FileSystem::Ntfs, LABEL, 4096),
            NTFS_START,
            NTFS_SIZE,
        )?;
        allocation_batch::write_fixture(
            &ntfs,
            NTFS_START,
            NTFS_SIZE,
            whole::SAMPLE,
            &whole::sample(0),
            true,
        )?;
    }
    let disks = inventory()?;
    whole::empty_raw(&disks, B)?;
    reallocation_checks::state(&disks, A, false)?;
    if reallocation_checks::os_layout(&disks)? != os {
        return Err("Guest OS layout changed during fixture preparation".into());
    }
    let source = quick_partition_checks::table(A)?;
    let layout = table_restore::archive_layout(&source)?;
    let [_, btrfs, raw, ntfs] = entries(&layout)?;
    let selected = selection(A, None)?;
    let bound = data_windows::resolve(&selected, true, false)?;
    let locks = data_windows::state_locks(&bound)?;
    let fresh = data_windows::resolve_locked(&selected, false, &bound, &locks)?;
    let file = data_windows::device(&fresh, false)?;
    let (files, ntfs_sample) = verify_source(&file, btrfs, raw, ntfs)?;
    data_windows::verify_data_locks(&locks, &fresh)?;
    let locate = |entry: &table_restore::Entry| {
        reallocation_checks::metadata_partition(A, None, entry.offset / MIB, entry.size / MIB)
    };
    let (btrfs_number, raw_number, ntfs_number) = (
        locate(btrfs)?.number,
        locate(raw)?.number,
        locate(ntfs)?.number,
    );
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt(&desktop))
        .map_err(err)?;
    serde_json::to_writer(
        &mut log,
        &Baseline {
            source,
            os,
            files,
            ntfs: ntfs_sample,
        },
    )
    .map_err(err)?;
    log.sync_all().map_err(err)?;
    Ok(format!("PASS: A is online, B empty RAW.\nA/{btrfs_number} Btrfs -> 192 MiB (80 MiB relocation).\nA/{raw_number} opaque -> leave original size and start.\nA/{ntfs_number} NTFS -> 48 MiB."))
}

pub(in super::super) fn verify() -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    reallocation_checks::state(&disks, A, false)?;
    reallocation_checks::state(&disks, B, true)?;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(&desktop))
            .map_err(err)?
            .take(1024 * 1024),
    )
    .map_err(err)?;
    if quick_partition_checks::table(A)? != baseline.source
        || reallocation_checks::os_layout(&disks)? != baseline.os
    {
        return Err("The source GPT or guest OS layout changed".into());
    }
    let source = table_restore::archive_layout(&baseline.source)?;
    let [msr, btrfs, raw, ntfs] = entries(&source)?;
    let archive = quick_partition_checks::table(B)?;
    let target = table_restore::archive_layout(&archive)?;
    if source.guid == target.guid || target.entries.len() != source.entries.len() {
        return Err("Target GPT identity or partition count differs".into());
    }
    let cloned = |original: &table_restore::Entry, size: u64| {
        target
            .entries
            .iter()
            .find(|entry| {
                entry.slot == original.slot
                    && entry.kind == original.kind
                    && entry.name == original.name
                    && entry.attributes == 0
                    && entry.size == size
                    && source.entries.iter().all(|old| old.guid != entry.guid)
            })
            .ok_or("A cloned GPT entry differs from its confirmed size or identity")
    };
    let (new_msr, new_btrfs, new_raw, new_ntfs) = (
        cloned(msr, msr.size)?,
        cloned(btrfs, MOVE_TARGET)?,
        cloned(raw, raw.size)?,
        cloned(ntfs, NTFS_TARGET)?,
    );
    if new_msr.offset != msr.offset || new_raw.offset != raw.offset {
        return Err("A fixed-start GPT partition moved".into());
    }
    let a = data_windows::device(
        &data_windows::resolve(&selection(A, None)?, false, false)?,
        false,
    )?;
    let b = data_windows::device(
        &data_windows::resolve(&selection(B, None)?, false, false)?,
        false,
    )?;
    data_windows::confirm_writable_offline(&b)?;
    let (files, ntfs_sample) = verify_source(&a, btrfs, raw, ntfs)?;
    let mut btrfs_copy = reader(&b, new_btrfs)?;
    let btrfs_id = fsid(&mut reader(&a, btrfs)?)?;
    let raw_hash = format!("{:x}", Sha256::digest(opaque_fixture()));
    if files != baseline.files
        || ntfs_sample != baseline.ntfs
        || move_contents(&mut btrfs_copy)? != files
        || fsid(&mut btrfs_copy)? != btrfs_id
        || data_io::hash_exact(&mut reader(&b, new_raw)?, new_raw.size, |_| Ok(()))? != raw_hash
        || whole::sample_hash(&mut reader(&b, new_ntfs)?, &whole::sample(0))? != ntfs_sample
    {
        return Err("A source or destination filesystem or sector copy differs".into());
    }
    disk_core::btrfs::plan_resize(&mut btrfs_copy, MOVE_SIZE)?;
    let occupied = archive
        .extents
        .iter()
        .map(|extent| (extent.offset, extent.offset + extent.bytes.len() as u64))
        .chain(
            [new_btrfs, new_raw, new_ntfs].map(|entry| (entry.offset, entry.offset + entry.size)),
        )
        .collect();
    let digest = whole::verify_bytes(&b, archive.size, occupied)?;
    data_windows::confirm_writable_offline(&b)?;
    Ok(format!("PASS: Btrfs 51 files and UUID, NTFS sample, opaque 32 MiB and fixed start match.\nNew GPT, zeroed gaps, original A and guest OS layouts verified. B offline.\nFull disk SHA-256\n{digest}"))
}

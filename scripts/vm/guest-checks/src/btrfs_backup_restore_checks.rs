//! Retained used-space and incremental Btrfs archives on the isolated A disk.
use super::*;
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use std::io::Cursor;

const FOLDER: &str = "DS-BtrfsRestore";
const EARLIER: &str = "Earlier.dsbackup";
const LATEST: &str = "Latest.dsbackup";
const LATEST_MARKER: &[u8; 16] = b"latest-restore-1";
const MARKER_AT: usize = 4096;
const OFFSETS: [u64; 3] = [32 * MIB, 464 * MIB, 704 * MIB];
const SIZES: [u64; 3] = [MOVE_SIZE, MOVE_TARGET, 448 * MIB];

#[derive(Serialize, Deserialize)]
struct Baseline {
    tables: [String; 3],
    archive_files: [String; 2],
    earlier: String,
    contents: String,
    identity: [u8; 16],
    original_marker: [u8; 16],
    edges: [String; 6],
}

fn receipt(desktop: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-btrfs-restore-{desktop}.json"
    ))
}

fn archive_digests(directory: &Dir) -> Result<[String; 2], String> {
    let digest = |name| {
        let mut file = directory.open(name).map_err(err)?.into_std();
        let length = file.metadata().map_err(err)?.len();
        if length == 0 || length > MOVE_SIZE + MIB {
            return Err("Unexpected archive file length".into());
        }
        data_io::hash_exact(&mut file, length, |_| Ok(()))
    };
    Ok([digest(EARLIER)?, digest(LATEST)?])
}

fn write_archive(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
    parent: Option<&backup_archive::Chain>,
) -> Result<(), String> {
    let mut image = Cursor::new(bytes);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let map = used_space::inspect(&mut image, MOVE_SIZE, 4096, None, &cancel)?;
    if map.volumes.len() != 1 || map.volumes[0].filesystem != "Btrfs" || map.skipped == 0 {
        return Err("Expected a verified Btrfs allocation plan".into());
    }
    map.proof.verify(&mut image, &cancel)?;
    let mut file = directory
        .open_with(
            name,
            CapOptions::new().read(true).write(true).create_new(true),
        )
        .map_err(err)?
        .into_std();
    let target = selection(A, None)?.target;
    let binding = data_windows::FileBinding::new(&file, &target)?;
    backup_archive::create_with_policy(
        &mut map.masked(image),
        &mut file,
        "partition",
        MOVE_SIZE,
        4096,
        true,
        parent,
        Some(backup_archive::AllocationPolicy {
            skipped_bytes: map.skipped,
            map_sha256: map.digest.clone(),
        }),
        |_| Ok(()),
    )?;
    file.sync_all().map_err(err)?;
    data_windows::FileBinding::validate_all(&[binding])
}

fn open_chain(directory: &Dir) -> Result<backup_archive::Chain, String> {
    let open = |name| backup_archive::Archive::open(directory.open(name).map_err(err)?.into_std());
    let mut chain = backup_archive::Chain::new(open(EARLIER)?).prepend(open(LATEST)?)?;
    chain.verify_at(1, |_| Ok(()))?;
    chain.verify(|_| Ok(()))?;
    Ok(chain)
}

fn marker(reader: &mut (impl Read + Seek), at: u64) -> Result<[u8; 16], String> {
    reader.seek(SeekFrom::Start(at)).map_err(err)?;
    let mut bytes = [0; 16];
    reader.read_exact(&mut bytes).map_err(err)?;
    Ok(bytes)
}

fn target_entries() -> Result<[u32; 3], String> {
    let layout = fat_resize_checks::model(B)?;
    if layout.entries.len() != 4
        || layout
            .entries
            .iter()
            .filter(|entry| entry.kind == "e3c9e316-0b5c-4db8-817d-f92df00215ae")
            .count()
            != 1
    {
        return Err("Expected an MSR and three exact Linux restore targets".into());
    }
    let mut numbers = [0; 3];
    for (number, (offset, size)) in numbers.iter_mut().zip(OFFSETS.into_iter().zip(SIZES)) {
        if !layout.entries.iter().any(|entry| {
            entry.kind == partitions::policy::LINUX_DATA
                && entry.attributes == 0
                && entry.offset == offset
                && entry.size == size
        }) {
            return Err("Restore fixture has an unexpected GPT extent".into());
        }
        *number =
            reallocation_checks::metadata_partition(B, None, offset / MIB, size / MIB)?.number;
    }
    Ok(numbers)
}

fn disk_b(offline: bool) -> Result<File, String> {
    reallocation_checks::state(&inventory()?, B, offline)?;
    let bound = data_windows::resolve(&selection(B, None)?, false, false)?;
    let file = data_windows::device(&bound, false)?;
    if offline {
        data_windows::confirm_writable_offline(&file)?;
    }
    Ok(file)
}

fn edges(file: &mut File) -> Result<[String; 6], String> {
    let mut result = std::array::from_fn(|_| String::new());
    for ((offset, size), hashes) in OFFSETS
        .into_iter()
        .zip(SIZES)
        .zip(result.chunks_exact_mut(2))
    {
        for (at, out) in [offset - 4096, offset + size].into_iter().zip(hashes) {
            let bytes = table_archive::read_at(file, at, 4096, 16 * GIB)?;
            *out = format!("{:x}", Sha256::digest(bytes));
        }
    }
    Ok(result)
}

pub(in super::super) fn prepare(resume: bool) -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    if !resume {
        for serial in [A, B] {
            crate::whole_file_clone_checks::empty_raw(&disks, serial)?;
        }
        let source_path = std::env::current_exe()
            .map_err(err)?
            .with_file_name("Btrfs-Clone-Source.raw");
        let expected =
            option_env!("DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256").ok_or("Missing fixture digest")?;
        let mut image = File::open(source_path).map_err(err)?.take(MOVE_SIZE + 1);
        let mut bytes = Vec::new();
        image.read_to_end(&mut bytes).map_err(err)?;
        if bytes.len() as u64 != MOVE_SIZE
            || format!("{:x}", Sha256::digest(&bytes)) != expected
            || disk_core::btrfs::plan_resize(&mut Cursor::new(&bytes), MOVE_TARGET)?
                .relocation_bytes()
                != MOVE_BYTES
        {
            return Err("Unexpected Btrfs source image".into());
        }
        let mut log = backup_protection::begin("btrfs-retained-restore-4kn")?;
        online(A)?;
        operation(A, partitions::Action::Initialize {})?;
        fat_resize_checks::create_capacity_fixture(
            &mut log,
            (partitions::FileSystem::Ntfs, "DSBTRFSARCH", 4096),
            32 * MIB,
            2 * GIB,
        )?;
        let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
        root.create_dir(FOLDER).map_err(err)?;
        let directory = root.open_dir(FOLDER).map_err(err)?;
        write_archive(&directory, EARLIER, &bytes, None)?;
        let parent = backup_archive::Chain::new(backup_archive::Archive::open(
            directory.open(EARLIER).map_err(err)?.into_std(),
        )?);
        bytes[MARKER_AT..MARKER_AT + LATEST_MARKER.len()].copy_from_slice(LATEST_MARKER);
        write_archive(&directory, LATEST, &bytes, Some(&parent))?;
        online(B)?;
        operation(B, partitions::Action::Initialize {})?;
        for (offset, size) in OFFSETS.into_iter().zip(SIZES) {
            operation(
                B,
                partitions::Action::Create {
                    offset,
                    size,
                    kind: partitions::PartitionKind::Linux,
                },
            )?;
        }
    }
    if reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("Guest OS layout changed during fixture preparation".into());
    }
    let partitions = target_entries()?;
    let mut target = disk_b(false)?;
    let mut buffer = vec![0; MIB as usize];
    for (offset, size) in OFFSETS.into_iter().zip(SIZES) {
        let mut view = data_io::Region::new(&target, offset, size).map_err(err)?;
        for _ in 0..size / MIB {
            view.read_exact(&mut buffer).map_err(err)?;
            if buffer.iter().any(|&byte| byte != 0) {
                return Err("A Btrfs restore target is no longer blank".into());
            }
        }
    }
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let directory = root.open_dir(FOLDER).map_err(err)?;
    let chain = open_chain(&directory)?;
    let earlier = chain.headers()[1].sha256.clone();
    let chain = std::sync::Arc::new(std::sync::Mutex::new(chain));
    let mut current = backup_archive::SharedReader::at(chain.clone(), 0)?;
    if marker(&mut current, MARKER_AT as u64)? != *LATEST_MARKER {
        return Err("Incremental archive marker differs".into());
    }
    let mut original = backup_archive::SharedReader::at(chain, 1)?;
    let original_marker = marker(&mut original, MARKER_AT as u64)?;
    if original_marker == *LATEST_MARKER
        || disk_core::btrfs::plan_resize(&mut original, MOVE_TARGET)?.relocation_bytes()
            != MOVE_BYTES
    {
        return Err("Earlier image or relocation case differs".into());
    }
    let baseline = Baseline {
        tables: ntfs_reconstruction_checks::table_hashes()?,
        archive_files: archive_digests(&directory)?,
        earlier,
        contents: move_contents(&mut original)?,
        identity: marker(&mut original, 65536 + 32)?,
        original_marker,
        edges: edges(&mut target)?,
    };
    let mut out = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt(&desktop))
        .map_err(err)?;
    serde_json::to_writer(&mut out, &baseline).map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok(format!("PASS: Btrfs archive chain at {}. Open Latest with Earlier dependency. Use Earlier for B/{} 384 MiB exact and B/{} 192 MiB resize. Use Latest for B/{} 448 MiB resize.", path.join(FOLDER).display(), partitions[0], partitions[1], partitions[2]))
}

pub(in super::super) fn verify() -> Result<String, String> {
    let desktop = desktop()?;
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    target_entries()?;
    let baseline: Baseline = serde_json::from_reader(
        File::open(receipt(&desktop))
            .map_err(err)?
            .take(1024 * 1024),
    )
    .map_err(err)?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let directory = root.open_dir(FOLDER).map_err(err)?;
    let mut file = disk_b(true)?;
    if baseline.tables != ntfs_reconstruction_checks::table_hashes()?
        || baseline.archive_files != archive_digests(&directory)?
        || baseline.edges != edges(&mut file)?
    {
        return Err("Archive files, GPTs, guest OS layout or neighboring sectors changed".into());
    }
    let mut digests = Vec::new();
    for (index, (offset, size)) in OFFSETS.into_iter().zip(SIZES).enumerate() {
        let mut view = data_io::Region::new(&file, offset, size).map_err(err)?;
        let expected = if index == 2 {
            *LATEST_MARKER
        } else {
            baseline.original_marker
        };
        if marker(&mut view, MARKER_AT as u64)? != expected
            || marker(&mut view, 65536 + 32)? != baseline.identity
            || move_contents(&mut view)? != baseline.contents
        {
            return Err("Restored point marker, UUID or files failed readback".into());
        }
        disk_core::btrfs::plan_resize(&mut view, if index == 1 { MOVE_SIZE } else { MOVE_TARGET })?;
        view.rewind().map_err(err)?;
        let digest = data_io::hash_exact(&mut view, size, |_| Ok(()))?;
        if index == 0 && digest != baseline.earlier {
            return Err("Exact restore differs from the earlier point digest".into());
        }
        digests.push(format!("{} MiB {digest}", size / MIB));
    }
    Ok(format!("PASS: earlier exact and shrink, latest growth. All 51 files, UUID, point markers, adjacent sectors, GPTs and archives verified. B offline.\n{}", digests.join("\n")))
}

//! Isolated Ext partition restore and clone fixtures using known upstream images.
use super::*;
use cap_std::fs::{Dir, OpenOptions as DirOptions};
use capacity_restore_checks::{archive_hash, hash, region};
use disk_core::MIB;
use partitions::{Action, FileSystem, PartitionKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};

const SECTOR: u64 = 512;
const FOLDER: &str = "DS-ExtCopy";
const RESTORE: [u64; 3] = [32 * MIB, 160 * MIB, 288 * MIB];
const CLONE: [u64; 2] = [2304 * MIB, 2432 * MIB];
const SIZES: [u64; 3] = [96 * MIB, 96 * MIB, 64 * MIB];
const NAMES: [&str; 3] = ["Ext2.dsbackup", "Ext3.dsbackup", "Ext4.dsbackup"];
const SOURCES: [&[u8]; 3] = [
    include_bytes!("../../../../tests/fixtures/test_disk_ext2.bin.zst"),
    include_bytes!("../../../../tests/fixtures/test_disk_ext3.bin.zst"),
    include_bytes!("../../../../tests/fixtures/test_disk1.bin.zst"),
];

#[derive(Serialize, Deserialize)]
struct Extent {
    digest: String,
    uuid: [u8; 16],
    retained: String,
    copied: u64,
}

#[derive(Serialize, Deserialize)]
struct Witness {
    layouts: [String; 3],
    archive_hashes: [String; 3],
    extents: [Extent; 3],
    edges_a: Vec<[String; 2]>,
    edges_b: Vec<[String; 2]>,
}

fn baseline(desktop: &str) -> PathBuf {
    PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-ext-copy-{desktop}.json"
    ))
}

fn disk(serial: &str, offline: bool) -> Result<fs::File, String> {
    let selected = selection(serial, None)?;
    let resolved = data_windows::resolve(&selected, false, false)?;
    if resolved.size != 16 * GIB
        || resolved.disk.logical_sector_size != Some(SECTOR)
        || resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(serial)
        || resolved.disk.is_offline != Some(offline)
    {
        return Err("Unexpected Ext fixture device or offline state".into());
    }
    let file = data_windows::device(&resolved, false)?;
    if offline {
        data_windows::verify_offline_handle(&resolved, &file)?;
    }
    Ok(file)
}

fn entries(serial: &str, offsets: &[u64], sizes: &[u64]) -> Result<Vec<u32>, String> {
    let layout = fat_resize_checks::model(serial)?;
    let expected_count = (if serial == A { 2 } else { 1 }) + offsets.len();
    if layout.entries.len() != expected_count {
        return Err("Unexpected Ext fixture layout".into());
    }
    offsets
        .iter()
        .zip(sizes)
        .map(|(&offset, &size)| {
            if !layout.entries.iter().any(|part| {
                part.kind == partitions::policy::LINUX_DATA
                    && part.attributes == 0
                    && part.offset == offset
                    && part.size == size
            }) {
                return Err("Missing exact ordinary Linux data entry".into());
            }
            Ok(
                reallocation_checks::metadata_partition(serial, None, offset / MIB, size / MIB)?
                    .number,
            )
        })
        .collect()
}

fn create_targets(serial: &str, offsets: &[u64], sizes: &[u64]) -> Result<Vec<u32>, String> {
    for (&offset, &size) in offsets.iter().zip(sizes) {
        operation(
            serial,
            Action::Create {
                offset,
                size,
                kind: PartitionKind::Linux,
            },
        )?;
    }
    entries(serial, offsets, sizes)
}

fn edges(file: &fs::File, offsets: &[u64], sizes: &[u64]) -> Result<Vec<[String; 2]>, String> {
    offsets
        .iter()
        .zip(sizes)
        .map(|(&offset, &size)| {
            Ok([
                hash(file, offset - SECTOR, SECTOR)?,
                hash(file, offset + size, SECTOR)?,
            ])
        })
        .collect()
}

fn archive(dir: &Dir, i: usize, disk_a: &Selection) -> Result<Extent, String> {
    let mut source = Cursor::new(zstd::stream::decode_all(SOURCES[i]).map_err(err)?);
    let size = SIZES[i];
    let cancel = AtomicBool::new(false);
    let uuid = source.get_ref()[1128..1144].try_into().unwrap();
    let map = used_space::inspect(&mut source, size, SECTOR, None, &cancel)?;
    if map.volumes[0].filesystem != ["Ext2", "Ext3", "Ext4"][i] || map.skipped == 0 {
        return Err("Expected an identified Ext allocation fixture".into());
    }
    map.proof.verify(&mut source, &cancel)?;
    let retained = map.retained_digest(&mut source, &cancel, |_, _| ())?;
    let mut file = dir
        .open_with(
            NAMES[i],
            DirOptions::new().read(true).write(true).create_new(true),
        )
        .map_err(err)?
        .into_std();
    let binding = data_windows::FileBinding::new(&file, &disk_a.target)?;
    let header = backup_archive::create_with_policy(
        &mut map.masked(source),
        &mut file,
        "partition",
        size,
        SECTOR,
        true,
        None,
        Some(backup_archive::AllocationPolicy {
            skipped_bytes: map.skipped,
            map_sha256: map.digest.clone(),
        }),
        |_| Ok(()),
    )?;
    file.sync_all().map_err(err)?;
    data_windows::FileBinding::validate_all(&[binding])?;
    Ok(Extent {
        digest: header.sha256,
        uuid,
        retained,
        copied: map.copied(),
    })
}

fn archives(dir: &Dir) -> Result<[String; 3], String> {
    Ok([
        archive_hash(dir, NAMES[0])?,
        archive_hash(dir, NAMES[1])?,
        archive_hash(dir, NAMES[2])?,
    ])
}

pub(super) fn prepare() -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let disks = inventory()?;
    let os = reallocation_checks::os_layout(&disks)?;
    for serial in [A, B] {
        whole_file_clone_checks::empty_raw(&disks, serial)?;
    }
    let path = baseline(&desktop);
    if path.try_exists().map_err(err)? {
        return Err("This Ext fixture build has already run".into());
    }
    let mut log = backup_protection::begin("ext-copy-512")?;
    online(A)?;
    operation(A, Action::Initialize {})?;
    fat_resize_checks::create_capacity_fixture(
        &mut log,
        (FileSystem::Ntfs, "DSEXTCOPY", 4096),
        32 * MIB,
        2 * GIB,
    )?;
    let clone = create_targets(A, &CLONE, &[SIZES[2]; 2])?;
    let (root, folder) = ntfs_reconstruction_checks::verified_a_root()?;
    root.create_dir(FOLDER).map_err(err)?;
    let dir = root.open_dir(FOLDER).map_err(err)?;
    let a = selection(A, None)?;
    let extents = [
        archive(&dir, 0, &a)?,
        archive(&dir, 1, &a)?,
        archive(&dir, 2, &a)?,
    ];
    online(B)?;
    operation(B, Action::Initialize {})?;
    let restore = create_targets(B, &RESTORE, &SIZES)?;
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    if layouts[2] != os {
        return Err("Guest OS layout changed".into());
    }
    let witness = Witness {
        layouts,
        archive_hashes: archives(&dir)?,
        extents,
        edges_a: edges(&disk(A, false)?, &CLONE, &[SIZES[2]; 2])?,
        edges_b: edges(&disk(B, false)?, &RESTORE, &SIZES)?,
    };
    let mut save = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    serde_json::to_writer(&mut save, &witness).map_err(err)?;
    save.sync_all().map_err(err)?;
    Ok(format!("PASS: {}. Restore Ext2 to B/{}, Ext3 to B/{}, Ext4 to B/{}. Then clone B/{} to A/{} by used space and A/{} by all sectors.", folder.join(FOLDER).display(), restore[0], restore[1], restore[2], restore[2], clone[0], clone[1]))
}

fn verify_extent(
    file: &fs::File,
    offset: u64,
    size: u64,
    i: usize,
    expected: &Extent,
) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(size as usize);
    region(file, offset, size)?
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected.digest
        || bytes[1128..1144] != expected.uuid
    {
        return Err("Ext byte stream or volume UUID differs from the archive".into());
    }
    let volume = ext4_view::Ext4::load(Box::new(bytes)).map_err(err)?;
    let (name, content) = if i == 1 {
        ("/medium_dir/42", "42")
    } else {
        ("/small_file", "hello, world!")
    };
    if volume.read_to_string(name).map_err(err)? != content {
        return Err("Ext sample bytes differ from the independent fixture expectation".into());
    }
    Ok(())
}

pub(super) fn verify(clones: bool) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let witness: Witness =
        serde_json::from_reader(fs::File::open(baseline(&desktop)).map_err(err)?).map_err(err)?;
    let a = disk(A, clones)?;
    let b = disk(B, true)?;
    if ntfs_reconstruction_checks::table_hashes()? != witness.layouts
        || edges(&a, &CLONE, &[SIZES[2]; 2])? != witness.edges_a
        || edges(&b, &RESTORE, &SIZES)? != witness.edges_b
    {
        return Err("GPT, OS layout or adjacent sectors changed".into());
    }
    for (i, offset) in RESTORE.into_iter().enumerate() {
        verify_extent(&b, offset, SIZES[i], i, &witness.extents[i])?;
    }
    let [ext2, ext3, ext4] = &witness.extents;
    if !clones {
        let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
        if archives(&root.open_dir(FOLDER).map_err(err)?)? != witness.archive_hashes {
            return Err("Source archive bytes have changed".into());
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(baseline(&desktop).with_extension("restore-pass"))
            .map_err(err)?;
        file.write_all(desktop.as_bytes()).map_err(err)?;
        file.sync_all().map_err(err)?;
        return Ok(format!("PASS: Restored Ext2, Ext3 and Ext4. Sample files, UUIDs, GPTs, archive files, edges and OS layout match. B stays offline.\nExt2 {}\nExt3 {}\nExt4 {}", ext2.digest, ext3.digest, ext4.digest));
    }
    if fs::read_to_string(baseline(&desktop).with_extension("restore-pass")).map_err(err)?
        != desktop
    {
        return Err("Missing independent restore acceptance before cloning".into());
    }
    for offset in CLONE {
        verify_extent(&a, offset, SIZES[2], 2, ext4)?;
    }
    Ok(format!("PASS: Both Ext4 clones, source and all restored partitions verified. Both disks remain offline. GPTs, edges and OS layout match.\nAll sectors {}\nUsed space {} bytes\n{}", ext4.digest, ext4.copied, ext4.retained))
}

//! Prepare and audit the isolated 4Kn encrypted whole-disk restore cases.
use super::*;
use cap_std::fs::{Dir, OpenOptions as DirOptions};
use disk_core::MIB;
use partitions::{policy, Action, FileSystem, PartitionKind};
use sha2::{Digest, Sha256};
use std::io::{self, Read};

#[path = "../../../../tests/fixtures/fat_view.rs"]
mod fat_fixture;

const SIZE: u64 = 16 * GIB;
const SECTOR: u64 = 4096;
const PART_SIZE: u64 = 8 * MIB;
const EARLIER_START: u64 = 16 * MIB;
const LATEST_START: u64 = 32 * MIB;
const FOLDER: &str = "DS-ArchiveRestore";
const ARCHIVE_SET: &str = "ArchiveSet";
const ARCHIVES: [&str; 2] = ["Earlier.dsbackup", "Latest.dsbackup"];
const PASSWORDS: [&str; 2] = ["DiskStudio-VM-Base-20", "DiskStudio-VM-Delta-20"];

#[derive(serde::Serialize, Deserialize)]
struct Witness {
    desktop: String,
    layouts: [String; 3],
    archives: [String; 2],
    edges: [String; 2],
    partition: String,
    latest_disk: String,
}

struct DiskStream {
    position: u64,
    extents: Vec<(u64, Vec<u8>)>,
}
impl Read for DiskStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = (buffer.len() as u64).min(SIZE - self.position) as usize;
        let end = self.position + length as u64;
        let buffer = &mut buffer[..length];
        buffer.fill(0);
        for (offset, bytes) in &self.extents {
            let first = self.position.max(*offset);
            let last = end.min(*offset + bytes.len() as u64);
            if first < last {
                buffer[(first - self.position) as usize..(last - self.position) as usize]
                    .copy_from_slice(&bytes[(first - offset) as usize..(last - offset) as usize]);
            }
        }
        self.position = end;
        Ok(length)
    }
}

fn fixture(latest: bool) -> Result<DiskStream, String> {
    let offset = if latest { LATEST_START } else { EARLIER_START };
    let gpt = table_restore::new_gpt(
        SIZE,
        SECTOR,
        format!("{:x}", Sha256::digest(b"4Kn encrypted restore fixture")),
        uuid::Uuid::from_u128(0x2b12_4715_802a_4178_9a04_ed19_2730_8933),
        &[table_restore::Entry {
            slot: 1,
            guid: "220D8D6B-4F82-4319-95CB-47D44F686D70".into(),
            kind: policy::BASIC_DATA.into(),
            offset,
            size: PART_SIZE,
            attributes: 0,
            name: "ARCHIVE-DATA".encode_utf16().collect(),
        }],
    )?;
    let mut extents = gpt
        .extents
        .into_iter()
        .map(|mut extent| {
            if latest && extent.kind == table_archive::Kind::PrimaryHeader {
                extent.bytes[16] ^= 1;
            }
            (extent.offset, extent.bytes)
        })
        .collect::<Vec<_>>();
    extents.push((offset, fat_fixture::exfat(SECTOR as usize)));
    Ok(DiskStream {
        position: 0,
        extents,
    })
}

fn write_archive(
    dir: &Dir,
    index: usize,
    mut source: DiskStream,
    parent: Option<&backup_archive::Chain>,
) -> Result<(backup_archive::Archive, String), String> {
    use backup_encryption::{Password, Writer};

    let file = dir
        .open_with(
            ARCHIVES[index],
            DirOptions::new().create_new(true).read(true).write(true),
        )
        .map_err(err)?
        .into_std();
    let password = Password::new(PASSWORDS[index].into()).map_err(err)?;
    let mut output = Writer::new(file, &password).map_err(err)?;
    let header = backup_archive::create(
        &mut source,
        &mut output,
        "disk",
        SIZE,
        SECTOR,
        true,
        parent,
        |_| Ok(()),
    )?;
    let reader = output.into_reader().map_err(err)?;
    Ok((backup_archive::Archive::from_reader(reader)?, header.sha256))
}

fn archive_hashes(dir: &Dir) -> Result<[String; 2], String> {
    let hash = |name| {
        let mut file = dir.open(name).map_err(err)?.into_std();
        let size = file.metadata().map_err(err)?.len();
        data_io::hash_exact(&mut file, size, |_| Ok(()))
    };
    Ok([hash(ARCHIVES[0])?, hash(ARCHIVES[1])?])
}

fn target(offline: bool) -> Result<fs::File, String> {
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let disk = disks
        .into_iter()
        .find(|disk| disk.serial_number.as_deref().map(policy::clean) == Some(B))
        .ok_or("B is missing")?;
    let legacy = WMIConnection::new()
        .map_err(err)?
        .raw_query::<policy::LegacyIdentity>("SELECT Index,SerialNumber FROM Win32_DiskDrive")
        .map_err(err)?;
    policy::corroborate(&disk, &legacy)?;
    if disk.is_offline != Some(offline) || disk.is_read_only != Some(false) {
        return Err("Unexpected state for test disk B".into());
    }
    // The disk-only case deliberately overwrites B with a damaged GPT. Verify
    // the device identity and extent without trusting the resulting table.
    let resolved = data_windows::Resolved {
        disk,
        size: SIZE,
        offset: 0,
        label: String::new(),
        volumes: vec![],
        ntfs_volume: false,
        partition_ranges: vec![],
        data_lock_proof: None,
    };
    let file = data_windows::device(&resolved, false)?;
    if offline {
        data_windows::verify_offline_handle(&resolved, &file)?;
    }
    Ok(file)
}

fn hash_range(file: &fs::File, offset: u64, size: u64) -> Result<String, String> {
    let mut region = data_io::Region::new(file, offset, size).map_err(err)?;
    data_io::hash_exact(&mut region, size, |_| Ok(()))
}

fn edges(file: &fs::File) -> Result<[String; 2], String> {
    Ok([
        hash_range(file, EARLIER_START - SECTOR, SECTOR)?,
        hash_range(file, EARLIER_START + PART_SIZE, SECTOR)?,
    ])
}

pub(super) fn prepare(continued: bool) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let [base_source, latest_source] = [fixture(false)?, fixture(true)?];
    let disks = inventory()?;
    fat_sector_checks::confirm_4kn(&disks)?;
    let os = reallocation_checks::os_layout(&disks)?;
    whole_file_clone_checks::empty_raw(&disks, B)?;
    if continued {
        let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
        let staged = root.open_dir(FOLDER).map_err(err)?;
        let partial = staged.symlink_metadata(ARCHIVES[0]).map_err(err)?;
        let layout = fat_resize_checks::model(A)?;
        if !partial.is_file()
            || partial.len() != 0
            || staged.entries().map_err(err)?.count() != 1
            || layout.entries.len() != 2
            || !layout.entries.iter().any(|entry| {
                entry.kind == policy::BASIC_DATA
                    && entry.offset == 32 * MIB
                    && entry.size == 2 * GIB
            })
        {
            return Err("The first preparation did not stop at the expected empty archive".into());
        }
        backup_protection::begin("archive-retained-restore-4kn-continue")?;
    } else {
        whole_file_clone_checks::empty_raw(&disks, A)?;
        let mut log = backup_protection::begin("archive-retained-restore-4kn")?;
        online(A)?;
        operation(A, Action::Initialize {})?;
        fat_resize_checks::create_capacity_fixture(
            &mut log,
            (FileSystem::Ntfs, "DSARCHIVE", SECTOR as u32),
            32 * MIB,
            2 * GIB,
        )?;
    }
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    if !continued {
        root.create_dir(FOLDER).map_err(err)?;
    }
    let staged = root.open_dir(FOLDER).map_err(err)?;
    staged.create_dir(ARCHIVE_SET).map_err(err)?;
    let dir = staged.open_dir(ARCHIVE_SET).map_err(err)?;
    let (base, _) = write_archive(&dir, 0, base_source, None)?;
    let chain = backup_archive::Chain::new(base);
    let (_, latest_disk) = write_archive(&dir, 1, latest_source, Some(&chain))?;
    drop(chain);

    online(B)?;
    operation(B, Action::Initialize {})?;
    operation(
        B,
        Action::Create {
            offset: EARLIER_START,
            size: PART_SIZE,
            kind: PartitionKind::Windows,
        },
    )?;
    let raw_gpt = quick_partition_checks::table(B)?;
    let layout = table_restore::archive_layout(&raw_gpt)?;
    let entry = layout
        .entries
        .iter()
        .find(|entry| entry.offset == EARLIER_START && entry.size == PART_SIZE)
        .ok_or("Missing B test extent")?;
    let disk = data_windows::resolve(&selection(B, None)?, false, false)?.disk;
    let records = quick_partition_checks::parts()?;
    let selected = records
        .iter()
        .filter(|part| {
            part.disk_number == disk.number
                && part.offset == entry.offset
                && part.size == entry.size
        })
        .collect::<Vec<_>>();
    let [partition] = selected.as_slice() else {
        return Err("Ambiguous B target partition".into());
    };
    operation(
        B,
        Action::Format {
            partition: partition.partition_number,
            label: "RESTORE4KN".into(),
            file_system: FileSystem::Fat12,
            cluster_size: SECTOR as u32,
            options: Default::default(),
        },
    )?;
    let layout = ntfs_reconstruction_checks::table_hashes()?;
    if layout[2] != os {
        return Err("Guest OS partition layout changed".into());
    }
    let witness = Witness {
        desktop,
        layouts: layout,
        archives: archive_hashes(&dir)?,
        edges: edges(&target(false)?)?,
        partition: format!("{:x}", Sha256::digest(fat_fixture::exfat(SECTOR as usize))),
        latest_disk,
    };
    let mut file = dir.create("baseline.json").map_err(err)?;
    serde_json::to_writer(&mut file, &witness).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!(
        "PASS: open Latest with Earlier dependency at {}. Select Earlier and restore 8 MiB to B/{}. Verify Partition, then select Latest and restore the whole disk to B.",
        path.join(FOLDER).join(ARCHIVE_SET).display(),
        partition.partition_number,
    ))
}

pub(super) fn verify(whole: bool) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root
        .open_dir(FOLDER)
        .map_err(err)?
        .open_dir(ARCHIVE_SET)
        .map_err(err)?;
    let before: Witness =
        serde_json::from_reader(dir.open("baseline.json").map_err(err)?).map_err(err)?;
    let a = quick_partition_checks::table(A)?.encode()?;
    if before.desktop != desktop
        || before.archives != archive_hashes(&dir)?
        || before.layouts[0] != format!("{:x}", Sha256::digest(a))
        || before.layouts[2] != reallocation_checks::os_layout(&inventory()?)?
    {
        return Err("Desktop, source archives, A GPT or OS layout changed".into());
    }
    let file = target(true)?;
    if !whole {
        let actual = hash_range(&file, EARLIER_START, PART_SIZE)?;
        if actual != before.partition
            || before.layouts != ntfs_reconstruction_checks::table_hashes()?
            || before.edges != edges(&file)?
        {
            return Err("Selected partition, adjacent sectors or GPT differ".into());
        }
        return Ok(format!("PASS: 8 MiB selected partition SHA-256 {actual}; both GPTs and adjacent sectors unchanged. B remains offline."));
    }
    let actual = hash_range(&file, 0, SIZE)?;
    if actual != before.latest_disk
        || hash_range(&file, LATEST_START, PART_SIZE)? != before.partition
    {
        return Err("Whole B or the archived partition bytes differ".into());
    }
    Ok(format!("PASS: 16 GiB SHA-256 {actual}; damaged GPT and relocated partition bytes preserved. B remains offline. Archives, A GPT and OS layout unchanged."))
}

#[cfg(test)]
#[test]
fn in_memory_fixture_has_point_specific_partition_bounds_and_table_integrity() {
    use disk_core::table_check::{inspect_image_with_ebrs, CheckState};

    for latest in [false, true] {
        let mut source = fixture(latest).unwrap();
        let mut head = vec![0; MIB as usize];
        let mut tail = vec![0; MIB as usize];
        source.read_exact(&mut head).unwrap();
        source.position = SIZE - MIB;
        source.read_exact(&mut tail).unwrap();
        let result = inspect_image_with_ebrs(&head, &tail, SIZE, "fixture.img", &[]).unwrap();
        if latest {
            assert_eq!(result.report.state, CheckState::Failed);
        } else {
            assert_eq!(result.report.state, CheckState::Passed);
            let table = result.disk.unwrap();
            assert!(table
                .partitions
                .iter()
                .any(|part| part.start == EARLIER_START && part.size == PART_SIZE));
        }
    }
}

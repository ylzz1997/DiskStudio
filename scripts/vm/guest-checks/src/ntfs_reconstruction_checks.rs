//! Guest-only damaged NTFS fixture and readback against known original bytes.
use super::*;
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const START: u64 = 14 * GIB + 8 * MIB;
const SIZE: u64 = 2 * MIB;
const ROOT: &str = "DS-NTFS-Reconstruction";
const IMAGE: &str = "damaged-ntfs.img";
const WITNESS: &str = "baseline.json";
const SAMPLE: &str = "file-with-12345";
const GOOD: &[u8] = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");

fn damage() -> Vec<u8> {
    // This fixed fixture has 512 B clusters, MFT at byte 16384, and 1024 B records.
    let mut bytes = GOOD.to_vec();
    bytes[..512].fill(0);
    bytes[16384..16384 + 2 * 1024].fill(0);
    bytes
}

pub(super) fn verified_a_root() -> Result<(Dir, PathBuf), String> {
    let source = selection(A, Some(2))?;
    let resolved = data_windows::resolve(&source, true, false)?;
    if resolved.disk.is_offline != Some(false) || resolved.size != 2 * GIB || !resolved.ntfs_volume
    {
        return Err("Expected the original 2 GiB NTFS volume on online A".into());
    }
    let letter = quick_partition_checks::parts()?
        .into_iter()
        .find(|part| part.disk_number == resolved.disk.number && part.partition_number == 2)
        .and_then(|part| part.drive_letter)
        .filter(|letter| letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic())
        .ok_or("A/2 needs an assigned drive letter for the native dialogs")?;
    let folder = PathBuf::from(format!("{letter}:\\"));
    if data_windows::folder_location(&folder)?.1 != source.target {
        return Err("A/2 drive letter is mapped to a different disk".into());
    }
    let dir = Dir::open_ambient_dir(&folder, cap_std::ambient_authority()).map_err(err)?;
    let root = dir.try_clone().map_err(err)?.into_std_file();
    let [volume] = resolved.volumes.as_slice() else {
        return Err("Expected one A/2 volume".into());
    };
    data_windows::file_location(&root)?.confirm_partition(
        &source.target,
        &source.layout_key,
        volume,
        (resolved.disk.number, resolved.offset, resolved.size),
    )?;
    Ok((dir, folder))
}

fn output_root() -> Result<(Dir, PathBuf), String> {
    verified_a_root().map(|(dir, folder)| (dir, folder.join(ROOT)))
}

fn read(file: &mut std::fs::File, offset: u64, size: usize) -> Result<Vec<u8>, String> {
    if offset < START - 512
        || offset
            .checked_add(size as u64)
            .is_none_or(|end| end > START + SIZE + 512)
    {
        return Err("Read outside the dedicated fixture and its boundary sectors".into());
    }
    file.seek(SeekFrom::Start(offset)).map_err(err)?;
    let mut bytes = vec![0; size];
    for chunk in bytes.chunks_mut(128 * 1024) {
        file.read_exact(chunk).map_err(err)?;
    }
    Ok(bytes)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn table_hashes() -> Result<[String; 3], String> {
    Ok([
        hash(&quick_partition_checks::table(A)?.encode()?),
        hash(&quick_partition_checks::table(B)?.encode()?),
        reallocation_checks::os_layout(&inventory()?)?,
    ])
}

fn witness(file: &mut std::fs::File, part: u32) -> Result<serde_json::Value, String> {
    let disks = inventory()?;
    reallocation_checks::state(&disks, B, true)?;
    let (offset, size) = (START / MIB, SIZE / MIB);
    if reallocation_checks::metadata_partition(B, None, offset, size)?.number != part
        || read(file, START, SIZE as usize)? != damage()
    {
        return Err("The offline damaged B partition was modified".into());
    }
    Ok(
        serde_json::json!({"partition":part,"tables":table_hashes()?,"before":hash(&read(file, START-512, 512)?),
        "after":hash(&read(file, START+SIZE, 512)?),"damagedSha256":hash(&damage())}),
    )
}

pub(super) fn prepare() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (a_root, _) = output_root()?;
    if a_root.symlink_metadata(ROOT).is_ok() {
        return Err("The isolated fixture folder already exists".into());
    }
    let a_table = quick_partition_checks::table(A)?;
    let a_layout = table_restore::archive_layout(&a_table)?;
    let before = table_restore::archive_layout(&quick_partition_checks::table(B)?)?;
    let os_before = reallocation_checks::os_layout(&inventory()?)?;
    if a_layout.guid == before.guid
        || a_layout
            .entries
            .iter()
            .any(|a| before.entries.iter().any(|b| a.guid == b.guid))
    {
        return Err("A and B have conflicting GPT identifiers".into());
    }
    if START / 512 <= before.first
        || (START + SIZE) / 512 > before.last
        || before
            .entries
            .iter()
            .any(|e| e.offset < START + SIZE + 512 && START - 512 < e.offset + e.size)
    {
        return Err("The dedicated B range or its boundary sectors are occupied".into());
    }
    let b = selection(B, None)?;
    let mut file = data_windows::device(&data_windows::resolve(&b, false, false)?, false)?;
    if read(&mut file, START, SIZE as usize)?
        .iter()
        .any(|&b| b != 0)
    {
        return Err("The dedicated B range is not empty".into());
    }
    let adjacent = [
        read(&mut file, START - 512, 512)?,
        read(&mut file, START + SIZE, 512)?,
    ];
    drop(file);
    online(B)?;
    operation(
        B,
        partitions::Action::Create {
            offset: START,
            size: SIZE,
            kind: partitions::PartitionKind::Windows,
        },
    )?;
    let fixture = Fixture::registered()?;
    let mut retained = fixture.layout.clone();
    retained.entries.retain(|entry| entry.offset != START);
    if !retained.agrees(&before)
        || fixture.adjacent != adjacent
        || fixture.a_hash != hash(&a_table.encode()?)
        || fixture.os != os_before
    {
        return Err("B GPT changed outside the new test entry".into());
    }
    fixture.seed()
}

struct Fixture {
    a_root: Dir,
    output: PathBuf,
    selection: Selection,
    partition: u32,
    ctx: partitions::platform::TableReadContext,
    layout: table_restore::Layout,
    a_hash: String,
    b_hash: String,
    os: String,
    adjacent: [Vec<u8>; 2],
}

impl Fixture {
    fn registered() -> Result<Self, String> {
        let (a_root, output) = output_root()?;
        if a_root.symlink_metadata(ROOT).is_ok() {
            return Err("The isolated fixture folder already exists".into());
        }
        let selected = selection(B, None)?;
        let (ctx, _, _) = partitions::platform::table_restore_context(&selected, None)?;
        let a = quick_partition_checks::table(A)?;
        let b = quick_partition_checks::table(B)?;
        let layout = table_restore::archive_layout(&b)?;
        let a_layout = table_restore::archive_layout(&a)?;
        if layout.guid == a_layout.guid
            || layout.entries.iter().any(|entry| {
                a_layout
                    .entries
                    .iter()
                    .any(|other| entry.guid == other.guid)
            })
        {
            return Err("A and B have conflicting GPT identifiers".into());
        }
        let mut target = ctx
            .parts
            .iter()
            .filter(|part| part.offset == START && part.size == SIZE);
        let part = target
            .next()
            .ok_or("The dedicated B partition is missing")?;
        if target.next().is_some()
            || part.is_boot != Some(false)
            || part.is_system != Some(false)
            || !layout.entries.iter().any(|e| {
                e.offset == START
                    && e.size == SIZE
                    && e.kind == partitions::policy::BASIC_DATA
                    && e.attributes == 0
            })
            || layout
                .entries
                .iter()
                .filter(|e| e.offset != START)
                .any(|e| e.offset < START + SIZE + 512 && START - 512 < e.offset + e.size)
        {
            return Err("The dedicated B test partition or its neighboring range differs".into());
        }
        let partition = part.partition_number;
        let mut file = data_windows::device(&ctx.resolved, false)?;
        if read(&mut file, START, SIZE as usize)?
            .iter()
            .any(|&byte| byte != 0)
        {
            return Err("The registered B test partition is not entirely zero".into());
        }
        let adjacent = [
            read(&mut file, START - 512, 512)?,
            read(&mut file, START + SIZE, 512)?,
        ];
        Ok(Self {
            a_root,
            output,
            selection: selected,
            partition,
            ctx,
            layout,
            a_hash: hash(&a.encode()?),
            b_hash: hash(&b.encode()?),
            os: reallocation_checks::os_layout(&inventory()?)?,
            adjacent,
        })
    }

    fn seed(self) -> Result<String, String> {
        let Self {
            a_root,
            output,
            selection: selected,
            partition: part,
            ctx,
            layout,
            a_hash,
            b_hash,
            os,
            adjacent,
        } = self;
        let locks = data_windows::lock(&ctx.resolved)?;
        let (fresh, _, _) =
            partitions::platform::table_restore_context(&selected, Some((&ctx, &locks)))?;
        let mut file = data_windows::exclusive_device(&fresh.resolved)?;
        data_windows::offline(&file)?;
        data_windows::confirm_writable_offline(&file)?;
        let native =
            extfs::restore_native(&file, Some(&fresh.parts.iter().collect::<Vec<_>>()), 512)?.0;
        if !layout.agrees(&native)
            || read(&mut file, START, SIZE as usize)?
                .iter()
                .any(|&b| b != 0)
        {
            return Err("B fixture changed before seeding".into());
        }
        file.seek(SeekFrom::Start(START)).map_err(err)?;
        let specimen = damage();
        for chunk in specimen.chunks(128 * 1024) {
            if file.write(chunk).map_err(err)? != chunk.len() {
                return Err("Short fixture write. B remains offline.".into());
            }
        }
        file.sync_all().map_err(err)?;
        if read(&mut file, START, SIZE as usize)? != specimen
            || read(&mut file, START - 512, 512)? != adjacent[0]
            || read(&mut file, START + SIZE, 512)? != adjacent[1]
        {
            return Err("B fixture or neighboring data mismatch after seeding".into());
        }
        drop(file);
        drop(locks);
        let mut view =
            data_windows::device(&data_windows::resolve(&selected, false, false)?, false)?;
        let baseline = witness(&mut view, part)?;
        if baseline["tables"][0] != a_hash
            || baseline["tables"][2] != os
            || baseline["tables"][1] != b_hash
        {
            return Err("Layout changed before collecting witness".into());
        }
        a_root.create_dir(ROOT).map_err(err)?;
        let output_dir = a_root.open_dir(ROOT).map_err(err)?;
        let write_new = |name: &str, bytes: &[u8]| -> Result<(), String> {
            let mut f = output_dir
                .open_with(name, CapOptions::new().write(true).create_new(true))
                .map_err(err)?;
            f.write_all(bytes).map_err(err)?;
            f.sync_all().map_err(err)
        };
        write_new(IMAGE, &specimen)?;
        write_new(WITNESS, &serde_json::to_vec(&baseline).map_err(err)?)?;
        Ok(format!("PASS: B partition {part} is offline with a new {SIZE} B fixture at LBA {}. Primary boot and MFT records 0 and 1 are damaged. Export {SAMPLE} into {}. The ordinary image for preview is {}.", START/512, output.display(), output.join(IMAGE).display()))
    }
}

pub(super) fn inspect() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let fixture = Fixture::registered()?;
    Ok(format!("PASS: B/{} exact 2 MiB test extent at LBA {} is all zero. No writes. Both GPTs match native layouts and have distinct identifiers. B table SHA-256 {}. Output {}.",
        fixture.partition, START/512, fixture.b_hash, fixture.output.display()))
}

pub(super) fn seed_registered() -> Result<String, String> {
    btrfs_checks::desktop()?;
    Fixture::registered()?.seed()
}

pub(super) fn verify() -> Result<String, String> {
    btrfs_checks::desktop()?;
    let (root, _) = output_root()?;
    let directory = root
        .open_dir(ROOT)
        .map_err(|e| format!("Open fixture folder: {e}"))?;
    let saved = directory
        .open(WITNESS)
        .map_err(|e| format!("Open fixture witness: {e}"))?;
    let baseline: serde_json::Value = serde_json::from_reader(saved.take(4096)).map_err(err)?;
    let part = baseline["partition"]
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .ok_or("Witness partition invalid")?;
    let selected = selection(B, None)?;
    let mut b = data_windows::device(&data_windows::resolve(&selected, false, false)?, false)?;
    if witness(&mut b, part)? != baseline
        || directory
            .read(IMAGE)
            .map_err(|e| format!("Read damaged reference image: {e}"))?
            != damage()
    {
        return Err("Source bytes, partition tables, neighboring data or OS layout changed".into());
    }
    let mut found = 0;
    for entry in directory.entries().map_err(err)? {
        let name = entry.map_err(err)?.file_name();
        if !name.to_string_lossy().starts_with("DiskStudio-recovered-") {
            continue;
        }
        let out = directory
            .open_dir(&name)
            .map_err(|e| format!("Open export directory: {e}"))?;
        let report = out
            .open(recovery_export::REPORT)
            .map_err(|e| format!("Open export manifest: {e}"))?;
        let manifest: serde_json::Value =
            serde_json::from_reader(report.take(128 * 1024)).map_err(err)?;
        let proof = &manifest["ntfsReconstruction"];
        if manifest["completed"] != true
            || proof["backupBoot"] != true
            || proof["mirroredRecords"] != serde_json::json!([0, 1])
            || proof["bytes"] != SIZE
            || manifest["savedFiles"] != 1
            || manifest["failedFiles"] != 0
            || manifest["entries"][0]["originalName"] != SAMPLE
            || manifest["entries"][0]["folder"] != 0
            || manifest["outcomes"][0]["sha256"] != hash(b"12345")
        {
            return Err(
                "The exported file or manifest differs from the independent fixture".into(),
            );
        }
        let leaf = manifest["entries"][0]["name"]
            .as_str()
            .filter(|name| !name.contains(['/', '\\', ':']) && *name == data_io::safe_name(name))
            .ok_or("The manifest output name is invalid")?;
        if out
            .read(leaf)
            .map_err(|e| format!("Read exported file {leaf}: {e}"))?
            != b"12345"
        {
            return Err("Exported file bytes differ from the independent fixture".into());
        }
        found += 1;
    }
    if found != 1 {
        return Err("Expected exactly one export into the isolated fixture folder".into());
    }
    Ok(format!("PASS: 5-byte file and reconstruction manifest match. SHA-256 {}. Damaged B partition unchanged and offline. Both GPTs, adjacent sectors, image copy and OS layout unchanged.", hash(b"12345")))
}

use super::*;
use crate::{
    backup_archive::{self, Archive},
    recovery::{self, Candidate, ScanMode},
    recovery_tree::Catalog,
};
use sha2::{Digest, Sha256};
use std::{io::Cursor, sync::atomic::AtomicBool};
const NTFS: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
const MIB: usize = 1024 * 1024;

fn archive(bytes: &[u8], kind: &str, sector: u64, parent: Option<&Chain>) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    backup_archive::create(
        &mut &bytes[..],
        &mut out,
        kind,
        bytes.len() as u64,
        sector,
        true,
        parent,
        |_| Ok(()),
    )
    .unwrap();
    out.into_inner()
}
fn chain(bytes: Vec<u8>) -> Chain {
    Chain::new(Archive::from_reader(Cursor::new(bytes)).unwrap())
}
fn browse(bytes: &[u8], kind: &str, sector: u64) -> Browser {
    Browser::from_chain(
        chain(archive(bytes, kind, sector, None)),
        "memory.dsbackup".into(),
        |_, _| Ok(()),
    )
    .unwrap()
}
fn scan(browser: &Browser, part: u32) -> (Vec<Candidate>, Catalog) {
    let mut files = vec![];
    let mut catalog = Catalog::default();
    recovery::scan_filesystem_owned(
        browser.reader(part).unwrap(),
        ScanMode::AllFiles,
        Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| files.push(c),
        |d| catalog.insert(d),
    )
    .unwrap();
    (files, catalog)
}
fn extract(browser: &Browser, part: u32, file: &Candidate) -> Vec<u8> {
    let mut bytes = vec![];
    recovery::extract_owned(
        browser.reader(part).unwrap(),
        file,
        &mut bytes,
        file.size,
        Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    bytes
}
fn assert_ntfs(browser: &Browser, part: u32) {
    let (files, catalog) = scan(browser, part);
    let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
    assert_eq!(extract(browser, part, file), b"12345");
    assert!(!catalog
        .path(file.directory.as_deref().unwrap())
        .unwrap()
        .is_empty());
    assert!(browser.reader(u32::MAX).is_err());
}
fn legacy(bytes: &[u8]) -> Vec<u8> {
    let json = serde_json::to_vec(&serde_json::json!({"version":1,"kind":"partition","size":bytes.len(),"sector":512,"sha256":format!("{:x}",Sha256::digest(bytes))})).unwrap();
    let mut out = vec![0; 4096];
    out[..8].copy_from_slice(b"DSBK0001");
    out[8..12].copy_from_slice(&(json.len() as u32).to_le_bytes());
    out[12..12 + json.len()].copy_from_slice(&json);
    out.extend_from_slice(bytes);
    out
}

#[test]
fn btrfs_restore_hint_follows_the_selected_point_and_respects_windows_boot() {
    let mut bytes = vec![0; 128 * 1024];
    let magic = 65536 + 64;
    bytes[magic..magic + 8].copy_from_slice(b"_BHRfS_M");
    let previous = chain(archive(&bytes, "partition", 4096, None));
    let earlier = previous.header().id.clone();
    bytes[3..11].copy_from_slice(b"NTFS    ");
    let latest = Archive::from_reader(Cursor::new(archive(&bytes, "partition", 4096, Some(&previous)))).unwrap();
    let browser = Browser::from_chain(previous.prepend(latest).unwrap(), "test.dsbackup".into(), |_, _| Ok(())).unwrap();
    assert_eq!(browser.info.partitions[0].restore_format, Some(RestoreFormat::Ntfs));
    let older = browser.select_point(&earlier, |_, _| Ok(())).unwrap();
    assert_eq!(older.info.partitions[0].restore_format, Some(RestoreFormat::Btrfs));
    bytes[3..11].copy_from_slice(b"-FVE-FS-");
    assert_eq!(restore_format(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap(), None);
}

#[test]
fn v1_and_v2_partition_backups_browse_and_extract_real_ntfs_records_in_memory() {
    assert_ntfs(&browse(NTFS, "partition", 512), 0);
    let browser = Browser::from_legacy(
        Cursor::new(legacy(NTFS)),
        "legacy.dsbackup".into(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(browser.info.version, 1);
    assert_ntfs(&browser, 0);
    assert!(browser.restore_reader(None).is_err());
    assert_eq!(browser.restore_size(Some(0)).unwrap(), NTFS.len() as u64);
    let mut restored = Vec::new();
    browser.restore_reader(Some(0)).unwrap().read_to_end(&mut restored).unwrap();
    assert_eq!(restored, NTFS);
    assert_eq!(browser.verify_content(|_, _| Ok(())).unwrap(), format!("{:x}", Sha256::digest(NTFS)));
    let mut a = browser.reader(0).unwrap();
    let mut b = browser.reader(0).unwrap();
    a.seek(SeekFrom::Start(1024)).unwrap();
    let mut buf = [0; 512];
    b.read_exact(&mut buf).unwrap();
    assert_eq!(buf, NTFS[..512]);
    a.read_exact(&mut buf).unwrap();
    assert_eq!(buf, NTFS[1024..1536]);
}

#[test]
fn compressed_incremental_readers_seek_across_chunks_independently_and_restore_exactly() {
    let mut content = vec![3; backup_archive::CHUNK * 2 + 512];
    let base = archive(&content, "partition", 512, None);
    let base_chain = chain(base.clone());
    content[backup_archive::CHUNK + 100] = 71;
    let delta = archive(&content, "partition", 512, Some(&base_chain));
    let mut layers = chain(delta.clone());
    assert!(SharedReader::new(Arc::new(Mutex::new(chain(delta.clone())))).is_err());
    assert!(Browser::from_chain(chain(delta), "missing".into(), |_, _| Ok(())).is_err());
    layers
        .add_parent(Archive::from_reader(Cursor::new(base)).unwrap())
        .unwrap();
    let retained = Arc::new(Mutex::new(layers));
    let mut a = SharedReader::new(retained.clone()).unwrap();
    let mut b = SharedReader::new(retained.clone()).unwrap();
    for at in [
        backup_archive::CHUNK - 100,
        backup_archive::CHUNK * 2 - 100,
        0,
        700,
        backup_archive::CHUNK + 100,
    ] {
        a.seek(SeekFrom::Start(at as u64)).unwrap();
        let mut got = vec![0; 512];
        a.read_exact(&mut got).unwrap();
        assert_eq!(got, content[at..at + 512]);
        assert_eq!(b.stream_position().unwrap(), 0);
    }
    let pos = a.stream_position().unwrap();
    assert!(a.seek(SeekFrom::Start(u64::MAX)).is_err());
    assert_eq!(a.stream_position().unwrap(), pos);
    assert!(b.seek(SeekFrom::Current(-1)).is_err());
    assert_eq!(b.seek(SeekFrom::End(0)).unwrap(), content.len() as u64);
    assert_eq!(b.read(&mut [0; 1]).unwrap(), 0);
    let mut locked = retained.lock().unwrap();
    let mut restored = vec![];
    locked.reader().read_to_end(&mut restored).unwrap();
    assert_eq!(restored, content);
    drop(locked);
    let browser = Browser::from_chain(
        Arc::try_unwrap({
            drop(a);
            drop(b);
            retained
        })
        .ok()
        .unwrap()
        .into_inner()
        .unwrap(),
        "delta".into(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(browser.info.layers, 2);
}

fn mbr_partition(bytes: &mut [u8], table: usize, kind: u8, start: u32, length: u32) {
    bytes[table + 510..table + 512].copy_from_slice(&[0x55, 0xaa]);
    bytes[table + 450] = kind;
    bytes[table + 454..table + 458].copy_from_slice(&start.to_le_bytes());
    bytes[table + 458..table + 462].copy_from_slice(&length.to_le_bytes());
}
#[test]
fn whole_mbr_and_ebr_backups_expose_only_bounded_data_partitions() {
    for extended in [false, true] {
        let mut disk = vec![0; NTFS.len() + 4 * MIB];
        let start = if extended { 2 * MIB } else { MIB };
        disk[start..start + NTFS.len()].copy_from_slice(NTFS);
        if extended {
            mbr_partition(
                &mut disk,
                0,
                0x0f,
                (MIB / 512) as u32,
                ((NTFS.len() + 2 * MIB) / 512) as u32,
            );
            mbr_partition(
                &mut disk,
                MIB,
                7,
                (MIB / 512) as u32,
                (NTFS.len() / 512) as u32,
            );
        } else {
            mbr_partition(
                &mut disk,
                0,
                7,
                (start / 512) as u32,
                (NTFS.len() / 512) as u32,
            );
        }
        let browser = browse(&disk, "disk", 512);
        assert_eq!(browser.restore_size(None).unwrap(), disk.len() as u64);
        assert_eq!(browser.restore_size(Some(0)).unwrap(), NTFS.len() as u64);
        for (part, expected) in [(None, disk.as_slice()), (Some(0), NTFS)] {
            let mut restored = Vec::new();
            browser.restore_reader(part).unwrap().read_to_end(&mut restored).unwrap();
            assert_eq!(restored, expected);
        }
        assert_eq!(browser.info.partitions.len(), 1);
        assert_eq!(browser.info.partitions[0].start, start as u64);
        assert_ntfs(&browser, 0);
        let mut region = browser.reader(0).unwrap();
        assert_eq!(region.seek(SeekFrom::End(0)).unwrap(), NTFS.len() as u64);
        assert!(region.seek(SeekFrom::Start(NTFS.len() as u64 + 1)).is_err());
    }
}
fn crc(bytes: &[u8]) -> u32 {
    let mut c = u32::MAX;
    for b in bytes {
        c ^= *b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320u32 & 0u32.wrapping_sub(c & 1));
        }
    }
    !c
}
fn gpt(sector: usize) -> Vec<u8> {
    let mut bytes = vec![0; NTFS.len() + 4 * MIB];
    let last = (bytes.len() / sector - 1) as u64;
    mbr_partition(&mut bytes, 0, 0xee, 1, last as u32);
    let mut entries = vec![0; 16384];
    entries[..16].fill(1);
    entries[16..32].fill(2);
    entries[32..40].copy_from_slice(&((MIB / sector) as u64).to_le_bytes());
    entries[40..48].copy_from_slice(&(((MIB + NTFS.len()) / sector - 1) as u64).to_le_bytes());
    for (own, other, at) in [(1, last, 2), (last, 1, last - 16384 / sector as u64)] {
        bytes[at as usize * sector..at as usize * sector + entries.len()].copy_from_slice(&entries);
        let h = &mut bytes[own as usize * sector..(own as usize + 1) * sector];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&own.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + 16384 / sector as u64).to_le_bytes());
        h[48..56].copy_from_slice(&(last - 16384 / sector as u64 - 1).to_le_bytes());
        h[56..72].fill(3);
        h[72..80].copy_from_slice(&at.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&crc(&entries).to_le_bytes());
        let checksum = crc(&h[..92]);
        h[16..20].copy_from_slice(&checksum.to_le_bytes());
    }
    bytes[MIB..MIB + NTFS.len()].copy_from_slice(NTFS);
    bytes
}
#[test]
fn mbr_disk_backup_indexes_four_kn_ebr_ranges_and_preserves_boot_sectors() {
    let disk = crate::table_archive::tests::mbr_4kn();
    let browser = browse(&disk, "disk", 4096);
    let parts = &browser.info.partitions;
    assert_eq!(parts.len(), 4);
    assert_eq!(
        parts.iter().map(|part| part.start).collect::<Vec<_>>(),
        [
            MIB as u64 + 4096,
            2 * MIB as u64 + 4096,
            3 * MIB as u64 + 4096,
            6 * MIB as u64,
        ]
    );
    for part in parts {
        let mut stream = browser.reader(part.id).unwrap();
        let mut first = vec![0; 4096];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(first, disk[part.start as usize..part.start as usize + 4096]);
    }
    let wrong = browse(&disk, "disk", 512);
    assert!(wrong.info.partitions.is_empty() && wrong.info.table_issue.is_some());
}

#[test]
fn gpt_backup_geometry_is_validated_before_partitions_are_available() {
    for sector in [512, 4096] {
        let disk = gpt(sector);
        let browser = browse(&disk, "disk", sector as u64);
        assert_eq!(browser.info.partitions.len(), 1);
        assert_ntfs(&browser, 0);
        let mut damaged = disk.clone();
        damaged[sector + 16] ^= 1;
        let damaged_browser = browse(&damaged, "disk", sector as u64);
        assert!(damaged_browser.info.table_issue.is_some());
        assert!(damaged_browser.info.partitions.is_empty());
        assert!(damaged_browser.restore_size(Some(0)).is_err());
        assert!(damaged_browser.reader(0).is_err());
        let mut restored = Vec::new();
        damaged_browser
            .restore_reader(None)
            .unwrap()
            .read_to_end(&mut restored)
            .unwrap();
        assert_eq!(restored, damaged);

        let archive_sector = if sector == 4096 { 512 } else { 4096 };
        let mismatched = browse(&disk, "disk", archive_sector);
        assert_eq!(mismatched.info.sector, archive_sector);
        assert_eq!(
            mismatched.info.table_issue.as_deref(),
            Some("备份记录的扇区大小与分区表不一致")
        );
        assert!(mismatched.info.partitions.is_empty());
        assert!(mismatched.restore_reader(Some(0)).is_err());
        restored.clear();
        mismatched
            .restore_reader(None)
            .unwrap()
            .read_to_end(&mut restored)
            .unwrap();
        assert_eq!(restored, disk);
    }
}

#[test]
fn encrypted_disk_points_keep_verified_bytes_when_partition_indexing_fails() {
    use crate::backup_encryption::{Password, Reader, Writer};

    fn encrypted_disk(mut bytes: &[u8], secret: &str, parent: Option<&Chain>) -> Archive {
        let password = Password::new(secret.into()).unwrap();
        let mut encrypted = Writer::new(Cursor::new(Vec::new()), &password).unwrap();
        let size = bytes.len() as u64;
        backup_archive::create(
            &mut bytes,
            &mut encrypted,
            "disk",
            size,
            4096,
            true,
            parent,
            |_| Ok(()),
        )
        .unwrap();
        let mut encrypted = encrypted.into_inner().unwrap();
        encrypted.set_position(0);
        Archive::from_reader(Reader::open(encrypted, &password).unwrap()).unwrap()
    }

    let disk = gpt(4096);
    let parent = Chain::new(encrypted_disk(&disk, "Synthetic base password", None));
    let base_id = parent.header().id.clone();
    let mut damaged = disk.clone();
    damaged[4096 + 16] ^= 1;
    let child = encrypted_disk(&damaged, "Synthetic delta password", Some(&parent));
    let latest_id = child.header.id.clone();
    let latest =
        Browser::from_chain(parent.prepend(child).unwrap(), "encrypted".into(), |_, _| Ok(()))
            .unwrap();
    assert!(latest.info.table_issue.is_some());
    assert!(latest.restore_reader(Some(0)).is_err());
    assert_eq!(
        latest.verify_content(|_, _| Ok(())).unwrap(),
        format!("{:x}", Sha256::digest(&damaged)),
    );

    let base = latest.select_point(&base_id, |_, _| Ok(())).unwrap();
    assert!(base.info.table_issue.is_none());
    let mut restored = Vec::new();
    base.restore_reader(Some(0))
        .unwrap()
        .read_to_end(&mut restored)
        .unwrap();
    assert_eq!(restored, NTFS);

    let latest = base.select_point(&latest_id, |_, _| Ok(())).unwrap();
    assert!(latest.info.partitions.is_empty());
    assert!(latest.info.table_issue.is_some());
    restored.clear();
    latest
        .restore_reader(None)
        .unwrap()
        .read_to_end(&mut restored)
        .unwrap();
    assert_eq!(restored, damaged);
}

#[test]
fn corruption_cancellation_and_unknown_ipc_fields_never_publish_a_browser() {
    let mut bad = legacy(NTFS);
    bad[4096 + 123] ^= 1;
    assert!(Browser::from_legacy(Cursor::new(bad), "bad".into(), |_, _| Ok(())).is_err());
    let mut bad = archive(NTFS, "partition", 512, None);
    bad[4096] ^= 1;
    assert!(Browser::from_chain(chain(bad), "bad".into(), |_, _| Ok(())).is_err());
    let good = archive(NTFS, "partition", 512, None);
    assert!(
        Browser::from_chain(chain(good), "cancel".into(), |_, _| Err("cancel".into())).is_err()
    );
    assert!(
        Browser::from_legacy(Cursor::new(legacy(NTFS)), "cancel".into(), |_, _| Err(
            "cancel".into()
        ))
        .is_err()
    );
    assert!(
        serde_json::from_value::<crate::data_tasks::BackupScanRequest>(
            serde_json::json!({"id":"opaque","partition":0,"path":"ignored"})
        )
        .is_err()
    );
    let mut restore = serde_json::json!({
        "type": "restoreBackupPoint", "id": "session", "point": "point",
        "partition": 0, "destination": { "target": "a".repeat(64), "layoutKey": "bound", "partition": 1 },
        "resize": false
    });
    serde_json::from_value::<crate::data_tasks::Request>(restore.clone()).unwrap();
    restore["path"] = serde_json::json!("ignored");
    assert!(serde_json::from_value::<crate::data_tasks::Request>(restore).is_err());
}

#[test]
fn ext_files_remain_extractable_through_compressed_archive_random_access() {
    let disk =
        zstd::stream::decode_all(&include_bytes!("../../tests/fixtures/test_disk1.bin.zst")[..])
            .unwrap();
    let browser = browse(&disk, "partition", 512);
    assert_eq!(browser.info.partitions[0].restore_format, Some(RestoreFormat::Ext));
    let (files, _) = scan(&browser, 0);
    let file = files
        .iter()
        .find(|c| c.supported && c.size > 0 && c.size < 64 * 1024)
        .unwrap();
    let from_backup = extract(&browser, 0, file);
    let mut expected = vec![];
    recovery::extract_owned(
        Cursor::new(disk),
        file,
        &mut expected,
        file.size,
        Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(from_backup, expected);
    assert!(!expected.is_empty());
}

#[test]
fn three_points_keep_independent_readers_and_validate_each_logical_version() {
    let mut bytes = vec![3; backup_archive::CHUNK * 2 + 512];
    let base = archive(&bytes, "partition", 512, None);
    let mut layers = chain(base);
    let base_id = layers.header().id.clone();
    bytes[100] = 8;
    let middle = archive(&bytes, "partition", 512, Some(&layers));
    layers = layers
        .prepend(Archive::from_reader(Cursor::new(middle)).unwrap())
        .unwrap();
    let middle_id = layers.header().id.clone();
    bytes[backup_archive::CHUNK + 99] = 9;
    let newest = archive(&bytes, "partition", 512, Some(&layers));
    layers = layers
        .prepend(Archive::from_reader(Cursor::new(newest)).unwrap())
        .unwrap();
    let newest_id = layers.header().id.clone();
    let browser = Browser::from_chain(layers, "three".into(), |_, _| Ok(())).unwrap();
    assert_eq!(browser.info.points.len(), 3);
    let mut newest_reader = browser.reader(0).unwrap();
    let middle = browser.select_point(&middle_id, |_, _| Ok(())).unwrap();
    let mut middle_reader = middle.reader(0).unwrap();
    let first = middle.select_point(&base_id, |_, _| Ok(())).unwrap();
    assert_ne!(first.info.id, browser.info.id);
    assert_eq!(first.info.selected_point, base_id);
    assert_eq!(browser.info.selected_point, newest_id);
    assert_ne!(first.recovery_identity().unwrap(), browser.recovery_identity().unwrap());
    assert_eq!(first.recovery_identity().unwrap(), browser.select_point(&base_id, |_, _| Ok(())).unwrap().recovery_identity().unwrap());
    for (view, id, label) in [
        (&browser, &newest_id, "最新增量备份"),
        (&middle, &middle_id, "此前第 1 层增量备份"),
        (&first, &base_id, "最早完整备份"),
    ] {
        let source = view.scope_provenance(Some(0)).unwrap();
        assert_eq!(&source.point_id, id);
        assert_eq!(source.point_label, label);
        assert_eq!(source.archive_name, "three");
        assert!(view.scope_label(Some(0)).unwrap().starts_with("three / "));
        assert!(view.scope_provenance(Some(99)).is_err());
        let digest = view.verify_content(|_, _| Ok(())).unwrap();
        assert_eq!(
            digest,
            crate::data_io::hash_exact(
                &mut view.restore_reader(Some(0)).unwrap(),
                view.restore_size(Some(0)).unwrap(),
                |_| Ok(()),
            ).unwrap()
        );
    }
    for (reader, expected) in [
        (&mut newest_reader, (8, 9)),
        (&mut middle_reader, (8, 3)),
        (&mut first.reader(0).unwrap(), (3, 3)),
    ] {
        for (at, value) in [
            (100, expected.0),
            (backup_archive::CHUNK + 99, expected.1),
            (backup_archive::CHUNK * 2, 3),
        ] {
            reader.seek(SeekFrom::Start(at as u64)).unwrap();
            let mut got = [0];
            reader.read_exact(&mut got).unwrap();
            assert_eq!(got[0], value);
        }
    }
    assert!(browser.select_point("unknown", |_, _| Ok(())).is_err());
    assert!(browser
        .select_point(&base_id, |_, _| Err("cancel".into()))
        .is_err());
    assert_eq!(browser.info.selected_point, newest_id);
    assert!(
        serde_json::from_value::<crate::data_tasks::BackupPointRequest>(
            serde_json::json!({"id":"opaque","point":"opaque","path":"bad"})
        )
        .is_err()
    );
}

#[test]
fn opening_new_point_rejects_corrupt_superseded_parent_data() {
    let original = vec![3; backup_archive::CHUNK];
    let base = archive(&original, "partition", 512, None);
    let base_chain = chain(base.clone());
    let newer = vec![9; original.len()];
    let delta = archive(&newer, "partition", 512, Some(&base_chain));
    // The parent manifest/index remains intact; only its replaced payload is
    // corrupt. The newest point does not reference that old payload, but opening
    // the backup still requires every retained dependency to verify completely.
    let mut corrupt_base = base;
    corrupt_base[4096] ^= 1;
    let mut layers = chain(delta);
    layers
        .add_parent(Archive::from_reader(Cursor::new(corrupt_base)).unwrap())
        .unwrap();
    assert!(Browser::from_chain(layers, "corrupt-old".into(), |_, _| Ok(())).is_err());
}

#[path = "../../tests/fixtures/fat_view.rs"]
mod fat_fixture;

#[test]
fn encrypted_partition_backup_can_be_reverified_and_resized_for_restore() {
    use crate::backup_encryption::{Password, Reader, Writer};
    let source = fat_fixture::exfat(512);
    let size = source.len() as u64;
    let password = Password::new("Synthetic restore fixture".to_owned()).unwrap();
    let mut encrypted = Writer::new(Cursor::new(Vec::new()), &password).unwrap();
    backup_archive::create(
        &mut source.as_slice(), &mut encrypted, "partition", size, 512, true, None, |_| Ok(()),
    ).unwrap();
    let mut encrypted = encrypted.into_inner().unwrap();
    encrypted.set_position(0);
    let archive = Archive::from_reader(Reader::open(encrypted, &password).unwrap()).unwrap();
    let browser = Browser::from_chain(Chain::new(archive), "encrypted".into(), |_, _| Ok(())).unwrap();
    assert_eq!(browser.verify_content(|_, _| Ok(())).unwrap(), format!("{:x}", Sha256::digest(&source)));
    let mut reader = browser.restore_reader(Some(0)).unwrap();
    let grown = size + MIB as u64;
    let offset = 16 * MIB as u64;
    let plan = crate::fat_recovery::resize::clone::prepare(&mut reader, 512, grown, offset).unwrap();
    let mut view = crate::fat_recovery::resize::clone::Reader::new(&plan, reader).unwrap();
    let cancel = AtomicBool::new(false);
    let allocation = crate::used_space::inspect(&mut view, grown, 512, None, &cancel).unwrap();
    let mut restored = Vec::new();
    allocation.masked(view).read_to_end(&mut restored).unwrap();
    assert_eq!(restored.len(), grown as usize);
    assert_eq!(u64::from_le_bytes(restored[64..72].try_into().unwrap()), offset / 512);
    plan.verify(&mut Cursor::new(&restored)).unwrap();
    let candidates = crate::recovery::ScanMode::Existing;
    let mut found = Vec::new();
    let mut output = Cursor::new(restored);
    recovery::scan_filesystem(&mut output, candidates, |_, _| Ok(()), |entry| found.push(entry)).unwrap();
    let entry = found.iter().find(|entry| entry.name == "exfat-view.txt").unwrap();
    let mut content = Vec::new();
    recovery::extract(&mut output, entry, &mut content, entry.size, |_| Ok(())).unwrap();
    assert_eq!(content, fat_fixture::EXFAT_CONTENT);
}

#[test]
fn every_disk_restore_point_reparses_its_own_partition_table_and_bounds() {
    let mut before = vec![0; NTFS.len() + 4 * MIB];
    mbr_partition(
        &mut before,
        0,
        7,
        (MIB / 512) as u32,
        (NTFS.len() / 512) as u32,
    );
    before[MIB..MIB + NTFS.len()].copy_from_slice(NTFS);
    let base = chain(archive(&before, "disk", 512, None));
    let base_id = base.header().id.clone();
    let mut after = vec![0; before.len()];
    mbr_partition(
        &mut after,
        0,
        7,
        (2 * MIB / 512) as u32,
        (NTFS.len() / 512) as u32,
    );
    after[2 * MIB..2 * MIB + NTFS.len()].copy_from_slice(NTFS);
    let delta = archive(&after, "disk", 512, Some(&base));
    let browser = Browser::from_chain(
        base.prepend(Archive::from_reader(Cursor::new(delta)).unwrap())
            .unwrap(),
        "disk-points".into(),
        |_, _| Ok(()),
    )
    .unwrap();
    let old = browser.select_point(&base_id, |_, _| Ok(())).unwrap();
    assert_eq!(browser.info.partitions[0].start, (2 * MIB) as u64);
    assert_eq!(old.info.partitions[0].start, MIB as u64);
    assert_ntfs(&old, 0);
    assert_ntfs(&browser, 0);
}

#[test]
#[ignore = "Explicit ordinary-file fixture preparation for native VM UI acceptance"]
fn prepare_vm_restore_points() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".vm/payload/BackupPoints");
    std::fs::create_dir_all(&root).unwrap();
    let mut content = NTFS.to_vec();
    let base = archive(&content, "partition", 512, None);
    let mut layers = chain(base.clone());
    let mut archives = vec![("01-full.dsbackup", base)];
    for (name, replacement) in [
        ("02-middle.dsbackup", b"67890"),
        ("03-latest.dsbackup", b"ABCDE"),
    ] {
        let previous = if name.starts_with("02") {
            b"12345"
        } else {
            b"67890"
        };
        for at in 0..content.len() - 4 {
            if &content[at..at + 5] == previous {
                content[at..at + 5].copy_from_slice(replacement);
            }
        }
        let delta = archive(&content, "partition", 512, Some(&layers));
        layers = layers
            .prepend(Archive::from_reader(Cursor::new(delta.clone())).unwrap())
            .unwrap();
        archives.push((name, delta));
    }
    let browser = Browser::from_chain(layers, "points".into(), |_, _| Ok(())).unwrap();
    for (point, expected) in browser
        .info
        .points
        .iter()
        .zip([b"ABCDE", b"67890", b"12345"])
    {
        let view = browser.select_point(&point.id, |_, _| Ok(())).unwrap();
        let (files, _) = scan(&view, 0);
        let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
        assert_eq!(extract(&view, 0, file), expected);
    }
    use std::io::Write;
    for (name, bytes) in archives {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join(name))
            .unwrap();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
    }
}

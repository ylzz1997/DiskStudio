use super::*;
use crate::{
    recovery::{self, Candidate, ScanMode},
    recovery_tree::Catalog,
};
use std::{io::Cursor, sync::atomic::AtomicBool};
const NTFS: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
const MIB: usize = 1024 * 1024;
fn open(bytes: Vec<u8>, kind: Kind, sector: u64) -> Browser {
    Browser::open(
        Cursor::new(bytes),
        "memory.img".into(),
        Options { kind, sector },
        || Ok(()),
    )
    .unwrap()
}

#[test]
fn explicit_search_opens_an_image_with_no_table_and_finds_a_bounded_partition() {
    use crate::{lost_partitions, surface_scan::Options as SearchOptions};

    let mut bytes = vec![0; MIB];
    bytes.extend_from_slice(NTFS);
    bytes.resize(bytes.len() + MIB, 0);
    assert!(Browser::open(
        Cursor::new(bytes.clone()),
        "no-table.img".into(),
        Options { kind: Kind::Disk, sector: 512 },
        || Ok(()),
    ).is_err());
    let browser = open(bytes, Kind::Search, 512);
    assert!(browser.info.partitions.is_empty());
    assert!(browser.reader(0).is_err());
    let range = SearchOptions { start_sector: 1, sector_count: browser.info.size / 512 - 1 }
        .range(0, browser.info.size, 512).unwrap();
    let mut candidates = Vec::new();
    lost_partitions::scan_range(
        browser.logical_reader().unwrap(),
        range,
        &Arc::new(AtomicBool::new(false)),
        |_, candidate| {
            candidates.extend(candidate);
            Ok(())
        },
    ).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].offset, MIB as u64);
    assert!(candidates[0].can_browse);
}
fn scan(browser: &Browser, part: u32, mode: ScanMode) -> (Vec<Candidate>, Catalog) {
    let mut files = vec![];
    let mut catalog = Catalog::default();
    recovery::scan_filesystem_owned(
        browser.reader(part).unwrap(),
        mode,
        Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| files.push(c),
        |d| catalog.insert(d),
    )
    .unwrap();
    (files, catalog)
}
fn extract(browser: &Browser, part: u32, candidate: &Candidate) -> Vec<u8> {
    let mut bytes = vec![];
    recovery::extract_owned(
        browser.reader(part).unwrap(),
        candidate,
        &mut bytes,
        candidate.size,
        Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    bytes
}

#[test]
fn explicitly_opened_four_kn_raw_mbr_binds_the_selected_sector_geometry() {
    let raw = crate::table_archive::tests::mbr_4kn();
    let browser = open(raw.clone(), Kind::Disk, 4096);
    assert_eq!(browser.info.partitions.len(), 4);
    for (part, offset) in
        browser
            .info
            .partitions
            .iter()
            .zip([MIB + 4096, 2 * MIB + 4096, 3 * MIB + 4096, 6 * MIB])
    {
        assert_eq!(part.start, offset as u64);
        let mut first = vec![0; 4096];
        browser
            .reader(part.id)
            .unwrap()
            .read_exact(&mut first)
            .unwrap();
        assert_eq!(first, raw[offset..offset + 4096]);
    }
    assert!(Browser::open(
        Cursor::new(raw),
        "wrong.img".into(),
        Options {
            kind: Kind::Disk,
            sector: 512
        },
        || Ok(()),
    )
    .is_err());
}

#[test]
#[ignore = "requires the generated thin Storage Spaces recovery fixture"]
fn thin_storage_space_browses_and_extracts_through_a_checkpoint() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use disk_core::storage_spaces::Group;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    struct Segment {
        offset: u64,
        data: String,
        sha256: String,
    }
    #[derive(Deserialize)]
    struct Member {
        capacity: u64,
        segments: Vec<Segment>,
    }
    #[derive(Deserialize)]
    struct Expected {
        bytes: u64,
        sha256: String,
    }
    #[derive(Deserialize)]
    struct Fixture {
        members: [Member; 2],
        existing: Expected,
        deleted: Expected,
    }

    #[derive(Clone)]
    struct SparseImage {
        size: u64,
        position: u64,
        segments: Arc<BTreeMap<u64, Vec<u8>>>,
    }
    impl Read for SparseImage {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let mut length =
                (output.len() as u64).min(self.size.saturating_sub(self.position)) as usize;
            if let Some((&start, bytes)) = self.segments.range(..=self.position).next_back() {
                if let Some(remaining) = bytes
                    .get((self.position - start) as usize..)
                    .filter(|s| !s.is_empty())
                {
                    length = length.min(remaining.len());
                    output[..length].copy_from_slice(&remaining[..length]);
                    self.position += length as u64;
                    return Ok(length);
                }
            }
            if let Some((&next, _)) = self.segments.range(self.position..).next() {
                length = length.min((next - self.position) as usize);
            }
            output[..length].fill(0);
            self.position += length as u64;
            Ok(length)
        }
    }
    impl Seek for SparseImage {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.position = match position {
                SeekFrom::Start(position) => Some(position),
                SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
                SeekFrom::End(offset) => self.size.checked_add_signed(offset),
            }
            .ok_or(io::ErrorKind::InvalidInput)?;
            Ok(self.position)
        }
    }

    struct Factory(Vec<SparseImage>);
    impl ReaderFactory for Factory {
        fn reader(&self) -> Result<Box<dyn Source>, String> {
            let group = Group::open(self.0.clone(), 4096)?;
            let id = group
                .metadata()
                .spaces
                .values()
                .find(|space| space.name == "DS-TEST-THIN")
                .unwrap()
                .id;
            Ok(Box::new(group.into_reader(id, false)?))
        }
    }

    let path = std::env::var_os("DISKSTUDIO_SPACES_THIN_FIXTURE")
        .expect("set thin directory fixture JSON");
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let members = fixture
        .members
        .into_iter()
        .map(|member| SparseImage {
            size: member.capacity,
            position: 0,
            segments: Arc::new(
                member
                    .segments
                    .into_iter()
                    .map(|segment| {
                        let bytes = STANDARD.decode(&segment.data).unwrap();
                        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), segment.sha256);
                        (segment.offset, bytes)
                    })
                    .collect(),
            ),
        })
        .collect();
    let browser = Browser::from_factory(
        Arc::new(Factory(members)),
        "storage-space.raw".into(),
        Options {
            kind: Kind::Disk,
            sector: 4096,
        },
        "Storage Spaces".into(),
        vec![],
        String::new(),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(browser.info.partitions.len(), 1);
    let part = &browser.info.partitions[0];
    assert_eq!(part.start, 512 * MIB as u64 - 23 * 4096);
    let (files, _) = scan(&browser, part.id, ScanMode::AllFiles);
    assert_eq!(files.len(), 2);
    for (name, is_deleted, expected) in [
        ("WITNESS.BIN", false, fixture.existing),
        ("_RASED.BIN", true, fixture.deleted),
    ] {
        let file = files
            .iter()
            .find(|candidate| candidate.name == name)
            .unwrap();
        assert_eq!(file.deleted, is_deleted);
        assert_eq!(file.size, expected.bytes);
        let bytes = extract(&browser, part.id, file);
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), expected.sha256);
    }
}

fn assert_ntfs(browser: &Browser) {
    let (files, catalog) = scan(browser, 0, ScanMode::Existing);
    let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
    assert_eq!(extract(browser, 0, file), b"12345");
    assert!(!catalog
        .path(file.directory.as_deref().unwrap())
        .unwrap()
        .is_empty());
    assert!(files.iter().all(|c| !c.deleted));
    let (deleted, _) = scan(browser, 0, ScanMode::Deleted);
    assert!(deleted.iter().all(|c| c.deleted));
    let (all, _) = scan(browser, 0, ScanMode::AllFiles);
    // Existing-file browsing also exposes NTFS metadata records; recovery modes
    // deliberately skip those. Compare user candidates, not the raw counts.
    assert!(all
        .iter()
        .any(|c| c.name == "file-with-12345" && !c.deleted));
    for candidate in all.iter().filter(|c| !c.deleted) {
        assert!(files
            .iter()
            .any(|c| c.name == candidate.name && c.size == candidate.size));
    }
    assert_eq!(all.iter().filter(|c| c.deleted).count(), deleted.len());
}
fn mbr(bytes: &mut [u8], table: usize, kind: u8, start: u32, length: u32) {
    bytes[table + 510..table + 512].copy_from_slice(&[0x55, 0xaa]);
    bytes[table + 450] = kind;
    bytes[table + 454..table + 458].copy_from_slice(&start.to_le_bytes());
    bytes[table + 458..table + 462].copy_from_slice(&length.to_le_bytes());
}
fn gpt(sector: usize) -> Vec<u8> {
    let mut b = vec![0; NTFS.len() + 4 * MIB];
    let last = (b.len() / sector - 1) as u64;
    mbr(&mut b, 0, 0xee, 1, last as u32);
    let mut entries = vec![0; 16384];
    entries[..16].fill(1);
    entries[16..32].fill(2);
    entries[32..40].copy_from_slice(&((MIB / sector) as u64).to_le_bytes());
    entries[40..48].copy_from_slice(&(((MIB + NTFS.len()) / sector - 1) as u64).to_le_bytes());
    for (own, other, at) in [(1, last, 2), (last, 1, last - 16384 / sector as u64)] {
        b[at as usize * sector..at as usize * sector + entries.len()].copy_from_slice(&entries);
        let h = &mut b[own as usize * sector..(own as usize + 1) * sector];
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
        h[88..92].copy_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    b[MIB..MIB + NTFS.len()].copy_from_slice(NTFS);
    b
}
#[test]
fn partition_images_browse_filter_preview_and_extract_ntfs_without_device_io() {
    let browser = open(NTFS.to_vec(), Kind::Partition, 512);
    assert_ntfs(&browser);
    assert!(browser.reader(1).is_err());
    let mut first = browser.reader(0).unwrap();
    let mut second = browser.reader(0).unwrap();
    first.seek(SeekFrom::Start(3)).unwrap();
    let mut magic = [0; 8];
    first.read_exact(&mut magic).unwrap();
    assert_eq!(&magic, b"NTFS    ");
    assert_eq!(second.stream_position().unwrap(), 0);
    let mut same = vec![];
    second.read_to_end(&mut same).unwrap();
    assert_eq!(same, NTFS);
    assert!(first.seek(SeekFrom::Start(NTFS.len() as u64 + 1)).is_err());
    assert_eq!(first.seek(SeekFrom::End(0)).unwrap(), NTFS.len() as u64);
    assert_eq!(first.read(&mut magic).unwrap(), 0);
}

#[test]
fn partition_group_shares_one_parser_and_keeps_independent_bounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Factory {
        bytes: Arc<[u8]>,
        opens: AtomicUsize,
    }
    impl ReaderFactory for Factory {
        fn reader(&self) -> Result<Box<dyn Source>, String> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Cursor::new(self.bytes.clone())))
        }
    }
    let mut disk = vec![0; 8 * MIB];
    for index in 0..2 {
        let entry = 446 + index * 16;
        disk[entry + 4] = 0x8e;
        disk[entry + 8..entry + 12]
            .copy_from_slice(&(((index + 1) * MIB * 2 / 512) as u32).to_le_bytes());
        disk[entry + 12..entry + 16].copy_from_slice(&((MIB / 512) as u32).to_le_bytes());
        disk[(index + 1) * MIB * 2..(index + 1) * MIB * 2 + MIB].fill(index as u8 + 1);
    }
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    let factory = Arc::new(Factory {
        bytes: disk.into(),
        opens: AtomicUsize::new(0),
    });
    let browser = Browser::from_factory(
        factory.clone(),
        "group.raw".into(),
        Options { kind: Kind::Disk, sector: 512 },
        "RAW".into(),
        vec!["group.raw".into()],
        String::new(),
        || Ok(()),
    ).unwrap();
    assert!(browser.readers(&[0, 99]).is_err());
    let mut readers = browser.readers(&[1, 0]).unwrap();
    let mut byte = [0; 1];
    readers[0].seek(SeekFrom::End(-1)).unwrap();
    readers[1].read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
    readers[0].read_exact(&mut byte).unwrap();
    assert_eq!(byte, [2]);
    assert_eq!(readers[0].read(&mut byte).unwrap(), 0);
    assert!(readers[1].seek(SeekFrom::End(1)).is_err());
    assert_eq!(factory.opens.load(Ordering::Relaxed), 2);
}
#[test]
fn mbr_and_logical_partitions_use_independent_bounded_readers() {
    for extended in [false, true] {
        let mut disk = vec![0; NTFS.len() + 4 * MIB];
        let start = if extended { 2 * MIB } else { MIB };
        disk[start..start + NTFS.len()].copy_from_slice(NTFS);
        if extended {
            mbr(
                &mut disk,
                0,
                0x0f,
                (MIB / 512) as u32,
                ((NTFS.len() + 2 * MIB) / 512) as u32,
            );
            mbr(
                &mut disk,
                MIB,
                7,
                (MIB / 512) as u32,
                (NTFS.len() / 512) as u32,
            );
        } else {
            mbr(
                &mut disk,
                0,
                7,
                (MIB / 512) as u32,
                (NTFS.len() / 512) as u32,
            );
        }
        let browser = open(disk.clone(), Kind::Disk, 512);
        assert_eq!(browser.info.partitions.len(), 1);
        assert_eq!(browser.info.partitions[0].start, start as u64);
        assert_ntfs(&browser);
        assert!(Browser::open(
            Cursor::new(disk),
            "memory.img".into(),
            Options {
                kind: Kind::Disk,
                sector: 4096
            },
            || Ok(())
        )
        .is_err());
    }
}
#[test]
fn gpt_sector_sizes_crc_and_partition_extent_are_enforced() {
    for sector in [512, 4096] {
        let disk = gpt(sector);
        let browser = open(disk.clone(), Kind::Disk, sector as u64);
        assert_eq!(browser.info.partitions[0].start, MIB as u64);
        assert_ntfs(&browser);
        let options = Options {
            kind: Kind::Disk,
            sector: sector as u64,
        };
        let mut bad = disk.clone();
        bad[sector + 16] ^= 1;
        assert!(Browser::open(Cursor::new(bad), "bad.img".into(), options, || Ok(())).is_err());
        let mut bad = disk.clone();
        let at = bad.len() - sector + 88;
        bad[at] ^= 1;
        assert!(Browser::open(Cursor::new(bad), "bad.img".into(), options, || Ok(())).is_err());
        assert!(Browser::open(
            Cursor::new(disk),
            "wrong-sector.img".into(),
            Options {
                sector: if sector == 512 { 4096 } else { 512 },
                ..options
            },
            || Ok(())
        )
        .is_err());
    }
}
#[test]
fn ext_files_read_through_partition_region_and_keep_the_same_bytes() {
    for fixture in [
        &include_bytes!("../../tests/fixtures/test_disk1.bin.zst")[..],
        &include_bytes!("../../tests/fixtures/test_disk_ext2.bin.zst")[..],
        &include_bytes!("../../tests/fixtures/test_disk_ext3.bin.zst")[..],
    ] {
        let raw = zstd::stream::decode_all(fixture).unwrap();
        let browser = open(raw.clone(), Kind::Partition, 512);
        let (files, _) = scan(&browser, 0, ScanMode::Existing);
        let file = files
            .iter()
            .find(|c| c.supported && c.size > 0 && c.size < 64 * 1024)
            .unwrap();
        let actual = extract(&browser, 0, file);
        let mut expected = vec![];
        recovery::extract_owned(
            Cursor::new(raw),
            file,
            &mut expected,
            file.size,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!actual.is_empty());
        assert_eq!(actual, expected);
    }
}
#[test]
fn unknown_options_bad_geometry_and_cancellation_never_open_a_view() {
    assert!(
        serde_json::from_str::<Options>(r#"{"kind":"disk","sector":512,"path":"anything"}"#)
            .is_err()
    );
    for (size, sector) in [
        (1023, 512),
        (MIB as u64 + 1, 512),
        (MIB as u64, 1024),
        ((1 << 43) + 512, 512),
    ] {
        assert!(Options {
            kind: Kind::Disk,
            sector
        }
        .validate(size)
        .is_err());
    }
    assert!(Browser::open(
        Cursor::new(gpt(512)),
        "cancel.img".into(),
        Options {
            kind: Kind::Disk,
            sector: 512
        },
        || Err("cancel".into())
    )
    .is_err());
    // A raw filesystem must be explicitly opened as a partition, never guessed.
    assert!(Browser::open(
        Cursor::new(NTFS.to_vec()),
        "partition.img".into(),
        Options {
            kind: Kind::Disk,
            sector: 512
        },
        || Ok(())
    )
    .is_err());
}
#[test]
fn exported_manifests_include_image_source_in_complete_and_partial_results() {
    let browser = open(NTFS.to_vec(), Kind::Partition, 512);
    let (files, catalog) = scan(&browser, 0, ScanMode::Existing);
    let file = files
        .iter()
        .find(|c| c.name == "file-with-12345")
        .unwrap()
        .clone();
    let (_, mut plan) = crate::recovery_export::select(
        &files,
        &catalog,
        &[file.id],
        None,
        crate::recovery_export::Layout::Tree,
    )
    .unwrap();
    plan.image_source = Some(browser.provenance(0).unwrap());
    for completed in [false, true] {
        let report: serde_json::Value =
            serde_json::from_slice(&plan.manifest(&[], completed).unwrap()).unwrap();
        assert_eq!(report["imageSource"]["fileName"], "memory.img");
        assert_eq!(report["imageSource"]["offset"], "0");
        assert_eq!(report["imageSource"]["size"], NTFS.len().to_string());
        assert!(report.get("backupSource").is_none());
        assert_eq!(report["completed"], completed);
    }
}

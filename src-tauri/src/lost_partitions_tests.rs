use super::*;
use std::io::Cursor;
const NTFS: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
fn range(size: usize) -> Range {
    Range {
        offset: 0,
        size: size as u64,
        sector: 512,
    }
}
fn findings(bytes: Vec<u8>, sector: u64) -> Vec<Candidate> {
    let r = Range {
        sector,
        ..range(bytes.len())
    };
    let mut items = vec![];
    scan(
        &mut Cursor::new(bytes),
        r,
        &AtomicBool::new(false),
        |_, c| {
            if let Some(c) = c {
                items.push(c)
            }
            Ok(())
        },
    )
    .unwrap();
    items
}
#[test]
fn ntfs_without_partition_table_crosses_scan_blocks_and_revalidates_evidence() {
    let offset = BLOCK - 512;
    let mut bytes = vec![0; offset];
    bytes.extend_from_slice(NTFS);
    bytes.extend_from_slice(&[0; 4096]);
    let found = findings(bytes.clone(), 512);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].filesystem, "NTFS");
    assert_eq!(found[0].offset, offset as u64);
    assert!(found[0].size <= NTFS.len() as u64);
    let part =
        crate::data_io::Region::new(Cursor::new(bytes.clone()), found[0].offset, found[0].size)
            .unwrap();
    let mut files = vec![];
    crate::recovery::scan_filesystem_owned(
        part,
        crate::recovery::ScanMode::AllFiles,
        std::sync::Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| files.push(c),
        |_| Ok(()),
    )
    .unwrap();
    let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
    let mut actual = vec![];
    crate::recovery::extract_owned(
        crate::data_io::Region::new(Cursor::new(bytes.clone()), found[0].offset, found[0].size)
            .unwrap(),
        file,
        &mut actual,
        file.size,
        std::sync::Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(actual, b"12345");
    let r = range(bytes.len());
    revalidate(
        &mut Cursor::new(bytes.clone()),
        r,
        &found[0],
        &AtomicBool::new(false),
    )
    .unwrap();
    bytes[offset + 72] ^= 1;
    assert!(revalidate(
        &mut Cursor::new(bytes),
        r,
        &found[0],
        &AtomicBool::new(false)
    )
    .is_err());
}
#[test]
fn fat_types_are_derived_from_cluster_count_and_corrupt_tables_are_rejected() {
    use disk_core::fat_format::{FatType, Plan};
    for (kind, size, sector, expected) in [
        (FatType::Fat12, 2 * 1024 * 1024, 512, "FAT12"),
        (FatType::Fat16, 16 * 1024 * 1024, 512, "FAT16"),
        (FatType::Fat32, 64 * 1024 * 1024, 512, "FAT32"),
        (FatType::Fat12, 4 * 1024 * 1024, 4096, "FAT12"),
    ] {
        let plan = Plan::new(kind, size, sector, 0, "LOST", 1234, 0).unwrap();
        let mut io = Cursor::new(vec![0; size as usize]);
        plan.format(&mut io, |_, _| Ok(())).unwrap();
        let original = io.into_inner();
        let mut bytes = vec![0; sector as usize];
        bytes.extend_from_slice(&original);
        let found = findings(bytes.clone(), sector as u64);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].filesystem, expected);
        assert_eq!(found[0].size, size);
        let first = u16at(&original, 14) * sector as u64 + sector as u64;
        bytes[first as usize] = 0;
        assert!(probe(
            &mut Cursor::new(bytes.clone()),
            Range {
                sector: sector as u64,
                ..range(bytes.len())
            },
            sector as u64,
            &AtomicBool::new(false)
        )
        .unwrap()
        .is_none());
    }
}
fn exfat_bytes() -> Vec<u8> {
    let mut b = vec![0u8; 512 * 125];
    b[3..11].copy_from_slice(b"EXFAT   ");
    b[72..80].copy_from_slice(&125u64.to_le_bytes());
    b[80] = 24;
    b[84] = 1;
    b[88] = 25;
    b[92] = 100;
    b[96] = 2;
    b[105] = 1;
    b[108] = 9;
    b[110] = 1;
    b[510] = 0x55;
    b[511] = 0xaa;
    let mut sum = 0u32;
    for (i, v) in b[..11 * 512].iter().enumerate() {
        if ![106, 107, 112].contains(&i) {
            sum = sum.rotate_right(1).wrapping_add(*v as u32)
        }
    }
    for x in b[11 * 512..12 * 512].chunks_exact_mut(4) {
        x.copy_from_slice(&sum.to_le_bytes())
    }
    b[24 * 512..24 * 512 + 8].copy_from_slice(&[0xf8, 255, 255, 255, 255, 255, 255, 255]);
    b
}
#[test]
fn exfat_geometry_and_boot_checksum_bound_candidates() {
    let b = exfat_bytes();
    assert_eq!(findings(b.clone(), 512)[0].filesystem, "exFAT");
    for at in [72, 108, 11 * 512, 24 * 512] {
        let mut bad = b.clone();
        bad[at] ^= 1;
        assert!(findings(bad, 512).is_empty());
    }
}
#[test]
fn upstream_ext_filesystems_found_without_a_partition_table() {
    for (compressed, name) in [
        (
            &include_bytes!("../../tests/fixtures/test_disk_ext2.bin.zst")[..],
            "Ext2",
        ),
        (
            &include_bytes!("../../tests/fixtures/test_disk_ext3.bin.zst")[..],
            "Ext3",
        ),
        (
            &include_bytes!("../../tests/fixtures/test_disk1.bin.zst")[..],
            "Ext4",
        ),
    ] {
        let mut bytes = zstd::decode_all(compressed).unwrap();
        let found = probe(
            &mut Cursor::new(&bytes),
            range(bytes.len()),
            0,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.filesystem, name);
        bytes[1024 + 0x5a] = 1;
        assert!(probe(
            &mut Cursor::new(&bytes),
            range(bytes.len()),
            0,
            &AtomicBool::new(false)
        )
        .unwrap()
        .is_none());
    }
}
#[test]
fn selected_range_cancellation_errors_and_truncation_do_not_report_completion() {
    let mut bytes = vec![0; 4096];
    bytes.extend_from_slice(NTFS);
    let r = Range {
        offset: 4096,
        size: NTFS.len() as u64,
        sector: 512,
    };
    struct Bounded {
        cursor: Cursor<Vec<u8>>,
        low: u64,
        high: u64,
    }
    impl Seek for Bounded {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(p)
        }
    }
    impl Read for Bounded {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.cursor.position();
            assert!(pos >= self.low && pos + b.len() as u64 <= self.high);
            self.cursor.read(b)
        }
    }
    let mut source = Bounded {
        cursor: Cursor::new(bytes.clone()),
        low: r.offset,
        high: r.offset + r.size,
    };
    let mut completed = 0;
    scan(&mut source, r, &AtomicBool::new(false), |n, _| {
        completed = n;
        Ok(())
    })
    .unwrap();
    assert_eq!(completed, r.size);
    let stop = AtomicBool::new(true);
    assert!(scan(&mut source, r, &stop, |_, _| Ok(())).is_err());
    assert!(scan(
        &mut Cursor::new(vec![0; 512]),
        r,
        &AtomicBool::new(false),
        |_, _| Ok(())
    )
    .is_err());
    assert!(probe(
        &mut Cursor::new(bytes),
        Range { sector: 0, ..r },
        r.offset,
        &stop
    )
    .is_err());
    assert!(findings(NTFS[..NTFS.len() / 2].to_vec(), 512).is_empty());
}
#[test]
fn production_cache_is_bounded_before_search_and_candidate_validation() {
    let offset = BLOCK as u64 - 512;
    let mut bytes = vec![0; offset as usize];
    bytes.extend_from_slice(NTFS);
    bytes.extend_from_slice(&[0; 4096]);
    let disk_size = bytes.len() as u64;
    let range = Range {
        offset,
        size: NTFS.len() as u64,
        sector: 512,
    };
    struct Guard {
        cursor: Cursor<Vec<u8>>,
        low: u64,
        high: u64,
    }
    impl Seek for Guard {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(p)
        }
    }
    impl Read for Guard {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let p = self.cursor.position();
            assert!(
                p >= self.low && p + b.len() as u64 <= self.high,
                "raw cache escaped bounds"
            );
            self.cursor.read(b)
        }
    }
    let raw = Guard {
        cursor: Cursor::new(bytes.clone()),
        low: offset,
        high: offset + range.size,
    };
    let mut bounded = crate::data_io::Region::new(raw, range.offset, range.size).unwrap();
    let mut found = vec![];
    scan(
        &mut bounded,
        Range { offset: 0, ..range },
        &AtomicBool::new(false),
        |_, c| {
            if let Some(mut c) = c {
                c.offset += offset;
                found.push(c)
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    let raw = Guard {
        cursor: Cursor::new(bytes.clone()),
        low: offset,
        high: offset + found[0].size,
    };
    let mut reader = candidate_reader(
        raw,
        disk_size,
        512,
        range,
        &found[0],
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut actual = vec![];
    reader.read_to_end(&mut actual).unwrap();
    assert_eq!(actual.len(), found[0].size as usize);
    assert!(
        actual == NTFS[..found[0].size as usize],
        "bounded reader content changed"
    );
    assert!(candidate_reader(
        Cursor::new(bytes),
        disk_size,
        4096,
        range,
        &found[0],
        &AtomicBool::new(false)
    )
    .is_err());
}

use super::*;
use std::io::Cursor;
#[path = "../../tests/fixtures/fat_view.rs"]
mod fat_view;
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
        Cursor::new(bytes),
        r,
        &Arc::new(AtomicBool::new(false)),
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
fn backup_boot_search_finds_bounded_ntfs_fat32_and_exfat_views() {
    let mut ntfs = NTFS.to_vec();
    ntfs[..512].fill(0);
    ntfs[16384..16384 + 2048].fill(0);
    let mut fat32 = fat_view::fat32(512);
    fat32[..512].fill(0);
    let mut exfat = fat_view::exfat(4096);
    exfat[..12 * 4096].fill(0);
    for (image, filesystem, sector, name, content) in [
        (ntfs, "NTFS", 512, "file-with-12345", &b"12345"[..]),
        (fat32, "FAT32", 512, "FATVIEW.TXT", fat_view::FAT_CONTENT),
        (
            exfat,
            "exFAT",
            4096,
            "exfat-view.txt",
            fat_view::EXFAT_CONTENT,
        ),
    ] {
        let start = BLOCK + sector;
        let mut bytes = vec![0; start];
        bytes.extend_from_slice(&image);
        bytes.resize(bytes.len() + BLOCK, 0);
        let mut source = Region::new(Cursor::new(bytes.clone()), 0, bytes.len() as u64).unwrap();
        let range = Range {
            offset: sector as u64,
            size: (bytes.len() - sector) as u64,
            sector: sector as u64,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut found = vec![];
        scan(Cursor::new(bytes.clone()), range, &cancel, |_, item| {
            found.extend(item);
            Ok(())
        })
        .unwrap();
        assert_eq!(found.len(), 1, "{filesystem}");
        let item = &found[0];
        assert_eq!(
            (&*item.filesystem, item.offset, item.size),
            (filesystem, start as u64, image.len() as u64)
        );
        assert!(item.can_browse && !item.can_restore && item.reconstruction().is_some());
        assert!(crate::lost_partition_restore::extent(&mut source, range, item, &cancel).is_err());
        let mut view = candidate_reader(
            source,
            bytes.len() as u64,
            sector as u64,
            range,
            item,
            &cancel,
        )
        .unwrap();
        let mut files = Vec::new();
        crate::recovery::scan_filesystem_with_directories(
            &mut view,
            crate::recovery::ScanMode::Existing,
            |_, _| Ok(()),
            |file| files.push(file),
            |_| Ok(()),
        )
        .unwrap();
        let file = files.iter().find(|file| file.name == name).unwrap();
        let mut extracted = Vec::new();
        crate::recovery::extract(&mut view, file, &mut extracted, u64::MAX, |_| Ok(())).unwrap();
        assert_eq!(extracted, content);
        assert!(view.seek(SeekFrom::End(1)).is_err());
        drop(view);
        let mut changed = bytes.clone();
        changed[start + 77] ^= 1;
        assert!(candidate_reader(
            Cursor::new(changed),
            bytes.len() as u64,
            sector as u64,
            range,
            item,
            &cancel
        )
        .is_err());
    }
}

#[test]
fn backup_boot_search_ignores_duplicate_conflicting_and_truncated_copies() {
    let sector = 512;
    // The extra tail would fit a wrongly interpreted copy with a shifted start.
    let healthy = fat_view::exfat(sector);
    let mut disk = vec![0; 4096];
    disk.extend_from_slice(&healthy);
    disk.resize(disk.len() + 64 * sector, 0);
    let candidates = findings(disk.clone(), sector as u64);
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].can_restore && candidates[0].reconstruction().is_none());
    disk[4096 + 100] ^= 1;
    assert!(findings(disk, sector as u64).is_empty());
    let mut damaged = healthy;
    damaged[..12 * sector].fill(0);
    let range = range(damaged.len());
    let anchor = backup_boot::hint(&damaged[12 * sector..], 12 * sector as u64, range).unwrap();
    let cancel = AtomicBool::new(false);
    let error = backup_boot::probe(
        &mut Cursor::new(&damaged),
        sector as u64,
        anchor,
        &cancel,
        &mut 0,
    )
    .unwrap_err();
    assert!(error.contains("搜索预算"));
    damaged.truncate(damaged.len() - sector);
    assert!(findings(damaged, sector as u64).is_empty());
}

#[test]
fn ext4_backup_search_checks_bounds_and_reopens_the_same_read_only_view() {
    let image =
        zstd::stream::decode_all(&include_bytes!("../../tests/fixtures/test_disk1.bin.zst")[..])
            .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    const FIRST_BACKUP: usize = 8_389_632;
    for (sector, start) in [(512, BLOCK - 1536), (4096, BLOCK - 4096)] {
        let mut disk = vec![0; start];
        disk.extend_from_slice(&image);
        let range = Range {
            sector,
            ..range(disk.len())
        };
        let healthy = findings(disk.clone(), sector);
        assert_eq!(healthy.len(), 1);
        assert!(healthy[0].can_restore);
        disk[start + 1080..start + 1082].fill(0);
        let backup = start + FIRST_BACKUP;
        let anchor = backup_ext::hint(&disk[backup..], backup as u64, range).unwrap();
        let fault = backup_ext::probe(
            Cursor::new(disk.clone()),
            sector,
            anchor,
            &cancel,
            &Rc::new(Cell::new(0)),
        )
        .unwrap_err();
        assert!(fault.contains("搜索预算"));

        let mut lost = findings(disk.clone(), sector);
        assert_eq!(lost.len(), 1);
        let item = lost.pop().unwrap();
        assert_eq!(
            (&*item.filesystem, item.offset, item.size),
            ("Ext4", start as u64, image.len() as u64)
        );
        assert!(item.can_browse && !item.can_restore);
        assert!(matches!(item.reconstruction(), Some(view::Proof::Ext(_))));
        assert!(crate::lost_partition_restore::extent(
            &mut Cursor::new(disk.as_slice()),
            range,
            &item,
            &cancel,
        )
        .is_err());
        let open = |bytes: Vec<u8>| {
            candidate_reader(
                Cursor::new(bytes),
                range.size,
                range.sector,
                range,
                &item,
                &cancel,
            )
        };
        let mut files = vec![];
        crate::ext_recovery::scan(
            open(disk.clone()).unwrap(),
            crate::recovery::ScanMode::Existing,
            cancel.clone(),
            |_, _| Ok(()),
            |file| files.push(file),
            |_| Ok(()),
        )
        .unwrap();
        let sample = files.iter().find(|c| c.name == "small_file").unwrap();
        let mut output = vec![];
        crate::recovery::extract_owned(
            open(disk.clone()).unwrap(),
            sample,
            &mut output,
            u64::MAX,
            cancel.clone(),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(output, b"hello, world!");

        let mut changed = disk.clone();
        changed[start + FIRST_BACKUP + 0x3fc] ^= 1;
        assert!(open(changed.clone()).is_err());
        assert_eq!(
            findings(changed, sector).len(),
            1,
            "use the next valid backup"
        );
        let mut changed = disk.clone();
        changed[start + 16_778_240 + 0x1f] = 1;
        assert!(open(changed.clone()).is_err());
        assert!(findings(changed, sector).is_empty(), "pending journal");
        let mut shorter = disk.clone();
        shorter.truncate(disk.len() - sector as usize);
        assert!(findings(shorter, sector).is_empty());
        disk[start + 1024..start + 2048].fill(0);
        assert!(
            findings(disk, sector).is_empty(),
            "missing primary identity"
        );
    }
}

#[test]
fn btrfs_search_deduplicates_mirrors_and_allows_read_only_recovery_without_primary() {
    let offset = BLOCK - 64 * 1024;
    let mut disk = vec![0; offset];
    let volume = crate::btrfs_recovery::fixture::allocation_image_with_mirror(true, 4096, true);
    let size = volume.len() as u64;
    disk.extend(volume);
    let range = Range {
        sector: 4096,
        ..range(disk.len())
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let mut source = Cursor::new(disk);
    let detect = |source: &Cursor<Vec<u8>>, range| {
        let mut candidates = Vec::new();
        scan(source.clone(), range, &cancel, |_, candidate| {
            candidates.extend(candidate);
            Ok(())
        })
        .unwrap();
        candidates
    };
    let candidates = detect(&mut source, range);
    let [healthy] = candidates.as_slice() else {
        panic!("expected one candidate")
    };
    assert_eq!((healthy.offset, healthy.size), (offset as u64, size));
    assert_eq!(healthy.filesystem, "Btrfs");
    assert!(healthy.can_restore);
    assert!(detect(
        &mut source,
        Range {
            size: range.size - 4096,
            ..range
        }
    )
    .is_empty());

    source.get_mut()[offset + 64 * 1024..][..4096].fill(0);
    assert!(revalidate(&mut source, range, healthy, &cancel).is_err());
    let candidates = detect(&mut source, range);
    let [mirror] = candidates.as_slice() else {
        panic!("expected one mirror candidate")
    };
    assert_eq!((mirror.offset, mirror.size), (offset as u64, size));
    assert!(!mirror.can_restore);
    let reader = candidate_reader(
        source.clone(),
        range.size,
        range.sector,
        range,
        mirror,
        &cancel,
    )
    .unwrap();
    let mut volume = disk_core::btrfs::open_recovered(reader, || Ok(())).unwrap();
    let mut output = Vec::new();
    volume
        .read_file(5, 257, u64::MAX, &mut output, || Ok(()))
        .unwrap();
    assert_eq!(output, [&[23; 4096][..], &[0; 7][..]].concat());
    assert!(!volume.has_current_primary().unwrap());
    volume.verify_view(|| Ok(())).unwrap();
    drop(volume);
    source.get_mut()[offset + 64 * 1024 * 1024] ^= 1;
    assert!(detect(&mut source, range).is_empty());
}
#[test]
fn btrfs_member_header_is_retained_for_grouping_when_its_directory_is_unavailable() {
    use crate::btrfs_recovery::fixture::{allocation_image_with_mirror, set64, sign};
    let mut bytes = allocation_image_with_mirror(true, 4096, false);
    let primary = &mut bytes[64 * 1024..68 * 1024];
    // Keep a valid multi-member superblock while the local mapping tree cannot
    // establish the second device. This header can only participate in a group.
    set64(primary, 0x70, 16 * 1024 * 1024);
    set64(primary, 0x88, 2);
    sign(primary);
    let found = findings(bytes.clone(), 4096);
    let [member] = &found[..] else {
        panic!("expected one header-only candidate")
    };
    assert!(!member.can_browse && !member.can_restore);
    assert_eq!(member.btrfs_member_count, Some(2));
    assert_eq!(member.btrfs_member.as_ref().unwrap().id, 1);
    let range = Range {
        sector: 4096,
        ..range(bytes.len())
    };
    let cancel = AtomicBool::new(false);
    let reader = candidate_reader(
        Cursor::new(bytes.clone()),
        range.size,
        range.sector,
        range,
        member,
        &cancel,
    )
    .unwrap();
    assert!(disk_core::btrfs::open_recovered_members(vec![reader], false, || Ok(())).is_err());
    let primary = &mut bytes[64 * 1024..68 * 1024];
    set64(primary, 0x48, 11);
    sign(primary);
    assert!(candidate_reader(
        Cursor::new(bytes),
        range.size,
        range.sector,
        range,
        member,
        &cancel,
    )
    .is_err());
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
    let source = || Bounded {
        cursor: Cursor::new(bytes.clone()),
        low: r.offset,
        high: r.offset + r.size,
    };
    let mut completed = 0;
    scan(source(), r, &Arc::new(AtomicBool::new(false)), |n, _| {
        completed = n;
        Ok(())
    })
    .unwrap();
    assert_eq!(completed, r.size);
    let stop = Arc::new(AtomicBool::new(true));
    assert!(scan(source(), r, &stop, |_, _| Ok(())).is_err());
    assert!(scan(
        Cursor::new(vec![0; 512]),
        r,
        &Arc::new(AtomicBool::new(false)),
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
    let bounded = crate::data_io::Region::new(raw, range.offset, range.size).unwrap();
    let mut found = vec![];
    scan(
        bounded,
        Range { offset: 0, ..range },
        &Arc::new(AtomicBool::new(false)),
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

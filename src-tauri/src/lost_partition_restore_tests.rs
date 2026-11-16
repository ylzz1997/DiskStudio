use super::*;
use disk_core::MIB;
use std::io::Cursor;
fn archive(bytes: &[u8], sector: u64) -> Archive {
    crate::table_archive::capture(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        sector,
        "a".repeat(64),
        false,
        || Ok(()),
    )
    .unwrap()
}
fn part(offset: u64, size: u64) -> Extent {
    Extent {
        offset,
        size,
        kind: crate::partitions::policy::BASIC_DATA.into(),
        name: "Recovered NTFS".into(),
    }
}
struct SectorReader {
    inner: Cursor<Vec<u8>>,
    sector: usize,
}
impl Read for SectorReader {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if self.inner.position() % self.sector as u64 != 0 || b.len() % self.sector != 0 {
            return Err(std::io::Error::other("raw read is not sector aligned"));
        }
        self.inner.read(b)
    }
}
impl Seek for SectorReader {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(p)
    }
}
#[test]
fn filesystem_evidence_reads_are_aligned_before_reaching_the_raw_device() {
    for sector in [512, 4096] {
        let size = 4 * MIB;
        let p = disk_core::fat_format::Plan::new(
            disk_core::fat_format::FatType::Fat12,
            size,
            sector,
            0,
            "LOST",
            123,
            0,
        )
        .unwrap();
        let mut bytes = Cursor::new(vec![0; size as usize]);
        p.format(&mut bytes, |_, _| Ok(())).unwrap();
        let range = Range {
            offset: 0,
            size,
            sector: sector as u64,
        };
        let cancel = AtomicBool::new(false);
        let c = lost_partitions::probe(&mut bytes, range, 0, &cancel)
            .unwrap()
            .unwrap();
        let mut raw = SectorReader {
            inner: bytes,
            sector: sector as usize,
        };
        assert_eq!(extent(&mut raw, range, &c, &cancel).unwrap().size, size);
    }
    let bytes = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec();
    let range = Range {
        offset: 0,
        size: bytes.len() as u64,
        sector: 512,
    };
    let cancel = AtomicBool::new(false);
    let c = lost_partitions::probe(&mut Cursor::new(&bytes), range, 0, &cancel)
        .unwrap()
        .unwrap();
    let mut raw = SectorReader {
        inner: Cursor::new(bytes),
        sector: 512,
    };
    assert_eq!(
        extent(&mut raw, range, &c, &cancel).unwrap().size,
        range.size
    );
}
#[test]
fn inserts_multiple_entries_and_preserves_existing_data_padding_and_identity() {
    for sector in [512, 4096] {
        let original = crate::table_archive::tests::gpt(sector);
        let current = archive(&original, sector as u64);
        let previous = table_restore::archive_layout(&current).unwrap();
        let wanted = add_entries(
            &current,
            &[
                part(3 * MIB, MIB),
                Extent {
                    kind: crate::partitions::policy::LINUX_DATA.into(),
                    name: "Recovered Ext4".into(),
                    ..part(5 * MIB, MIB)
                },
            ],
        )
        .unwrap();
        let after = table_restore::archive_layout(&wanted).unwrap();
        assert_eq!(after.entries.len(), 3);
        assert_eq!(after.guid, previous.guid);
        assert_eq!(after.entries[0], previous.entries[0]);
        assert_eq!(after.entries[1].size, MIB);
        assert_eq!(after.entries[2].kind, crate::partitions::policy::LINUX_DATA);
        let p = table_restore::prepare(
            &mut Cursor::new(&original),
            &wanted,
            &current.source,
            current.size,
            current.sector,
            &previous,
            &[previous.entries[0].guid.clone()],
            false,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(p.changes.len(), 2);
        assert!(p.changes.iter().all(|c| c.before.is_none()));
        let mut io = Cursor::new(original.clone());
        assert_eq!(p.execute(&mut io, |_| Ok(()), || Ok(())).unwrap().ranges, 4);
        let expected = wanted.extents.iter().fold(original.clone(), |mut b, e| {
            b[e.offset as usize..e.offset as usize + e.bytes.len()].copy_from_slice(&e.bytes);
            b
        });
        assert!(
            io.get_ref() == &expected,
            "only generated table extents may change"
        );
        for old in current
            .extents
            .iter()
            .filter(|e| matches!(e.kind, Kind::PrimaryEntries | Kind::BackupEntries))
        {
            let new = wanted.extents.iter().find(|e| e.kind == old.kind).unwrap();
            let used = previous.count as usize * 128;
            assert_eq!(
                &new.bytes[used..],
                &old.bytes[used..],
                "each array keeps its original padding"
            );
        }
    }
}
#[test]
fn rejects_overlaps_reserved_ranges_invalid_types_and_full_tables() {
    let current = archive(&crate::table_archive::tests::gpt(512), 512);
    for entries in [
        vec![],
        vec![part(MIB, MIB)],
        vec![part(0, MIB)],
        vec![part(current.size - 512, 512)],
        vec![part(3 * MIB, MIB), part(3 * MIB + 512, MIB)],
        vec![part(3 * MIB + 1, MIB)],
        vec![part(3 * MIB, 0)],
        vec![Extent {
            kind: crate::partitions::policy::LINUX_SWAP.into(),
            ..part(3 * MIB, MIB)
        }],
    ] {
        assert!(add_entries(&current, &entries).is_err());
    }
    let mut full = current.clone();
    // Unused entries with retained nonzero payloads are not blank slots.
    for e in full
        .extents
        .iter_mut()
        .filter(|e| matches!(e.kind, Kind::PrimaryEntries | Kind::BackupEntries))
    {
        for slot in e.bytes[..17 * 128].chunks_exact_mut(128).skip(1) {
            slot[56] = 1;
        }
    }
    for (hk, ek) in [
        (Kind::PrimaryHeader, Kind::PrimaryEntries),
        (Kind::BackupHeader, Kind::BackupEntries),
    ] {
        let crc =
            crc32fast::hash(&full.extents.iter().find(|e| e.kind == ek).unwrap().bytes[..17 * 128]);
        let h = &mut full
            .extents
            .iter_mut()
            .find(|e| e.kind == hk)
            .unwrap()
            .bytes;
        h[88..92].copy_from_slice(&crc.to_le_bytes());
        h[16..20].fill(0);
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    assert!(add_entries(&full, &[part(3 * MIB, MIB)]).is_err());
}
#[test]
fn lost_ntfs_restores_backup_boot_extent_and_allows_exact_file_read_after_table_write() {
    let fixture = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
    let mut bytes = crate::table_archive::tests::gpt(512);
    let at = 3 * MIB as usize;
    bytes[at..at + fixture.len()].copy_from_slice(fixture);
    let range = Range {
        offset: at as u64,
        size: fixture.len() as u64,
        sector: 512,
    };
    let cancel = AtomicBool::new(false);
    let c = lost_partitions::probe(&mut Cursor::new(&bytes), range, at as u64, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(c.size + 512, fixture.len() as u64);
    let e = extent(&mut Cursor::new(&bytes), range, &c, &cancel).unwrap();
    assert_eq!(e.size, fixture.len() as u64);
    let current = archive(&bytes, 512);
    let desired = add_entries(&current, &[e.clone()]).unwrap();
    let native = table_restore::archive_layout(&current).unwrap();
    let plan = table_restore::prepare(
        &mut Cursor::new(&bytes),
        &desired,
        &current.source,
        current.size,
        512,
        &native,
        &[],
        false,
        || Ok(()),
    )
    .unwrap();
    let mut io = Cursor::new(bytes.clone());
    plan.execute(&mut io, |_| Ok(()), || Ok(())).unwrap();
    assert_eq!(&io.get_ref()[at..at + fixture.len()], fixture.as_slice());
    let mut files = vec![];
    crate::recovery::scan_filesystem_owned(
        crate::data_io::Region::new(Cursor::new(io.get_ref().clone()), e.offset, e.size).unwrap(),
        crate::recovery::ScanMode::AllFiles,
        std::sync::Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| files.push(c),
        |_| Ok(()),
    )
    .unwrap();
    let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
    let mut output = vec![];
    crate::recovery::extract_owned(
        crate::data_io::Region::new(io, e.offset, e.size).unwrap(),
        file,
        &mut output,
        file.size,
        std::sync::Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(output, b"12345");
    assert!(extent(
        &mut Cursor::new(&bytes),
        Range {
            size: range.size - 512,
            ..range
        },
        &c,
        &cancel
    )
    .is_err());
    let saved = Evidence {
        range,
        candidate: c.clone(),
        extent: e,
    };
    bytes[at + fixture.len() - 512 + 72] ^= 1;
    assert!(verify(&mut Cursor::new(&bytes), &[saved], &cancel).is_err());
    assert!(extent(&mut Cursor::new(&bytes), range, &c, &AtomicBool::new(true)).is_err());
}

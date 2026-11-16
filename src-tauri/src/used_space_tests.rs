use super::*;
use std::io::Cursor;
struct Observed {
    bytes: Cursor<Vec<u8>>,
    reads: Vec<Span>,
    writes: Vec<Span>,
}
impl Observed {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            reads: vec![],
            writes: vec![],
        }
    }
}
impl Read for Observed {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let at = self.bytes.position();
        let n = self.bytes.read(out)?;
        self.reads.push(Span {
            start: at,
            end: at + n as u64,
        });
        Ok(n)
    }
}
impl Write for Observed {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let at = self.bytes.position();
        let n = self.bytes.write(b)?;
        self.writes.push(Span {
            start: at,
            end: at + n as u64,
        });
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for Observed {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.bytes.seek(p)
    }
}
fn plan() -> Plan {
    let mut b = Builder::new(8 * CHUNK as u64, 512).unwrap();
    b.free(CHUNK as u64, 2 * CHUNK as u64).unwrap();
    b.free(5 * CHUNK as u64, 3 * CHUNK as u64).unwrap();
    b.finish().unwrap()
}
#[test]
fn copies_and_verifies_every_retained_byte_without_touching_free_or_neighboring_ranges() {
    let p = plan();
    let raw = (0..p.size).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let mut source = Observed::new(raw.clone());
    let mut target = Observed::new(vec![0xa7; raw.len() + 512]);
    let hash = p
        .copy_verified(
            &mut source,
            &mut target,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
            |_, _, _| Ok(()),
        )
        .unwrap();
    assert_eq!(hash.len(), 64);
    for at in 0..raw.len() {
        assert_eq!(
            target.bytes.get_ref()[at],
            if p.free
                .iter()
                .any(|s| s.start <= at as u64 && (at as u64) < s.end)
            {
                0xa7
            } else {
                raw[at]
            }
        );
    }
    assert_eq!(&target.bytes.get_ref()[raw.len()..], &[0xa7; 512]);
    for r in source
        .reads
        .iter()
        .chain(target.writes.iter())
        .chain(target.reads.iter())
    {
        assert!(!p.free.iter().any(|f| r.start < f.end && f.start < r.end));
    }
    assert_eq!(
        source.reads.iter().map(|s| s.size()).sum::<u64>(),
        p.copied()
    );
    assert_eq!(
        target.reads.iter().map(|s| s.size()).sum::<u64>(),
        p.copied()
    );
}
#[test]
fn normalized_backup_reads_zeroes_for_free_space_without_reading_it_from_source() {
    let p = plan();
    let mut source = Observed::new(vec![19; p.size as usize]);
    let mut view = p.masked(&mut source);
    let mut bytes = vec![];
    view.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), p.size as usize);
    assert_eq!(
        bytes.iter().filter(|&&v| v == 19).count() as u64,
        p.copied()
    );
    view.seek(SeekFrom::Start(CHUNK as u64 - 2)).unwrap();
    let mut edge = [0; 5];
    view.read_exact(&mut edge).unwrap();
    assert_eq!(edge, [19, 19, 0, 0, 0]);
    drop(view);
    for r in source.reads {
        assert!(!p.free.iter().any(|f| r.start < f.end && f.start < r.end));
    }
}
#[test]
fn short_source_cancellation_and_corrupt_readback_never_report_success() {
    let p = plan();
    let mut target = Cursor::new(vec![0; p.size as usize]);
    assert!(p
        .copy_verified(
            &mut Cursor::new(vec![1; 1]),
            &mut target,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
            |_, _, _| Ok(())
        )
        .is_err());
    let original = target.get_ref().clone();
    assert!(p
        .copy_verified(
            &mut Cursor::new(vec![1; p.size as usize]),
            &mut target,
            Arc::new(AtomicBool::new(true)),
            |_| Ok(()),
            |_, _, _| Ok(())
        )
        .is_err());
    assert_eq!(target.get_ref(), &original);
    assert!(p
        .copy_verified(
            &mut Cursor::new(vec![1; p.size as usize]),
            &mut target,
            Arc::new(AtomicBool::new(false)),
            |t| {
                t.get_mut()[512] ^= 1;
                Ok(())
            },
            |_, _, _| Ok(())
        )
        .unwrap_err()
        .contains("SHA-256"));
}
#[test]
fn audited_metadata_must_remain_allocated_and_unchanged() {
    let mut source = Cursor::new(vec![1; 4096]);
    let cancel = AtomicBool::new(false);
    let mut b = Builder::new(4096, 512).unwrap();
    {
        let mut a = Audit::new(&mut source, 0, 4096, &mut b.proof, &cancel).unwrap();
        let mut bytes = [0; 512];
        a.read_exact(&mut bytes).unwrap();
    }
    b.free(512, 512).unwrap();
    let p = b.finish().unwrap();
    p.proof.verify(&mut source, &cancel).unwrap();
    source.get_mut()[0] ^= 1;
    assert!(p.proof.verify(&mut source, &cancel).is_err());
    let mut b = Builder::new(4096, 512).unwrap();
    b.proof = p.proof;
    b.free(0, 512).unwrap();
    assert!(b.finish().is_err());
}
#[test]
fn ranges_align_inward_merge_and_fail_closed_on_overlap_or_overflow() {
    let mut b = Builder::new(16384, 4096).unwrap();
    b.free(1, 8191).unwrap();
    b.free(8192, 4096).unwrap();
    let p = b.finish().unwrap();
    assert_eq!(
        p.free,
        [Span {
            start: 4096,
            end: 12288
        }]
    );
    assert_eq!(
        p.retained().collect::<Vec<_>>(),
        [
            Span {
                start: 0,
                end: 4096
            },
            Span {
                start: 12288,
                end: 16384
            }
        ]
    );
    let mut b = Builder::new(4096, 512).unwrap();
    assert!(b.free(u64::MAX, 512).is_err());
    b.free(512, 512).unwrap();
    assert!(b.free(512, 512).is_err());
    let mut b = Builder::new(4096, 512).unwrap();
    b.free(0, 4096).unwrap();
    assert!(b.finish().is_err());
}
#[test]
fn exact_views_reject_out_of_range_writes_and_preserve_neighbor_bytes() {
    let mut raw = Cursor::new(vec![3; 4096]);
    {
        let mut view = View::new(&mut raw, 512, 512).unwrap();
        assert!(view.write_all(&[7; 513]).is_err());
        assert!(view.seek(SeekFrom::Start(513)).is_err());
        view.write_all(&[8; 512]).unwrap();
    }
    assert!(raw.get_ref()[..512].iter().all(|&v| v == 3));
    assert!(raw.get_ref()[512..1024].iter().all(|&v| v == 8));
    assert!(raw.get_ref()[1024..].iter().all(|&v| v == 3));
}

fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn fat(bits: u8) -> Vec<u8> {
    let (sectors, reserved, fat_sectors, roots) = match bits {
        12 => (100usize, 1usize, 1usize, 16usize),
        16 => (9000, 1, 36, 512),
        _ => (70000, 32, 547, 0),
    };
    let mut b = vec![0; sectors * 512];
    put16(&mut b, 11, 512);
    b[13] = 1;
    put16(&mut b, 14, reserved as u16);
    b[16] = 2;
    put16(&mut b, 17, roots as u16);
    b[21] = 0xf8;
    if sectors < 65536 {
        put16(&mut b, 19, sectors as u16);
    } else {
        put32(&mut b, 32, sectors as u32);
    }
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    if bits == 32 {
        put32(&mut b, 36, fat_sectors as u32);
        put32(&mut b, 44, 2);
        put16(&mut b, 50, 6);
        let boot = b[..512].to_vec();
        b[6 * 512..7 * 512].copy_from_slice(&boot);
    } else {
        put16(&mut b, 22, fat_sectors as u16);
    }
    for copy in 0..2 {
        let at = (reserved + copy * fat_sectors) * 512;
        if bits == 12 {
            b[at..at + 5].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff, 0x0f]);
        } else if bits == 16 {
            put16(&mut b, at, 0xfff8);
            put16(&mut b, at + 2, 0xffff);
            put16(&mut b, at + 4, 0xffff);
        } else {
            put32(&mut b, at, 0xffffff8);
            put32(&mut b, at + 4, 0xfffffff);
            put32(&mut b, at + 8, 0xfffffff);
        }
    }
    let heap = (reserved + 2 * fat_sectors + (roots * 32).div_ceil(512)) * 512;
    if bits != 32 {
        b[heap..heap + 5].copy_from_slice(b"hello");
    }
    b
}
fn exfat() -> Vec<u8> {
    let mut b = vec![0; 512 * 125];
    b[3..11].copy_from_slice(b"EXFAT   ");
    b[72] = 125;
    b[80] = 24;
    b[84] = 1;
    b[88] = 25;
    b[92] = 100;
    b[96] = 2;
    b[105] = 1;
    b[108] = 9;
    b[110] = 1;
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    let sum = b[..512 * 11]
        .iter()
        .enumerate()
        .filter(|(i, _)| ![106, 107, 112].contains(i))
        .fold(0u32, |h, (_, &v)| h.rotate_right(1).wrapping_add(v as u32));
    for c in b[512 * 11..512 * 12].chunks_exact_mut(4) {
        c.copy_from_slice(&sum.to_le_bytes());
    }
    let main = b[..512 * 12].to_vec();
    b[512 * 12..512 * 24].copy_from_slice(&main);
    for cluster in 2..5 {
        put32(&mut b, 512 * 24 + cluster * 4, 0xffffffff);
    }
    let root = 512 * 25;
    b[root] = 0x81;
    b[root + 20] = 3;
    b[root + 24] = 13;
    b[root + 32] = 0x82;
    b[root + 32 + 20] = 4;
    b[root + 32 + 24] = 2;
    // A bounded upcase payload with an independently computed checksum; these
    // tests concern allocation, not Unicode lookup semantics.
    b[512 * 27..512 * 27 + 2].copy_from_slice(&[0x41, 0]);
    put32(&mut b, root + 36, 0x80000020);
    b[512 * 26] = 7;
    b
}
fn analyze(bytes: &[u8]) -> Result<Plan, String> {
    inspect(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        512,
        None,
        &AtomicBool::new(false),
    )
}
#[test]
fn allocation_fat12_fat16_fat32_and_exfat_retain_metadata_and_allocated_clusters() {
    for bytes in [fat(12), fat(16), fat(32), exfat()] {
        let plan = analyze(&bytes).unwrap();
        assert!(plan.skipped > 0);
        assert!(plan.copied() > 512);
        plan.proof
            .verify(&mut Cursor::new(&bytes), &AtomicBool::new(false))
            .unwrap();
        let mut target = Cursor::new(vec![0xa7; bytes.len()]);
        plan.copy_verified(
            &mut Cursor::new(&bytes),
            &mut target,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
            |_, _, _| Ok(()),
        )
        .unwrap();
        let copied = target.into_inner();
        let copied_plan = analyze(&copied).unwrap();
        assert_eq!(copied_plan.free, plan.free);
        for span in plan.retained() {
            assert_eq!(
                &copied[span.start as usize..span.end as usize],
                &bytes[span.start as usize..span.end as usize]
            );
        }
    }
}

#[test]
fn free_space_wipe_real_allocation_parsers_preserve_ntfs_and_all_fat_metadata() {
    for bytes in [
        fat(12),
        fat(16),
        fat(32),
        exfat(),
        include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec(),
    ] {
        let plan = analyze(&bytes).unwrap();
        let mut io = Cursor::new(bytes.clone());
        let mut stats = crate::maintenance_io::WipeProgress::default();
        let result = crate::free_space_wipe::execute(
            &mut io,
            &plan,
            &crate::maintenance_io::WipeOptions {
                passes: 1,
                pattern: crate::maintenance_io::Pattern::Bytes { hex: "A5".into() },
            },
            &AtomicBool::new(false),
            &mut stats,
            |_| panic!("not random"),
            |_| Ok(()),
            |_| {},
        )
        .unwrap();
        assert_eq!(stats.written_bytes, plan.skipped);
        assert_eq!(stats.readback_bytes, plan.skipped);
        assert!(result.preserved_sha256.is_some());
        let actual = io.into_inner();
        assert_eq!(analyze(&actual).unwrap().digest, plan.digest);
        for span in plan.retained() {
            assert_eq!(
                &actual[span.start as usize..span.end as usize],
                &bytes[span.start as usize..span.end as usize]
            );
        }
        for span in plan.free_ranges() {
            assert!(actual[span.start as usize..span.end as usize]
                .iter()
                .all(|&b| b == 0xa5));
        }
    }
}
#[test]
fn allocation_dirty_mirrored_or_self_free_fat_metadata_fail_closed() {
    let mut b = fat(16);
    b[512 + 3] &= 0x7f;
    assert!(analyze(&b).is_err());
    let mut b = fat(32);
    b[6 * 512 + 48] ^= 1;
    assert!(analyze(&b).is_err());
    let mut b = fat(16);
    b[(1 + 36) * 512 + 4] = 0;
    assert!(analyze(&b).err().unwrap().contains("分配表"));
    let mut b = exfat();
    b[106] = 2;
    assert!(analyze(&b).is_err());
    let mut b = exfat();
    b[512 * 26] = 3;
    assert!(analyze(&b).is_err());
    let mut b = exfat();
    b[512 * 12 + 120] ^= 1;
    assert!(analyze(&b).is_err());
}
#[test]
fn allocation_ntfs_clone_and_normalized_backup_keep_a_known_file_readable() {
    let mut bytes = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec();
    let flags_at = ntfs_volume_information(&bytes) + 10;
    // Current Windows defaults to disabling 8.3 names. Preserve that policy
    // in both outputs while proving the allocated file remains readable.
    put16(&mut bytes, flags_at, 0x0080);
    let plan = analyze(&bytes).unwrap();
    assert!(plan.skipped > 0);
    let mut normalized = vec![];
    plan.masked(Cursor::new(bytes.clone()))
        .read_to_end(&mut normalized)
        .unwrap();
    let mut target = Cursor::new(vec![0xcc; bytes.len()]);
    plan.copy_verified(
        &mut Cursor::new(&bytes),
        &mut target,
        Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
        |_, _, _| Ok(()),
    )
    .unwrap();
    for copied in [normalized, target.into_inner()] {
        assert_eq!(&copied[flags_at..flags_at + 2], &[0x80, 0]);
        let mut reader = Cursor::new(copied);
        let mut files = vec![];
        crate::recovery::scan_filesystem_with_directories(
            &mut reader,
            crate::recovery::ScanMode::Existing,
            |_, _| Ok(()),
            |c| files.push(c),
            |_| Ok(()),
        )
        .unwrap();
        let file = files.iter().find(|c| c.name == "file-with-12345").unwrap();
        let mut content = vec![];
        crate::recovery::extract(&mut reader, file, &mut content, u64::MAX, |_| Ok(())).unwrap();
        assert_eq!(content, b"12345");
    }
}
fn ntfs_volume_information(bytes: &[u8]) -> usize {
    let mut r = Cursor::new(bytes);
    let fs = ntfs::Ntfs::new(&mut r).unwrap();
    let volume = fs.file(&mut r, 3).unwrap();
    let mut attrs = volume.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let attr = item.to_attribute().unwrap();
        if attr.ty().unwrap() == ntfs::NtfsAttributeType::VolumeInformation {
            assert!(attr.is_resident());
            let raw = attr.record_bytes();
            let value_offset = u16::from_le_bytes(raw[20..22].try_into().unwrap()) as usize;
            let at = attr.position().value().unwrap().get() as usize + value_offset;
            // This fixture's resident value does not intersect an NTFS USA
            // replacement word, so changing these bytes needs no fixup edit.
            assert!(at % 512 + 12 < 510);
            return at;
        }
    }
    panic!("fixture volume-information record missing");
}
#[test]
fn allocation_ntfs_short_name_policy_never_masks_pending_or_unknown_flags() {
    let original = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec();
    let at = ntfs_volume_information(&original);
    let expected = analyze(&original).unwrap();
    let mut bytes = original.clone();
    put16(&mut bytes, at + 10, 0x80);
    let actual = analyze(&bytes).unwrap();
    assert_eq!(expected.free, actual.free);
    assert_eq!(expected.skipped, actual.skipped);
    // All other individual flags and their combinations with the harmless
    // policy must still reject before copying or opening a destination.
    for bit in (0..16).filter(|&bit| bit != 7) {
        for policy in [0, 0x80] {
            let flags = (1 << bit) | policy;
            put16(&mut bytes, at + 10, flags);
            let error = analyze(&bytes)
                .err()
                .unwrap_or_else(|| panic!("unsafe volume flags 0x{flags:04X} accepted at {at}"));
            assert!(error.contains(&format!("0x{flags:04X}")), "{error}");
        }
    }
    put16(&mut bytes, at + 10, 0x80);
    bytes[at + 9] = 0;
    assert!(analyze(&bytes).is_err(), "NTFS 3.0 must remain unsupported");
}
#[test]
fn allocation_ntfs_corrupt_backup_and_bitmap_metadata_are_rejected() {
    let mut b = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec();
    let last = b.len() - 512;
    b[last + 40] ^= 1;
    assert!(analyze(&b).is_err());
    let mut b = include_bytes!("../../tests/fixtures/ntfs-testfs1.img").to_vec();
    let mut r = Cursor::new(&b);
    let fs = ntfs::Ntfs::new(&mut r).unwrap();
    let f = fs.file(&mut r, 6).unwrap();
    let item = f.data(&mut r, "").unwrap().unwrap();
    let a = item.to_attribute().unwrap();
    let pos = match a.value(&mut r).unwrap() {
        ntfs::attribute_value::NtfsAttributeValue::NonResident(v) => {
            v.data_position().value().unwrap().get()
        }
        _ => panic!("fixture bitmap must be nonresident"),
    };
    b[pos as usize] &= !1;
    assert!(analyze(&b).is_err());
}
fn ext(kind: usize) -> Vec<u8> {
    let compressed: &[u8] = match kind {
        0 => include_bytes!("../../tests/fixtures/test_disk_ext2.bin.zst"),
        1 => include_bytes!("../../tests/fixtures/test_disk_ext3.bin.zst"),
        _ => include_bytes!("../../tests/fixtures/test_disk1.bin.zst"),
    };
    zstd::stream::decode_all(compressed).unwrap()
}
#[test]
fn allocation_ext2_ext3_ext4_normalized_copies_preserve_independently_known_files() {
    for kind in 0..3 {
        let bytes = ext(kind);
        let plan = analyze(&bytes).unwrap();
        assert!(plan.skipped > 0);
        let mut normalized = vec![];
        plan.masked(Cursor::new(bytes))
            .read_to_end(&mut normalized)
            .unwrap();
        let fs = ext4_view::Ext4::load(Box::new(normalized)).unwrap();
        let (path, expected) = if kind == 1 {
            ("/medium_dir/42", "42")
        } else {
            ("/small_file", "hello, world!")
        };
        assert_eq!(fs.read_to_string(path).unwrap(), expected);
    }
}
#[test]
fn allocation_ext_dirty_checksum_unknown_features_and_bad_free_counts_reject() {
    let mut b = ext(0);
    b[1024 + 0x3a] = 0;
    assert!(analyze(&b).is_err());
    let mut b = ext(2);
    b[1024 + 0x3fc] ^= 1;
    assert!(analyze(&b).is_err());
    let mut b = ext(0);
    b[1024 + 0x61] |= 0x10;
    assert!(analyze(&b).is_err());
    let mut b = ext(0);
    b[1024 + 0xc] ^= 1;
    assert!(analyze(&b).is_err());
}
#[test]
fn allocation_unknown_filesystems_are_retained_without_claiming_empty_space() {
    let b = vec![17; 8192];
    let p = analyze(&b).unwrap();
    assert_eq!(p.skipped, 0);
    assert_eq!(p.volumes[0].filesystem, "未识别");
    assert!(inspect(
        &mut Cursor::new(&b),
        8192,
        512,
        Some(&[]),
        &AtomicBool::new(false)
    )
    .is_err());
}
#[test]
fn allocation_backup_bound_accounts_for_mixed_chunks_and_a_zero_partial_tail() {
    let mut b = Builder::new(10 * 512, 512).unwrap();
    b.free(512, 512).unwrap();
    b.free(4 * 512, 6 * 512).unwrap();
    let p = b.finish().unwrap();
    assert_eq!(p.backup_payload_bound(4 * 512).unwrap(), 4 * 512);
    assert!(p.backup_payload_bound(513).is_err());
    let mut b = Builder::new(10 * 512, 512).unwrap();
    b.free(9 * 512, 512).unwrap();
    assert_eq!(
        b.finish().unwrap().backup_payload_bound(4 * 512).unwrap(),
        10 * 512
    );
}
#[test]
fn allocation_whole_disk_preserves_tables_gaps_and_requires_matching_provider_extents() {
    let partition = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
    let mut bytes = vec![0; 6 * CHUNK];
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
    bytes[450] = 7;
    put32(&mut bytes, 454, (CHUNK / 512) as u32);
    put32(&mut bytes, 458, (partition.len() / 512) as u32);
    bytes[CHUNK..CHUNK + partition.len()].copy_from_slice(partition);
    let expected = [Span {
        start: CHUNK as u64,
        end: (CHUNK + partition.len()) as u64,
    }];
    let p = inspect(
        &mut Cursor::new(&bytes),
        bytes.len() as u64,
        512,
        Some(&expected),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(p.skipped > 0);
    assert!(p
        .free
        .iter()
        .all(|s| s.start >= expected[0].start && s.end <= expected[0].end));
    assert!(inspect(
        &mut Cursor::new(&bytes),
        bytes.len() as u64,
        512,
        Some(&[]),
        &AtomicBool::new(false)
    )
    .is_err());
    bytes[454] ^= 1;
    assert!(p
        .proof
        .verify(&mut Cursor::new(&bytes), &AtomicBool::new(false))
        .is_err());
}
#[test]
fn allocation_gpt_copies_and_4kn_unknown_volumes_keep_both_table_copies() {
    for sector in [512, 4096] {
        let mut bytes = crate::table_archive::tests::gpt(sector as usize);
        if sector == 512 {
            let fs = fat(12);
            bytes[CHUNK..CHUNK + fs.len()].copy_from_slice(&fs);
        }
        let ranges = [Span {
            start: CHUNK as u64,
            end: 2 * CHUNK as u64,
        }];
        let p = inspect(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            sector,
            Some(&ranges),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(p.skipped > 0, sector == 512);
        let mut target = Cursor::new(vec![0x71; bytes.len()]);
        p.copy_verified(
            &mut Cursor::new(&bytes),
            &mut target,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
            |_, _, _| Ok(()),
        )
        .unwrap();
        let archive = crate::table_archive::capture(
            &mut target,
            bytes.len() as u64,
            sector,
            "a".repeat(64),
            false,
            || Ok(()),
        )
        .unwrap();
        for extent in archive.extents {
            assert_eq!(
                extent.bytes,
                &bytes[extent.offset as usize..extent.offset as usize + extent.bytes.len()]
            );
        }
    }
}

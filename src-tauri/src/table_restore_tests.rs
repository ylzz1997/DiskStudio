use super::*;
use std::io::{self, Cursor};
const SIZE: usize = 8 * MIB as usize;
fn fixture(sector: usize) -> Vec<u8> {
    crate::table_archive::tests::gpt(sector)
}
fn archive(bytes: &[u8], sector: usize, boot: bool) -> Archive {
    crate::table_archive::capture(
        &mut Cursor::new(bytes),
        SIZE as u64,
        sector as u64,
        "a".repeat(64),
        boot,
        || Ok(()),
    )
    .unwrap()
}
fn edit_entries(bytes: &mut [u8], sector: usize, edit: impl Fn(&mut [u8])) {
    let last = SIZE / sector - 1;
    for own in [1, last] {
        let at = u64le(&bytes[own * sector..], 72) as usize * sector;
        let len = u32le(&bytes[own * sector..], 80) as usize * 128;
        edit(&mut bytes[at..at + len]);
        let crc = crc32fast::hash(&bytes[at..at + len]);
        let header = &mut bytes[own * sector..own * sector + 92];
        header[88..92].copy_from_slice(&crc.to_le_bytes());
        header[16..20].fill(0);
        let crc = crc32fast::hash(header);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
    }
}
fn added(bytes: &mut [u8], sector: usize) {
    edit_entries(bytes, sector, |entries| {
        let original = entries[..128].to_vec();
        let e = &mut entries[128..256];
        e.copy_from_slice(&original);
        e[16..32].fill(0x43);
        e[32..40].copy_from_slice(&(3 * MIB / sector as u64).to_le_bytes());
        e[40..48].copy_from_slice(&(4 * MIB / sector as u64 - 1).to_le_bytes());
    });
}
fn plan(bytes: &[u8], archive: &Archive, protected: &[String], boot: bool) -> Result<Plan, String> {
    let native = current_layout(
        &bytes[..MIB as usize],
        &bytes[SIZE - MIB as usize..],
        SIZE as u64,
        archive.sector,
    )?
    .0;
    prepare(
        &mut Cursor::new(bytes),
        archive,
        &"a".repeat(64),
        SIZE as u64,
        archive.sector,
        &native,
        protected,
        boot,
        || Ok(()),
    )
}
struct Medium {
    data: Cursor<Vec<u8>>,
    writes: Vec<(u64, usize)>,
    fault: u8,
}
impl Medium {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Cursor::new(data),
            writes: vec![],
            fault: 0,
        }
    }
}
impl Read for Medium {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.data.read(b)
    }
}
impl Seek for Medium {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.data.seek(p)
    }
}
impl Write for Medium {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.writes.push((self.data.position(), b.len()));
        match self.fault {
            1 => self.data.write(&b[..b.len() / 2]),
            2 => Err(io::Error::other("device error")),
            3 => {
                let mut bad = b.to_vec();
                bad[0] ^= 1;
                self.data.write(&bad)
            }
            _ => self.data.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn restores_changed_tables_backup_first_and_preserves_every_other_byte() {
    for sector in [512, 4096] {
        let original = fixture(sector);
        let a = archive(&original, sector, false);
        let mut current = original.clone();
        added(&mut current, sector);
        let p = plan(&current, &a, &[], false).unwrap();
        assert_eq!(p.changes.len(), 1);
        assert!(p.changes[0].after.is_none());
        assert_eq!(
            p.writes.iter().map(|w| w.kind.clone()).collect::<Vec<_>>(),
            [
                Kind::BackupEntries,
                Kind::BackupHeader,
                Kind::PrimaryEntries,
                Kind::PrimaryHeader
            ]
        );
        let mut disk = Medium::new(current);
        let mut syncs = 0;
        let result = p
            .execute(
                &mut disk,
                |_| {
                    syncs += 1;
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
        assert_eq!(syncs, 4);
        assert_eq!(result.ranges, 4);
        assert_eq!(result.written_bytes, p.write_bytes);
        assert_eq!(disk.data.into_inner(), original);
    }
}
#[test]
fn optional_boot_is_explicit_and_noop_does_not_write() {
    let original = fixture(512);
    let a = archive(&original, 512, true);
    let mut changed = original.clone();
    changed[MIB as usize] ^= 1;
    let p = plan(&changed, &a, &[], false).unwrap();
    assert_eq!(p.ranges(), 0);
    let mut disk = Medium::new(changed.clone());
    p.execute(&mut disk, |_| panic!("no flush"), || Ok(()))
        .unwrap();
    assert!(disk.writes.is_empty());
    assert_eq!(disk.data.into_inner(), changed);
    let p = plan(&changed, &a, &[], true).unwrap();
    assert_eq!(p.ranges(), 1);
    let mut disk = Medium::new(changed.clone());
    p.execute(&mut disk, |_| Ok(()), || Ok(())).unwrap();
    assert_eq!(disk.data.into_inner(), original);
    assert!(plan(
        &changed,
        &a,
        &[archive_layout(&a).unwrap().entries[0].guid.clone()],
        true
    )
    .is_err());
    assert!(plan(&original, &archive(&original, 512, false), &[], true).is_err());
}
#[test]
fn preserves_protected_entries_and_rejects_unknown_types() {
    let mut original = fixture(512);
    edit_entries(&mut original, 512, |entries| {
        entries[..16].copy_from_slice(
            &uuid::Uuid::parse_str("e3c9e316-0b5c-4db8-817d-f92df00215ae")
                .unwrap()
                .to_bytes_le(),
        )
    });
    let a = archive(&original, 512, false);
    assert!(plan(&original, &a, &[], false).is_ok());
    let mut changed = original.clone();
    edit_entries(&mut changed, 512, |e| e[56] ^= 1);
    assert!(plan(&changed, &a, &[], false).is_err());
    edit_entries(&mut original, 512, |e| e[..16].fill(0x35));
    let a = archive(&original, 512, false);
    assert!(plan(&original, &a, &[], false).is_err());
    let original = fixture(512);
    let a = archive(&original, 512, false);
    let mut current = original.clone();
    added(&mut current, 512);
    let current_archive = archive(&current, 512, false);
    let protected = archive_layout(&current_archive).unwrap().entries[1]
        .guid
        .clone();
    assert!(plan(&current, &a, &[protected], false).is_err());
}
#[test]
fn single_corrupt_copy_can_be_repaired_but_conflicts_both_bad_and_hybrid_fail() {
    for sector in [512, 4096] {
        let original = fixture(sector);
        let a = archive(&original, sector, false);
        for offset in [sector + 16, SIZE - sector + 16] {
            let mut damaged = original.clone();
            damaged[offset] ^= 1;
            let p = plan(&damaged, &a, &[], false).unwrap();
            assert_eq!(p.ranges(), 1);
            let mut disk = Medium::new(damaged);
            p.execute(&mut disk, |_| Ok(()), || Ok(())).unwrap();
            assert_eq!(disk.data.into_inner(), original);
        }
        let mut damaged = original.clone();
        damaged[sector + 16] ^= 1;
        damaged[SIZE - sector + 16] ^= 1;
        assert!(plan(&damaged, &a, &[], false).is_err());
        let mut conflict = original.clone();
        added(&mut conflict, sector);
        conflict[..MIB as usize].copy_from_slice(&original[..MIB as usize]);
        assert!(plan(&conflict, &a, &[], false).is_err());
        let mut hybrid = original.clone();
        hybrid[466] = 7;
        assert!(plan(&hybrid, &a, &[], false).is_err());
    }
}
#[test]
fn wrong_identity_geometry_native_fields_and_cancel_are_rejected_before_writes() {
    let bytes = fixture(512);
    let a = archive(&bytes, 512, false);
    let native = archive_layout(&a).unwrap();
    for n in 0..5 {
        let identity = "a".repeat(64);
        let mut altered = native.clone();
        if n == 2 {
            altered.entries[0].attributes = 1;
        }
        if n == 3 {
            altered.guid = "wrong".into();
        }
        let result = prepare(
            &mut Cursor::new(&bytes),
            &a,
            if n == 0 { "wrong" } else { &identity },
            SIZE as u64,
            if n == 1 { 4096 } else { 512 },
            &altered,
            &[],
            false,
            || if n == 4 { Err("cancel".into()) } else { Ok(()) },
        );
        assert!(result.is_err());
    }
    let mut renumbered = native.clone();
    renumbered.entries[0].slot = 19;
    assert!(native.agrees(&renumbered));
}
#[test]
fn stale_guards_short_writes_io_errors_flush_failures_and_bad_readback_never_retry() {
    let original = fixture(512);
    let a = archive(&original, 512, false);
    let mut current = original.clone();
    added(&mut current, 512);
    let p = plan(&current, &a, &[], false).unwrap();
    let mut stale = Medium::new(current.clone());
    stale.data.get_mut()[8000] ^= 1;
    assert!(p.execute(&mut stale, |_| Ok(()), || Ok(())).is_err());
    assert!(stale.writes.is_empty());
    for fault in 1..=4 {
        let mut disk = Medium::new(current.clone());
        disk.fault = fault;
        let error = p
            .execute(
                &mut disk,
                |_| {
                    if fault == 4 {
                        Err("flush".into())
                    } else {
                        Ok(())
                    }
                },
                || Ok(()),
            )
            .err()
            .unwrap();
        assert!(error.contains("未重试或回滚"));
        assert_eq!(disk.writes.len(), 1);
    }
    let mut disk = Medium::new(current);
    let mut flushed = false;
    // Cancellation is checked between single-attempt durable ranges.
    let flag = std::cell::Cell::new(false);
    let error = p
        .execute(
            &mut disk,
            |_| {
                flushed = true;
                flag.set(true);
                Ok(())
            },
            || {
                if flag.get() {
                    Err("cancel".into())
                } else {
                    Ok(())
                }
            },
        )
        .err()
        .unwrap();
    assert!(flushed);
    assert_eq!(disk.writes.len(), 1);
    assert!(error.contains("此前已校验 1"));
}

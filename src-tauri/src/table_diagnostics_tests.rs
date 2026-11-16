use super::*;
use std::io::{self, Cursor};
fn inspect_bytes(bytes: &[u8], sector: u64) -> Snapshot {
    inspect(&mut Cursor::new(bytes), bytes.len() as u64, sector, || {
        Ok(())
    })
    .unwrap()
}
fn state(snapshot: &Snapshot, code: &str) -> CheckState {
    snapshot
        .report
        .items
        .iter()
        .find(|i| i.code == code)
        .unwrap()
        .state
        .clone()
}
#[test]
fn repairs_either_bad_copy_with_exact_original_bytes_and_no_layout_change() {
    for sector in [512, 4096] {
        let original = crate::table_archive::tests::gpt(sector);
        let identity = "a".repeat(64);
        for primary in [true, false] {
            for corruption in [0, 1, 2] {
                let mut bytes = original.clone();
                let at = if primary {
                    sector
                } else {
                    bytes.len() - sector
                };
                let array = u64::from_le_bytes(bytes[at + 72..at + 80].try_into().unwrap())
                    as usize
                    * sector;
                if corruption != 1 {
                    bytes[at + 16] ^= 1;
                }
                if corruption != 0 {
                    bytes[array + 18] ^= 1;
                }
                let snapshot = inspect_bytes(&bytes, sector as u64);
                assert_eq!(snapshot.report.state, CheckState::Failed);
                let repair = snapshot.repair(&identity).unwrap().unwrap();
                let native = snapshot.layout().unwrap();
                let plan = table_restore::prepare(
                    &mut Cursor::new(&bytes),
                    &repair.archive,
                    &identity,
                    bytes.len() as u64,
                    sector as u64,
                    &native,
                    &[],
                    false,
                    || Ok(()),
                )
                .unwrap();
                assert!(plan.changes.is_empty());
                assert_eq!(plan.ranges(), if corruption == 2 { 2 } else { 1 });
                let mut disk = Cursor::new(bytes);
                plan.execute(&mut disk, |_| Ok(()), || Ok(())).unwrap();
                assert_eq!(disk.into_inner(), original);
            }
        }
    }
}
#[test]
fn repair_preserves_target_array_padding_and_all_protected_partition_fields() {
    let mut original = crate::table_archive::tests::gpt(512);
    let len = original.len();
    let header = len - 512;
    let table =
        u64::from_le_bytes(original[header + 72..header + 80].try_into().unwrap()) as usize * 512;
    original[table + 17 * 128..table + 2560].fill(0x9c);
    let mut broken = original.clone();
    broken[table + 19] ^= 1;
    broken[header + 16] ^= 1;
    let snapshot = inspect_bytes(&broken, 512);
    let repair = snapshot.repair(&"a".repeat(64)).unwrap().unwrap();
    let native = snapshot.layout().unwrap();
    let protected = native
        .entries
        .iter()
        .map(|e| e.guid.clone())
        .collect::<Vec<_>>();
    let plan = table_restore::prepare(
        &mut Cursor::new(&broken),
        &repair.archive,
        &"a".repeat(64),
        len as u64,
        512,
        &native,
        &protected,
        false,
        || Ok(()),
    )
    .unwrap();
    assert!(plan.changes.is_empty());
    let mut disk = Cursor::new(broken);
    plan.execute(&mut disk, |_| Ok(()), || Ok(())).unwrap();
    assert_eq!(disk.into_inner(), original);
}
#[test]
fn healthy_conflicting_double_bad_hybrid_wrong_sector_and_unsupported_tables_are_not_repaired() {
    let original = crate::table_archive::tests::gpt(512);
    assert!(inspect_bytes(&original, 512)
        .repair(&"a".repeat(64))
        .unwrap()
        .is_none());
    for kind in 0..5 {
        let mut bytes = original.clone();
        let end = bytes.len();
        match kind {
            0 => {
                bytes[528] ^= 1;
                bytes[end - 512 + 16] ^= 1;
            }
            1 => {
                bytes[466] = 7;
                bytes[end - 512 + 16] ^= 1;
            }
            2 => {}
            3 => {
                // An independently valid but different backup cannot be chosen automatically.
                let at = end - 512;
                bytes[at + 56] ^= 1;
                bytes[at + 16..at + 20].fill(0);
                let crc = crc32fast::hash(&bytes[at..at + 92]);
                bytes[at + 16..at + 20].copy_from_slice(&crc.to_le_bytes());
            }
            _ => {
                bytes[510] = 0;
                bytes[end - 512 + 16] ^= 1;
            }
        }
        let snapshot = inspect_bytes(&bytes, if kind == 2 { 4096 } else { 512 });
        assert!(snapshot.repair(&"a".repeat(64)).is_err());
    }
}
#[test]
fn mbr_chain_is_fully_read_but_cycles_and_four_kn_are_not_reported_as_passed() {
    let original = crate::table_archive::tests::mbr();
    let s = inspect_bytes(&original, 512);
    assert_eq!(s.ebrs.len(), 2);
    assert_eq!(state(&s, "ebr-chain"), CheckState::Passed);
    assert!(s.repair(&"a".repeat(64)).is_err());
    let s = inspect_bytes(&original, 4096);
    assert!(s.ebrs.is_empty());
    assert_eq!(state(&s, "mbr-layout"), CheckState::Unchecked);
    let mut cycle = original.clone();
    let at = 2 * MIB as usize + 462;
    cycle[at + 4] = 0x0f;
    cycle[at + 8..at + 12].copy_from_slice(&2048u32.to_le_bytes());
    cycle[at + 12..at + 16].copy_from_slice(&4096u32.to_le_bytes());
    let s = inspect_bytes(&cycle, 512);
    assert_eq!(s.report.state, CheckState::Failed);
}
#[test]
fn stale_bytes_cancel_invalid_geometry_and_read_errors_do_not_publish_success() {
    struct Reader {
        inner: Cursor<Vec<u8>>,
        reads: usize,
        fail: bool,
    }
    impl Read for Reader {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.fail {
                Err(io::Error::other("read error"))
            } else {
                self.inner.read(b)
            }
        }
    }
    impl Seek for Reader {
        fn seek(&mut self, s: SeekFrom) -> io::Result<u64> {
            self.inner.seek(s)
        }
    }
    let bytes = crate::table_archive::tests::gpt(512);
    let size = bytes.len() as u64;
    let mut r = Reader {
        inner: Cursor::new(bytes.clone()),
        reads: 0,
        fail: false,
    };
    assert!(inspect(&mut r, size, 1024, || Ok(())).is_err());
    assert_eq!(r.reads, 0);
    assert!(inspect(&mut r, size, 512, || Err("cancel".into())).is_err());
    assert_eq!(r.reads, 0);
    r.fail = true;
    assert!(inspect(&mut r, size, 512, || Ok(())).is_err());
    let s = inspect_bytes(&bytes, 512);
    let mut altered = bytes;
    altered[8000] ^= 1;
    assert!(s.matches(&mut Cursor::new(altered), || Ok(())).is_err());
}

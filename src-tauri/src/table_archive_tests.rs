use super::*;
use std::io::{self, Cursor};
const SIZE: usize = 8 * MIB as usize;
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes())
}
fn put64(b: &mut [u8], at: usize, n: u64) {
    b[at..at + 8].copy_from_slice(&n.to_le_bytes())
}
pub(crate) fn gpt(sector: usize) -> Vec<u8> {
    let mut bytes = vec![0; SIZE];
    let last = SIZE / sector - 1;
    bytes[450] = 0xee;
    put32(&mut bytes, 454, 1);
    put32(&mut bytes, 458, last as u32);
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
    // A partial last array sector exercises preserved padding, outside its CRC.
    let count = 17;
    let table_len: usize = count * 128;
    let rounded = table_len.div_ceil(sector) * sector;
    let mut entries = vec![0; rounded];
    entries[..16].copy_from_slice(&[
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ]);
    entries[16..32].fill(0x42);
    put64(&mut entries, 32, MIB / sector as u64);
    put64(&mut entries, 40, 2 * MIB / sector as u64 - 1);
    for (i, c) in "data".encode_utf16().enumerate() {
        entries[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    entries[table_len..].fill(0x6d);
    let crc = crc32fast::hash(&entries[..table_len]);
    for (own, alternate, table_at) in [(1, last, 2), (last, 1, last - rounded / sector)] {
        let mut h = vec![0; sector];
        h[..8].copy_from_slice(b"EFI PART");
        put32(&mut h, 8, 0x10000);
        put32(&mut h, 12, 92);
        put64(&mut h, 24, own as u64);
        put64(&mut h, 32, alternate as u64);
        put64(&mut h, 40, 256);
        put64(&mut h, 48, (last - 256) as u64);
        h[56..72].fill(0x31);
        put64(&mut h, 72, table_at as u64);
        put32(&mut h, 80, count as u32);
        put32(&mut h, 84, 128);
        put32(&mut h, 88, crc);
        let checksum = crc32fast::hash(&h[..92]);
        put32(&mut h, 16, checksum);
        bytes[own * sector..(own + 1) * sector].copy_from_slice(&h);
        bytes[table_at * sector..table_at * sector + rounded].copy_from_slice(&entries);
    }
    bytes[MIB as usize..MIB as usize + sector].fill(0x25);
    bytes[MIB as usize + sector..MIB as usize + sector * 2].fill(0x7e);
    bytes
}
fn captured(bytes: Vec<u8>, sector: u64, boot: bool) -> Archive {
    capture(
        &mut Cursor::new(bytes),
        SIZE as u64,
        sector,
        "a".repeat(64),
        boot,
        || Ok(()),
    )
    .unwrap()
}
#[test]
fn gpt_roundtrip_contains_only_exact_tables_and_optional_first_partition_sectors() {
    for sector in [512, 4096] {
        let bytes = gpt(sector);
        for boot in [false, true] {
            let archive = captured(bytes.clone(), sector as u64, boot);
            assert_eq!(archive.extents.len(), if boot { 6 } else { 5 });
            for e in &archive.extents {
                assert_eq!(
                    e.bytes,
                    bytes[e.offset as usize..e.offset as usize + e.bytes.len()]
                );
            }
            let info = archive.validate().unwrap();
            assert_eq!(info.scheme, "GPT");
            assert_eq!(info.partitions, 1);
            assert_eq!(info.boot_bytes, if boot { sector as u64 } else { 0 });
            let expected = 3 * sector + 2 * (17usize * 128).div_ceil(sector) * sector;
            assert_eq!(info.table_bytes, expected as u64);
            let round = Archive::decode(&archive.encode().unwrap()).unwrap();
            assert_eq!(round.extents, archive.extents);
            assert_eq!(round.validate().unwrap().sha256, info.sha256);
            assert!(!archive
                .extents
                .iter()
                .any(|e| e.bytes.iter().all(|&b| b == 0x7e)));
        }
    }
}
fn mbr_entry(bytes: &mut [u8], slot: usize, kind: u8, lba: u32, count: u32) {
    let at = 446 + slot * 16;
    bytes[at + 4] = kind;
    put32(bytes, at + 8, lba);
    put32(bytes, at + 12, count);
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
}
pub(crate) fn mbr() -> Vec<u8> {
    let mut b = vec![0; SIZE];
    mbr_entry(&mut b, 0, 0x0f, 2048, 8192);
    mbr_entry(&mut b, 1, 7, 12288, 1024);
    mbr_entry(&mut b[1048576..1049088], 0, 0x0c, 1, 1023);
    mbr_entry(&mut b[1048576..1049088], 1, 0x0f, 2048, 4096);
    mbr_entry(&mut b[2097152..2097664], 0, 7, 1, 1023);
    b[1049088..1049600].fill(31);
    b[2097664..2098176].fill(32);
    b[6291456..6291968].fill(33);
    b
}
#[test]
fn mbr_extended_chain_and_each_data_boot_sector_are_preserved_without_container_bytes() {
    let archive = captured(mbr(), 512, true);
    assert_eq!(archive.validate().unwrap().partitions, 3);
    assert_eq!(
        archive.extents.iter().map(|e| e.offset).collect::<Vec<_>>(),
        [0, 1048576, 1049088, 2097152, 2097664, 6291456]
    );
    assert_eq!(
        archive
            .extents
            .iter()
            .filter(|e| e.kind == Kind::Ebr)
            .count(),
        2
    );
    assert_eq!(
        archive
            .extents
            .iter()
            .filter(|e| e.kind == Kind::Boot)
            .count(),
        3
    );
    assert!(capture(
        &mut Cursor::new(mbr()),
        SIZE as u64,
        4096,
        "a".repeat(64),
        true,
        || Ok(())
    )
    .is_err());
    let mut cyclic = mbr();
    mbr_entry(&mut cyclic[2097152..2097664], 1, 0x0f, 2048, 4096);
    assert!(capture(
        &mut Cursor::new(cyclic),
        SIZE as u64,
        512,
        "a".repeat(64),
        true,
        || Ok(())
    )
    .is_err());
}
#[test]
fn altered_file_truncation_extra_data_and_structurally_valid_hashes_do_not_bypass_validation() {
    let a = captured(gpt(512), 512, true);
    let original = a.encode().unwrap();
    for at in [0, 8, 12, 43, 44, original.len() - 1] {
        let mut b = original.clone();
        b[at] ^= 1;
        assert!(Archive::decode(&b).is_err());
    }
    for len in [0, 7, 43, 44, original.len() - 1] {
        assert!(Archive::decode(&original[..len]).is_err());
    }
    let mut extra = original.clone();
    extra.push(0);
    assert!(Archive::decode(&extra).is_err());
    for change in 0..7 {
        let mut altered = a.clone();
        match change {
            0 => {
                altered.extents.remove(2);
            }
            1 => {
                altered.extents[2].offset += 512;
            }
            2 => {
                altered.extents[0].kind = Kind::Boot;
            }
            3 => {
                altered.includes_boot = false;
            }
            4 => {
                altered.extents[2].bytes[10] ^= 1;
            }
            5 => {
                altered.source = "not-an-identity".into();
            }
            _ => {
                altered.extents.push(Extent {
                    kind: Kind::Boot,
                    offset: SIZE as u64 - 512,
                    bytes: vec![0; 512],
                });
            }
        }
        let payload = serde_json::to_vec(&altered).unwrap();
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        file.extend_from_slice(&Sha256::digest(&payload));
        file.extend_from_slice(&payload);
        assert!(Archive::decode(&file).is_err(), "case {change}");
    }
}
#[test]
fn gpt_disagreement_damage_hybrid_and_wrong_geometry_fail_before_archive_publication() {
    for change in 0..5 {
        let mut b = gpt(512);
        match change {
            0 => b[512 + 16] ^= 1,
            1 => b[SIZE - 512 + 16] ^= 1,
            2 => b[1024] ^= 1,
            3 => mbr_entry(&mut b, 1, 7, 2048, 1024),
            _ => {
                b[4096..4104].copy_from_slice(b"EFI PART");
            }
        }
        assert!(capture(
            &mut Cursor::new(b),
            SIZE as u64,
            512,
            "a".repeat(64),
            true,
            || Ok(())
        )
        .is_err());
    }
    assert!(capture(
        &mut Cursor::new(gpt(512)),
        SIZE as u64,
        4096,
        "a".repeat(64),
        false,
        || Ok(())
    )
    .is_err());
    assert!(capture(
        &mut Cursor::new(vec![0; 512]),
        u64::MAX,
        512,
        "a".repeat(64),
        false,
        || Ok(())
    )
    .is_err());
}
struct Traced {
    data: Cursor<Vec<u8>>,
    reads: Vec<(u64, usize)>,
    calls: usize,
    mutate_at: Option<usize>,
}
impl Read for Traced {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.mutate_at == Some(self.calls) {
            self.data.get_mut()[MIB as usize] ^= 1;
        }
        let at = self.data.position();
        let n = self.data.read(b)?;
        self.reads.push((at, n));
        Ok(n)
    }
}
impl Seek for Traced {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.data.seek(p)
    }
}
#[test]
fn capture_checks_cancellation_and_rereads_before_publishing_without_writes() {
    let mut source = Traced {
        data: Cursor::new(gpt(512)),
        reads: vec![],
        calls: 0,
        mutate_at: Some(12),
    };
    assert!(
        capture(&mut source, SIZE as u64, 512, "a".repeat(64), true, || Ok(
            ()
        ))
        .is_err()
    );
    let mut source = Traced {
        data: Cursor::new(gpt(512)),
        reads: vec![],
        calls: 0,
        mutate_at: None,
    };
    let mut checkpoints = 0;
    assert!(
        capture(&mut source, SIZE as u64, 512, "a".repeat(64), true, || {
            checkpoints += 1;
            if checkpoints == 3 {
                Err("stop".into())
            } else {
                Ok(())
            }
        })
        .is_err()
    );
    assert_eq!(
        source.reads,
        [(0, MIB as usize), (SIZE as u64 - MIB, MIB as usize)]
    );
    let a = captured(gpt(512), 512, true);
    let mut changed = gpt(512);
    changed[MIB as usize] ^= 1;
    assert!(a.matches(&mut Cursor::new(changed), || Ok(())).is_err());
}
#[test]
fn encoded_archive_rejects_unknown_fields_and_excessive_json_before_recovery() {
    let a = captured(mbr(), 512, false);
    let mut json = serde_json::to_value(a).unwrap();
    json["path"] = serde_json::json!("device");
    let payload = serde_json::to_vec(&json).unwrap();
    let mut file = MAGIC.to_vec();
    file.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    file.extend_from_slice(&Sha256::digest(&payload));
    file.extend_from_slice(&payload);
    assert!(Archive::decode(&file).is_err());
    assert!(Archive::decode(&vec![0; MAX_FILE + 1]).is_err());
}

#[test]
fn saturated_pmbr_is_preserved_with_warning_through_capture_and_file_verification() {
    for sector in [512, 4096] {
        let mut bytes = gpt(sector);
        put32(&mut bytes, 458, u32::MAX);
        let archive = captured(bytes.clone(), sector as u64, true);
        let restored = Archive::decode(&archive.encode().unwrap()).unwrap();
        assert_eq!(restored.extents[0].bytes, bytes[..sector]);
        assert_eq!(
            restored.validate().unwrap().report.state,
            CheckState::Warning
        );
        let mut broken = bytes.clone();
        broken[SIZE - sector + 16] ^= 1;
        assert!(capture(
            &mut Cursor::new(broken),
            SIZE as u64,
            sector as u64,
            "a".repeat(64),
            true,
            || Ok(())
        )
        .is_err());
    }
}

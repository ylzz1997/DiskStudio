use super::*;
use std::io::Cursor;

fn report(bytes: &[u8], sector: u64) -> Report {
    inspect(&mut Cursor::new(bytes), bytes.len() as u64, sector).unwrap()
}
fn repaired(bytes: &[u8], sector: u64, choice: Side) -> (Vec<u8>, Changes) {
    let digest = report(bytes, sector).digest;
    let plan = prepare(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        sector,
        choice,
        &digest,
    )
    .unwrap();
    let mut output = Vec::new();
    plan.reader(Cursor::new(bytes))
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    plan.verify_output(&mut Cursor::new(&output), || Ok(()))
        .unwrap();
    (output, plan.preview.gpt_repair.unwrap())
}
fn header_crc(bytes: &mut [u8], at: usize) {
    bytes[at + 16..at + 20].fill(0);
    let crc = crc32fast::hash(&bytes[at..at + 92]);
    bytes[at + 16..at + 20].copy_from_slice(&crc.to_le_bytes());
}
fn array_crc(bytes: &mut [u8], at: usize, sector: usize) {
    let start = u64le(&bytes[at + 72..at + 80]) as usize * sector;
    let count = u32le(&bytes[at + 80..at + 84]) as usize;
    let stride = u32le(&bytes[at + 84..at + 88]) as usize;
    let crc = crc32fast::hash(&bytes[start..start + count * stride]);
    bytes[at + 88..at + 92].copy_from_slice(&crc.to_le_bytes());
    header_crc(bytes, at);
}

#[test]
fn rebuilds_either_failed_copy_with_only_the_original_table_bytes() {
    for sector in [512, 4096] {
        let original = table_archive::tests::gpt(sector);
        let size = original.len();
        for side in [Side::Primary, Side::Backup] {
            let mut damaged = original.clone();
            let at = side.opposite().header_at(size as u64, sector as u64) as usize;
            let array = u64le(&damaged[at + 72..at + 80]) as usize * sector;
            damaged[at + 16] ^= 0x40;
            damaged[array + 60] ^= 0x40;
            let (output, changes) = repaired(&damaged, sector as u64, side);
            assert_eq!(output, original);
            assert_eq!(changes.authority, side);
            assert!(changes.repair_peer);
            assert!(!changes.repair_protective_mbr);
        }
    }
}

#[test]
fn accepts_explicit_authority_in_either_direction_without_touching_either_layouts_data() {
    let sector = 4096;
    let original = table_archive::tests::gpt(sector);
    let size = original.len();
    let mut source = original.clone();
    let backup_at = size - sector;
    let backup_array = u64le(&source[backup_at + 72..backup_at + 80]) as usize * sector;
    let old_start = u64le(&source[backup_array + 32..backup_array + 40]);
    let old_end = u64le(&source[backup_array + 40..backup_array + 48]);
    source[backup_array + 32..backup_array + 40].copy_from_slice(&(old_start + 1).to_le_bytes());
    source[backup_array + 40..backup_array + 48].copy_from_slice(&(old_end + 1).to_le_bytes());
    source[backup_array + 56..backup_array + 58].copy_from_slice(&('改' as u16).to_le_bytes());
    array_crc(&mut source, backup_at, sector);
    let datum = old_start as usize * sector;
    source[datum..datum + sector].fill(0x65);
    let inspected = report(&source, sector as u64);
    assert!(inspected.conflict);
    assert!(inspected.copies.iter().all(|copy| copy.changes.is_some()));
    for side in [Side::Primary, Side::Backup] {
        let (output, changes) = repaired(&source, sector as u64, side);
        assert!(changes.repair_peer);
        assert_eq!(output[datum..datum + sector], source[datum..datum + sector]);
        let selected = if side == Side::Primary { 0 } else { 1 };
        let completed = report(&output, sector as u64);
        assert!(!completed.conflict);
        let prior = inspected.copies[selected].layout.as_ref().unwrap();
        for copy in completed.copies {
            let layout = copy.layout.unwrap();
            assert_eq!(layout.partitions[0].offset, prior.partitions[0].offset);
            assert_eq!(layout.partitions[0].name, prior.partitions[0].name);
            assert_eq!(layout.disk_id, prior.disk_id);
        }
    }
}

#[test]
fn rebuilds_missing_protection_and_primary_using_only_a_valid_backup() {
    let sector = 4096;
    let mut image = table_archive::tests::gpt(sector);
    let last = image.len() as u64 / sector as u64 - 1;
    table_restore::write_protective_entries(&mut image, last);
    image[100] = 0x65;
    image[2500] = 0x71;
    let original = image.clone();
    image[446..512].fill(0);
    let (fixed, changes) = repaired(&image, sector as u64, Side::Primary);
    assert!(changes.repair_protective_mbr);
    assert!(!changes.repair_peer);
    assert_eq!(fixed, original);
    image[sector..2 * sector].fill(0);
    let (fixed, changes) = repaired(&image, sector as u64, Side::Backup);
    assert!(changes.repair_protective_mbr && changes.repair_peer);
    assert_eq!(fixed, original);
}

#[test]
fn a_larger_competing_array_cannot_overwrite_unknown_reserved_bytes() {
    let sector = 512;
    let mut image = table_archive::tests::gpt(sector);
    let last = image.len() / sector - 1;
    let primary_array = 2 * sector;
    image[primary_array + 17 * 128..primary_array + 128 * 128].fill(0);
    image[sector + 80..sector + 84].copy_from_slice(&128u32.to_le_bytes());
    array_crc(&mut image, sector, sector);
    let sentinel = (last - 32) * sector + 20;
    image[sentinel] = 0x9c;
    let diagnosis = report(&image, sector as u64);
    assert!(diagnosis.conflict);
    assert!(diagnosis.copies[0].changes.is_none());
    assert!(diagnosis.copies[0]
        .repair_error
        .as_deref()
        .unwrap()
        .contains("保留区域"));
    let (safe, _) = repaired(&image, sector as u64, Side::Backup);
    assert_eq!(safe[sentinel], image[sentinel]);
}

#[test]
fn refuses_hybrid_unsupported_peer_missing_authority_and_stale_snapshots() {
    let sector = 512;
    let original = table_archive::tests::gpt(sector);
    let end = original.len() - sector;
    assert!(report(&original, sector as u64)
        .copies
        .iter()
        .all(|p| p.changes.is_none()));
    let mut altered = original.clone();
    altered[466] = 0x07;
    let hybrid = report(&altered, sector as u64);
    assert_eq!(hybrid.protective_mbr, "blocked");
    assert!(hybrid.copies.iter().all(|p| p.changes.is_none()));
    altered = original.clone();
    altered[end + 8..end + 12].copy_from_slice(&0x20000u32.to_le_bytes());
    header_crc(&mut altered, end);
    let unsupported = report(&altered, sector as u64);
    assert!(unsupported.copies.iter().all(|p| p.changes.is_none()));
    assert!(unsupported.copies[0]
        .repair_error
        .as_deref()
        .unwrap()
        .contains("超出可核对范围"));
    altered[sector + 16] ^= 1;
    assert!(report(&altered, sector as u64)
        .copies
        .iter()
        .all(|p| p.layout.is_none()));
    altered = original.clone();
    altered[end + 16] ^= 1;
    let diagnosis = report(&altered, sector as u64);
    assert!(prepare(
        &mut Cursor::new(&altered),
        original.len() as u64,
        sector as u64,
        Side::Backup,
        &diagnosis.digest
    )
    .is_err());
    let plan = prepare(
        &mut Cursor::new(&altered),
        original.len() as u64,
        sector as u64,
        Side::Primary,
        &diagnosis.digest,
    )
    .unwrap();
    altered[100] ^= 1;
    assert!(plan
        .verify_source(&mut Cursor::new(&altered), || Ok(()))
        .is_err());
    assert!(prepare(
        &mut Cursor::new(&altered),
        original.len() as u64,
        sector as u64,
        Side::Primary,
        &diagnosis.digest
    )
    .is_err());
}

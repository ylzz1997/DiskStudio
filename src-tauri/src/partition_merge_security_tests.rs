use super::*;
use std::io::Cursor;

fn descriptor(reverse_order: bool) -> Vec<u8> {
    let mut bytes = vec![0; 40];
    bytes[0] = 1;
    bytes[2..4].copy_from_slice(&0x8004u16.to_le_bytes());
    let (sid, acl) = if reverse_order {
        (28u32, 20u32)
    } else {
        (20, 32)
    };
    bytes[4..8].copy_from_slice(&sid.to_le_bytes());
    bytes[16..20].copy_from_slice(&acl.to_le_bytes());
    bytes[sid as usize..sid as usize + 12].copy_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0]);
    bytes[acl as usize..acl as usize + 8].copy_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
    bytes
}

fn record(payload: &[u8], id: u32, offset: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(descriptor_hash(payload).to_le_bytes());
    bytes.extend(id.to_le_bytes());
    bytes.extend(offset.to_le_bytes());
    bytes.extend(((SDS_HEADER + payload.len()) as u32).to_le_bytes());
    bytes.extend(payload);
    bytes.resize(bytes.len().div_ceil(16) * 16, 0);
    bytes
}

#[test]
fn descriptor_comparison_uses_components_and_access_flags() {
    let left = Descriptor::read(descriptor(false)).unwrap();
    let swapped = Descriptor::read(descriptor(true)).unwrap();
    assert!(left.agrees(&swapped));
    let mut changed = descriptor(false);
    changed[28] = 33;
    assert!(!left.agrees(&Descriptor::read(changed).unwrap()));
    let mut null_acl = descriptor(false);
    null_acl[16..20].fill(0);
    let with_null_acl = Descriptor::read(null_acl.clone()).unwrap();
    assert!(!with_null_acl.agrees(&left));
    null_acl[2] = 0;
    assert!(!with_null_acl.agrees(&Descriptor::read(null_acl).unwrap()));
}

#[test]
fn rejects_out_of_bounds_owner_acl_and_overlapping_components() {
    let mut bytes = descriptor(false);
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Descriptor::read(bytes).is_err());
    let mut bytes = descriptor(false);
    bytes[34..36].copy_from_slice(&12u16.to_le_bytes());
    assert!(Descriptor::read(bytes).is_err());
    let mut bytes = descriptor(false);
    bytes[36..38].copy_from_slice(&1u16.to_le_bytes());
    assert!(Descriptor::read(bytes).is_err());
    let mut bytes = descriptor(false);
    bytes[8..12].copy_from_slice(&24u32.to_le_bytes());
    bytes[24..26].copy_from_slice(&[1, 0]);
    assert!(Descriptor::read(bytes).is_err());
}

#[test]
fn sds_reads_mirrored_buckets_and_rejects_missing_corrupt_or_duplicate_ids() {
    let first = record(&descriptor(false), 0x100, 0);
    let second = record(&descriptor(true), 0x101, (SDS_BUCKET * 2) as u64);
    let mut sds = vec![0; SDS_BUCKET * 3 + second.len()];
    for offset in [0, SDS_BUCKET] {
        sds[offset..offset + first.len()].copy_from_slice(&first);
    }
    for offset in [2 * SDS_BUCKET, 3 * SDS_BUCKET] {
        sds[offset..offset + second.len()].copy_from_slice(&second);
    }
    let requested = BTreeSet::from([0x100, 0x101]);
    let parsed = read_sds(&mut Cursor::new(&sds), &requested, || Ok(())).unwrap();
    assert!(parsed[&0x100].agrees(&parsed[&0x101]));

    assert!(read_sds(&mut Cursor::new(&sds), &BTreeSet::from([0x777]), || Ok(())).is_err());
    sds[3 * SDS_BUCKET + 25] ^= 1;
    assert!(read_sds(&mut Cursor::new(&sds), &requested, || Ok(())).is_err());
    sds[3 * SDS_BUCKET + 25] ^= 1;
    for offset in [2 * SDS_BUCKET, 3 * SDS_BUCKET] {
        sds[offset + 4..offset + 8].copy_from_slice(&0x100u32.to_le_bytes());
    }
    assert!(read_sds(&mut Cursor::new(&sds), &requested, || Ok(())).is_err());
}

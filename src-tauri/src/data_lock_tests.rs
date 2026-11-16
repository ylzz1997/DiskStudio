//! Pure metadata/IOCTL-buffer regressions. No WMI, devices or filesystem calls.
use super::*;
use std::collections::HashSet;

fn selected() -> Selection {
    Selection {
        target: Target("synthetic-target".into()),
        layout_key: "layout".into(),
        partition: Some(2),
    }
}
fn resolved() -> Resolved {
    Resolved {
        disk: Disk {
            number: 2,
            size: 16 * 1024 * 1024,
            serial_number: Some("synthetic".into()),
            is_offline: Some(false),
            is_read_only: Some(false),
            ..Default::default()
        },
        offset: 4096,
        size: 4096,
        label: String::new(),
        volumes: vec!["volume-a".into(), "volume-b".into()],
        ntfs_volume: false,
        partition_ranges: vec![],
        data_lock_proof: None,
    }
}
fn volume() -> Volume {
    Volume {
        path: "provider-path".into(),
        unique_id: Some("volume-a".into()),
        file_system: Some("NTFS".into()),
    }
}
fn extent(number: u32, offset: u64, size: u64) -> [u8; 32] {
    let mut b = [0; 32];
    b[0] = 1;
    b[8..12].copy_from_slice(&number.to_le_bytes());
    b[16..24].copy_from_slice(&offset.to_le_bytes());
    b[24..32].copy_from_slice(&size.to_le_bytes());
    b
}

#[test]
fn locks_cannot_be_transferred_or_reused_after_identity_or_protection_changes() {
    let r = resolved();
    let proof = LockIdentity::new(&r).unwrap();
    proof.validate(&r, 2).unwrap();
    assert!(proof.validate(&r, 1).is_err());
    assert!(proof.validate(&r, 3).is_err());
    let changes: [fn(&mut Resolved); 8] = [
        |r| r.disk.number += 1,
        |r| r.disk.size += 512,
        |r| r.disk.serial_number = Some("different".into()),
        |r| r.disk.is_offline = Some(true),
        |r| r.disk.is_read_only = Some(true),
        |r| r.disk.is_read_only = None,
        |r| r.volumes.reverse(),
        |r| r.volumes.pop().map(drop).unwrap(),
    ];
    for change in changes {
        let mut changed = r.clone();
        change(&mut changed);
        assert!(proof.validate(&changed, 2).is_err());
    }
    let mut duplicate = r.clone();
    duplicate.volumes[1] = duplicate.volumes[0].clone();
    assert!(LockIdentity::new(&duplicate).is_err());
    let mut unknown = r;
    unknown.disk.is_offline = None;
    assert!(LockIdentity::new(&unknown).is_err());
}

#[test]
fn offline_zero_locks_still_require_complete_matching_native_attributes() {
    let mut r = resolved();
    r.disk.is_offline = Some(true);
    let proof = LockIdentity::new(&r).unwrap();
    proof.validate(&r, 0).unwrap();
    assert!(proof.validate(&r, 2).is_err());
    let mut native = [0; 16];
    native[0] = 16;
    native[8] = 1;
    validate_offline_attributes(&r.disk, &native).unwrap();
    assert!(validate_offline_attributes(&r.disk, &native[..15]).is_err());
    native[0] = 0;
    assert!(validate_offline_attributes(&r.disk, &native).is_err());
    native[0] = 16;
    for flags in [0, 2, 3] {
        native[8] = flags;
        assert!(validate_offline_attributes(&r.disk, &native).is_err());
    }
    native[8] = 1;
    r.disk.is_offline = Some(false);
    assert!(validate_offline_attributes(&r.disk, &native).is_err());
    r.disk.is_offline = Some(true);
    r.disk.is_read_only = None;
    assert!(validate_offline_attributes(&r.disk, &native).is_err());
}

#[test]
fn destination_partition_locks_all_associated_volumes_but_source_locks_only_selected() {
    let s = selected();
    assert!(!lock_scope(&s, false, 1));
    assert!(lock_scope(&s, false, 2));
    assert!(lock_scope(&s, true, 1));
    assert!(lock_scope(&s, true, 2));
    let mut whole = s.clone();
    whole.partition = None;
    assert!(lock_scope(&whole, false, 1));
    let matches = |offset, size| size == 4096 && [4096, 16384].contains(&offset);
    let mut seen = HashSet::new();
    validate_lock_extent(&extent(2, 4096, 4096), 2, &matches, &mut seen).unwrap();
    validate_lock_extent(&extent(2, 16384, 4096), 2, &matches, &mut seen).unwrap();
    assert!(validate_lock_extent(&extent(2, 4096, 4096), 2, &matches, &mut seen).is_err());
    for bad in [
        extent(3, 4096, 4096),
        extent(2, 8192, 4096),
        extent(2, 4096, 8192),
        extent(2, 4096, 0),
    ] {
        assert!(validate_lock_extent(&bad, 2, &matches, &mut HashSet::new()).is_err());
    }
    assert!(validate_lock_extent(
        &extent(2, 4096, 4096)[..31],
        2,
        &matches,
        &mut HashSet::new()
    )
    .is_err());
}

#[test]
fn retained_associations_allow_only_absence_not_conflict_or_ambiguity() {
    let old = volume();
    assert!(retained_association(vec![], None).unwrap().is_none());
    assert!(retained_association(vec![], Some(&old)).unwrap().as_ref() == Some(&old));
    assert!(
        retained_association(vec![old.clone()], Some(&old))
            .unwrap()
            .as_ref()
            == Some(&old)
    );
    assert!(retained_association(vec![old.clone(), old.clone()], Some(&old)).is_err());
    let mut changed = old.clone();
    changed.unique_id = Some("other".into());
    assert!(retained_association(vec![changed], Some(&old)).is_err());
    let mut changed = old.clone();
    changed.file_system = None;
    assert!(retained_association(vec![changed], Some(&old)).is_err());
}

#[test]
fn retained_scope_and_fingerprint_bind_entire_layout_and_protections() {
    let s = selected();
    let r = resolved();
    let p = Partition {
        partition_number: 2,
        offset: 4096,
        size: 4096,
        is_hidden: Some(false),
        ..Default::default()
    };
    let v = vec![(2, volume())];
    let fingerprint = lock_fingerprint(&r.disk, &[&p], &v, &s, true).unwrap();
    let proof = DataLockProof {
        source: s.clone(),
        writing: true,
        fingerprint: fingerprint.clone(),
        associations: v.clone(),
        ranges: vec![],
    };
    validate_proof_selection(&proof, &s, true).unwrap();
    assert!(validate_proof_selection(&proof, &s, false).is_err());
    let mut other = s.clone();
    other.partition = Some(1);
    assert!(validate_proof_selection(&proof, &other, true).is_err());
    other = s.clone();
    other.target = Target("other".into());
    assert!(validate_proof_selection(&proof, &other, true).is_err());
    other = s.clone();
    other.layout_key = "changed".into();
    assert!(validate_proof_selection(&proof, &other, true).is_err());
    for change in [0, 1, 2] {
        let mut changed = p.clone();
        match change {
            0 => changed.offset += 512,
            1 => changed.is_hidden = None,
            _ => changed.is_read_only = Some(true),
        }
        assert_ne!(
            fingerprint,
            lock_fingerprint(&r.disk, &[&changed], &v, &s, true).unwrap()
        );
    }
    let neighbor = Partition {
        partition_number: 3,
        offset: 16384,
        size: 4096,
        ..Default::default()
    };
    assert_ne!(
        fingerprint,
        lock_fingerprint(&r.disk, &[&p, &neighbor], &v, &s, true).unwrap()
    );
    assert_ne!(
        fingerprint,
        lock_fingerprint(&r.disk, &[&p], &[], &s, true).unwrap()
    );
}

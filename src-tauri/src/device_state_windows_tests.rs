use super::*;
fn disk() -> Disk {
    Disk {
        path: "disk".into(),
        number: 1,
        partition_style: Some(1),
        size: 16 * policy::GIB,
        logical_sector_size: Some(512),
        ..Default::default()
    }
}
fn row(d: &Disk) -> Identity {
    Identity {
        path: d.path.clone(),
        number: d.number,
        partition_style: d.partition_style,
        guid: None,
        signature: Some(17),
        boot_from_disk: Some(false),
        offline_reason: Some(1),
    }
}
#[test]
fn identity_collision_offline_reason_and_boot_source_are_checked() {
    let a = disk();
    let mut b = a.clone();
    b.number = 2;
    b.path = "other".into();
    let ra = row(&a);
    let mut rb = row(&b);
    let disks = [a.clone(), b];
    assert!(identity_check(
        &[ra.clone(), rb.clone()],
        &disks,
        &[],
        &a,
        StateAction::Online
    )
    .is_err());
    rb.signature = Some(18);
    assert!(identity_check(
        &[ra.clone(), rb.clone()],
        &disks,
        &[],
        &a,
        StateAction::Online
    )
    .is_ok());
    for reason in [None, Some(2), Some(3), Some(4), Some(5), Some(6), Some(7)] {
        let mut r = ra.clone();
        r.offline_reason = reason;
        assert!(identity_check(&[r, rb.clone()], &disks, &[], &a, StateAction::Online).is_err());
    }
    let mut r = ra.clone();
    r.boot_from_disk = None;
    assert!(identity_check(&[r, rb.clone()], &disks, &[], &a, StateAction::ReadOnly).is_err());
    rb.signature = None;
    assert!(identity_check(&[ra, rb], &disks, &[], &a, StateAction::Online).is_err());
}
#[test]
fn native_mbr_geometry_types_and_bounds_are_cross_checked() {
    const H: usize = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    const E: usize = size_of::<PARTITION_INFORMATION_EX>();
    let d = disk();
    let mut b = vec![0u8; H + 4 * E];
    b[4..8].copy_from_slice(&4u32.to_le_bytes());
    b[8..12].copy_from_slice(&17u32.to_le_bytes());
    // SDK layout fixture: partition style, aligned offset/length, partition number,
    // then the MBR union (type 0x07); all padding/booleans start zero.
    assert_eq!(H, 48);
    assert_eq!(E, 144);
    b[H + 8..H + 16].copy_from_slice(&(policy::MIB as i64).to_le_bytes());
    b[H + 16..H + 24].copy_from_slice(&(2 * policy::MIB as i64).to_le_bytes());
    b[H + 24..H + 28].copy_from_slice(&1u32.to_le_bytes());
    b[H + 32] = 7;
    let p = Partition {
        partition_number: 1,
        offset: policy::MIB,
        size: 2 * policy::MIB,
        ..Default::default()
    };
    assert!(mbr_raw_layout(&b, &d, &[p.clone()], "mbr:00000011").is_ok());
    let mut bad = b.clone();
    bad[H + 32] = 0x42;
    assert!(mbr_raw_layout(&bad, &d, &[p.clone()], "mbr:00000011").is_err());
    let mut bad = b.clone();
    bad[H + 33] = 2;
    assert!(mbr_raw_layout(&bad, &d, &[p.clone()], "mbr:00000011").is_err());
    assert!(mbr_raw_layout(&b[..b.len() - 1], &d, &[p.clone()], "mbr:00000011").is_err());
    assert!(mbr_raw_layout(&b, &d, &[], "mbr:00000011").is_err());
    assert!(mbr_raw_layout(&b, &d, &[p.clone()], "mbr:00000012").is_err());
    let mut changed = p;
    changed.offset += 512;
    assert!(mbr_raw_layout(&b, &d, &[changed], "mbr:00000011").is_err());
    let mut raw = d;
    raw.partition_style = Some(0);
    let mut bytes = vec![0u8; H];
    bytes[..4].copy_from_slice(&2u32.to_le_bytes());
    assert!(mbr_raw_layout(&bytes, &raw, &[], "raw").is_ok());
    bytes[4] = 1;
    assert!(mbr_raw_layout(&bytes, &raw, &[], "raw").is_err());
}
#[test]
fn confirmation_is_consumed_on_mismatch_expiry_and_success() {
    fn ticket(now: Instant) -> Pending {
        Pending { request:serde_json::from_value(serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":null},"action":"online"})).unwrap(),preview:model::Preview{token:"once".into(),title:"x".into(),target:"fixture".into(),size:1,partitions:0,before:State{offline:true,read_only:false},after:State{offline:false,read_only:false},warning:"".into()},expected:ExpectedState{fingerprint:"".into(),identity:"".into(),native:"".into(),state:State{offline:true,read_only:false}},deadline:now+Duration::from_secs(300) }
    }
    let now = Instant::now();
    for (token, time, ok) in [
        ("bad", now, false),
        ("once", now + Duration::from_secs(300), false),
        ("once", now, true),
    ] {
        let mut s = Some(ticket(now));
        assert_eq!(consume(&mut s, token, time).is_ok(), ok);
        assert!(s.is_none());
        assert!(consume(&mut s, "once", now).is_err());
    }
}

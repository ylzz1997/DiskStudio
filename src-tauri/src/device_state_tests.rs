use super::*;
fn disk() -> Disk {
    Disk {
        path: "fixture-disk".into(),
        number: 1,
        size: 16 * policy::GIB,
        logical_sector_size: Some(512),
        partition_style: Some(2),
        number_of_partitions: Some(0),
        bus_type: Some(17),
        is_boot: Some(false),
        is_system: Some(false),
        is_offline: Some(false),
        is_read_only: Some(false),
        is_clustered: Some(false),
        ..Default::default()
    }
}
#[test]
fn actions_only_change_one_attribute_and_reject_noops() {
    for offline in [false, true] {
        for read_only in [false, true] {
            let mut d = disk();
            d.is_offline = Some(offline);
            d.is_read_only = Some(read_only);
            let before = State { offline, read_only };
            for action in [
                Action::Online,
                Action::Offline,
                Action::ReadOnly,
                Action::ReadWrite,
            ] {
                let after = action.desired(before);
                let allowed = after != before
                    && !(offline && matches!(action, Action::ReadOnly | Action::ReadWrite));
                assert_eq!(eligible(&d, &[], action).is_ok(), allowed);
                match action {
                    Action::Online | Action::Offline => {
                        assert_eq!(after.read_only, before.read_only)
                    }
                    _ => assert_eq!(after.offline, before.offline),
                }
            }
        }
    }
    let mut raw = disk();
    raw.partition_style = Some(0);
    assert!(eligible(&raw, &[], Action::ReadOnly).is_err());
    assert!(eligible(&raw, &[], Action::Offline).is_ok());
    let mut mbr = disk();
    mbr.partition_style = Some(1);
    assert!(eligible(&mbr, &[], Action::ReadOnly).is_ok());
}
#[test]
fn protected_missing_state_and_unsupported_device_rejected() {
    for i in 0..9 {
        let mut d = disk();
        match i {
            0 => d.is_boot = Some(true),
            1 => d.is_system = Some(true),
            2 => d.is_clustered = Some(true),
            3 => d.is_boot = None,
            4 => d.is_offline = None,
            5 => d.is_read_only = None,
            6 => d.bus_type = Some(16),
            7 => d.logical_sector_size = None,
            _ => d.path.clear(),
        }
        assert!(eligible(&d, &[], Action::ReadOnly).is_err());
    }
    assert!(eligible(
        &disk(),
        &[Partition {
            is_boot: Some(true),
            is_system: Some(false),
            ..Default::default()
        }],
        Action::Offline
    )
    .is_err());
    assert!(eligible(
        &disk(),
        &[Partition {
            is_boot: Some(false),
            is_system: None,
            ..Default::default()
        }],
        Action::Offline
    )
    .is_err());
}
#[test]
fn cloned_partition_guid_and_incomplete_inventory_refuse_activation() {
    let mut a = disk();
    a.number_of_partitions = Some(1);
    let mut b = a.clone();
    b.number = 2;
    b.path = "fixture-b".into();
    let p = Partition {
        path: "fixture-part-a".into(),
        disk_number: 1,
        partition_number: 1,
        offset: policy::MIB,
        size: policy::MIB,
        guid: Some(uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    };
    let mut q = p.clone();
    q.path = "fixture-part-b".into();
    q.disk_number = 2;
    assert!(unique_partition_ids(&[a.clone(), b.clone()], &[p.clone(), q.clone()], 1).is_err());
    q.guid = Some(uuid::Uuid::new_v4().to_string());
    assert!(unique_partition_ids(&[a.clone(), b.clone()], &[p.clone(), q.clone()], 1).is_ok());
    assert!(unique_partition_ids(&[a.clone(), b.clone()], &[p.clone()], 1).is_err());
    q.guid = None;
    assert!(unique_partition_ids(&[a, b], &[p, q], 1).is_err());
}
#[test]
fn ipc_accepts_only_typed_action_and_opaque_selection() {
    let value = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":null},"action":"online"});
    assert!(serde_json::from_value::<Request>(value.clone()).is_ok());
    for field in ["path", "method", "guid", "force", "isReadOnly"] {
        let mut v = value.clone();
        v[field] = serde_json::json!("injected");
        assert!(serde_json::from_value::<Request>(v).is_err());
    }
    let mut v = value;
    v["action"] = serde_json::json!("SetAttributes");
    assert!(serde_json::from_value::<Request>(v).is_err());
}

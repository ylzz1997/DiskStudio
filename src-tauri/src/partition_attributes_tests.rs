use super::*;
use crate::partitions::policy::{Disk, Partition, BASIC_DATA};
use crate::table_archive::Kind;
use std::io::Cursor;
fn fixture(sector: usize) -> Archive {
    let bytes = crate::table_archive::tests::gpt(sector);
    crate::table_archive::capture(
        &mut Cursor::new(&bytes),
        bytes.len() as u64,
        sector as u64,
        "a".repeat(64),
        false,
        || Ok(()),
    )
    .unwrap()
}
#[test]
fn all_flags_roundtrip_preserves_every_other_byte_on_512_and_4kn() {
    for sector in [512, 4096] {
        let original = fixture(sector);
        let id = table_restore::archive_layout(&original).unwrap().entries[0]
            .guid
            .clone();
        let mut current = original.clone();
        for (action, bits) in [
            (Action::Hide, HIDDEN),
            (Action::ReadOnly, HIDDEN | READ_ONLY),
            (Action::NoDefaultLetter, ALLOWED),
            (Action::Show, READ_ONLY | NO_LETTER),
            (Action::ReadWrite, NO_LETTER),
            (Action::AutomaticLetter, 0),
        ] {
            let after = expected(&current, &id, action).unwrap();
            assert_eq!(
                table_restore::archive_layout(&after).unwrap().entries[0].attributes,
                bits
            );
            for (old, new) in current.extents.iter().zip(&after.extents) {
                assert_eq!(old.offset, new.offset);
                assert_eq!(old.kind, new.kind);
                assert_eq!(old.bytes.len(), new.bytes.len());
                for (i, (a, b)) in old.bytes.iter().zip(&new.bytes).enumerate() {
                    if a != b {
                        assert!(match old.kind {
                            Kind::PrimaryEntries | Kind::BackupEntries => (48..56).contains(&i),
                            Kind::PrimaryHeader | Kind::BackupHeader =>
                                (16..20).contains(&i) || (88..92).contains(&i),
                            _ => false,
                        });
                    }
                }
            }
            assert!(expected(&after, &id, action).is_err());
            current = after;
        }
        assert_eq!(current.encode().unwrap(), original.encode().unwrap());
    }
}
#[test]
fn invalid_or_protected_gpt_is_rejected() {
    let original = fixture(512);
    let id = table_restore::archive_layout(&original).unwrap().entries[0]
        .guid
        .clone();
    assert!(expected(&original, "missing", Action::Hide).is_err());
    let mut bad = original.clone();
    bad.includes_boot = true;
    assert!(expected(&bad, &id, Action::Hide).is_err());
    let mut bad = original.clone();
    bad.extents[1].bytes[16] ^= 1;
    assert!(expected(&bad, &id, Action::Hide).is_err());
    // Valid checksums with an OEM-required flag still must not be editable.
    let mut protected = original;
    for e in &mut protected.extents {
        if matches!(e.kind, Kind::PrimaryEntries | Kind::BackupEntries) {
            e.bytes[48] = 1;
        }
    }
    let crc = crc32fast::hash(
        &protected
            .extents
            .iter()
            .find(|e| e.kind == Kind::PrimaryEntries)
            .unwrap()
            .bytes[..17 * 128],
    );
    for e in &mut protected.extents {
        if matches!(e.kind, Kind::PrimaryHeader | Kind::BackupHeader) {
            e.bytes[88..92].copy_from_slice(&crc.to_le_bytes());
            e.bytes[16..20].fill(0);
            let crc = crc32fast::hash(&e.bytes[..92]);
            e.bytes[16..20].copy_from_slice(&crc.to_le_bytes());
        }
    }
    assert!(protected.validate().is_ok());
    assert!(expected(&protected, &id, Action::Hide).is_err());
}
#[test]
fn only_known_unprotected_online_data_partitions_are_eligible() {
    let d = Disk {
        number: 1,
        partition_style: Some(2),
        is_offline: Some(false),
        is_read_only: Some(false),
        is_boot: Some(false),
        is_system: Some(false),
        is_clustered: Some(false),
        bus_type: Some(17),
        ..Default::default()
    };
    let p = Partition {
        path: "synthetic".into(),
        disk_number: 1,
        gpt_type: Some(BASIC_DATA.into()),
        mbr_type: None,
        is_boot: Some(false),
        is_system: Some(false),
        is_shadow_copy: Some(false),
        is_offline: Some(false),
        is_hidden: Some(false),
        is_read_only: Some(false),
        ..Default::default()
    };
    for hidden in [false, true] {
        for read_only in [false, true] {
            let mut p = p.clone();
            p.is_hidden = Some(hidden);
            p.is_read_only = Some(read_only);
            assert!(eligible(&d, &p).is_ok());
        }
    }
    for i in 0..10 {
        let mut d = d.clone();
        let mut p = p.clone();
        match i {
            0 => d.is_offline = Some(true),
            1 => d.is_read_only = Some(true),
            2 => d.is_system = None,
            3 => d.partition_style = Some(1),
            4 => p.is_shadow_copy = Some(true),
            5 => p.is_system = Some(true),
            6 => p.is_hidden = None,
            7 => p.is_read_only = None,
            8 => p.gpt_type = Some(crate::partitions::policy::LINUX_DATA.into()),
            _ => p.is_offline = Some(true),
        };
        assert!(eligible(&d, &p).is_err());
    }
}
#[test]
fn ipc_rejects_paths_masks_guid_and_arbitrary_methods() {
    let value = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":2},"action":"hide"});
    assert!(serde_json::from_value::<Request>(value.clone()).is_ok());
    for key in ["path", "attributes", "guid", "method", "force"] {
        let mut v = value.clone();
        v[key] = serde_json::json!(123);
        assert!(serde_json::from_value::<Request>(v).is_err());
    }
    let mut v = value;
    v["action"] = serde_json::json!("SetAttributes");
    assert!(serde_json::from_value::<Request>(v).is_err());
}

#[test]
fn provider_receives_only_one_explicit_attribute() {
    for (action, key, value) in [
        (Action::Hide, "IsHidden", true),
        (Action::Show, "IsHidden", false),
        (Action::ReadOnly, "IsReadOnly", true),
        (Action::ReadWrite, "IsReadOnly", false),
        (Action::NoDefaultLetter, "NoDefaultDriveLetter", true),
        (Action::AutomaticLetter, "NoDefaultDriveLetter", false),
    ] {
        let object = serde_json::to_value(params(action)).unwrap();
        assert_eq!(object.as_object().unwrap().len(), 1);
        assert_eq!(object[key], serde_json::json!(value));
    }
}

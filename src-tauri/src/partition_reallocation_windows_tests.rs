use super::*;

const VOLUME_A: &str = r"\\?\Volume{11110000-0000-4000-8000-000000000001}\";
const VOLUME_B: &str = r"\\?\Volume{11110000-0000-4000-8000-000000000002}\";
const PARTITION_GUID: &str = "10000000-0000-4000-8000-000000000001";

fn volume() -> Volume {
    Volume {
        path: "volume-a".into(),
        object_id: "a".into(),
        volume_path: Some(VOLUME_A.into()),
        file_system: Some("NTFS".into()),
        file_system_label: Some("source".into()),
        drive_letter: Some("G".into()),
        size: Some(256 * policy::MIB),
    }
}

fn snapshot() -> Snapshot {
    let target = "a".repeat(64);
    let part = Partition {
        path: "partition-a".into(),
        disk_number: 2,
        partition_number: 3,
        offset: 64 * policy::MIB,
        size: 256 * policy::MIB,
        guid: Some(PARTITION_GUID.into()),
        gpt_type: Some(policy::BASIC_DATA.into()),
        drive_letter: Some("G".into()),
        ..Default::default()
    };
    Snapshot {
        disk: Disk {
            path: "disk".into(),
            number: 2,
            size: 2048 * policy::MIB,
            number_of_partitions: Some(1),
            ..Default::default()
        },
        parts: vec![part.clone()],
        volumes: vec![Some(volume())],
        archive: table_archive::Archive {
            version: 1,
            size: 2048 * policy::MIB,
            sector: 512,
            source: target,
            includes_boot: false,
            extents: vec![table_archive::Extent {
                kind: table_archive::Kind::Mbr,
                offset: 0,
                bytes: vec![0; 512],
            }],
        },
        layout: table_restore::Layout {
            guid: "10000000-0000-4000-8000-000000000002".into(),
            count: 128,
            first: 34,
            last: 4194270,
            entries: vec![table_restore::Entry {
                slot: 3,
                guid: PARTITION_GUID.into(),
                kind: policy::BASIC_DATA.into(),
                offset: part.offset,
                size: part.size,
                attributes: 0,
                name: vec![],
            }],
        },
        native_protected: vec![],
    }
}

fn pending(now: Instant) -> PendingBatch {
    let snapshot = snapshot();
    PendingBatch {
        request: model::Request {
            target: Target(snapshot.archive.source.clone()),
            revision: "revision".into(),
            operation: model::Operation::Split {
                partition: 3,
                new_size: 64 * policy::MIB,
                format: None,
            },
        },
        preview: model::Preview {
            confirmation: Plan {
                token: "expected".into(),
                title: String::new(),
                target_name: String::new(),
                serial: String::new(),
                partition: None,
                before_size: None,
                after_size: None,
                warning: String::new(),
                expires_at: 0,
            },
            steps: vec![],
            journals: vec![],
            merge_folder: None,
        },
        snapshot,
        journals: VecDeque::new(),
        deadline: now + CONFIRM_FOR,
    }
}

#[test]
fn token_is_consumed_by_success_mismatch_and_expiry() {
    let now = Instant::now();
    for (token, time, expected) in [
        ("expected", now, true),
        ("different", now, false),
        ("expected", now + CONFIRM_FOR, false),
    ] {
        let mut state = Some(pending(now));
        assert_eq!(consume(&mut state, token, time).is_ok(), expected);
        assert!(consume(&mut state, "expected", now).is_err());
    }
}

#[test]
fn stop_accepts_only_the_current_batches_token() {
    let cancel = Arc::new(AtomicBool::new(false));
    let batch = RunningBatch {
        token: "current".into(),
        cancel: Arc::clone(&cancel),
    };
    assert!(!batch.request_stop("previous"));
    assert!(!cancel.load(Ordering::Relaxed));
    assert!(batch.request_stop("current"));
    assert!(cancel.load(Ordering::Relaxed));
}

#[test]
fn saved_snapshot_rejects_a_substituted_volume_or_changed_raw_table() {
    let prior = snapshot();
    let mut after = snapshot();
    assert!(after.exact_layout_and_volumes(&prior).is_ok());
    after.volumes[0].as_mut().unwrap().volume_path = Some(VOLUME_B.into());
    assert!(after.exact_layout_and_volumes(&prior).is_err());
    let mut after = snapshot();
    after.archive.extents[0].bytes[0] = 1;
    assert!(after.exact_layout_and_volumes(&prior).is_err());
}

#[test]
fn post_resize_allows_reenumeration_but_requires_same_volume_and_protection() {
    let prior = snapshot();
    let original = &prior.parts[0];
    let new_size = 192 * policy::MIB;
    let step = Step::Resize {
        partition: PartitionRef {
            guid: PARTITION_GUID.into(),
            number: original.partition_number,
            offset: original.offset,
            size: original.size,
        },
        new_size,
    };
    let mut after = snapshot();
    after.parts[0].size = new_size;
    after.parts[0].partition_number = 5;
    after.parts[0].path = "partition-remounted".into();
    after.layout.entries[0].slot = 5;
    after.layout.entries[0].size = new_size;
    let current = after.volumes[0].as_mut().unwrap();
    current.object_id = "reidentified-volume".into();
    current.path = "volume-remounted".into();
    current.size = Some(new_size);
    assert!(prior.verify_transition(&step, &after).is_ok());

    after.parts[0].is_read_only = Some(true);
    assert!(prior.verify_transition(&step, &after).is_err());
    after.parts[0].is_read_only = original.is_read_only;
    after.volumes[0].as_mut().unwrap().volume_path = Some(VOLUME_B.into());
    assert!(prior.verify_transition(&step, &after).is_err());
    after.volumes[0].as_mut().unwrap().volume_path = Some(VOLUME_A.into());
    after.archive.extents[0].bytes[0] = 1;
    assert!(prior.verify_transition(&step, &after).is_err());
}

#[test]
fn post_merge_checks_only_the_confirmed_partition_was_removed() {
    let mut before = snapshot();
    let absorbed_guid = "10000000-0000-4000-8000-000000000003";
    let mut removed = before.parts[0].clone();
    removed.partition_number += 1;
    removed.path = "partition-to-absorb".into();
    removed.offset += removed.size;
    removed.guid = Some(absorbed_guid.into());
    let bind = |part: &Partition| PartitionRef {
        guid: part.guid.clone().unwrap(),
        number: part.partition_number,
        offset: part.offset,
        size: part.size,
    };
    let step = Step::CopyAndRemove {
        keep: bind(&before.parts[0]),
        absorb: bind(&removed),
    };
    let mut entry = before.layout.entries[0].clone();
    entry.guid = absorbed_guid.into();
    entry.slot += 1;
    entry.offset = removed.offset;
    before.layout.entries.push(entry);
    before.parts.push(removed);
    let mut absorbed_volume = volume();
    absorbed_volume.volume_path = Some(VOLUME_B.into());
    before.volumes.push(Some(absorbed_volume));
    before.disk.number_of_partitions = Some(2);

    let mut after = snapshot();
    assert!(before.verify_transition(&step, &after).is_ok());
    after.volumes[0].as_mut().unwrap().volume_path = Some(VOLUME_B.into());
    assert!(before.verify_transition(&step, &after).is_err());
    assert!(before.verify_transition(&step, &before).is_err());
}

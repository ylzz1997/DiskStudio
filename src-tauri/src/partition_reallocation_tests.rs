use super::*;

fn partition(number: u32, offset: u64, size: u64) -> Partition {
    Partition {
        path: format!("provider-{number}"),
        disk_number: 1,
        partition_number: number,
        offset: offset * MIB,
        size: size * MIB,
        guid: Some(format!("00000000-0000-4000-8000-{number:012x}")),
        gpt_type: Some(policy::BASIC_DATA.into()),
        is_boot: Some(false),
        is_system: Some(false),
        is_read_only: Some(false),
        is_offline: Some(false),
        is_hidden: Some(false),
        is_shadow_copy: Some(false),
        ..Default::default()
    }
}

fn disk(parts: &[Partition], sector: u64) -> Disk {
    Disk {
        path: "provider-disk".into(),
        number: 1,
        size: 2048 * MIB,
        logical_sector_size: Some(sector),
        partition_style: Some(2),
        number_of_partitions: Some(parts.len() as u32),
        is_boot: Some(false),
        is_system: Some(false),
        is_read_only: Some(false),
        is_offline: Some(false),
        is_clustered: Some(false),
        ..Default::default()
    }
}

fn plan(disk: &Disk, parts: &[Partition], operation: &Operation) -> Result<Vec<Step>, String> {
    let mut layout = gpt(parts);
    layout.last = disk.size / disk.logical_sector_size.unwrap() - 34;
    super::plan(disk, parts, &layout, operation)
}

#[test]
fn split_creates_only_within_the_released_extent_and_formats_the_new_volume() {
    let parts = [partition(7, 320, 256), partition(8, 704, 128)];
    for sector in [512, 4096] {
        let format = quick::Format {
            file_system: super::super::FileSystem::Ntfs,
            label: " NEW ".into(),
            cluster_size: 4096,
            options: Default::default(),
        };
        let op = Operation::Split {
            partition: 7,
            new_size: 64 * MIB,
            format: Some(format),
        };
        let result = plan(&disk(&parts, sector), &parts, &op).unwrap();
        let [Step::Resize {
            partition,
            new_size,
        }, Step::Create { entry }, Step::Format { entry: formatted }] = result.as_slice()
        else {
            panic!("unexpected plan {result:?}");
        };
        assert_eq!(
            (*new_size, partition.offset, partition.size),
            (192 * MIB, 320 * MIB, 256 * MIB)
        );
        assert_eq!((entry.offset, entry.size), (512 * MIB, 64 * MIB));
        assert_eq!(entry.offset + entry.size, partition.offset + partition.size);
        assert_eq!(formatted.offset, entry.offset);
        assert_eq!(formatted.format.as_ref().unwrap().label, "NEW");
    }
}

#[test]
fn right_hand_gap_moves_nearest_partition_first_and_expands_the_receiver() {
    let parts = [
        partition(9, 64, 128),
        partition(3, 192, 128),
        partition(7, 320, 128),
        partition(6, 544, 128),
    ];
    let operation = Operation::Allocate {
        partition: 9,
        free_offset: 448 * MIB,
        amount: 64 * MIB,
    };
    let steps = plan(&disk(&parts, 4096), &parts, &operation).unwrap();
    let [Step::Move {
        partition: second,
        new_offset: second_offset,
    }, Step::Move {
        partition: first,
        new_offset: first_offset,
    }, Step::Resize {
        partition: target,
        new_size,
    }] = steps.as_slice()
    else {
        panic!("unexpected plan {steps:?}");
    };
    assert_eq!((second.number, *second_offset), (7, 384 * MIB));
    assert_eq!((first.number, *first_offset), (3, 256 * MIB));
    assert_eq!((target.number, *new_size), (9, 192 * MIB));
}

#[test]
fn left_hand_gap_shifts_the_receiver_and_keeps_its_old_end() {
    let parts = [
        partition(9, 64, 128),
        partition(7, 256, 128),
        partition(3, 384, 128),
    ];
    let operation = Operation::Allocate {
        partition: 3,
        free_offset: 192 * MIB,
        amount: 32 * MIB,
    };
    let steps = plan(&disk(&parts, 512), &parts, &operation).unwrap();
    let [Step::Move {
        partition: first,
        new_offset,
    }, Step::Move {
        partition: second,
        new_offset: receiver_offset,
    }, Step::Resize {
        partition: receiver,
        new_size,
    }] = steps.as_slice()
    else {
        panic!("unexpected plan {steps:?}");
    };
    assert_eq!((first.number, *new_offset), (7, 224 * MIB));
    assert_eq!((second.number, *receiver_offset), (3, 352 * MIB));
    assert_eq!(second.guid, receiver.guid);
    assert_eq!(receiver.offset, *receiver_offset);
    assert_eq!(receiver.offset + new_size, second.offset + second.size);
}

#[test]
fn an_adjacent_right_gap_needs_no_moves() {
    let parts = [partition(2, 64, 128), partition(8, 224, 128)];
    let operation = Operation::Allocate {
        partition: 2,
        free_offset: 192 * MIB,
        amount: 32 * MIB,
    };
    let steps = plan(&disk(&parts, 512), &parts, &operation).unwrap();
    assert!(
        matches!(steps.as_slice(), [Step::Resize { partition, new_size }] if partition.number == 2 && *new_size == 160 * MIB)
    );
}

#[test]
fn transfer_from_the_right_uses_the_donors_post_shrink_extent() {
    let parts = [partition(3, 64, 128), partition(6, 192, 256)];
    let operation = Operation::Transfer {
        donor: 6,
        receiver: 3,
        amount: 64 * MIB,
    };
    let steps = plan(&disk(&parts, 512), &parts, &operation).unwrap();
    let [Step::Resize {
        partition: donor,
        new_size: shrunk,
    }, Step::Move {
        partition: moved,
        new_offset,
    }, Step::Resize {
        partition: receiver,
        new_size: grown,
    }] = steps.as_slice()
    else {
        panic!("unexpected plan {steps:?}");
    };
    assert_eq!(
        (donor.size, *shrunk, moved.size),
        (256 * MIB, 192 * MIB, 192 * MIB)
    );
    assert_eq!(donor.guid, moved.guid);
    assert_eq!(receiver.offset + grown, *new_offset);
    assert_eq!(new_offset + moved.size, donor.offset + donor.size);
}

#[test]
fn transfer_from_the_left_increases_the_receiver_without_moving_the_donor() {
    let parts = [partition(3, 64, 128), partition(6, 192, 256)];
    let operation = Operation::Transfer {
        donor: 3,
        receiver: 6,
        amount: 32 * MIB,
    };
    let steps = plan(&disk(&parts, 4096), &parts, &operation).unwrap();
    let [Step::Resize {
        partition: donor,
        new_size: shrunk,
    }, Step::Move {
        partition: receiver,
        new_offset,
    }, Step::Resize {
        partition: grown,
        new_size,
    }] = steps.as_slice()
    else {
        panic!("unexpected plan {steps:?}");
    };
    assert_eq!(donor.offset + shrunk, *new_offset);
    assert_eq!(receiver.guid, grown.guid);
    assert_eq!(grown.offset + new_size, receiver.offset + receiver.size);
}

#[test]
fn merge_copy_and_remove_precedes_move_and_growth_and_keeps_the_chosen_identity() {
    let parts = [partition(1, 64, 128), partition(9, 208, 128)];
    let d = disk(&parts, 4096);
    let left = plan(&d, &parts, &Operation::Merge { keep: 1, absorb: 9 }).unwrap();
    assert!(
        matches!(left.as_slice(), [Step::CopyAndRemove { keep, absorb }, Step::Resize { partition, new_size }] if keep.number == 1 && absorb.number == 9 && keep.guid == partition.guid && *new_size == 272 * MIB)
    );
    let right = plan(&d, &parts, &Operation::Merge { keep: 9, absorb: 1 }).unwrap();
    assert!(
        matches!(right.as_slice(), [Step::CopyAndRemove { keep, absorb }, Step::Move { partition, new_offset }, Step::Resize {partition: resized, new_size}] if absorb.number == 1 && keep.guid == partition.guid && keep.guid == resized.guid && *new_offset == 64 * MIB && *new_size == 272 * MIB)
    );
}

#[test]
fn stale_gaps_protected_intermediates_invalid_sizes_and_bad_geometry_fail_closed() {
    let mut parts = [
        partition(1, 64, 128),
        partition(7, 192, 128),
        partition(4, 352, 128),
    ];
    let d = disk(&parts, 4096);
    for operation in [
        Operation::Split {
            partition: 1,
            new_size: 65 * MIB,
            format: None,
        },
        Operation::Split {
            partition: 1,
            new_size: u64::MAX,
            format: None,
        },
        Operation::Transfer {
            donor: 1,
            receiver: 1,
            amount: MIB,
        },
        Operation::Allocate {
            partition: 1,
            free_offset: 321 * MIB,
            amount: MIB,
        },
        Operation::Allocate {
            partition: 1,
            free_offset: 320 * MIB,
            amount: 33 * MIB,
        },
        Operation::Merge { keep: 1, absorb: 4 },
        Operation::Merge { keep: 1, absorb: 1 },
    ] {
        assert!(plan(&d, &parts, &operation).is_err(), "{operation:?}");
    }
    let operation = Operation::Allocate {
        partition: 1,
        free_offset: 320 * MIB,
        amount: 16 * MIB,
    };
    parts[1].is_hidden = None;
    assert!(plan(&d, &parts, &operation).is_err());
    parts[1].is_hidden = Some(false);
    parts[2].offset += 512;
    assert!(plan(&d, &parts, &operation).is_err());
    assert!(plan(&d, &parts[..2], &operation).is_err());
}

#[test]
fn requests_only_deserialize_typed_partition_numbers_and_geometry() {
    let value =
        serde_json::json!({"type":"allocate","partition":1,"freeOffset":2048,"amount":65536});
    assert!(serde_json::from_value::<Operation>(value.clone()).is_ok());
    for field in ["path", "method", "guid"] {
        let mut injected = value.clone();
        injected[field] = "untrusted".into();
        assert!(serde_json::from_value::<Operation>(injected).is_err());
    }
}

fn gpt(parts: &[Partition]) -> Layout {
    Layout {
        guid: "10000000-0000-4000-8000-000000000000".into(),
        first: 34,
        last: 4194270,
        count: 128,
        entries: parts
            .iter()
            .map(|p| GptEntry {
                slot: p.partition_number,
                guid: p.guid.clone().unwrap(),
                kind: policy::BASIC_DATA.into(),
                offset: p.offset,
                size: p.size,
                attributes: 0,
                name: vec![0x0044, 0x0041, 0x0054, 0x0041],
            })
            .collect(),
    }
}

#[test]
fn current_partition_rebinds_by_guid_and_geometry_across_renumbering() {
    let before = partition(4, 64, 128);
    let selected = PartitionRef::from_partition(&before).unwrap();
    let different = partition(4, 192, 128);
    let mut current = before.clone();
    current.partition_number = 5;
    assert_eq!(selected.resolve(&[&current]).unwrap().partition_number, 5);
    assert!(selected.resolve(&[&different, &current]).is_err());
    current.size -= MIB;
    assert!(selected.resolve(&[&current]).is_err());
    current.size += MIB;
    current.is_offline = Some(true);
    assert!(selected.resolve(&[&current]).is_err());
}

#[test]
fn growth_rejects_a_change_to_any_unselected_entry_or_gpt_field() {
    let parts = [partition(3, 64, 128), partition(6, 256, 128)];
    let op = Operation::Allocate {
        partition: 3,
        free_offset: 192 * MIB,
        amount: 32 * MIB,
    };
    let steps = plan(&disk(&parts, 512), &parts, &op).unwrap();
    let before = gpt(&parts);
    let mut after = before.clone();
    after.entries[0].size += 32 * MIB;
    after.entries[1].slot = 7;
    assert!(steps[0].verify_geometry(&before, &after).is_ok());
    assert!(steps[0].verify_geometry(&before, &before).is_err());

    let mut changed = after.clone();
    changed.last -= 1;
    assert!(steps[0].verify_geometry(&before, &changed).is_err());
    changed = after.clone();
    changed.entries[0].attributes |= 1 << 63;
    assert!(steps[0].verify_geometry(&before, &changed).is_err());
    changed = after;
    changed.entries[1].name[0] = 0x003F;
    assert!(steps[0].verify_geometry(&before, &changed).is_err());
}

#[test]
fn split_create_and_merge_remove_check_exact_entry_count_and_identity() {
    let parts = [partition(1, 64, 256), partition(7, 320, 128)];
    let d = disk(&parts, 4096);
    let merge = plan(&d, &parts, &Operation::Merge { keep: 1, absorb: 7 }).unwrap();
    let before = gpt(&parts);
    let mut absorbed = before.clone();
    absorbed.entries.remove(1);
    assert!(merge[0].verify_geometry(&before, &absorbed).is_ok());
    absorbed.entries[0].guid = before.entries[1].guid.clone();
    assert!(merge[0].verify_geometry(&before, &absorbed).is_err());

    let split = plan(
        &d,
        &parts,
        &Operation::Split {
            partition: 1,
            new_size: 64 * MIB,
            format: None,
        },
    )
    .unwrap();
    let mut shrunk = before.clone();
    shrunk.entries[0].size = 192 * MIB;
    assert!(split[0].verify_geometry(&before, &shrunk).is_ok());
    let mut created = shrunk.clone();
    let new = partition(8, 256, 64);
    created.entries.push(gpt(&[new]).entries.remove(0));
    assert!(split[1].verify_geometry(&shrunk, &created).is_ok());
    created.entries[2].attributes = 1;
    assert!(split[1].verify_geometry(&shrunk, &created).is_err());
}

#[test]
fn planner_uses_actual_gpt_bounds_capacity_and_protection_flags() {
    let parts = [partition(1, 64, 128), partition(3, 192, 128)];
    let d = disk(&parts, 4096);
    let mut raw = gpt(&parts);
    raw.last = 352 * MIB / 4096 - 1;
    let allocate = Operation::Allocate {
        partition: 1,
        free_offset: 320 * MIB,
        amount: 64 * MIB,
    };
    assert!(super::plan(&d, &parts, &raw, &allocate).is_err());

    let allocate = Operation::Allocate {
        partition: 1,
        free_offset: 320 * MIB,
        amount: 32 * MIB,
    };
    assert!(super::plan(&d, &parts, &raw, &allocate).is_ok());
    raw.entries[1].attributes = 1;
    assert!(super::plan(&d, &parts, &raw, &allocate).is_err());

    let split = Operation::Split {
        partition: 1,
        new_size: 32 * MIB,
        format: None,
    };
    raw.count = 2;
    assert!(super::plan(&d, &parts, &raw, &split).is_err());
    raw.count += 1;
    assert!(super::plan(&d, &parts, &raw, &split).is_ok());
    raw.entries[1].size += MIB;
    assert!(super::plan(&d, &parts, &raw, &split).is_err());
}

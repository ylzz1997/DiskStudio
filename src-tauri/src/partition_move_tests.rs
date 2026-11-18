use super::*;
use crate::table_archive;
use disk_core::{partition_move::Journal, MIB};
use std::io::Cursor;

type Image = Cursor<Vec<u8>>;

fn persist(_: &mut Image) -> io::Result<()> {
    Ok(())
}

fn fixture(sector: u64) -> (Image, Archive, String) {
    let mut image = Cursor::new(table_archive::tests::gpt(sector as usize));
    let size = image.get_ref().len() as u64;
    let archive =
        table_archive::capture(&mut image, size, sector, "c".repeat(64), false, || Ok(())).unwrap();
    let guid = archive_layout(&archive).unwrap().entries[0].guid.clone();
    (image, archive, guid)
}

#[test]
fn same_disk_move_and_gpt_commit_preserve_other_disk_bytes() {
    for sector in [512, 4096] {
        let (mut image, archive, guid) = fixture(sector);
        let plan = LayoutPlan::prepare(archive, &guid, MIB + 256 * 1024).unwrap();
        let mut expected = image.get_ref().clone();
        expected.copy_within(
            MIB as usize..2 * MIB as usize,
            plan.range.destination() as usize,
        );
        for extent in &plan.after.extents {
            let at = extent.offset as usize;
            expected[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
        }
        let source_hash = plan.range.source_sha256(&mut image, |_| Ok(())).unwrap();
        let mut journal = Journal::create(
            Cursor::new(vec![]),
            persist,
            plan.range,
            source_hash,
            plan.before.encode().unwrap(),
        )
        .unwrap();
        journal.copy(&mut image, persist, |_, _| Ok(())).unwrap();
        assert_eq!(plan.committed_ranges(&mut image).unwrap(), 0);
        journal
            .verify_destination(&mut image, |_, _| Ok(()), |_, _| Ok(()))
            .unwrap();
        plan.commit(&mut image, persist, || Ok(())).unwrap();
        journal.mark_completed().unwrap();
        assert_eq!(image.get_ref(), &expected);

        let actual = table_archive::capture(
            &mut image,
            plan.range.capacity(),
            sector,
            "c".repeat(64),
            false,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(archive_layout(&actual).unwrap(), plan.new_layout);
        let mut old_entry = plan.old_layout.entries[0].clone();
        old_entry.offset = plan.range.destination();
        assert_eq!(plan.new_layout.entries, vec![old_entry]);
    }
}

#[test]
fn interrupted_gpt_commit_accepts_only_exact_completed_prefixes() {
    for sector in [512, 4096] {
        let (image, archive, guid) = fixture(sector);
        let plan = LayoutPlan::prepare(archive, &guid, 3 * MIB).unwrap();
        let mut complete = image.clone();
        let mut writes = 0;
        plan.commit(
            &mut complete,
            |_| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(writes, COMMIT_ORDER.len());
        for stop_after in 0..COMMIT_ORDER.len() {
            let mut disk = image.clone();
            let mut flushes = 0;
            assert!(plan
                .commit(
                    &mut disk,
                    |_| {
                        let next = flushes;
                        flushes += 1;
                        if next == stop_after {
                            Err(io::Error::other("unexpected interruption"))
                        } else {
                            Ok(())
                        }
                    },
                    || Ok(())
                )
                .is_err());
            assert_eq!(plan.committed_ranges(&mut disk).unwrap(), stop_after + 1);
            let mut resumed = 0;
            plan.commit(
                &mut disk,
                |_| {
                    resumed += 1;
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
            assert_eq!(resumed, COMMIT_ORDER.len() - stop_after - 1);
            assert_eq!(disk.into_inner(), *complete.get_ref());
        }
    }
}

#[test]
fn rejects_changes_to_the_pmbr_torn_tables_and_out_of_order_writes() {
    let (image, archive, guid) = fixture(4096);
    let plan = LayoutPlan::prepare(archive, &guid, 3 * MIB).unwrap();
    for offset in [0, 4096 + 17] {
        let mut disk = image.clone();
        disk.get_mut()[offset] ^= 1;
        assert!(plan
            .commit(
                &mut disk,
                |_| panic!("unknown GPT changes cannot be overwritten"),
                || Ok(())
            )
            .is_err());
    }
    let mut changed = image;
    let next = plan.extent(&plan.after, Kind::PrimaryEntries).unwrap();
    let offset = next.offset as usize;
    changed.get_mut()[offset..offset + next.bytes.len()].copy_from_slice(&next.bytes);
    assert!(plan
        .commit(
            &mut changed,
            |_| panic!("out of order GPT cannot be overwritten"),
            || Ok(())
        )
        .is_err());
}

#[test]
fn checks_usable_boundaries_other_partitions_and_gpt_protection() {
    let (_, archive, guid) = fixture(512);
    let current = archive_layout(&archive).unwrap();
    assert!(LayoutPlan::prepare(archive.clone(), &guid, 0).is_err());
    assert!(LayoutPlan::prepare(archive.clone(), &guid, archive.size - MIB).is_err());
    let protected = edit_gpt_entry(&archive, &current, 1, |p| p[48] = 1).unwrap();
    assert!(LayoutPlan::prepare(protected, &guid, 3 * MIB).is_err());

    let another = edit_gpt_entry(&archive, &current, 2, |p| {
        let kind = uuid::Uuid::parse_str(BASIC_DATA).unwrap();
        p[..16].copy_from_slice(&kind.to_bytes_le());
        p[16..32].fill(0x69);
        p[32..40].copy_from_slice(&(3 * MIB / 512).to_le_bytes());
        p[40..48].copy_from_slice(&(4 * MIB / 512 - 1).to_le_bytes());
    })
    .unwrap();
    assert!(LayoutPlan::prepare(another.clone(), &guid, 3 * MIB).is_err());
    assert!(LayoutPlan::prepare(another, &guid, 4 * MIB).is_ok());
}

#[test]
fn recorded_plan_continues_from_boot_and_gpt_writes_on_both_sector_profiles() {
    use disk_core::fat_format::{FatType, Plan as Format};
    for sector in [512, 4096] {
        let (mut disk, archive, guid) = fixture(sector);
        let mut volume = Cursor::new(vec![0xbc; MIB as usize]);
        Format::new(
            FatType::Fat12,
            MIB,
            sector as u32,
            sector as u32,
            "MOVE",
            41,
            (MIB / sector) as u32,
        )
        .unwrap()
        .format(&mut volume, |_, _| Ok(()))
        .unwrap();
        let source = MIB as usize;
        disk.get_mut()[source..source + MIB as usize].copy_from_slice(volume.get_ref());
        let destination = MIB + 256 * 1024;
        let mut plan = Plan::prepare(
            archive,
            &guid,
            destination,
            &mut volume,
            "FAT",
            &AtomicBool::new(false),
        )
        .unwrap();
        let range = plan.layout.range;
        let source_hash = range.source_sha256(&mut disk, |_| Ok(())).unwrap();
        let mut stale = disk.clone();
        stale.get_mut()[source + 28] ^= 1;
        let stale_hash = range.source_sha256(&mut stale, |_| Ok(())).unwrap();
        let mut stale_journal = Journal::create(
            Cursor::new(vec![]),
            persist,
            range,
            stale_hash,
            plan.metadata().to_vec(),
        )
        .unwrap();
        assert!(plan
            .transfer(
                &mut stale,
                &mut stale_journal,
                |_| panic!("stale boot cannot be moved"),
                |_, _| Ok(()),
                || Ok(())
            )
            .is_err());
        let verify = |bytes: &[u8]| bytes.try_into().map_err(|_| "invalid test seal".to_owned());
        assert!(Plan::resume(&stale_journal, verify).is_err());
        plan.seal(source_hash, |digest| Ok(digest.to_vec()))
            .unwrap();
        let forged = Journal::create(
            Cursor::new(vec![]),
            persist,
            range,
            stale_hash,
            plan.metadata().to_vec(),
        )
        .unwrap();
        assert!(Plan::resume(&forged, verify).is_err());
        let mut journal_file = Cursor::new(vec![]);
        {
            let mut journal = Journal::create(
                &mut journal_file,
                |_| Ok(()),
                range,
                source_hash,
                plan.metadata().to_vec(),
            )
            .unwrap();
            let mut writes = 0;
            assert!(plan
                .transfer(
                    &mut disk,
                    &mut journal,
                    |_| {
                        writes += 1;
                        if writes == 6 {
                            Err(io::Error::other("stop after backup GPT array"))
                        } else {
                            Ok(())
                        }
                    },
                    |_, _| Ok(()),
                    || Ok(())
                )
                .is_err());
            assert!(journal.copy_finished());
            assert_eq!(plan.layout.committed_ranges(&mut disk).unwrap(), 1);
        }
        let mut journal = Journal::open(&mut journal_file, |_| Ok(())).unwrap();
        let plan = Plan::resume(&journal, verify).unwrap();
        plan.transfer(&mut disk, &mut journal, persist, |_, _| Ok(()), || Ok(()))
            .unwrap();
        let at = destination as usize;
        let mut expected = volume.into_inner();
        expected[28..32].copy_from_slice(&((destination / sector) as u32).to_le_bytes());
        assert_eq!(&disk.get_ref()[at..at + MIB as usize], expected);
        journal.mark_completed().unwrap();
        plan.transfer(
            &mut disk,
            &mut journal,
            |_| panic!("completed plan cannot write"),
            |_, _| Ok(()),
            || Ok(()),
        )
        .unwrap();
    }
}

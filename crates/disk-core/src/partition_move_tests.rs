use super::*;
use std::io::Cursor;

type Memory = Cursor<Vec<u8>>;

fn durable(_: &mut Memory) -> io::Result<()> {
    Ok(())
}

fn fixture(sector: u64, dest: u64) -> (Range, Memory, Journal<Memory>) {
    let range = Range::new(12 * sector, dest * sector, 25 * sector, 96 * sector, sector).unwrap();
    let mut disk = Cursor::new(
        (0..range.capacity as usize)
            .map(|i| (i * 127 ^ i / 257 ^ i / 41) as u8)
            .collect::<Vec<_>>(),
    );
    let digest = range.source_sha256(&mut disk, |_| Ok(())).unwrap();
    let journal = Journal::create(
        Cursor::new(vec![]),
        durable,
        range,
        digest,
        b"validated move plan".to_vec(),
    )
    .unwrap();
    (range, disk, journal)
}

fn assert_result(range: Range, disk: &mut Memory, original: &[u8], journal: &Journal<Memory>) {
    let mut expected = original.to_vec();
    expected.copy_within(
        range.source as usize..(range.source + range.length) as usize,
        range.destination as usize,
    );
    assert_eq!(disk.get_ref(), &expected);
    journal
        .verify_destination(disk, |_, _| Ok(()), |_, _| Ok(()))
        .unwrap();
}

#[test]
fn moves_both_overlap_directions_and_nonoverlap_without_writing_other_bytes() {
    for sector in [512, 4096] {
        for destination in [9, 15, 49] {
            let (range, mut disk, mut journal) = fixture(sector, destination);
            let original = disk.get_ref().clone();
            journal.copy(&mut disk, durable, |_, _| Ok(())).unwrap();
            assert_result(range, &mut disk, &original, &journal);
            assert!(journal.copy_finished());
            assert!(!journal.completed());
            journal.mark_completed().unwrap();
            let again = Journal::open(journal.io, durable).unwrap();
            assert!(again.completed());
            assert_eq!(again.range(), range);
            assert_eq!(again.metadata(), b"validated move plan");
        }
    }
}

#[test]
fn an_uncheckpointed_write_can_be_recovered_in_either_overlap_direction() {
    for destination in [9, 15] {
        let (range, mut disk, mut journal) = fixture(4096, destination);
        let original = disk.get_ref().clone();
        let mut flushes = 0;
        assert!(journal
            .copy(
                &mut disk,
                |_| {
                    flushes += 1;
                    if flushes == 2 {
                        Err(io::Error::other("interrupted disk flush"))
                    } else {
                        Ok(())
                    }
                },
                |_, _| Ok(())
            )
            .is_err());
        assert_eq!(flushes, 2);
        let mut journal = Journal::open(journal.io, durable).unwrap();
        assert_eq!(journal.copied_bytes(), 3 * 4096);
        journal.copy(&mut disk, durable, |_, _| Ok(())).unwrap();
        assert_result(range, &mut disk, &original, &journal);
    }
}

#[test]
fn a_torn_newer_slot_uses_the_preceding_record_and_revalidates_all_data() {
    let (range, mut disk, mut journal) = fixture(512, 15);
    let original = disk.get_ref().clone();
    assert!(journal
        .copy(&mut disk, durable, |stage, done| {
            if stage == Stage::Copy && done == 2 * range.chunk_bytes() {
                Err("stop before third chunk".into())
            } else {
                Ok(())
            }
        })
        .is_err());
    let slot = slot_start(journal.metadata().len());
    journal.io.get_mut()[slot + 9] ^= 0x71;
    let mut journal = Journal::open(journal.io, durable).unwrap();
    assert_eq!(journal.copied_bytes(), range.chunk_bytes());
    journal.copy(&mut disk, durable, |_, _| Ok(())).unwrap();
    assert_result(range, &mut disk, &original, &journal);
}

#[test]
fn modified_copied_or_remaining_bytes_stop_resume_before_any_write() {
    for destination in [9, 15] {
        let (range, mut disk, mut journal) = fixture(512, destination);
        assert!(journal
            .copy(&mut disk, durable, |stage, done| {
                if stage == Stage::Copy && done == range.chunk_bytes() {
                    Err("stop before second chunk".into())
                } else {
                    Ok(())
                }
            })
            .is_err());
        let committed = if range.backwards() {
            range.destination + range.length - 1
        } else {
            range.destination
        };
        let remaining = if range.backwards() {
            range.source
        } else {
            range.source + range.length - 1
        };
        let saved = disk.get_ref().clone();
        for changed in [committed, remaining] {
            *disk.get_mut() = saved.clone();
            disk.get_mut()[changed as usize] ^= 1;
            let before = disk.get_ref().clone();
            assert!(journal
                .copy(
                    &mut disk,
                    |_| panic!("changed content cannot be written"),
                    |_, _| Ok(())
                )
                .is_err());
            assert_eq!(disk.get_ref(), &before);
        }
    }
}

#[test]
fn rejects_inconsistent_headers_metadata_and_out_of_bounds_geometry() {
    assert!(Range::new(1, 4096, 4096, 65536, 4096).is_err());
    assert!(Range::new(0, 4096, 4096, 4096, 4096).is_err());
    assert!(Range::new(0, 0, 4096, 65536, 4096).is_err());
    assert!(Range::new(0, 4096, 4096, 65536, 0).is_err());
    let (_, _, journal) = fixture(512, 15);
    let valid = journal.io.into_inner();
    for offset in [
        0,
        12,
        52,
        HEADER_BYTES,
        slot_start(b"validated move plan".len()),
    ] {
        let mut broken = valid.clone();
        broken[offset] ^= 1;
        assert!(Journal::open(Cursor::new(broken), durable).is_err());
    }
    assert!(Journal::open(Cursor::new(valid[..valid.len() - 1].to_vec()), durable).is_err());
}

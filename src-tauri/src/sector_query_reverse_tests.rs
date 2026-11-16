use super::*;
use crate::data_io::Region;
use std::{
    cell::RefCell,
    io::{self, Cursor},
    rc::Rc,
};

fn hex(value: &str, offset: u32) -> Condition {
    Condition {
        pattern: Pattern::Hex(value.into()),
        offset,
        ignore_ascii_case: false,
    }
}
fn query(conditions: Vec<Condition>, mode: Mode) -> CompiledQuery {
    Query { conditions, mode }
        .compile_for(Direction::Backward)
        .unwrap()
}
fn run(bytes: &[u8], q: &CompiledQuery) -> SearchResult {
    search_direction(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        q,
        Direction::Backward,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap()
}

#[test]
fn reverse_any_and_all_agree_with_independent_mask_oracle() {
    let mut data: Vec<u8> = (0..CHUNK * 2 + 19)
        .map(|i| ((i * 73 + i / 17) % 256) as u8)
        .collect();
    for at in [0, CHUNK - 9, CHUNK - 1, CHUNK + 11, CHUNK * 2 + 9] {
        data[at + 2..at + 4].copy_from_slice(&[0xa5, 0x55]);
        data[at + 7..at + 10].copy_from_slice(&[0x5f, 0x81, 0x3a]);
    }
    // A shorter Any alternative may match where the longest one does not fit.
    let tail = data.len() - 2;
    data[tail..].copy_from_slice(&[0xa5, 0x55]);
    for mode in [Mode::Any, Mode::All] {
        let expected: Vec<_> = (0..data.len())
            .rev()
            .filter(|&i| {
                let a = data.get(i + 2).is_some_and(|b| b & 0xf0 == 0xa0)
                    && data.get(i + 3) == Some(&0x55);
                let b = data.get(i + 7).is_some_and(|b| b & 0x0f == 15)
                    && data.get(i + 8) == Some(&0x81)
                    && data.get(i + 9) == Some(&0x3a);
                if mode == Mode::Any {
                    a || b
                } else {
                    a && b
                }
            })
            .take(MAX_HITS)
            .map(|i| i as u64)
            .collect();
        let r = run(
            &data,
            &query(vec![hex("A? 55", 2), hex("?F 81 3A", 7)], mode),
        );
        assert_eq!(r.hits, expected);
        assert_eq!(r.state, "completed");
        assert_eq!(r.checked_bytes, data.len() as u64);
    }
}

#[test]
fn reverse_preserves_cross_block_overlap_and_maximum_span() {
    let mut data = vec![0; CHUNK * 2 + 7];
    data[CHUNK - 2048..CHUNK + 2050].fill(1);
    let r = run(&data, &query(vec![hex(&"01 ".repeat(4096), 0)], Mode::All));
    assert_eq!(
        r.hits,
        vec![
            (CHUNK - 2046) as u64,
            (CHUNK - 2047) as u64,
            (CHUNK - 2048) as u64
        ]
    );
    for at in [CHUNK - 1, CHUNK, CHUNK * 2 + 6] {
        data[at] = 255;
    }
    let r = run(&data, &query(vec![hex("FF", 65535)], Mode::All));
    assert_eq!(r.hits, vec![(CHUNK + 7) as u64, 1, 0]);
}

#[test]
fn reverse_text_folding_preserves_utf16_code_units_and_binary_literals() {
    let text = Condition {
        pattern: Pattern::Utf16Le("Ła".into()),
        offset: 0,
        ignore_ascii_case: true,
    };
    let data = [0x41, 1, 0x61, 0, 0x61, 1, 0x61, 0, 0x41, 1, 0x41, 0];
    assert_eq!(run(&data, &query(vec![text], Mode::Any)).hits, vec![8, 0]);
    assert_eq!(
        run(
            &[0, 10, 255, 0, 10, 255],
            &query(vec![hex("00 0? FF", 0)], Mode::Any)
        )
        .hits,
        vec![3, 0]
    );
}

struct Traced<'a> {
    inner: Cursor<Vec<u8>>,
    reads: Rc<RefCell<Vec<(u64, usize)>>>,
    fail_below: Option<u64>,
    cancel_below: Option<(u64, &'a AtomicBool)>,
}
impl Read for Traced<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let at = self.inner.position();
        self.reads.borrow_mut().push((at, out.len()));
        if self.fail_below.is_some_and(|limit| at < limit) {
            return Err(io::Error::other("injected read failure"));
        }
        if let Some((limit, flag)) = self.cancel_below {
            if at < limit {
                flag.store(true, Ordering::Relaxed);
            }
        }
        self.inner.read(out)
    }
}
impl Seek for Traced<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn reverse_region_reads_each_aligned_block_once_without_adjacent_bytes() {
    for sector in [512usize, 4096] {
        let size = 2 * 1024 * 1024 + sector;
        let mut bytes = vec![0u8; size + 2 * sector];
        bytes[..sector].fill(42);
        bytes[sector + size..].fill(42);
        bytes[sector] = 42;
        bytes[sector + size - 1] = 42;
        let reads = Rc::new(RefCell::new(vec![]));
        let reader = Traced {
            inner: Cursor::new(bytes),
            reads: reads.clone(),
            fail_below: None,
            cancel_below: None,
        };
        let mut region = Region::new(reader, sector as u64, size as u64).unwrap();
        let r = search_direction(
            &mut region,
            size as u64,
            &query(vec![hex("2A", 0)], Mode::All),
            Direction::Backward,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(r.hits, vec![(size - 1) as u64, 0]);
        assert_eq!(
            *reads.borrow(),
            vec![
                ((sector + 2 * 1024 * 1024) as u64, sector),
                ((sector + 1024 * 1024) as u64, 1024 * 1024),
                (sector as u64, 1024 * 1024)
            ]
        );
        assert_eq!(r.checked_bytes, size as u64);
    }
}

#[test]
fn reverse_limit_keeps_highest_overlapping_hits_and_suffix_progress() {
    let data = vec![0; 2 * CHUNK];
    let r = run(&data, &query(vec![hex("?? ??", 0)], Mode::All));
    assert_eq!(r.state, "limited");
    assert_eq!(
        r.hits,
        ((data.len() - 1 - MAX_HITS)..data.len() - 1)
            .rev()
            .map(|i| i as u64)
            .collect::<Vec<_>>()
    );
    assert_eq!(r.checked_bytes, MAX_HITS as u64 + 1);
}

#[test]
fn reverse_cancel_and_failure_never_claim_unfinished_window() {
    let q = query(vec![hex("2A", 0)], Mode::Any);
    let mut data = vec![42; CHUNK + 7];
    // Keep seven committed tail hits, then cancel while reading the next block.
    let flag = AtomicBool::new(false);
    let reads = Rc::new(RefCell::new(vec![]));
    let mut reader = Traced {
        inner: Cursor::new(data.clone()),
        reads: reads.clone(),
        fail_below: None,
        cancel_below: Some((CHUNK as u64, &flag)),
    };
    let r = search_direction(
        &mut reader,
        data.len() as u64,
        &q,
        Direction::Backward,
        &flag,
        |_| {},
    )
    .unwrap();
    assert_eq!(r.state, "canceled");
    assert_eq!(r.checked_bytes, 7);
    assert_eq!(
        r.hits,
        (CHUNK..CHUNK + 7)
            .rev()
            .map(|i| i as u64)
            .collect::<Vec<_>>()
    );
    assert_eq!(reads.borrow().len(), 2);
    let mut reader = Traced {
        inner: Cursor::new(data.clone()),
        reads: Rc::new(RefCell::new(vec![])),
        fail_below: Some(CHUNK as u64),
        cancel_below: None,
    };
    let r = search_direction(
        &mut reader,
        data.len() as u64,
        &q,
        Direction::Backward,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    assert_eq!(r.state, "failed");
    assert_eq!(r.checked_bytes, 7);
    assert_eq!(r.hits.len(), 7);
    let mut reader = Cursor::new(data.clone());
    let r = search_direction(
        &mut reader,
        data.len() as u64,
        &q,
        Direction::Backward,
        &AtomicBool::new(true),
        |_| {},
    )
    .unwrap();
    assert_eq!(r.state, "canceled");
    assert_eq!(reader.position(), 0);
    assert_eq!(r.checked_bytes, 0);
    data.truncate(CHUNK + 3);
    let r = search_direction(
        &mut Cursor::new(data),
        (CHUNK + 7) as u64,
        &q,
        Direction::Backward,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    assert_eq!(r.state, "failed");
    assert!(r.hits.is_empty());
    assert_eq!(r.checked_bytes, 0);
}

#[test]
fn typed_direction_and_query_validation_reject_unknown_values_before_io() {
    assert_eq!(
        serde_json::from_str::<Direction>("\"backward\"").unwrap(),
        Direction::Backward
    );
    assert!(serde_json::from_str::<Direction>("\"reverse\"").is_err());
    assert!(Query {
        conditions: vec![hex("AA", 0), hex("BB", 0)],
        mode: Mode::All
    }
    .compile_for(Direction::Backward)
    .is_err());
    assert!(Query {
        conditions: vec![hex("AA BB", 65535)],
        mode: Mode::Any
    }
    .compile_for(Direction::Backward)
    .is_err());
    let r = run(&[0], &query(vec![hex("AA BB", 0)], Mode::Any));
    assert_eq!(r.state, "completed");
    assert!(r.hits.is_empty());
    assert_eq!(r.checked_bytes, 1);
}

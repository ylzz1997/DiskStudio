use super::*;
use std::io::{self, Cursor};
struct Fake {
    bytes: Cursor<Vec<u8>>,
    writes: Vec<(u64, usize)>,
    reads: Vec<(u64, usize)>,
    short: bool,
    corrupt: bool,
}
impl Fake {
    fn new(size: usize) -> Self {
        Self {
            bytes: Cursor::new(vec![0x79; size]),
            writes: vec![],
            reads: vec![],
            short: false,
            corrupt: false,
        }
    }
}
impl Seek for Fake {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.bytes.seek(p)
    }
}
impl Read for Fake {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.reads.push((self.bytes.position(), b.len()));
        self.bytes.read(b)
    }
}
impl Write for Fake {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let at = self.bytes.position();
        self.writes.push((at, b.len()));
        let n = if self.short { 17.min(b.len()) } else { b.len() };
        self.bytes.write_all(&b[..n])?;
        if self.corrupt {
            self.bytes.get_mut()[at as usize] ^= 1;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn range(size: u64) -> Range {
    Range {
        offset: 4096,
        size,
        sector: 512,
    }
}
fn wipe_options() -> WipeOptions {
    WipeOptions {
        pattern: Pattern::Bytes {
            hex: "AABBCC".into(),
        },
        passes: 2,
    }
}
#[test]
fn sparse_wipe_validates_all_ranges_before_io_and_preserves_gaps_with_random_4kn_passes() {
    let r = Range {
        offset: 4096,
        size: 4096,
        sector: 4096,
    };
    for ranges in [
        vec![],
        vec![r, r],
        vec![r, Range { offset: 0, ..r }],
        vec![
            r,
            Range {
                offset: 8192,
                sector: 512,
                ..r
            },
        ],
        vec![r, Range { offset: 8193, ..r }],
    ] {
        let mut io = Fake::new(32768);
        assert!(wipe_ranges(
            &mut io,
            &ranges,
            &wipe_options(),
            &AtomicBool::new(false),
            &mut WipeProgress::default(),
            |_| Ok(()),
            |_| Ok(()),
            |_| {}
        )
        .is_err());
        assert!(io.reads.is_empty() && io.writes.is_empty());
    }
    let mut io = Fake::new(32768);
    let mut progress = WipeProgress::default();
    let mut calls = 0u8;
    wipe_ranges(
        &mut io,
        &[r, Range { offset: 16384, ..r }],
        &WipeOptions {
            passes: 2,
            pattern: Pattern::Random,
        },
        &AtomicBool::new(false),
        &mut progress,
        |b| {
            calls += 1;
            b.fill(calls);
            Ok(())
        },
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(calls, 4);
    assert_eq!(progress.verified_passes, 2);
    assert_eq!(progress.written_bytes, 16384);
    assert_eq!(progress.readback_bytes, 16384);
    assert_ne!(progress.pass_sha256[0], progress.pass_sha256[1]);
    assert!(io.bytes.get_ref()[8192..16384].iter().all(|&b| b == 0x79));
    assert!(io.bytes.get_ref()[4096..8192].iter().all(|&b| b == 3));
    assert!(io.bytes.get_ref()[16384..20480].iter().all(|&b| b == 4));
}
#[test]
fn patterned_wipe_crosses_blocks_preserves_neighbors_and_verifies_every_pass() {
    let r = range(BLOCK as u64 + 4096);
    let mut io = Fake::new((r.offset + r.size + 4096) as usize);
    let mut p = WipeProgress::default();
    wipe(
        &mut io,
        r,
        &wipe_options(),
        &AtomicBool::new(false),
        &mut p,
        |_| panic!("not random"),
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(p.verified_passes, 2);
    assert_eq!(p.written_bytes, r.size * 2);
    assert_eq!(p.readback_bytes, r.size * 2);
    assert_eq!(p.pass_sha256.len(), 2);
    assert_eq!(p.pass_sha256[0], p.pass_sha256[1]);
    assert!(io.bytes.get_ref()[..4096].iter().all(|b| *b == 0x79));
    assert!(io.bytes.get_ref()[(4096 + r.size) as usize..]
        .iter()
        .all(|b| *b == 0x79));
    for (i, b) in io.bytes.get_ref()[4096..(4096 + r.size) as usize]
        .iter()
        .enumerate()
    {
        assert_eq!(*b, [0xaa, 0xbb, 0xcc][i % 3]);
    }
    assert_eq!(io.writes.len(), 4);
    assert_eq!(io.reads.len(), 4);
}
#[test]
fn random_wipe_hashes_actual_written_stream_without_regenerating_for_readback() {
    let r = range(BLOCK as u64 + 512);
    let mut io = Fake::new((8192 + r.size) as usize);
    let mut p = WipeProgress::default();
    let mut calls = 0;
    wipe(
        &mut io,
        r,
        &WipeOptions {
            pattern: Pattern::Random,
            passes: 2,
        },
        &AtomicBool::new(false),
        &mut p,
        |b| {
            calls += 1;
            b.fill(calls);
            Ok(())
        },
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(calls, 4);
    assert_eq!(p.verified_passes, 2);
    assert_ne!(p.pass_sha256[0], p.pass_sha256[1]);
}
#[test]
fn invalid_wipe_options_never_read_or_write() {
    for bad in [
        WipeOptions {
            passes: 0,
            ..wipe_options()
        },
        WipeOptions {
            passes: 8,
            ..wipe_options()
        },
        WipeOptions {
            passes: 1,
            pattern: Pattern::Bytes { hex: "ABC".into() },
        },
        WipeOptions {
            passes: 1,
            pattern: Pattern::Bytes {
                hex: "00".repeat(33),
            },
        },
    ] {
        let mut io = Fake::new(16384);
        assert!(wipe(
            &mut io,
            range(4096),
            &bad,
            &AtomicBool::new(false),
            &mut WipeProgress::default(),
            |_| Ok(()),
            |_| Ok(()),
            |_| {}
        )
        .is_err());
        assert!(io.writes.is_empty() && io.reads.is_empty());
    }
}
#[test]
fn wipe_short_write_sync_error_and_corrupt_readback_stop_without_retry() {
    for (short, corrupt, sync_error) in [
        (true, false, false),
        (false, false, true),
        (false, true, false),
    ] {
        let mut io = Fake::new(16384);
        io.short = short;
        io.corrupt = corrupt;
        let mut p = WipeProgress::default();
        assert!(wipe(
            &mut io,
            range(4096),
            &wipe_options(),
            &AtomicBool::new(false),
            &mut p,
            |_| Ok(()),
            |_| if sync_error {
                Err(io::Error::other("fault"))
            } else {
                Ok(())
            },
            |_| {}
        )
        .is_err());
        assert_eq!(io.writes.len(), 1);
        assert_eq!(p.verified_passes, 0);
        assert!(p.pass_sha256.is_empty());
        assert_eq!(p.written_bytes, if short { 17 } else { 4096 });
    }
}
#[test]
fn cancellation_keeps_partial_wipe_counts_and_never_rolls_back() {
    let r = range(BLOCK as u64 * 2);
    let mut io = Fake::new((r.size + 8192) as usize);
    let cancel = AtomicBool::new(false);
    let mut p = WipeProgress::default();
    let result = wipe(
        &mut io,
        r,
        &wipe_options(),
        &cancel,
        &mut p,
        |_| Ok(()),
        |_| Ok(()),
        |p| {
            if p.written_bytes > 0 {
                cancel.store(true, Ordering::Relaxed)
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(io.writes.len(), 1);
    assert_eq!(p.written_bytes, BLOCK as u64);
    assert_eq!(p.readback_bytes, 0);
    assert_eq!(p.verified_passes, 0);
    assert_eq!(io.bytes.get_ref()[4096], 0xaa);
}
fn bench(write: bool, access: Access) -> BenchmarkOptions {
    BenchmarkOptions {
        duration_seconds: None,
        thread_count: None,
        write,
        access,
        operations: 5,
        block_size: 4096,
        queue_depth: 1,
        read_percent: None,
    }
}
#[test]
fn sequential_benchmark_wraps_within_range_and_never_writes_in_read_mode() {
    let mut io = Fake::new(24576);
    let before = io.bytes.get_ref().clone();
    let mut p = BenchmarkProgress::default();
    benchmark(
        &mut io,
        range(12288),
        &bench(false, Access::Sequential),
        &AtomicBool::new(false),
        &mut p,
        |_| panic!("no random needed"),
        |_| panic!("no write fill for reads"),
        |_| panic!("no sync for reads"),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        io.reads,
        vec![
            (4096, 4096),
            (8192, 4096),
            (12288, 4096),
            (4096, 4096),
            (8192, 4096)
        ]
    );
    assert!(io.writes.is_empty());
    assert_eq!(&before, io.bytes.get_ref());
    assert_eq!(p.operations, 5);
    assert_eq!(p.transferred_bytes, 20480);
    assert!(p.iops.is_finite() && p.iops > 0.0);
}
#[test]
fn random_write_benchmark_verifies_each_request_and_preserves_outside_bytes() {
    let mut io = Fake::new(24576);
    let mut p = BenchmarkProgress::default();
    let mut seed = 0u64;
    benchmark(
        &mut io,
        range(12288),
        &bench(true, Access::Random),
        &AtomicBool::new(false),
        &mut p,
        |b| {
            seed += 1;
            b.copy_from_slice(&seed.to_le_bytes());
            Ok(())
        },
        |b| {
            b.fill(42);
            Ok(())
        },
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(p.operations, 5);
    assert_eq!(p.verified_bytes, p.transferred_bytes);
    assert_eq!(io.writes, io.reads);
    assert!(io
        .writes
        .iter()
        .all(|(at, n)| *at >= 4096 && *at + *n as u64 <= 16384));
    assert!(io.bytes.get_ref()[..4096]
        .iter()
        .chain(&io.bytes.get_ref()[16384..])
        .all(|b| *b == 0x79));
}
#[test]
fn mixed_benchmark_separates_workload_reads_writes_and_required_readback() {
    let mut io = Fake::new(24576);
    let opt = BenchmarkOptions {
        read_percent: Some(50),
        ..bench(true, Access::Sequential)
    };
    let mut p = BenchmarkProgress::default();
    benchmark(
        &mut io,
        range(12288),
        &opt,
        &AtomicBool::new(false),
        &mut p,
        |_| panic!("sequential positions need no random seed"),
        |bytes| {
            bytes.fill(42);
            Ok(())
        },
        |_| Ok(()),
        |_| {},
    )
    .unwrap();
    assert_eq!(p.operations, 5);
    assert_eq!((p.read_operations, p.write_operations), (3, 2));
    assert_eq!(
        (p.read_bytes, p.write_bytes, p.verified_bytes),
        (12288, 8192, 8192)
    );
    assert_eq!(io.writes, vec![(4096, 4096), (12288, 4096)]);
    assert_eq!(
        io.reads,
        vec![
            (4096, 4096),
            (8192, 4096),
            (12288, 4096),
            (4096, 4096),
            (8192, 4096)
        ]
    );
}
#[test]
fn benchmark_invalid_alignment_short_write_and_corruption_fail_closed() {
    let mut io = Fake::new(24576);
    let mut p = BenchmarkProgress::default();
    assert!(benchmark(
        &mut io,
        Range {
            offset: 512,
            ..range(12288)
        },
        &bench(true, Access::Sequential),
        &AtomicBool::new(false),
        &mut p,
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |_| {}
    )
    .is_err());
    assert!(io.writes.is_empty());
    for short in [true, false] {
        let mut io = Fake::new(24576);
        io.short = short;
        io.corrupt = !short;
        assert!(benchmark(
            &mut io,
            range(12288),
            &bench(true, Access::Sequential),
            &AtomicBool::new(false),
            &mut p,
            |_| panic!("sequential positions need no random seed"),
            |b| {
                b.fill(42);
                Ok(())
            },
            |_| Ok(()),
            |_| {}
        )
        .is_err());
        assert_eq!(io.writes.len(), 1);
        assert_eq!(p.verified_bytes, 0);
    }
}

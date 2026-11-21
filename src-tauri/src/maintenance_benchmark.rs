//! Raw benchmark scheduling and accounting. Callers authenticate and open devices.
use super::{buffer, check, valid, BLOCK};
use crate::surface_scan::{Range, ScanSource, TransferError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        mpsc::{sync_channel, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

const SAMPLE_NANOS: u64 = 250_000_000;
const MAX_SAMPLES: usize = 256;
pub const MAX_QUEUE_DEPTH: u8 = 32;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Sequential,
    Random,
}

fn single_queue() -> u8 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkOptions {
    pub access: Access,
    pub block_size: u32,
    pub operations: u32,
    pub write: bool,
    #[serde(default = "single_queue")]
    pub queue_depth: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_percent: Option<u8>,
}

impl BenchmarkOptions {
    pub fn validate(&self, range: Range) -> Result<(), String> {
        valid(range)?;
        if !self.block_size.is_power_of_two()
            || !(4096..=BLOCK as u32).contains(&self.block_size)
            || range.offset % 4096 != 0
            || range.size % 4096 != 0
            || range.size < u64::from(self.block_size)
        {
            return Err("测速范围须按 4 KiB 对齐，块大小须为 4 KiB 至 1 MiB 的 2 的幂。".into());
        }
        if !(1..=65536).contains(&self.operations) {
            return Err("请求次数须为 1–65536。".into());
        }
        if !(1..=MAX_QUEUE_DEPTH).contains(&self.queue_depth)
            || u32::from(self.queue_depth) > self.operations
            || self.write && self.queue_depth != 1
        {
            return Err("只读队列深度须为 1–32 且不超过请求次数。含写入的测速使用 QD1。".into());
        }
        if self.read_percent.is_some_and(|percent| {
            !self.write || !(1..=99).contains(&percent) || self.operations < 2
        }) {
            return Err("混合测速至少需要两个请求，读取比例须为 1–99%。".into());
        }
        Ok(())
    }

    fn writes(&self, index: u32) -> bool {
        let Some(percent) = self.read_percent else {
            return self.write;
        };
        // Spread the rounded read count through the run and always include
        // both directions, even in a test with only two requests.
        let reads =
            ((self.operations * u32::from(percent) + 50) / 100).clamp(1, self.operations - 1);
        index * reads / self.operations == (index + 1) * reads / self.operations
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkSample {
    elapsed_nanoseconds: u64,
    read_bytes: u64,
    write_bytes: u64,
}

#[derive(Clone, Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkProgress {
    pub operations: u32,
    pub transferred_bytes: u64,
    pub verified_bytes: u64,
    pub io_nanoseconds: u64,
    pub elapsed_ms: u64,
    pub min_microseconds: u64,
    pub max_microseconds: u64,
    pub p50_microseconds: u64,
    pub p95_microseconds: u64,
    pub mib_per_second: f64,
    pub iops: f64,
    pub read_operations: u32,
    pub write_operations: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub peak_queue_depth: u8,
    pub samples: Vec<BenchmarkSample>,
}

struct Measurement<'a> {
    progress: &'a mut BenchmarkProgress,
    started: Instant,
    latencies: Vec<u64>,
}

fn nanoseconds(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

impl<'a> Measurement<'a> {
    fn new(progress: &'a mut BenchmarkProgress, operations: u32) -> Self {
        *progress = BenchmarkProgress::default();
        Self {
            progress,
            started: Instant::now(),
            latencies: Vec::with_capacity(operations as usize),
        }
    }

    fn account(&mut self, bytes: usize, write: bool, completed: Option<Duration>) {
        let progress = &mut self.progress;
        progress.transferred_bytes += bytes as u64;
        if write {
            progress.write_bytes += bytes as u64;
        } else {
            progress.read_bytes += bytes as u64;
        }
        if let Some(duration) = completed {
            let ns = nanoseconds(duration).max(1);
            self.latencies.push(ns);
            progress.io_nanoseconds = progress.io_nanoseconds.saturating_add(ns);
            progress.operations += 1;
            if write {
                progress.write_operations += 1;
            } else {
                progress.read_operations += 1;
            }
        }
    }

    fn publish(&mut self, final_sample: bool, publish: &mut impl FnMut(&BenchmarkProgress)) {
        let elapsed = nanoseconds(self.started.elapsed()).max(1);
        self.publish_at(elapsed, final_sample, publish);
    }

    fn publish_at(
        &mut self,
        elapsed: u64,
        final_sample: bool,
        publish: &mut impl FnMut(&BenchmarkProgress),
    ) {
        let progress = &mut self.progress;
        let last = progress
            .samples
            .last()
            .map_or(0, |sample| sample.elapsed_nanoseconds);
        if !final_sample && elapsed - last < SAMPLE_NANOS {
            return;
        }
        if final_sample && !self.latencies.is_empty() {
            self.latencies.sort_unstable();
            let len = self.latencies.len();
            progress.min_microseconds = self.latencies[0] / 1000;
            progress.max_microseconds = self.latencies[len - 1] / 1000;
            progress.p50_microseconds = self.latencies[(len - 1) / 2] / 1000;
            progress.p95_microseconds = self.latencies[(len * 95).div_ceil(100) - 1] / 1000;
        }
        progress.elapsed_ms = elapsed / 1_000_000;
        let seconds = elapsed as f64 / 1e9;
        progress.mib_per_second = progress.transferred_bytes as f64 / BLOCK as f64 / seconds;
        progress.iops = f64::from(progress.operations) / seconds;
        if elapsed > last {
            if progress.samples.len() == MAX_SAMPLES {
                let mut index = 0;
                progress.samples.retain(|_| {
                    index += 1;
                    index % 2 == 0
                });
            }
            progress.samples.push(BenchmarkSample {
                elapsed_nanoseconds: elapsed,
                read_bytes: progress.read_bytes,
                write_bytes: progress.write_bytes,
            });
        }
        publish(progress);
    }
}

fn position(range: Range, opt: &BenchmarkOptions, index: u32, random: u64) -> u64 {
    let blocks = range.size / u64::from(opt.block_size);
    let block = match opt.access {
        Access::Sequential => u64::from(index) % blocks,
        Access::Random => random % blocks,
    };
    range.offset + block * u64::from(opt.block_size)
}

fn plan_position(
    range: Range,
    opt: &BenchmarkOptions,
    index: u32,
    random: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<u64, String> {
    let mut seed = [0u8; 8];
    if opt.access == Access::Random {
        random(&mut seed)?;
    }
    Ok(position(range, opt, index, u64::from_le_bytes(seed)))
}

/// QD1 for workloads that can write. Every write is flushed and verified
/// before accepting another operation on the same range.
pub fn benchmark<R: Read + Write + Seek>(
    io: &mut R,
    range: Range,
    opt: &BenchmarkOptions,
    cancel: &AtomicBool,
    progress: &mut BenchmarkProgress,
    mut random: impl FnMut(&mut [u8]) -> Result<(), String>,
    mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
    mut publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    opt.validate(range)?;
    if opt.queue_depth != 1 {
        return Err("只读队列深度须为 1–32 且不超过请求次数。含写入的测速使用 QD1。".into());
    }
    let mut read_buffer = buffer();
    let mut expected = buffer();
    let len = opt.block_size as usize;
    let read_buffer = &mut read_buffer.0[..len];
    let expected = &mut expected.0[..len];
    let mut measure = Measurement::new(progress, opt.operations);
    let result = (|| {
        for index in 0..opt.operations {
            check(cancel)?;
            let write = opt.writes(index);
            let at = plan_position(range, opt, index, &mut random)?;
            if write {
                random(expected)?;
            }
            io.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
            measure.progress.peak_queue_depth = 1;
            let started = Instant::now();
            let n = if write {
                io.write(expected)
            } else {
                io.read(read_buffer)
            }
            .map_err(|e| format!("测速 I/O 失败，写入可能已部分完成：{e}"))?;
            if n != len {
                measure.account(n, write, None);
                return Err(format!("测速短 I/O {n}/{len}，不重试。"));
            }
            if write {
                if let Err(e) = sync(io) {
                    measure.account(n, true, None);
                    return Err(format!("测速写入刷新失败：{e}"));
                }
            }
            measure.account(n, write, Some(started.elapsed()));
            if write {
                io.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
                let n = io.read(read_buffer).map_err(|e| e.to_string())?;
                if n != len || Sha256::digest(&*read_buffer) != Sha256::digest(&*expected) {
                    return Err("测速写入 SHA-256 回读失败。".into());
                }
                measure.progress.verified_bytes += n as u64;
            }
            measure.publish(false, &mut publish);
        }
        Ok(())
    })();
    measure.publish(true, &mut publish);
    result
}

/// Each read worker owns one authenticated handle and one aligned buffer. Any
/// failure stops dispatch. Outstanding native reads drain before return.
pub fn benchmark_reads<R: ScanSource + Send>(
    readers: Vec<R>,
    range: Range,
    opt: &BenchmarkOptions,
    cancel: &AtomicBool,
    progress: &mut BenchmarkProgress,
    mut random: impl FnMut(&mut [u8]) -> Result<(), String>,
    mut publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    opt.validate(range)?;
    if opt.write || readers.len() != usize::from(opt.queue_depth) {
        return Err("读取工作队列与已核对的设备句柄数量不一致。".into());
    }
    let mut positions = Vec::with_capacity(opt.operations as usize);
    for index in 0..opt.operations {
        if cancel.load(Ordering::Relaxed) {
            return Err("任务已取消。".into());
        }
        positions.push(plan_position(range, opt, index, &mut random)?);
    }
    let mut measure = Measurement::new(progress, opt.operations);
    let len = opt.block_size as usize;
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let active = AtomicU8::new(0);
    let peak = AtomicU8::new(0);
    let mut failure = None;
    thread::scope(|scope| {
        let (tx, rx) = sync_channel(readers.len());
        for mut reader in readers {
            let sender = tx.clone();
            let positions = &positions;
            let (next, stop, active, peak) = (&next, &stop, &active, &peak);
            if let Err(error) = thread::Builder::new().spawn_scoped(scope, move || {
                let mut storage = buffer();
                while !stop.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&offset) = positions.get(index) else {
                        break;
                    };
                    let count = active.fetch_add(1, Ordering::Relaxed) + 1;
                    peak.fetch_max(count, Ordering::Relaxed);
                    let started = Instant::now();
                    let read = reader.read_at(offset, &mut storage.0[..len], stop);
                    let elapsed = started.elapsed();
                    active.fetch_sub(1, Ordering::Relaxed);
                    if !matches!(read, Ok(n) if n == len) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if sender.send((read, elapsed)).is_err() {
                        break;
                    }
                }
            }) {
                failure = Some(format!("无法启动读取工作线程：{error}"));
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
        drop(tx);
        loop {
            if cancel.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok((Ok(n), elapsed)) if n == len => measure.account(n, false, Some(elapsed)),
                Ok((Ok(n), _)) => {
                    measure.account(n, false, None);
                    failure.get_or_insert_with(|| format!("测速短 I/O {n}/{len}，不重试。"));
                }
                Ok((Err(TransferError::Canceled), _)) | Err(RecvTimeoutError::Timeout) => {}
                Ok((Err(error), _)) => {
                    failure.get_or_insert_with(|| match error {
                        TransferError::Io(error) => format!("无缓存读取失败：{error}"),
                        #[cfg(test)]
                        TransferError::Position(error) => error.to_string(),
                        TransferError::TimedOut => "单次测速读取超时。".into(),
                        TransferError::Canceled => unreachable!(),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
            measure.progress.peak_queue_depth = peak.load(Ordering::Relaxed);
            measure.publish(false, &mut publish);
        }
    });
    measure.progress.peak_queue_depth = peak.load(Ordering::Relaxed);
    measure.publish(true, &mut publish);
    if let Some(error) = failure {
        return Err(error);
    }
    if measure.progress.operations != opt.operations {
        return if cancel.load(Ordering::Relaxed) {
            Err("任务已取消。".into())
        } else {
            Err("读取队列在完成全部请求前停止。".into())
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, Mutex};

    fn fixture(depth: u8, count: u32) -> (Range, BenchmarkOptions) {
        (
            Range {
                offset: 4096,
                size: 8192,
                sector: 4096,
            },
            BenchmarkOptions {
                access: Access::Sequential,
                block_size: 4096,
                operations: count,
                write: false,
                queue_depth: depth,
                read_percent: None,
            },
        )
    }

    struct Witness<'a> {
        barrier: Option<&'a Barrier>,
        offsets: &'a Mutex<Vec<u64>>,
    }
    impl ScanSource for Witness<'_> {
        fn read_at(
            &mut self,
            offset: u64,
            bytes: &mut [u8],
            _: &AtomicBool,
        ) -> Result<usize, TransferError> {
            if let Some(barrier) = self.barrier.take() {
                barrier.wait();
            }
            assert_eq!(bytes.as_ptr() as usize % 4096, 0);
            self.offsets.lock().unwrap().push(offset);
            Ok(bytes.len())
        }
    }

    #[test]
    fn four_readers_issue_bounded_concurrent_requests_and_report_wall_throughput() {
        let (range, options) = fixture(4, 12);
        let barrier = Barrier::new(4);
        let offsets = Mutex::new(Vec::new());
        let readers = (0..4)
            .map(|_| Witness {
                barrier: Some(&barrier),
                offsets: &offsets,
            })
            .collect();
        let mut outcome = BenchmarkProgress::default();
        benchmark_reads(
            readers,
            range,
            &options,
            &AtomicBool::new(false),
            &mut outcome,
            |_| unreachable!(),
            |_| {},
        )
        .unwrap();
        assert_eq!(outcome.peak_queue_depth, 4);
        assert_eq!(outcome.read_operations, 12);
        assert_eq!(outcome.read_bytes, 12 * 4096);
        assert_eq!(outcome.write_operations, 0);
        assert_eq!(outcome.verified_bytes, 0);
        let mut offsets = offsets.into_inner().unwrap();
        offsets.sort_unstable();
        assert_eq!(&offsets[..6], &[4096; 6]);
        assert_eq!(&offsets[6..], &[8192; 6]);
        let elapsed = outcome.samples.last().unwrap().elapsed_nanoseconds as f64 / 1e9;
        assert_eq!(outcome.iops, 12.0 / elapsed);
    }

    struct Interrupted<'a> {
        cancel: Option<&'a AtomicBool>,
        calls: &'a AtomicUsize,
    }
    impl ScanSource for Interrupted<'_> {
        fn read_at(
            &mut self,
            _: u64,
            buffer: &mut [u8],
            stop: &AtomicBool,
        ) -> Result<usize, TransferError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(cancel) = self.cancel {
                cancel.store(true, Ordering::Relaxed);
                while !stop.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
                Err(TransferError::Canceled)
            } else {
                Ok(buffer.len() - 1)
            }
        }
    }
    #[test]
    fn short_or_canceled_reads_stop_before_counting_or_dispatching_more_requests() {
        let (range, options) = fixture(1, 8);
        for cancel_mid_request in [false, true] {
            let cancel = AtomicBool::new(false);
            let calls = AtomicUsize::new(0);
            let reader = Interrupted {
                cancel: cancel_mid_request.then_some(&cancel),
                calls: &calls,
            };
            let mut stats = BenchmarkProgress::default();
            assert!(benchmark_reads(
                vec![reader],
                range,
                &options,
                &cancel,
                &mut stats,
                |_| unreachable!(),
                |_| {}
            )
            .is_err());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(stats.operations, 0);
            assert_eq!(stats.read_bytes, if cancel_mid_request { 0 } else { 4095 });
        }
    }

    #[test]
    fn options_and_samples_remain_compatible_bounded_and_monotonic() {
        let (range, _) = fixture(1, 8);
        let legacy: BenchmarkOptions = serde_json::from_str(
            r#"{"access":"random","blockSize":4096,"operations":5,"write":false}"#,
        )
        .unwrap();
        assert_eq!(legacy.queue_depth, 1);
        for options in [
            BenchmarkOptions {
                write: true,
                queue_depth: 2,
                ..legacy.clone()
            },
            BenchmarkOptions {
                read_percent: Some(75),
                ..legacy.clone()
            },
            BenchmarkOptions {
                queue_depth: 6,
                ..legacy.clone()
            },
        ] {
            assert!(options.validate(range).is_err());
        }
        let mut stats = BenchmarkProgress::default();
        let mut measure = Measurement::new(&mut stats, 300);
        for second in 1..=300 {
            measure.account(4096, false, Some(Duration::from_millis(1)));
            measure.publish_at(second * SAMPLE_NANOS, false, &mut |_| {});
        }
        measure.publish_at(301 * SAMPLE_NANOS, true, &mut |_| {});
        assert!(stats.samples.len() <= MAX_SAMPLES);
        assert!(stats
            .samples
            .windows(2)
            .all(|samples| samples[0].elapsed_nanoseconds < samples[1].elapsed_nanoseconds));
        assert_eq!(stats.samples.last().unwrap().read_bytes, 300 * 4096);
    }
}

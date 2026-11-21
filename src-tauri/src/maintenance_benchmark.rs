//! Benchmark scheduling and accounting. Callers authenticate and open devices.
use super::{check, valid, BLOCK};
use crate::surface_scan::{Range, ScanSource, TransferError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering},
        mpsc::{sync_channel, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

const SAMPLE_NANOS: u64 = 250_000_000;
const MAX_SAMPLES: usize = 256;
const MAX_RAW_QUEUE_DEPTH: u16 = 32;
const MAX_FILE_QUEUE_DEPTH: u16 = 512;
const MAX_FILE_THREADS: u8 = 64;
const MAX_FILE_BLOCK: u32 = 8 * BLOCK as u32;

#[derive(Clone)]
#[repr(C, align(4096))]
struct Page {
    _bytes: [u8; 4096],
}

fn aligned_pages(len: usize) -> Vec<Page> {
    vec![Page { _bytes: [0; 4096] }; len / 4096]
}

fn bytes(pages: &mut [Page]) -> &mut [u8] {
    // SAFETY: Page has 4096 bytes and 4096-byte alignment, so there is no
    // padding between array elements. The slice retains the exclusive borrow.
    let len = std::mem::size_of_val(pages);
    unsafe { std::slice::from_raw_parts_mut(pages.as_mut_ptr().cast::<u8>(), len) }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Sequential,
    Random,
}

fn single_queue() -> u16 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkOptions {
    pub access: Access,
    pub block_size: u32,
    pub operations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u8>,
    pub write: bool,
    #[serde(default = "single_queue")]
    pub queue_depth: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_count: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_percent: Option<u8>,
}

impl BenchmarkOptions {
    pub fn validate_raw(&self, range: Range) -> Result<(), String> {
        if self.duration_seconds.is_some() {
            return Err("按时长测速仅支持文件范围。".into());
        }
        if self.thread_count.is_some() || self.queue_depth > MAX_RAW_QUEUE_DEPTH {
            return Err("原始读取队列深度须为 1–32，线程数与队列深度相同。".into());
        }
        if self.block_size > BLOCK as u32 {
            return Err("测速范围须按 4 KiB 对齐，块大小须为 4 KiB 至 1 MiB 的 2 的幂。".into());
        }
        self.validate(range)
    }

    pub fn validate(&self, range: Range) -> Result<(), String> {
        valid(range)?;
        if !self.block_size.is_power_of_two()
            || !(4096..=MAX_FILE_BLOCK).contains(&self.block_size)
            || range.offset % 4096 != 0
            || range.size % 4096 != 0
            || range.size < u64::from(self.block_size)
        {
            return Err(
                "测速范围须按 4 KiB 对齐并容纳所选块，块大小须为 4 KiB 至 8 MiB 的 2 的幂。".into(),
            );
        }
        match self.duration_seconds {
            Some(5..=60) if self.operations == 0 => {}
            Some(_) => return Err("测速时长须为 5–60 秒，且不能同时指定请求次数。".into()),
            None if (1..=65536).contains(&self.operations) => {}
            None => return Err("请求次数须为 1–65536。".into()),
        }
        if !(1..=MAX_FILE_QUEUE_DEPTH).contains(&self.queue_depth)
            || self.duration_seconds.is_none() && u32::from(self.queue_depth) > self.operations
            || !(1..=MAX_FILE_THREADS).contains(&self.file_threads())
            || u16::from(self.file_threads()) > self.queue_depth
            || self.write && (self.queue_depth != 1 || self.file_threads() != 1)
        {
            return Err("文件读取支持 QD1–QD512 与 1–64 个线程。线程数不能超过队列深度，按次数运行时队列深度不能超过请求数。含写入的测速使用单线程 QD1。".into());
        }
        if self.read_percent.is_some_and(|percent| {
            !self.write
                || !(1..=99).contains(&percent)
                || self.duration_seconds.is_none() && self.operations < 2
        }) {
            return Err("混合测速至少需要两个请求，读取比例须为 1–99%。".into());
        }
        Ok(())
    }

    pub(crate) fn file_threads(&self) -> u8 {
        self.thread_count
            .unwrap_or(self.queue_depth.min(u16::from(MAX_FILE_THREADS)) as u8)
    }

    pub(crate) fn queue_sizes(&self) -> impl Iterator<Item = u16> {
        let workers = u16::from(self.file_threads().max(1));
        let (base, extra) = (self.queue_depth / workers, self.queue_depth % workers);
        (0..workers).map(move |index| base + u16::from(index < extra))
    }

    fn writes(&self, index: u64) -> bool {
        let Some(percent) = self.read_percent else {
            return self.write;
        };
        // Spread the rounded read count through the run and always include
        // both directions, even in a test with only two requests.
        let (span, reads) = if self.duration_seconds.is_some() {
            (100, u64::from(percent))
        } else {
            let span = u64::from(self.operations);
            (
                span,
                ((span * u64::from(percent) + 50) / 100).clamp(1, span - 1),
            )
        };
        let step = index % span;
        step * reads / span == (step + 1) * reads / span
    }

    fn can_dispatch(&self, started: Instant, ordinal: u64) -> bool {
        self.duration_seconds
            .map_or(ordinal < u64::from(self.operations), |seconds| {
                started.elapsed() < Duration::from_secs(u64::from(seconds))
            })
    }

    pub fn progress(&self, stats: &BenchmarkProgress) -> (u64, u64) {
        if let Some(seconds) = self.duration_seconds {
            let total = u64::from(seconds) * 1000;
            (stats.elapsed_ms.min(total), total)
        } else {
            (stats.operations, u64::from(self.operations))
        }
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
    pub operations: u64,
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
    pub read_operations: u64,
    pub write_operations: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub peak_queue_depth: u16,
    pub worker_threads: u8,
    pub samples: Vec<BenchmarkSample>,
    pub approximate_percentiles: bool,
}

struct Measurement<'a> {
    progress: &'a mut BenchmarkProgress,
    started: Instant,
    latencies: Latencies,
}

enum Latencies {
    Exact(Vec<u64>),
    Histogram(Vec<u64>),
}

impl Latencies {
    fn add(&mut self, ns: u64) {
        match self {
            Self::Exact(values) => values.push(ns),
            Self::Histogram(bins) => {
                let index = if ns < 32 {
                    ns as usize
                } else {
                    let power = ns.ilog2() as usize;
                    32 + (power - 5) * 16 + ((ns >> (power - 4)) as usize - 16)
                };
                bins[index] += 1;
            }
        }
    }

    fn percentiles(&mut self, count: u64) -> (u64, u64) {
        let ranks = [count.div_ceil(2), (count * 95).div_ceil(100)];
        let values = match self {
            Self::Exact(values) => {
                values.sort_unstable();
                ranks.map(|rank| values[rank as usize - 1])
            }
            Self::Histogram(bins) => ranks.map(|rank| {
                let mut total = 0;
                let index = bins
                    .iter()
                    .position(|&count| {
                        total += count;
                        total >= rank
                    })
                    .expect("every completed operation has a sample");
                if index < 32 {
                    index as u64
                } else {
                    let index = index - 32;
                    (16 + index as u64 % 16) << (index / 16 + 1)
                }
            }),
        };
        (values[0] / 1000, values[1] / 1000)
    }
}

fn nanoseconds(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

impl<'a> Measurement<'a> {
    fn new(progress: &'a mut BenchmarkProgress, options: &BenchmarkOptions) -> Self {
        *progress = BenchmarkProgress::default();
        progress.approximate_percentiles = options.duration_seconds.is_some();
        Self {
            progress,
            started: Instant::now(),
            latencies: if options.duration_seconds.is_some() {
                Latencies::Histogram(vec![0; 32 + 59 * 16])
            } else {
                Latencies::Exact(Vec::with_capacity(options.operations as usize))
            },
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
            self.latencies.add(ns);
            let us = ns / 1000;
            if progress.operations == 0 || us < progress.min_microseconds {
                progress.min_microseconds = us;
            }
            progress.max_microseconds = progress.max_microseconds.max(us);
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
        if final_sample && progress.operations > 0 {
            let bounds = (progress.min_microseconds, progress.max_microseconds);
            let (p50, p95) = self.latencies.percentiles(progress.operations);
            progress.p50_microseconds = p50.clamp(bounds.0, bounds.1);
            progress.p95_microseconds = p95.clamp(bounds.0, bounds.1);
        }
        progress.elapsed_ms = elapsed / 1_000_000;
        let seconds = elapsed as f64 / 1e9;
        progress.mib_per_second = progress.transferred_bytes as f64 / BLOCK as f64 / seconds;
        progress.iops = progress.operations as f64 / seconds;
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

fn position(range: Range, opt: &BenchmarkOptions, index: u64, random: u64) -> u64 {
    let blocks = range.size / u64::from(opt.block_size);
    let block = match opt.access {
        Access::Sequential => index % blocks,
        Access::Random => random % blocks,
    };
    range.offset + block * u64::from(opt.block_size)
}

// SplitMix64 gives a stable pseudo-random position for each issued request.
// Seed once before measurement so timed workers need no growing request list.
fn random_position(seed: u64, ordinal: u64) -> u64 {
    let mut value = seed.wrapping_add(ordinal.wrapping_mul(0x9e3779b97f4a7c15));
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn plan_position(
    range: Range,
    opt: &BenchmarkOptions,
    index: u64,
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
    mut fill: impl FnMut(&mut [u8]) -> Result<(), String>,
    mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
    mut publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    opt.validate(range)?;
    if opt.queue_depth != 1 {
        return Err("只读队列深度须为 1–32 且不超过请求次数。含写入的测速使用 QD1。".into());
    }
    let len = opt.block_size as usize;
    let mut read_buffer = aligned_pages(len);
    let mut expected = aligned_pages(len);
    let read_buffer = bytes(&mut read_buffer);
    let expected = bytes(&mut expected);
    let mut measure = Measurement::new(progress, opt);
    let result = (|| {
        for index in 0u64.. {
            if !opt.can_dispatch(measure.started, index) {
                break;
            }
            check(cancel)?;
            let write = opt.writes(index);
            let at = plan_position(range, opt, index, &mut random)?;
            if write {
                fill(expected)?;
            }
            io.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
            if !opt.can_dispatch(measure.started, index) {
                break;
            }
            measure.progress.peak_queue_depth = 1;
            measure.progress.worker_threads = 1;
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
        if measure.progress.operations == 0 {
            return Err("没有在测速期限内完成请求。".into());
        }
        Ok(())
    })();
    measure.publish(true, &mut publish);
    result
}

enum ReadEvent {
    Completed(Result<usize, TransferError>, Duration),
    WorkerFailed(String),
}

struct ReadDispatch<'a> {
    range: Range,
    options: &'a BenchmarkOptions,
    started: Instant,
    seed: u64,
    cancel: &'a AtomicBool,
    stop: AtomicBool,
    next: AtomicU64,
    active: AtomicU16,
    peak: AtomicU16,
    started_threads: AtomicU8,
}

pub(crate) struct ReadWorker<'a> {
    dispatch: &'a ReadDispatch<'a>,
    sender: SyncSender<ReadEvent>,
    has_issued: Cell<bool>,
}

impl ReadWorker<'_> {
    pub(crate) fn stopping(&self) -> bool {
        self.dispatch.stop.load(Ordering::Relaxed) || self.dispatch.cancel.load(Ordering::Relaxed)
    }

    pub(crate) fn next_offset(&self) -> Option<u64> {
        if self.stopping() {
            return None;
        }
        let job = self.dispatch;
        let index = job.next.fetch_add(1, Ordering::Relaxed);
        if !job.options.can_dispatch(job.started, index) {
            return None;
        }
        let random = if job.options.access == Access::Random {
            random_position(job.seed, index)
        } else {
            0
        };
        Some(position(job.range, job.options, index, random))
    }

    pub(crate) fn issued(&self) -> Instant {
        if !self.has_issued.replace(true) {
            self.dispatch
                .started_threads
                .fetch_add(1, Ordering::Relaxed);
        }
        let count = self.dispatch.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.dispatch.peak.fetch_max(count, Ordering::Relaxed);
        Instant::now()
    }

    pub(crate) fn completed(&self, read: Result<usize, TransferError>, started: Instant) {
        let elapsed = started.elapsed();
        self.dispatch.active.fetch_sub(1, Ordering::Relaxed);
        if !matches!(read, Ok(n) if n == self.dispatch.options.block_size as usize) {
            self.dispatch.stop.store(true, Ordering::Relaxed);
        }
        if self
            .sender
            .send(ReadEvent::Completed(read, elapsed))
            .is_err()
        {
            self.dispatch.stop.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) trait BenchmarkReadQueue: Send {
    fn capacity(&self) -> u16;
    /// Drain all submitted reads before returning, even on error or cancellation.
    fn run(self, worker: &ReadWorker<'_>) -> Result<(), String>;
}

struct SingleReader<R> {
    reader: R,
    storage: Vec<Page>,
}

impl<R: ScanSource + Send> BenchmarkReadQueue for SingleReader<R> {
    fn capacity(&self) -> u16 {
        1
    }

    fn run(mut self, worker: &ReadWorker<'_>) -> Result<(), String> {
        let buffer = bytes(&mut self.storage);
        while let Some(offset) = worker.next_offset() {
            let started = worker.issued();
            let result = self.reader.read_at(offset, buffer, &worker.dispatch.stop);
            worker.completed(result, started);
        }
        Ok(())
    }
}

pub fn benchmark_reads<R: ScanSource + Send>(
    readers: Vec<R>,
    range: Range,
    opt: &BenchmarkOptions,
    cancel: &AtomicBool,
    progress: &mut BenchmarkProgress,
    random: impl FnMut(&mut [u8]) -> Result<(), String>,
    publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    opt.validate(range)?;
    if readers.len() != usize::from(opt.queue_depth) {
        return Err("读取工作队列与已核对的设备句柄数量不一致。".into());
    }
    let queues = readers
        .into_iter()
        .map(|reader| SingleReader {
            reader,
            storage: aligned_pages(opt.block_size as usize),
        })
        .collect();
    measure_queues(queues, range, opt, cancel, progress, random, publish)
}

pub(crate) fn benchmark_file_queues<Q: BenchmarkReadQueue>(
    queues: Vec<Q>,
    range: Range,
    opt: &BenchmarkOptions,
    cancel: &AtomicBool,
    progress: &mut BenchmarkProgress,
    random: impl FnMut(&mut [u8]) -> Result<(), String>,
    publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    opt.validate(range)?;
    if !queues
        .iter()
        .map(BenchmarkReadQueue::capacity)
        .eq(opt.queue_sizes())
    {
        return Err("文件读取的异步队列数量与测速设置不一致。".into());
    }
    measure_queues(queues, range, opt, cancel, progress, random, publish)
}

fn measure_queues<Q: BenchmarkReadQueue>(
    queues: Vec<Q>,
    range: Range,
    opt: &BenchmarkOptions,
    cancel: &AtomicBool,
    progress: &mut BenchmarkProgress,
    mut random: impl FnMut(&mut [u8]) -> Result<(), String>,
    mut publish: impl FnMut(&BenchmarkProgress),
) -> Result<(), String> {
    if opt.write {
        return Err("读取队列只能执行只读负载。".into());
    }
    check(cancel)?;
    let mut seed = [0; 8];
    if opt.access == Access::Random {
        random(&mut seed)?;
    }
    let mut measure = Measurement::new(progress, opt);
    let dispatch = ReadDispatch {
        range,
        options: opt,
        started: measure.started,
        seed: u64::from_le_bytes(seed),
        cancel,
        stop: AtomicBool::new(false),
        next: AtomicU64::new(0),
        active: AtomicU16::new(0),
        peak: AtomicU16::new(0),
        started_threads: AtomicU8::new(0),
    };
    let mut failure = None;
    let mut interrupted = false;
    let len = opt.block_size as usize;
    thread::scope(|scope| {
        let (tx, rx) = sync_channel(usize::from(opt.queue_depth));
        for queue in queues {
            let worker = ReadWorker {
                dispatch: &dispatch,
                sender: tx.clone(),
                has_issued: Cell::new(false),
            };
            if let Err(error) = thread::Builder::new().spawn_scoped(scope, move || {
                if let Err(error) = queue.run(&worker) {
                    worker.dispatch.stop.store(true, Ordering::Relaxed);
                    let _ = worker.sender.send(ReadEvent::WorkerFailed(error));
                }
            }) {
                failure = Some(format!("无法启动读取工作线程：{error}"));
                dispatch.stop.store(true, Ordering::Relaxed);
                break;
            }
        }
        drop(tx);
        loop {
            if cancel.load(Ordering::Relaxed) {
                dispatch.stop.store(true, Ordering::Relaxed);
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ReadEvent::Completed(Ok(n), elapsed)) if n == len => {
                    measure.account(n, false, Some(elapsed))
                }
                Ok(ReadEvent::Completed(Ok(n), _)) => {
                    measure.account(n, false, None);
                    failure.get_or_insert_with(|| format!("测速短 I/O {n}/{len}，不重试。"));
                }
                Ok(ReadEvent::Completed(Err(TransferError::Canceled), _)) => interrupted = true,
                Ok(ReadEvent::Completed(Err(error), _)) => {
                    failure.get_or_insert_with(|| match error {
                        TransferError::Io(error) => format!("无缓存读取失败：{error}"),
                        #[cfg(test)]
                        TransferError::Position(error) => error.to_string(),
                        TransferError::TimedOut => "单次测速读取超时。".into(),
                        TransferError::Canceled => unreachable!(),
                    });
                }
                Ok(ReadEvent::WorkerFailed(error)) => {
                    failure.get_or_insert(error);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            measure.progress.peak_queue_depth = dispatch.peak.load(Ordering::Relaxed);
            measure.progress.worker_threads = dispatch.started_threads.load(Ordering::Relaxed);
            measure.publish(false, &mut publish);
        }
    });
    measure.progress.peak_queue_depth = dispatch.peak.load(Ordering::Relaxed);
    measure.progress.worker_threads = dispatch.started_threads.load(Ordering::Relaxed);
    measure.publish(true, &mut publish);
    if let Some(error) = failure {
        return Err(error);
    }
    let finished = if opt.duration_seconds.is_some() {
        !opt.can_dispatch(dispatch.started, 0)
            && measure.progress.operations > 0
            && !interrupted
            && !cancel.load(Ordering::Relaxed)
    } else {
        measure.progress.operations == u64::from(opt.operations)
    };
    if !finished {
        return if cancel.load(Ordering::Relaxed) {
            Err("任务已取消。".into())
        } else {
            Err("读取队列在测速完成前停止。".into())
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Barrier, Mutex};

    fn fixture(depth: u16, count: u32) -> (Range, BenchmarkOptions) {
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
                duration_seconds: None,
                write: false,
                queue_depth: depth,
                thread_count: None,
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
        let mut measure = Measurement::new(&mut stats, &fixture(1, 300).1);
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

    #[test]
    fn timed_workloads_validate_deadline_mix_and_bounded_latency_storage() {
        let (range, _) = fixture(1, 8);
        let mut options = BenchmarkOptions {
            operations: 0,
            duration_seconds: Some(5),
            write: true,
            read_percent: Some(33),
            ..fixture(1, 8).1
        };
        assert!(options.validate(range).is_ok());
        assert!(options.validate_raw(range).is_err());
        assert!(options.can_dispatch(Instant::now(), 100_000));
        assert!(!options.can_dispatch(Instant::now() - Duration::from_secs(5), 0));
        assert_eq!((0..300).filter(|&index| !options.writes(index)).count(), 99);
        let mut stats = BenchmarkProgress::default();
        let mut timed = Measurement::new(&mut stats, &options);
        for index in 1..=100_000 {
            timed.account(4096, false, Some(Duration::from_nanos(index)));
        }
        timed.publish_at(5_100_000_000, true, &mut |_| {});
        assert!(stats.approximate_percentiles);
        assert_eq!(stats.operations, 100_000);
        assert!((46..=50).contains(&stats.p50_microseconds));
        assert!((89..=95).contains(&stats.p95_microseconds));
        assert_eq!(options.progress(&stats), (5000, 5000));
        options.operations = 8;
        assert!(options.validate(range).is_err());
        options.operations = 0;
        options.duration_seconds = Some(4);
        assert!(options.validate(range).is_err());
    }

    struct DeepQueue<'a> {
        capacity: u16,
        first_batch_ready: &'a Barrier,
    }

    impl BenchmarkReadQueue for DeepQueue<'_> {
        fn capacity(&self) -> u16 {
            self.capacity
        }

        fn run(self, worker: &ReadWorker<'_>) -> Result<(), String> {
            let requests: Vec<_> = (0..self.capacity)
                .map(|_| {
                    let offset = worker.next_offset().unwrap();
                    assert!(matches!(offset, 4096 | 8192));
                    worker.issued()
                })
                .collect();
            self.first_batch_ready.wait();
            for start in requests {
                worker.completed(Ok(4096), start);
            }
            Ok(())
        }
    }

    #[test]
    fn file_queue_depth_is_independent_of_actual_submission_threads() {
        let (range, mut options) = fixture(512, 512);
        options.thread_count = Some(3);
        let barrier = Barrier::new(3);
        assert_eq!(options.queue_sizes().collect::<Vec<_>>(), [171, 171, 170]);
        assert!(options.validate_raw(range).is_err());
        let mut stats = BenchmarkProgress::default();
        let workers = options
            .queue_sizes()
            .map(|capacity| DeepQueue {
                capacity,
                first_batch_ready: &barrier,
            })
            .collect();
        benchmark_file_queues(
            workers,
            range,
            &options,
            &AtomicBool::new(false),
            &mut stats,
            |_| unreachable!(),
            |_| {},
        )
        .unwrap();
        assert_eq!(stats.worker_threads, 3);
        assert_eq!(stats.peak_queue_depth, 512);
        assert_eq!(stats.read_operations, 512);
        options.thread_count = Some(65);
        assert!(options.validate(range).is_err());
    }
}

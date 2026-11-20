//! Read-only, bounded surface verification. No device discovery or writes.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

pub const BLOCK: u64 = 1024 * 1024;
pub const CELLS: usize = 256;
const MAX_ERRORS: usize = 1024;
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    #[default]
    Forward,
    Backward,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub start_sector: u64,
    pub sector_count: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Range {
    pub offset: u64,
    pub size: u64,
    pub sector: u64,
}
impl Options {
    pub fn range(self, offset: u64, size: u64, sector: u64) -> Result<Range, String> {
        if !matches!(sector, 512 | 4096)
            || offset % sector != 0
            || size % sector != 0
            || self.sector_count == 0
        {
            return Err("请选择非空且按逻辑扇区对齐的检测范围。".into());
        }
        let start = self
            .start_sector
            .checked_mul(sector)
            .ok_or("起始扇区溢出")?;
        let len = self
            .sector_count
            .checked_mul(sector)
            .ok_or("扇区数量溢出")?;
        if start.checked_add(len).is_none_or(|end| end > size)
            || offset
                .checked_add(size)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err("检测范围超出所选磁盘或分区。".into());
        }
        Ok(Range {
            offset: offset + start,
            size: len,
            sector,
        })
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cell {
    pub checked_bytes: u64,
    pub failed_bytes: u64,
    pub worst: u8,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadFailure {
    pub start_sector: u64,
    pub sector_count: u64,
    pub code: i32,
    pub elapsed_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    pub state: String,
    pub offset: u64,
    pub size: u64,
    pub sector_size: u64,
    pub block_size: u64,
    pub checked_bytes: u64,
    pub readable_bytes: u64,
    pub failed_bytes: u64,
    pub elapsed_ms: u64,
    pub latency_blocks: [u64; 6],
    pub cells: Vec<Cell>,
    pub failures: Vec<ReadFailure>,
    pub omitted_failures: u64,
    pub message: String,
}
impl Report {
    pub fn range(&self) -> Range {
        Range {
            offset: self.offset,
            size: self.size,
            sector: self.sector_size,
        }
    }
    /// Validate a checkpoint in bounded time, independent of disk capacity.
    pub fn validate(&self) -> Result<(), String> {
        let invalid = || "检测进度的范围或统计数据无效。".to_string();
        Options {
            start_sector: 0,
            sector_count: self.size / self.sector_size.max(1),
        }
        .range(self.offset, self.size, self.sector_size)?;
        if self.block_size != BLOCK
            || self.checked_bytes > self.size
            || (self.checked_bytes != self.size && self.checked_bytes % BLOCK != 0)
            || self.readable_bytes.checked_add(self.failed_bytes) != Some(self.checked_bytes)
            || self.elapsed_ms > 9_007_199_254_740_991
            || self.message.len() > 4096
            || !matches!(
                self.state.as_str(),
                "running" | "completed" | "paused" | "canceled" | "failed"
            )
            || (self.state == "completed" && self.checked_bytes != self.size)
        {
            return Err(invalid());
        }
        let blocks = self.size.div_ceil(BLOCK);
        let checked = self.checked_bytes.div_ceil(BLOCK);
        let failures = self.latency_blocks[5];
        if self
            .latency_blocks
            .iter()
            .try_fold(0u64, |sum, n| sum.checked_add(*n))
            != Some(checked)
            || self.cells.len() != CELLS.min(blocks as usize)
            || self.failures.len() != MAX_ERRORS.min(failures as usize)
            || self.omitted_failures != failures.saturating_sub(MAX_ERRORS as u64)
        {
            return Err(invalid());
        }
        let full_failed = failures.checked_mul(BLOCK).ok_or_else(invalid)?;
        let short_tail = self.size % BLOCK;
        let last_failed = short_tail != 0
            && self.checked_bytes == self.size
            && failures > 0
            && self.failed_bytes == full_failed - (BLOCK - short_tail);
        if self.failed_bytes != full_failed && !last_failed {
            return Err(invalid());
        }
        let mut failed_sum = 0u64;
        for (index, cell) in self.cells.iter().enumerate() {
            let first =
                (index as u128 * blocks as u128).div_ceil(self.cells.len() as u128) as u64 * BLOCK;
            let end = ((index + 1) as u128 * blocks as u128).div_ceil(self.cells.len() as u128)
                as u64
                * BLOCK;
            let expected = self.checked_bytes.min(end).saturating_sub(first);
            if cell.checked_bytes != expected
                || cell.failed_bytes > expected
                || cell.failed_bytes % self.sector_size != 0
                || cell.worst > 5
                || (expected == 0 && cell.worst != 0)
                || (cell.worst == 5) != (cell.failed_bytes > 0)
            {
                return Err(invalid());
            }
            failed_sum = failed_sum
                .checked_add(cell.failed_bytes)
                .ok_or_else(invalid)?;
        }
        if failed_sum != self.failed_bytes {
            return Err(invalid());
        }
        let mut prior = None;
        let mut listed_bytes = 0u64;
        for entry in &self.failures {
            let start = entry
                .start_sector
                .checked_mul(self.sector_size)
                .ok_or_else(invalid)?;
            let relative = start.checked_sub(self.offset).ok_or_else(invalid)?;
            let len = entry
                .sector_count
                .checked_mul(self.sector_size)
                .ok_or_else(invalid)?;
            if relative >= self.checked_bytes
                || relative % BLOCK != 0
                || len != BLOCK.min(self.size - relative)
                || relative
                    .checked_add(len)
                    .is_none_or(|end| end > self.checked_bytes)
                || prior.is_some_and(|p| p >= relative)
                || !matches!(entry.code, 23 | 27 | 1117)
                || entry.elapsed_ms > self.elapsed_ms
            {
                return Err(invalid());
            }
            prior = Some(relative);
            listed_bytes += len;
        }
        if listed_bytes > self.failed_bytes
            || (self.omitted_failures == 0 && listed_bytes != self.failed_bytes)
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn new(range: Range) -> Self {
        Self {
            state: "running".into(),
            offset: range.offset,
            size: range.size,
            sector_size: range.sector,
            block_size: BLOCK,
            checked_bytes: 0,
            readable_bytes: 0,
            failed_bytes: 0,
            elapsed_ms: 0,
            latency_blocks: [0; 6],
            cells: vec![Cell::default(); CELLS.min(range.size.div_ceil(BLOCK) as usize)],
            failures: vec![],
            omitted_failures: 0,
            message: String::new(),
        }
    }
    fn record(&mut self, len: u64, ms: u64, code: Option<i32>) {
        let block = self.checked_bytes / BLOCK;
        let count = self.size.div_ceil(BLOCK);
        let index = (block as u128 * self.cells.len() as u128 / count as u128) as usize;
        let cell = &mut self.cells[index];
        let bucket = if code.is_some() {
            5
        } else {
            [10, 50, 150, 500]
                .iter()
                .position(|limit| ms < *limit)
                .unwrap_or(4)
        };
        self.latency_blocks[bucket] += 1;
        cell.checked_bytes += len;
        cell.worst = cell.worst.max(bucket as u8);
        if let Some(code) = code {
            self.failed_bytes += len;
            cell.failed_bytes += len;
            if self.failures.len() < MAX_ERRORS {
                self.failures.push(ReadFailure {
                    start_sector: (self.offset + self.checked_bytes) / self.sector_size,
                    sector_count: len / self.sector_size,
                    code,
                    elapsed_ms: ms,
                });
            } else {
                self.omitted_failures += 1;
            }
        } else {
            self.readable_bytes += len;
        }
        self.checked_bytes += len;
    }
}
#[repr(align(4096))]
struct AlignedBlock([u8; BLOCK as usize]);

/// One read per block: no retries or writes. Only media/I/O errors continue;
/// access, disconnect, alignment, seek and short-read errors stop the job.
#[cfg(test)]
pub fn scan<R: Read + Seek>(
    reader: &mut R,
    range: Range,
    cancel: &AtomicBool,
    publish: impl FnMut(&Report),
) -> Result<Report, String> {
    // Validate even callers that constructed Range directly.
    Options {
        start_sector: 0,
        sector_count: range.size / range.sector.max(1),
    }
    .range(range.offset, range.size, range.sector)?;
    continue_scan(reader, Report::new(range), cancel, publish)
}
pub fn continue_scan<R: Read + Seek>(
    reader: &mut R,
    mut report: Report,
    cancel: &AtomicBool,
    mut publish: impl FnMut(&Report),
) -> Result<Report, String> {
    report.validate()?;
    let range = report.range();
    let previous_ms = report.elapsed_ms;
    report.state = "running".into();
    report.message.clear();
    let mut buffer = Box::<AlignedBlock>::new_uninit();
    // SAFETY: the allocation has AlignedBlock's 4096-byte alignment; all-zero
    // bytes are valid for its only field. Initialize on heap to avoid stack overflow.
    let mut buffer = unsafe {
        buffer.as_mut_ptr().write_bytes(0, 1);
        buffer.assume_init()
    };
    let began = Instant::now();
    let mut last = Instant::now();
    publish(&report);
    while report.checked_bytes < range.size {
        if cancel.load(Ordering::Relaxed) {
            report.state = "canceled".into();
            report.message = "检测已取消；未检测范围不计入结果。".into();
            break;
        }
        let len = (range.size - report.checked_bytes).min(BLOCK) as usize;
        if let Err(e) = reader.seek(SeekFrom::Start(range.offset + report.checked_bytes)) {
            report.state = "failed".into();
            report.message = format!("定位失败，检测已停止：{e}");
            break;
        }
        let started = Instant::now();
        let result = reader.read(&mut buffer.0[..len]);
        let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        match result {
            Ok(n) if n == len => report.record(len as u64, ms, None),
            Err(e) if matches!(e.raw_os_error(), Some(23 | 27 | 1117)) => {
                report.record(len as u64, ms, e.raw_os_error())
            }
            result => {
                report.state = "failed".into();
                report.message = match result {
                    Ok(n) => format!("读取长度异常：请求 {len} 字节、收到 {n} 字节，检测已停止。"),
                    Err(e) => format!("设备访问失败，检测已停止：{e}"),
                };
                break;
            }
        }
        report.elapsed_ms =
            previous_ms.saturating_add(began.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if last.elapsed().as_millis() >= 200 {
            publish(&report);
            last = Instant::now();
        }
    }
    report.elapsed_ms =
        previous_ms.saturating_add(began.elapsed().as_millis().min(u64::MAX as u128) as u64);
    if report.state == "running" {
        report.state = "completed".into();
        report.message = if report.failed_bytes == 0 {
            "所选范围已读完，未发现读取失败。此结果不代表磁盘健康保证。"
        } else {
            "所选范围已检测完成，存在无法完整读取的块；失败块范围不等于精确坏扇区数量。"
        }
        .into();
    }
    publish(&report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};
    #[test]
    fn resume_reads_only_unfinished_tail_and_preserves_failures_and_time() {
        let canceled = AtomicBool::new(false);
        struct StopAfterTwo<'a> {
            inner: Fake,
            flag: &'a AtomicBool,
        }
        impl Seek for StopAfterTwo<'_> {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.inner.seek(from)
            }
        }
        impl Read for StopAfterTwo<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let result = self.inner.read(out);
                if self.inner.calls.len() == 2 {
                    self.flag.store(true, Ordering::Relaxed);
                }
                result
            }
        }
        let mut first = StopAfterTwo {
            inner: fake(Some(23)),
            flag: &canceled,
        };
        let mut report = scan(&mut first, range(BLOCK * 2 + 512), &canceled, |_| {}).unwrap();
        assert_eq!(report.state, "canceled");
        assert_eq!(report.checked_bytes, BLOCK * 2);
        report.elapsed_ms = 1000;
        report.validate().unwrap();
        let mut second = fake(None);
        let complete = continue_scan(&mut second, report, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(second.calls, vec![(4096 + BLOCK * 2, 512)]);
        assert_eq!(complete.state, "completed");
        assert_eq!(complete.failed_bytes, BLOCK * 2);
        assert_eq!(complete.readable_bytes, 512);
        assert_eq!(complete.failures.len(), 2);
        assert!(complete.elapsed_ms >= 1000);
        complete.validate().unwrap();
    }
    #[test]
    fn invalid_resume_statistics_are_rejected_before_device_io() {
        let original = scan(
            &mut fake(Some(23)),
            range(BLOCK + 512),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        original.validate().unwrap();
        let mut cases = vec![];
        let mut r = original.clone();
        r.checked_bytes = 512;
        cases.push(r);
        let mut r = original.clone();
        r.cells[0].checked_bytes += 512;
        cases.push(r);
        let mut r = original.clone();
        r.cells[0].worst = 6;
        cases.push(r);
        let mut r = original.clone();
        r.latency_blocks[0] = u64::MAX;
        cases.push(r);
        let mut r = original.clone();
        r.failures[1].start_sector = r.failures[0].start_sector;
        cases.push(r);
        let mut r = original.clone();
        r.failures[0].sector_count = 1;
        cases.push(r);
        let mut r = original.clone();
        r.omitted_failures = 1;
        cases.push(r);
        for invalid in cases {
            let mut reader = fake(None);
            assert!(continue_scan(&mut reader, invalid, &AtomicBool::new(false), |_| {}).is_err());
            assert!(reader.calls.is_empty());
        }
    }
    #[test]
    fn resumed_four_kn_and_truncated_error_history_stay_consistent() {
        let mut report = scan(
            &mut fake(Some(1117)),
            range(BLOCK * 1030 + 512),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        report.validate().unwrap();
        assert_eq!(report.omitted_failures, 7);
        report.cells.last_mut().unwrap().failed_bytes -= 512;
        assert!(report.validate().is_err());
        let mut report = Report::new(Range {
            offset: 4096,
            size: BLOCK + 4096,
            sector: 4096,
        });
        report.record(BLOCK, 9, None);
        report.state = "paused".into();
        let mut reader = fake(None);
        let final_report =
            continue_scan(&mut reader, report, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(reader.calls, vec![(4096 + BLOCK, 4096)]);
        final_report.validate().unwrap();
    }
    struct Fake {
        pos: u64,
        calls: Vec<(u64, usize)>,
        error: Option<i32>,
        short: bool,
    }
    impl Seek for Fake {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            if let SeekFrom::Start(n) = from {
                self.pos = n;
                Ok(n)
            } else {
                unreachable!()
            }
        }
    }
    impl Read for Fake {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            assert_eq!(out.as_ptr() as usize % 4096, 0);
            self.calls.push((self.pos, out.len()));
            if let Some(code) = self.error {
                Err(io::Error::from_raw_os_error(code))
            } else {
                Ok(out.len() - usize::from(self.short))
            }
        }
    }
    fn fake(error: Option<i32>) -> Fake {
        Fake {
            pos: 0,
            calls: vec![],
            error,
            short: false,
        }
    }
    fn range(size: u64) -> Range {
        Range {
            offset: 4096,
            size,
            sector: 512,
        }
    }
    #[test]
    fn boundaries_overflow_alignment_are_checked_before_io() {
        for (start, count, offset, size, sector) in [
            (u64::MAX, 1, 0, 4096, 512),
            (0, 0, 0, 4096, 512),
            (8, 1, 0, 4096, 512),
            (0, 1, u64::MAX - 511, 512, 512),
            (0, 1, 0, 4096, 0),
            (0, 1, 0, 513, 512),
            (0, 1, 1, 4096, 512),
        ] {
            assert!(Options {
                start_sector: start,
                sector_count: count
            }
            .range(offset, size, sector)
            .is_err());
        }
        let mut f = fake(None);
        assert!(scan(&mut f, range(513), &AtomicBool::new(false), |_| {}).is_err());
        assert!(f.calls.is_empty());
    }
    #[test]
    fn exact_range_and_partial_final_block_are_read_once() {
        let mut f = fake(None);
        let r = scan(&mut f, range(BLOCK + 512), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(f.calls, vec![(4096, BLOCK as usize), (4096 + BLOCK, 512)]);
        assert_eq!(r.readable_bytes, BLOCK + 512);
        assert_eq!(r.state, "completed");
        assert_eq!(r.cells.iter().map(|c| c.checked_bytes).sum::<u64>(), r.size);
    }
    #[test]
    fn media_failures_are_ranges_not_invented_bad_sector_counts() {
        let mut f = fake(Some(23));
        let r = scan(&mut f, range(BLOCK + 512), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.failed_bytes, r.size);
        assert_eq!(r.readable_bytes, 0);
        assert_eq!(r.failures[0].start_sector, 8);
        assert_eq!(r.failures[1].sector_count, 1);
        assert_eq!(f.calls.len(), 2);
        assert_eq!(r.latency_blocks[5], 2);
    }
    #[test]
    fn access_disconnect_and_short_read_stop_without_marking_bad_blocks() {
        for code in [5, 21, 87, 1167] {
            let mut f = fake(Some(code));
            let r = scan(&mut f, range(BLOCK * 2), &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(r.state, "failed");
            assert_eq!(r.failed_bytes, 0);
            assert_eq!(r.checked_bytes, 0);
            assert_eq!(f.calls.len(), 1);
        }
        let mut f = fake(None);
        f.short = true;
        assert_eq!(
            scan(&mut f, range(BLOCK), &AtomicBool::new(false), |_| {})
                .unwrap()
                .state,
            "failed"
        );
    }
    #[test]
    fn cancellation_does_not_read_and_keeps_unscanned_cells_unknown() {
        let mut f = fake(None);
        let cancel = AtomicBool::new(false);
        let r = scan(&mut f, range(BLOCK), &cancel, |_| {
            cancel.store(true, Ordering::Relaxed)
        })
        .unwrap();
        assert_eq!(r.state, "canceled");
        assert_eq!(r.checked_bytes, 0);
        assert!(f.calls.is_empty());
    }
    #[test]
    fn failure_storage_and_grid_remain_bounded_without_losing_totals() {
        let mut f = fake(Some(1117));
        let r = scan(&mut f, range(BLOCK * 1030), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.failures.len(), MAX_ERRORS);
        assert_eq!(r.omitted_failures, 6);
        assert_eq!(r.cells.len(), CELLS);
        assert_eq!(r.failed_bytes, r.size);
        assert_eq!(r.cells.iter().map(|c| c.failed_bytes).sum::<u64>(), r.size);
    }
    #[test]
    fn four_kn_with_nonzero_relative_start_is_bounded() {
        let r = Options {
            start_sector: 1,
            sector_count: 2,
        }
        .range(4096, 16384, 4096)
        .unwrap();
        let mut input = Cursor::new(vec![0; 32768]);
        let result = scan(&mut input, r, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(input.position(), 16384);
        assert_eq!(result.offset, 8192);
        assert_eq!(result.readable_bytes, 8192);
    }
}

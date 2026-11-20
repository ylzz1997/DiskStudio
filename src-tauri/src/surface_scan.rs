//! Read-only, bounded surface verification. No device discovery or writes.
use serde::{Deserialize, Serialize};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

pub const BLOCK: u64 = 1024 * 1024;
pub const CELLS: usize = 256;
const BUFFER_ALIGNMENT: usize = 4096;
const MAX_ERRORS: usize = 1024;
const MAX_FOLLOW_UP_PER_BLOCK: u64 = 64;
const MAX_FOLLOW_UP_READS: u64 = 8192;

pub fn validate_read_timeout(seconds: Option<u16>) -> Result<(), String> {
    if seconds.is_some_and(|value| !(1..=120).contains(&value)) {
        return Err("单次读取超时须为 1 到 120 秒。".into());
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    #[default]
    Forward,
    Backward,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
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
pub struct TimedOutRead {
    pub start_sector: u64,
    pub sector_count: u64,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FailureResolution {
    SectorReadFailed,
    Unresolved,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RefinedRange {
    pub start_sector: u64,
    pub sector_count: u64,
    pub code: i32,
    pub resolution: FailureResolution,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Refinement {
    pub reads: u64,
    pub failed_sectors: u64,
    pub unresolved_sectors: u64,
    pub ranges: Vec<RefinedRange>,
    pub omitted_ranges: u64,
}

impl Refinement {
    fn record(&mut self, ranges: Vec<RefinedRange>) {
        for entry in ranges {
            match entry.resolution {
                FailureResolution::SectorReadFailed => self.failed_sectors += 1,
                FailureResolution::Unresolved => self.unresolved_sectors += entry.sector_count,
            }
            if self.ranges.len() < MAX_ERRORS {
                self.ranges.push(entry);
            } else {
                self.omitted_ranges += 1;
            }
        }
    }

    fn validate(&self, report: &Report) -> Result<(), String> {
        let invalid = || "扇区细查进度的范围或统计数据无效。".to_owned();
        if self.reads > MAX_FOLLOW_UP_READS
            || self.ranges.len() > MAX_ERRORS
            || self
                .failed_sectors
                .checked_add(self.unresolved_sectors)
                .is_none_or(|sum| sum > report.failed_bytes / report.sector_size)
            || (self.omitted_ranges > 0 && self.ranges.len() != MAX_ERRORS)
        {
            return Err(invalid());
        }

        let origin = report.offset / report.sector_size;
        let len = report.checked_bytes / report.sector_size;
        let block = BLOCK / report.sector_size;
        let mut prior_end = 0;
        let (mut failed, mut unresolved) = (0, 0);
        for entry in &self.ranges {
            let at = entry.start_sector.checked_sub(origin).ok_or_else(invalid)?;
            let end = at.checked_add(entry.sector_count).ok_or_else(invalid)?;
            let initial = entry.start_sector - at % block;
            let recorded = report
                .failures
                .binary_search_by_key(&initial, |failure| failure.start_sector)
                .is_ok();
            let omitted = report.omitted_failures != 0
                && report
                    .failures
                    .last()
                    .is_some_and(|last| initial > last.start_sector);
            if entry.sector_count == 0
                || at < prior_end
                || end > len
                || at / block != (end - 1) / block
                || (!recorded && !omitted)
                || !media_error(entry.code)
            {
                return Err(invalid());
            }
            match entry.resolution {
                FailureResolution::SectorReadFailed if entry.sector_count == 1 => failed += 1,
                FailureResolution::Unresolved => unresolved += entry.sector_count,
                _ => return Err(invalid()),
            }
            prior_end = end;
        }
        let missing = self
            .failed_sectors
            .checked_sub(failed)
            .ok_or_else(invalid)?
            + self
                .unresolved_sectors
                .checked_sub(unresolved)
                .ok_or_else(invalid)?;
        if self.omitted_ranges > missing || (self.omitted_ranges == 0 && missing != 0) {
            return Err(invalid());
        }
        Ok(())
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refinement: Option<Refinement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_timeout_seconds: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out_read: Option<TimedOutRead>,
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
                "running" | "completed" | "paused" | "canceled" | "failed" | "timedOut"
            )
            || (self.state == "completed" && self.checked_bytes != self.size)
        {
            return Err(invalid());
        }
        validate_read_timeout(self.read_timeout_seconds)?;
        if (self.state == "timedOut") != self.timed_out_read.is_some() {
            return Err(invalid());
        }
        if let Some(read) = &self.timed_out_read {
            let first = (self.offset + self.checked_bytes) / self.sector_size;
            let end = first + (self.size - self.checked_bytes).min(BLOCK) / self.sector_size;
            if self.read_timeout_seconds.is_none()
                || read.sector_count == 0
                || read.start_sector < first
                || read
                    .start_sector
                    .checked_add(read.sector_count)
                    .is_none_or(|last| last > end)
            {
                return Err(invalid());
            }
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
                || !media_error(entry.code)
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
        if let Some(refinement) = &self.refinement {
            refinement.validate(self)?;
        }
        Ok(())
    }
    pub fn new(range: Range, refine_failures: bool) -> Self {
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
            refinement: refine_failures.then(Refinement::default),
            read_timeout_seconds: None,
            timed_out_read: None,
        }
    }

    fn stop(&mut self, error: ReadStop, refining: bool) {
        match error {
            ReadStop::Canceled => {
                self.state = "canceled".into();
                self.message = if refining {
                    "检测已取消。正在细查的块未计入进度。"
                } else {
                    "检测已取消；未检测范围不计入结果。"
                }
                .into();
            }
            ReadStop::Failed(message) => {
                self.state = "failed".into();
                self.message = message;
            }
            ReadStop::TimedOut { offset, bytes } if self.read_timeout_seconds.is_some() => {
                self.state = "timedOut".into();
                self.timed_out_read = Some(TimedOutRead {
                    start_sector: offset / self.sector_size,
                    sector_count: bytes as u64 / self.sector_size,
                });
                self.message = "单次读取超时。当前块尚未完成，可以调整超时后继续。".into();
            }
            ReadStop::TimedOut { .. } => {
                self.state = "failed".into();
                self.message = "设备访问超时，检测已停止。".into();
            }
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

pub(crate) fn media_error(code: i32) -> bool {
    matches!(code, 23 | 27 | 1117)
}

#[derive(Clone, Copy)]
pub struct TransferLimit(usize);

impl TransferLimit {
    pub fn from_adapter(bytes: u32, pages: u32, mask: u32, page_size: u32) -> Result<Self, String> {
        let alignment = BUFFER_ALIGNMENT as u64;
        let page = u64::from(page_size);
        let limit = BLOCK.min(u64::from(bytes)).min(u64::from(pages) * page);
        // Every chunk starts at the required buffer alignment and on a logical
        // sector boundary. Reject a descriptor we cannot honor with this buffer.
        if page == 0 || alignment % page != 0 || u64::from(mask) >= alignment || limit < alignment {
            return Err("无法确认坏道检测的单次传输上限。".into());
        }
        Ok(Self((limit / alignment * alignment) as usize))
    }
}

pub enum TransferError {
    #[cfg(test)]
    Position(io::Error),
    Io(io::Error),
    Canceled,
    TimedOut,
}

pub trait ScanSource {
    fn read_at(
        &mut self,
        offset: u64,
        buffer: &mut [u8],
        cancel: &AtomicBool,
    ) -> Result<usize, TransferError>;
}

#[cfg(test)]
impl<R: io::Read + io::Seek> ScanSource for R {
    fn read_at(
        &mut self,
        offset: u64,
        buffer: &mut [u8],
        _: &AtomicBool,
    ) -> Result<usize, TransferError> {
        self.seek(io::SeekFrom::Start(offset))
            .map_err(TransferError::Position)?;
        self.read(buffer).map_err(TransferError::Io)
    }
}

enum ReadStop {
    Canceled,
    Failed(String),
    TimedOut { offset: u64, bytes: usize },
}

/// A scan extent can contain multiple device transfers. Surface each native
/// error before issuing the next one, even if the driver would split a large read.
fn read_once(
    reader: &mut impl ScanSource,
    offset: u64,
    buffer: &mut [u8],
    limit: TransferLimit,
    cancel: &AtomicBool,
) -> Result<(Option<i32>, u64), ReadStop> {
    let started = Instant::now();
    let mut code = None;
    let mut offset = offset;
    for part in buffer.chunks_mut(limit.0) {
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadStop::Canceled);
        }
        match reader.read_at(offset, part, cancel) {
            Ok(n) if n == part.len() => offset += n as u64,
            Ok(n) => {
                return Err(ReadStop::Failed(format!(
                    "读取长度异常：请求 {} 字节、收到 {n} 字节，检测已停止。",
                    part.len()
                )))
            }
            Err(TransferError::Io(e)) if e.raw_os_error().is_some_and(media_error) => {
                code = e.raw_os_error();
                break;
            }
            Err(TransferError::Io(e)) => {
                return Err(ReadStop::Failed(format!("设备访问失败，检测已停止：{e}")))
            }
            #[cfg(test)]
            Err(TransferError::Position(e)) => {
                return Err(ReadStop::Failed(format!("定位失败，检测已停止：{e}")))
            }
            Err(TransferError::Canceled) => return Err(ReadStop::Canceled),
            Err(TransferError::TimedOut) => {
                return Err(ReadStop::TimedOut {
                    offset,
                    bytes: part.len(),
                })
            }
        }
    }
    Ok((
        code,
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    ))
}

struct FollowUp<'a, R> {
    reader: &'a mut R,
    buffer: &'a mut AlignedBlock,
    cancel: &'a AtomicBool,
    total_reads: &'a mut u64,
    block_reads: u64,
    sector: u64,
    transfer_limit: TransferLimit,
    ranges: Vec<RefinedRange>,
}

impl<R: ScanSource> FollowUp<'_, R> {
    fn split_failed(
        &mut self,
        start_sector: u64,
        sector_count: u64,
        code: i32,
    ) -> Result<(), ReadStop> {
        if sector_count == 1 {
            self.ranges.push(RefinedRange {
                start_sector,
                sector_count,
                code,
                resolution: FailureResolution::SectorReadFailed,
            });
            return Ok(());
        }
        if self.cancel.load(Ordering::Relaxed) {
            return Err(ReadStop::Canceled);
        }
        if *self.total_reads >= MAX_FOLLOW_UP_READS {
            self.ranges.push(RefinedRange {
                start_sector,
                sector_count,
                code,
                resolution: FailureResolution::Unresolved,
            });
            return Ok(());
        }

        let left = sector_count / 2;
        for (start_sector, sector_count) in [
            (start_sector, left),
            (start_sector + left, sector_count - left),
        ] {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(ReadStop::Canceled);
            }
            if *self.total_reads >= MAX_FOLLOW_UP_READS
                || self.block_reads >= MAX_FOLLOW_UP_PER_BLOCK
            {
                self.ranges.push(RefinedRange {
                    start_sector,
                    sector_count,
                    code,
                    resolution: FailureResolution::Unresolved,
                });
                continue;
            }
            *self.total_reads += 1;
            self.block_reads += 1;
            let (code, _) = read_once(
                self.reader,
                start_sector * self.sector,
                &mut self.buffer.0[..(sector_count * self.sector) as usize],
                self.transfer_limit,
                self.cancel,
            )?;
            if let Some(error) = code {
                self.split_failed(start_sector, sector_count, error)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub fn scan<R: ScanSource>(
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
    continue_scan(
        reader,
        Report::new(range, false),
        TransferLimit(BLOCK as usize),
        cancel,
        publish,
    )
}
pub fn continue_scan<R: ScanSource>(
    reader: &mut R,
    mut report: Report,
    transfer_limit: TransferLimit,
    cancel: &AtomicBool,
    mut publish: impl FnMut(&Report),
) -> Result<Report, String> {
    report.validate()?;
    let range = report.range();
    let previous_ms = report.elapsed_ms;
    report.state = "running".into();
    report.message.clear();
    report.timed_out_read = None;
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
        let offset = range.offset + report.checked_bytes;
        let (code, ms) =
            match read_once(reader, offset, &mut buffer.0[..len], transfer_limit, cancel) {
                Ok(result) => result,
                Err(error) => {
                    report.stop(error, false);
                    break;
                }
            };
        if let (Some(error), Some(refinement)) = (code, &mut report.refinement) {
            let mut follow_up = FollowUp {
                reader,
                buffer: &mut buffer,
                cancel,
                total_reads: &mut refinement.reads,
                block_reads: 0,
                sector: range.sector,
                transfer_limit,
                ranges: Vec::new(),
            };
            let result =
                follow_up.split_failed(offset / range.sector, len as u64 / range.sector, error);
            let ranges = follow_up.ranges;
            match result {
                Ok(()) => refinement.record(ranges),
                Err(error) => {
                    report.stop(error, true);
                    break;
                }
            }
        }
        report.record(len as u64, ms, code);
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
        } else if report.refinement.is_some() {
            "所选范围已检测完成。初读错误、单扇区读取失败和尚未定位的范围分别记录。"
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
    use std::io::{self, Cursor, Read, Seek, SeekFrom};

    fn continue_scan(
        reader: &mut impl ScanSource,
        report: Report,
        cancel: &AtomicBool,
        publish: impl FnMut(&Report),
    ) -> Result<Report, String> {
        super::continue_scan(
            reader,
            report,
            TransferLimit(BLOCK as usize),
            cancel,
            publish,
        )
    }

    #[test]
    fn read_timeout_preserves_completed_blocks_and_never_counts_a_bad_block() {
        struct TimeoutAt(u64, Vec<u64>);
        impl ScanSource for TimeoutAt {
            fn read_at(
                &mut self,
                offset: u64,
                buffer: &mut [u8],
                _: &AtomicBool,
            ) -> Result<usize, TransferError> {
                self.1.push(offset);
                if offset == self.0 {
                    Err(TransferError::TimedOut)
                } else {
                    Ok(buffer.len())
                }
            }
        }
        let range = Range {
            offset: 8192,
            size: 2 * BLOCK,
            sector: 4096,
        };
        let failed_at = range.offset + BLOCK + BLOCK / 2;
        let mut device = TimeoutAt(failed_at, Vec::new());
        let mut initial = Report::new(range, true);
        initial.read_timeout_seconds = Some(3);
        let limit = TransferLimit((BLOCK / 2) as usize);
        let cancel = AtomicBool::new(false);
        let stopped = super::continue_scan(&mut device, initial, limit, &cancel, |_| {}).unwrap();
        stopped.validate().unwrap();
        assert_eq!(stopped.state, "timedOut");
        assert_eq!(stopped.checked_bytes, BLOCK);
        assert_eq!(stopped.failed_bytes, 0);
        let transfer = stopped.timed_out_read.as_ref().unwrap();
        assert_eq!(transfer.start_sector, failed_at / range.sector);
        assert_eq!(transfer.sector_count, BLOCK / 2 / range.sector);

        let mut resumed_device = TimeoutAt(u64::MAX, Vec::new());
        let completed =
            super::continue_scan(&mut resumed_device, stopped, limit, &cancel, |_| {}).unwrap();
        completed.validate().unwrap();
        assert_eq!(completed.state, "completed");
        assert!(completed.timed_out_read.is_none());
        assert_eq!(resumed_device.1, [range.offset + BLOCK, failed_at]);
    }

    #[test]
    fn read_timeout_during_refinement_discards_the_unfinished_block() {
        struct RefineTimeout(Vec<usize>);
        impl ScanSource for RefineTimeout {
            fn read_at(
                &mut self,
                _: u64,
                buffer: &mut [u8],
                _: &AtomicBool,
            ) -> Result<usize, TransferError> {
                self.0.push(buffer.len());
                if buffer.len() == BLOCK as usize {
                    Err(TransferError::Io(io::Error::from_raw_os_error(1117)))
                } else {
                    Err(TransferError::TimedOut)
                }
            }
        }
        let mut device = RefineTimeout(Vec::new());
        let mut report = Report::new(range(BLOCK), true);
        report.read_timeout_seconds = Some(1);
        let report = continue_scan(&mut device, report, &AtomicBool::new(false), |_| {}).unwrap();
        report.validate().unwrap();
        assert_eq!(report.state, "timedOut");
        assert_eq!(report.checked_bytes, 0);
        assert!(report.failures.is_empty());
        assert_eq!(report.refinement.unwrap().reads, 1);
        assert_eq!(device.0, [BLOCK as usize, BLOCK as usize / 2]);
    }
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
        let mut report = Report::new(
            Range {
                offset: 4096,
                size: BLOCK + 4096,
                sector: 4096,
            },
            false,
        );
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
                let n = out.len() - usize::from(self.short);
                self.pos += n as u64;
                Ok(n)
            }
        }
    }
    struct SectorFault<'a> {
        fake: Fake,
        sector: u64,
        bad: &'a [u64],
        interrupt_after: Option<(usize, &'a AtomicBool)>,
        disconnect_on: Option<usize>,
    }
    impl Seek for SectorFault<'_> {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.fake.seek(from)
        }
    }
    impl Read for SectorFault<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            assert_eq!(self.fake.pos % self.sector, 0);
            assert_eq!(out.len() as u64 % self.sector, 0);
            let start = self.fake.pos / self.sector;
            let end = start + out.len() as u64 / self.sector;
            let result = self.fake.read(out)?;
            let reads = self.fake.calls.len();
            if let Some((after, canceled)) = self.interrupt_after {
                if reads == after {
                    canceled.store(true, Ordering::Relaxed);
                }
            }
            if self.disconnect_on == Some(reads) {
                return Err(io::Error::from_raw_os_error(1167));
            }
            if self.bad.iter().any(|lba| (start..end).contains(lba)) {
                return Err(io::Error::from_raw_os_error(1117));
            }
            Ok(result)
        }
    }
    fn sector_fault<'a>(sector: u64, bad: &'a [u64]) -> SectorFault<'a> {
        SectorFault {
            fake: fake(None),
            sector,
            bad,
            interrupt_after: None,
            disconnect_on: None,
        }
    }
    #[test]
    fn adapter_limits_cap_transfers_without_changing_the_report_grid() {
        let limit = TransferLimit::from_adapter(BLOCK as u32, 128, 4095, 4096).unwrap();
        assert_eq!(limit.0, BLOCK as usize / 2);
        assert!(TransferLimit::from_adapter(0, 128, 0, 4096).is_err());
        assert!(TransferLimit::from_adapter(65536, 0, 0, 4096).is_err());
        assert!(TransferLimit::from_adapter(65536, 128, 8191, 4096).is_err());
        assert!(TransferLimit::from_adapter(65536, 128, 0, 8192).is_err());

        let range = Range {
            offset: 4096,
            size: BLOCK * 2 + 4096,
            sector: 4096,
        };
        let lbas = [1 + BLOCK / 4096 / 2 + 9];
        let mut reader = sector_fault(4096, &lbas);
        let result = super::continue_scan(
            &mut reader,
            Report::new(range, true),
            limit,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        result.validate().unwrap();
        assert_eq!(result.checked_bytes, range.size);
        assert_eq!(result.failed_bytes, BLOCK);
        assert_eq!(result.failures[0].start_sector, 1);
        assert_eq!(result.refinement.unwrap().ranges[0].start_sector, lbas[0]);
        assert_eq!(
            &reader.fake.calls[..2],
            &[
                (range.offset, limit.0),
                (range.offset + limit.0 as u64, limit.0)
            ]
        );
        assert!(reader.fake.calls.iter().all(|call| call.1 <= limit.0));
    }

    #[test]
    fn cancel_between_native_transfers_does_not_commit_an_incomplete_block() {
        let range = Range {
            offset: 4096,
            size: BLOCK,
            sector: 4096,
        };
        let cancel = AtomicBool::new(false);
        let limit = TransferLimit((BLOCK / 2) as usize);
        let mut reader = sector_fault(4096, &[]);
        reader.interrupt_after = Some((1, &cancel));
        let report = super::continue_scan(
            &mut reader,
            Report::new(range, true),
            limit,
            &cancel,
            |_| {},
        )
        .unwrap();
        report.validate().unwrap();
        assert_eq!(report.state, "canceled");
        assert_eq!(report.checked_bytes, 0);
        assert_eq!(reader.fake.calls, [(range.offset, limit.0)]);
    }
    #[test]
    fn refinement_locates_faults_in_both_sector_sizes_and_odd_tail() {
        for sector in [512, 4096] {
            let range = Range {
                offset: 3 * sector,
                size: BLOCK + 5 * sector,
                sector,
            };
            let lbas = [6, 3 + BLOCK / sector + 4];
            let mut reader = sector_fault(sector, &lbas);
            let report = continue_scan(
                &mut reader,
                Report::new(range, true),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            report.validate().unwrap();
            assert_eq!(report.state, "completed");
            assert_eq!(report.failed_bytes, range.size);
            let result = report.refinement.unwrap();
            assert_eq!(result.failed_sectors, 2);
            assert_eq!(result.unresolved_sectors, 0);
            assert_eq!(
                result
                    .ranges
                    .iter()
                    .map(|r| r.start_sector)
                    .collect::<Vec<_>>(),
                lbas
            );
            assert!(result.ranges.iter().all(|r| r.sector_count == 1
                && r.code == 1117
                && r.resolution == FailureResolution::SectorReadFailed));
            assert_eq!(reader.fake.calls.len() as u64, result.reads + 2);
            assert!(reader
                .fake
                .calls
                .iter()
                .all(|&(offset, len)| offset >= range.offset
                    && offset + len as u64 <= range.offset + range.size));
        }
    }
    #[test]
    fn exhausted_refinement_keeps_initial_scan_moving_and_counts_omissions() {
        let range = range(BLOCK * 1100);
        let mut reader = fake(Some(23));
        let report = continue_scan(
            &mut reader,
            Report::new(range, true),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        report.validate().unwrap();
        assert_eq!(report.state, "completed");
        assert_eq!(report.checked_bytes, range.size);
        let per_block_reads = reader
            .calls
            .iter()
            .skip(1)
            .take_while(|&&(_, len)| len < BLOCK as usize)
            .count();
        assert_eq!(per_block_reads as u64, MAX_FOLLOW_UP_PER_BLOCK);
        let result = report.refinement.unwrap();
        assert_eq!(result.reads, MAX_FOLLOW_UP_READS);
        assert_eq!(
            result.failed_sectors + result.unresolved_sectors,
            range.size / range.sector
        );
        assert!(result.unresolved_sectors > 0 && result.omitted_ranges > 0);
        assert_eq!(reader.calls.len() as u64, 1100 + MAX_FOLLOW_UP_READS);
    }
    #[test]
    fn pause_during_refinement_restarts_only_the_unfinished_block() {
        let range = Range {
            offset: 3 * 4096,
            size: BLOCK * 2,
            sector: 4096,
        };
        let lbas = [3 + BLOCK / range.sector + 131];
        let cancel = AtomicBool::new(false);
        let mut reader = sector_fault(4096, &lbas);
        reader.interrupt_after = Some((3, &cancel));
        let paused = continue_scan(&mut reader, Report::new(range, true), &cancel, |_| {}).unwrap();
        paused.validate().unwrap();
        assert_eq!(paused.state, "canceled");
        assert_eq!(paused.checked_bytes, BLOCK);
        assert_eq!(paused.failed_bytes, 0);
        assert_eq!(paused.refinement.as_ref().unwrap().reads, 1);
        let mut resumed_reader = sector_fault(4096, &lbas);
        let completed =
            continue_scan(&mut resumed_reader, paused, &AtomicBool::new(false), |_| {}).unwrap();
        completed.validate().unwrap();
        let result = completed.refinement.unwrap();
        assert_eq!(completed.checked_bytes, range.size);
        assert_eq!(result.reads, 17);
        assert_eq!(result.ranges[0].start_sector, lbas[0]);
        assert!(resumed_reader
            .fake
            .calls
            .iter()
            .all(|&(offset, _)| offset >= range.offset + BLOCK));
    }
    #[test]
    fn disconnect_during_refinement_stops_before_recording_the_initial_failure() {
        let range = range(BLOCK);
        let mut reader = sector_fault(512, &[8]);
        reader.disconnect_on = Some(2);
        let report = continue_scan(
            &mut reader,
            Report::new(range, true),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        report.validate().unwrap();
        assert_eq!(report.state, "failed");
        assert_eq!(report.checked_bytes, 0);
        assert!(report.failures.is_empty());
        assert_eq!(reader.fake.calls.len(), 2);
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

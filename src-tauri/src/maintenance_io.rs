//! Bounded maintenance engines. Callers own identity, protection and native handles.
use crate::surface_scan::{Options, Range};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

pub const BLOCK: usize = 1024 * 1024;
#[repr(align(4096))]
struct Buffer([u8; BLOCK]);
fn buffer() -> Box<Buffer> {
    let mut b = Box::<Buffer>::new_uninit();
    // SAFETY: Buffer has the required alignment and its byte array accepts zero.
    unsafe {
        b.as_mut_ptr().write_bytes(0, 1);
        b.assume_init()
    }
}
fn valid(range: Range) -> Result<(), String> {
    Options {
        start_sector: 0,
        sector_count: range.size / range.sector.max(1),
    }
    .range(range.offset, range.size, range.sector)
    .map(|_| ())
}
fn check(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("任务已取消；已完成的写入不会回滚。".into())
    } else {
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Pattern {
    Bytes { hex: String },
    Random,
}
impl Pattern {
    pub fn bytes(&self) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::Random => Ok(None),
            Self::Bytes { hex } => {
                if hex.is_empty()
                    || hex.len() > 64
                    || hex.len() % 2 != 0
                    || !hex.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("填充内容须为 1–32 字节的十六进制字符，不带空格。".into());
                }
                (0..hex.len())
                    .step_by(2)
                    .map(|i| {
                        u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "填充内容无效".into())
                    })
                    .collect::<Result<Vec<_>, String>>()
                    .map(Some)
            }
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WipeOptions {
    pub pattern: Pattern,
    pub passes: u8,
}
impl WipeOptions {
    pub fn validate(&self, range: Range) -> Result<(), String> {
        valid(range)?;
        self.pattern.bytes()?;
        if !(1..=7).contains(&self.passes)
            || range
                .size
                .checked_mul(u64::from(self.passes) * 2)
                .is_none_or(|n| n > 9_007_199_254_740_991)
        {
            return Err("覆写次数须为 1–7，工作量须在可表示范围内。".into());
        }
        Ok(())
    }
}
#[derive(Clone, Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WipeProgress {
    pub pass: u8,
    pub phase: String,
    pub written_bytes: u64,
    pub readback_bytes: u64,
    pub verified_passes: u8,
    pub pass_sha256: Vec<String>,
    pub elapsed_ms: u64,
}
/// Exactly one write request per block; short/error writes stop, with no retries.
/// Every full pass is flushed and read back in full, including random patterns.
pub fn wipe<R: Read + Write + Seek>(
    io: &mut R,
    range: Range,
    options: &WipeOptions,
    cancel: &AtomicBool,
    progress: &mut WipeProgress,
    random: impl FnMut(&mut [u8]) -> Result<(), String>,
    sync: impl FnMut(&mut R) -> std::io::Result<()>,
    publish: impl FnMut(&WipeProgress),
) -> Result<(), String> {
    wipe_ranges(
        io,
        &[range],
        options,
        cancel,
        progress,
        random,
        sync,
        publish,
    )
}

/// Sparse ranges are derived in Rust from a verified allocation map. Validate
/// the entire list before any I/O; never widen a request across a retained gap.
pub(crate) fn wipe_ranges<R: Read + Write + Seek>(
    io: &mut R,
    ranges: &[Range],
    options: &WipeOptions,
    cancel: &AtomicBool,
    progress: &mut WipeProgress,
    mut random: impl FnMut(&mut [u8]) -> Result<(), String>,
    mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
    mut publish: impl FnMut(&WipeProgress),
) -> Result<(), String> {
    let sector = ranges.first().ok_or("没有可清除的空闲范围")?.sector;
    let mut end = 0;
    let mut total = 0u64;
    for range in ranges {
        options.validate(*range)?;
        if range.offset < end || range.sector != sector {
            return Err("清除范围重叠、乱序或扇区大小不一致".into());
        }
        end = range.offset.checked_add(range.size).ok_or("清除范围溢出")?;
        total = total.checked_add(range.size).ok_or("清除总量溢出")?;
    }
    options.validate(Range {
        offset: 0,
        size: total,
        sector,
    })?;
    *progress = WipeProgress::default();
    let bytes = options.pattern.bytes()?;
    let mut b = buffer();
    let began = Instant::now();
    let result = (|| {
        for pass in 1..=options.passes {
            progress.pass = pass;
            progress.phase = "覆写".into();
            let mut expected = Sha256::new();
            for range in ranges {
                let mut at = 0u64;
                while at < range.size {
                    check(cancel)?;
                    let len = (range.size - at).min(BLOCK as u64) as usize;
                    if let Some(bytes) = &bytes {
                        for (i, v) in b.0[..len].iter_mut().enumerate() {
                            *v = bytes[((at + i as u64) % bytes.len() as u64) as usize];
                        }
                    } else {
                        random(&mut b.0[..len])?;
                    }
                    io.seek(SeekFrom::Start(range.offset + at))
                        .map_err(|e| e.to_string())?;
                    let n = io
                        .write(&b.0[..len])
                        .map_err(|e| format!("写入失败，可能已部分改动且无法确定长度：{e}"))?;
                    progress.written_bytes += n as u64;
                    if n != len {
                        return Err(format!("短写入 {n}/{len} 字节，停止且不重试。"));
                    }
                    expected.update(&b.0[..len]);
                    at += len as u64;
                    progress.elapsed_ms = began.elapsed().as_millis() as u64;
                    publish(progress);
                }
            }
            sync(io).map_err(|e| format!("覆写后刷新失败：{e}"))?;
            progress.phase = "完整回读".into();
            let mut actual = Sha256::new();
            for range in ranges {
                let mut at = 0;
                while at < range.size {
                    check(cancel)?;
                    let len = (range.size - at).min(BLOCK as u64) as usize;
                    io.seek(SeekFrom::Start(range.offset + at))
                        .map_err(|e| e.to_string())?;
                    let n = io
                        .read(&mut b.0[..len])
                        .map_err(|e| format!("回读失败：{e}"))?;
                    progress.readback_bytes += n as u64;
                    if n != len {
                        return Err("回读长度不足，未验证本轮。".into());
                    }
                    actual.update(&b.0[..len]);
                    at += len as u64;
                    progress.elapsed_ms = began.elapsed().as_millis() as u64;
                    publish(progress);
                }
            }
            let hash = expected.finalize();
            if hash != actual.finalize() {
                return Err("完整回读 SHA-256 不一致，停止且不重试。".into());
            }
            progress.pass_sha256.push(format!("{hash:x}"));
            progress.verified_passes = pass;
        }
        Ok(())
    })();
    progress.elapsed_ms = began.elapsed().as_millis() as u64;
    publish(progress);
    result
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Sequential,
    Random,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkOptions {
    pub access: Access,
    pub block_size: u32,
    pub operations: u32,
    pub write: bool,
}
impl BenchmarkOptions {
    pub fn validate(&self, range: Range) -> Result<(), String> {
        valid(range)?;
        if !matches!(self.block_size, 4096 | 65536 | 1048576)
            || range.offset % 4096 != 0
            || range.size % 4096 != 0
            || range.size < u64::from(self.block_size)
            || !(1..=65536).contains(&self.operations)
        {
            return Err(
                "测速范围须按 4 KiB 对齐，块大小为 4 KiB、64 KiB 或 1 MiB，请求次数为 1–65536。"
                    .into(),
            );
        }
        Ok(())
    }
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
}
fn position(range: Range, opt: &BenchmarkOptions, index: u32, random: u64) -> u64 {
    let blocks = range.size / u64::from(opt.block_size);
    let n = match opt.access {
        Access::Sequential => u64::from(index) % blocks,
        Access::Random => random % blocks,
    };
    range.offset + n * u64::from(opt.block_size)
}
/// QD1: positioned, uncached I/O is supplied by the Windows adapter.
/// Write timings include flush; mandatory readback is counted separately.
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
    *progress = BenchmarkProgress::default();
    let mut b = buffer();
    let mut expected = buffer();
    let len = opt.block_size as usize;
    let began = Instant::now();
    let mut times = Vec::with_capacity(opt.operations as usize);
    let result = (|| {
        for index in 0..opt.operations {
            check(cancel)?;
            let mut random_at = [0u8; 8];
            if opt.access == Access::Random {
                random(&mut random_at)?;
            }
            let at = position(range, opt, index, u64::from_le_bytes(random_at));
            if opt.write {
                random(&mut expected.0[..len])?;
            }
            let started = Instant::now();
            io.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
            let n = if opt.write {
                io.write(&expected.0[..len])
            } else {
                io.read(&mut b.0[..len])
            }
            .map_err(|e| format!("测速 I/O 失败，写入可能已部分完成：{e}"))?;
            progress.transferred_bytes += n as u64;
            if n != len {
                return Err(format!("测速短 I/O {n}/{len}，不重试。"));
            }
            if opt.write {
                sync(io).map_err(|e| format!("测速写入刷新失败：{e}"))?;
            }
            let ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            times.push(ns.max(1));
            progress.io_nanoseconds = progress.io_nanoseconds.saturating_add(ns.max(1));
            progress.operations += 1;
            if opt.write {
                io.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
                let n = io.read(&mut b.0[..len]).map_err(|e| e.to_string())?;
                if n != len || Sha256::digest(&b.0[..len]) != Sha256::digest(&expected.0[..len]) {
                    return Err("测速写入 SHA-256 回读失败。".into());
                }
                progress.verified_bytes += n as u64;
            }
            progress.elapsed_ms = began.elapsed().as_millis() as u64;
            let seconds = progress.io_nanoseconds as f64 / 1e9;
            progress.mib_per_second =
                f64::from(progress.operations) * f64::from(opt.block_size) / 1048576.0 / seconds;
            progress.iops = f64::from(progress.operations) / seconds;
            publish(progress);
        }
        Ok(())
    })();
    times.sort_unstable();
    if !times.is_empty() {
        progress.min_microseconds = times[0] / 1000;
        progress.max_microseconds = times[times.len() - 1] / 1000;
        progress.p50_microseconds = times[(times.len() - 1) / 2] / 1000;
        progress.p95_microseconds = times[(times.len() * 95).div_ceil(100) - 1] / 1000;
        let seconds = progress.io_nanoseconds as f64 / 1e9;
        progress.mib_per_second =
            f64::from(progress.operations) * f64::from(opt.block_size) / 1048576.0 / seconds;
        progress.iops = f64::from(progress.operations) / seconds;
    }
    progress.elapsed_ms = began.elapsed().as_millis() as u64;
    publish(progress);
    result
}

#[cfg(test)]
#[path = "maintenance_io_tests.rs"]
mod tests;

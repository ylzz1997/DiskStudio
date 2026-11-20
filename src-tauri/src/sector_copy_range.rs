use super::Report;
use crate::surface_scan::{Direction, Range};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range as Span,
};

const WINDOW: u64 = 1024 * 1024;
// A Windows 4Kn NVMe guest reported success for a 1 MiB read spanning a
// confirmed media error that surfaced on reads of 512 KiB and smaller.
const READ_CHUNK: usize = 512 * 1024;
const MAX_FAILURES: usize = 1024;
const MAX_DEFERRED_SPANS: usize = 65_536;
const IO_DEVICE_ERROR: i32 = 1117;
const UNCORROBORATED_IO: &str = "源设备报告 I/O 错误，无法在所选范围确认仍可读取的扇区。";

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(super) enum Errors {
    #[default]
    Stop,
    Fill {
        skip_sectors: u64,
        marker: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reread: bool,
    },
    Defer {
        marker: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reread {
    None,
    Isolated,
    Large,
}

pub(super) enum Policy {
    Stop,
    Fill {
        skip: u64,
        pattern: Vec<u8>,
        reread: Reread,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Output {
    File,
    Device,
}

impl Errors {
    pub(super) fn for_filled_reread(&self) -> Result<Self, String> {
        match self {
            Self::Fill {
                skip_sectors,
                marker,
                ..
            } => Ok(Self::Fill {
                skip_sectors: *skip_sectors,
                marker: marker.clone(),
                reread: true,
            }),
            Self::Defer { .. } => Ok(self.clone()),
            Self::Stop => Err("复制记录没有可复读的填充扇区。".into()),
        }
    }

    pub(super) fn prepare(&self, sector: u64) -> Result<Policy, String> {
        let (skip, marker, reread) = match self {
            Self::Stop => return Ok(Policy::Stop),
            Self::Fill {
                skip_sectors,
                marker,
                reread,
            } => (
                *skip_sectors,
                marker,
                if *reread {
                    Reread::Isolated
                } else {
                    Reread::None
                },
            ),
            Self::Defer { marker } => (0, marker, Reread::Large),
        };
        let pattern = match marker.len() {
            0 => vec![0; sector as usize],
            1..=128 => marker.bytes().cycle().take(sector as usize).collect(),
            _ => return Err("填充文本最多 128 个 UTF-8 字节。".into()),
        };
        Ok(Policy::Fill {
            skip,
            pattern,
            reread,
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Failure {
    pub failed_lba: u64,
    pub first_filled_lba: u64,
    pub filled_sectors: u64,
    pub windows_code: i32,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Failures {
    pub filled_sectors: u64,
    pub unreadable_sectors: u64,
    pub omitted: u64,
    pub ranges: Vec<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reread: Option<RereadReport>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RereadReport {
    pub initial_read_failures: u64,
    pub queued_sectors: u64,
    pub recovered_sectors: u64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct ReadWitness {
    lba: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum Pass {
    Copy,
    Reread {
        initial_digest: [u8; 32],
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_filled: bool,
    },
}

/// Durable progress after complete windows in traversal order.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ResumeState {
    pass: Pass,
    done: u64,
    digest: [u8; 32],
    remaining_skip: u64,
    failures: Failures,
    deferred: Vec<Span<u64>>,
    last_readable: Option<ReadWitness>,
    needs_read_confirmation: bool,
}

impl ResumeState {
    pub(super) fn done(&self) -> u64 {
        self.done
    }

    /// Legacy JSON has no source identity. Compare every unfilled byte to
    /// independent source data under the caller's original source locks.
    pub(super) fn corroborate_source(
        &mut self,
        reader: &mut (impl Read + Seek),
        old_copy: &mut (impl Read + Seek),
        range: Range,
        direction: Direction,
        mut progress: impl FnMut(&str, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut actual = vec![0; WINDOW.min(range.size) as usize];
        let mut expected = vec![0; actual.len()];
        let mut witness = None;
        let mut done = 0;
        while done < range.size {
            let length = WINDOW.min(range.size - done);
            let offset = next_offset(direction, range.size, done, length);
            progress("核对旧报告的源扇区", done)?;
            for (filled, part) in window_parts(&self.deferred, done, length, direction) {
                if filled {
                    continue;
                }
                let at = offset + part.start as u64;
                let size = part.len();
                read_at(reader, range.offset + at, &mut actual[..size])
                    .map_err(|_| "原来未填充的源扇区现在无法读取。")?;
                read_at(old_copy, at, &mut expected[..size]).map_err(|e| e.to_string())?;
                if actual[..size] != expected[..size] {
                    return Err("源盘上未填充的字节与旧副本不一致。".into());
                }
                let index = match direction {
                    Direction::Forward => size - range.sector as usize,
                    Direction::Backward => 0,
                };
                witness = Some(ReadWitness {
                    lba: (at + index as u64) / range.sector,
                    digest: Sha256::digest(&actual[index..index + range.sector as usize]).into(),
                });
            }
            done += length;
        }
        progress("核对旧报告的源扇区", done)?;
        self.last_readable = Some(witness.ok_or("旧报告没有可核对来源的未填充扇区。")?);
        Ok(())
    }
}

pub(super) struct Checkpoint<'a> {
    pub state: Option<ResumeState>,
    pub commit: &'a mut dyn FnMut(&ResumeState) -> Result<(), String>,
}

struct CopySource<R> {
    reader: R,
    range: Range,
    direction: Direction,
    policy: Policy,
    remaining_skip: u64,
    failures: Failures,
    // Byte ranges in copy traversal order. Backward traversal starts at
    // the end of the selected extent, so these coordinates always increase.
    deferred: Vec<Span<u64>>,
    last_readable: Option<ReadWitness>,
    needs_read_confirmation: bool,
}

struct Position {
    pass: Pass,
    done: u64,
    hash: Sha256,
    canonical: Sha256,
}

fn read_at(source: &mut (impl Read + Seek), at: u64, bytes: &mut [u8]) -> io::Result<()> {
    source.seek(SeekFrom::Start(at))?;
    read_exact(source, bytes)
}

fn read_exact(source: &mut impl Read, bytes: &mut [u8]) -> io::Result<()> {
    for chunk in bytes.chunks_mut(READ_CHUNK) {
        source.read_exact(chunk)?;
    }
    Ok(())
}

fn fillable_code(error: &io::Error) -> Option<i32> {
    error
        .raw_os_error()
        .filter(|code| matches!(*code, 23 | 27 | IO_DEVICE_ERROR))
}

fn next_offset(direction: Direction, total: u64, done: u64, length: u64) -> u64 {
    match direction {
        Direction::Forward => done,
        Direction::Backward => total - done - length,
    }
}

impl<R: Read + Seek> CopySource<R> {
    fn try_read(&mut self, lba: u64, buf: &mut [u8]) -> Result<io::Result<()>, String> {
        let sector = self.range.sector;
        let len = buf.len() as u64;
        let mut done = 0;
        while done < len {
            let count = (READ_CHUNK as u64).min(len - done);
            let at = next_offset(self.direction, len, done, count);
            self.reader
                .seek(SeekFrom::Start(self.range.offset + lba * sector + at))
                .map_err(|e| e.to_string())?;
            if let Err(error) = self
                .reader
                .read_exact(&mut buf[at as usize..(at + count) as usize])
            {
                return Ok(Err(error));
            }
            done += count;
        }
        let index = match self.direction {
            Direction::Forward => len - sector,
            Direction::Backward => 0,
        };
        self.last_readable = Some(ReadWitness {
            lba: lba + index / sector,
            digest: Sha256::digest(&buf[index as usize..(index + sector) as usize]).into(),
        });
        self.needs_read_confirmation = false;
        Ok(Ok(()))
    }

    fn fill(&self, bytes: &mut [u8]) {
        let Policy::Fill { pattern, .. } = &self.policy else {
            unreachable!("Stop policy never fills sectors");
        };
        for sector in bytes.chunks_exact_mut(pattern.len()) {
            sector.copy_from_slice(pattern);
        }
    }

    fn record_failure(&mut self, lba: u64, first: u64, len: u64, code: i32) {
        let base = self.range.offset / self.range.sector;
        self.failures.filled_sectors += len;
        self.failures.unreadable_sectors += 1;
        if self.failures.ranges.len() < MAX_FAILURES {
            self.failures.ranges.push(Failure {
                failed_lba: base + lba,
                first_filled_lba: base + first,
                filled_sectors: len,
                windows_code: code,
            });
        } else {
            self.failures.omitted += 1;
        }
    }

    fn defer(&mut self, first: u64, len: u64) -> Result<(), String> {
        let sector = self.range.sector;
        let start = next_offset(
            self.direction,
            self.range.size,
            first * sector,
            len * sector,
        );
        let end = start + len * sector;
        if let Some(tail) = self.deferred.last_mut().filter(|last| last.end == start) {
            tail.end = end;
        } else {
            if self.deferred.len() == MAX_DEFERRED_SPANS {
                return Err("延后重读范围过多，请缩小复制范围。".into());
            }
            self.deferred.push(start..end);
        }
        let report = self.failures.reread.as_mut().expect("reread policy");
        report.initial_read_failures += 1;
        report.queued_sectors += len;
        Ok(())
    }

    fn confirm_read(&mut self) -> Result<(), String> {
        if self.needs_read_confirmation {
            let witness = self.last_readable.ok_or(UNCORROBORATED_IO)?;
            let mut sector = [0; 4096];
            read_at(
                &mut self.reader,
                self.range.offset + witness.lba * self.range.sector,
                &mut sector[..self.range.sector as usize],
            )
            .map_err(|_| UNCORROBORATED_IO)?;
            self.needs_read_confirmation = false;
        }
        Ok(())
    }

    fn read_window(
        &mut self,
        mut lba: u64,
        mut buf: &mut [u8],
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if buf.is_empty() {
            return Ok(());
        }
        checkpoint()?;
        let sector = self.range.sector;
        let sectors = buf.len() as u64 / sector;
        if self.remaining_skip > 0 {
            let skipped = sectors.min(self.remaining_skip);
            let length = (skipped * sector) as usize;
            let (fill, read) = match self.direction {
                Direction::Forward => {
                    lba += skipped;
                    buf.split_at_mut(length)
                }
                Direction::Backward => {
                    let (read, fill) = buf.split_at_mut(buf.len() - length);
                    (fill, read)
                }
            };
            self.fill(fill);
            self.remaining_skip -= skipped;
            buf = read;
            if buf.is_empty() {
                return Ok(());
            }
        }

        let Err(error) = self.try_read(lba, buf)? else {
            return Ok(());
        };
        let (Policy::Fill { skip, reread, .. }, Some(code)) = (&self.policy, fillable_code(&error))
        else {
            return Err(format!("读取所选范围失败：{error}"));
        };

        if buf.len() == sector as usize || *reread == Reread::Large {
            self.needs_read_confirmation |= code == IO_DEVICE_ERROR;
            let total = self.range.size / sector;
            let failed = buf.len() as u64 / sector;
            let available = match self.direction {
                Direction::Forward => total - lba - failed,
                Direction::Backward => lba,
            };
            let skip = if *reread == Reread::Large {
                total.div_ceil(8)
            } else {
                *skip
            };
            self.remaining_skip = skip.min(available);
            let first = match self.direction {
                Direction::Forward => lba,
                Direction::Backward => lba - self.remaining_skip,
            };
            let len = failed + self.remaining_skip;
            if *reread == Reread::None {
                self.record_failure(lba, first, len, code);
            } else {
                self.defer(first, len)?;
            }
            self.fill(buf);
            return Ok(());
        }

        let total = buf.len() as u64;
        let subread = if total > sector * 128 {
            sector * 128
        } else {
            sector
        };
        let mut done = 0;
        while done < total {
            let length = subread.min(total - done);
            let offset = next_offset(self.direction, total, done, length);
            self.read_window(
                lba + offset / sector,
                &mut buf[offset as usize..(offset + length) as usize],
                checkpoint,
            )?;
            done += length;
        }
        Ok(())
    }

    fn retry_chunk(
        &mut self,
        lba: u64,
        bytes: &mut [u8],
        direction: Direction,
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        checkpoint()?;
        let sector = self.range.sector;
        let Err(error) = self.try_read(lba, bytes)? else {
            return Ok(());
        };
        let code = fillable_code(&error).ok_or_else(|| format!("读取所选范围失败：{error}"))?;
        let len = bytes.len() as u64;
        if len == sector {
            self.needs_read_confirmation |= code == IO_DEVICE_ERROR;
            self.record_failure(lba, lba, 1, code);
            self.fill(bytes);
            return Ok(());
        }
        // After a failed batch, approach its children from the other end.
        let direction = match direction {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        };
        let step = if len > sector * 8 { sector * 8 } else { sector };
        let mut done = 0;
        while done < len {
            let count = step.min(len - done);
            let at = next_offset(direction, len, done, count);
            self.retry_chunk(
                lba + at / sector,
                &mut bytes[at as usize..(at + count) as usize],
                direction,
                checkpoint,
            )?;
            done += count;
        }
        Ok(())
    }
}

fn window_parts(
    deferred: &[Span<u64>],
    from: u64,
    len: u64,
    direction: Direction,
) -> Vec<(bool, Span<usize>)> {
    let mut parts = Vec::new();
    let mut cursor = from;
    let end = from + len;
    let first = deferred.partition_point(|span| span.end <= from);
    let mut add = |pending, start, finish| {
        if start < finish {
            let count = finish - start;
            let index = next_offset(direction, len, start - from, count);
            parts.push((pending, index as usize..(index + count) as usize));
        }
    };
    for span in &deferred[first..] {
        if span.start >= end {
            break;
        }
        add(false, cursor, span.start);
        add(true, cursor.max(span.start), span.end.min(end));
        cursor = span.end.min(end);
    }
    add(false, cursor, end);
    parts
}

fn write_at(
    output: &mut (impl Seek + Write),
    output_kind: Output,
    at: u64,
    bytes: &[u8],
) -> Result<(), String> {
    output
        .seek(SeekFrom::Start(at))
        .map_err(|e| e.to_string())?;
    match output_kind {
        Output::File => output.write_all(bytes).map_err(|e| e.to_string()),
        Output::Device => {
            if output.write(bytes).map_err(|e| e.to_string())? != bytes.len() {
                return Err("目标设备发生短写入。".into());
            }
            Ok(())
        }
    }
}

fn write_changed(
    output: &mut (impl Seek + Write),
    output_kind: Output,
    at: u64,
    mut bytes: &[u8],
    mut prior: &[u8],
    sector: usize,
) -> Result<(), String> {
    let mut offset = at;
    while let Some(first) = bytes
        .chunks_exact(sector)
        .zip(prior.chunks_exact(sector))
        .position(|(now, before)| now != before)
    {
        let first = first * sector;
        let len = bytes[first..]
            .chunks_exact(sector)
            .zip(prior[first..].chunks_exact(sector))
            .take_while(|(now, before)| now != before)
            .count()
            * sector;
        let end = first + len;
        write_at(
            output,
            output_kind,
            offset + first as u64,
            &bytes[first..end],
        )?;
        bytes = &bytes[end..];
        prior = &prior[end..];
        offset += end as u64;
    }
    Ok(())
}

const CHECKPOINT_INTERVAL: u64 = 16 * WINDOW;

fn save_checkpoint<W: Write>(
    checkpoint: &mut Option<Checkpoint<'_>>,
    source: &CopySource<impl Read + Seek>,
    pass: Pass,
    done: u64,
    hash: &Sha256,
    output: &mut W,
    sync: &impl Fn(&mut W) -> io::Result<()>,
) -> Result<(), String> {
    if let Some(checkpoint) = checkpoint {
        output.flush().map_err(|e| e.to_string())?;
        sync(output).map_err(|e| e.to_string())?;
        (checkpoint.commit)(&ResumeState {
            pass,
            done,
            digest: hash.clone().finalize().into(),
            remaining_skip: source.remaining_skip,
            failures: source.failures.clone(),
            deferred: source.deferred.clone(),
            last_readable: source.last_readable,
            needs_read_confirmation: source.needs_read_confirmation,
        })?;
    }
    Ok(())
}

fn checkpoint_boundary(done: u64, total: u64) -> bool {
    done == total || done % CHECKPOINT_INTERVAL == 0
}

fn canonical_window(
    source: &CopySource<impl Read + Seek>,
    output: &mut (impl Read + Seek),
    done: u64,
    bytes: &mut [u8],
) -> Result<(), String> {
    let len = bytes.len() as u64;
    let at = next_offset(source.direction, source.range.size, done, len);
    for (pending, part) in window_parts(&source.deferred, done, len, source.direction) {
        if pending {
            source.fill(&mut bytes[part]);
        } else {
            read_at(output, at + part.start as u64, &mut bytes[part]).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn restore_checkpoint(
    source: &mut CopySource<impl Read + Seek>,
    output: &mut (impl Read + Seek),
    state: ResumeState,
    buffer: &mut [u8],
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<Position, String> {
    let Range {
        offset,
        sector,
        size,
    } = source.range;
    let reread = source.failures.reread.is_some();
    let first = matches!(state.pass, Pass::Copy);
    if state.done > size
        || (state.done != size && state.done % WINDOW != 0)
        || (first && state.remaining_skip > (size - state.done) / sector)
        || state.deferred.len() > MAX_DEFERRED_SPANS
        || state.failures.ranges.len() > MAX_FAILURES
        || state.failures.reread.is_some() != reread
        || (!reread && (!state.deferred.is_empty() || !first))
        || state.deferred.iter().any(|span| {
            span.start >= span.end
                || span.end > size
                || span.start % sector != 0
                || span.end % sector != 0
        })
        || state
            .deferred
            .windows(2)
            .any(|spans| spans[0].end >= spans[1].start)
        || state
            .last_readable
            .is_some_and(|witness| witness.lba >= size / sector)
    {
        return Err("扇区复制断点与确认范围不一致。".into());
    }
    progress(0)?;
    if let Some(witness) = state.last_readable {
        let bytes = &mut buffer[..sector as usize];
        read_at(&mut source.reader, offset + witness.lba * sector, bytes).map_err(|e| {
            crate::native_locale::current().format(
                "无法复核断点记录中的源扇区：{error}",
                &[("error", &e.to_string())],
            )
        })?;
        if <[u8; 32]>::from(Sha256::digest(bytes)) != witness.digest {
            return Err("断点记录中的源扇区内容已变化。".into());
        }
    }
    source.remaining_skip = state.remaining_skip;
    source.failures = state.failures;
    source.deferred = state.deferred;
    source.last_readable = state.last_readable;
    source.needs_read_confirmation = state.needs_read_confirmation;
    let mut digest = Sha256::new();
    let mut initial = Sha256::new();
    let mut canonical = Sha256::new();
    let total = if first { state.done } else { size };
    let mut done = 0;
    while done < total {
        progress(done)?;
        let len = WINDOW.min(total - done);
        let bytes = &mut buffer[..len as usize];
        canonical_window(source, output, done, bytes)?;
        if first {
            digest.update(bytes);
        } else {
            initial.update(&*bytes);
            if done < state.done {
                canonical.update(&*bytes);
                let at = next_offset(source.direction, size, done, len);
                read_at(output, at, bytes).map_err(|e| e.to_string())?;
                digest.update(bytes);
            }
        }
        done += len;
    }
    progress(total)?;
    if <[u8; 32]>::from(digest.clone().finalize()) != state.digest
        || matches!(state.pass, Pass::Reread { initial_digest, .. }
            if <[u8; 32]>::from(initial.finalize()) != initial_digest)
    {
        return Err("断点记录与目标已写数据的 SHA-256 不一致。".into());
    }
    Ok(Position {
        pass: state.pass,
        done: state.done,
        hash: digest,
        canonical,
    })
}

fn retry_deferred<R: Read + Seek, W: Read + Write + Seek>(
    source: &mut CopySource<R>,
    output: &mut W,
    output_kind: Output,
    buffer: &mut [u8],
    expected_initial: [u8; 32],
    position: Position,
    progress: &mut impl FnMut(&str, u64) -> Result<(), String>,
    mut committed: impl FnMut(&mut W, &CopySource<R>, u64, &Sha256) -> Result<(), String>,
) -> Result<[u8; 32], String> {
    let Range { size, sector, .. } = source.range;
    let direction = source.direction;
    let Position {
        pass,
        mut done,
        hash: mut final_bytes,
        canonical: mut initial,
    } = position;
    let keep_filled = matches!(
        pass,
        Pass::Reread {
            keep_filled: true,
            ..
        }
    );
    let phase = if keep_filled {
        "复读已填充扇区"
    } else {
        "延后重读扇区"
    };
    let mut prior = keep_filled.then(|| vec![0; buffer.len()]);
    while done < size {
        progress(phase, done)?;
        let len = WINDOW.min(size - done);
        let at = next_offset(direction, size, done, len);
        let bytes = &mut buffer[..len as usize];
        let parts = window_parts(&source.deferred, done, len, direction);
        canonical_window(source, output, done, bytes)?;
        initial.update(&*bytes);

        for (_, part) in parts.iter().filter(|(pending, _)| *pending) {
            let mut consumed = 0;
            let step = sector as usize * 128;
            while consumed < part.len() {
                let count = step.min(part.len() - consumed);
                let index = part.start
                    + next_offset(direction, part.len() as u64, consumed as u64, count as u64)
                        as usize;
                source.retry_chunk(
                    (at + index as u64) / sector,
                    &mut bytes[index..index + count],
                    direction,
                    &mut || progress(phase, done),
                )?;
                consumed += count;
            }
        }
        if output_kind == Output::Device {
            source.confirm_read()?;
        }
        for (_, part) in parts.into_iter().filter(|(pending, _)| *pending) {
            progress(phase, done)?;
            let offset = at + part.start as u64;
            if let Some(prior) = &mut prior {
                let previous = &mut prior[..part.len()];
                read_at(output, offset, previous).map_err(|e| e.to_string())?;
                write_changed(
                    output,
                    output_kind,
                    offset,
                    &bytes[part],
                    previous,
                    sector as usize,
                )?;
            } else {
                write_at(output, output_kind, offset, &bytes[part])?;
            }
        }
        final_bytes.update(bytes);
        done += len;
        if checkpoint_boundary(done, size) {
            committed(output, source, done, &final_bytes)?;
        }
    }
    source.confirm_read()?;
    progress(phase, done)?;
    if <[u8; 32]>::from(initial.finalize()) != expected_initial {
        return Err("延后重读时校验首次写入的数据失败。".into());
    }
    let report = source.failures.reread.as_mut().expect("reread policy");
    report.recovered_sectors = report.queued_sectors - source.failures.filled_sectors;
    Ok(final_bytes.finalize().into())
}

fn readback_digest(
    output: &mut (impl Read + Seek),
    size: u64,
    direction: Direction,
    buffer: &mut [u8],
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<[u8; 32], String> {
    let mut hash = Sha256::new();
    let mut done = 0;
    while done < size {
        progress(done)?;
        let len = WINDOW.min(size - done);
        let at = next_offset(direction, size, done, len);
        let bytes = &mut buffer[..len as usize];
        read_at(output, at, bytes).map_err(|e| e.to_string())?;
        hash.update(bytes);
        done += len;
    }
    progress(done)?;
    Ok(hash.finalize().into())
}

pub(super) fn filled_spans(range: Range, report: &Report) -> Result<Vec<Span<u64>>, String> {
    let Range {
        offset,
        size,
        sector,
    } = range;
    if !matches!(sector, 512 | 4096)
        || offset % sector != 0
        || size == 0
        || size % sector != 0
        || offset.checked_add(size).is_none()
    {
        return Err("复制记录的填充区间不完整或与原范围不一致。".into());
    }
    let base = offset / sector;
    let count = size / sector;
    let failures = &report.failures;
    if failures.filled_sectors == 0 || failures.ranges.is_empty() {
        return Err("复制记录没有可复读的填充扇区。".into());
    }
    if report.start_lba != base
        || report.sector_size != sector
        || report.sector_count != count
        || failures.omitted != 0
        || failures.ranges.len() > MAX_FAILURES
        || failures.unreadable_sectors != failures.ranges.len() as u64
    {
        return Err("复制记录的填充区间不完整或与原范围不一致。".into());
    }
    let mut spans = Vec::with_capacity(failures.ranges.len());
    let mut filled = 0u64;
    for item in &failures.ranges {
        let first = item
            .first_filled_lba
            .checked_sub(base)
            .ok_or("复制记录的填充区间超出原范围。")?;
        let end = first
            .checked_add(item.filled_sectors)
            .filter(|&end| first < end && end <= count)
            .ok_or("复制记录的填充区间超出原范围。")?;
        if !matches!(item.windows_code, 23 | 27 | IO_DEVICE_ERROR)
            || !(base + first..base + end).contains(&item.failed_lba)
        {
            return Err("复制记录的填充区间不完整或与原范围不一致。".into());
        }
        let start = next_offset(
            report.direction,
            size,
            first * sector,
            (end - first) * sector,
        );
        spans.push(start..start + (end - first) * sector);
        filled = filled
            .checked_add(item.filled_sectors)
            .filter(|&filled| filled <= count)
            .ok_or("复制记录的填充区间不完整或与原范围不一致。")?;
    }
    spans.sort_unstable_by_key(|span| span.start);
    let mut merged: Vec<Span<u64>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(previous) if previous.end > span.start => {
                return Err("复制记录的填充区间存在重叠。".into());
            }
            Some(previous) if previous.end == span.start => previous.end = span.end,
            _ => merged.push(span),
        }
    }
    if filled != failures.filled_sectors {
        return Err("复制记录的填充区间不完整或与原范围不一致。".into());
    }
    Ok(merged)
}

pub(super) fn start_filled_reread(
    output: &mut (impl Read + Seek),
    range: Range,
    report: &Report,
    previous: Option<&ResumeState>,
    mut progress: impl FnMut(&str, u64) -> Result<(), String>,
) -> Result<ResumeState, String> {
    let spans = filled_spans(range, report)?;
    let Policy::Fill { pattern, .. } = report.errors.prepare(range.sector)? else {
        return Err("复制记录没有可复读的填充扇区。".into());
    };
    let mut buf = vec![0; WINDOW.min(range.size) as usize];
    let verify = |done| progress("复核原副本 SHA-256", done);
    if output.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != range.size {
        return Err("原副本长度与复制记录不一致。".into());
    }
    let forward = readback_digest(output, range.size, Direction::Forward, &mut buf, verify)?;
    if format_digest(forward) != report.sha256 {
        return Err("原副本的 SHA-256 与复制记录不一致。".into());
    }
    for span in &spans {
        let mut read = span.start;
        while read < span.end {
            progress("复核已填充扇区", read)?;
            let length = WINDOW.min(span.end - read);
            let offset = next_offset(report.direction, range.size, read, length);
            let bytes = &mut buf[..length as usize];
            read_at(output, offset, bytes).map_err(|e| e.to_string())?;
            if bytes
                .chunks_exact(pattern.len())
                .any(|sector| sector != pattern)
            {
                return Err("复制记录中的目标扇区与原填充内容不一致。".into());
            }
            read += length;
        }
    }
    let initial_digest = if report.direction == Direction::Forward {
        forward
    } else {
        readback_digest(output, range.size, report.direction, &mut buf, |done| {
            progress("复核原副本 SHA-256", done)
        })?
    };
    Ok(ResumeState {
        pass: Pass::Reread {
            initial_digest,
            keep_filled: true,
        },
        done: 0,
        digest: Sha256::digest([]).into(),
        remaining_skip: 0,
        last_readable: previous.and_then(|state| state.last_readable),
        needs_read_confirmation: false,
        failures: Failures {
            reread: Some(RereadReport {
                initial_read_failures: report.failures.unreadable_sectors,
                queued_sectors: report.failures.filled_sectors,
                recovered_sectors: 0,
            }),
            ..Failures::default()
        },
        deferred: spans,
    })
}

fn format_digest(digest: [u8; 32]) -> String {
    format!("{:x}", sha2::digest::Output::<Sha256>::from(digest))
}

#[cfg(test)]
pub(super) fn copy<R, W>(
    source: R,
    output: &mut W,
    range: Range,
    direction: Direction,
    policy: Policy,
    output_kind: Output,
    sync: impl Fn(&mut W) -> io::Result<()>,
    progress: impl FnMut(&str, u64) -> Result<(), String>,
) -> Result<(String, Failures), String>
where
    R: Read + Seek,
    W: Read + Write + Seek,
{
    copy_resumable(
        source,
        output,
        range,
        direction,
        policy,
        output_kind,
        sync,
        None,
        progress,
    )
}

pub(super) fn copy_resumable<R, W>(
    source: R,
    output: &mut W,
    range: Range,
    direction: Direction,
    policy: Policy,
    output_kind: Output,
    sync: impl Fn(&mut W) -> io::Result<()>,
    mut checkpoint: Option<Checkpoint<'_>>,
    mut progress: impl FnMut(&str, u64) -> Result<(), String>,
) -> Result<(String, Failures), String>
where
    R: Read + Seek,
    W: Read + Write + Seek,
{
    if !matches!(range.sector, 512 | 4096)
        || range.size == 0
        || range.offset % range.sector != 0
        || range.size % range.sector != 0
        || range.offset.checked_add(range.size).is_none()
    {
        return Err("请选择非空且按逻辑扇区对齐的检测范围。".into());
    }
    let reread = matches!(&policy, Policy::Fill { reread, .. } if *reread != Reread::None);
    let mut source = CopySource {
        reader: source,
        range,
        direction,
        policy,
        remaining_skip: 0,
        failures: Failures {
            reread: reread.then(RereadReport::default),
            ..Failures::default()
        },
        deferred: Vec::new(),
        last_readable: None,
        needs_read_confirmation: false,
    };
    let mut buffer = vec![0; WINDOW.min(range.size) as usize];
    let mut position = if let Some(state) = checkpoint
        .as_mut()
        .and_then(|checkpoint| checkpoint.state.take())
    {
        if output.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? > range.size {
            return Err("输出文件长度与确认范围不一致。".into());
        }
        restore_checkpoint(&mut source, output, state, &mut buffer, |done| {
            progress("复核扇区复制断点", done)
        })?
    } else {
        let hash = Sha256::new();
        progress("复制扇区", 0)?;
        save_checkpoint(
            &mut checkpoint,
            &source,
            Pass::Copy,
            0,
            &hash,
            output,
            &sync,
        )?;
        Position {
            pass: Pass::Copy,
            done: 0,
            hash,
            canonical: Sha256::new(),
        }
    };
    let (expected, keep_filled) = match position.pass {
        Pass::Copy => {
            while position.done < range.size {
                let done = position.done;
                let length = WINDOW.min(range.size - done);
                let offset = next_offset(direction, range.size, done, length);
                let bytes = &mut buffer[..length as usize];
                source.read_window(offset / range.sector, bytes, &mut || {
                    progress("复制扇区", done)
                })?;
                if !reread && output_kind == Output::Device {
                    source.confirm_read()?;
                }
                progress("复制扇区", done)?;
                if reread && !source.deferred.is_empty() {
                    for (_, part) in window_parts(&source.deferred, done, length, direction)
                        .into_iter()
                        .filter(|(pending, _)| !*pending)
                    {
                        progress("复制扇区", done)?;
                        write_at(
                            output,
                            output_kind,
                            offset + part.start as u64,
                            &bytes[part],
                        )?;
                    }
                } else {
                    write_at(output, output_kind, offset, bytes)?;
                }
                position.hash.update(bytes);
                position.done += length;
                if checkpoint_boundary(position.done, range.size) {
                    save_checkpoint(
                        &mut checkpoint,
                        &source,
                        Pass::Copy,
                        position.done,
                        &position.hash,
                        output,
                        &sync,
                    )?;
                }
            }
            let digest = position.hash.finalize_reset().into();
            position.done = 0;
            (digest, false)
        }
        Pass::Reread {
            initial_digest,
            keep_filled,
        } => (initial_digest, keep_filled),
    };
    let expected = if source.deferred.is_empty() {
        expected
    } else {
        let pass = Pass::Reread {
            initial_digest: expected,
            keep_filled,
        };
        position.pass = pass;
        retry_deferred(
            &mut source,
            output,
            output_kind,
            &mut buffer,
            expected,
            position,
            &mut progress,
            |output, source, done, hash| {
                save_checkpoint(&mut checkpoint, source, pass, done, hash, output, &sync)
            },
        )?
    };
    source.confirm_read()?;
    progress("复制扇区", range.size)?;
    output.flush().map_err(|e| e.to_string())?;
    sync(output).map_err(|e| e.to_string())?;
    if output.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != range.size {
        return Err("输出文件长度与确认范围不一致。".into());
    }
    let verify_phase = match output_kind {
        Output::File => "回读输出文件",
        Output::Device => "校验目标",
    };
    let verified = readback_digest(output, range.size, direction, &mut buffer, |done| {
        progress(verify_phase, done)
    })?;
    if verified != expected {
        return Err(match output_kind {
            Output::File => "导出文件回读 SHA-256 不一致。",
            Output::Device => "写后 SHA-256 校验失败",
        }
        .into());
    }
    let sha256 = if direction == Direction::Forward {
        verified
    } else {
        // Verification followed the physical read order. Report the digest
        // users obtain by hashing the published file in byte order.
        let phase = match output_kind {
            Output::File => "计算文件 SHA-256",
            Output::Device => "校验目标",
        };
        readback_digest(
            output,
            range.size,
            Direction::Forward,
            &mut buffer,
            |done| progress(phase, done),
        )?
    };
    Ok((format_digest(sha256), source.failures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Source {
        disk: Cursor<Vec<u8>>,
        fault_at: u64,
        os_error: i32,
        reads: Vec<(u64, usize)>,
    }

    impl Seek for Source {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.disk.seek(from)
        }
    }

    impl Read for Source {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let start = self.disk.position();
            assert!(out.len() <= READ_CHUNK);
            self.reads.push((start, out.len()));
            if (start..start + out.len() as u64).contains(&self.fault_at) {
                return Err(io::Error::from_raw_os_error(self.os_error));
            }
            self.disk.read(out)
        }
    }

    #[test]
    fn completed_copy_rereads_only_listed_fills_and_writes_only_changed_sectors() {
        struct Measured {
            bytes: Cursor<Vec<u8>>,
            writes: Vec<(u64, usize)>,
        }
        impl Read for Measured {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.bytes.read(out)
            }
        }
        impl Seek for Measured {
            fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
                self.bytes.seek(to)
            }
        }
        impl Write for Measured {
            fn write(&mut self, input: &[u8]) -> io::Result<usize> {
                self.writes.push((self.bytes.position(), input.len()));
                self.bytes.write(input)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let sector = 4096;
        let range = Range {
            offset: 0,
            size: sector * 8,
            sector,
        };
        let original = vec![0x87; range.size as usize];
        let source = || Source {
            disk: Cursor::new(original.clone()),
            fault_at: 4 * sector,
            os_error: IO_DEVICE_ERROR,
            reads: Vec::new(),
        };
        let errors = Errors::Fill {
            marker: "LOSS".into(),
            skip_sectors: 2,
            reread: false,
        };
        let mut output = Cursor::new(Vec::new());
        let mut previous = None;
        let (sha256, failures) = copy_resumable(
            source(),
            &mut output,
            range,
            Direction::Backward,
            errors.prepare(sector).unwrap(),
            Output::File,
            |_| Ok(()),
            Some(Checkpoint {
                state: None,
                commit: &mut |state| {
                    previous = Some(state.clone());
                    Ok(())
                },
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        let previous = previous.unwrap();
        let mut report = Report {
            version: 2,
            start_lba: 0,
            sector_size: sector,
            sector_count: 8,
            direction: Direction::Backward,
            errors,
            output_name: "test".into(),
            destination_start_lba: Some(0),
            sha256,
            failures,
            elapsed_ms: 0,
        };
        let mut changed = output.clone();
        changed.get_mut()[(2 * sector) as usize] ^= 1;
        assert!(
            start_filled_reread(&mut changed, range, &report, Some(&previous), |_, _| Ok(()))
                .is_err()
        );
        let mut forged = report.clone();
        forged.sha256 = format_digest(Sha256::digest(changed.get_ref()).into());
        assert!(
            start_filled_reread(&mut changed, range, &forged, Some(&previous), |_, _| Ok(()))
                .is_err()
        );
        report.failures.omitted = 1;
        assert!(filled_spans(range, &report).is_err());
        report.failures.omitted = 0;

        let retry =
            start_filled_reread(&mut output, range, &report, Some(&previous), |_, _| Ok(()))
                .unwrap();
        let mut measured = Measured {
            bytes: output,
            writes: Vec::new(),
        };
        let mut source = source();
        let (digest, report) = copy_resumable(
            &mut source,
            &mut measured,
            range,
            Direction::Backward,
            report
                .errors
                .for_filled_reread()
                .unwrap()
                .prepare(sector)
                .unwrap(),
            Output::Device,
            |_| Ok(()),
            Some(Checkpoint {
                state: Some(retry),
                commit: &mut |_| Ok(()),
            }),
            |_, _| Ok(()),
        )
        .unwrap();
        let mut expected = original;
        expected[(4 * sector) as usize..(5 * sector) as usize]
            .copy_from_slice(&b"LOSS".repeat(sector as usize / 4));
        assert_eq!(*measured.bytes.get_ref(), expected);
        assert_eq!(measured.writes, [(2 * sector, (2 * sector) as usize)]);
        assert!(source
            .reads
            .iter()
            .all(
                |(start, size)| (*start >= 2 * sector && *start + *size as u64 <= 5 * sector)
                    || (*start == 0 && *size == sector as usize)
            ));
        assert_eq!(report.filled_sectors, 1);
        assert_eq!(report.reread.unwrap().recovered_sectors, 2);
        assert_eq!(digest, format_digest(Sha256::digest(expected).into()));
    }

    #[test]
    fn legacy_source_binding_compares_all_unfilled_spans_and_skips_the_fault() {
        for direction in [Direction::Forward, Direction::Backward] {
            let range = Range {
                offset: 4096,
                size: 4 * 4096,
                sector: 4096,
            };
            let data: Vec<u8> = [1u8, 2, 3, 4, 5]
                .into_iter()
                .flat_map(|b| [b; 4096])
                .collect();
            let mut copy = data[4096..].to_vec();
            copy[4096..8192].fill(b'x');
            let mut report = Report {
                version: 1,
                start_lba: 1,
                sector_size: 4096,
                sector_count: 4,
                direction,
                errors: Errors::Fill {
                    skip_sectors: 0,
                    marker: "x".into(),
                    reread: false,
                },
                output_name: "original.bin".into(),
                destination_start_lba: None,
                sha256: format_digest(Sha256::digest(&copy).into()),
                elapsed_ms: 0,
                failures: Failures {
                    filled_sectors: 1,
                    unreadable_sectors: 1,
                    ranges: vec![Failure {
                        failed_lba: 2,
                        first_filled_lba: 2,
                        filled_sectors: 1,
                        windows_code: IO_DEVICE_ERROR,
                    }],
                    ..Failures::default()
                },
            };
            let mut old_copy = Cursor::new(copy);
            let mut state =
                start_filled_reread(&mut old_copy, range, &report, None, |_, _| Ok(())).unwrap();
            let mut faulty_source = Source {
                disk: Cursor::new(data.clone()),
                fault_at: 2 * 4096,
                os_error: IO_DEVICE_ERROR,
                reads: Vec::new(),
            };
            state
                .corroborate_source(
                    &mut faulty_source,
                    &mut old_copy,
                    range,
                    direction,
                    |_, _| Ok(()),
                )
                .unwrap();
            let reads = match direction {
                Direction::Forward => [(4096, 4096), (12288, 8192)],
                Direction::Backward => [(12288, 8192), (4096, 4096)],
            };
            assert_eq!(faulty_source.reads, reads);
            assert!(state.last_readable.is_some());
            let mut changed = data;
            changed[4 * 4096] ^= 1;
            let error = state
                .corroborate_source(
                    &mut Cursor::new(&changed),
                    &mut old_copy,
                    range,
                    direction,
                    |_, _| Ok(()),
                )
                .unwrap_err();
            assert_eq!(error, "源盘上未填充的字节与旧副本不一致。");

            report.failures.filled_sectors = 4;
            report.failures.ranges[0].first_filled_lba = 1;
            report.failures.ranges[0].filled_sectors = 4;
            old_copy.get_mut().fill(b'x');
            report.sha256 = format_digest(Sha256::digest(old_copy.get_ref()).into());
            let mut state =
                start_filled_reread(&mut old_copy, range, &report, None, |_, _| Ok(())).unwrap();
            let error = state
                .corroborate_source(
                    &mut faulty_source,
                    &mut old_copy,
                    range,
                    direction,
                    |_, _| Ok(()),
                )
                .unwrap_err();
            assert_eq!(error, "旧报告没有可核对来源的未填充扇区。");
        }
    }

    #[test]
    fn file_resume_revalidates_committed_output_and_remembers_backward_order() {
        let range = Range {
            offset: 4096,
            size: CHECKPOINT_INTERVAL + WINDOW,
            sector: 4096,
        };
        let bytes = (0..range.offset + range.size)
            .map(|n| (n / 4096 % 239) as u8)
            .collect::<Vec<_>>();
        let mut output = Cursor::new(Vec::new());
        let mut saved = None;
        let interrupted = copy_resumable(
            Cursor::new(&bytes),
            &mut output,
            range,
            Direction::Backward,
            Policy::Stop,
            Output::File,
            |_| Ok(()),
            Some(Checkpoint {
                state: None,
                commit: &mut |state| {
                    saved = Some(state.clone());
                    Ok(())
                },
            }),
            |phase, done| {
                if phase == "复制扇区" && done == CHECKPOINT_INTERVAL {
                    return Err("interrupted".into());
                }
                Ok(())
            },
        );
        assert!(matches!(interrupted, Err(error) if error == "interrupted"));
        let saved = saved.unwrap();
        assert_eq!(saved.done(), CHECKPOINT_INTERVAL);
        assert_eq!(
            &output.get_ref()[..WINDOW as usize],
            vec![0; WINDOW as usize]
        );
        let resume = |data, output: &mut Cursor<Vec<u8>>| {
            copy_resumable(
                Cursor::new(data),
                output,
                range,
                Direction::Backward,
                Policy::Stop,
                Output::File,
                |_| Ok(()),
                Some(Checkpoint {
                    state: Some(saved.clone()),
                    commit: &mut |_| Ok(()),
                }),
                |_, _| Ok(()),
            )
        };
        let mut changed = output.clone();
        changed.get_mut()[WINDOW as usize] ^= 1;
        let snapshot = changed.get_ref().clone();
        assert!(resume(&bytes, &mut changed).is_err());
        assert_eq!(*changed.get_ref(), snapshot);
        let mut changed_source = bytes.clone();
        changed_source[(range.offset + WINDOW) as usize] ^= 1;
        assert!(resume(&changed_source, &mut output).is_err());
        assert_eq!(
            resume(&bytes, &mut output).unwrap().0,
            format!("{:x}", Sha256::digest(&bytes[4096..]))
        );
        assert_eq!(*output.get_ref(), bytes[4096..]);
    }

    #[test]
    fn second_pass_resume_keeps_recovered_prefix_and_rechecks_healthy_bytes() {
        let range = Range {
            offset: 0,
            size: CHECKPOINT_INTERVAL + WINDOW,
            sector: 4096,
        };
        let bytes = vec![0x47; range.size as usize];
        let mut output = Cursor::new(vec![0x19; range.size as usize]);
        let mut saved = None;
        let policy = || {
            Errors::Fill {
                skip_sectors: 2,
                marker: "LOSS".into(),
                reread: true,
            }
            .prepare(range.sector)
            .unwrap()
        };
        let start = copy_resumable(
            Source {
                disk: Cursor::new(bytes.clone()),
                fault_at: 2 * range.sector,
                os_error: IO_DEVICE_ERROR,
                reads: Vec::new(),
            },
            &mut output,
            range,
            Direction::Forward,
            policy(),
            Output::Device,
            |_| Ok(()),
            Some(Checkpoint {
                state: None,
                commit: &mut |state| {
                    saved = Some(state.clone());
                    Ok(())
                },
            }),
            |phase, done| {
                if phase == "延后重读扇区" && done == CHECKPOINT_INTERVAL {
                    return Err("interrupted".into());
                }
                Ok(())
            },
        );
        assert!(matches!(start, Err(error) if error == "interrupted"));
        let state = saved.unwrap();
        assert_eq!(state.done(), CHECKPOINT_INTERVAL);
        assert!(matches!(state.pass, Pass::Reread { .. }));
        let resume = |output: &mut Cursor<Vec<u8>>| {
            copy_resumable(
                // The original fault is now clear. The committed part of the
                // second pass is verified and left as recorded.
                Cursor::new(&bytes),
                output,
                range,
                Direction::Forward,
                policy(),
                Output::Device,
                |_| Ok(()),
                Some(Checkpoint {
                    state: Some(state.clone()),
                    commit: &mut |_| Ok(()),
                }),
                |_, _| Ok(()),
            )
        };
        let mut bad = output.clone();
        bad.get_mut()[CHECKPOINT_INTERVAL as usize] ^= 1;
        assert!(resume(&mut bad).is_err());
        let mut bad = output.clone();
        bad.get_mut()[(range.sector * 2) as usize] ^= 1;
        assert!(resume(&mut bad).is_err());
        let prior = output.get_ref().clone();
        let (_, report) = resume(&mut output).unwrap();
        assert_eq!(report.filled_sectors, 1);
        assert_eq!(report.reread.unwrap().recovered_sectors, 2);
        assert_eq!(*output.get_ref(), prior);
    }

    #[test]
    fn deferred_ranges_cross_windows_in_both_directions_with_only_persistent_fills() {
        for direction in [Direction::Forward, Direction::Backward] {
            for large in [false, true] {
                let sector = 4096;
                let range = Range {
                    offset: sector,
                    size: 4 * WINDOW,
                    sector,
                };
                let relative_fault = match direction {
                    Direction::Forward => WINDOW * 2 - sector,
                    Direction::Backward => WINDOW,
                };
                let bytes: Vec<_> = (0..range.offset + range.size)
                    .map(|n| ((n + n / sector) % 251) as u8)
                    .collect();
                let mut expected = bytes[sector as usize..].to_vec();
                let start = relative_fault as usize;
                expected[start..start + sector as usize].fill(b'F');
                let source = Source {
                    disk: Cursor::new(bytes),
                    fault_at: range.offset + relative_fault,
                    os_error: IO_DEVICE_ERROR,
                    reads: Vec::new(),
                };
                let policy = if large {
                    Errors::Defer { marker: "F".into() }
                } else {
                    Errors::Fill {
                        skip_sectors: 2,
                        marker: "F".into(),
                        reread: true,
                    }
                };
                let kind = if large { Output::Device } else { Output::File };
                let mut output = Cursor::new(vec![0x9e; range.size as usize]);
                let (digest, report) = copy(
                    source,
                    &mut output,
                    range,
                    direction,
                    policy.prepare(sector).unwrap(),
                    kind,
                    |_| Ok(()),
                    |_, _| Ok(()),
                )
                .unwrap();
                assert_eq!(output.into_inner(), expected);
                assert_eq!(digest, format!("{:x}", Sha256::digest(expected)));
                assert_eq!(report.filled_sectors, 1);
                assert_eq!(report.unreadable_sectors, 1);
                assert_eq!(
                    report.ranges[0].first_filled_lba,
                    (range.offset + relative_fault) / sector
                );
                let reread = report.reread.unwrap();
                assert_eq!(reread.initial_read_failures, 1);
                assert_eq!(reread.queued_sectors, if large { 384 } else { 3 });
                assert_eq!(reread.recovered_sectors + 1, reread.queued_sectors);
            }
        }
    }

    #[test]
    fn delayed_device_write_requires_a_readable_source_and_cancellation_preserves_holes() {
        let range = Range {
            offset: 0,
            size: 512,
            sector: 512,
        };
        for cancel in [false, true] {
            let mut output = Cursor::new(vec![17; 512]);
            let source = Source {
                disk: Cursor::new(vec![8; 512]),
                fault_at: 0,
                os_error: IO_DEVICE_ERROR,
                reads: Vec::new(),
            };
            let policy = Errors::Defer {
                marker: String::new(),
            }
            .prepare(512)
            .unwrap();
            let result = copy(
                source,
                &mut output,
                range,
                Direction::Forward,
                policy,
                Output::Device,
                |_| panic!("failed jobs cannot finish"),
                |phase, _| {
                    if cancel && phase == "延后重读扇区" {
                        Err("cancel".into())
                    } else {
                        Ok(())
                    }
                },
            );
            let error = if cancel { "cancel" } else { UNCORROBORATED_IO };
            assert!(matches!(result, Err(message) if message == error));
            assert_eq!(output.into_inner(), vec![17; 512]);
        }
    }

    #[test]
    fn a_transient_read_error_can_be_fully_recovered() {
        struct FailOnce {
            source: Cursor<Vec<u8>>,
            failed: bool,
        }
        impl Seek for FailOnce {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.source.seek(from)
            }
        }
        impl Read for FailOnce {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if !std::mem::replace(&mut self.failed, true) {
                    return Err(io::Error::from_raw_os_error(IO_DEVICE_ERROR));
                }
                self.source.read(bytes)
            }
        }
        let bytes = vec![43; 2048];
        let source = FailOnce {
            source: Cursor::new(bytes.clone()),
            failed: false,
        };
        let mut output = Cursor::new(Vec::new());
        let range = Range {
            offset: 0,
            size: 2048,
            sector: 512,
        };
        let policy = Errors::Defer {
            marker: "TEMP".into(),
        }
        .prepare(512)
        .unwrap();
        let (_, report) = copy(
            source,
            &mut output,
            range,
            Direction::Backward,
            policy,
            Output::File,
            |_| Ok(()),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(output.into_inner(), bytes);
        assert_eq!(report.filled_sectors, 0);
        assert_eq!(report.unreadable_sectors, 0);
        assert_eq!(report.reread.unwrap().recovered_sectors, 4);
    }

    #[test]
    fn fills_and_skips_across_windows_in_each_direction_without_shifting_output() {
        for (direction, relative_bad, relative_first) in
            [(Direction::Forward, 255, 255), (Direction::Backward, 4, 2)]
        {
            let sector = 4096;
            let range = Range {
                offset: sector,
                size: WINDOW + 4 * sector,
                sector,
            };
            let bytes: Vec<u8> = (0..range.offset + range.size + sector)
                .map(|i| ((i * 11 + i / 4096) % 251) as u8)
                .collect();
            let mut expected =
                bytes[range.offset as usize..(range.offset + range.size) as usize].to_vec();
            let pattern: Vec<u8> = b"BAD"
                .iter()
                .copied()
                .cycle()
                .take(sector as usize)
                .collect();
            for lba in relative_first..relative_first + 3 {
                let at = (lba * sector) as usize;
                expected[at..at + sector as usize].copy_from_slice(&pattern);
            }
            let mut source = Source {
                disk: Cursor::new(bytes),
                fault_at: range.offset + relative_bad * sector,
                os_error: 23,
                reads: vec![],
            };
            let mut output = Cursor::new(Vec::new());
            let (sha, report) = copy(
                &mut source,
                &mut output,
                range,
                direction,
                Errors::Fill {
                    skip_sectors: 2,
                    marker: "BAD".into(),
                    reread: false,
                }
                .prepare(sector)
                .unwrap(),
                Output::File,
                |_| Ok(()),
                |_, _| Ok(()),
            )
            .unwrap();
            assert_eq!(output.into_inner(), expected);
            assert_eq!(sha, format!("{:x}", Sha256::digest(expected)));
            assert_eq!(report.unreadable_sectors, 1);
            assert_eq!(report.filled_sectors, 3);
            assert_eq!(report.omitted, 0);
            let [failure] = report.ranges.as_slice() else {
                panic!("Expected one failure")
            };
            assert_eq!(failure.failed_lba, 1 + relative_bad);
            assert_eq!(failure.first_filled_lba, 1 + relative_first);
            assert_eq!(failure.filled_sectors, 3);
            assert_eq!(failure.windows_code, 23);
            assert_eq!(
                source.reads[0],
                (
                    if direction == Direction::Backward {
                        5 * sector + READ_CHUNK as u64
                    } else {
                        sector
                    },
                    READ_CHUNK
                )
            );
            assert!(source.reads.iter().all(|&(at, len)| {
                at >= range.offset && at + len as u64 <= range.offset + range.size
            }));
        }
    }

    #[test]
    fn short_read_access_denied_and_device_disconnect_stop_even_with_fill_selected() {
        let range = Range {
            offset: 0,
            size: 1024,
            sector: 512,
        };
        for (length, code) in [(511, 0), (1024, 5), (1024, 1167)] {
            let mut source = Source {
                disk: Cursor::new(vec![0; length]),
                fault_at: if code == 0 { u64::MAX } else { 512 },
                os_error: code,
                reads: vec![],
            };
            let mut output = Cursor::new(Vec::new());
            let result = copy(
                &mut source,
                &mut output,
                range,
                Direction::Forward,
                Errors::Fill {
                    skip_sectors: 0,
                    marker: String::new(),
                    reread: false,
                }
                .prepare(range.sector)
                .unwrap(),
                Output::File,
                |_| Ok(()),
                |_, _| Ok(()),
            );
            assert!(result.is_err());
            assert!(output.into_inner().is_empty());
        }
    }

    #[test]
    fn generic_device_error_requires_a_confirmed_read_after_the_fault() {
        let range = Range {
            sector: 512,
            offset: 0,
            size: 1024,
        };
        let policy = || {
            Errors::Fill {
                skip_sectors: 0,
                marker: "x".into(),
                reread: false,
            }
            .prepare(512)
            .unwrap()
        };
        let mut source = Source {
            disk: Cursor::new(vec![42; 1024]),
            fault_at: 512,
            os_error: IO_DEVICE_ERROR,
            reads: vec![],
        };
        let mut output = Cursor::new(Vec::new());
        let (digest, report) = copy(
            &mut source,
            &mut output,
            range,
            Direction::Forward,
            policy(),
            Output::File,
            |_| Ok(()),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(source.reads, [(0, 1024), (0, 512), (512, 512), (0, 512)]);
        assert_eq!(&output.get_ref()[..512], &[42; 512]);
        assert_eq!(&output.get_ref()[512..], &[b'x'; 512]);
        assert_eq!(digest, format!("{:x}", Sha256::digest(output.into_inner())));
        assert_eq!(report.ranges[0].windows_code, IO_DEVICE_ERROR);

        let mut output = Cursor::new(Vec::new());
        let unreadable = Range {
            offset: 512,
            size: 512,
            ..range
        };
        let result = copy(
            &mut source,
            &mut output,
            unreadable,
            Direction::Forward,
            policy(),
            Output::Device,
            |_| Ok(()),
            |_, _| Ok(()),
        );
        assert!(matches!(result, Err(error) if error == UNCORROBORATED_IO));
        assert!(output.into_inner().is_empty());
    }

    #[test]
    fn device_readback_uses_the_exact_destination_offset_and_keeps_neighbors() {
        let sector = 4096;
        let range = Range {
            sector,
            offset: sector,
            size: 3 * sector,
        };
        let mut source = Source {
            disk: Cursor::new(vec![41; (4 * sector) as usize]),
            fault_at: 2 * sector,
            os_error: IO_DEVICE_ERROR,
            reads: vec![],
        };
        let mut target = Cursor::new(vec![19; (7 * sector) as usize]);
        let mut destination =
            crate::used_space::View::new(&mut target, 2 * sector, range.size).unwrap();
        let policy = Errors::Fill {
            skip_sectors: 1,
            marker: "N".into(),
            reread: false,
        }
        .prepare(sector)
        .unwrap();
        let (sha, report) = copy(
            &mut source,
            &mut destination,
            range,
            Direction::Backward,
            policy,
            Output::Device,
            |_| Ok(()),
            |_, _| Ok(()),
        )
        .unwrap();
        let bytes = target.into_inner();
        let sector = sector as usize;
        assert_eq!(&bytes[..2 * sector], vec![19; 2 * sector]);
        assert_eq!(&bytes[2 * sector..4 * sector], vec![b'N'; 2 * sector]);
        assert_eq!(&bytes[4 * sector..5 * sector], vec![41; sector]);
        assert_eq!(&bytes[5 * sector..], vec![19; 2 * sector]);
        assert_eq!(
            sha,
            format!("{:x}", Sha256::digest(&bytes[2 * sector..5 * sector]))
        );
        assert_eq!(report.ranges[0].failed_lba, 2);
        assert_eq!(report.ranges[0].first_filled_lba, 1);
        assert_eq!(report.filled_sectors, 2);
    }

    #[test]
    fn a_short_device_write_stops_without_retry_or_verifying() {
        struct ShortDevice {
            bytes: Cursor<Vec<u8>>,
            writes: usize,
        }
        impl Read for ShortDevice {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.bytes.read(bytes)
            }
        }
        impl Seek for ShortDevice {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.bytes.seek(from)
            }
        }
        impl Write for ShortDevice {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.write(&bytes[..512])
            }
            fn flush(&mut self) -> io::Result<()> {
                self.bytes.flush()
            }
        }
        let range = Range {
            sector: 512,
            offset: 0,
            size: 1024,
        };
        let mut device = ShortDevice {
            bytes: Cursor::new(vec![7; 1024]),
            writes: 0,
        };
        let result = copy(
            Cursor::new(vec![3; 1024]),
            &mut device,
            range,
            Direction::Forward,
            Policy::Stop,
            Output::Device,
            |_| panic!("short writes cannot reach verification"),
            |_, _| Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(device.writes, 1);
        assert_eq!(&device.bytes.get_ref()[..512], &[3; 512]);
        assert_eq!(&device.bytes.get_ref()[512..], &[7; 512]);
    }

    #[test]
    fn strict_reverse_surfaces_media_errors_and_checks_cancellation() {
        let range = Range {
            offset: 0,
            size: 4096,
            sector: 4096,
        };
        let mut source = Source {
            disk: Cursor::new(vec![0; 4096]),
            fault_at: 0,
            os_error: 27,
            reads: vec![],
        };
        let mut output = Cursor::new(Vec::new());
        assert!(copy(
            &mut source,
            &mut output,
            range,
            Direction::Backward,
            Policy::Stop,
            Output::File,
            |_| Ok(()),
            |_, _| Ok(()),
        )
        .is_err());
        assert_eq!(source.reads, [(0, 4096)]);
        assert!(output.into_inner().is_empty());

        let mut source = Cursor::new(vec![42; 4096]);
        let mut output = Cursor::new(Vec::new());
        let result = copy(
            &mut source,
            &mut output,
            range,
            Direction::Backward,
            Policy::Stop,
            Output::File,
            |_| Ok(()),
            |phase, _| {
                if phase == "回读输出文件" {
                    Err("cancel".into())
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(error) if error == "cancel"));
    }
}

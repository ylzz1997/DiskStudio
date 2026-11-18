use crate::{data_io, sector_query::Direction, surface_scan::Range};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};

const WINDOW: u64 = 1024 * 1024;
const MAX_FAILURES: usize = 1024;

#[derive(Default, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(super) enum Errors {
    #[default]
    Stop,
    Fill { skip_sectors: u64, marker: String },
}

pub(super) enum Policy {
    Stop,
    Fill { skip: u64, pattern: Vec<u8> },
}

impl Errors {
    pub(super) fn prepare(&self, sector: u64) -> Result<Policy, String> {
        match self {
            Self::Stop => Ok(Policy::Stop),
            Self::Fill { skip_sectors, marker } if (1..=128).contains(&marker.len()) => {
                Ok(Policy::Fill {
                    skip: *skip_sectors,
                    pattern: marker.bytes().cycle().take(sector as usize).collect(),
                })
            }
            Self::Fill { skip_sectors, marker } if marker.is_empty() => Ok(Policy::Fill {
                skip: *skip_sectors,
                pattern: vec![0; sector as usize],
            }),
            Self::Fill { .. } => Err("填充文本最多 128 个 UTF-8 字节。".into()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Failure {
    pub failed_lba: u64,
    pub first_filled_lba: u64,
    pub filled_sectors: u64,
    pub windows_code: i32,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Failures {
    pub filled_sectors: u64,
    pub unreadable_sectors: u64,
    pub omitted: u64,
    pub ranges: Vec<Failure>,
}

struct CopySource<R> {
    reader: R,
    range: Range,
    direction: Direction,
    policy: Policy,
    remaining_skip: u64,
    failures: Failures,
}

fn read_at(source: &mut (impl Read + Seek), at: u64, bytes: &mut [u8]) -> io::Result<()> {
    source.seek(SeekFrom::Start(at))?;
    source.read_exact(bytes)
}

fn media_code(error: &io::Error) -> Option<i32> {
    // Generic I/O errors may mean the device disappeared. Only explicit CRC
    // and sector-not-found errors can authorize substituting source bytes.
    error.raw_os_error().filter(|code| matches!(code, 23 | 27))
}

fn next_offset(direction: Direction, total: u64, done: u64, length: u64) -> u64 {
    match direction {
        Direction::Forward => done,
        Direction::Backward => total - done - length,
    }
}

impl<R: Read + Seek> CopySource<R> {
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
            let Policy::Fill { pattern, .. } = &self.policy else {
                unreachable!("Stop policy does not skip sectors");
            };
            for target in fill.chunks_exact_mut(sector as usize) {
                target.copy_from_slice(pattern);
            }
            self.remaining_skip -= skipped;
            buf = read;
            if buf.is_empty() {
                return Ok(());
            }
        }

        let read = read_at(&mut self.reader, self.range.offset + lba * sector, buf);
        let Err(error) = read else { return Ok(()) };
        let (Policy::Fill { skip, pattern }, Some(code)) = (&self.policy, media_code(&error)) else {
            return Err(format!("读取所选范围失败：{error}"));
        };

        if buf.len() == sector as usize {
            let total = self.range.size / sector;
            let available = match self.direction {
                Direction::Forward => total - lba - 1,
                Direction::Backward => lba,
            };
            self.remaining_skip = (*skip).min(available);
            let len = 1 + self.remaining_skip;
            let absolute_lba = self.range.offset / sector + lba;
            self.failures.filled_sectors += len;
            self.failures.unreadable_sectors += 1;
            if self.failures.ranges.len() < MAX_FAILURES {
                self.failures.ranges.push(Failure {
                    failed_lba: absolute_lba,
                    first_filled_lba: match self.direction {
                        Direction::Forward => absolute_lba,
                        Direction::Backward => absolute_lba - self.remaining_skip,
                    },
                    filled_sectors: len,
                    windows_code: code,
                });
            } else {
                self.failures.omitted += 1;
            }
            buf.copy_from_slice(pattern);
            return Ok(());
        }

        let total = buf.len() as u64;
        let subread = if total > sector * 128 { sector * 128 } else { sector };
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
}

pub(super) fn copy<R, W>(
    source: R,
    output: &mut W,
    range: Range,
    direction: Direction,
    policy: Policy,
    sync: impl FnOnce(&W) -> io::Result<()>,
    mut progress: impl FnMut(&str, u64) -> Result<(), String>,
) -> Result<(String, Failures), String>
where
    R: Read + Seek,
    W: Read + Write + Seek + Send,
{
    if !matches!(range.sector, 512 | 4096)
        || range.size == 0
        || range.offset % range.sector != 0
        || range.size % range.sector != 0
        || range.offset.checked_add(range.size).is_none()
    {
        return Err("请选择非空且按逻辑扇区对齐的检测范围。".into());
    }
    let mut source = CopySource {
        reader: source,
        range,
        direction,
        policy,
        remaining_skip: 0,
        failures: Failures::default(),
    };
    let mut copy_hash = Sha256::new();
    let mut buffer = vec![0; WINDOW.min(range.size) as usize];
    let mut done = 0;
    while done < range.size {
        let length = WINDOW.min(range.size - done);
        let offset = next_offset(direction, range.size, done, length);
        let bytes = &mut buffer[..length as usize];
        source.read_window(offset / range.sector, bytes, &mut || progress("复制扇区", done))?;
        progress("复制扇区", done)?;
        output.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        output.write_all(bytes).map_err(|e| e.to_string())?;
        copy_hash.update(bytes);
        done += length;
    }
    progress("复制扇区", done)?;
    output.flush().map_err(|e| e.to_string())?;
    sync(output).map_err(|e| e.to_string())?;
    if output.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != range.size {
        return Err("输出文件长度与确认范围不一致。".into());
    }
    let mut readback_hash = Sha256::new();
    done = 0;
    while done < range.size {
        progress("回读输出文件", done)?;
        let length = WINDOW.min(range.size - done);
        let offset = next_offset(direction, range.size, done, length);
        let bytes = &mut buffer[..length as usize];
        read_at(output, offset, bytes).map_err(|e| e.to_string())?;
        readback_hash.update(bytes);
        done += length;
    }
    progress("回读输出文件", done)?;
    let verified = readback_hash.finalize();
    if verified != copy_hash.finalize() {
        return Err("导出文件回读 SHA-256 不一致。".into());
    }
    let sha256 = if direction == Direction::Forward {
        format!("{verified:x}")
    } else {
        // Verification followed the physical read order. Report the digest
        // users obtain by hashing the published file in byte order.
        output.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        data_io::hash_exact(output, range.size, |done| progress("计算文件 SHA-256", done))?
    };
    Ok((sha256, source.failures))
}

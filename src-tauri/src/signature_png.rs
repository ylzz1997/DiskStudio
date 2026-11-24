//! Bounded PNG chunk and filtered image-data validation, per W3C PNG.
use super::{PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use miniz_oxide::{
    inflate::stream::{inflate, InflateState},
    DataFormat, MZError, MZFlush, MZStatus,
};
use std::io::{Read, Seek};

#[path = "signature_png_fragments.rs"]
pub(crate) mod fragments;

pub(super) const MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub(super) const IEND: &[u8; 12] = b"\0\0\0\0IEND\xaeB`\x82";
const MAX_FILTERED: u64 = 128 * 1024 * 1024;
const BLOCK: usize = 64 * 1024;

fn require(ok: bool) -> PResult<()> {
    ok.then_some(()).ok_or(ProbeError::Invalid)
}

#[derive(Clone, Copy)]
struct Image {
    width: u32,
    height: u32,
    color: u8,
    depth: u8,
    channels: u8,
    interlaced: bool,
}
fn image(data: [u8; 13]) -> PResult<Image> {
    let width = u32::from_be_bytes(data[..4].try_into().unwrap());
    let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
    let (depth, color) = (data[8], data[9]);
    let (channels, valid_depth) = match color {
        0 => (1, matches!(depth, 1 | 2 | 4 | 8 | 16)),
        2 => (3, matches!(depth, 8 | 16)),
        3 => (1, matches!(depth, 1 | 2 | 4 | 8)),
        4 => (2, matches!(depth, 8 | 16)),
        6 => (4, matches!(depth, 8 | 16)),
        _ => return Err(ProbeError::Invalid),
    };
    require(
        width > 0
            && width <= i32::MAX as u32
            && height > 0
            && height <= i32::MAX as u32
            && valid_depth
            && data[10..12] == [0, 0]
            && data[12] <= 1,
    )?;
    Ok(Image {
        width,
        height,
        color,
        depth,
        channels,
        interlaced: data[12] == 1,
    })
}

#[derive(Clone, Copy)]
struct Chunk {
    kind: [u8; 4],
    data: u64,
    length: u64,
    end: u64,
}
fn chunk_header(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    start: u64,
) -> PResult<Chunk> {
    let raw = probe.bytes::<8>(start)?;
    let size = u64::from(u32::from_be_bytes(raw[..4].try_into().unwrap()));
    let kind: [u8; 4] = raw[4..].try_into().unwrap();
    require(
        size <= i32::MAX as u64
            && kind.iter().all(u8::is_ascii_alphabetic)
            && kind[2].is_ascii_uppercase(),
    )?;
    let end = probe.bounded(start.checked_add(12 + size).ok_or(ProbeError::Invalid)?)?;
    Ok(Chunk {
        kind,
        data: start + 8,
        length: size,
        end,
    })
}

fn chunk_crc(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    budget: &mut Budget,
    chunk: Chunk,
    mut consume: impl FnMut(&[u8], &mut Budget) -> PResult<()>,
) -> PResult<()> {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&chunk.kind);
    budget
        .charge(12 + chunk.length)
        .map_err(ProbeError::Failed)?;
    let (mut offset, end) = (chunk.data, chunk.data + chunk.length);
    let mut bytes = [0; BLOCK];
    while offset < end {
        let n = (end - offset).min(BLOCK as u64) as usize;
        probe.read_at(offset, &mut bytes[..n])?;
        crc.update(&bytes[..n]);
        consume(&bytes[..n], budget)?;
        offset += n as u64;
    }
    require(crc.finalize().to_be_bytes() == probe.bytes::<4>(offset)?)
}
fn checked_chunk(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    budget: &mut Budget,
    start: u64,
) -> PResult<Chunk> {
    let chunk = chunk_header(probe, start)?;
    chunk_crc(probe, budget, chunk, |_, _| Ok(()))?;
    Ok(chunk)
}

fn animation(kind: [u8; 4]) -> bool {
    matches!(&kind, b"acTL" | b"fcTL" | b"fdAT")
}
fn prefix(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    budget: &mut Budget,
    static_only: bool,
) -> PResult<(Image, u64)> {
    require(probe.bytes::<8>(0)? == *MAGIC)?;
    let header = checked_chunk(probe, budget, 8)?;
    require(header.kind == *b"IHDR" && header.length == 13)?;
    let image = image(probe.bytes::<13>(header.data)?)?;
    let mut cursor = header.end;
    let mut palette = false;
    loop {
        let chunk = chunk_header(probe, cursor)?;
        match &chunk.kind {
            b"IDAT" => {
                require(image.color != 3 || palette)?;
                return Ok((image, cursor));
            }
            b"PLTE" => {
                require(
                    !palette
                        && !matches!(image.color, 0 | 4)
                        && chunk.length > 0
                        && chunk.length <= 768
                        && chunk.length % 3 == 0
                        && (image.color != 3 || chunk.length / 3 <= 1 << image.depth),
                )?;
                palette = true;
            }
            _ => require(
                chunk.kind[0].is_ascii_lowercase() && (!static_only || !animation(chunk.kind)),
            )?,
        }
        chunk_crc(probe, budget, chunk, |_, _| Ok(()))?;
        cursor = chunk.end;
    }
}

fn trailer(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    budget: &mut Budget,
    start: u64,
    static_only: bool,
) -> PResult<u64> {
    let mut cursor = start;
    loop {
        let chunk = chunk_header(probe, cursor)?;
        if chunk.kind == *b"IEND" {
            require(chunk.length == 0)?;
            chunk_crc(probe, budget, chunk, |_, _| Ok(()))?;
            return Ok(chunk.end);
        }
        require(chunk.kind[0].is_ascii_lowercase() && (!static_only || !animation(chunk.kind)))?;
        chunk_crc(probe, budget, chunk, |_, _| Ok(()))?;
        cursor = chunk.end;
    }
}

pub(super) fn inspect(
    probe: &mut Probe<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
    budget: &mut Budget,
) -> PResult<u64> {
    let (_, mut cursor) = prefix(probe, budget, false)?;
    let mut payload = false;
    loop {
        let chunk = chunk_header(probe, cursor)?;
        if &chunk.kind != b"IDAT" {
            require(payload)?;
            return trailer(probe, budget, cursor, false);
        }
        chunk_crc(probe, budget, chunk, |_, _| Ok(()))?;
        payload |= chunk.length != 0;
        cursor = chunk.end;
    }
}

#[derive(Clone)]
struct Rows {
    passes: Vec<(u64, u64)>,
    pass: usize,
    row: u64,
    left: u64,
}
impl Rows {
    fn new(image: Image) -> PResult<Self> {
        let w = u64::from(image.width);
        let h = u64::from(image.height);
        let bits = u64::from(image.depth) * u64::from(image.channels);
        let passes: &[(u64, u64, u64, u64)] = if image.interlaced {
            &[
                (0, 0, 8, 8),
                (4, 0, 8, 8),
                (0, 4, 4, 8),
                (2, 0, 4, 4),
                (0, 2, 2, 4),
                (1, 0, 2, 2),
                (0, 1, 1, 2),
            ]
        } else {
            &[(0, 0, 1, 1)]
        };
        let mut total = 0u64;
        let mut rows = Vec::new();
        for &(x, y, dx, dy) in passes {
            let width = w.saturating_sub(x).div_ceil(dx);
            let height = h.saturating_sub(y).div_ceil(dy);
            if width == 0 || height == 0 {
                continue;
            }
            let row_bytes = (width * bits).div_ceil(8);
            let bytes = (row_bytes + 1)
                .checked_mul(height)
                .ok_or(ProbeError::Invalid)?;
            total = total.checked_add(bytes).ok_or(ProbeError::Invalid)?;
            require(total <= MAX_FILTERED)?;
            rows.push((row_bytes, height));
        }
        Ok(Self {
            passes: rows,
            pass: 0,
            row: 0,
            left: 0,
        })
    }

    fn push(&mut self, mut bytes: &[u8]) -> PResult<()> {
        while !bytes.is_empty() {
            let &(stride, height) = self.passes.get(self.pass).ok_or(ProbeError::Invalid)?;
            if self.left == 0 {
                require(bytes[0] <= 4)?;
                bytes = &bytes[1..];
                self.left = stride;
            }
            let n = self.left.min(bytes.len() as u64);
            bytes = &bytes[n as usize..];
            self.left -= n;
            if self.left == 0 {
                self.row += 1;
                if self.row == height {
                    self.row = 0;
                    self.pass += 1;
                }
            }
        }
        Ok(())
    }
    fn done(&self) -> bool {
        self.pass == self.passes.len()
    }
}

#[derive(Clone)]
struct ImageStream {
    zlib: Box<InflateState>,
    rows: Rows,
    ended: bool,
}
impl ImageStream {
    fn new(info: Image) -> PResult<Self> {
        Ok(Self {
            zlib: InflateState::new_boxed(DataFormat::Zlib),
            rows: Rows::new(info)?,
            ended: false,
        })
    }

    fn feed(&mut self, mut bytes: &[u8], budget: &mut Budget) -> PResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        require(!self.ended)?;
        let mut out = [0u8; BLOCK];
        loop {
            let result = inflate(&mut self.zlib, bytes, &mut out, MZFlush::None);
            let status = match result.status {
                // An exact output-buffer boundary can trigger a final
                // input-free call after draining the pending dictionary.
                Err(MZError::Buf) if bytes.is_empty() && result.bytes_written == 0 => return Ok(()),
                result => result.map_err(|_| ProbeError::Invalid)?,
            };
            budget
                .charge(result.bytes_written as u64)
                .map_err(ProbeError::Failed)?;
            self.rows.push(&out[..result.bytes_written])?;
            bytes = &bytes[result.bytes_consumed..];
            if status == MZStatus::StreamEnd {
                require(bytes.is_empty() && self.rows.done())?;
                self.ended = true;
                return Ok(());
            }
            if bytes.is_empty() && result.bytes_written < out.len() {
                return Ok(());
            }
            require(result.bytes_consumed != 0 || result.bytes_written != 0)?;
        }
    }
}

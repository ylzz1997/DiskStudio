//! Bounded logical byte views over retained physical ranges.
use crate::used_space::Plan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
};

pub(crate) fn search_spans(
    range: Range<u64>,
    free: Option<&Plan>,
) -> impl Iterator<Item = Range<u64>> + '_ {
    free.into_iter()
        .flat_map(Plan::free_ranges)
        .map(move |span| span.start.max(range.start)..span.end.min(range.end))
        .chain(free.is_none().then_some(range))
        .filter(|span| span.start < span.end)
}

pub(crate) const SEARCH_BLOCK: usize = 1024 * 1024;

pub(crate) fn read_block(
    reader: &mut (impl Read + Seek),
    start: u64,
    end: u64,
    buffer: &mut [u8],
) -> Result<(usize, usize), String> {
    let step = (end - start).min(SEARCH_BLOCK as u64) as usize;
    let n = (end - start).min(buffer.len() as u64) as usize;
    reader
        .seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    reader
        .read_exact(&mut buffer[..n])
        .map_err(|e| e.to_string())?;
    Ok((step, n))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Part {
    pub start: u64,
    pub length: u64,
}

pub(crate) fn valid_parts(parts: &[Part], start: u64, size: u64, range: &Range<u64>) -> bool {
    if parts.first().is_none_or(|part| part.start != start) {
        return false;
    }

    let mut ranges = Vec::with_capacity(parts.len());
    let mut total = 0u64;
    for part in parts {
        let Some(end) = part.start.checked_add(part.length) else {
            return false;
        };
        if part.length == 0 || part.start < range.start || end > range.end {
            return false;
        }
        let Some(sum) = total.checked_add(part.length) else {
            return false;
        };
        total = sum;
        ranges.push((part.start, end));
    }
    ranges.sort_unstable();
    total == size && ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0)
}

pub(crate) struct Mapped<'a, R> {
    source: &'a mut R,
    parts: &'a [Part],
    ends: Vec<u64>,
    pos: u64,
}
impl<'a, R> Mapped<'a, R> {
    // The caller has already checked exact logical coverage and all extents.
    pub(crate) fn new(source: &'a mut R, parts: &'a [Part]) -> Self {
        let mut end = 0;
        let ends = parts
            .iter()
            .map(|part| {
                end += part.length;
                end
            })
            .collect();
        Self {
            source,
            parts,
            ends,
            pos: 0,
        }
    }
}
impl<R: Read + Seek> Read for Mapped<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let i = self.ends.partition_point(|&end| end <= self.pos);
        let Some(part) = self.parts.get(i) else {
            return Ok(0);
        };
        let within = self.pos - (self.ends[i] - part.length);
        let n = (self.ends[i] - self.pos).min(out.len() as u64) as usize;
        self.source.seek(SeekFrom::Start(part.start + within))?;
        let copied = self.source.read(&mut out[..n])?;
        self.pos += copied as u64;
        Ok(copied)
    }
}
impl<R: Read + Seek> Seek for Mapped<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.ends.last().unwrap_or(&0).checked_add_signed(delta),
        };
        self.pos =
            next.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fragment offset"))?;
        Ok(self.pos)
    }
}

pub(crate) fn copy_verified(
    mut mapped: Mapped<'_, impl Read + Seek>,
    size: u64,
    expected: &[u8; 32],
    out: &mut impl Write,
    take: u64,
    invalid: &str,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    mapped.rewind().map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut done = 0;
    while done < size {
        tick(done.min(take))?;
        let n = (size - done).min(buffer.len() as u64) as usize;
        mapped
            .read_exact(&mut buffer[..n])
            .map_err(|e| e.to_string())?;
        hash.update(&buffer[..n]);
        let write = (n as u64).min(take.saturating_sub(done)) as usize;
        out.write_all(&buffer[..write]).map_err(|e| e.to_string())?;
        done += n as u64;
    }
    let actual: [u8; 32] = hash.finalize().into();
    if &actual != expected {
        return Err(invalid.into());
    }
    Ok(take)
}

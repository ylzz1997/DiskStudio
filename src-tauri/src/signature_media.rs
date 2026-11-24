//! Bounded container checks for signature recovery. No codec is loaded.
use super::{Kind, PResult, Probe, ProbeError, MAX_FIELDS};
use crate::signature_zip::Budget;
use std::{
    collections::BTreeMap,
    io::{Read, Seek},
    ops::Range,
};

const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_TRACKS: usize = 64;
const MAX_SAMPLES: usize = 1_000_000;

pub(super) fn movie_kind(bytes: &[u8]) -> Option<Kind> {
    let header = bytes.get(..12)?;
    let size = be32(header, 0);
    if &header[4..8] != b"ftyp" || !(16..=4096).contains(&size) || size % 4 != 0 {
        return None;
    }
    match &header[8..12] {
        b"qt  " => Some(Kind::Mov),
        b"isom" | b"iso2" | b"iso3" | b"iso4" | b"iso5" | b"iso6" | b"iso7" | b"iso8" | b"iso9"
        | b"mp41" | b"mp42" | b"avc1" | b"M4V " | b"M4A " | b"MSNV" => Some(Kind::Mp4),
        _ => None,
    }
}

pub(super) fn ogg_start(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS\0") && matches!(bytes.get(5), Some(2 | 6))
}

#[derive(Default)]
struct PageStream {
    next: u32,
    continuing: bool,
    ended: bool,
}

const fn ogg_crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 == 0 {
                    0
                } else {
                    0x04c1_1db7
                };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

const OGG_CRC: [u32; 256] = ogg_crc_table();

#[derive(Clone, Copy)]
pub(super) struct OggPage {
    pub length: u64,
    pub serial: u32,
    pub sequence: u32,
    pub first: bool,
    pub last: bool,
    continued: bool,
    continues: bool,
}

pub(super) fn ogg_page<R, T>(
    p: &mut Probe<'_, R, T>,
    budget: &mut Budget,
    offset: u64,
    body: &mut Vec<u8>,
) -> PResult<OggPage>
where
    R: Read + Seek,
    T: FnMut() -> Result<(), String>,
{
    let mut header = p.bytes::<27>(offset)?;
    budget.charge(27).map_err(ProbeError::Failed)?;
    let flags = header[5];
    require(header.starts_with(b"OggS\0") && flags & !7 == 0)?;
    let first = flags & 2 != 0;
    let last = flags & 4 != 0;
    let continued = flags & 1 != 0;
    let segments = usize::from(header[26]);
    require(segments != 0 || last && !continued && !first)?;
    let mut lacing = [0; 255];
    p.read_at(offset + 27, &mut lacing[..segments])?;
    let bytes: usize = lacing[..segments].iter().map(|&n| usize::from(n)).sum();
    require(!first || bytes != 0)?;
    let length = (27 + segments + bytes) as u64;
    p.bounded(offset + length)?;
    budget.charge(length - 27).map_err(ProbeError::Failed)?;
    body.resize(bytes, 0);
    p.read_at(offset + (27 + segments) as u64, body)?;
    let checksum = le32(&header, 22);
    header[22..26].fill(0);
    let calculated = header
        .iter()
        .chain(&lacing[..segments])
        .chain(body.iter())
        .fold(0u32, |crc, &byte| {
            (crc << 8) ^ OGG_CRC[((crc >> 24) as u8 ^ byte) as usize]
        });
    let continues = segments != 0 && lacing[segments - 1] == 255;
    require(calculated == checksum && (!last || !continues))?;
    Ok(OggPage {
        length,
        serial: le32(&header, 14),
        sequence: le32(&header, 18),
        first,
        last,
        continued,
        continues,
    })
}

pub(super) struct OggOrder {
    streams: BTreeMap<u32, PageStream>,
    beginning: bool,
}
impl Default for OggOrder {
    fn default() -> Self {
        Self {
            streams: BTreeMap::new(),
            beginning: true,
        }
    }
}
impl OggOrder {
    pub fn push(&mut self, page: OggPage) -> PResult<bool> {
        if page.first {
            require(
                self.beginning
                    && self.streams.len() < MAX_TRACKS
                    && !self.streams.contains_key(&page.serial)
                    && page.sequence == 0
                    && !page.continued,
            )?;
            self.streams.insert(page.serial, PageStream::default());
        } else {
            self.beginning = false;
        }
        let state = self
            .streams
            .get_mut(&page.serial)
            .ok_or(ProbeError::Invalid)?;
        require(!state.ended && page.sequence == state.next && state.continuing == page.continued)?;
        state.continuing = page.continues;
        state.next = state.next.checked_add(1).ok_or(ProbeError::Invalid)?;
        state.ended = page.last;
        Ok(page.last && self.streams.values().all(|stream| stream.ended))
    }
}

pub(super) fn ogg<R, T>(p: &mut Probe<'_, R, T>, budget: &mut Budget) -> PResult<u64>
where
    R: Read + Seek,
    T: FnMut() -> Result<(), String>,
{
    let mut order = OggOrder::default();
    let mut offset = 0;
    let mut body = Vec::new();
    loop {
        let page = ogg_page(p, budget, offset, &mut body)?;
        offset += page.length;
        if order.push(page)? {
            return Ok(offset);
        }
    }
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn require(condition: bool) -> PResult<()> {
    condition.then_some(()).ok_or(ProbeError::Invalid)
}

fn atom_header(header: &[u8]) -> PResult<(u64, usize)> {
    require(header.len() >= 8)?;
    let short = be32(header, 0);
    let (size, prefix) = if short == 1 {
        let extended = header.get(8..16).ok_or(ProbeError::Invalid)?;
        (u64::from_be_bytes(extended.try_into().unwrap()), 16)
    } else {
        (u64::from(short), 8)
    };
    require(size >= prefix as u64)?;
    Ok((size, prefix))
}

struct Atom<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

fn children(mut bytes: &[u8]) -> PResult<Vec<Atom<'_>>> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        require(result.len() < MAX_FIELDS)?;
        let (size, prefix) = atom_header(bytes)?;
        let size = usize::try_from(size).map_err(|_| ProbeError::Invalid)?;
        let (child, rest) = bytes.split_at_checked(size).ok_or(ProbeError::Invalid)?;
        result.push(Atom {
            kind: child[4..8].try_into().unwrap(),
            body: &child[prefix..],
        });
        bytes = rest;
    }
    Ok(result)
}

fn one<'a, 'b>(atoms: &'b [Atom<'a>], kinds: &[[u8; 4]]) -> PResult<&'b Atom<'a>> {
    let mut found = atoms.iter().filter(|atom| kinds.contains(&atom.kind));
    let atom = found.next().ok_or(ProbeError::Invalid)?;
    require(found.next().is_none())?;
    Ok(atom)
}

fn full_count(bytes: &[u8], word: usize, maximum: usize) -> PResult<(usize, &[u8])> {
    require(bytes.len() >= 8 && bytes[..4] == [0; 4])?;
    let count = be32(bytes, 4) as usize;
    require(count <= maximum && (word == 0 || bytes.len() == 8 + count * word))?;
    Ok((count, &bytes[8..]))
}

enum SampleSize<'a> {
    Uniform(u32),
    Table { width: u8, bytes: &'a [u8] },
}

impl SampleSize<'_> {
    fn length(&self, indexes: Range<usize>) -> u64 {
        match self {
            Self::Uniform(n) => u64::from(*n) * indexes.len() as u64,
            Self::Table { width, bytes } => indexes
                .map(|i| {
                    u64::from(match width {
                        4 => u32::from(if i % 2 == 0 {
                            bytes[i / 2] >> 4
                        } else {
                            bytes[i / 2] & 15
                        }),
                        8 => u32::from(bytes[i]),
                        16 => u32::from(u16::from_be_bytes(
                            bytes[i * 2..i * 2 + 2].try_into().unwrap(),
                        )),
                        _ => be32(bytes, i * 4),
                    })
                })
                .sum(),
        }
    }
}

fn sample_sizes<'a>(atom: &Atom<'a>) -> PResult<(usize, SampleSize<'a>)> {
    let data = atom.body;
    require(data.len() >= 12 && data[..4] == [0; 4])?;
    let count = be32(data, 8) as usize;
    require((1..=MAX_SAMPLES).contains(&count))?;
    let bytes = &data[12..];
    if &atom.kind == b"stsz" {
        let size = be32(data, 4);
        if size != 0 {
            require(bytes.is_empty())?;
            return Ok((count, SampleSize::Uniform(size)));
        }
        require(bytes.len() == count * 4)?;
        Ok((count, SampleSize::Table { width: 32, bytes }))
    } else {
        let width = data[7];
        require(data[4..7] == [0; 3] && matches!(width, 4 | 8 | 16))?;
        require(bytes.len() == (count * usize::from(width)).div_ceil(8))?;
        Ok((count, SampleSize::Table { width, bytes }))
    }
}

fn local_descriptions(minf: &[Atom<'_>], tables: &[Atom<'_>]) -> PResult<usize> {
    let dinf = children(one(minf, &[*b"dinf"])?.body)?;
    let (references, bytes) = full_count(one(&dinf, &[*b"dref"])?.body, 0, MAX_FIELDS)?;
    let sources = children(bytes)?;
    require(references != 0 && references == sources.len())?;
    let (count, bytes) = full_count(one(tables, &[*b"stsd"])?.body, 0, MAX_FIELDS)?;
    let descriptions = children(bytes)?;
    require(count != 0 && count == descriptions.len())?;
    for desc in descriptions {
        require(
            desc.body.len() >= 8
                && desc.body[..6] == [0; 6]
                && !matches!(&desc.kind, b"encv" | b"enca" | b"enct"),
        )?;
        let reference = u16::from_be_bytes(desc.body[6..8].try_into().unwrap());
        let source = sources
            .get(usize::from(
                reference.checked_sub(1).ok_or(ProbeError::Invalid)?,
            ))
            .ok_or(ProbeError::Invalid)?;
        require(matches!(&source.kind, b"url " | b"alis") && source.body == [0, 0, 0, 1])?;
    }
    Ok(count)
}

fn track_chunks(track: &Atom<'_>, output: &mut Vec<Range<u64>>) -> PResult<bool> {
    let track = children(track.body)?;
    let media = children(one(&track, &[*b"mdia"])?.body)?;
    let handler = one(&media, &[*b"hdlr"])?
        .body
        .get(8..12)
        .ok_or(ProbeError::Invalid)?;
    let minf = children(one(&media, &[*b"minf"])?.body)?;
    let table = children(one(&minf, &[*b"stbl"])?.body)?;
    let descriptions = local_descriptions(&minf, &table)?;
    let (samples, sizes) = sample_sizes(one(&table, &[*b"stsz", *b"stz2"])?)?;
    let (_, times) = full_count(one(&table, &[*b"stts"])?.body, 8, MAX_FIELDS)?;
    let timed: u64 = times
        .chunks_exact(8)
        .map(|entry| u64::from(be32(entry, 0)))
        .sum();
    require(timed == samples as u64)?;
    let offset_table = one(&table, &[*b"stco", *b"co64"])?;
    let width = if &offset_table.kind == b"stco" { 4 } else { 8 };
    let (chunks, offsets) = full_count(offset_table.body, width, MAX_FIELDS - output.len())?;
    let (runs, mapping) = full_count(one(&table, &[*b"stsc"])?.body, 12, chunks)?;
    require(chunks > 0 && runs > 0 && be32(mapping, 0) == 1)?;
    let mut previous = 0;
    let mut sample = 0usize;
    for (index, entry) in mapping.chunks_exact(12).enumerate() {
        let first = be32(entry, 0) as usize;
        let per_chunk = be32(entry, 4) as usize;
        let description = be32(entry, 8) as usize;
        let end = if index + 1 == runs {
            chunks + 1
        } else {
            be32(mapping, (index + 1) * 12) as usize
        };
        require(
            first > previous
                && end > first
                && end <= chunks + 1
                && per_chunk > 0
                && (1..=descriptions).contains(&description),
        )?;
        for chunk in first - 1..end - 1 {
            let last = sample
                .checked_add(per_chunk)
                .filter(|end| *end <= samples)
                .ok_or(ProbeError::Invalid)?;
            let offset = if width == 4 {
                u64::from(be32(offsets, chunk * width))
            } else {
                u64::from_be_bytes(
                    offsets[chunk * width..(chunk + 1) * width]
                        .try_into()
                        .unwrap(),
                )
            };
            let length = sizes.length(sample..last);
            require(length != 0)?;
            output.push(offset..offset.checked_add(length).ok_or(ProbeError::Invalid)?);
            sample = last;
        }
        previous = first;
    }
    require(sample == samples)?;
    Ok(matches!(handler, b"vide" | b"soun"))
}

fn movie_chunks(body: &[u8]) -> PResult<Vec<Range<u64>>> {
    let children = children(body)?;
    let movie = one(&children, &[*b"mvhd"])?.body;
    let scale = match movie.first() {
        Some(0) if movie.len() >= 100 => be32(movie, 12),
        Some(1) if movie.len() >= 112 => be32(movie, 20),
        _ => return Err(ProbeError::Invalid),
    };
    require(scale != 0 && !children.iter().any(|atom| &atom.kind == b"mvex"))?;
    let tracks = children
        .iter()
        .filter(|atom| &atom.kind == b"trak")
        .collect::<Vec<_>>();
    require((1..=MAX_TRACKS).contains(&tracks.len()))?;
    let mut chunks = Vec::new();
    let mut has_media = false;
    for track in tracks {
        has_media |= track_chunks(track, &mut chunks)?;
    }
    chunks.sort_unstable_by_key(|chunk| chunk.start);
    require(has_media && chunks.windows(2).all(|pair| pair[0].end <= pair[1].start))?;
    Ok(chunks)
}

pub(super) fn movie<R, T>(
    p: &mut Probe<'_, R, T>,
    expected: Kind,
    budget: &mut Budget,
) -> PResult<u64>
where
    R: Read + Seek,
    T: FnMut() -> Result<(), String>,
{
    let brand = p.bytes::<12>(0)?;
    require(movie_kind(&brand) == Some(expected))?;
    let mut offset = p.bounded(u64::from(be32(&brand, 0)))?;
    let mut media = Vec::<Range<u64>>::new();
    let mut chunks = None::<Vec<Range<u64>>>;
    for _ in 0..MAX_FIELDS {
        let short = p.bytes::<8>(offset)?;
        let (size, prefix) = if be32(&short, 0) == 1 {
            atom_header(&p.bytes::<16>(offset)?)?
        } else {
            atom_header(&short)?
        };
        let end = p.bounded(offset.checked_add(size).ok_or(ProbeError::Invalid)?)?;
        let body = offset + prefix as u64;
        match &short[4..8] {
            b"mdat" => {
                require(end > body)?;
                media.push(body..end);
            }
            b"moov" => {
                require(chunks.is_none() && end - body <= MAX_METADATA)?;
                budget.charge(end - body).map_err(ProbeError::Failed)?;
                let mut bytes = vec![0; (end - body) as usize];
                for (index, part) in bytes.chunks_mut(64 * 1024).enumerate() {
                    p.read_at(body + (index * 64 * 1024) as u64, part)?;
                }
                let indexed = movie_chunks(&bytes)?;
                require(indexed.last().is_some_and(|chunk| chunk.end <= p.limit))?;
                chunks = Some(indexed);
            }
            b"free" | b"skip" | b"wide" | b"uuid" | b"udta" | b"meta" | b"pdin" => (),
            _ => return Err(ProbeError::Invalid),
        }
        if let Some(indexed) = &chunks {
            if indexed.last().is_some_and(|chunk| chunk.end <= end) {
                let valid = indexed.iter().all(|chunk| {
                    let index = media.partition_point(|extent| extent.end <= chunk.start);
                    media.get(index).is_some_and(|extent| {
                        extent.start <= chunk.start && chunk.end <= extent.end
                    })
                });
                require(valid)?;
                return Ok(end);
            }
        }
        offset = end;
    }
    Err(ProbeError::Invalid)
}

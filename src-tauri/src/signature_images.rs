//! Image containers with explicit, bounded references to the stored pixel data.
use super::{Kind, PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek},
};

const PNG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_TIFF_DIRECTORIES: usize = 256;
const MAX_TIFF_FIELDS: u64 = 1024;
const MAX_IMAGE_RANGES: usize = 32_768;

fn require(condition: bool) -> PResult<()> {
    condition.then_some(()).ok_or(ProbeError::Invalid)
}

#[derive(Clone, Copy)]
struct Tiff {
    little: bool,
    wide: bool,
}

impl Tiff {
    fn detect(bytes: &[u8]) -> Option<Self> {
        let little = match bytes.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let mut layout = Self {
            little,
            wide: false,
        };
        match layout.number(bytes.get(2..4)?) {
            42 => Some(layout),
            43 if layout.number(bytes.get(4..6)?) == 8 && bytes.get(6..8)? == b"\0\0" => {
                layout.wide = true;
                Some(layout)
            }
            _ => None,
        }
    }

    fn number(self, bytes: &[u8]) -> u64 {
        let mut storage = [0; 8];
        if self.little {
            storage[..bytes.len()].copy_from_slice(bytes);
            u64::from_le_bytes(storage)
        } else {
            storage[8 - bytes.len()..].copy_from_slice(bytes);
            u64::from_be_bytes(storage)
        }
    }

    fn offset_width(self) -> usize {
        if self.wide {
            8
        } else {
            4
        }
    }

    fn count_width(self) -> usize {
        if self.wide {
            8
        } else {
            2
        }
    }

    fn field_width(self, kind: u64) -> PResult<usize> {
        match kind {
            1 | 2 | 6 | 7 => Ok(1),
            3 | 8 => Ok(2),
            4 | 9 | 11 | 13 => Ok(4),
            5 | 10 | 12 => Ok(8),
            16..=18 if self.wide => Ok(8),
            _ => Err(ProbeError::Invalid),
        }
    }
}

pub(super) fn tiff_start(bytes: &[u8]) -> bool {
    Tiff::detect(bytes).is_some()
}

#[derive(Clone, Copy)]
struct Field {
    kind: u64,
    start: u64,
    count: u64,
    width: usize,
}

impl Field {
    fn integers<R: Read + Seek, T: FnMut() -> Result<(), String>>(
        self,
        tiff: Tiff,
        p: &mut Probe<'_, R, T>,
        budget: &mut Budget,
    ) -> PResult<Vec<u64>> {
        require(
            matches!(self.kind, 3 | 4 | 13 | 16 | 18) && self.count <= MAX_IMAGE_RANGES as u64,
        )?;
        let mut bytes = vec![0; self.count as usize * self.width];
        budget
            .charge(bytes.len() as u64)
            .map_err(ProbeError::Failed)?;
        p.read_at(self.start, &mut bytes)?;
        Ok(bytes
            .chunks_exact(self.width)
            .map(|bytes| tiff.number(bytes))
            .collect())
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Storage {
    Directory,
    Metadata,
    Pixels,
}

struct Ranges {
    items: Vec<(u64, u64, Storage)>,
    first: u64,
    limit: u64,
}

impl Ranges {
    fn add(&mut self, offset: u64, length: u64, storage: Storage) -> PResult<()> {
        let end = offset.checked_add(length).ok_or(ProbeError::Invalid)?;
        require(offset >= self.first && length != 0 && end <= self.limit)?;
        require(self.items.len() < MAX_IMAGE_RANGES)?;
        self.items.push((offset, end, storage));
        Ok(())
    }

    fn end(mut self) -> PResult<u64> {
        self.items.sort_unstable();
        self.items.dedup();
        let mut end = self.first;
        for (start, next, _) in self.items {
            require(start >= end)?;
            end = next;
        }
        Ok(end)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Directory {
    Image,
    Metadata,
}

struct Ifd {
    fields: BTreeMap<u64, Field>,
    next: u64,
    children: Vec<(u64, Directory)>,
}

fn tiff_directory<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    budget: &mut Budget,
    tiff: Tiff,
    offset: u64,
    ranges: &mut Ranges,
) -> PResult<Ifd> {
    require(offset >= ranges.first && offset % 2 == 0)?;
    let count_width = tiff.count_width();
    let offset_width = tiff.offset_width();
    let count = tiff.number(&p.bytes::<8>(offset)?[..count_width]);
    require((1..=MAX_TIFF_FIELDS).contains(&count))?;
    let entry_width = 4 + 2 * offset_width;
    let mut table = vec![0; count as usize * entry_width + offset_width];
    ranges.add(
        offset,
        (count_width + table.len()) as u64,
        Storage::Directory,
    )?;
    budget
        .charge((count_width + table.len()) as u64)
        .map_err(ProbeError::Failed)?;
    let table_at = offset + count_width as u64;
    p.read_at(table_at, &mut table)?;
    let entries_end = table.len() - offset_width;
    let next = tiff.number(&table[entries_end..]);
    let mut fields = BTreeMap::new();
    let mut children = Vec::new();
    let mut last_tag = 0;
    for (index, entry) in table[..entries_end].chunks_exact(entry_width).enumerate() {
        let tag = tiff.number(&entry[..2]);
        // Offset-based legacy JPEG tables and camera RAW require additional
        // format-specific reference rules before a complete range can be known.
        require(tag > last_tag && !matches!(tag, 519..=521 | 50706))?;
        last_tag = tag;
        let kind = tiff.number(&entry[2..4]);
        let value_at = entry_width - offset_width;
        let count = tiff.number(&entry[4..value_at]);
        let width = tiff.field_width(kind)?;
        let length = count.checked_mul(width as u64).ok_or(ProbeError::Invalid)?;
        let start = if length > offset_width as u64 {
            let start = tiff.number(&entry[value_at..]);
            ranges.add(start, length, Storage::Metadata)?;
            start
        } else {
            table_at + (index * entry_width + value_at) as u64
        };
        let field = Field {
            kind,
            start,
            count,
            width,
        };
        let child_kind = match tag {
            330 => Some(Directory::Image),
            34665 | 34853 | 40965 => Some(Directory::Metadata),
            _ if matches!(kind, 13 | 18) => Some(Directory::Metadata),
            _ => None,
        };
        if let Some(child_kind) = child_kind {
            require(count != 0 && count <= MAX_TIFF_DIRECTORIES as u64)?;
            for child in field.integers(tiff, p, budget)? {
                require(child != 0 && children.len() < MAX_TIFF_DIRECTORIES)?;
                children.push((child, child_kind));
            }
        }
        fields.insert(tag, field);
    }
    Ok(Ifd {
        fields,
        next,
        children,
    })
}

fn tiff_pixels<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    budget: &mut Budget,
    tiff: Tiff,
    ifd: &Ifd,
    directory: Directory,
    ranges: &mut Ranges,
) -> PResult<bool> {
    let mut scalar = |tag| -> PResult<Option<u64>> {
        ifd.fields
            .get(&tag)
            .map(|field| {
                require(field.count == 1 && matches!(field.kind, 3 | 4 | 16))?;
                Ok(field.integers(tiff, p, budget)?[0])
            })
            .transpose()
    };
    let strip = ifd.fields.contains_key(&273) || ifd.fields.contains_key(&279);
    let tile = ifd.fields.contains_key(&324) || ifd.fields.contains_key(&325);
    require(!(strip && tile))?;
    let jpeg_start = scalar(513)?;
    let jpeg_size = scalar(514)?;
    require(jpeg_start.is_some() == jpeg_size.is_some())?;
    let geometry = if strip || tile {
        let w = scalar(256)?.filter(|&w| w > 0).ok_or(ProbeError::Invalid)?;
        let h = scalar(257)?.filter(|&h| h > 0).ok_or(ProbeError::Invalid)?;
        require(ifd.fields.contains_key(&262) && scalar(259)? != Some(6))?;
        let planes = match scalar(284)?.unwrap_or(1) {
            1 => 1,
            2 => scalar(277)?.filter(|&n| n > 0).ok_or(ProbeError::Invalid)?,
            _ => return Err(ProbeError::Invalid),
        };
        let (tags, count) = if tile {
            let tw = scalar(322)?.filter(|&w| w > 0).ok_or(ProbeError::Invalid)?;
            let th = scalar(323)?.filter(|&h| h > 0).ok_or(ProbeError::Invalid)?;
            (
                (324, 325),
                h.div_ceil(th)
                    .checked_mul(w.div_ceil(tw))
                    .ok_or(ProbeError::Invalid)?,
            )
        } else {
            let rows = scalar(278)?.unwrap_or(u32::MAX as u64);
            require(rows != 0)?;
            ((273, 279), h.div_ceil(rows))
        };
        Some((tags, count.checked_mul(planes).ok_or(ProbeError::Invalid)?))
    } else {
        None
    };
    if let Some(((offset_tag, size_tag), count)) = geometry {
        let offsets = ifd.fields.get(&offset_tag).ok_or(ProbeError::Invalid)?;
        let sizes = ifd.fields.get(&size_tag).ok_or(ProbeError::Invalid)?;
        require(offsets.count == count && sizes.count == count)?;
        for (start, size) in offsets
            .integers(tiff, p, budget)?
            .into_iter()
            .zip(sizes.integers(tiff, p, budget)?)
        {
            ranges.add(start, size, Storage::Pixels)?;
        }
    }
    if let (Some(start), Some(size)) = (jpeg_start, jpeg_size) {
        require(size >= 5)?;
        ranges.add(start, size, Storage::Pixels)?;
        require(
            p.bytes::<3>(start)? == *b"\xff\xd8\xff"
                && p.bytes::<2>(start + size - 2)? == *b"\xff\xd9",
        )?;
    }
    require(directory == Directory::Metadata || geometry.is_some() || jpeg_start.is_some())?;
    Ok(geometry.is_some())
}

pub(super) fn tiff<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    budget: &mut Budget,
) -> PResult<u64> {
    let header = p.bytes::<16>(0)?;
    let layout = Tiff::detect(&header).ok_or(ProbeError::Invalid)?;
    let first = if layout.wide { 16 } else { 8 };
    let next = layout.number(&header[first / 2..first]);
    let mut ranges = Ranges {
        items: Vec::new(),
        first: first as u64,
        limit: p.limit,
    };
    let mut seen = BTreeSet::new();
    let mut pending = vec![(next, Directory::Image)];
    while let Some((offset, role)) = pending.pop() {
        require(seen.len() < MAX_TIFF_DIRECTORIES && seen.insert(offset))?;
        let ifd = tiff_directory(p, budget, layout, offset, &mut ranges)?;
        let image = tiff_pixels(p, budget, layout, &ifd, role, &mut ranges)?;
        require(seen.len() != 1 || image)?;
        pending.extend(ifd.children);
        if ifd.next != 0 {
            pending.push((ifd.next, role));
        }
        require(pending.len() + seen.len() <= MAX_TIFF_DIRECTORIES)?;
    }
    ranges.end()
}

pub(super) fn icon_kind(bytes: &[u8]) -> Option<Kind> {
    let header = bytes.get(..6)?;
    let count = le16(&header[4..]);
    if !(1..=256).contains(&count) {
        return None;
    }
    match &header[..4] {
        b"\0\0\x01\0" => Some(Kind::Ico),
        b"\0\0\x02\0" => Some(Kind::Cur),
        _ => None,
    }
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn icon_dib<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    start: u64,
    length: u64,
    width: u64,
    height: u64,
) -> PResult<()> {
    require(length >= 40)?;
    let header = p.bytes::<40>(start)?;
    let depth = le16(&header[14..]);
    require(
        le32(&header) == 40
            && u64::from(le32(&header[4..])) == width
            && u64::from(le32(&header[8..])) == height * 2
            && le16(&header[12..]) == 1
            && matches!(depth, 1 | 4 | 8 | 16 | 24 | 32)
            && le32(&header[16..]) == 0,
    )?;
    let used = u64::from(le32(&header[32..]));
    let palette = if depth <= 8 {
        let colors = 1u64 << depth;
        require(used <= colors)?;
        if used == 0 {
            colors
        } else {
            used
        }
    } else {
        require(used == 0)?;
        0
    };
    let xor = (width * u64::from(depth)).div_ceil(32) * 4 * height;
    let and = width.div_ceil(32) * 4 * height;
    let out = 40 + palette * 4 + xor;
    require(length == out + and || depth == 32 && length == out)
}

fn icon_png<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    budget: &mut Budget,
    start: u64,
    end: u64,
    width: u64,
    height: u64,
) -> PResult<()> {
    require(end - start <= 8 * 1024 * 1024)?;
    let mut at = start + 8;
    let mut image_data = false;
    let mut palette = false;
    let mut indexed = false;
    let mut buffer = vec![0; 16 * 1024];
    while end - at >= 12 {
        let header = p.bytes::<8>(at)?;
        let length = u64::from(be32(&header));
        let kind = &header[4..];
        require(kind.iter().all(u8::is_ascii_alphabetic) && kind[2].is_ascii_uppercase())?;
        let next = at + 12 + length;
        require(next <= end)?;
        budget.charge(next - at).map_err(ProbeError::Failed)?;
        let data = at + 8;
        match kind {
            b"IHDR" if at == start + 8 && length == 13 => {
                let ihdr = p.bytes::<13>(data)?;
                require(u64::from(be32(&ihdr)) == width && u64::from(be32(&ihdr[4..])) == height)?;
                indexed = ihdr[9] == 3;
                require(
                    matches!(
                        (ihdr[9], ihdr[8]),
                        (0 | 3, 1 | 2 | 4 | 8) | (0 | 2 | 4 | 6, 8 | 16)
                    ) && ihdr[10..12] == [0, 0]
                        && ihdr[12] <= 1,
                )?;
            }
            _ if at == start + 8 => return Err(ProbeError::Invalid),
            b"PLTE"
                if !palette && !image_data && (3..=768).contains(&length) && length % 3 == 0 =>
            {
                palette = true;
            }
            b"IDAT" if !indexed || palette => image_data |= length != 0,
            b"IEND" if image_data && length == 0 && next == end => {}
            _ => require(kind[0].is_ascii_lowercase())?,
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        let mut cursor = data;
        while cursor < next - 4 {
            let n = (next - 4 - cursor).min(buffer.len() as u64) as usize;
            p.read_at(cursor, &mut buffer[..n])?;
            crc.update(&buffer[..n]);
            cursor += n as u64;
        }
        require(crc.finalize() == be32(&p.bytes::<4>(next - 4)?))?;
        if kind == b"IEND" {
            return Ok(());
        }
        at = next;
    }
    Err(ProbeError::Invalid)
}

pub(super) fn icon<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    kind: Kind,
    budget: &mut Budget,
) -> PResult<u64> {
    let header = p.bytes::<6>(0)?;
    require(icon_kind(&header) == Some(kind))?;
    let dir_size = u64::from(le16(&header[4..])) * 16;
    let first = 6 + dir_size;
    let mut ranges = Ranges {
        items: Vec::new(),
        first,
        limit: p.limit,
    };
    let mut directory = vec![0; dir_size as usize];
    budget.charge(first).map_err(ProbeError::Failed)?;
    p.read_at(6, &mut directory)?;
    let dimension = |byte| if byte == 0 { 256 } else { u64::from(byte) };
    for entry in directory.chunks_exact(16) {
        let width = dimension(entry[0]);
        let height = dimension(entry[1]);
        require(entry[3] == 0)?;
        if kind == Kind::Cur {
            require(u64::from(le16(&entry[4..])) < width && u64::from(le16(&entry[6..])) < height)?;
        }
        let length = u64::from(le32(&entry[8..]));
        let start = u64::from(le32(&entry[12..]));
        require(length >= 40)?;
        ranges.add(start, length, Storage::Pixels)?;
        if p.bytes::<8>(start)? == *PNG {
            icon_png(p, budget, start, start + length, width, height)?;
        } else {
            icon_dib(p, start, length, width, height)?;
        }
    }
    ranges.end()
}

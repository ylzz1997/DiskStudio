//! Bounded RIFF AVI and IFF AIFF/AIFF-C structure checks.
use super::{Kind, PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use std::{
    collections::BTreeMap,
    io::{Read, Seek},
};

pub(super) fn kind(header: &[u8]) -> Option<Kind> {
    match (header.get(..4)?, header.get(8..12)?) {
        (b"RIFF", b"AVI ") => Some(Kind::Avi),
        (b"FORM", b"AIFF") => Some(Kind::Aiff),
        (b"FORM", b"AIFC") => Some(Kind::Aifc),
        _ => None,
    }
}

pub(super) fn declared_size(header: &[u8]) -> Option<u64> {
    let size = match kind(header)? {
        Kind::Avi => le32(&header[4..8]),
        _ => be32(&header[4..8]),
    };
    (size >= 4 && size % 2 == 0).then_some(u64::from(size) + 8)
}

pub(super) fn inspect<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    expected: Kind,
    header: &[u8; 12],
    budget: &mut Budget,
) -> PResult<u64> {
    require(kind(header) == Some(expected))?;
    let end = probe.bounded(declared_size(header).ok_or(ProbeError::Invalid)?)?;
    let mut parser = Container {
        probe,
        budget,
        little_endian: expected == Kind::Avi,
    };
    if expected == Kind::Avi {
        parser.avi(end)?;
    } else {
        parser.aiff(end, expected == Kind::Aifc)?;
    }
    Ok(end)
}

struct Container<'a, 'r, R, T> {
    probe: &'a mut Probe<'r, R, T>,
    budget: &'a mut Budget,
    little_endian: bool,
}

#[derive(Clone, Copy)]
struct Chunk {
    id: [u8; 4],
    data: u64,
    size: u64,
    next: u64,
}

impl Chunk {
    fn end(self) -> u64 {
        self.data + self.size
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Video,
    Audio,
    Text,
}

impl<R: Read + Seek, T: FnMut() -> Result<(), String>> Container<'_, '_, R, T> {
    fn bytes<const N: usize>(&mut self, at: u64) -> PResult<[u8; N]> {
        self.budget.charge(N as u64).map_err(ProbeError::Failed)?;
        self.probe.bytes(at)
    }

    fn chunk(&mut self, start: u64, limit: u64) -> PResult<Chunk> {
        require(start + 8 <= limit)?;
        let header = self.bytes::<8>(start)?;
        let id = header[..4].try_into().expect("four-byte chunk ID");
        require(fourcc(&id))?;
        let size = u64::from(if self.little_endian {
            le32(&header[4..])
        } else {
            be32(&header[4..])
        });
        let data = start + 8;
        let next = data + size + (size & 1);
        require(next <= limit)?;
        if !self.little_endian && size & 1 != 0 {
            require(self.bytes::<1>(next - 1)? == [0])?;
        }
        Ok(Chunk {
            id,
            data,
            size,
            next,
        })
    }

    fn list_type(&mut self, chunk: Chunk) -> PResult<[u8; 4]> {
        require(chunk.size >= 4)?;
        let kind = self.bytes::<4>(chunk.data)?;
        require(fourcc(&kind))?;
        Ok(kind)
    }

    fn avi(&mut self, end: u64) -> PResult<()> {
        let mut at = 12;
        let mut headers = None;
        let mut movi = None;
        let mut index = None;
        while at < end {
            let chunk = self.chunk(at, end)?;
            match &chunk.id {
                b"LIST" => match &self.list_type(chunk)? {
                    b"hdrl" if headers.is_none() && movi.is_none() => {
                        headers = Some(self.avi_headers(chunk)?)
                    }
                    b"movi" if headers.is_some() && movi.is_none() => movi = Some(chunk),
                    b"INFO" => {}
                    _ => return Err(ProbeError::Invalid),
                },
                b"idx1" if index.is_none() && movi.is_some() => index = Some(chunk),
                b"JUNK" | b"PAD " => {}
                _ => return Err(ProbeError::Invalid),
            }
            at = chunk.next;
        }
        let (streams, index_required) = headers.ok_or(ProbeError::Invalid)?;
        let movi = movi.ok_or(ProbeError::Invalid)?;
        let mut packets = BTreeMap::new();
        require(self.avi_packets(movi, &streams, false, &mut packets)?)?;
        if let Some(index) = index {
            self.avi_index(index, movi, &packets)?;
        } else {
            require(!index_required)?;
        }
        if end + 12 <= self.probe.limit {
            let following = self.bytes::<12>(end)?;
            require(!following.starts_with(b"RIFF") || &following[8..] != b"AVIX")?;
        }
        Ok(())
    }

    fn avi_headers(&mut self, list: Chunk) -> PResult<(Vec<Stream>, bool)> {
        let main = self.chunk(list.data + 4, list.end())?;
        require(&main.id == b"avih" && main.size == 56)?;
        let bytes = self.bytes::<56>(main.data)?;
        let count = le32(&bytes[24..28]) as usize;
        require(
            (1..=64).contains(&count)
                && le32(&bytes[..4]) != 0
                && le32(&bytes[32..36]) != 0
                && le32(&bytes[36..40]) != 0,
        )?;
        let mut streams = Vec::with_capacity(count);
        let mut odml = false;
        let mut at = main.next;
        while at < list.end() {
            let chunk = self.chunk(at, list.end())?;
            match &chunk.id {
                b"LIST" => match &self.list_type(chunk)? {
                    b"strl" if streams.len() < count => streams.push(self.avi_stream(chunk)?),
                    // Some writers reserve a dmlh block even in classic single-RIFF AVI.
                    b"odml" if !odml => {
                        let extended = self.chunk(chunk.data + 4, chunk.end())?;
                        require(
                            &extended.id == b"dmlh"
                                && extended.size == 248
                                && extended.next == chunk.end(),
                        )?;
                        odml = true;
                    }
                    _ => return Err(ProbeError::Invalid),
                },
                b"JUNK" | b"PAD " => {}
                _ => return Err(ProbeError::Invalid),
            }
            at = chunk.next;
        }
        require(streams.len() == count && streams.contains(&Stream::Video))?;
        // HASINDEX and MUSTUSEINDEX both warrant checking the index.
        Ok((streams, le32(&bytes[12..16]) & 0x30 != 0))
    }

    fn avi_stream(&mut self, list: Chunk) -> PResult<Stream> {
        let header = self.chunk(list.data + 4, list.end())?;
        require(&header.id == b"strh" && header.size == 56)?;
        let bytes = self.bytes::<56>(header.data)?;
        let stream = match &bytes[..4] {
            b"vids" => Stream::Video,
            b"auds" => Stream::Audio,
            b"txts" => Stream::Text,
            _ => return Err(ProbeError::Invalid),
        };
        require(le32(&bytes[20..24]) != 0 && le32(&bytes[24..28]) != 0)?;
        let mut at = header.next;
        let mut format = false;
        let mut video_properties = false;
        while at < list.end() {
            let chunk = self.chunk(at, list.end())?;
            match &chunk.id {
                b"strf" if !format => {
                    match stream {
                        Stream::Video => {
                            require(chunk.size >= 40)?;
                            let bytes = self.bytes::<40>(chunk.data)?;
                            require(
                                (40..=chunk.size).contains(&u64::from(le32(&bytes[..4])))
                                    && le32(&bytes[4..8]) != 0
                                    && le32(&bytes[8..12]) != 0
                                    && bytes[12..14] == [1, 0],
                            )?;
                        }
                        Stream::Audio => {
                            require(chunk.size >= 16)?;
                            let bytes = self.bytes::<16>(chunk.data)?;
                            require(
                                bytes[2..4] != [0, 0]
                                    && le32(&bytes[4..8]) != 0
                                    && bytes[12..14] != [0, 0],
                            )?;
                        }
                        Stream::Text => {}
                    }
                    format = true;
                }
                b"vprp" if stream == Stream::Video && !video_properties => {
                    require(chunk.size >= 36)?;
                    let properties = self.bytes::<36>(chunk.data)?;
                    let fields = le32(&properties[32..]);
                    require(
                        (1..=2).contains(&fields) && chunk.size == 36 + 32 * u64::from(fields),
                    )?;
                    video_properties = true;
                }
                b"strd" | b"strn" | b"JUNK" | b"PAD " => {}
                _ => return Err(ProbeError::Invalid),
            }
            at = chunk.next;
        }
        require(format)?;
        Ok(stream)
    }

    fn avi_packets(
        &mut self,
        list: Chunk,
        streams: &[Stream],
        inside_record: bool,
        packets: &mut BTreeMap<u64, ([u8; 4], u64)>,
    ) -> PResult<bool> {
        let mut at = list.data + 4;
        let mut has_video = false;
        while at < list.end() {
            let chunk = self.chunk(at, list.end())?;
            match &chunk.id {
                b"JUNK" | b"PAD " => {}
                b"LIST" if !inside_record && &self.list_type(chunk)? == b"rec " => {
                    has_video |= self.avi_packets(chunk, streams, true, packets)?;
                    packets.insert(at, (*b"rec ", chunk.size));
                }
                [a @ b'0'..=b'9', b @ b'0'..=b'9', suffix_a, suffix_b] => {
                    let index = usize::from(a - b'0') * 10 + usize::from(b - b'0');
                    let stream = streams.get(index).ok_or(ProbeError::Invalid)?;
                    let suffix = [*suffix_a, *suffix_b];
                    require(match stream {
                        Stream::Video => matches!(&suffix, b"db" | b"dc" | b"pc"),
                        Stream::Audio => &suffix == b"wb",
                        Stream::Text => true,
                    })?;
                    has_video |= *stream == Stream::Video && &suffix != b"pc" && chunk.size != 0;
                    packets.insert(at, (chunk.id, chunk.size));
                }
                _ => return Err(ProbeError::Invalid),
            }
            at = chunk.next;
        }
        Ok(has_video)
    }

    fn avi_index(
        &mut self,
        index: Chunk,
        movi: Chunk,
        packets: &BTreeMap<u64, ([u8; 4], u64)>,
    ) -> PResult<()> {
        require(
            index.size != 0 && index.size % 16 == 0 && index.size / 16 <= packets.len() as u64,
        )?;
        // AVI writers use absolute offsets or offsets relative to the movi list.
        // Every entry must agree on a single base and match a different chunk.
        let mut bases = vec![0, movi.data, movi.data + 4];
        let mut seen = std::collections::BTreeSet::new();
        for at in (index.data..index.end()).step_by(16) {
            let entry = self.bytes::<16>(at)?;
            let offset = u64::from(le32(&entry[8..12]));
            let size = u64::from(le32(&entry[12..16]));
            require(seen.insert(offset))?;
            bases.retain(|base| {
                packets
                    .get(&(base + offset))
                    .is_some_and(|(id, length)| entry[..4] == *id && size == *length)
            });
            require(!bases.is_empty())?;
        }
        Ok(())
    }

    fn aiff(&mut self, end: u64, compressed_form: bool) -> PResult<()> {
        let mut at = 12;
        let mut version = false;
        let mut samples = None;
        let mut sound = None;
        while at < end {
            let chunk = self.chunk(at, end)?;
            match &chunk.id {
                b"FVER" => {
                    require(compressed_form && !version && samples.is_none() && chunk.size == 4)?;
                    require(be32(&self.bytes::<4>(chunk.data)?) == 0xa2805140)?;
                    version = true;
                }
                b"COMM" => {
                    require(samples.is_none() && (!compressed_form || version))?;
                    samples = Some(self.aiff_samples(chunk, compressed_form)?);
                }
                b"SSND" => {
                    require(sound.is_none() && chunk.size >= 8)?;
                    let fields = self.bytes::<8>(chunk.data)?;
                    let offset = u64::from(be32(&fields[..4]));
                    let block = u64::from(be32(&fields[4..]));
                    let payload = chunk.size - 8;
                    require(offset <= payload && (block != 0 || offset == 0))?;
                    sound = Some((payload - offset, block));
                }
                _ => {}
            }
            at = chunk.next;
        }
        let samples = samples.ok_or(ProbeError::Invalid)?;
        match sound {
            Some((available, block)) => require(
                available == samples
                    || (block != 0 && available > samples && available - samples < block),
            ),
            None => require(samples == 0),
        }
    }

    fn aiff_samples(&mut self, common: Chunk, compressed_form: bool) -> PResult<u64> {
        require(if compressed_form {
            (24..=278).contains(&common.size)
        } else {
            common.size == 18
        })?;
        let bytes = self.bytes::<18>(common.data)?;
        let channels = u16::from_be_bytes([bytes[0], bytes[1]]);
        let frames = be32(&bytes[2..6]);
        let depth = u16::from_be_bytes([bytes[6], bytes[7]]);
        let exponent = u16::from_be_bytes([bytes[8], bytes[9]]);
        require(
            (1..=256).contains(&channels)
                && (0x3fff..=0x401e).contains(&exponent)
                && bytes[10] & 0x80 != 0,
        )?;
        let sample_bytes = if compressed_form {
            let compression = self.bytes::<5>(common.data + 18)?;
            let name = 1 + u64::from(compression[4]);
            require(common.size == 22 + name + (name & 1))?;
            if name & 1 != 0 {
                require(self.bytes::<1>(common.end() - 1)? == [0])?;
            }
            match &compression[..4] {
                b"NONE" | b"twos" | b"sowt" => pcm_width(depth)?,
                b"fl32" | b"FL32" if depth == 32 => 4,
                b"fl64" | b"FL64" if depth == 64 => 8,
                b"ulaw" | b"ULAW" | b"alaw" | b"ALAW" if matches!(depth, 8 | 16) => 1,
                _ => return Err(ProbeError::Invalid),
            }
        } else {
            pcm_width(depth)?
        };
        Ok(u64::from(frames) * u64::from(channels) * sample_bytes)
    }
}

fn fourcc(bytes: &[u8; 4]) -> bool {
    bytes.iter().all(|byte| (b' '..=b'~').contains(byte)) && bytes != b"    "
}

fn pcm_width(depth: u16) -> PResult<u64> {
    require((1..=32).contains(&depth))?;
    Ok(u64::from(depth.div_ceil(8)))
}

fn require(valid: bool) -> PResult<()> {
    valid.then_some(()).ok_or(ProbeError::Invalid)
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte field"))
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four-byte field"))
}

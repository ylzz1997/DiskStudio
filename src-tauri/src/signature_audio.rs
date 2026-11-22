//! Standard MIDI Files, Sony DSF, and uncompressed Philips DSDIFF.
use super::{Kind, PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use std::{
    collections::BTreeSet,
    io::{Read, Seek},
};

const MAX_MIDI_TRACK: u64 = 8 * 1024 * 1024;
const MAX_ID3_BYTES: u64 = 32 * 1024 * 1024;
const DSF_BLOCK: u64 = 4096;

pub(super) fn midi_start(bytes: &[u8]) -> bool {
    bytes.starts_with(b"MThd")
        && bytes
            .get(4..8)
            .is_some_and(|size| (6..=256).contains(&be32(size)))
}

pub(super) fn dsf_start(bytes: &[u8]) -> bool {
    bytes.starts_with(b"DSD \x1c\0\0\0\0\0\0\0")
}

pub(super) fn dff_start(bytes: &[u8]) -> bool {
    bytes.starts_with(b"FRM8") && bytes.get(12..16) == Some(b"DSD ")
}

pub(super) fn declared_size(kind: Kind, bytes: &[u8]) -> Option<u64> {
    match kind {
        Kind::Dsf if dsf_start(bytes) => Some(le64(bytes.get(12..20)?)),
        Kind::Dff if dff_start(bytes) => be64(bytes.get(4..12)?).checked_add(12),
        _ => None,
    }
}

pub(super) fn midi<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    header: &[u8; 14],
    budget: &mut Budget,
) -> PResult<u64> {
    require(midi_start(header))?;
    let format = be16(&header[8..10]);
    let tracks = be16(&header[10..12]);
    let division = be16(&header[12..14]);
    let valid_division = if division & 0x8000 == 0 {
        division != 0
    } else {
        matches!(header[12] as i8, -24 | -25 | -29 | -30) && header[13] != 0
    };
    require(format <= 2 && tracks > 0 && (format != 0 || tracks == 1) && valid_division)?;
    let mut at = probe.bounded(8 + u64::from(be32(&header[4..8])))?;
    let mut bytes = Vec::new();
    let mut found = 0;
    while found < tracks {
        let chunk = probe.bytes::<8>(at)?;
        let size = u64::from(be32(&chunk[4..]));
        let end = probe.bounded(at + 8 + size)?;
        budget.charge(8).map_err(ProbeError::Failed)?;
        if &chunk[..4] == b"MTrk" {
            require((4..=MAX_MIDI_TRACK).contains(&size))?;
            budget.charge(size).map_err(ProbeError::Failed)?;
            bytes.resize(size as usize, 0);
            probe.read_at(at + 8, &mut bytes)?;
            midi_track(&bytes, &mut probe.tick)?;
            found += 1;
        } else {
            require(ascii_chunk(&chunk[..4]) && &chunk[..4] != b"MThd")?;
        }
        at = end;
    }
    Ok(at)
}

fn midi_track(mut bytes: &[u8], tick: &mut impl FnMut() -> Result<(), String>) -> PResult<()> {
    let mut running = None;
    let mut events = 0usize;
    while !bytes.is_empty() {
        if events % 4096 == 0 {
            tick().map_err(ProbeError::Failed)?;
        }
        events += 1;
        midi_vlq(&mut bytes)?;
        let first = *bytes.first().ok_or(ProbeError::Invalid)?;
        let status = if first & 0x80 != 0 {
            midi_byte(&mut bytes)?
        } else {
            running.ok_or(ProbeError::Invalid)?
        };
        match status {
            0x80..=0xef => {
                let size = if matches!(status >> 4, 0xc | 0xd) {
                    1
                } else {
                    2
                };
                require(midi_take(&mut bytes, size)?.iter().all(|v| *v < 128))?;
                running = Some(status);
            }
            0xf0 | 0xf7 => {
                let size = midi_vlq(&mut bytes)?;
                midi_take(&mut bytes, size)?;
                running = None;
            }
            0xff => {
                let kind = midi_byte(&mut bytes)?;
                let size = midi_vlq(&mut bytes)?;
                require(kind < 128)?;
                if kind == 0x2f {
                    return require(size == 0 && bytes.is_empty());
                }
                let content = midi_take(&mut bytes, size)?;
                require(match kind {
                    0 => size == 2,
                    0x20 | 0x21 => size == 1,
                    0x51 => size == 3 && content != [0, 0, 0],
                    0x54 => size == 5,
                    0x58 => size == 4,
                    0x59 => size == 2,
                    _ => true,
                })?;
                running = None;
            }
            _ => return Err(ProbeError::Invalid),
        }
    }
    Err(ProbeError::Invalid)
}

fn midi_vlq(bytes: &mut &[u8]) -> PResult<usize> {
    let mut value = 0;
    for _ in 0..4 {
        let byte = midi_byte(bytes)?;
        value = (value << 7) | usize::from(byte & 0x7f);
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(ProbeError::Invalid)
}

fn midi_byte(bytes: &mut &[u8]) -> PResult<u8> {
    Ok(midi_take(bytes, 1)?[0])
}

fn midi_take<'a>(bytes: &mut &'a [u8], count: usize) -> PResult<&'a [u8]> {
    let (data, rest) = bytes.split_at_checked(count).ok_or(ProbeError::Invalid)?;
    *bytes = rest;
    Ok(data)
}

pub(super) fn dsf<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    header: &[u8; 28],
    budget: &mut Budget,
) -> PResult<u64> {
    let end = probe.bounded(declared_size(Kind::Dsf, header).ok_or(ProbeError::Invalid)?)?;
    let metadata = le64(&header[20..28]);
    require(end >= 92)?;
    let format = probe.bytes::<52>(28)?;
    require(
        &format[..4] == b"fmt "
            && le64(&format[4..12]) == 52
            && le32(&format[12..16]) == 1
            && le32(&format[16..20]) == 0,
    )?;
    let channel_type = le32(&format[20..24]) as usize;
    let channels = u64::from(le32(&format[24..28]));
    let rate = le32(&format[28..32]);
    let bits = le32(&format[32..36]);
    let samples = le64(&format[36..44]);
    require(
        channels != 0
            && [0, 1, 2, 3, 4, 4, 5, 6].get(channel_type) == Some(&channels)
            && [2_822_400, 3_072_000]
                .into_iter()
                .any(|base| rate % base == 0 && (rate / base).is_power_of_two())
            && matches!(bits, 1 | 8)
            && samples > 0
            && u64::from(le32(&format[44..48])) == DSF_BLOCK
            && format[48..52] == [0; 4],
    )?;
    let data = probe.bytes::<12>(80)?;
    require(&data[..4] == b"data")?;
    let channel_bytes = samples.div_ceil(8);
    let groups = channel_bytes.div_ceil(DSF_BLOCK);
    let data_size = 12 + groups * DSF_BLOCK * channels;
    let audio_end = 80 + data_size;
    require(le64(&data[4..12]) == data_size && audio_end <= end)?;
    let used = channel_bytes % DSF_BLOCK;
    if used != 0 {
        let pad = (DSF_BLOCK - used) as usize;
        let last_group = audio_end - DSF_BLOCK * channels;
        let mut padding = [0; DSF_BLOCK as usize];
        for channel in 0..channels {
            budget.charge(pad as u64).map_err(ProbeError::Failed)?;
            probe.read_at(last_group + channel * DSF_BLOCK + used, &mut padding[..pad])?;
            require(padding[..pad].iter().all(|byte| *byte == 0))?;
        }
    }
    if metadata == 0 {
        require(audio_end == end)?;
    } else {
        require(metadata == audio_end)?;
        id3(probe, metadata, end)?;
    }
    Ok(end)
}

fn id3<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    at: u64,
    end: u64,
) -> PResult<()> {
    require((10..=MAX_ID3_BYTES).contains(&(end - at)))?;
    let header = probe.bytes::<10>(at)?;
    let allowed_flags = match header[3] {
        2 => 0xc0,
        3 => 0xe0,
        4 => 0xf0,
        _ => return Err(ProbeError::Invalid),
    };
    require(
        header.starts_with(b"ID3")
            && header[4] != 255
            && header[5] & !allowed_flags == 0
            && header[6..].iter().all(|v| *v < 128),
    )?;
    let size = header[6..]
        .iter()
        .fold(0, |size, &byte| (size << 7) | u64::from(byte));
    let footer = header[3] == 4 && header[5] & 0x10 != 0;
    require(at + 10 + size + if footer { 10 } else { 0 } == end)?;
    if footer {
        let footer = probe.bytes::<10>(end - 10)?;
        require(footer.starts_with(b"3DI") && footer[3..] == header[3..])?;
    }
    Ok(())
}

struct Chunk {
    id: [u8; 4],
    size: u64,
    data: u64,
    end: u64,
    next: u64,
}

fn dff_chunk<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    at: u64,
    end: u64,
    budget: &mut Budget,
) -> PResult<Chunk> {
    require(at + 12 <= end)?;
    budget.charge(12).map_err(ProbeError::Failed)?;
    let bytes = probe.bytes::<12>(at)?;
    require(ascii_chunk(&bytes[..4]))?;
    let size = be64(&bytes[4..]);
    let data = at + 12;
    require(size <= end - data)?;
    let chunk_end = data + size;
    let next = chunk_end + (size & 1);
    require(next <= end)?;
    if size & 1 != 0 {
        require(probe.bytes::<1>(chunk_end)? == [0])?;
    }
    Ok(Chunk {
        id: bytes[..4].try_into().expect("four-byte ID"),
        size,
        data,
        end: chunk_end,
        next,
    })
}

pub(super) fn dff<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    header: &[u8; 16],
    budget: &mut Budget,
) -> PResult<u64> {
    let end = probe.bounded(declared_size(Kind::Dff, header).ok_or(ProbeError::Invalid)?)?;
    require(end % 2 == 0)?;
    let version = dff_chunk(probe, 16, end, budget)?;
    require(&version.id == b"FVER" && version.size == 4)?;
    let version_bytes = probe.bytes::<4>(version.data)?;
    require(version_bytes[0] == 1 && version_bytes[2..] == [0, 0])?;
    let mut at = version.next;
    let mut channels = None;
    let mut audio = false;
    let mut tagged = false;
    while at < end {
        let chunk = dff_chunk(probe, at, end, budget)?;
        match &chunk.id {
            b"PROP" if channels.is_none() && !audio => {
                channels = Some(dff_properties(probe, &chunk, budget)?)
            }
            b"DSD " if !audio => {
                let channels = channels.ok_or(ProbeError::Invalid)?;
                require(chunk.size != 0 && chunk.size % channels == 0)?;
                audio = true;
            }
            b"ID3 " if !tagged => {
                id3(probe, chunk.data, chunk.end)?;
                tagged = true;
            }
            b"FRM8" | b"FVER" | b"PROP" | b"DSD " | b"DST " | b"DSTI" | b"ID3 " => {
                return Err(ProbeError::Invalid)
            }
            _ => {}
        }
        at = chunk.next;
    }
    require(audio)?;
    Ok(end)
}

fn dff_properties<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    props: &Chunk,
    budget: &mut Budget,
) -> PResult<u64> {
    require(props.size >= 4 && &probe.bytes::<4>(props.data)? == b"SND ")?;
    let mut at = props.data + 4;
    let mut rate = false;
    let mut channels = None;
    let mut uncompressed = false;
    while at < props.end {
        let chunk = dff_chunk(probe, at, props.end, budget)?;
        match &chunk.id {
            b"FS  " if !rate && chunk.size == 4 => {
                let hertz = be32(&probe.bytes::<4>(chunk.data)?);
                require(hertz != 0 && hertz % 8 == 0)?;
                rate = true;
            }
            b"CHNL" if channels.is_none() && chunk.size >= 6 => {
                let count = u64::from(be16(&probe.bytes::<2>(chunk.data)?));
                require((1..=256).contains(&count) && chunk.size == 2 + count * 4)?;
                let mut ids = BTreeSet::new();
                for offset in (chunk.data + 2..chunk.end).step_by(4) {
                    let id = probe.bytes::<4>(offset)?;
                    require(ascii_chunk(&id) && ids.insert(id))?;
                }
                channels = Some(count);
            }
            b"CMPR" if !uncompressed && chunk.size >= 5 => {
                let code = probe.bytes::<5>(chunk.data)?;
                require(&code[..4] == b"DSD " && chunk.size == 5 + u64::from(code[4]))?;
                uncompressed = true;
            }
            b"FS  " | b"CHNL" | b"CMPR" => return Err(ProbeError::Invalid),
            _ => {}
        }
        at = chunk.next;
    }
    require(rate && uncompressed)?;
    channels.ok_or(ProbeError::Invalid)
}

fn require(valid: bool) -> PResult<()> {
    valid.then_some(()).ok_or(ProbeError::Invalid)
}

fn ascii_chunk(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (b' '..=b'~').contains(byte)) && bytes != b"    "
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().expect("two-byte field"))
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four-byte field"))
}

fn be64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("eight-byte field"))
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte field"))
}

fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight-byte field"))
}

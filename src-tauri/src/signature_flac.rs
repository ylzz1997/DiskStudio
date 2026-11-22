//! Native FLAC frame boundaries as defined in RFC 9639. No PCM is produced.
use super::{PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use std::io::{Read, Seek};

const MAX_SAMPLE_POINTS: u64 = 256 * 1024 * 1024;
const MAX_FRAMES: u64 = 65_536;

#[derive(Clone, Copy)]
struct Stream {
    block_min: u32,
    block_max: u32,
    frame_min: u64,
    frame_max: u64,
    rate: u32,
    channels: u32,
    width: u32,
    samples: u64,
}

fn stream(header: &[u8]) -> Option<Stream> {
    if header.len() != 42
        || !header.starts_with(b"fLaC")
        || header[4] & 0x7f != 0
        || header[5..8] != [0, 0, 34]
    {
        return None;
    }
    let block_min = u32::from(u16::from_be_bytes(header[8..10].try_into().ok()?));
    let block_max = u32::from(u16::from_be_bytes(header[10..12].try_into().ok()?));
    let frame_min = u64::from(u32::from_be_bytes([0, header[12], header[13], header[14]]));
    let frame_max = u64::from(u32::from_be_bytes([0, header[15], header[16], header[17]]));
    let packed = u64::from_be_bytes(header[18..26].try_into().ok()?);
    let rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 7) as u32 + 1;
    let width = ((packed >> 36) & 31) as u32 + 1;
    let samples = packed & ((1 << 36) - 1);
    (block_min >= 16
        && block_max >= block_min
        && (frame_min <= frame_max || frame_max == 0)
        && rate != 0
        && (4..=32).contains(&width)
        && samples != 0
        && samples * u64::from(channels) <= MAX_SAMPLE_POINTS)
        .then_some(Stream {
            block_min,
            block_max,
            frame_min,
            frame_max,
            rate,
            channels,
            width,
            samples,
        })
}

pub(super) fn valid_streaminfo(header: &[u8]) -> bool {
    stream(header).is_some()
}

pub(super) fn inspect<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    header: &[u8; 42],
    budget: &mut Budget,
) -> PResult<u64> {
    let info = stream(header).ok_or(ProbeError::Invalid)?;
    let mut at = header.len() as u64;
    let mut last = header[4] & 0x80 != 0;
    let mut seen = [false; 7];
    while !last {
        let metadata = probe.bytes::<4>(at)?;
        last = metadata[0] & 0x80 != 0;
        let kind = usize::from(metadata[0] & 0x7f);
        let size = u64::from(u32::from_be_bytes([
            0,
            metadata[1],
            metadata[2],
            metadata[3],
        ]));
        require(match kind {
            1 => true,
            2 => size >= 4,
            3 => size % 18 == 0 && !seen[3],
            4 => size >= 8 && !seen[4],
            5 => size >= 396 && !seen[5],
            6 => size >= 32,
            _ => false,
        })?;
        seen[kind] = true;
        at = probe.bounded(at + 4 + size)?;
        budget.charge(4).map_err(ProbeError::Failed)?;
    }
    let mut bits = Bits::new(probe, budget, at);
    let mut blocking = None;
    let mut decoded = 0;
    let mut frames = 0;
    while decoded < info.samples {
        require(frames < MAX_FRAMES)?;
        if frames % 64 == 0 {
            (bits.probe.tick)().map_err(ProbeError::Failed)?;
        }
        let (block, variable) = frame(&mut bits, info, blocking, frames, decoded)?;
        decoded += u64::from(block);
        blocking = Some(variable);
        frames += 1;
    }
    Ok(bits.offset)
}

fn frame<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    bits: &mut Bits<'_, '_, R, T>,
    info: Stream,
    blocking: Option<bool>,
    number: u64,
    decoded: u64,
) -> PResult<(u32, bool)> {
    let start = bits.offset;
    bits.begin_frame(info.frame_max);
    require(bits.read(15)? == 0x7ffc)?;
    let variable = bits.read(1)? != 0;
    require(
        blocking.is_none_or(|old| old == variable)
            && (variable || info.block_min == info.block_max),
    )?;
    let block_code = bits.read(4)?;
    let rate_code = bits.read(4)?;
    let channel_code = bits.read(4)?;
    let width_code = bits.read(3)?;
    require(bits.read(1)? == 0)?;
    let sequence = utf_number(bits)?;
    let expected_number = if variable { decoded } else { number };
    require(sequence == expected_number && (variable || sequence <= 0x7fff_ffff))?;
    let block = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => bits.read(8)? + 1,
        7 => bits.read(16)? + 1,
        8..=15 => 256 << (block_code - 8),
        _ => return Err(ProbeError::Invalid),
    };
    let remaining = info.samples - decoded;
    require(block <= info.block_max && u64::from(block) <= remaining)?;
    if u64::from(block) < remaining {
        require(block >= info.block_min)?;
    }
    let rate = match rate_code {
        0 => info.rate,
        1..=11 => [
            88200, 176400, 192000, 8000, 16000, 22050, 24000, 32000, 44100, 48000, 96000,
        ][rate_code as usize - 1],
        12 => bits.read(8)? * 1000,
        13 => bits.read(16)?,
        14 => bits.read(16)? * 10,
        _ => return Err(ProbeError::Invalid),
    };
    let width = match width_code {
        0 => info.width,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return Err(ProbeError::Invalid),
    };
    let channels = match channel_code {
        0..=7 => channel_code + 1,
        8..=10 => 2,
        _ => return Err(ProbeError::Invalid),
    };
    require(rate == info.rate && width == info.width && channels == info.channels)?;
    let header_crc = bits.header_crc.take().ok_or(ProbeError::Invalid)?;
    require(bits.read(8)? == u32::from(header_crc))?;
    for channel in 0..channels {
        let side =
            (channel_code == 9 && channel == 0) || (matches!(channel_code, 8 | 10) && channel == 1);
        subframe(bits, block, width + u32::from(side))?;
    }
    if bits.used != 0 {
        require(bits.read(u32::from(8 - bits.used))? == 0)?;
    }
    bits.read(16)?;
    let length = bits.offset - start;
    require(bits.frame_crc == 0 && (info.frame_min == 0 || length >= info.frame_min))?;
    Ok((block, variable))
}

fn utf_number<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    bits: &mut Bits<'_, '_, R, T>,
) -> PResult<u64> {
    let first = bits.read(8)? as u8;
    if first < 128 {
        return Ok(u64::from(first));
    }
    let bytes = first.leading_ones();
    require((2..=7).contains(&bytes))?;
    let mut number = u64::from(first & (0x7f >> bytes));
    for _ in 1..bytes {
        let next = bits.read(8)?;
        require(next & 0xc0 == 0x80)?;
        number = (number << 6) | u64::from(next & 63);
    }
    let minimum = [0, 0, 1 << 7, 1 << 11, 1 << 16, 1 << 21, 1 << 26, 1 << 31][bytes as usize];
    require(number >= minimum)?;
    Ok(number)
}

fn subframe<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    bits: &mut Bits<'_, '_, R, T>,
    block: u32,
    mut width: u32,
) -> PResult<()> {
    require(bits.read(1)? == 0)?;
    let kind = bits.read(6)?;
    if bits.read(1)? != 0 {
        let wasted = 1 + bits.zero_run(width - 2)?;
        width -= wasted;
    }
    let order = match kind {
        0 => return bits.skip(u64::from(width)),
        1 => return bits.skip(u64::from(width) * u64::from(block)),
        8..=12 => kind - 8,
        32..=63 => kind - 31,
        _ => return Err(ProbeError::Invalid),
    };
    require(order < block)?;
    bits.skip(u64::from(order * width))?;
    if kind >= 32 {
        let precision = bits.read(4)? + 1;
        require(precision <= 15 && bits.read(5)? < 16)?;
        bits.skip(u64::from(order * precision))?;
    }
    let param_width = match bits.read(2)? {
        0 => 4,
        1 => 5,
        _ => return Err(ProbeError::Invalid),
    };
    let partitions = 1 << bits.read(4)?;
    require(block % partitions == 0 && block / partitions > order)?;
    let partition_samples = block / partitions;
    for partition in 0..partitions {
        let samples = partition_samples - if partition == 0 { order } else { 0 };
        let parameter = bits.read(param_width)?;
        if parameter == (1 << param_width) - 1 {
            let raw_width = bits.read(5)?;
            bits.skip(u64::from(samples * raw_width))?;
        } else {
            for _ in 0..samples {
                let quotient = bits.zero_run(u32::MAX >> parameter)?;
                let remainder = bits.read(parameter)?;
                require(((quotient << parameter) | remainder) != u32::MAX)?;
            }
        }
    }
    Ok(())
}

struct Bits<'a, 'r, R, T> {
    probe: &'a mut Probe<'r, R, T>,
    budget: &'a mut Budget,
    buffer: [u8; 8192],
    base: u64,
    available: usize,
    offset: u64,
    limit: u64,
    used: u8,
    header_crc: Option<u8>,
    frame_crc: u16,
}

impl<'a, 'r, R: Read + Seek, T: FnMut() -> Result<(), String>> Bits<'a, 'r, R, T> {
    fn new(probe: &'a mut Probe<'r, R, T>, budget: &'a mut Budget, offset: u64) -> Self {
        let limit = probe.limit;
        Self {
            probe,
            budget,
            buffer: [0; 8192],
            base: offset,
            available: 0,
            offset,
            limit,
            used: 0,
            header_crc: None,
            frame_crc: 0,
        }
    }

    fn begin_frame(&mut self, size: u64) {
        self.frame_crc = 0;
        self.header_crc = Some(0);
        self.limit = if size == 0 {
            self.probe.limit
        } else {
            self.probe.limit.min(self.offset + size)
        };
    }

    fn byte(&mut self) -> PResult<u8> {
        require(self.offset < self.limit)?;
        let index = (self.offset - self.base) as usize;
        if index < self.available {
            return Ok(self.buffer[index]);
        }
        let count = (self.probe.limit - self.offset).min(self.buffer.len() as u64) as usize;
        self.budget
            .charge(count as u64)
            .map_err(ProbeError::Failed)?;
        self.probe.read_at(self.offset, &mut self.buffer[..count])?;
        self.base = self.offset;
        self.available = count;
        Ok(self.buffer[0])
    }

    fn read(&mut self, mut count: u32) -> PResult<u32> {
        let mut result = 0;
        while count > 0 {
            let byte = self.byte()?;
            let left = u32::from(8 - self.used);
            let take = count.min(left);
            result = (result << take) | (u32::from(byte) >> (left - take)) & ((1 << take) - 1);
            self.consume(byte, take as u8);
            count -= take;
        }
        Ok(result)
    }

    fn skip(&mut self, mut count: u64) -> PResult<()> {
        require(count <= 8 * (self.limit - self.offset) - u64::from(self.used))?;
        while count != 0 {
            let byte = self.byte()?;
            let take = count.min(u64::from(8 - self.used)) as u8;
            self.consume(byte, take);
            count -= u64::from(take);
        }
        Ok(())
    }

    fn zero_run(&mut self, max: u32) -> PResult<u32> {
        let mut total = 0u32;
        loop {
            let byte = self.byte()?;
            let left = u32::from(8 - self.used);
            let zeros = (byte << self.used).leading_zeros().min(left);
            total = total
                .checked_add(zeros)
                .filter(|n| *n <= max)
                .ok_or(ProbeError::Invalid)?;
            let terminator = zeros < left;
            self.consume(byte, zeros as u8 + u8::from(terminator));
            if terminator {
                return Ok(total);
            }
        }
    }

    fn consume(&mut self, byte: u8, bits: u8) {
        self.used += bits;
        if self.used == 8 {
            self.offset += 1;
            self.used = 0;
            self.frame_crc =
                (self.frame_crc << 8) ^ CRC16[usize::from((self.frame_crc >> 8) as u8 ^ byte)];
            if let Some(crc) = &mut self.header_crc {
                *crc = CRC8[usize::from(*crc ^ byte)];
            }
        }
    }
}

const fn crc_table<const POLY: u16, const TOP: u16, const SHIFT: u32>() -> [u16; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = (index as u16) << SHIFT;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1) ^ if crc & TOP != 0 { POLY } else { 0 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

const CRC16: [u16; 256] = crc_table::<0x8005, 0x8000, 8>();
const CRC8_WIDE: [u16; 256] = crc_table::<0x07, 0x80, 0>();
const CRC8: [u8; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        table[i] = CRC8_WIDE[i] as u8;
        i += 1;
    }
    table
};

fn require(valid: bool) -> PResult<()> {
    valid.then_some(()).ok_or(ProbeError::Invalid)
}

//! Bounded XPRESS Huffman and 32 KiB WIM LZX decoding for NTFS file-provider chunks.
//! XPRESS follows MS-XCA §2.2. LZX uses the WIM block headers and chunk-local E8
//! translation described by wimlib's format notes.

const INVALID: &str = "WOF 压缩区块无效或不完整。";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Xpress4k,
    Lzx,
    Xpress8k,
    Xpress16k,
}

impl Algorithm {
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(Self::Xpress4k),
            1 => Some(Self::Lzx),
            2 => Some(Self::Xpress8k),
            3 => Some(Self::Xpress16k),
            _ => None,
        }
    }

    pub fn chunk_size(self) -> usize {
        match self {
            Self::Xpress4k => 4096,
            Self::Lzx => 32768,
            Self::Xpress8k => 8192,
            Self::Xpress16k => 16384,
        }
    }

    pub fn decode(self, input: &[u8], output: &mut [u8]) -> Result<(), &'static str> {
        if input.is_empty() || input.len() > output.len() || output.len() > self.chunk_size() {
            return Err(INVALID);
        }
        if input.len() == output.len() {
            output.copy_from_slice(input);
            return Ok(());
        }
        match self {
            Self::Lzx => lzx(input, output),
            _ => xpress(input, output),
        }
    }
}

type Result<T, E = &'static str> = std::result::Result<T, E>;

struct Bits<'a> {
    input: &'a [u8],
    value: u32,
    available: u8,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            value: 0,
            available: 0,
        }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = self.input.get(..len).ok_or(INVALID)?;
        self.input = &self.input[len..];
        Ok(bytes)
    }

    fn ensure(&mut self, count: u8) -> Result<()> {
        if self.available < count {
            let word = u16::from_le_bytes(self.bytes(2)?.try_into().unwrap());
            self.value = (self.value << 16) | u32::from(word);
            self.available += 16;
        }
        Ok(())
    }

    fn read(&mut self, count: u8) -> Result<usize> {
        self.ensure(count)?;
        self.available -= count;
        Ok(((self.value >> self.available) & ((1 << count) - 1)) as usize)
    }

    fn align_for_lzx_raw_block(&mut self) -> Result<()> {
        self.ensure(1)?;
        self.available = 0;
        Ok(())
    }
}

struct Huffman {
    counts: [u16; 17],
    first_code: [u32; 17],
    first_symbol: [usize; 17],
    symbols: Vec<u16>,
    max: u8,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
        let max = lengths.iter().copied().max().unwrap_or(0);
        if max > 16 {
            return Err(INVALID);
        }
        let mut counts = [0u16; 17];
        for &len in lengths.iter().filter(|&&n| n != 0) {
            counts[usize::from(len)] += 1;
        }
        let mut first_code = [0; 17];
        let mut first_symbol = [0; 17];
        for len in 1..=usize::from(max) {
            first_code[len] = (first_code[len - 1] + u32::from(counts[len - 1])) * 2;
            if first_code[len] + u32::from(counts[len]) > 1 << len {
                return Err(INVALID);
            }
            first_symbol[len] = first_symbol[len - 1] + usize::from(counts[len - 1]);
        }
        let mut symbols = vec![0; counts.iter().map(|&n| usize::from(n)).sum()];
        let mut next = first_symbol;
        for (symbol, &len) in lengths.iter().enumerate().filter(|&(_, &n)| n != 0) {
            let at = &mut next[usize::from(len)];
            symbols[*at] = symbol as u16;
            *at += 1;
        }
        Ok(Self {
            counts,
            first_code,
            first_symbol,
            symbols,
            max,
        })
    }

    fn read(&self, bits: &mut Bits<'_>) -> Result<usize> {
        let mut code = 0u32;
        for len in 1..=usize::from(self.max) {
            code = code * 2 + bits.read(1)? as u32;
            let index = code.wrapping_sub(self.first_code[len]);
            if index < u32::from(self.counts[len]) {
                return Ok(usize::from(
                    self.symbols[self.first_symbol[len] + index as usize],
                ));
            }
        }
        Err(INVALID)
    }
}

fn copy_match(out: &mut [u8], position: &mut usize, distance: usize, count: usize) -> Result<()> {
    if distance == 0 || distance > *position || count > out.len() - *position {
        return Err(INVALID);
    }
    for index in *position..*position + count {
        out[index] = out[index - distance];
    }
    *position += count;
    Ok(())
}

fn xpress(input: &[u8], out: &mut [u8]) -> Result<()> {
    let (header, body) = input.split_at_checked(256).ok_or(INVALID)?;
    let lengths: Vec<_> = header
        .iter()
        .flat_map(|&byte| [byte & 15, byte >> 4])
        .collect();
    let codes = Huffman::new(&lengths)?;
    let mut bits = Bits::new(body);
    let mut position = 0;
    while position < out.len() {
        let symbol = codes.read(&mut bits)?;
        if symbol < 256 {
            out[position] = symbol as u8;
            position += 1;
            continue;
        }
        let token = symbol - 256;
        // The length bytes are stored past the next prefetched coding word.
        bits.ensure(16)?;
        let distance_bits = (token >> 4) as u8;
        let distance = (1 << distance_bits) + bits.read(distance_bits)?;
        let mut count = token & 15;
        if count == 15 {
            count += usize::from(bits.bytes(1)?[0]);
            if count == 270 {
                count = usize::from(u16::from_le_bytes(bits.bytes(2)?.try_into().unwrap()));
                if count < 15 {
                    return Err(INVALID);
                }
            }
        }
        copy_match(out, &mut position, distance, count + 3)?;
    }
    Ok(())
}

fn update_lengths(bits: &mut Bits<'_>, lengths: &mut [u8]) -> Result<()> {
    let mut precode = [0; 20];
    for len in &mut precode {
        *len = bits.read(4)? as u8;
    }
    let codes = Huffman::new(&precode)?;
    let mut position = 0;
    while position < lengths.len() {
        let (count, value) = match codes.read(bits)? {
            delta @ 0..=16 => (1, (usize::from(lengths[position]) + 17 - delta) % 17),
            17 => (4 + bits.read(4)?, 0),
            18 => (20 + bits.read(5)?, 0),
            19 => {
                let count = 4 + bits.read(1)?;
                let delta = codes.read(bits)?;
                if delta > 16 {
                    return Err(INVALID);
                }
                (count, (usize::from(lengths[position]) + 17 - delta) % 17)
            }
            _ => return Err(INVALID),
        };
        lengths
            .get_mut(position..position + count)
            .ok_or(INVALID)?
            .fill(value as u8);
        position += count;
    }
    Ok(())
}

fn lzx(input: &[u8], out: &mut [u8]) -> Result<()> {
    let mut bits = Bits::new(input);
    let mut recent = [1usize; 3];
    let mut main_lengths = [0; 256 + 30 * 8];
    let mut match_lengths = [0; 249];
    let mut position = 0;
    while position < out.len() {
        let kind = bits.read(3)?;
        let block_size = if bits.read(1)? != 0 {
            32768
        } else {
            bits.read(16)?
        };
        let end = position + block_size;
        if !(1..=3).contains(&kind) || block_size == 0 || end > out.len() {
            return Err(INVALID);
        }
        if kind == 3 {
            bits.align_for_lzx_raw_block()?;
            for distance in &mut recent {
                *distance = u32::from_le_bytes(bits.bytes(4)?.try_into().unwrap()) as usize;
                if !(1..32768).contains(distance) {
                    return Err(INVALID);
                }
            }
            out[position..end].copy_from_slice(bits.bytes(block_size)?);
            bits.bytes(block_size % 2)?;
            position = end;
            continue;
        }
        let mut aligned_lengths = [0; 8];
        if kind == 2 {
            for len in &mut aligned_lengths {
                *len = bits.read(3)? as u8;
            }
        }
        let aligned = Huffman::new(&aligned_lengths)?;
        update_lengths(&mut bits, &mut main_lengths[..256])?;
        update_lengths(&mut bits, &mut main_lengths[256..])?;
        update_lengths(&mut bits, &mut match_lengths)?;
        let main = Huffman::new(&main_lengths)?;
        let lengths = Huffman::new(&match_lengths)?;
        while position < end {
            let symbol = main.read(&mut bits)?;
            if symbol < 256 {
                out[position] = symbol as u8;
                position += 1;
                continue;
            }
            let (slot, length_code) = ((symbol - 256) / 8, symbol % 8);
            let length = length_code
                + 2
                + if length_code == 7 {
                    lengths.read(&mut bits)?
                } else {
                    0
                };
            let distance = if slot < 3 {
                recent.swap(0, slot);
                recent[0]
            } else {
                let extra = (slot / 2 - 1) as u8;
                let base = ((2 + slot % 2) << extra) - 2;
                let delta = if kind == 2 && extra >= 3 {
                    (bits.read(extra - 3)? << 3) + aligned.read(&mut bits)?
                } else {
                    bits.read(extra)?
                };
                let distance = base + delta;
                recent = [distance, recent[0], recent[1]];
                distance
            };
            copy_match(&mut out[..end], &mut position, distance, length)?;
        }
    }
    undo_e8(out);
    Ok(())
}

fn undo_e8(out: &mut [u8]) {
    const TRANSLATION_SIZE: i32 = 12_000_000;
    let mut position = 0;
    while position < out.len().saturating_sub(10) {
        if out[position] != 0xe8 {
            position += 1;
            continue;
        }
        let start = position as i32;
        let operand = &mut out[position + 1..position + 5];
        let target = i32::from_le_bytes(operand.try_into().unwrap());
        if (-start..TRANSLATION_SIZE).contains(&target) {
            let adjustment = if target >= 0 {
                -start
            } else {
                TRANSLATION_SIZE
            };
            operand.copy_from_slice(&(target + adjustment).to_le_bytes());
        }
        position += 5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn chunk_bounds_and_corrupt_code_tables_are_rejected() {
        let mut out = [0; 4096];
        assert!(Algorithm::Xpress4k.decode(&[0x11; 258], &mut out).is_err());
        assert!(Algorithm::Xpress4k.decode(&[0; 256], &mut out).is_err());
        assert!(Algorithm::Lzx.decode(&[0, 0], &mut out).is_err());
        assert!(Algorithm::Xpress4k.decode(&[], &mut out).is_err());
        assert!(Algorithm::Xpress4k.decode(&[0; 4097], &mut out).is_err());
        let mut tail = [0; 3];
        Algorithm::Lzx.decode(b"end", &mut tail).unwrap();
        assert_eq!(&tail, b"end");
    }

    #[test]
    fn wimlib_chunks_decode_to_the_independent_logical_digests() {
        let fixtures: &[(&[u8], &str)] = &[
            (
                include_bytes!("../../../tests/fixtures/ntfs-wof-0.bin"),
                "eba995b04d57e07ea2b6361c46cc5ebfe307341b053878b3b2ea7bc0f0306d29",
            ),
            (
                include_bytes!("../../../tests/fixtures/ntfs-wof-1.bin"),
                "71d72e65e73264f27427adfdf647354f499073575a22d3baec1f0f206abb33e9",
            ),
            (
                include_bytes!("../../../tests/fixtures/ntfs-wof-2.bin"),
                "772913106eca2694b0a47213234be33623d0762a97f200926fc8c727c0a1daef",
            ),
            (
                include_bytes!("../../../tests/fixtures/ntfs-wof-3.bin"),
                "41e3cd510105ae12bbae29e2e71f26b2c91cf21334a763d86a454bf86e054989",
            ),
        ];
        for (id, &(stored, expected)) in fixtures.iter().enumerate() {
            let codec = Algorithm::from_id(id as u32).unwrap();
            let mut logical = vec![0; 196731];
            let count = logical.len().div_ceil(codec.chunk_size());
            let payload = &stored[(count - 1) * 4..];
            let mut previous = 0;
            for (index, output) in logical.chunks_mut(codec.chunk_size()).enumerate() {
                let end = if index + 1 == count {
                    payload.len()
                } else {
                    u32::from_le_bytes(stored[index * 4..index * 4 + 4].try_into().unwrap())
                        as usize
                };
                codec
                    .decode(&payload[previous..end], output)
                    .unwrap_or_else(|e| panic!("{codec:?}, chunk {index}: {e}"));
                previous = end;
            }
            assert_eq!(
                format!("{:x}", Sha256::digest(logical)),
                expected,
                "{codec:?}"
            );
        }
    }
}

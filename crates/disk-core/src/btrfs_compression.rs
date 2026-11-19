//! Bounded decoding of Btrfs inline and regular compressed extents.
pub(super) const LIMIT: usize = 128 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Codec {
    Zlib,
    Lzo,
    Zstd,
}

impl Codec {
    pub(super) fn from_byte(value: u8) -> Result<Option<Self>, String> {
        Ok(match value {
            0 => None,
            1 => Some(Self::Zlib),
            2 => Some(Self::Lzo),
            3 => Some(Self::Zstd),
            _ => return Err("Btrfs 压缩算法暂不支持。".into()),
        })
    }

    pub(super) fn decode(
        self,
        input: &[u8],
        size: usize,
        sector: usize,
        inline: bool,
    ) -> Result<Vec<u8>, String> {
        if input.is_empty() || input.len() > LIMIT || !(1..=LIMIT).contains(&size) {
            return Err("Btrfs 压缩范围超过上限或为空。".into());
        }
        let mut output = vec![0; size + 1];
        let (consumed, written) = match self {
            Self::Zlib => {
                let mut decoder = flate2::Decompress::new(true);
                let result = decoder
                    .decompress(input, &mut output, flate2::FlushDecompress::Finish)
                    .map_err(|e| e.to_string())?;
                if result != flate2::Status::StreamEnd {
                    return Err("Btrfs zlib 流未结束或解压长度过大。".into());
                }
                (decoder.total_in() as usize, decoder.total_out() as usize)
            }
            Self::Zstd => {
                if !input.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                    return Err("Btrfs Zstd 帧标记无效。".into());
                }
                let len = zstd::zstd_safe::find_frame_compressed_size(input)
                    .map_err(|_| "Btrfs Zstd 帧长度无效。")?;
                let mut decoder = zstd::bulk::Decompressor::new().map_err(|e| e.to_string())?;
                decoder.window_log_max(17).map_err(|e| e.to_string())?;
                let written = decoder
                    .decompress_to_buffer(&input[..len], output.as_mut_slice())
                    .map_err(|e| e.to_string())?;
                (len, written)
            }
            Self::Lzo => decode_lzo(input, &mut output[..size], sector, inline)?,
        };
        let expected_input = if inline {
            consumed
        } else {
            consumed.div_ceil(sector) * sector
        };
        // Linux can stop compressing at EOF while ram_bytes records the
        // sector-aligned length. Only the last logical sector can be short.
        let expected_output = if inline {
            written
        } else {
            written.div_ceil(sector) * sector
        };
        if written == 0
            || expected_output != size
            || input.len() != expected_input
            || input[consumed..].iter().any(|&b| b != 0)
        {
            return Err("Btrfs 压缩范围的解压长度或扇区填充无效。".into());
        }
        output[written..size].fill(0);
        output.truncate(size);
        Ok(output)
    }
}

fn length(bytes: &[u8], at: usize) -> Result<usize, String> {
    bytes
        .get(at..at + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(|bytes| u32::from_le_bytes(bytes) as usize)
        .ok_or_else(|| "Btrfs LZO 头被截断。".into())
}

fn decode_lzo(
    input: &[u8],
    output: &mut [u8],
    sector: usize,
    inline: bool,
) -> Result<(usize, usize), String> {
    let total = length(input, 0)?;
    let segments = input
        .get(..total)
        .filter(|bytes| bytes.len() > 8)
        .ok_or("Btrfs LZO 总长度无效。")?;
    let mut at = 4;
    let mut written = 0;
    while at < total {
        if written == output.len() || (inline && at != 4) {
            return Err("Btrfs LZO 段数量超出解压范围。".into());
        }
        let len = length(segments, at)?;
        at += 4;
        if len == 0 || len > sector + sector / 16 + 67 {
            return Err("Btrfs LZO 段长度无效。".into());
        }
        let segment = segments.get(at..at + len).ok_or("Btrfs LZO 段被截断。")?;
        let expected = (output.len() - written).min(sector);
        let count = lzo::decompress_into(segment, &mut output[written..written + expected])
            .map_err(|e| e.to_string())?;
        written += count;
        at += len;
        let available = sector - at % sector;
        if !inline && at < total && available < 4 {
            let padding = segments
                .get(at..at + available)
                .ok_or("Btrfs LZO 扇区填充被截断。")?;
            if padding.iter().any(|&byte| byte != 0) {
                return Err("Btrfs LZO 扇区填充无效。".into());
            }
            at += available;
        }
        if count == 0 || (count != expected && at < total) {
            return Err("Btrfs LZO 段与扇区解压长度不一致。".into());
        }
    }
    Ok((total, written))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn decodes_kernel_extent_encodings_and_rejects_bad_padding_and_lengths() {
        let input = [b'a'; 4096];
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&input).unwrap();
        let zlib = encoder.finish().unwrap();
        let zstd = zstd::bulk::compress(&input, 1).unwrap();
        for (codec, mut bytes) in [(Codec::Zlib, zlib), (Codec::Zstd, zstd)] {
            assert_eq!(codec.decode(&bytes, 4096, 4096, true).unwrap(), input);
            assert!(codec.decode(&bytes, 4095, 4096, true).is_err());
            bytes.resize(4096, 0);
            assert_eq!(codec.decode(&bytes, 4096, 4096, false).unwrap(), input);
            bytes[4095] = 1;
            assert!(codec.decode(&bytes, 4096, 4096, false).is_err());
        }
        let lzo = [15, 0, 0, 0, 7, 0, 0, 0, 20, b'a', b'b', b'c', 17, 0, 0];
        assert_eq!(Codec::Lzo.decode(&lzo, 3, 4096, true).unwrap(), b"abc");
        assert!(Codec::Lzo.decode(&lzo, 2, 4096, true).is_err());
        assert!(Codec::Lzo.decode(&lzo[..14], 3, 4096, true).is_err());
    }

    #[test]
    fn regular_extents_zero_fill_only_the_last_partial_sector() {
        let data = b"abc";
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        let lzo = vec![15, 0, 0, 0, 7, 0, 0, 0, 20, b'a', b'b', b'c', 17, 0, 0];
        let codecs = [
            (Codec::Zlib, encoder.finish().unwrap()),
            (Codec::Zstd, zstd::bulk::compress(data, 1).unwrap()),
            (Codec::Lzo, lzo),
        ];
        let mut expected = vec![0; 4096];
        expected[..data.len()].copy_from_slice(data);
        for (codec, mut compressed) in codecs {
            assert!(codec.decode(&compressed, 4, 4096, true).is_err());
            compressed.resize(4096, 0);
            assert_eq!(
                codec.decode(&compressed, 4096, 4096, false).unwrap(),
                expected
            );
            assert!(codec.decode(&compressed, 8192, 4096, false).is_err());
        }

        let mut short_middle = 26u32.to_le_bytes().to_vec();
        for _ in 0..2 {
            short_middle.extend([7, 0, 0, 0, 20, b'a', b'b', b'c', 17, 0, 0]);
        }
        short_middle.resize(4096, 0);
        assert!(Codec::Lzo.decode(&short_middle, 8192, 4096, false).is_err());
    }
}

// Pure bounded byte decoding for imported VMDK grains. No paths or device I/O.
use flate2::{Decompress, FlushDecompress, Status};
pub fn inflate_grain(bytes: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    if !(8192..=1024 * 1024).contains(&expected)
        || !expected.is_power_of_two()
        || bytes.is_empty()
        || bytes.len() > expected * 2
    {
        return Err("VMDK 压缩输入或输出容量超限。".into());
    }
    // One spare byte detects output longer than declared without unbounded
    // decompression, and StreamEnd plus total_in rejects truncated/trailing data.
    let mut output = vec![0; expected + 1];
    let mut decoder = Decompress::new(true);
    let status = decoder
        .decompress(bytes, &mut output, FlushDecompress::Finish)
        .map_err(|_| "VMDK 压缩数据或校验和损坏。".to_string())?;
    if status != Status::StreamEnd
        || decoder.total_in() != bytes.len() as u64
        || decoder.total_out() != expected as u64
    {
        return Err("VMDK 压缩流不完整、含多余字节或解压长度不符。".into());
    }
    output.truncate(expected);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{read::GzDecoder, write::ZlibEncoder, Compression};
    use std::io::{Read, Write};
    const QEMU: &[u8] = include_bytes!("../../tests/fixtures/vmdk-zlib-grain.bin");
    fn compressed(n: usize) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&vec![0x5a; n]).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn qemu_zlib_grain_matches_independent_raw_source() {
        let mut raw = Vec::new();
        GzDecoder::new(include_bytes!("../../tests/fixtures/vmdk-raw.gz").as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(inflate_grain(QEMU, 65536).unwrap(), raw[..65536]);
        for n in [8192, 65536, 1024 * 1024] {
            assert_eq!(inflate_grain(&compressed(n), n).unwrap(), vec![0x5a; n]);
        }
    }
    #[test]
    fn rejects_truncation_checksum_failure_concatenation_and_trailing_bytes() {
        for n in [1, QEMU.len() / 2, QEMU.len() - 1] {
            assert!(inflate_grain(&QEMU[..n], 65536).is_err());
        }
        let mut bad = QEMU.to_vec();
        *bad.last_mut().unwrap() ^= 1;
        assert!(inflate_grain(&bad, 65536).is_err());
        for suffix in [&[0u8][..], QEMU] {
            let mut joined = QEMU.to_vec();
            joined.extend_from_slice(suffix);
            assert!(inflate_grain(&joined, 65536).is_err());
        }
    }
    #[test]
    fn bounds_inputs_and_expansion_before_allocating_unbounded_output() {
        for n in [0, 8191, 8193, 1024 * 1024 + 1, usize::MAX] {
            assert!(inflate_grain(QEMU, n).is_err());
        }
        assert!(inflate_grain(&[], 8192).is_err());
        assert!(inflate_grain(&vec![0; 16385], 8192).is_err());
        for n in [8191, 8193, 4 * 1024 * 1024] {
            assert!(inflate_grain(&compressed(n), 8192).is_err());
        }
    }
}

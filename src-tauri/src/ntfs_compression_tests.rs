use super::*;
use crate::recovery::{self, Candidate, ScanMode};
use std::io::Cursor;
const ORIGINAL: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
const CLUSTER: usize =
    (ORIGINAL[11] as usize + ((ORIGINAL[12] as usize) << 8)) * ORIGINAL[13] as usize;
const UNIT: usize = CLUSTER * 16;
const PAYLOAD: usize = 0x180000;

#[cfg(windows)]
#[link(name = "ntdll")]
extern "system" {
    fn RtlGetCompressionWorkSpaceSize(format: u16, compress: *mut u32, fragment: *mut u32) -> i32;
    fn RtlCompressBuffer(
        format: u16,
        input: *const u8,
        size: u32,
        output: *mut u8,
        capacity: u32,
        chunk: u32,
        final_size: *mut u32,
        workspace: *mut std::ffi::c_void,
    ) -> i32;
}
// Windows' codec is used only as an independent in-memory test oracle.
#[cfg(windows)]
fn windows_pack(bytes: &[u8]) -> Vec<u8> {
    let (mut work, mut fragment) = (0, 0);
    unsafe {
        assert_eq!(
            RtlGetCompressionWorkSpaceSize(2, &mut work, &mut fragment),
            0
        );
    }
    let mut workspace = vec![0u64; (work as usize).div_ceil(8)];
    let mut packed = vec![0; bytes.len() * 2 + 4096];
    let mut len = 0;
    let status = unsafe {
        RtlCompressBuffer(
            2,
            bytes.as_ptr(),
            bytes.len() as u32,
            packed.as_mut_ptr(),
            packed.len() as u32,
            4096,
            &mut len,
            workspace.as_mut_ptr().cast(),
        )
    };
    assert!(status >= 0, "codec status {status:x}");
    packed.truncate(len as usize);
    packed
}
fn repeated(byte: u8, chunks: usize) -> Vec<u8> {
    // One literal and an overlapping length-4095 reference, per 4 KiB chunk.
    (0..chunks)
        .flat_map(|_| [3, 0xb0, 2, byte, 0xfc, 0x0f])
        .collect()
}
#[test]
fn lznt1_literal_overlap_chunk_boundaries_and_official_vector() {
    assert_eq!(
        lznt1(&repeated(b'A', 16), 65536).unwrap(),
        vec![b'A'; 65536]
    );
    let literal = [2, 0x30, b'a', b'b', b'c'];
    assert_eq!(lznt1(&literal, 4096).unwrap(), b"abc");
    // Microsoft MS-XCA 3.3 public interoperability example (59 encoded bytes).
    let encoded = [
        0x38, 0xb0, 0x88, 0x46, 0x23, 0x20, 0x00, 0x20, 0x47, 0x20, 0x41, 0x00, 0x10, 0xa2, 0x47,
        0x01, 0xa0, 0x45, 0x20, 0x44, 0x00, 0x08, 0x45, 0x01, 0x50, 0x79, 0x00, 0xc0, 0x45, 0x20,
        0x05, 0x24, 0x13, 0x88, 0x05, 0xb4, 0x02, 0x4a, 0x44, 0xef, 0x03, 0x58, 0x02, 0x8c, 0x09,
        0x16, 0x01, 0x48, 0x45, 0x00, 0xbe, 0x00, 0x9e, 0x00, 0x04, 0x01, 0x18, 0x90, 0x00,
    ];
    let decoded = lznt1(&encoded, 4096).unwrap();
    assert_eq!(decoded.len(), 142);
    assert!(decoded.starts_with(b"F# F# G A A G F#"));
    assert_eq!(decoded.last(), Some(&0));
}
#[cfg(windows)]
#[test]
fn decoder_matches_windows_codec_for_lengths_and_incompressible_patterns() {
    for size in [
        1, 15, 16, 17, 31, 32, 33, 255, 256, 257, 4095, 4096, 4097, 8192, 65536,
    ] {
        for pattern in 0..3 {
            let mut seed = 0x734af27du32;
            let bytes: Vec<u8> = (0..size)
                .map(|i| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    match pattern {
                        0 => b'X',
                        1 => (i % 97) as u8,
                        _ => seed as u8,
                    }
                })
                .collect();
            let packed = windows_pack(&bytes);
            assert_eq!(
                lznt1(&packed, 65536).unwrap(),
                bytes,
                "size {size} pattern {pattern}"
            );
        }
    }
}
#[test]
fn decoder_rejects_truncation_bad_signature_reference_and_oversized_output() {
    let good = repeated(b'Z', 1);
    for n in 1..good.len() {
        assert!(lznt1(&good[..n], 4096).is_err());
    }
    for invalid in [
        &[0, 0x20, 0][..],
        &[2, 0xb0, 1, 0, 0][..],
        &[1, 0xb0, 1, 0][..],
        &[0, 0xb0, 0][..],
    ] {
        assert!(lznt1(invalid, 4096).is_err());
    }
    assert!(lznt1(&good, 4095).is_err());
    let mut all = good.clone();
    all.extend_from_slice(&good);
    assert!(lznt1(&all, 4096).is_err());
    for b in 0..=255u8 {
        let input = vec![b; 4096];
        let _ = lznt1(&input, 65536);
    }
}

fn record_named(name: &str) -> (u64, usize, Vec<u8>) {
    let mut reader = Cursor::new(ORIGINAL);
    let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
    for n in 16..1024 {
        if let Ok(file) = ntfs.file(&mut reader, n) {
            if file
                .name(&mut reader, None, None)
                .is_some_and(|n| n.is_ok_and(|n| n.name().to_string_lossy() == name))
            {
                let at = file.position().value().unwrap().get() as usize;
                let mut header = ORIGINAL[at..at + ntfs.file_record_size() as usize].to_vec();
                let usa = usize::from(u16le(&header, 4));
                let sector = usize::from(ntfs.sector_size());
                for i in 1..usize::from(u16le(&header, 6)) {
                    let tail = i * sector - 2;
                    header[tail] = header[usa + 2 * i];
                    header[tail + 1] = header[usa + 2 * i + 1];
                }
                return (n, at, header);
            }
        }
    }
    panic!("fixture missing {name}")
}
fn attrs(header: &[u8]) -> Vec<Vec<u8>> {
    let mut at = usize::from(u16le(header, 20));
    let mut out = vec![];
    while header[at..at + 4] != u32::MAX.to_le_bytes() {
        let len = u32::from_le_bytes(header[at + 4..at + 8].try_into().unwrap()) as usize;
        out.push(header[at..at + len].to_vec());
        at += len;
    }
    out
}
fn write_record(bytes: &mut [u8], at: usize, mut header: Vec<u8>, mut attributes: Vec<Vec<u8>>) {
    attributes.sort_by_key(|a| u32::from_le_bytes(a[..4].try_into().unwrap()));
    let start = usize::from(u16le(&header, 20));
    header[start..].fill(0);
    let mut end = start;
    for attr in attributes {
        assert!(end + attr.len() + 8 <= header.len());
        header[end..end + attr.len()].copy_from_slice(&attr);
        end += attr.len();
    }
    header[end..end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    header[24..28].copy_from_slice(&((end + 8) as u32).to_le_bytes());
    let usa = usize::from(u16le(&header, 4));
    let sector = usize::from(u16le(bytes, 11));
    for i in 1..usize::from(u16le(&header, 6)) {
        let tail = i * sector - 2;
        header[usa + 2 * i] = header[tail];
        header[usa + 2 * i + 1] = header[tail + 1];
        header[tail] = header[usa];
        header[tail + 1] = header[usa + 1];
    }
    bytes[at..at + header.len()].copy_from_slice(&header);
}
fn data(low: u64, high: u64, runs: &[u8]) -> Vec<u8> {
    let mut b = vec![0; (72 + runs.len()).div_ceil(8) * 8];
    let len = b.len() as u32;
    b[..4].copy_from_slice(&0x80u32.to_le_bytes());
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[8] = 1;
    b[12] = 1;
    b[14] = 42;
    b[16..24].copy_from_slice(&low.to_le_bytes());
    b[24..32].copy_from_slice(&high.to_le_bytes());
    b[32] = 72;
    b[34] = 4;
    if low == 0 {
        b[40..48].copy_from_slice(&(3 * UNIT as u64).to_le_bytes());
        b[48..56].copy_from_slice(&((3 * UNIT - 7) as u64).to_le_bytes());
        b[56..64].copy_from_slice(&((3 * UNIT - 1024) as u64).to_le_bytes());
        b[64..72].copy_from_slice(&(17 * CLUSTER as u64).to_le_bytes());
    }
    b[72..72 + runs.len()].copy_from_slice(runs);
    b
}
fn fixture(edit: impl FnOnce(&mut Vec<u8>, &mut Vec<Vec<u8>>)) -> (Vec<u8>, Vec<u8>) {
    let mut bytes = ORIGINAL.to_vec();
    let (_, at, mut header) = record_named("file-with-12345");
    let mut attributes = attrs(&header);
    attributes.retain(|a| a[..4] != 0x80u32.to_le_bytes());
    attributes.push(data(0, 47, &mapping(0)));
    edit(&mut header, &mut attributes);
    write_record(&mut bytes, at, header, attributes);
    let packed = repeated(b'X', UNIT / 4096);
    bytes[PAYLOAD..PAYLOAD + CLUSTER].fill(0);
    bytes[PAYLOAD..PAYLOAD + packed.len()].copy_from_slice(&packed);
    let mut expected = vec![b'X'; UNIT];
    let raw: Vec<u8> = (0..UNIT).map(|i| (i % 251) as u8).collect();
    bytes[PAYLOAD + CLUSTER..PAYLOAD + CLUSTER + UNIT].copy_from_slice(&raw);
    expected.extend(raw);
    expected.resize(3 * UNIT - 7, 0);
    (bytes, expected)
}
fn mapping(delta: u16) -> Vec<u8> {
    let lcn = (PAYLOAD / CLUSTER) as u16 + delta;
    vec![
        0x21,
        1,
        lcn as u8,
        (lcn >> 8) as u8,
        0x01,
        15,
        0x11,
        16,
        1,
        0x01,
        16,
        0,
    ]
}
fn candidate(bytes: &[u8]) -> Candidate {
    let mut found = None;
    recovery::scan_filesystem_owned(
        Cursor::new(bytes.to_vec()),
        ScanMode::AllFiles,
        Default::default(),
        |_, _| Ok(()),
        |c| {
            if c.name == "file-with-12345" {
                found = Some(c)
            }
        },
        |_| Ok(()),
    )
    .unwrap();
    found.unwrap()
}
fn extract(bytes: &[u8], c: &Candidate, limit: u64) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    recovery::extract_owned(
        Cursor::new(bytes.to_vec()),
        c,
        &mut out,
        limit,
        Default::default(),
        |_| Ok(()),
    )?;
    Ok(out)
}
#[test]
fn ntfs_scan_preview_and_backup_extract_mixed_compressed_raw_sparse_units() {
    let (bytes, expected) = fixture(|_, _| {});
    let c = candidate(&bytes);
    assert!(c.supported, "{}", c.note);
    assert!(c.note.contains("LZNT1"));
    assert_eq!(extract(&bytes, &c, u64::MAX).unwrap(), expected);
    assert_eq!(
        extract(&bytes, &c, (UNIT + 100) as u64).unwrap(),
        expected[..UNIT + 100]
    );
    let mut packed = Cursor::new(vec![]);
    crate::backup_archive::create(
        &mut &bytes[..],
        &mut packed,
        "partition",
        bytes.len() as u64,
        512,
        true,
        None,
        |_| Ok(()),
    )
    .unwrap();
    let browser = crate::backup_browser::Browser::from_chain(
        crate::backup_archive::Chain::new(
            crate::backup_archive::Archive::from_reader(packed).unwrap(),
        ),
        "compressed.ntfs.dsbackup".into(),
        |_, _| Ok(()),
    )
    .unwrap();
    let mut out = vec![];
    recovery::extract_owned(
        browser.reader(0).unwrap(),
        &c,
        &mut out,
        c.size,
        Default::default(),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(out, expected);
}
#[test]
fn deleted_compressed_file_and_uninitialized_tail_have_explicit_behavior() {
    let (bytes, expected) = fixture(|h, _| h[22..24].copy_from_slice(&0u16.to_le_bytes()));
    let c = candidate(&bytes);
    assert!(c.deleted && c.supported);
    assert_eq!(extract(&bytes, &c, u64::MAX).unwrap(), expected);
    let (bytes, mut expected) = fixture(|_, a| {
        a.iter_mut()
            .find(|a| a[..4] == 0x80u32.to_le_bytes())
            .unwrap()[56..64]
            .copy_from_slice(&100u64.to_le_bytes())
    });
    expected[100..].fill(0);
    assert_eq!(
        extract(&bytes, &candidate(&bytes), u64::MAX).unwrap(),
        expected
    );
}
#[test]
fn source_changes_damage_and_cancellation_fail_without_first_unit_output() {
    let (bytes, _) = fixture(|_, _| {});
    let c = candidate(&bytes);
    for variant in 0..4 {
        let mut altered = bytes.clone();
        match variant {
            0 => altered[72] ^= 1,      // volume serial
            1 => altered[PAYLOAD] ^= 1, // malformed compressed first chunk
            2 => {
                let (_, at, h) = record_named("file-with-12345");
                let mut a = attrs(&h);
                a.retain(|a| a[..4] != 0x80u32.to_le_bytes());
                let mut d = data(0, 47, &mapping(1));
                d[14] = 43;
                a.push(d);
                write_record(&mut altered, at, h, a);
            }
            _ => altered.truncate(PAYLOAD + 2),
        }
        let mut out = vec![];
        assert!(recovery::extract_owned(
            Cursor::new(altered),
            &c,
            &mut out,
            c.size,
            Default::default(),
            |_| Ok(())
        )
        .is_err());
        assert!(out.is_empty(), "variant {variant}");
    }
    let mut out = vec![];
    assert!(recovery::extract_owned(
        Cursor::new(bytes),
        &c,
        &mut out,
        c.size,
        Default::default(),
        |_| Err("cancel".into())
    )
    .is_err());
    assert!(out.is_empty());
}
#[test]
fn invalid_compression_layout_efs_and_reparse_files_are_not_exportable() {
    for variant in 0..8 {
        let (bytes, _) = fixture(|_, a| {
            let d = a
                .iter_mut()
                .find(|a| a[..4] == 0x80u32.to_le_bytes())
                .unwrap();
            match variant {
                0 => d[34] = 3,
                1 => d[13] = 0x40,
                2 => d[72] = 0xf1,
                3 => d[24] = 48,
                4 => d[64] = 1,
                5 => d[82] = 0,
                6 => {
                    let s = a
                        .iter_mut()
                        .find(|a| a[..4] == 0x10u32.to_le_bytes())
                        .unwrap();
                    let at = usize::from(u16le(s, 20));
                    s[at + 33] |= 4;
                }
                7 => d[12] = 2,
                _ => unreachable!(),
            }
        });
        let c = candidate(&bytes);
        assert!(!c.supported, "variant {variant}: {}", c.note);
        assert!(extract(&bytes, &c, u64::MAX).is_err());
    }
    let mut bad = data(
        0,
        47,
        &[0x21, 1, 0x80, 1, 0x01, 15, 0x11, 16, 1, 0x01, 16, 0],
    );
    for n in 0..80 {
        assert!(parse_segment(&bad[..n], 4096, ORIGINAL.len() as u64).is_err());
    }
    bad[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(parse_segment(&bad, 4096, ORIGINAL.len() as u64).is_err());
}

fn attribute_list(entries: &[(u64, u16, u16, u64)]) -> Vec<u8> {
    let mut b = vec![0; 24 + entries.len() * 32];
    let len = b.len() as u32;
    b[..4].copy_from_slice(&0x20u32.to_le_bytes());
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[14] = 41;
    b[16..20].copy_from_slice(&((entries.len() * 32) as u32).to_le_bytes());
    b[20] = 24;
    for (i, (record, seq, instance, vcn)) in entries.iter().enumerate() {
        let e = &mut b[24 + i * 32..24 + (i + 1) * 32];
        e[..4].copy_from_slice(&0x80u32.to_le_bytes());
        e[4] = 32;
        e[8..16].copy_from_slice(&vcn.to_le_bytes());
        e[16..24].copy_from_slice(&(record | (u64::from(*seq) << 48)).to_le_bytes());
        e[24..26].copy_from_slice(&instance.to_le_bytes());
    }
    b
}
#[test]
fn compression_units_across_attribute_list_extents_validate_record_ownership() {
    let (base, base_at, base_header) = record_named("file-with-12345");
    let base_seq = u16le(&base_header, 16);
    let (extension, extension_at, extension_header) = record_named("1");
    let extension_seq = u16le(&extension_header, 16);
    let (original, expected) = fixture(|_, _| {});
    for variant in 0..5 {
        let mut bytes = original.clone();
        let mut base_attrs = attrs(&base_header);
        base_attrs.retain(|a| a[..4] != 0x80u32.to_le_bytes());
        let m = mapping(0);
        let mut first = m[..6].to_vec();
        first.push(0);
        base_attrs.push(data(0, 15, &first));
        base_attrs.push(attribute_list(&[
            (base, base_seq, 42, 0),
            (extension, extension_seq, 42, 16),
        ]));
        write_record(&mut bytes, base_at, base_header.clone(), base_attrs);
        let mut h = extension_header.clone();
        h[22..24].copy_from_slice(&1u16.to_le_bytes());
        h[32..40].copy_from_slice(&(base | u64::from(base_seq) << 48).to_le_bytes());
        let lcn = ((PAYLOAD / CLUSTER) + 1) as u16;
        let mut d = data(16, 47, &[0x21, 16, lcn as u8, (lcn >> 8) as u8, 1, 16, 0]);
        match variant {
            1 => h[32] ^= 1,
            2 => h[16] ^= 1,
            3 => d[16] = 17,
            4 => d[14] = 43,
            _ => {}
        }
        write_record(&mut bytes, extension_at, h, vec![d]);
        if variant == 0 {
            let c = candidate(&bytes);
            assert!(c.supported, "{}", c.note);
            assert_eq!(extract(&bytes, &c, u64::MAX).unwrap(), expected);
        } else {
            let mut reader = Cursor::new(bytes);
            let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
            let file = ntfs.file(&mut reader, base).unwrap();
            assert!(Plan::read(&mut reader, &file).is_err(), "variant {variant}");
        }
    }
}

#[test]
fn fragmented_raw_units_negative_offsets_and_layout_overlap_checks() {
    // The full raw middle unit is split into two physical regions with a
    // backwards signed LCN delta. Both must concatenate in virtual order.
    let lcn = (PAYLOAD / CLUSTER) as u16;
    let runs = [
        0x21,
        1,
        lcn as u8,
        (lcn >> 8) as u8,
        1,
        15,
        0x11,
        8,
        20,
        0x11,
        8,
        0xed,
        1,
        16,
        0,
    ];
    let (mut bytes, expected) = fixture(|_, a| {
        a.retain(|a| a[..4] != 0x80u32.to_le_bytes());
        a.push(data(0, 47, &runs));
    });
    bytes[PAYLOAD + 20 * CLUSTER..PAYLOAD + 28 * CLUSTER]
        .copy_from_slice(&expected[UNIT..UNIT + 8 * CLUSTER]);
    bytes[PAYLOAD + CLUSTER..PAYLOAD + 9 * CLUSTER]
        .copy_from_slice(&expected[UNIT + 8 * CLUSTER..2 * UNIT]);
    let c = candidate(&bytes);
    assert!(c.supported, "{}", c.note);
    assert_eq!(extract(&bytes, &c, u64::MAX).unwrap(), expected);
    for bad in [
        vec![
            0x21,
            1,
            lcn as u8,
            (lcn >> 8) as u8,
            1,
            7,
            0x11,
            8,
            1,
            0x11,
            16,
            8,
            1,
            16,
            0,
        ], // physical data after hole within unit
        vec![
            0x21,
            1,
            lcn as u8,
            (lcn >> 8) as u8,
            1,
            15,
            0x11,
            16,
            0,
            1,
            16,
            0,
        ], // zero delta means physical overlap, not sparse
    ] {
        let (bytes, _) = fixture(|_, a| {
            a.retain(|a| a[..4] != 0x80u32.to_le_bytes());
            a.push(data(0, 47, &bad));
        });
        assert!(!candidate(&bytes).supported);
    }
}

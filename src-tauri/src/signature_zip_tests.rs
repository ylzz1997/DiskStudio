use super::*;
use crate::{
    recovery::{Candidate, Locator},
    signature_scan::{self, Options},
};
use std::io::{self, Cursor, Write};

fn u16(b: &mut Vec<u8>, v: u16) {
    b.extend(v.to_le_bytes());
}
fn u32(b: &mut Vec<u8>, v: u32) {
    b.extend(v.to_le_bytes());
}
fn u64(b: &mut Vec<u8>, v: u64) {
    b.extend(v.to_le_bytes());
}
fn set16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn set32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn build(
    files: &[(&str, &[u8])],
    deflate: bool,
    descriptor: u8,
    wide: bool,
    reverse: bool,
    comment: &[u8],
) -> Vec<u8> {
    let mut b = vec![];
    let mut central = vec![];
    for (name, bytes) in files {
        let packed = if deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        } else {
            bytes.to_vec()
        };
        let offset = b.len() as u64;
        let flags = 0x800 | if descriptor != 0 { 8 } else { 0 };
        let method = if deflate { 8 } else { 0 };
        let version = if wide { 45 } else { 20 };
        let crc = crc32fast::hash(bytes);
        let mut extra = vec![];
        if wide {
            u16(&mut extra, 1);
            u16(&mut extra, 16);
            u64(&mut extra, bytes.len() as u64);
            u64(&mut extra, packed.len() as u64);
        }
        b.extend(b"PK\x03\x04");
        u16(&mut b, version);
        u16(&mut b, flags);
        u16(&mut b, method);
        u32(&mut b, 0);
        u32(&mut b, if descriptor != 0 { 0 } else { crc });
        u32(
            &mut b,
            if wide {
                u32::MAX
            } else if descriptor != 0 {
                0
            } else {
                packed.len() as u32
            },
        );
        u32(
            &mut b,
            if wide {
                u32::MAX
            } else if descriptor != 0 {
                0
            } else {
                bytes.len() as u32
            },
        );
        u16(&mut b, name.len() as u16);
        u16(&mut b, extra.len() as u16);
        b.extend(name.as_bytes());
        b.extend(&extra);
        b.extend(&packed);
        if descriptor != 0 {
            if descriptor == 2 {
                b.extend(b"PK\x07\x08");
            }
            u32(&mut b, crc);
            if wide {
                u64(&mut b, packed.len() as u64);
                u64(&mut b, bytes.len() as u64);
            } else {
                u32(&mut b, packed.len() as u32);
                u32(&mut b, bytes.len() as u32);
            }
        }
        let mut c = b"PK\x01\x02".to_vec();
        u16(&mut c, version);
        u16(&mut c, version);
        u16(&mut c, flags);
        u16(&mut c, method);
        u32(&mut c, 0);
        u32(&mut c, crc);
        u32(&mut c, if wide { u32::MAX } else { packed.len() as u32 });
        u32(&mut c, if wide { u32::MAX } else { bytes.len() as u32 });
        u16(&mut c, name.len() as u16);
        u16(&mut c, if wide { 28 } else { 0 });
        u16(&mut c, 0);
        u16(&mut c, 0);
        u16(&mut c, 0);
        u32(&mut c, 0);
        u32(&mut c, if wide { u32::MAX } else { offset as u32 });
        c.extend(name.as_bytes());
        if wide {
            u16(&mut c, 1);
            u16(&mut c, 24);
            u64(&mut c, bytes.len() as u64);
            u64(&mut c, packed.len() as u64);
            u64(&mut c, offset);
        }
        central.push(c);
    }
    if reverse {
        central.reverse();
    }
    let cd_at = b.len() as u64;
    for c in central {
        b.extend(c);
    }
    let cd_size = b.len() as u64 - cd_at;
    if wide {
        let at = b.len() as u64;
        b.extend(b"PK\x06\x06");
        u64(&mut b, 44);
        u16(&mut b, 45);
        u16(&mut b, 45);
        u32(&mut b, 0);
        u32(&mut b, 0);
        u64(&mut b, files.len() as u64);
        u64(&mut b, files.len() as u64);
        u64(&mut b, cd_size);
        u64(&mut b, cd_at);
        b.extend(b"PK\x06\x07");
        u32(&mut b, 0);
        u64(&mut b, at);
        u32(&mut b, 1);
    }
    b.extend(b"PK\x05\x06");
    u16(&mut b, 0);
    u16(&mut b, 0);
    u16(&mut b, if wide { u16::MAX } else { files.len() as u16 });
    u16(&mut b, if wide { u16::MAX } else { files.len() as u16 });
    u32(&mut b, if wide { u32::MAX } else { cd_size as u32 });
    u32(&mut b, if wide { u32::MAX } else { cd_at as u32 });
    u16(&mut b, comment.len() as u16);
    b.extend(comment);
    b
}
fn scan(bytes: &[u8], kinds: Vec<Kind>) -> Vec<Candidate> {
    let mut found = vec![];
    signature_scan::scan(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        &Options {
            kinds,
            ..Default::default()
        },
        |_, _| Ok(()),
        |c| found.push(c),
    )
    .unwrap();
    found
}
fn inspect_bytes(bytes: &[u8]) -> Option<Archive> {
    let at = bytes.windows(4).rposition(|b| b == b"PK\x05\x06").unwrap();
    probe(
        &mut Cursor::new(bytes),
        at as u64,
        0..bytes.len() as u64,
        &mut Budget::default(),
        || Ok(()),
    )
    .unwrap()
}

#[test]
fn zip_stored_deflate_exact_archive_export_and_changed_middle_rejected() {
    for deflate in [false, true] {
        let payload = vec![0x42; 180_000];
        let archive = build(
            &[("文档.bin", &payload), ("empty", b"")],
            deflate,
            0,
            false,
            false,
            b"comment",
        );
        let mut source = vec![0; 53];
        source.extend(&archive);
        source.extend([0; 27]);
        let found = scan(&source, vec![Kind::Zip]);
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!(c.size, archive.len() as u64);
        assert!(c.name.ends_with(".zip"));
        assert!(matches!(
            c.locator,
            Locator::Signature {
                content_hash: Some(_),
                ..
            }
        ));
        for limit in [7, MAX_FILE] {
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&source), c, &mut out, limit, |_| Ok(()))
                .unwrap();
            assert_eq!(out, archive[..archive.len().min(limit as usize)]);
        }
        source[53 + 45] ^= 1;
        let mut out = vec![];
        assert!(crate::recovery::extract(
            &mut Cursor::new(&source),
            c,
            &mut out,
            MAX_FILE,
            |_| Ok(())
        )
        .unwrap_err()
        .contains("已变化"));
        assert!(out.is_empty());
    }
}
#[test]
fn zip_independent_dotnet_archives_validate_crc_and_export_identical_bytes() {
    for bytes in [
        include_bytes!("../../tests/fixtures/zip-dotnet-NoCompression.zip").as_slice(),
        include_bytes!("../../tests/fixtures/zip-dotnet-Optimal.zip").as_slice(),
    ] {
        let found = scan(bytes, vec![Kind::Zip]);
        assert_eq!(found.len(), 1);
        let mut output = vec![];
        crate::recovery::extract(
            &mut Cursor::new(bytes),
            &found[0],
            &mut output,
            MAX_FILE,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(output, bytes);
    }
}
#[test]
fn zip_descriptors_zip64_reordered_directory_and_comment_are_supported() {
    for wide in [false, true] {
        for descriptor in 0..=2 {
            for deflate in [false, true] {
                let bytes = build(
                    &[("a", b"abc"), ("b", b"DEF")],
                    deflate,
                    descriptor,
                    wide,
                    true,
                    b"comment PK\x05\x06 truncated",
                );
                let found = scan(&bytes, vec![Kind::Zip]);
                assert_eq!(found.len(), 1, "{wide} {descriptor} {deflate}");
                assert_eq!(found[0].size, bytes.len() as u64);
            }
        }
    }
    let empty = build(&[], false, 0, false, false, b"");
    assert_eq!(scan(&empty, vec![Kind::Zip]).len(), 1);
}

#[test]
fn zip_and_empty_zip64_candidates_survive_progress_serialization() {
    for wide in [false, true] {
        for files in [
            vec![],
            vec![("item.txt", b"resume verified archive".as_slice())],
        ] {
            let bytes = build(&files, true, 0, wide, false, b"");
            let found = scan(&bytes, vec![Kind::Zip]);
            assert_eq!(found.len(), 1);
            let record = signature_scan::StoredCandidate::from_candidate(&found[0]).unwrap();
            let record: signature_scan::StoredCandidate =
                serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
            let restored = record
                .candidate(0, 0..bytes.len() as u64, &Options::default())
                .unwrap();
            assert_eq!(restored.size, bytes.len() as u64);
            assert!(matches!(
                restored.locator,
                Locator::Signature {
                    content_hash: Some(_),
                    ..
                }
            ));
        }
    }
}
#[test]
fn zip_corrupt_headers_crc_descriptors_and_unsupported_features_are_rejected() {
    let original = build(&[("a", b"abc")], false, 0, false, false, b"");
    let cd = original
        .windows(4)
        .position(|x| x == b"PK\x01\x02")
        .unwrap();
    let end = original.len() - 22;
    for (at, value) in [
        (31, 0),
        (cd + 16, 0),
        (cd + 46, b'b'),
        (cd + 42, 1),
        (end + 4, 1),
        (end + 10, 2),
        (cd + 8, 1),
        (cd + 10, 99),
        (cd + 6, 99),
        (26, 0),
        (cd + 20, 255),
    ] {
        let mut b = original.clone();
        b[at] = value;
        assert!(scan(&b, vec![Kind::Zip]).is_empty(), "offset {at}");
    }
    let mut b = build(&[("a", b"abc")], true, 2, false, false, b"");
    let d = b.windows(4).position(|x| x == b"PK\x07\x08").unwrap();
    b[d + 4] ^= 1;
    assert!(inspect_bytes(&b).is_none());
    for len in 1..original.len() {
        assert!(scan(&original[..len], vec![Kind::Zip]).is_empty());
    }
    let duplicate = build(
        &[("same", b"a"), ("same", b"b")],
        false,
        0,
        false,
        false,
        b"",
    );
    assert!(inspect_bytes(&duplicate).is_none());
}
#[test]
fn zip_expansion_directory_limits_invalid_zip64_and_deflate_trailing_bytes_fail() {
    let original = build(&[("a", b"hello")], true, 0, false, false, b"");
    let cd = original
        .windows(4)
        .position(|x| x == b"PK\x01\x02")
        .unwrap();
    let end = original.len() - 22;
    let mut b = original.clone();
    set32(&mut b, cd + 24, MAX_EXPANDED as u32 + 1);
    assert!(inspect_bytes(&b).is_none());
    let mut b = original.clone();
    set16(&mut b, end + 8, MAX_ENTRIES as u16 + 1);
    set16(&mut b, end + 10, MAX_ENTRIES as u16 + 1);
    assert!(inspect_bytes(&b).is_none());
    let mut b = original.clone();
    set32(&mut b, end + 12, MAX_DIRECTORY as u32 + 1);
    assert!(inspect_bytes(&b).is_none());
    let mut b = original.clone();
    b[31] = 255;
    assert!(inspect_bytes(&b).is_none());
    // Add an unconsumed byte to the claimed compressed stream, updating all sizes.
    let mut b = original.clone();
    b.insert(cd, 0);
    let packed = le32(&b, 18) + 1;
    set32(&mut b, 18, packed);
    set32(&mut b, cd + 1 + 20, packed);
    set32(&mut b, end + 1 + 16, cd as u32 + 1);
    assert!(inspect_bytes(&b).is_none());
    let original = build(&[("a", b"hello")], false, 0, true, false, b"");
    for pos in [
        33,
        original.len() - 22 - 76 + 4,
        original.len() - 22 - 20 + 8,
    ] {
        let mut b = original.clone();
        b[pos] ^= 1;
        assert!(inspect_bytes(&b).is_none());
    }
}

struct Guard {
    data: Cursor<Vec<u8>>,
    range: Range<u64>,
    fail: bool,
}
impl Read for Guard {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let at = self.data.position();
        assert!(at >= self.range.start && at + b.len() as u64 <= self.range.end);
        if self.fail {
            return Err(io::Error::other("injected ZIP read error"));
        }
        self.data.read(b)
    }
}
impl Seek for Guard {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}
#[test]
fn zip_source_bounds_eocd_chunk_boundaries_and_unclosed_other_signature() {
    let file = build(&[("a", b"hello")], true, 2, false, false, b"");
    for overlap in 1..=12 {
        let mut source = vec![0; 1024 * 1024 - overlap - (file.len() - 22)];
        let start = source.len();
        source.extend(&file);
        source.extend([0; 24]);
        let found = scan(&source, vec![Kind::Zip]);
        assert_eq!(found.len(), 1);
        assert!(matches!(found[0].locator,Locator::Signature{offset,..} if offset==start as u64));
    }
    let mut source = b"%PDF-unclosed".to_vec();
    source.extend(&file);
    assert_eq!(scan(&source, vec![Kind::Zip, Kind::Pdf]).len(), 1);
    let mut source = file.clone();
    source.extend([0; 31]);
    let start = source.len() as u64;
    source.extend(&file);
    let end = source.len() as u64;
    source.extend(&file);
    for range in [start..end, start + 1..end, start..end - 1] {
        let mut g = Guard {
            data: Cursor::new(source.clone()),
            range: range.clone(),
            fail: false,
        };
        let mut found = vec![];
        signature_scan::scan(
            &mut g,
            source.len() as u64,
            &Options {
                kinds: vec![Kind::Zip],
                start: range.start,
                length: Some(range.end - range.start),
            },
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        assert_eq!(found.len(), usize::from(range == (start..end)));
    }
}
fn office(kind: Kind, external: bool, macro_type: bool) -> Vec<u8> {
    let (part, ty) = match kind {
        Kind::Docx => ("custom/main.xml", "wordprocessingml.document"),
        Kind::Xlsx => ("xl/workbook.xml", "spreadsheetml.sheet"),
        Kind::Pptx => ("ppt/presentation.xml", "presentationml.presentation"),
        _ => panic!(),
    };
    let ty = if macro_type {
        "application/vnd.ms-word.document.macroEnabled.main+xml".into()
    } else {
        format!("application/vnd.openxmlformats-officedocument.{ty}.main+xml")
    };
    let types=format!("<?xml version=\"1.0\"?><c:Types xmlns:c=\"http://schemas.openxmlformats.org/package/2006/content-types\"><c:Override PartName=\"/{part}\" ContentType=\"{ty}\"/></c:Types>");
    let rels=format!("<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"{part}\" {}/></Relationships>",if external{"TargetMode=\"External\""}else{""});
    build(
        &[
            ("[Content_Types].xml", types.as_bytes()),
            ("_rels/.rels", rels.as_bytes()),
            (part, b"<document/>"),
        ],
        true,
        2,
        false,
        false,
        b"",
    )
}
#[test]
fn zip_office_package_relationships_types_and_filters_select_correct_extension() {
    for (kind, extension) in [
        (Kind::Docx, "docx"),
        (Kind::Xlsx, "xlsx"),
        (Kind::Pptx, "pptx"),
    ] {
        let file = office(kind, false, false);
        let found = scan(&file, vec![Kind::Zip, kind]);
        assert_eq!(found.len(), 1);
        assert!(found[0].name.ends_with(extension));
        assert!(scan(&file, vec![Kind::Zip]).is_empty());
        assert_eq!(scan(&file, vec![kind]).len(), 1);
        for (external, macros) in [(true, false), (false, true)] {
            let other = office(kind, external, macros);
            assert!(scan(&other, vec![kind]).is_empty());
            assert_eq!(scan(&other, vec![Kind::Zip]).len(), 1);
        }
    }
    let fake = build(
        &[("word/document.xml", b"fake")],
        false,
        0,
        false,
        false,
        b"",
    );
    assert!(scan(&fake, vec![Kind::Docx]).is_empty());
}
#[test]
fn zip_xml_entities_nested_documents_duplicate_attributes_and_uri_fetches_are_rejected() {
    let ns = b"urn:test";
    for s in [
        "<!DOCTYPE R [<!ENTITY x SYSTEM 'file:///private'>]><R xmlns='urn:test'>&x;</R>",
        "<R xmlns='urn:test'/><R xmlns='urn:test'/>",
        "<R xmlns='urn:test'><x a='1' a='2'/></R>",
        "<R xmlns='urn:test'><x><y/></x></R>",
        "<R xmlns='wrong'/>",
        "<R xmlns='urn:test'>",
        "<R xmlns='urn:test'><x></R>",
    ] {
        assert!(flat_xml(s.as_bytes(), ns, b"R").is_none(), "{s}");
    }
    assert!(flat_xml(
        b"<p:R xmlns:p='urn:test'><p:x a='a&amp;b'/></p:R>",
        ns,
        b"R"
    )
    .is_some());
    for name in [
        "../main.xml",
        "http://test/main.xml",
        "a\\b",
        "/a//b",
        "%2e%2e/a",
        "a?b",
        "a#b",
    ] {
        assert!(part_name(name).is_none());
    }
}
#[test]
fn zip_cancellation_io_errors_and_work_budget_propagate_without_publishing_candidate() {
    let file = build(&[("a", &vec![0x41; 200_000])], true, 0, false, false, b"");
    let at = file.len() as u64 - 22;
    let mut calls = 0;
    let result = probe(
        &mut Cursor::new(&file),
        at,
        0..file.len() as u64,
        &mut Budget::default(),
        || {
            calls += 1;
            if calls > 8 {
                Err("cancel".into())
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(result,Err(e) if e=="cancel"));
    let result = probe(
        &mut Cursor::new(&file),
        at,
        0..file.len() as u64,
        &mut Budget { remaining: 60 },
        || Ok(()),
    );
    assert!(matches!(result,Err(e) if e.contains("工作量")));
    let mut reader = Guard {
        data: Cursor::new(file.clone()),
        range: 0..file.len() as u64,
        fail: true,
    };
    assert!(probe(
        &mut reader,
        at,
        0..file.len() as u64,
        &mut Budget::default(),
        || Ok(())
    )
    .is_err());
    for len in 4..512 {
        let mut b = vec![0xff; len];
        b[len - 4..].copy_from_slice(b"PK\x05\x06");
        assert!(scan(&b, vec![Kind::Zip]).is_empty());
    }
}
#[test]
fn zip_offsets_near_u64_max_do_not_wrap_or_read_outside_the_source() {
    struct Shifted {
        bytes: Cursor<Vec<u8>>,
        base: u64,
    }
    impl Read for Shifted {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.bytes.read(b)
        }
    }
    impl Seek for Shifted {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            let SeekFrom::Start(at) = p else {
                panic!("absolute seeks only")
            };
            let relative = at.checked_sub(self.base).expect("seek before source");
            assert!(relative <= self.bytes.get_ref().len() as u64);
            self.bytes.seek(SeekFrom::Start(relative))?;
            Ok(at)
        }
    }
    let normal = build(&[("a", b"hello")], false, 0, false, false, b"");
    for mode in 0..3 {
        let mut bytes = normal.clone();
        if mode == 1 {
            let cd = bytes.len() - 22 - 47;
            set16(&mut bytes, cd + 30, u16::MAX);
        }
        if mode == 2 {
            set16(&mut bytes, 28, u16::MAX);
        }
        let base = u64::MAX - bytes.len() as u64;
        let eocd = u64::MAX - 22;
        let result = probe(
            &mut Shifted {
                bytes: Cursor::new(bytes),
                base,
            },
            eocd,
            base..u64::MAX,
            &mut Budget::default(),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(result.is_some(), mode == 0);
    }
    let mut bytes = vec![0; 42];
    bytes[..4].copy_from_slice(b"PK\x06\x07");
    set32(&mut bytes, 16, 1);
    bytes[20..24].copy_from_slice(b"PK\x05\x06");
    let base = u64::MAX - bytes.len() as u64;
    assert!(probe(
        &mut Shifted {
            bytes: Cursor::new(bytes),
            base
        },
        u64::MAX - 22,
        base..u64::MAX,
        &mut Budget::default(),
        || Ok(())
    )
    .unwrap()
    .is_none());
}

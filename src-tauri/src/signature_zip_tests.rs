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

const OFFICE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn document_package(kind: Kind, main: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let content_type = match kind {
        Kind::Docx => "wordprocessingml.document",
        Kind::Xlsx => "spreadsheetml.sheet",
        Kind::Pptx => "presentationml.presentation",
        _ => unreachable!(),
    };
    let types = format!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/{main}" ContentType="application/vnd.openxmlformats-officedocument.{content_type}.main+xml"/></Types>"#
    );
    let rel = relationships(&[("rId1", "officeDocument", main, false)]);
    let mut files = vec![
        ("[Content_Types].xml", types.as_bytes()),
        ("_rels/.rels", rel.as_bytes()),
    ];
    files.extend_from_slice(parts);
    build(&files, true, 0, false, true, b"package comment")
}

fn relationships(entries: &[(&str, &str, &str, bool)]) -> String {
    let mut xml = String::from(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, kind, target, external) in entries {
        let mode = if *external { "External" } else { "Internal" };
        xml.push_str(&format!(r#"<Relationship Id="{id}" Type="{OFFICE_REL}/{kind}" Target="{target}" TargetMode="{mode}"/>"#));
    }
    xml.push_str("</Relationships>");
    xml
}

#[test]
fn office_preview_word_reads_only_document_paragraphs_and_bounds_output() {
    let word = |body: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        )
    };
    let document = word(
        r#"<w:p><w:r><w:t>中文 &lt;script&gt;</w:t><w:tab/><w:t>line &#x2603;</w:t><w:br/><w:t>last</w:t></w:r><w:del><w:r><w:t>Removed</w:t></w:r></w:del></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>table text</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
    );
    let main = "office/main.xml";
    let archive = document_package(Kind::Docx, main, &[(main, document.as_bytes())]);
    let preview = |bytes| {
        crate::office_preview::read(bytes, Kind::Docx)
            .map(|view| serde_json::to_value(view).unwrap())
    };
    assert_eq!(
        preview(&archive).unwrap(),
        serde_json::json!({
            "kind": "document", "paragraphs": ["中文 <script>\tline ☃\nlast", "table text"], "limited": false
        })
    );
    assert!(crate::office_preview::read(&archive, Kind::Xlsx).is_none());
    assert!(preview(&archive[..archive.len() - 1]).is_none());
    let long = word(&format!(
        "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
        "字".repeat(5000)
    ));
    let capped = document_package(Kind::Docx, main, &[(main, long.as_bytes())]);
    let result = preview(&capped).unwrap();
    assert_eq!(result["limited"], true);
    assert_eq!(result["paragraphs"][0].as_str().unwrap(), "字".repeat(1365));
    let dtd = format!(
        r#"<!DOCTYPE w:document [<!ENTITY sample SYSTEM "file:///secret">]><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>&sample;</w:body></w:document>"#
    );
    assert!(preview(&document_package(
        Kind::Docx,
        main,
        &[(main, dtd.as_bytes())]
    ))
    .is_none());
}

#[test]
fn office_preview_workbook_follows_sheet_order_and_shows_cached_cell_values() {
    let main = "xl/workbook.xml";
    let rels = relationships(&[
        ("one", "worksheet", "worksheets/sheet1.xml", false),
        ("two", "worksheet", "/xl/worksheets/sheet2.xml", false),
        ("strings", "sharedStrings", "sharedStrings.xml", false),
        (
            "link",
            "externalLink",
            "https://example.invalid/external",
            true,
        ),
    ]);
    let book = format!(
        r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:x="{OFFICE_REL}"><sheets><sheet name="Totals &amp; Notes" x:id="two"/><sheet name="First" x:id="one"/></sheets></workbook>"#
    );
    let worksheet = |cells: &str| {
        format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="7">{cells}</row></sheetData></worksheet>"#
        )
    };
    let first = worksheet(r#"<c r="A7" t="str"><v>next sheet</v></c>"#);
    let second = worksheet(
        r#"<c r="A7" t="s"><v>0</v></c><c r="C7"><f>6*7</f><v>42</v></c><c r="D7" t="inlineStr"><is><t>inline</t></is></c><c r="M7" t="b"><v>1</v></c>"#,
    );
    let strings = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><r><t>shared </t></r><r><t>value</t></r><rPh><t>do not display</t></rPh></si></sst>"#;
    let parts = [
        (main, book.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", first.as_bytes()),
        ("xl/worksheets/sheet2.xml", second.as_bytes()),
        ("xl/sharedStrings.xml", strings.as_slice()),
    ];
    let archive = document_package(Kind::Xlsx, main, &parts);
    let preview = crate::office_preview::read(&archive, Kind::Xlsx).unwrap();
    let result = serde_json::to_value(preview).unwrap();
    assert_eq!(result["limited"], true);
    assert_eq!(result["sheets"][0]["name"], "Totals & Notes");
    assert_eq!(result["sheets"][0]["rows"][0]["number"], 7);
    let cells = result["sheets"][0]["rows"][0]["cells"].as_array().unwrap();
    assert_eq!(
        &cells[..4],
        &[
            serde_json::json!("shared value"),
            serde_json::Value::Null,
            serde_json::json!("42"),
            serde_json::json!("inline")
        ]
    );
    assert_eq!(result["sheets"][1]["rows"][0]["cells"][0], "next sheet");

    let mut invalid_parts = parts;
    let wrong_row = worksheet(r#"<c r="B8"><v>7</v></c>"#);
    invalid_parts[3].1 = wrong_row.as_bytes();
    assert!(crate::office_preview::read(
        &document_package(Kind::Xlsx, main, &invalid_parts),
        Kind::Xlsx
    )
    .is_none());
}

#[test]
fn office_preview_slides_follow_internal_relationships_only() {
    let main = "ppt/presentation.xml";
    let presentation = format!(
        r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{OFFICE_REL}"><p:sldIdLst><p:sldId r:id="second"/><p:sldId r:id="first"/></p:sldIdLst></p:presentation>"#
    );
    let slide = |text: &str| {
        format!(
            r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    };
    let first = slide("second on screen");
    let second = slide("first on screen");
    let rels = relationships(&[
        ("first", "slide", "slides/slide1.xml", false),
        ("second", "slide", "slides/slide2.xml", false),
    ]);
    let mut parts = [
        (main, presentation.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
        ("ppt/slides/slide1.xml", first.as_bytes()),
        ("ppt/slides/slide2.xml", second.as_bytes()),
    ];
    let archive = document_package(Kind::Pptx, main, &parts);
    let preview = crate::office_preview::read(&archive, Kind::Pptx).unwrap();
    assert_eq!(
        serde_json::to_value(preview).unwrap(),
        serde_json::json!({
            "kind": "presentation", "slides": [
                {"number": 1, "paragraphs": ["first on screen"]},
                {"number": 2, "paragraphs": ["second on screen"]}
            ], "limited": false
        })
    );
    let external = relationships(&[
        ("first", "slide", "slides/slide1.xml", false),
        ("second", "slide", "https://example.invalid/slide.xml", true),
    ]);
    parts[1].1 = external.as_bytes();
    assert!(
        crate::office_preview::read(&document_package(Kind::Pptx, main, &parts), Kind::Pptx)
            .is_none()
    );
}

#[test]
fn sector_alignment_checks_the_archive_start_not_the_end_record() {
    let archive = build(&[("sample", b"payload")], false, 0, false, false, b"");
    let mut input = vec![0; 4 * 4096];
    input[4096..][..archive.len()].copy_from_slice(&archive);
    input[8209..][..archive.len()].copy_from_slice(&archive);
    let options = Options {
        kinds: vec![Kind::Zip],
        start: 1,
        sector_aligned: true,
        ..Default::default()
    };
    let mut found = Vec::new();
    signature_scan::continue_scan_scoped(
        &mut Cursor::new(&input),
        input.len() as u64,
        &options,
        &mut signature_scan::Checkpoint::default(),
        4096,
        None,
        |_, _| Ok(()),
        |file| found.push(file),
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "recovered_0000000000001000.zip");
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

fn relocate(bytes: &[u8], positions: &[usize], image: &mut [u8]) -> Vec<Range<usize>> {
    let eocd = bytes.windows(4).rposition(|b| b == b"PK\x05\x06").unwrap() as u64;
    let mut reader = Cursor::new(bytes);
    let mut budget = Budget::default();
    let mut p = Probe {
        reader: &mut reader,
        range: 0..bytes.len() as u64,
        budget: &mut budget,
        tick: || Ok(()),
    };
    let d = directory(&mut p, eocd, false).ok().unwrap();
    let edges: Vec<_> = d
        .entries
        .iter()
        .map(|e| e.offset as usize)
        .chain([d.cd_offset as usize, bytes.len()])
        .collect();
    assert_eq!(positions.len() + 1, edges.len());
    edges
        .windows(2)
        .zip(positions)
        .map(|(edge, &at)| {
            let target = at..at + edge[1] - edge[0];
            image[target.clone()].copy_from_slice(&bytes[edge[0]..edge[1]]);
            target
        })
        .collect()
}

fn fragment_scan(image: &[u8], options: &Options) -> Vec<Candidate> {
    let mut files = Vec::new();
    signature_scan::scan(
        &mut Cursor::new(image),
        image.len() as u64,
        options,
        |_, _| Ok(()),
        |c| files.push(c),
    )
    .unwrap();
    files
}

#[test]
fn fragmented_zip_roundtrips_exact_bytes_in_physical_and_directory_disorder() {
    let test_archives = [
        include_bytes!("../../tests/fixtures/zip-dotnet-NoCompression.zip").to_vec(),
        include_bytes!("../../tests/fixtures/zip-dotnet-Optimal.zip").to_vec(),
        build(
            &[("first", b"Hello"), ("second", b"World")],
            true,
            2,
            true,
            true,
            b"photo archive",
        ),
    ];
    let options = Options {
        kinds: vec![Kind::Zip],
        zip_fragments: true,
        ..Options::default()
    };
    for (i, original) in test_archives.into_iter().enumerate() {
        let mut image = vec![0x6e; 4 << 20];
        // Split the first magic across scan buffers and place the directory
        // before the first logical record.
        let positions = if i < 2 {
            vec![(1 << 20) - 2, 3 << 20, 2 << 20, 4096]
        } else {
            vec![(1 << 20) - 2, 3 << 20, 4096]
        };
        let placed = relocate(&original, &positions, &mut image);
        assert!(scan(&image, vec![Kind::Zip]).is_empty());
        let candidates = fragment_scan(&image, &options);
        assert_eq!(candidates.len(), 1);
        let saved = signature_scan::StoredCandidate::from_candidate(&candidates[0]).unwrap();
        assert!(saved.fragmented());
        let stored = serde_json::to_vec(&saved).unwrap();
        let restored: signature_scan::StoredCandidate = serde_json::from_slice(&stored).unwrap();
        let range = 0..image.len() as u64;
        assert!(restored
            .candidate(0, 0..placed[1].end as u64 - 1, &options, None)
            .is_err());
        let ordinary = Options {
            zip_fragments: false,
            ..options.clone()
        };
        assert!(restored
            .candidate(0, range.clone(), &ordinary, None)
            .is_err());
        let restored = restored.candidate(0, range, &options, None).unwrap();
        for limit in [9, MAX_FILE] {
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&image), &restored, &mut out, limit, |_| {
                Ok(())
            })
            .unwrap();
            assert_eq!(out, original[..original.len().min(limit as usize)]);
        }
        image[placed[1].end - 1] ^= 1;
        let mut out = vec![];
        assert!(crate::recovery::extract(
            &mut Cursor::new(&image),
            &restored,
            &mut out,
            5,
            |_| Ok(())
        )
        .is_err());
        assert!(out.is_empty());
        assert!(fragment_scan(&image, &options).is_empty());
    }
}

#[test]
fn fragmented_zip_refuses_duplicate_members_and_does_not_repeat_contiguous_archives() {
    let original = build(
        &[("first", b"hello"), ("second", b"world")],
        false,
        0,
        false,
        false,
        b"",
    );
    let mut image = vec![0; 16384];
    let parts = relocate(&original, &[4096, 2051, 8192], &mut image);
    let options = Options {
        kinds: vec![Kind::Zip],
        zip_fragments: true,
        ..Options::default()
    };
    image.copy_within(parts[1].clone(), 12288);
    assert!(fragment_scan(&image, &options).is_empty());
    assert_eq!(
        fragment_scan(
            &image,
            &Options {
                length: Some(10240),
                ..options.clone()
            }
        )
        .len(),
        1
    );
    let contiguous = fragment_scan(&original, &options);
    assert_eq!(contiguous.len(), 1);
    assert!(
        !signature_scan::StoredCandidate::from_candidate(&contiguous[0])
            .unwrap()
            .fragmented()
    );
}

#[test]
fn fragmented_document_checks_every_free_extent_and_previews_word_contents() {
    use crate::used_space::Builder;
    let original = document_package(Kind::Docx, "word/document.xml", &[
        ("word/document.xml", br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Separated document</w:t></w:r></w:p></w:body></w:document>"#),
    ]);
    let size = 8 * 4096;
    let mut image = vec![0u8; size];
    relocate(&original, &[4096, 3 * 4096 + 5, 139, 6 * 4096], &mut image);
    let make_plan = |allow_middle: bool| {
        let mut builder = Builder::new(size as u64, 4096).unwrap();
        builder.free(0, 8192).unwrap();
        if allow_middle {
            builder.free(3 * 4096, 4096).unwrap();
        }
        builder.free(6 * 4096, 4096).unwrap();
        builder.finish().unwrap()
    };
    let complete = make_plan(true);
    let incomplete = make_plan(false);
    let options = Options {
        kinds: vec![Kind::Docx],
        zip_fragments: true,
        free_only: true,
        sector_aligned: true,
        ..Options::default()
    };
    let run = |plan: &crate::used_space::Plan| {
        let mut files = Vec::new();
        signature_scan::continue_scan_scoped(
            &mut Cursor::new(&image),
            size as u64,
            &options,
            &mut Default::default(),
            4096,
            Some(plan),
            |_, _| Ok(()),
            |c| files.push(c),
        )
        .unwrap();
        files
    };
    assert!(run(&incomplete).is_empty());
    let files = run(&complete);
    assert_eq!(files.len(), 1);
    assert!(files[0].name.ends_with(".docx"));
    signature_scan::verify_free_candidates(&complete, &files).unwrap();
    let mut candidate = files[0].clone();
    if let Locator::Signature {
        allocation_digest, ..
    } = &mut candidate.locator
    {
        *allocation_digest = Some(incomplete.digest.clone());
    }
    assert!(signature_scan::verify_free_candidates(&incomplete, [&candidate]).is_err());
    let mut output = Vec::new();
    crate::recovery::extract(
        &mut Cursor::new(image),
        &files[0],
        &mut output,
        MAX_FILE,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(output, original);
    assert!(crate::office_preview::read(&output, Kind::Docx).is_some());
}

#[test]
fn fragmented_pass_pause_preserves_contiguous_results_and_restarts_atomically() {
    let contiguous = build(&[("plain", b"test")], false, 0, false, false, b"");
    let original = build(
        &[("first", b"hello"), ("second", b"world")],
        false,
        0,
        false,
        false,
        b"",
    );
    let mut image = vec![0; 16384];
    image[..contiguous.len()].copy_from_slice(&contiguous);
    relocate(&original, &[4096, 2048, 8192], &mut image);
    let options = Options {
        kinds: vec![Kind::Zip],
        zip_fragments: true,
        ..Options::default()
    };
    let mut cursor = signature_scan::Checkpoint::default();
    let size = image.len() as u64;
    let mut files = Vec::new();
    assert!(signature_scan::continue_scan(
        &mut Cursor::new(&image),
        size,
        &options,
        &mut cursor,
        |done, total| if done == total {
            Err("pause".into())
        } else {
            Ok(())
        },
        |c| files.push(c)
    )
    .is_err());
    assert_eq!(files.len(), 1);
    assert_eq!(cursor.work_done(&options), size);
    assert!(!cursor.sealed);
    let mut cursor = serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    signature_scan::continue_scan(
        &mut Cursor::new(&image),
        size,
        &options,
        &mut cursor,
        |_, _| Ok(()),
        |c| files.push(c),
    )
    .unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].id, 1);
    assert_eq!(cursor.work_done(&options), size * 2);
    assert!(cursor.sealed);
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
                .candidate(0, 0..bytes.len() as u64, &Options::default(), None)
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
                ..Default::default()
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
        let external = office(kind, true, false);
        assert!(scan(&external, vec![kind]).is_empty());
        assert_eq!(scan(&external, vec![Kind::Zip]).len(), 1);
        let macros = office(kind, false, true);
        assert!(scan(&macros, vec![kind, Kind::Zip]).is_empty());
        assert_eq!(scan(&macros, vec![Kind::Docm]).len(), 1);
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
fn new_document_kinds_keep_archive_evidence_and_preview_saved_content() {
    let cases: [(Kind, &str, &[u8]); 6] = [
        (
            Kind::Docm,
            "docm",
            include_bytes!("../../tests/fixtures/signature-document.docm"),
        ),
        (
            Kind::Xlsm,
            "xlsm",
            include_bytes!("../../tests/fixtures/signature-document.xlsm"),
        ),
        (
            Kind::Pptm,
            "pptm",
            include_bytes!("../../tests/fixtures/signature-document.pptm"),
        ),
        (
            Kind::Odt,
            "odt",
            include_bytes!("../../tests/fixtures/signature-document.odt"),
        ),
        (
            Kind::Ods,
            "ods",
            include_bytes!("../../tests/fixtures/signature-document.ods"),
        ),
        (
            Kind::Odp,
            "odp",
            include_bytes!("../../tests/fixtures/signature-document.odp"),
        ),
    ];
    for (kind, extension, bytes) in cases {
        assert!(scan(bytes, vec![Kind::Zip]).is_empty());
        let options = Options {
            kinds: vec![kind],
            ..Options::default()
        };
        let mut source = vec![0; 4096];
        source.extend_from_slice(bytes);
        source.extend_from_slice(&[0; 17]);
        let end = source.len() as u64;
        let mut found = scan(&source, vec![kind, Kind::Zip]);
        assert_eq!(found.len(), 1);
        let item = found.pop().unwrap();
        assert_eq!(item.name, format!("recovered_0000000000001000.{extension}"));
        let saved = signature_scan::StoredCandidate::from_candidate(&item).unwrap();
        let decoded: signature_scan::StoredCandidate =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        let restored = decoded.candidate(item.id, 0..end, &options, None).unwrap();
        let mut output = vec![];
        let mut reader = Cursor::new(source);
        crate::recovery::extract(&mut reader, &restored, &mut output, end, |_| Ok(())).unwrap();
        assert_eq!(output, bytes);
        assert!(crate::office_preview::read(bytes, Kind::Zip).is_none());
        let preview =
            serde_json::to_value(crate::office_preview::read(bytes, kind).unwrap()).unwrap();
        assert_eq!(preview["limited"], false);
        match kind {
            Kind::Docm => {
                assert_eq!(preview["paragraphs"][0], "Recovered DOCM text & 中文");
                assert_eq!(preview["paragraphs"][1], "DOCM second paragraph");
            }
            Kind::Odt => {
                assert_eq!(preview["paragraphs"][0], "Recovered ODT");
                assert_eq!(
                    preview["paragraphs"][1],
                    "Before bold & 中文 after   end\ttab\nline"
                );
            }
            Kind::Xlsm | Kind::Ods => {
                assert_eq!(preview["sheets"][0]["name"], "Saved totals");
                assert_eq!(preview["sheets"][0]["rows"][0]["number"], 7);
                assert!(preview["sheets"][0]["rows"][0]["cells"][1].is_null());
                assert_eq!(preview["sheets"][0]["rows"][0]["cells"][2], "42");
                assert_eq!(preview["sheets"][1]["name"], "Notes");
                let cells = &preview["sheets"][1]["rows"][0]["cells"];
                assert_eq!(
                    cells[0],
                    format!("{} worksheet two", extension.to_uppercase())
                );
                if kind == Kind::Ods {
                    assert_eq!(cells[0], cells[1]);
                }
            }
            Kind::Pptm | Kind::Odp => {
                for (i, suffix) in ["one", "two"].iter().enumerate() {
                    assert_eq!(preview["slides"][i]["number"], i + 1);
                    assert_eq!(
                        preview["slides"][i]["paragraphs"][0],
                        format!("Recovered {} slide {suffix}", extension.to_uppercase())
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}

const ODT_MIME: &[u8] = b"application/vnd.oasis.opendocument.text";
const ODT_MANIFEST: &str = r#"<m:manifest xmlns:m="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><m:file-entry m:full-path="/" m:media-type="application/vnd.oasis.opendocument.text"/><m:file-entry m:full-path="content.xml" m:media-type="text/xml"/></m:manifest>"#;

#[test]
fn odf_identity_requires_owned_first_mime_and_unambiguous_unencrypted_manifest() {
    let content = ("content.xml", b"<content/>".as_slice());
    let mime = ("mimetype", ODT_MIME);
    let manifest = ("META-INF/manifest.xml", ODT_MANIFEST.as_bytes());
    for (files, deflate, wide) in [
        (vec![content, mime, manifest], false, false),
        (vec![mime, content, manifest], true, false),
        (vec![mime, content, manifest], false, true),
        (vec![mime, manifest], false, false),
        (
            vec![mime, content, manifest, ("unlisted.xml", b"x")],
            false,
            false,
        ),
    ] {
        let file = build(&files, deflate, 0, wide, true, b"");
        assert_eq!(inspect_bytes(&file).unwrap().kind, Kind::Zip);
    }
    for invalid in [
        ODT_MANIFEST.replace("opendocument.text", "opendocument.presentation"),
        ODT_MANIFEST.replace("m:full-path", "full-path"),
        ODT_MANIFEST.replace(
            "/></m:manifest>",
            "><m:encryption-data/></m:file-entry></m:manifest>",
        ),
        ODT_MANIFEST.replace(
            "</m:manifest>",
            r#"<m:file-entry m:full-path="/" m:media-type="text/xml"/></m:manifest>"#,
        ),
    ] {
        let file = build(
            &[mime, content, ("META-INF/manifest.xml", invalid.as_bytes())],
            false,
            0,
            false,
            false,
            b"",
        );
        assert_eq!(inspect_bytes(&file).unwrap().kind, Kind::Zip);
        assert!(crate::office_preview::read(&file, Kind::Odt).is_none());
    }
    let named = build(&[mime, content, manifest], false, 0, false, true, b"");
    assert_eq!(inspect_bytes(&named).unwrap().kind, Kind::Odt);
    assert!(crate::office_preview::read(&named, Kind::Odt).is_none());
}

#[test]
fn odf_preview_bounds_space_and_omits_active_objects() {
    let content = br#"<o:document-content xmlns:o="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:t="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><o:scripts><t:p>not body</t:p></o:scripts><o:body><o:text><t:p>first<o:annotation><t:p>annotation</t:p></o:annotation><t:a>label</t:a></t:p><t:p>X<t:s t:c="4294967295"/>Y</t:p></o:text></o:body></o:document-content>"#;
    let file = build(
        &[
            ("mimetype", ODT_MIME),
            ("content.xml", content),
            ("META-INF/manifest.xml", ODT_MANIFEST.as_bytes()),
        ],
        false,
        0,
        false,
        false,
        b"",
    );
    let preview =
        serde_json::to_value(crate::office_preview::read(&file, Kind::Odt).unwrap()).unwrap();
    assert_eq!(preview["limited"], true);
    assert_eq!(preview["paragraphs"].as_array().unwrap().len(), 2);
    assert_eq!(preview["paragraphs"][0], "firstlabel");
    let bounded = preview["paragraphs"][1].as_str().unwrap();
    assert_eq!(bounded.len(), 4096);
    assert_eq!(bounded.trim_end(), "X");
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

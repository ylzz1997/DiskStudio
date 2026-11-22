//! Prepare files on the isolated NTFS data volume and audit their read-only previews.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

#[path = "../../../../tests/fixtures/paged_preview.rs"]
mod paged;

const FOLDER: &str = "DS-Office-Preview";
const PAGED_FOLDER: &str = "DS-Paged-Preview";
const WITNESS: &str = "baseline.json";
const WORD: &str = "Native Word.docx";
const BOOK: &str = "Native Workbook.xlsx";
const SLIDES: &str = "Native Slides.pptx";
const DAMAGED: &str = "Truncated Word.docx";
const PDF: &str = "Native PDF.pdf";
const TIFF: &str = "Native TIFF.tif";
const BIG_TIFF: &str = "Native BigTIFF.tiff";
const DAMAGED_PDF: &str = "Truncated PDF.pdf";
const OFFICE_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

#[derive(Deserialize, serde::Serialize, PartialEq, Eq)]
struct Witness {
    desktop: String,
    tables: [String; 3],
    files: BTreeMap<String, String>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn rels(entries: &[(&str, &str, &str)]) -> String {
    let mut xml = String::from(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, kind, target) in entries {
        xml.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{OFFICE_RELS}/{kind}" Target="{target}"/>"#
        ));
    }
    xml.push_str("</Relationships>");
    xml
}

fn package(
    main: &str,
    media: &str,
    others: &[(&str, &str)],
    parts: &[(&str, &str)],
) -> Result<Vec<u8>, String> {
    let mut types = String::from(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>"#,
    );
    types.push_str(&format!(r#"<Override PartName="/{main}" ContentType="application/vnd.openxmlformats-officedocument.{media}.main+xml"/>"#));
    for (path, media) in others {
        types.push_str(&format!(r#"<Override PartName="/{path}" ContentType="application/vnd.openxmlformats-officedocument.{media}+xml"/>"#));
    }
    types.push_str("</Types>");
    let rels = rels(&[("root", "officeDocument", main)]);
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, xml) in [
        ("[Content_Types].xml", types.as_str()),
        ("_rels/.rels", rels.as_str()),
    ]
    .iter()
    .chain(parts.iter())
    {
        zip.start_file(*name, options).map_err(err)?;
        zip.write_all(xml.as_bytes()).map_err(err)?;
    }
    Ok(zip.finish().map_err(err)?.into_inner())
}

fn documents() -> Result<[(&'static str, Vec<u8>); 4], String> {
    let word = package(
        "word/document.xml",
        "wordprocessingml.document",
        &[],
        &[(
            "word/document.xml",
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>DiskStudio native DOCX &amp; 中文</w:t></w:r></w:p><w:p><w:r><w:t>Second paragraph</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    )?;
    let mut damaged = word.clone();
    damaged.truncate(damaged.len() - 15);
    let workbook = format!(
        r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="{OFFICE_RELS}"><sheets><sheet name="Totals" sheetId="2" r:id="second"/><sheet name="Notes" sheetId="1" r:id="first"/></sheets></workbook>"#
    );
    let book_rels = rels(&[
        ("first", "worksheet", "worksheets/sheet1.xml"),
        ("second", "worksheet", "worksheets/sheet2.xml"),
        ("strings", "sharedStrings", "sharedStrings.xml"),
    ]);
    let sheet = |body: &str| {
        format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{body}</sheetData></worksheet>"#
        )
    };
    let first = sheet(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Native worksheet two</t></is></c></row>"#,
    );
    let second =
        sheet(r#"<row r="7"><c r="A7" t="s"><v>0</v></c><c r="C7"><f>6*7</f><v>42</v></c></row>"#);
    let book = package(
        "xl/workbook.xml",
        "spreadsheetml.sheet",
        &[
            ("xl/worksheets/sheet1.xml", "spreadsheetml.worksheet"),
            ("xl/worksheets/sheet2.xml", "spreadsheetml.worksheet"),
            ("xl/sharedStrings.xml", "spreadsheetml.sharedStrings"),
        ],
        &[
            ("xl/workbook.xml", &workbook),
            ("xl/_rels/workbook.xml.rels", &book_rels),
            ("xl/worksheets/sheet1.xml", &first),
            ("xl/worksheets/sheet2.xml", &second),
            (
                "xl/sharedStrings.xml",
                r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>Native shared value</t></si></sst>"#,
            ),
        ],
    )?;
    let presentation = format!(
        r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{OFFICE_RELS}"><p:sldIdLst><p:sldId id="256" r:id="second"/><p:sldId id="257" r:id="first"/></p:sldIdLst></p:presentation>"#
    );
    let slide_rels = rels(&[
        ("first", "slide", "slides/slide1.xml"),
        ("second", "slide", "slides/slide2.xml"),
    ]);
    let slide = |text: &str| {
        format!(
            r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    };
    let slides = package(
        "ppt/presentation.xml",
        "presentationml.presentation",
        &[
            ("ppt/slides/slide1.xml", "presentationml.slide"),
            ("ppt/slides/slide2.xml", "presentationml.slide"),
        ],
        &[
            ("ppt/presentation.xml", &presentation),
            ("ppt/_rels/presentation.xml.rels", &slide_rels),
            ("ppt/slides/slide1.xml", &slide("Native slide two")),
            ("ppt/slides/slide2.xml", &slide("Native slide one")),
        ],
    )?;
    Ok([
        (WORD, word),
        (BOOK, book),
        (SLIDES, slides),
        (DAMAGED, damaged),
    ])
}

fn save(dir: &Dir, name: &str, data: &[u8]) -> Result<(), String> {
    let mut output = dir
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    output.write_all(data).map_err(err)?;
    output.sync_all().map_err(err)
}

fn verified_b_offline() -> Result<(), String> {
    reallocation_checks::state(&inventory()?, B, true)
}

fn prepare_files(folder: &str, documents: [(&str, Vec<u8>); 4]) -> Result<PathBuf, String> {
    let desktop = btrfs_checks::desktop()?;
    verified_b_offline()?;
    let (root, path) = ntfs_reconstruction_checks::verified_a_root()?;
    let tables = ntfs_reconstruction_checks::table_hashes()?;
    root.create_dir(folder).map_err(err)?;
    let dir = root.open_dir(folder).map_err(err)?;
    let mut files = BTreeMap::new();
    for (name, bytes) in documents {
        save(&dir, name, &bytes)?;
        files.insert(name.to_owned(), digest(&bytes));
    }
    let witness = Witness {
        desktop,
        tables,
        files,
    };
    save(&dir, WITNESS, &serde_json::to_vec(&witness).map_err(err)?)?;
    if ntfs_reconstruction_checks::table_hashes()? != witness.tables {
        return Err("Guest tables or OS layout changed during ordinary file preparation.".into());
    }
    Ok(path.join(folder))
}

fn verify_files(folder: &str, names: [&str; 4]) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    verified_b_offline()?;
    let (root, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let dir = root.open_dir(folder).map_err(err)?;
    let expected: Witness =
        serde_json::from_reader(dir.open(WITNESS).map_err(err)?.take(4096)).map_err(err)?;
    let mut files = BTreeMap::new();
    for name in names {
        let mut bytes = Vec::new();
        dir.open(name)
            .map_err(err)?
            .take(1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(err)?;
        files.insert(name.to_owned(), digest(&bytes));
    }
    let actual = Witness {
        desktop,
        tables: ntfs_reconstruction_checks::table_hashes()?,
        files,
    };
    if actual != expected {
        return Err("A fixture digest, desktop payload, test GPT or OS layout differs.".into());
    }
    Ok("PASS: all four source SHA-256 values, both test GPTs, and the guest OS layout match. B stayed offline.".into())
}

pub(super) fn prepare_office() -> Result<String, String> {
    let path = prepare_files(FOLDER, documents()?)?;
    Ok(format!("PASS: ordinary Office fixtures are ready in {} on A/2. Preview all three formats and the truncated DOCX fallback.", path.display()))
}

pub(super) fn verify_office() -> Result<String, String> {
    verify_files(FOLDER, [WORD, BOOK, SLIDES, DAMAGED])
}

pub(super) fn prepare_paged() -> Result<String, String> {
    let pdf = paged::pdf();
    let broken_pdf = pdf[..pdf.len() - 7].to_vec();
    let files = [
        (PDF, pdf),
        (TIFF, paged::TIFF.to_vec()),
        (BIG_TIFF, paged::BIG_TIFF.to_vec()),
        (DAMAGED_PDF, broken_pdf),
    ];
    let path = prepare_files(PAGED_FOLDER, files)?;
    Ok(format!("PASS: paged fixtures are ready in {} on A/2. Preview both PDF and TIFF pages, BigTIFF and the truncated PDF fallback.", path.display()))
}

pub(super) fn verify_paged() -> Result<String, String> {
    verify_files(PAGED_FOLDER, [PDF, TIFF, BIG_TIFF, DAMAGED_PDF])
}

#[test]
fn independently_written_zip_fixtures_match_preview_support() {
    let docs = documents().unwrap();
    for ((_, bytes), kind) in docs.iter().zip([
        signature_scan::Kind::Docx,
        signature_scan::Kind::Xlsx,
        signature_scan::Kind::Pptx,
    ]) {
        assert!(office_preview::read(bytes, kind).is_some(), "{kind:?}");
    }
    assert!(office_preview::read(&docs[3].1, signature_scan::Kind::Docx).is_none());
}

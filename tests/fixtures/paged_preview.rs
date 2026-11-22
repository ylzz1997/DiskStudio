//! Synthetic documents shared by the parser tests and isolated VM acceptance.
use std::io::Write;

pub const TIFF: &[u8] = include_bytes!("signature-tiff-pages.tif");
pub const BIG_TIFF: &[u8] = include_bytes!("signature-bigtiff-tiles.tif");

pub fn pdf() -> Vec<u8> {
    let page = |extent: &str, content_id| {
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {extent}] /Resources << /Font << /F1 7 0 R >> >> /Contents {content_id} 0 R >>")
    };
    let stream = |rgb: &str, extent: &str, text: &str| {
        let data =
            format!("{rgb} rg 0 0 {extent} re f 0 0 0 rg BT /F1 28 Tf 28 72 Td ({text}) Tj ET\n");
        format!("<< /Length {} >>\nstream\n{data}endstream", data.len())
    };
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
        page("432 288", 5),
        page("288 432", 6),
        stream("0.8 0.9 1", "432 288", "DiskStudio PDF page one"),
        stream("1 0.9 0.65", "288 432", "PDF page two"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut bytes = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(bytes, "{} 0 obj\n{}\nendobj", index + 1, object).unwrap();
    }
    let startxref = bytes.len();
    writeln!(bytes, "xref\n0 {}\n0000000000 65535 f ", offsets.len() + 1).unwrap();
    for offset in &offsets {
        writeln!(bytes, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        bytes,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF",
        offsets.len() + 1
    )
    .unwrap();
    bytes
}

//! Read-only, bounded content previews for Office Open XML packages.
use crate::{
    signature_scan::Kind,
    signature_zip::{self, OfficePackage},
};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_TEXT: usize = 64 * 1024;
const MAX_PARAGRAPHS: usize = 192;
const MAX_PARAGRAPH_BYTES: usize = 4096;
const MAX_SHEETS: usize = 12;
const MAX_SLIDES: usize = 24;
const MAX_ROWS: usize = 32;
const MAX_COLUMNS: usize = 12;
const MAX_CELL_BYTES: usize = 512;
const PACKAGE_RELS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const OFFICE_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_RELS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum Preview {
    Document {
        paragraphs: Vec<String>,
        limited: bool,
    },
    Workbook {
        sheets: Vec<Sheet>,
        limited: bool,
    },
    Presentation {
        slides: Vec<Slide>,
        limited: bool,
    },
}

#[derive(Serialize)]
pub(crate) struct Sheet {
    name: String,
    rows: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    number: u32,
    cells: Vec<Option<String>>,
}

#[derive(Serialize)]
pub(crate) struct Slide {
    number: usize,
    paragraphs: Vec<String>,
}

struct TextLimit {
    left: usize,
    limited: bool,
}

impl TextLimit {
    fn copy(&mut self, text: &str, limit: usize) -> String {
        let mut length = text.len().min(limit).min(self.left);
        while !text.is_char_boundary(length) {
            length -= 1;
        }
        self.limited |= length < text.len();
        self.left -= length;
        text[..length].to_owned()
    }
}

pub(crate) fn kind_for_extension(extension: &str) -> Option<Kind> {
    match extension {
        "docx" => Some(Kind::Docx),
        "xlsx" => Some(Kind::Xlsx),
        "pptx" => Some(Kind::Pptx),
        _ => None,
    }
}

pub(crate) fn read(bytes: &[u8], expected: Kind) -> Option<Preview> {
    let package = signature_zip::office_package(bytes)?;
    if expected != package.kind {
        return None;
    }
    let root = part(&package, &package.main)?;
    let mut limit = TextLimit {
        left: MAX_TEXT,
        limited: false,
    };
    match package.kind {
        Kind::Docx if root.is(Ns::Word, "document") => {
            let paragraphs = paragraphs(root.child(Ns::Word, "body")?, Ns::Word, &mut limit);
            Some(Preview::Document {
                paragraphs,
                limited: limit.limited,
            })
        }
        Kind::Xlsx if root.is(Ns::Sheet, "workbook") => {
            let sheets = workbook(&package, &root, &mut limit)?;
            Some(Preview::Workbook {
                sheets,
                limited: limit.limited,
            })
        }
        Kind::Pptx if root.is(Ns::Slide, "presentation") => {
            let slides = presentation(&package, &root, &mut limit)?;
            Some(Preview::Presentation {
                slides,
                limited: limit.limited,
            })
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    None,
    Word,
    Sheet,
    Slide,
    Drawing,
    Rel,
    Other,
}

fn namespace(resolved: ResolveResult<'_>) -> Option<Ns> {
    Some(match resolved {
        ResolveResult::Unbound => Ns::None,
        ResolveResult::Unknown(_) => return None,
        ResolveResult::Bound(uri) => match uri.as_ref() {
            "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            | "http://purl.oclc.org/ooxml/wordprocessingml/main" => Ns::Word,
            "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
            | "http://purl.oclc.org/ooxml/spreadsheetml/main" => Ns::Sheet,
            "http://schemas.openxmlformats.org/presentationml/2006/main"
            | "http://purl.oclc.org/ooxml/presentationml/main" => Ns::Slide,
            "http://schemas.openxmlformats.org/drawingml/2006/main"
            | "http://purl.oclc.org/ooxml/drawingml/main" => Ns::Drawing,
            OFFICE_RELS | STRICT_RELS => Ns::Rel,
            _ => Ns::Other,
        },
    })
}

struct Node {
    ns: Ns,
    name: Box<str>,
    attrs: Vec<((Ns, Box<str>), String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn is(&self, ns: Ns, name: &str) -> bool {
        self.ns == ns && self.name.as_ref() == name
    }

    fn child(&self, ns: Ns, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.is(ns, name))
    }

    fn attr(&self, ns: Ns, name: &str) -> Option<&str> {
        self.attrs.iter().find_map(|((attr_ns, attr_name), value)| {
            (*attr_ns == ns && attr_name.as_ref() == name).then_some(value.as_str())
        })
    }
}

fn part(package: &OfficePackage, name: &str) -> Option<Node> {
    parse_xml(std::str::from_utf8(package.xml.get(name.as_bytes())?).ok()?)
}

fn attach(node: Node, stack: &mut [Node], root: &mut Option<Node>) -> Option<()> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.is_none() {
        *root = Some(node);
    } else {
        return None;
    }
    Some(())
}

fn parse_xml(xml: &str) -> Option<Node> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().check_comments = true;
    reader.config_mut().check_end_names = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut declaration = false;
    // Cap allocations and traversal even if the XML mostly contains empty tags.
    for _ in 0..100_000 {
        let event = reader.read_event().ok()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                if stack.len() >= 64 {
                    return None;
                }
                let resolver = reader.resolver();
                let (resolved, local) = resolver.resolve_element(e.name());
                let mut node = Node {
                    ns: namespace(resolved)?,
                    name: local.into_inner().into(),
                    attrs: Vec::new(),
                    text: String::new(),
                    children: Vec::new(),
                };
                for attribute in e.attributes() {
                    let attribute = attribute.ok()?;
                    let (resolved, local) = resolver.resolve_attribute(attribute.key);
                    let ns = namespace(resolved)?;
                    if matches!(ns, Ns::None | Ns::Rel) {
                        let key = (ns, local.into_inner().into());
                        if node.attrs.len() >= 32 || node.attrs.iter().any(|(name, _)| name == &key)
                        {
                            return None;
                        }
                        node.attrs.push((
                            key,
                            attribute
                                .normalized_value(quick_xml::XmlVersion::default())
                                .ok()?
                                .into_owned(),
                        ));
                    }
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(node);
                } else {
                    attach(node, &mut stack, &mut root)?;
                }
            }
            Event::End(_) => {
                let node = stack.pop()?;
                attach(node, &mut stack, &mut root)?;
            }
            Event::Text(text) => {
                let text = text.xml10_content();
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                } else if !text.as_bytes().iter().all(u8::is_ascii_whitespace) {
                    return None;
                }
            }
            Event::CData(text) => stack.last_mut()?.text.push_str(&text.xml10_content()),
            Event::GeneralRef(reference) => {
                let ch = match reference.resolve_char_ref().ok()? {
                    Some(ch) => ch,
                    None => match reference.as_ref() {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "apos" => '\'',
                        "quot" => '"',
                        _ => return None,
                    },
                };
                if !(matches!(ch, '\t' | '\r' | '\n')
                    || ch >= ' ' && ch != '\u{fffe}' && ch != '\u{ffff}')
                {
                    return None;
                }
                stack.last_mut()?.text.push(ch);
            }
            Event::Decl(decl) => {
                if declaration
                    || root.is_some()
                    || !stack.is_empty()
                    || decl.version().ok()?.as_ref() != "1.0"
                    || decl.encoding().is_some_and(|encoding| {
                        !encoding.is_ok_and(|value| value.eq_ignore_ascii_case("utf-8"))
                    })
                {
                    return None;
                }
                declaration = true;
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::DocType(_) => return None,
            Event::Eof => return if stack.is_empty() { root } else { None },
        }
    }
    None
}

fn text_tree(node: &Node, ns: Ns) -> bool {
    match ns {
        Ns::Word => {
            node.ns == Ns::Word
                && !matches!(
                    node.name.as_ref(),
                    "del" | "moveFrom" | "object" | "drawing"
                )
        }
        Ns::Drawing => matches!(node.ns, Ns::Slide | Ns::Drawing),
        _ => false,
    }
}

fn paragraph_text(node: &Node, ns: Ns, result: &mut String) {
    if !text_tree(node, ns) || node.is(ns, "p") {
        return;
    }
    match (node.ns == ns, node.name.as_ref()) {
        (true, "t") => result.push_str(&node.text),
        (true, "tab") if ns == Ns::Word => result.push('\t'),
        (true, "br" | "cr") => result.push('\n'),
        _ => {
            for child in &node.children {
                paragraph_text(child, ns, result);
            }
        }
    }
}

fn paragraphs(root: &Node, ns: Ns, limit: &mut TextLimit) -> Vec<String> {
    fn collect(node: &Node, ns: Ns, out: &mut Vec<String>, limit: &mut TextLimit) {
        if !text_tree(node, ns) {
            return;
        }
        if node.is(ns, "p") {
            if out.len() == MAX_PARAGRAPHS || limit.left == 0 {
                limit.limited = true;
                return;
            }
            let mut text = String::new();
            for child in &node.children {
                paragraph_text(child, ns, &mut text);
            }
            out.push(limit.copy(&text, MAX_PARAGRAPH_BYTES));
        } else {
            for child in &node.children {
                collect(child, ns, out, limit);
            }
        }
    }
    let mut result = Vec::new();
    collect(root, ns, &mut result, limit);
    result
}

struct Relationship {
    kind: String,
    target: Option<String>,
}

fn relationships(package: &OfficePackage) -> Option<BTreeMap<String, Relationship>> {
    let (dir, file) = package.main.rsplit_once('/').unwrap_or(("", &package.main));
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let path = format!("{prefix}_rels/{file}.rels");
    let rows = signature_zip::flat_xml(
        package.xml.get(path.as_bytes())?,
        PACKAGE_RELS,
        b"Relationships",
    )?;
    let mut rels = BTreeMap::new();
    for (name, mut attrs) in rows {
        if name != b"Relationship" {
            return None;
        }
        let id = attrs.remove(b"Id".as_slice())?;
        let kind = attrs.remove(b"Type".as_slice())?;
        let raw = attrs.get(b"Target".as_slice())?;
        let internal = attrs
            .get(b"TargetMode".as_slice())
            .is_none_or(|mode| mode == "Internal");
        let target = internal
            .then(|| signature_zip::part_name(raw))
            .flatten()
            .map(|name| {
                if raw.starts_with('/') {
                    name.to_owned()
                } else {
                    format!("{prefix}{name}")
                }
            });
        let kind = kind
            .strip_prefix(OFFICE_RELS)
            .or_else(|| kind.strip_prefix(STRICT_RELS))
            .and_then(|tail| tail.strip_prefix('/'))
            .unwrap_or("")
            .to_owned();
        if rels.insert(id, Relationship { kind, target }).is_some() {
            return None;
        }
    }
    Some(rels)
}

fn presentation(package: &OfficePackage, root: &Node, limit: &mut TextLimit) -> Option<Vec<Slide>> {
    let Some(ids) = root.child(Ns::Slide, "sldIdLst") else {
        return Some(Vec::new());
    };
    let rels = relationships(package)?;
    let mut seen = BTreeSet::new();
    let mut slides = Vec::new();
    for (index, id) in ids
        .children
        .iter()
        .filter(|id| id.is(Ns::Slide, "sldId"))
        .enumerate()
    {
        if index == MAX_SLIDES {
            limit.limited = true;
            break;
        }
        let rel = rels.get(id.attr(Ns::Rel, "id")?)?;
        let target = rel.target.as_deref()?;
        if rel.kind != "slide" || !seen.insert(target) {
            return None;
        }
        let slide = part(package, target)?;
        if !slide.is(Ns::Slide, "sld") {
            return None;
        }
        let tree = slide.child(Ns::Slide, "cSld")?.child(Ns::Slide, "spTree")?;
        slides.push(Slide {
            number: index + 1,
            paragraphs: paragraphs(tree, Ns::Drawing, limit),
        });
    }
    Some(slides)
}

fn rich_string(node: &Node) -> String {
    let mut text = String::new();
    for child in &node.children {
        if child.is(Ns::Sheet, "t") {
            text.push_str(&child.text);
        } else if child.is(Ns::Sheet, "r") {
            if let Some(run) = child.child(Ns::Sheet, "t") {
                text.push_str(&run.text);
            }
        }
    }
    text
}

fn workbook(package: &OfficePackage, root: &Node, limit: &mut TextLimit) -> Option<Vec<Sheet>> {
    let rels = relationships(package)?;
    let mut strings = rels.values().filter(|rel| rel.kind == "sharedStrings");
    let shared = if let Some(rel) = strings.next() {
        if strings.next().is_some() {
            return None;
        }
        let strings = part(package, rel.target.as_deref()?)?;
        if !strings.is(Ns::Sheet, "sst") {
            return None;
        }
        strings
            .children
            .iter()
            .filter(|item| item.is(Ns::Sheet, "si"))
            .map(rich_string)
            .collect()
    } else {
        Vec::new()
    };
    let mut sheets = Vec::new();
    let mut seen = BTreeSet::new();
    for sheet in root
        .child(Ns::Sheet, "sheets")?
        .children
        .iter()
        .filter(|sheet| sheet.is(Ns::Sheet, "sheet"))
    {
        if sheets.len() == MAX_SHEETS {
            limit.limited = true;
            break;
        }
        let rel = rels.get(sheet.attr(Ns::Rel, "id")?)?;
        if matches!(rel.kind.as_str(), "chartsheet" | "dialogsheet") {
            limit.limited = true;
            continue;
        }
        let target = rel.target.as_deref()?;
        if rel.kind != "worksheet" || !seen.insert(target) {
            return None;
        }
        let name = limit.copy(sheet.attr(Ns::None, "name")?, 128);
        let worksheet = part(package, target)?;
        if !worksheet.is(Ns::Sheet, "worksheet") {
            return None;
        }
        let rows = worksheet_rows(worksheet.child(Ns::Sheet, "sheetData")?, &shared, limit)?;
        sheets.push(Sheet { name, rows });
    }
    Some(sheets)
}

fn cell_address(address: &str) -> Option<(u32, usize)> {
    let digits = address.find(|ch: char| !ch.is_ascii_uppercase())?;
    let (letters, number) = address.split_at(digits);
    if letters.is_empty()
        || letters.len() > 3
        || number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let column = letters
        .bytes()
        .fold(0, |n, ch| n * 26 + usize::from(ch - b'A' + 1));
    let row = number.parse::<u32>().ok()?;
    (column <= 16_384 && (1..=1_048_576).contains(&row)).then_some((row, column))
}

fn worksheet_rows(data: &Node, shared: &[String], limit: &mut TextLimit) -> Option<Vec<Row>> {
    let mut rows = Vec::new();
    let mut previous_row = 0;
    for row in data.children.iter().filter(|row| row.is(Ns::Sheet, "row")) {
        if rows.len() == MAX_ROWS || limit.left == 0 {
            limit.limited = true;
            break;
        }
        let number = row
            .attr(Ns::None, "r")
            .map_or(Some(previous_row + 1), |r| r.parse::<u32>().ok())?;
        if number <= previous_row || number > 1_048_576 {
            return None;
        }
        previous_row = number;
        let mut cells = vec![None; MAX_COLUMNS];
        let mut previous_column = 0;
        for cell in row.children.iter().filter(|cell| cell.is(Ns::Sheet, "c")) {
            let (cell_row, column) = cell
                .attr(Ns::None, "r")
                .map_or(Some((number, previous_column + 1)), cell_address)?;
            if cell_row != number || column <= previous_column || column > 16_384 {
                return None;
            }
            previous_column = column;
            let kind = cell.attr(Ns::None, "t").unwrap_or("n");
            let inline;
            let text = if kind == "inlineStr" {
                inline = cell
                    .child(Ns::Sheet, "is")
                    .map(rich_string)
                    .unwrap_or_default();
                inline.as_str()
            } else if let Some(value) = cell.child(Ns::Sheet, "v") {
                match kind {
                    "s" => shared
                        .get(value.text.trim().parse::<usize>().ok()?)?
                        .as_str(),
                    "b" => match value.text.trim() {
                        "0" => "FALSE",
                        "1" => "TRUE",
                        _ => return None,
                    },
                    "n" | "str" | "e" | "d" => value.text.as_str(),
                    _ => return None,
                }
            } else {
                continue;
            };
            if column <= MAX_COLUMNS {
                cells[column - 1] = Some(limit.copy(text, MAX_CELL_BYTES));
            } else {
                limit.limited = true;
            }
        }
        if cells.iter().any(Option::is_some) {
            rows.push(Row { number, cells });
        }
    }
    Some(rows)
}

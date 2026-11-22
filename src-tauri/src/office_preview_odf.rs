//! Bounded plain-text views of the content.xml body of an OpenDocument package.
use super::{
    Kind, Node, Ns, Preview, Row, Sheet, Slide, TextLimit, MAX_CELL_BYTES, MAX_COLUMNS,
    MAX_PARAGRAPHS, MAX_PARAGRAPH_BYTES, MAX_ROWS, MAX_SHEETS, MAX_SLIDES,
};

pub(super) fn preview(root: &Node, kind: Kind, limit: &mut TextLimit) -> Option<Preview> {
    if !root.is(Ns::OdfOffice, "document-content") {
        return None;
    }
    let body = root.child(Ns::OdfOffice, "body")?;
    match kind {
        Kind::Odt => {
            let paragraphs = paragraphs(body.child(Ns::OdfOffice, "text")?, limit);
            Some(Preview::Document {
                paragraphs,
                limited: limit.limited,
            })
        }
        Kind::Ods => {
            let sheets = sheets(body.child(Ns::OdfOffice, "spreadsheet")?, limit)?;
            Some(Preview::Workbook {
                sheets,
                limited: limit.limited,
            })
        }
        Kind::Odp => {
            let presentation = body.child(Ns::OdfOffice, "presentation")?;
            let mut slides = Vec::new();
            for page in presentation
                .children
                .iter()
                .filter(|e| e.is(Ns::OdfDraw, "page"))
            {
                if slides.len() == MAX_SLIDES || limit.left == 0 {
                    limit.limited = true;
                    break;
                }
                slides.push(Slide {
                    number: slides.len() + 1,
                    paragraphs: paragraphs(page, limit),
                });
            }
            Some(Preview::Presentation {
                slides,
                limited: limit.limited,
            })
        }
        _ => None,
    }
}

fn paragraphs(root: &Node, limit: &mut TextLimit) -> Vec<String> {
    fn collect(node: &Node, result: &mut Vec<String>, limit: &mut TextLimit) {
        match (node.ns, node.name.as_ref()) {
            (Ns::OdfText, "p" | "h") => {
                if result.len() == MAX_PARAGRAPHS || limit.left == 0 {
                    limit.limited = true;
                    return;
                }
                let mut text = String::new();
                let mut room = TextLimit {
                    left: MAX_PARAGRAPH_BYTES.min(limit.left),
                    limited: false,
                };
                inline(node, &mut text, &mut room);
                limit.left -= text.len();
                limit.limited |= room.limited;
                result.push(text);
            }
            (Ns::OdfOffice, "text")
            | (Ns::OdfText, "section" | "list" | "list-item" | "list-header")
            | (Ns::OdfDraw, "page" | "g" | "frame" | "text-box" | "custom-shape")
            | (
                Ns::OdfTable,
                "table" | "table-cell" | "table-row" | "table-row-group" | "table-header-rows"
                | "table-rows",
            ) => {
                for child in &node.children {
                    collect(child, result, limit);
                }
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    collect(root, &mut result, limit);
    result
}

fn inline(node: &Node, result: &mut String, room: &mut TextLimit) {
    let mut start = 0;
    for child in &node.children {
        let end = child.parent_text_offset;
        result.push_str(&room.copy(&node.text[start..end], MAX_PARAGRAPH_BYTES));
        start = end;
        match (child.ns, child.name.as_ref()) {
            (Ns::OdfText, "span" | "a" | "date" | "time" | "page-number" | "page-count") => {
                inline(child, result, room);
            }
            (Ns::OdfText, "s") => {
                if let Some(count) = repetition(child, Ns::OdfText, "c") {
                    let count = count as usize;
                    let spaces = count.min(room.left);
                    result.extend(std::iter::repeat_n(' ', spaces));
                    room.left -= spaces;
                    room.limited |= spaces < count;
                } else {
                    room.limited = true;
                }
            }
            (Ns::OdfText, "tab") => result.push_str(&room.copy("\t", 1)),
            (Ns::OdfText, "line-break") => result.push_str(&room.copy("\n", 1)),
            _ => {}
        }
    }
    result.push_str(&room.copy(&node.text[start..], MAX_PARAGRAPH_BYTES));
}

fn repetition(node: &Node, ns: Ns, key: &str) -> Option<u32> {
    node.attr(ns, key)
        .map_or(Some(1), |n| n.parse::<u32>().ok().filter(|&n| n > 0))
}

fn sheets(spreadsheet: &Node, limit: &mut TextLimit) -> Option<Vec<Sheet>> {
    let mut sheets = Vec::new();
    for table in spreadsheet
        .children
        .iter()
        .filter(|e| e.is(Ns::OdfTable, "table"))
    {
        if sheets.len() == MAX_SHEETS || limit.left == 0 {
            limit.limited = true;
            break;
        }
        let name = limit.copy(table.attr(Ns::OdfTable, "name")?, 128);
        let mut rows = Vec::new();
        let mut number = 1;
        worksheet_rows(table, &mut number, &mut rows, limit)?;
        sheets.push(Sheet { name, rows });
    }
    Some(sheets)
}

fn worksheet_rows(
    table: &Node,
    number: &mut u32,
    rows: &mut Vec<Row>,
    limit: &mut TextLimit,
) -> Option<()> {
    for row in table.children.iter().filter(|e| e.ns == Ns::OdfTable) {
        match row.name.as_ref() {
            "table-row-group" | "table-header-rows" | "table-rows" => {
                worksheet_rows(row, number, rows, limit)?;
            }
            "table-row" => {
                if rows.len() == MAX_ROWS || limit.left == 0 {
                    limit.limited = true;
                    break;
                }
                let repeat = repetition(row, Ns::OdfTable, "number-rows-repeated")?;
                let next = number.checked_add(repeat)?;
                let cells = worksheet_cells(row, limit)?;
                if cells.iter().any(Option::is_some) {
                    for current in *number..next {
                        if rows.len() == MAX_ROWS || limit.left == 0 {
                            limit.limited = true;
                            break;
                        }
                        rows.push(Row {
                            number: current,
                            cells: cells
                                .iter()
                                .map(|value| {
                                    value
                                        .as_ref()
                                        .map(|value| limit.copy(value, MAX_CELL_BYTES))
                                })
                                .collect(),
                        });
                    }
                }
                *number = next;
            }
            _ => {}
        }
    }
    Some(())
}

fn worksheet_cells(row: &Node, limit: &mut TextLimit) -> Option<Vec<Option<String>>> {
    let mut cells = vec![None; MAX_COLUMNS];
    let mut column = 0u32;
    for cell in row.children.iter().filter(|e| e.ns == Ns::OdfTable) {
        if !matches!(cell.name.as_ref(), "table-cell" | "covered-table-cell") {
            continue;
        }
        let repeat = repetition(cell, Ns::OdfTable, "number-columns-repeated")?;
        let next = column.checked_add(repeat)?;
        if cell.is(Ns::OdfTable, "table-cell") {
            let mut room = TextLimit {
                left: MAX_CELL_BYTES,
                limited: false,
            };
            let text = paragraphs(cell, &mut room).join("\n");
            let stored = match cell.attr(Ns::OdfOffice, "value-type") {
                Some("string") => cell.attr(Ns::OdfOffice, "string-value"),
                Some("float" | "percentage" | "currency") => cell.attr(Ns::OdfOffice, "value"),
                Some("boolean") => cell.attr(Ns::OdfOffice, "boolean-value"),
                Some("date") => cell.attr(Ns::OdfOffice, "date-value"),
                Some("time") => cell.attr(Ns::OdfOffice, "time-value"),
                _ => None,
            };
            if let Some(value) = (!text.is_empty()).then_some(text.as_str()).or(stored) {
                limit.limited |= room.limited || next as usize > MAX_COLUMNS;
                for output in cells.iter_mut().take(next as usize).skip(column as usize) {
                    let mut cap = TextLimit {
                        left: MAX_CELL_BYTES,
                        limited: false,
                    };
                    *output = Some(cap.copy(value, MAX_CELL_BYTES));
                    limit.limited |= cap.limited;
                }
            } else {
                limit.limited |= room.limited;
            }
        }
        column = next;
    }
    Some(cells)
}

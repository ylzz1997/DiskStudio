//! Native, bounded browsing over the same recovery engines as the desktop.
use super::partitions::Volume;
use crate::{
    native_locale::{self, Locale},
    recovery::{self, Candidate, ScanMode},
    recovery_tree::{Catalog, ROOT, UNKNOWN},
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const PAGE: usize = 100;
const PREVIEW: u64 = 4096;

#[derive(Clone)]
pub(super) enum Row {
    Up(String),
    Folder(String),
    File(usize),
    Offset(usize),
}

pub(super) struct Listing {
    pub(super) volume: Arc<Volume>,
    pub(super) files: Vec<Candidate>,
    children: HashMap<String, Vec<(String, Row)>>,
    pub(super) catalog: Catalog,
    summary: String,
}

fn active(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("读取已取消。".into());
    }
    Ok(())
}

fn display(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

pub(super) fn scan(
    volume: Arc<Volume>,
    mode: ScanMode,
    cancel: Arc<AtomicBool>,
) -> Result<Listing, String> {
    let mut catalog = Catalog::default();
    let mut files = Vec::new();
    let label = recovery::scan_filesystem_owned(
        volume.reader(&cancel)?,
        mode,
        cancel.clone(),
        |_, _| active(&cancel),
        |file| files.push(file),
        |folder| catalog.insert(folder),
    )?;
    active(&cancel)?;
    let mut children: HashMap<String, Vec<(String, Row)>> = HashMap::new();
    let directories = catalog.snapshot();
    let known: HashSet<_> = directories.iter().map(|dir| dir.id.as_str()).collect();
    for dir in &directories {
        if let Some(parent) = &dir.parent {
            children
                .entry(parent.clone())
                .or_default()
                .push((display(&dir.name), Row::Folder(dir.id.clone())));
        }
    }
    for (index, file) in files.iter().enumerate() {
        let parent = file
            .directory
            .as_deref()
            .filter(|id| known.contains(id))
            .unwrap_or(UNKNOWN);
        children
            .entry(parent.into())
            .or_default()
            .push((display(&file.name), Row::File(index)));
    }
    for rows in children.values_mut() {
        rows.sort_by_cached_key(|(name, row)| (matches!(row, Row::File(_)), name.to_lowercase()));
    }
    active(&cancel)?;
    let origin = volume.provenance()?;
    volume.source.verify_disk()?;
    let partition = volume.display_partition(&origin.partition, &native_locale::current());
    let summary = format!("{} / {partition} · {label}", origin.file_name);
    Ok(Listing {
        volume,
        files,
        children,
        catalog,
        summary,
    })
}

impl Listing {
    pub(super) fn exportable(&self, row: &Row) -> bool {
        match row {
            Row::File(index) => self.files.get(*index).is_some_and(|file| file.supported),
            Row::Folder(folder) => folder != UNKNOWN,
            Row::Up(_) | Row::Offset(_) => false,
        }
    }

    pub(super) fn page(
        &self,
        folder: &str,
        offset: usize,
        locale: &Locale,
    ) -> (String, Vec<(String, Row)>) {
        let mut visible = Vec::new();
        let (path, parent) = if folder == UNKNOWN {
            (locale.text("未知目录").to_owned(), Some(ROOT.into()))
        } else {
            let path = self.catalog.path(folder).unwrap_or_default();
            let parent = path.last().and_then(|d| d.parent.clone());
            let label = path
                .iter()
                .map(|d| {
                    display(if d.id == ROOT {
                        locale.text(&d.name)
                    } else {
                        &d.name
                    })
                })
                .collect::<Vec<_>>()
                .join(" / ");
            (label, parent)
        };
        if let Some(parent) = parent {
            visible.push((locale.text("上一级目录").into(), Row::Up(parent)));
        }
        if folder == ROOT && self.children.contains_key(UNKNOWN) {
            visible.push((locale.text("未知目录").into(), Row::Folder(UNKNOWN.into())));
        }
        if offset > 0 {
            visible.push((
                locale.text("上一页").into(),
                Row::Offset(offset.saturating_sub(PAGE)),
            ));
        }
        let children = self
            .children
            .get(folder)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (name, row) in children.iter().skip(offset).take(PAGE) {
            let label = match row {
                Row::Folder(_) => format!("[{}] {name}", locale.text("目录")),
                Row::File(index) => {
                    let file = &self.files[*index];
                    format!("{name} · {} B", file.size)
                }
                Row::Up(_) | Row::Offset(_) => unreachable!(),
            };
            visible.push((label, row.clone()));
        }
        if offset + PAGE < children.len() {
            visible.push((locale.text("下一页").into(), Row::Offset(offset + PAGE)));
        }
        let summary = format!(
            "{}\n{}  ·  {} {}\n",
            self.summary,
            path,
            children.len(),
            locale.text("个条目")
        );
        (summary, visible)
    }

    pub(super) fn preview(
        &self,
        index: usize,
        locale: &Locale,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        let file = self.files.get(index).ok_or("文件不存在")?;
        let mut output = format!(
            "{}  ·  {} B\n{}\n\n",
            display(&file.name),
            file.size,
            display(locale.text(&file.note))
        );
        if !file.supported {
            return Ok(output);
        }
        let mut data = Vec::new();
        recovery::extract_owned(
            self.volume.reader(&cancel)?,
            file,
            &mut data,
            PREVIEW,
            cancel.clone(),
            |_| active(&cancel),
        )?;
        self.volume.source.verify_disk()?;
        active(&cancel)?;
        if file.size > PREVIEW {
            let _ = writeln!(output, "{}\n", locale.text("仅预览前 4096 字节。"));
        }
        if let Some(text) = text_preview(&data) {
            output.push_str(&text);
        } else {
            for (line, bytes) in data.chunks(16).enumerate() {
                let _ = write!(output, "{:08x}  ", line * 16);
                for byte in bytes {
                    let _ = write!(output, "{byte:02x} ");
                }
                output.push('\n');
            }
        }
        Ok(output)
    }
}

fn text_preview(data: &[u8]) -> Option<String> {
    let decoded = if let Some(data) = data.strip_prefix(&[0xff, 0xfe]) {
        if data.len() % 2 != 0 {
            return None;
        }
        String::from_utf16(
            &data
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .ok()?
    } else {
        std::str::from_utf8(data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data))
            .ok()?
            .to_owned()
    };
    if decoded
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return None;
    }
    Some(decoded.replace("\r\n", "\n").replace('\r', "\n"))
}

#[test]
fn uses_hex_for_controls_and_normalizes_text_line_endings() {
    assert_eq!(
        text_preview(b"\xef\xbb\xbfsample\r\nnext"),
        Some("sample\nnext".into())
    );
    assert_eq!(text_preview(&[0xff, 0xfe, b'a', 0]), Some("a".into()));
    assert_eq!(text_preview(b"text\0not plain text"), None);
}

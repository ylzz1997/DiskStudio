use crate::{
    native_locale::Locale,
    surface_scan::{FailureResolution, Report},
    surface_tasks::Snapshot,
};
use rust_xlsxwriter::{Format, Workbook, XlsxError};
use serde::Deserialize;
use std::fmt::Write;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReportFormat {
    #[default]
    Json,
    Text,
    Excel,
}

impl ReportFormat {
    pub fn file_name(self) -> (&'static str, &'static str) {
        match self {
            Self::Json => ("json", "DiskStudio-surface.json"),
            Self::Text => ("txt", "DiskStudio-surface.txt"),
            Self::Excel => ("xlsx", "DiskStudio-surface.xlsx"),
        }
    }
}

const REFINEMENT_LIMITS: &str = "耗时分布和失败块统计基于初读，平均速度包含细查时间。细查在初读失败的块内读取更小的范围，记录单扇区读取失败及预算用尽时尚未定位的范围。结果反映该次读取，不能判定介质健康。未锁定在线卷，内容可能变化。";
const COARSE_LIMITS: &str = "每次读取最多 1 MiB；读取失败标记整个请求块，不代表块内每个扇区损坏。耗时包含设备和虚拟化缓存影响，不是健康诊断。未读取范围没有结论。未锁定在线卷，内容可能变化。";
const LATENCIES: [&str; 6] = [
    "< 10 ms",
    "10–49 ms",
    "50–149 ms",
    "150–499 ms",
    "≥ 500 ms",
    "读取失败",
];

enum Cell {
    Text(String),
    Integer(i128),
}

impl Cell {
    fn integer(value: impl Into<i128>) -> Self {
        Self::Integer(value.into())
    }

    fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
}

struct Section<'a> {
    name: &'a str,
    headings: &'a [&'a str],
    rows: Vec<Vec<Cell>>,
}

fn summary(snapshot: &Snapshot, locale: &Locale, limits: &str) -> Vec<Vec<Cell>> {
    let report = &snapshot.report;
    let state = match report.state.as_str() {
        "running" => "正在检测",
        "completed" => "检测完成",
        "paused" => "检测已暂停",
        "timedOut" => "检测因读取超时停止",
        "canceled" => "检测已取消",
        _ => "检测已停止",
    };
    let row = |name, value| vec![Cell::text(locale.text(name)), value];
    let mut rows = vec![
        row("磁盘", Cell::text(&snapshot.source)),
        row(
            "分区编号",
            snapshot
                .partition
                .map_or_else(|| Cell::text(locale.text("整个磁盘")), Cell::integer),
        ),
        row("状态", Cell::text(locale.text(state))),
        row(
            "绝对起始 LBA",
            Cell::integer(report.offset / report.sector_size),
        ),
        row("扇区数量", Cell::integer(report.size / report.sector_size)),
        row("逻辑扇区（字节）", Cell::integer(report.sector_size)),
        row("检测字节数", Cell::integer(report.checked_bytes)),
        row("可读字节数", Cell::integer(report.readable_bytes)),
        row("失败块字节数", Cell::integer(report.failed_bytes)),
        row("耗时（毫秒）", Cell::integer(report.elapsed_ms)),
        row(
            "单次读取超时（秒）",
            report
                .read_timeout_seconds
                .map_or_else(|| Cell::text(locale.text("系统默认")), Cell::integer),
        ),
        row("省略的失败块", Cell::integer(report.omitted_failures)),
    ];
    if let Some(read) = &report.timed_out_read {
        rows.push(row("超时请求起始 LBA", Cell::integer(read.start_sector)));
        rows.push(row("超时请求扇区数", Cell::integer(read.sector_count)));
    }
    if let Some(refinement) = &report.refinement {
        rows.push(row(
            "单扇区读取失败",
            Cell::integer(refinement.failed_sectors),
        ));
        rows.push(row(
            "尚未定位的扇区",
            Cell::integer(refinement.unresolved_sectors),
        ));
        rows.push(row(
            "省略的细查范围",
            Cell::integer(refinement.omitted_ranges),
        ));
    }
    for (label, count) in LATENCIES.into_iter().zip(report.latency_blocks) {
        rows.push(vec![
            Cell::text(format!(
                "{} · {}",
                locale.text("读取耗时分布"),
                locale.text(label)
            )),
            Cell::integer(count),
        ]);
    }
    if !report.message.is_empty() {
        rows.push(row("结果", Cell::text(locale.text(&report.message))));
    }
    rows.push(row("使用限制", Cell::text(locale.text(limits))));
    rows
}

fn initial_failures(report: &Report) -> Vec<Vec<Cell>> {
    report
        .failures
        .iter()
        .map(|entry| {
            vec![
                Cell::integer(entry.start_sector),
                Cell::integer(entry.sector_count),
                Cell::integer(entry.code),
                Cell::integer(entry.elapsed_ms),
            ]
        })
        .collect()
}

fn text_report(sections: &[Section<'_>], locale: &Locale) -> Vec<u8> {
    let mut text = String::from("DiskStudio\r\n");
    for section in sections {
        let _ = write!(text, "\r\n{}\r\n", locale.text(section.name));
        let headings: Vec<_> = section.headings.iter().map(|h| locale.text(h)).collect();
        let _ = write!(text, "{}\r\n", headings.join("\t"));
        for row in &section.rows {
            for (col, value) in row.iter().enumerate() {
                if col > 0 {
                    text.push('\t');
                }
                match value {
                    Cell::Text(value) => {
                        text.extend(
                            value
                                .chars()
                                .map(|ch| if ch.is_control() { ' ' } else { ch }),
                        )
                    }
                    Cell::Integer(value) => {
                        let _ = write!(text, "{value}");
                    }
                }
            }
            text.push_str("\r\n");
        }
    }
    text.into_bytes()
}

fn workbook(sections: &[Section<'_>], locale: &Locale) -> Result<Vec<u8>, XlsxError> {
    let mut book = Workbook::new();
    let header = Format::new().set_bold().set_background_color(0xE6EEF5);
    let wrapped = Format::new().set_text_wrap();
    for section in sections {
        let sheet = book.add_worksheet();
        sheet.set_name(locale.text(section.name))?;
        sheet.set_freeze_panes(1, 0)?;
        let widths = if section.headings.len() == 2 {
            &[32, 75][..]
        } else {
            &[23, 21, 18, 32][..]
        };
        for (col, (heading, width)) in section.headings.iter().zip(widths).enumerate() {
            sheet.set_column_width(col as u16, *width)?;
            sheet.write_string_with_format(0, col as u16, locale.text(heading), &header)?;
        }
        for (index, row) in section.rows.iter().enumerate() {
            let index = index as u32 + 1;
            for (col, value) in row.iter().enumerate() {
                let col = col as u16;
                match value {
                    Cell::Text(value) if value.chars().count() > 55 => {
                        sheet.set_row_height(index, 58)?;
                        sheet.write_string_with_format(index, col, value, &wrapped)?;
                    }
                    Cell::Text(value) => {
                        sheet.write_string(index, col, value)?;
                    }
                    // Excel displays at most 15 significant decimal digits.
                    Cell::Integer(value) if value.abs() >= 1_000_000_000_000_000 => {
                        sheet.write_string(index, col, value.to_string())?;
                    }
                    Cell::Integer(value) => {
                        sheet.write_number(index, col, *value as f64)?;
                    }
                }
            }
        }
    }
    book.save_to_buffer()
}

pub fn encode(
    format: ReportFormat,
    snapshot: &Snapshot,
    locale: &Locale,
) -> Result<Vec<u8>, String> {
    let report = &snapshot.report;
    report.validate()?;
    let limits = if report.refinement.is_some() {
        REFINEMENT_LIMITS
    } else {
        COARSE_LIMITS
    };
    if matches!(format, ReportFormat::Json) {
        return serde_json::to_vec_pretty(&serde_json::json!({
            "format": "DiskStudio surface report",
            "version": 1,
            "limitations": limits,
            "result": snapshot,
        }))
        .map_err(|e| e.to_string());
    }
    let mut sections = vec![
        Section {
            name: "检测摘要",
            headings: &["项目", "值"],
            rows: summary(snapshot, locale, limits),
        },
        Section {
            name: "初读失败块",
            headings: &["绝对起始 LBA", "扇区数量", "错误码", "耗时（毫秒）"],
            rows: initial_failures(report),
        },
    ];
    if let Some(refined) = &report.refinement {
        let rows = refined
            .ranges
            .iter()
            .map(|entry| {
                let result = match entry.resolution {
                    FailureResolution::SectorReadFailed => "单扇区读取失败",
                    FailureResolution::Unresolved => "尚未定位",
                };
                vec![
                    Cell::integer(entry.start_sector),
                    Cell::integer(entry.sector_count),
                    Cell::integer(entry.code),
                    Cell::text(locale.text(result)),
                ]
            })
            .collect();
        sections.push(Section {
            name: "细查范围",
            headings: &["绝对起始 LBA", "扇区数量", "错误码", "结果"],
            rows,
        });
    }
    match format {
        ReportFormat::Text => Ok(text_report(&sections, locale)),
        ReportFormat::Excel => workbook(&sections, locale).map_err(|err| err.to_string()),
        ReportFormat::Json => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        native_locale::Language,
        surface_scan::{self, Options, Range, ScanSource, TransferError, TransferLimit},
    };
    use std::{io, sync::atomic::AtomicBool};

    #[test]
    fn preserves_absolute_sector_failures_and_distinguishes_the_initial_spans() {
        struct FaultAt(u64);
        impl ScanSource for FaultAt {
            fn read_at(
                &mut self,
                offset: u64,
                buffer: &mut [u8],
                _: &AtomicBool,
            ) -> Result<usize, TransferError> {
                if (offset..offset + buffer.len() as u64).contains(&self.0) {
                    Err(TransferError::Io(io::Error::from_raw_os_error(1117)))
                } else {
                    Ok(buffer.len())
                }
            }
        }
        let range = Range {
            offset: 8_000_000_000_000_000_000,
            size: 8 * 4096,
            sector: 4096,
        };
        let report = surface_scan::continue_scan(
            &mut FaultAt(range.offset + 3 * range.sector),
            Report::new(range, true),
            TransferLimit::from_adapter(1048576, 256, 0, 4096).unwrap(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let snapshot = Snapshot {
            id: "test".into(),
            target: "test-disk".into(),
            source: "=Example\t资料".into(),
            started_at: 100,
            report,
            partition: None,
            options: Options {
                start_sector: range.offset / range.sector,
                sector_count: 8,
            },
            loaded: false,
            resume_count: 0,
        };
        let locale = Locale::new(Language::En);
        let txt =
            String::from_utf8(encode(ReportFormat::Text, &snapshot, &locale).unwrap()).unwrap();
        assert!(txt.contains("=Example 资料"));
        assert!(txt.contains("1953125000000000\t8\t1117\t"));
        assert!(txt.contains("1953125000000003\t1\t1117\tSector read failures"));
        let json: serde_json::Value =
            serde_json::from_slice(&encode(ReportFormat::Json, &snapshot, &locale).unwrap())
                .unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["result"]["source"], snapshot.source);
        for language in [
            Language::En,
            Language::ZhCn,
            Language::ZhTw,
            Language::Ja,
            Language::Ko,
            Language::Pt,
            Language::Es,
            Language::De,
            Language::Fr,
            Language::It,
            Language::Ru,
        ] {
            let workbook = encode(ReportFormat::Excel, &snapshot, &Locale::new(language)).unwrap();
            assert!(workbook.starts_with(b"PK\x03\x04"));
        }
    }
}

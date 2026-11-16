//! Offline directory listing from an in-memory scan snapshot. No device/path I/O.
use crate::{
    recovery::{Candidate, Locator, ScanMode, MAX_RESULTS},
    recovery_tree::{Catalog, Directory, ROOT, UNKNOWN},
};
use serde::Serialize;
use std::{collections::HashSet, io::Write};

const MAX_JSON: usize = 64 * 1024 * 1024;
#[derive(Clone, Copy, Default)]
pub enum Completion {
    #[default]
    Running,
    Ended,
    Canceled,
    Paused,
    Failed,
}
impl Completion {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "扫描进行中 · 仅包含生成时已读取的结果",
            Self::Ended => "扫描已结束 · 仍受扫描方式、范围及数量上限限制",
            Self::Canceled => "扫描已取消 · 仅包含停止前已读取的结果",
            Self::Paused => "扫描已暂停 · 仅包含完整扫描块内已读取的结果",
            Self::Failed => "扫描未正常结束 · 仅包含停止前已读取的结果",
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry<'a> {
    id: u32,
    name: &'a str,
    // Decimal string preserves every u64 byte count in JavaScript.
    bytes: String,
    state: &'static str,
    method: &'a str,
    note: &'a str,
    supported: bool,
    directory: &'a str,
    directory_note: &'a str,
    metadata: &'a crate::file_metadata::Metadata,
}
#[derive(Serialize)]
struct Listing<'a> {
    version: u8,
    mode: &'static str,
    completion: &'static str,
    directories: Vec<Directory>,
    files: Vec<Entry<'a>>,
}
struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let escaped = bytes
            .iter()
            .filter(|b| matches!(b, b'<' | b'>' | b'&'))
            .count();
        if bytes.len().saturating_add(escaped.saturating_mul(5))
            > MAX_JSON.saturating_sub(self.0.len())
        {
            return Err(std::io::Error::other("目录清单超过 64 MiB 上限。"));
        }
        for byte in bytes {
            match byte {
                b'<' => self.0.extend_from_slice(b"\\u003c"),
                b'>' => self.0.extend_from_slice(b"\\u003e"),
                b'&' => self.0.extend_from_slice(b"\\u0026"),
                _ => self.0.push(*byte),
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn render(
    catalog: &Catalog,
    candidates: &[Candidate],
    mode: ScanMode,
    completion: Completion,
) -> Result<String, String> {
    if candidates.len() > MAX_RESULTS {
        return Err("目录清单超过 100000 个文件上限。".into());
    }
    let directories = catalog.snapshot();
    let known: HashSet<_> = directories.iter().map(|d| d.id.clone()).collect();
    if !known.contains(ROOT) {
        return Err("缺少根目录。".into());
    }
    let mut ids = HashSet::new();
    let files: Vec<_> = candidates
        .iter()
        .map(|c| {
            if !ids.insert(c.id) {
                return Err("文件身份重复，不能生成目录清单。".into());
            }
            if c.name.len() > 4096
                || c.note.len() > 16384
                || c.directory_note.len() > 16384
                || c.method.len() > 1024
            {
                return Err("文件元数据超过目录清单长度上限。".into());
            }
            Ok(Entry {
                id: c.id,
                name: &c.name,
                bytes: c.size.to_string(),
                state: if matches!(c.locator, Locator::Signature { .. }) {
                    "删除状态未知"
                } else if c.deleted {
                    "已删除"
                } else {
                    "现存文件"
                },
                method: &c.method,
                note: &c.note,
                supported: c.supported,
                directory: c
                    .directory
                    .as_deref()
                    .filter(|id| known.contains(*id))
                    .unwrap_or(UNKNOWN),
                directory_note: &c.directory_note,
                metadata: &c.metadata,
            })
        })
        .collect::<Result<_, String>>()?;
    let listing = Listing {
        version: 1,
        mode: match mode {
            ScanMode::Deleted => "找回删除文件",
            ScanMode::AllFiles => "扫描全部文件",
            ScanMode::Existing => "浏览现存文件",
            ScanMode::Signatures => "按文件签名扫描",
        },
        completion: completion.label(),
        directories,
        files,
    };
    let mut json = Bounded(Vec::new());
    serde_json::to_writer(&mut json, &listing).map_err(|e| e.to_string())?;
    // JSON sits in an inert script element, so HTML raw-text termination must
    // also be escaped. Metadata is subsequently rendered with textContent only.
    let json = String::from_utf8(json.0).map_err(|e| e.to_string())?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    // Data substitution MUST be last: metadata is never treated as a template.
    Ok(include_str!("directory_report.html")
        .replace("__REPORT_STYLE__", include_str!("directory_report.css"))
        .replace("__REPORT_SCRIPT__", include_str!("directory_report.js"))
        .replace("__QUERY_SCRIPT__", include_str!("directory_query.js"))
        .replace("__REPORT_NONCE__", &nonce)
        .replace("__REPORT_DATA__", &json))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(id: u32, directory: Option<&str>) -> Candidate {
        Candidate {
            metadata: crate::file_metadata::Metadata::default(),
            id,
            name: format!("文件-{id}.txt"),
            size: u64::MAX,
            deleted: false,
            method: "NTFS 文件表".into(),
            note: "内容未校验".into(),
            supported: true,
            directory: directory.map(str::to_owned),
            directory_note: String::new(),
            locator: Locator::Ntfs {
                link: crate::ntfs_names::Name::fixture("report"),
                record: 8491,
                sequence: 7,
                compression: None,
                stream: None,
            },
        }
    }
    fn data(html: &str) -> serde_json::Value {
        let json = html
            .split("id=\"report-data\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap();
        serde_json::from_str(json).unwrap()
    }
    #[test]
    fn report_retains_identity_empty_folders_unknown_and_exact_sizes() {
        let mut catalog = Catalog::default();
        for (id, parent, name) in [
            ("a", ROOT, "同名目录"),
            ("b", ROOT, "同名目录"),
            ("c", "a", "空目录"),
        ] {
            catalog
                .insert(Directory {
                    id: id.into(),
                    parent: Some(parent.into()),
                    name: name.into(),
                    deleted: id == "b",
                    metadata: Default::default(),
                })
                .unwrap();
        }
        let mut files = vec![
            candidate(0, Some("a")),
            candidate(1, Some("b")),
            candidate(2, Some("missing")),
            candidate(3, None),
        ];
        files[3].locator = Locator::Signature {
            content_hash: None,
            offset: 987654321,
            header: vec![],
            footer: vec![],
        };
        files[1].deleted = true;
        files[1].supported = false;
        let html = render(&catalog, &files, ScanMode::AllFiles, Completion::Ended).unwrap();
        let d = data(&html);
        assert_eq!(d["directories"].as_array().unwrap().len(), 4);
        assert_eq!(d["files"][0]["bytes"], u64::MAX.to_string());
        assert_eq!(d["files"][0]["directory"], "a");
        assert_eq!(d["files"][1]["directory"], "b");
        assert_eq!(d["files"][2]["directory"], UNKNOWN);
        assert_eq!(d["files"][3]["state"], "删除状态未知");
        assert_eq!(d["files"][1]["state"], "已删除");
        assert_eq!(d["files"][1]["supported"], false);
        assert!(!html.contains("987654321"));
        assert!(d["files"][0].get("locator").is_none());
    }
    #[test]
    fn untrusted_metadata_cannot_escape_json_or_become_markup() {
        let mut c = candidate(1, None);
        c.name = "</script><img src=https://example.invalid onerror=alert(1)> & \" '__REPORT_NONCE__' __REPORT_SCRIPT__ \u{2028}\u{2029}".into();
        c.metadata.modified = Some(crate::file_metadata::Stamp {
            instant_ns: None,
            text: c.name.clone(),
            zone: "UTC".into(),
            precision: "100 ns".into(),
        });
        let html = render(
            &Catalog::default(),
            &[c.clone()],
            ScanMode::Signatures,
            Completion::Canceled,
        )
        .unwrap();
        assert_eq!(data(&html)["files"][0]["name"], c.name);
        assert_eq!(
            data(&html)["files"][0]["metadata"]["modified"]["text"],
            c.name
        );
        assert!(!html.contains("</script><img"));
        assert_eq!(html.matches("<script").count(), 2);
        assert!(html.contains("connect-src 'none'"));
        assert!(html.contains("form-action 'none'"));
        assert!(data(&html)["completion"].as_str().unwrap().contains("取消"));
    }
    #[test]
    fn report_bounds_reject_duplicates_oversized_metadata_and_output() {
        let c = candidate(1, None);
        assert!(render(
            &Catalog::default(),
            &[c.clone(), c.clone()],
            ScanMode::Deleted,
            Completion::Failed
        )
        .is_err());
        let mut huge = c.clone();
        huge.note = "x".repeat(16385);
        assert!(render(
            &Catalog::default(),
            &[huge],
            ScanMode::Deleted,
            Completion::Failed
        )
        .is_err());
        assert!(render(
            &Catalog::default(),
            &vec![c; MAX_RESULTS + 1],
            ScanMode::Deleted,
            Completion::Failed
        )
        .is_err());
        let mut out = Bounded(vec![0; MAX_JSON - 1]);
        assert!(out.write_all(b"xy").is_err());
        assert_eq!(out.0.len(), MAX_JSON - 1);
    }
    #[test]
    fn complete_multilevel_browser_fixture_and_partial_status() {
        let mut catalog = Catalog::default();
        for (id, parent, name) in [
            ("work", ROOT, "工作资料"),
            ("photos", ROOT, "旅行照片"),
            ("reports", "work", "年度报告"),
            ("empty", ROOT, "空目录"),
        ] {
            catalog
                .insert(Directory {
                    id: id.into(),
                    parent: Some(parent.into()),
                    name: name.into(),
                    deleted: id == "reports",
                    metadata: Default::default(),
                })
                .unwrap();
        }
        let files: Vec<_> = (0..205)
            .map(|id| {
                let mut c = candidate(
                    id,
                    if id % 5 == 0 {
                        None
                    } else {
                        Some(["work", "photos", "reports"][id as usize % 3])
                    },
                );
                c.name = if id == 204 {
                    "</script><img src=x onerror=alert(1)>.txt".into()
                } else {
                    format!("项目报告-{id}.txt")
                };
                c.size = (id as u64 + 1) * 1048576;
                c.deleted = id % 2 == 0;
                c.supported = id % 17 != 0;
                c.metadata = crate::file_metadata::Metadata {
                    source: Some("NTFS 标准信息".into()),
                    modified: Some(crate::file_metadata::Stamp {
                        instant_ns: None,
                        text: "2026-11-12 10:30:20.1234567".into(),
                        zone: "UTC".into(),
                        precision: "100 ns".into(),
                    }),
                    attributes: vec!["只读".into(), "归档".into()],
                    raw_flags: Some("0x00000021".into()),
                    ..Default::default()
                };
                c
            })
            .collect();
        let html = render(&catalog, &files, ScanMode::AllFiles, Completion::Failed).unwrap();
        assert_eq!(data(&html)["files"].as_array().unwrap().len(), 205);
        assert!(data(&html)["completion"]
            .as_str()
            .unwrap()
            .contains("未正常结束"));
        // Optional developer artifact. Ordinary tests remain entirely in memory.
        if let Some(path) = std::env::var_os("DISKSTUDIO_REPORT_FIXTURE") {
            std::fs::write(path, html).unwrap();
        }
    }
}

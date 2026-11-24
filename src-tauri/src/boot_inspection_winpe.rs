use super::{Artifact, Mode, Report};
use crate::native_locale::Locale;
use std::fmt::Write as _;

fn inline(text: &str) -> String {
    text.chars()
        .take(512)
        .map(|value| if value.is_control() { ' ' } else { value })
        .collect()
}

fn artifact(output: &mut String, file: &Artifact, locale: &Locale) {
    let status = locale.text(match file.state {
        "present" => "已找到",
        "missing" => "未找到",
        "invalid" => "无效",
        _ => "无法读取",
    });
    let _ = writeln!(output, "  {} · {status}", inline(&file.path));
    if let Some(digest) = &file.sha256 {
        let _ = writeln!(output, "    SHA-256  {digest}");
    }
    if let Some(detail) = &file.detail {
        let _ = writeln!(output, "    {}", inline(locale.text(detail)));
    }
}

impl Report {
    pub(crate) fn winpe_text(&self, locale: &Locale) -> String {
        let mut text = String::new();
        let layout = match self.scheme.as_str() {
            "partition" => locale.text("分区镜像"),
            scheme => scheme,
        };
        let _ = writeln!(text, "{}  ·  {layout}", inline(&self.source));
        if !self.layout_warning.is_empty() {
            let _ = writeln!(text, "{}", inline(locale.text(&self.layout_warning)));
        }
        if let Some(present) = self.mbr_code_present {
            let _ = writeln!(
                text,
                "{}",
                locale.text(if present {
                    "MBR 含启动代码"
                } else {
                    "MBR 启动代码为空"
                })
            );
        }
        if self.volumes.is_empty() {
            let _ = writeln!(
                text,
                "\n{}",
                locale.text("分区表中没有 Windows 启动候选分区。")
            );
        }
        for volume in &self.volumes {
            let mode = match volume.mode {
                Mode::Uefi => "UEFI",
                Mode::Bios => "BIOS",
            };
            let _ = writeln!(
                text,
                "\n{mode}  ·  {}  ·  {}",
                inline(&volume.name),
                volume
                    .filesystem
                    .unwrap_or_else(|| locale.text("文件系统未知"))
            );
            let _ = writeln!(
                text,
                "  {}  {} B   {}  {} B",
                locale.text("起始偏移"),
                volume.offset,
                locale.text("容量"),
                volume.size
            );
            if let Some(error) = &volume.error {
                let _ = writeln!(text, "  {}", inline(locale.text(error)));
            }
            for file in &volume.artifacts {
                artifact(&mut text, file, locale);
            }
            if let Some(store) = &volume.store {
                let _ = writeln!(
                    text,
                    "  {}  {}",
                    locale.text("BCD 启动项"),
                    store.entries.len()
                );
                for entry in store.entries.iter().take(24) {
                    let _ = writeln!(
                        text,
                        "    {}  {}",
                        entry.id,
                        inline(entry.path.as_deref().unwrap_or_default())
                    );
                }
                if store.entries.len() > 24 {
                    let _ = writeln!(text, "    {}", locale.text("仅显示前 24 个启动项。"));
                }
            }
        }
        text
    }
}

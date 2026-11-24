//! Verified physical sources for the native WinPE workspace.
use super::image::{self, Opened, Source};
use crate::{
    backup_archive,
    data_tasks::Selection,
    data_windows::{self, Resolved},
    image_browser::{Browser, Kind, Options, ReaderFactory},
    lost_search_scope,
    native_locale::Locale,
    partitions::Target,
    storage,
    table_archive::{self, Extent},
    table_restore,
    table_restore_task::{Outcome, RetainedRecovery},
};
use std::{
    fmt::Write,
    sync::{atomic::AtomicBool, Arc},
};

// The table parser accepts a logical image basename. Keep the UI disk label
// separate from this internal name used for already-verified logical bytes.
pub(super) const LOGICAL_NAME: &str = "physical-disk.img";

#[derive(Clone)]
pub(super) struct Choice {
    pub(super) label: String,
    selection: Selection,
    number: u32,
    size: u64,
    sector: u64,
}

pub(super) fn inventory(locale: &Locale, cancel: &AtomicBool) -> Result<Vec<Choice>, String> {
    image::active(cancel)?;
    let snapshot = storage::read_snapshot()?;
    image::active(cancel)?;
    Ok(snapshot
        .disks
        .into_iter()
        .filter_map(|disk| {
            let hardware = disk.hardware;
            let selection = Selection {
                target: Target::try_from(hardware.identity?).ok()?,
                layout_key: hardware.layout_key?,
                partition: None,
            };
            let model: String = hardware
                .model
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect();
            let name = locale.format("磁盘 {number}", &[("number", &hardware.number.to_string())]);
            Some(Choice {
                label: format!(
                    "{name} · {} MiB · {} B · {model}",
                    disk.size / disk_core::MIB,
                    disk.sector_size
                ),
                selection,
                number: hardware.number,
                size: disk.size,
                sector: disk.sector_size,
            })
        })
        .collect())
}

// Reopen a verified read handle per job and reject changed raw table sectors.
pub(super) struct Physical {
    selection: Selection,
    snapshot: Resolved,
    table: Vec<(u64, Vec<u8>)>,
}

impl Physical {
    pub(super) fn target(&self) -> &Target {
        &self.selection.target
    }

    pub(super) fn number(&self) -> u32 {
        self.snapshot.disk.number
    }

    fn scheme(&self) -> &str {
        scheme(&self.snapshot)
    }

    pub(super) fn windows_layout(&self) -> (&str, &[crate::used_space::Span]) {
        (self.scheme(), &self.snapshot.partition_ranges)
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        self.reader().map(drop)
    }

    fn can_recover(&self) -> bool {
        let disk = &self.snapshot.disk;
        self.scheme() == "GPT"
            && disk.is_offline == Some(false)
            && disk.is_read_only == Some(false)
            && disk.is_system == Some(false)
            && disk.is_boot == Some(false)
    }
}

pub(super) fn can_recover(source: &Source) -> bool {
    source.browser.info.partition_table.as_deref() == Some("GPT")
        && source.disk().is_some_and(Physical::can_recover)
}

pub(super) struct Recovery {
    target: String,
    plan: RetainedRecovery,
}

impl Recovery {
    pub(super) fn prepare(
        source: &Arc<Source>,
        marked: &[Arc<super::partitions::Volume>],
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let disk = source
            .disk()
            .filter(|_| can_recover(source))
            .ok_or("登记到实体盘需要已核对的普通可写 GPT 磁盘。")?;
        let candidates = marked
            .iter()
            .map(|volume| {
                if !Arc::ptr_eq(source, &volume.source) {
                    return Err("候选已失效，请重新搜索。");
                }
                volume
                    .registration_candidate()
                    .map(|(range, candidate)| (range, candidate.clone()))
                    .ok_or("该候选仅允许浏览和提取，无法登记分区记录。")
            })
            .collect::<Result<Vec<_>, _>>()?;
        image::active(cancel)?;
        disk.verify()?;
        let plan = RetainedRecovery::prepare(
            disk.selection.clone(),
            candidates,
            |current| {
                if !current
                    .extents
                    .iter()
                    .map(|extent| (extent.offset, &extent.bytes))
                    .eq(disk.table.iter().map(|(offset, bytes)| (*offset, bytes)))
                {
                    return Err("实体磁盘的分区表已变化，请重新选择。".into());
                }
                Ok(())
            },
            cancel,
        )?;
        disk.verify()?;
        image::active(cancel)?;
        Ok(Self {
            target: source.browser.info.name.clone(),
            plan,
        })
    }

    pub(super) fn summary(&self, locale: &Locale) -> String {
        let preview = self.plan.preview();
        let details = locale.format(
            "将在 GPT 新增 {count} 个数据分区条目，写入 {ranges} 个表范围，共 {bytes} 字节。",
            &[
                ("count", &preview.changes.len().to_string()),
                ("ranges", &preview.ranges.to_string()),
                ("bytes", &preview.write_bytes.to_string()),
            ],
        );
        format!(
            "{}\n\n{details}\n\n{}",
            self.target,
            locale.text(
                "确认后锁定卷并使磁盘脱机。只写入 GPT 表，逐范围回读 SHA-256。磁盘保持脱机。"
            ),
        )
    }

    pub(super) fn preview(&self, locale: &Locale) -> String {
        let mut text = self.summary(locale);
        text.push_str("\n\n");
        let _ = writeln!(text, "{}", locale.text(
            "根据文件系统证据确定分区范围，并为新增条目生成 GUID。预览在 5 分钟后失效。",
        ));
        for change in &self.plan.preview().changes {
            if let Some(entry) = &change.after {
                let _ = writeln!(text, "{}", describe_entry(entry));
            }
        }
        text
    }

    pub(super) fn execute(self, locale: &Locale, cancel: &AtomicBool) -> Result<String, String> {
        let Outcome { verification, .. } = self.plan.execute(cancel)?;
        let report = locale.format(
            "实体盘登记完成。已回读核对 {ranges} 个 GPT 范围，共 {bytes} 字节。Windows 原生分区布局一致，磁盘保持脱机。",
            &[
                ("ranges", &verification.ranges.to_string()),
                ("bytes", &verification.written_bytes.to_string()),
            ],
        );
        Ok(format!(
            "{report}\n\n{}\nSHA-256 {}",
            self.target, verification.sha256
        ))
    }
}

fn describe_entry(entry: &table_restore::Entry) -> String {
    let name_end = entry
        .name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(entry.name.len());
    format!(
        "{} · {} B + {} B · {}",
        String::from_utf16_lossy(&entry.name[..name_end]),
        entry.offset,
        entry.size,
        entry.guid,
    )
}

#[test]
fn physical_preview_preserves_bounds_after_a_padded_gpt_name() {
    let mut name: Vec<_> = "Recovered FAT12".encode_utf16().collect();
    name.resize(36, 0);
    let entry = table_restore::Entry {
        slot: 2,
        guid: "cf9a27ae-dc7b-4f30-b46a-29749f8280a4".into(),
        kind: crate::partitions::policy::BASIC_DATA.into(),
        offset: 1048576,
        size: 4194304,
        attributes: 0,
        name,
    };
    assert_eq!(
        describe_entry(&entry),
        "Recovered FAT12 · 1048576 B + 4194304 B · cf9a27ae-dc7b-4f30-b46a-29749f8280a4"
    );
}

impl ReaderFactory for Physical {
    fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
        let fresh = data_windows::resolve(&self.selection, false, false)?;
        if fresh.disk != self.snapshot.disk
            || fresh.partition_ranges != self.snapshot.partition_ranges
        {
            return Err("实体磁盘的身份、布局或状态已变化，请重新选择。".into());
        }
        let mut reader = data_windows::reader(&fresh)?;
        for (offset, bytes) in &self.table {
            if table_archive::read_at(&mut reader, *offset, bytes.len(), fresh.size)? != *bytes {
                return Err("实体磁盘的分区表已变化，请重新选择。".into());
            }
        }
        Ok(Box::new(reader))
    }
}

fn scheme(disk: &Resolved) -> &str {
    match disk.disk.partition_style {
        Some(1) => "MBR",
        Some(2) => "GPT",
        _ => "RAW",
    }
}

fn browser(
    factory: Arc<dyn ReaderFactory>,
    label: &str,
    format: &str,
    options: Options,
    cancel: &AtomicBool,
) -> Result<Browser, String> {
    let mut browser = Browser::from_factory(
        factory,
        LOGICAL_NAME.into(),
        options,
        format.into(),
        Vec::new(),
        String::new(),
        || image::active(cancel),
    )?;
    browser.info.name = label.into();
    Ok(browser)
}

pub(super) fn open(choice: Choice, locale: &Locale, cancel: &AtomicBool) -> Result<Opened, String> {
    image::active(cancel)?;
    let snapshot = data_windows::resolve(&choice.selection, false, false)?;
    if snapshot.disk.number != choice.number
        || snapshot.size != choice.size
        || snapshot.disk.logical_sector_size != Some(choice.sector)
    {
        return Err("实体磁盘的身份、布局或状态已变化，请重新选择。".into());
    }
    Options {
        kind: Kind::Disk,
        sector: choice.sector,
    }
    .validate(choice.size)?;
    if choice.size < 4 * disk_core::MIB {
        return Err("实体磁盘至少需要 4 MiB。".into());
    }
    let mut reader = data_windows::reader(&snapshot)?;
    let archive = table_archive::capture(
        &mut reader,
        choice.size,
        choice.sector,
        choice.selection.target.0.clone(),
        false,
        || image::active(cancel),
    );
    let (kind, table, diagnostic) = match archive {
        Ok(archive) => {
            lost_search_scope::corroborate_layout(
                &archive.validate()?,
                scheme(&snapshot),
                &snapshot.partition_ranges,
            )?;
            let table = archive
                .extents
                .into_iter()
                .map(|Extent { offset, bytes, .. }| (offset, bytes))
                .collect();
            (Kind::Disk, table, None)
        }
        Err(error) => {
            image::active(cancel)?;
            let mut table = Vec::with_capacity(2);
            for offset in [0, choice.size - choice.sector] {
                table.push((
                    offset,
                    table_archive::read_at(
                        &mut reader,
                        offset,
                        choice.sector as usize,
                        choice.size,
                    )?,
                ));
            }
            (Kind::Search, table, Some(error))
        }
    };
    drop(reader);
    let disk = Arc::new(Physical {
        selection: choice.selection,
        snapshot,
        table,
    });
    let browser = browser(
        disk.clone(),
        &choice.label,
        locale.text("实体磁盘"),
        Options {
            kind,
            sector: choice.sector,
        },
        cancel,
    )?;
    let report = match diagnostic {
        Some(error) => format!(
            "{}\n\n{}",
            locale.text("无法核对实体磁盘分区表。可以检查分区表、查看扇区或指定范围搜索。"),
            locale.text(&error)
        ),
        None => image::boot_report(&browser, locale, cancel)?,
    };
    disk.verify()?;
    image::active(cancel)?;
    let report = format!(
        "{}\n{}\n\n{report}",
        locale.text("已只读打开实体磁盘。"),
        choice.label
    );
    Ok(Opened {
        source: Source::physical(browser, disk),
        report,
    })
}

#[test]
fn verified_logical_bytes_keep_the_physical_disk_label() {
    struct Fixture(Vec<u8>);
    impl ReaderFactory for Fixture {
        fn reader(&self) -> Result<Box<dyn backup_archive::Source>, String> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
    }

    let mut bytes = vec![0_u8; 8 * 1024 * 1024];
    bytes[446 + 4] = 1;
    bytes[454..458].copy_from_slice(&256_u32.to_le_bytes());
    bytes[458..462].copy_from_slice(&1024_u32.to_le_bytes());
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
    let opened = browser(
        Arc::new(Fixture(bytes)),
        "Disk 0 · 8 MiB · 4096 B · NVMe",
        "Physical disk",
        Options {
            kind: Kind::Disk,
            sector: 4096,
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(opened.info.name, "Disk 0 · 8 MiB · 4096 B · NVMe");
    assert_eq!(opened.info.partitions[0].start, 1024 * 1024);
    assert_eq!(opened.info.partition_table.as_deref(), Some("MBR"));
}

//! Search and read lost volumes in the retained WinPE image.
use super::image::Source;
use crate::{
    backup_archive,
    image_browser::Kind,
    lost_partitions, lost_search_scope,
    native_locale::Locale,
    recovery_export::ImageSource,
    surface_scan::{Options, Range},
    table_archive,
};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc,
};

pub(super) enum Scope {
    Whole,
    Unallocated,
    Lba { first: u64, last: u64 },
}

impl Scope {
    fn ranges(
        self,
        source: &Source,
        reader: &mut Box<dyn backup_archive::Source>,
        cancel: &AtomicBool,
    ) -> Result<Vec<Range>, String> {
        let info = &source.browser.info;
        let sector = info.options.sector;
        if info.options.kind == Kind::Partition {
            return Err("请选择整盘镜像以搜索丢失分区。".into());
        }
        let range = match self {
            Self::Whole => Range {
                offset: 0,
                size: info.size,
                sector,
            },
            Self::Lba { first, last } => {
                let count = last
                    .checked_sub(first)
                    .and_then(|n| n.checked_add(1))
                    .ok_or("起止 LBA 范围无效。")?;
                Options {
                    start_sector: first,
                    sector_count: count,
                }
                .range(0, info.size, sector)?
            }
            Self::Unallocated => {
                if info.options.kind != Kind::Disk {
                    return Err("搜索全部未分配空间需要完整的 GPT 或 MBR 分区表。".into());
                }
                let archive = table_archive::capture(
                    reader,
                    info.size,
                    sector,
                    format!("{:x}", Sha256::digest(info.id.as_bytes())),
                    false,
                    || super::image::active(cancel),
                )?;
                return lost_search_scope::unallocated(
                    &archive,
                    source.disk().map(|disk| disk.windows_layout()),
                );
            }
        };
        Ok(vec![range])
    }
}

#[derive(Default)]
pub(super) struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    found: AtomicUsize,
}

impl Progress {
    pub(super) fn message(&self, locale: &Locale) -> String {
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let found = self.found.load(Ordering::Relaxed);
        if total == 0 {
            return locale.text("正在核对分区搜索范围…").into();
        }
        format!(
            "{}\n\n{done} / {total} B\n{} {found}",
            locale.text("搜索丢失分区"),
            locale.text("分区候选")
        )
    }
}

enum Origin {
    Table(u32),
    Found {
        candidate: lost_partitions::Candidate,
        range: Range,
    },
}

pub(super) struct Volume {
    pub(super) source: Arc<Source>,
    origin: Origin,
}

impl Volume {
    pub(super) fn table(source: Arc<Source>, id: u32) -> Self {
        Self {
            source,
            origin: Origin::Table(id),
        }
    }

    pub(super) fn can_browse(&self) -> bool {
        match &self.origin {
            Origin::Table(_) => true,
            Origin::Found { candidate, .. } => candidate.can_browse,
        }
    }

    pub(super) fn registration_candidate(&self) -> Option<(Range, &lost_partitions::Candidate)> {
        match &self.origin {
            Origin::Found { candidate, range } if candidate.can_restore => {
                Some((*range, candidate))
            }
            _ => None,
        }
    }

    pub(super) fn title(&self, locale: &Locale) -> Result<String, String> {
        Ok(match &self.origin {
            Origin::Table(id) => {
                let partition = self.source.browser.part(*id)?;
                format!(
                    "{} · {} MiB",
                    self.display_partition(&partition.name, locale),
                    partition.size / disk_core::MIB
                )
            }
            Origin::Found { .. } => format!(
                "[{}] {}",
                locale.text("分区候选"),
                self.provenance()?.partition
            ),
        })
    }

    pub(super) fn display_partition(&self, name: &str, locale: &Locale) -> String {
        if matches!(self.origin, Origin::Table(_))
            && self.source.browser.info.partition_table.as_deref() == Some("MBR")
        {
            for (prefix, template) in [
                ("主分区 ", "主分区 {number}"),
                ("逻辑分区 ", "逻辑分区 {number}"),
            ] {
                if let Some(number) = name
                    .strip_prefix(prefix)
                    .filter(|value| value.parse::<u32>().is_ok())
                {
                    return locale.format(template, &[("number", number)]);
                }
            }
        }
        name.to_owned()
    }

    pub(super) fn describe(&self, locale: &Locale) -> Option<String> {
        let Origin::Found { candidate, range } = &self.origin else {
            return None;
        };
        let availability = if candidate.can_browse {
            "可读取文件目录。"
        } else {
            "当前候选缺少可用于浏览的完整文件系统证据。"
        };
        Some(format!(
            "{} · LBA {} · {} B\n{}\n\n{}\n{}",
            candidate.filesystem,
            candidate.offset / range.sector,
            candidate.size,
            locale.text(availability),
            locale.text(&candidate.evidence),
            locale.text(&candidate.warning),
        ))
    }

    pub(super) fn provenance(&self) -> Result<ImageSource, String> {
        let browser = &self.source.browser;
        match &self.origin {
            Origin::Table(id) => browser.scope_provenance(Some(*id)),
            Origin::Found { candidate, range } => Ok(ImageSource {
                file_name: browser.info.name.clone(),
                format: browser.info.format.clone(),
                partition: format!(
                    "{} · LBA {}",
                    candidate.filesystem,
                    candidate.offset / range.sector
                ),
                offset: candidate.offset.to_string(),
                size: candidate.size.to_string(),
                sector: range.sector,
                parallels_snapshot: None,
            }),
        }
    }

    pub(super) fn reader(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Box<dyn backup_archive::Source>, String> {
        super::image::active(cancel)?;
        let browser = &self.source.browser;
        match &self.origin {
            Origin::Table(id) => Ok(Box::new(browser.reader(*id)?)),
            Origin::Found { candidate, range } if candidate.can_browse => {
                Ok(Box::new(lost_partitions::candidate_reader(
                    browser.logical_reader()?,
                    browser.info.size,
                    browser.info.options.sector,
                    *range,
                    candidate,
                    cancel,
                )?))
            }
            Origin::Found { .. } => Err("当前候选缺少可用于浏览的完整文件系统证据。".into()),
        }
    }
}

pub(super) struct Results {
    pub(super) volumes: Vec<Volume>,
    done: u64,
    total: u64,
    canceled: bool,
    error: Option<String>,
}

impl Results {
    pub(super) fn describe(&self, locale: &Locale) -> String {
        let status = match (self.canceled, &self.error) {
            (true, _) => locale.text("已停止搜索。保留已找到的候选。"),
            (false, Some(_)) => locale.text("搜索未完成。保留已找到的候选。"),
            (false, None) => locale.text("分区搜索完成。"),
        };
        let mut text = format!(
            "{status}\n\n{} / {} B\n{} {}",
            self.done,
            self.total,
            locale.text("分区候选"),
            self.volumes.len(),
        );
        if let (false, Some(error)) = (self.canceled, &self.error) {
            text.push_str("\n\n");
            text.push_str(locale.text(error));
        }
        text
    }
}

pub(super) fn search(
    source: Arc<Source>,
    scope: Scope,
    cancel: Arc<AtomicBool>,
    progress: &Progress,
) -> Result<Results, String> {
    let mut reader = source.browser.logical_reader()?;
    let ranges = scope.ranges(&source, &mut reader, &cancel)?;
    let total = ranges.iter().map(|range| range.size).sum();
    progress.total.store(total, Ordering::Relaxed);
    let mut volumes = Vec::new();
    let error = lost_partitions::scan_ranges(reader, &ranges, &cancel, |done, found| {
        progress.done.store(done, Ordering::Relaxed);
        if let Some(candidate) = found {
            let range = lost_search_scope::containing(&ranges, candidate.offset, candidate.size)?;
            volumes.push(Volume {
                source: source.clone(),
                origin: Origin::Found { candidate, range },
            });
            progress.found.store(volumes.len(), Ordering::Relaxed);
        }
        Ok(())
    })
    .err();
    source.verify_disk()?;
    let done = if error.is_none() {
        total
    } else {
        progress.done.load(Ordering::Relaxed)
    };
    Ok(Results {
        volumes,
        done,
        total,
        canceled: error.is_some() && cancel.load(Ordering::Relaxed),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{image_browser::Options, native_locale::Language};
    use disk_core::{
        fat_format::{FatType, Plan},
        MIB,
    };
    use std::{
        fs,
        io::{Cursor, Write},
    };

    #[test]
    fn searches_only_the_requested_extents_and_reopens_found_volumes() {
        for sector in [512, 4096] {
            let path = std::env::temp_dir().join(format!(
                "diskstudio-winpe-search-{}.img",
                uuid::Uuid::new_v4()
            ));
            let mut image = vec![0; (12 * MIB) as usize];
            let mut volume = Cursor::new(vec![0; (2 * MIB) as usize]);
            Plan::new(FatType::Fat12, 2 * MIB, sector as u32, 0, "LOST", 123, 0)
                .unwrap()
                .format(&mut volume, |_, _| Ok(()))
                .unwrap();
            for start in [MIB, 4 * MIB, 8 * MIB] {
                image[start as usize..(start + 2 * MIB) as usize].copy_from_slice(volume.get_ref());
            }
            image[446 + 4] = 0x01;
            image[454..458].copy_from_slice(&((4 * MIB / sector) as u32).to_le_bytes());
            image[458..462].copy_from_slice(&((2 * MIB / sector) as u32).to_le_bytes());
            image[510..512].copy_from_slice(&[0x55, 0xaa]);
            let mut output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .unwrap();
            output.write_all(&image).unwrap();
            drop(output);

            let locale = Locale::new(Language::En);
            let cancel = Arc::new(AtomicBool::new(false));
            let options = Options {
                kind: Kind::Disk,
                sector,
            };
            let opened = super::super::image::open(&path, options, &locale, &cancel).unwrap();
            let source = Arc::new(opened.source);
            assert_eq!(source.browser.info.partitions.len(), 1);
            let table_volume = Volume::table(source.clone(), 0);
            assert_eq!(
                table_volume.title(&locale).unwrap(),
                "Primary partition 1 · 2 MiB"
            );
            assert_eq!(table_volume.provenance().unwrap().partition, "主分区 1");
            drop(table_volume);
            let run = |scope| {
                search(source.clone(), scope, cancel.clone(), &Progress::default()).unwrap()
            };
            let results = run(Scope::Unallocated);
            assert!(results.error.is_none());
            let offsets = results
                .volumes
                .iter()
                .map(|volume| {
                    assert!(volume.can_browse());
                    assert!(volume.reader(&cancel).is_ok());
                    volume.provenance().unwrap().offset.parse::<u64>().unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(offsets, vec![MIB, 8 * MIB]);
            drop(results);
            assert_eq!(run(Scope::Whole).volumes.len(), 3);
            let results = run(Scope::Lba {
                first: 8 * MIB / sector,
                last: 10 * MIB / sector - 1,
            });
            assert_eq!(results.volumes.len(), 1);
            assert_eq!(
                results.volumes[0].provenance().unwrap().offset,
                (8 * MIB).to_string()
            );
            drop(results);
            cancel.store(true, Ordering::Relaxed);
            assert!(run(Scope::Whole).canceled);
            drop(source);
            assert_eq!(fs::read(&path).unwrap(), image);

            image[..sector as usize].fill(0);
            fs::write(&path, &image).unwrap();
            cancel.store(false, Ordering::Relaxed);
            let opened = super::super::image::open(&path, options, &locale, &cancel).unwrap();
            let source = Arc::new(opened.source);
            assert_eq!(source.browser.info.options.kind, Kind::Search);
            let run = |scope| search(source.clone(), scope, cancel.clone(), &Progress::default());
            assert!(run(Scope::Unallocated).is_err());
            assert!(run(Scope::Lba { first: 1, last: 0 }).is_err());
            assert_eq!(run(Scope::Whole).unwrap().volumes.len(), 3);
            drop(source);
            assert_eq!(fs::read(&path).unwrap(), image);
            fs::remove_file(path).unwrap();
        }
    }
}

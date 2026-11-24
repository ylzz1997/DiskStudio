//! Backup from a retained, read-only image. The output is an ordinary backup
//! archive of the selected logical range, independent of the image container.
use super::{
    backup_output_bound, choose_chain, complete_verified, image_browse_tasks, progress,
    validate_backup_options, BackupOptions,
};
use crate::{
    backup_archive::{self, AllocationPolicy, Chain, Source},
    backup_files, backup_secrets, data_io, data_windows,
    image_browser::{Info, Kind},
    native_locale::{self, Locale},
    partitions::Target,
    used_space::{self, Mode, Span, View},
};
use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

struct Scope {
    partition: Option<u32>,
    start: u64,
    size: u64,
    sector: u64,
}

impl Scope {
    fn from_image(info: &Info, partition: Option<u32>, mode: Mode) -> Result<Self, String> {
        if info.options.kind == Kind::Search && mode == Mode::UsedSpace {
            return Err("按已用空间备份需要通过检查的镜像分区表。".into());
        }
        let partition = partition.or_else(|| (info.options.kind == Kind::Partition).then_some(0));
        let (start, size) = match partition {
            Some(id) => {
                let part = info
                    .partitions
                    .iter()
                    .find(|p| p.id == id)
                    .ok_or("镜像分区选择已失效。")?;
                (part.start, part.size)
            }
            None => (0, info.size),
        };
        let sector = info.options.sector;
        if size == 0
            || size > backup_archive::MAX_SIZE
            || start.checked_add(size).is_none_or(|end| end > info.size)
            || !matches!(sector, 512 | 4096)
            || start % sector != 0
            || size % sector != 0
        {
            return Err("备份范围不支持".into());
        }
        Ok(Self {
            partition,
            start,
            size,
            sector,
        })
    }

    fn kind(&self) -> &'static str {
        if self.partition.is_some() {
            "partition"
        } else {
            "disk"
        }
    }

    fn reader(&self, image: &image_browse_tasks::Opened) -> Result<View<Box<dyn Source>>, String> {
        View::bounded(
            image.logical_reader()?,
            self.start,
            self.size,
            image.browser.info.size,
        )
        .map_err(|e| e.to_string())
    }
}

pub(crate) struct Backup {
    image: image_browse_tasks::Opened,
    scope: Scope,
    pub(super) path: PathBuf,
    destination: Target,
    options: BackupOptions,
    parent: Option<Chain>,
}

impl Backup {
    pub(super) fn prepare(
        id: String,
        partition: Option<u32>,
        options: BackupOptions,
    ) -> Result<Option<Self>, String> {
        validate_backup_options(&options)?;
        if options.hot {
            return Err("镜像备份不使用 VSS 热备份。".into());
        }
        let image = image_browse_tasks::opened(&id)?;
        image.validate()?;
        let scope = Scope::from_image(&image.browser.info, partition, options.mode)?;
        let sources = &image.source_disks;
        if sources.is_empty() {
            return Err("无法核对镜像所在的物理磁盘。".into());
        }
        let locale = native_locale::current();
        let parent = if options.incremental {
            let Some(path) = rfd::FileDialog::new()
                .set_title(locale.text("选择上一次备份（完整或增量）"))
                .add_filter(locale.text("DiskStudio 备份"), &["dsbackup"])
                .pick_file()
            else {
                return Ok(None);
            };
            let input = backup_files::Input::open_excluding(&path, sources)?;
            let Some(chain) = choose_chain(input, sources)? else {
                return Ok(None);
            };
            let header = chain.header();
            if header.kind != scope.kind()
                || header.size != scope.size
                || header.sector != scope.sector
                || chain.depth() >= backup_archive::MAX_CHAIN
            {
                return Err("基础备份与所选范围不匹配，或已达到 16 层上限。".into());
            }
            Some(chain)
        } else {
            None
        };
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text("保存磁盘/分区备份"))
            .add_filter(locale.text("DiskStudio 备份"), &["dsbackup"])
            .set_file_name("DiskStudio-image.dsbackup")
            .save_file()
        else {
            return Ok(None);
        };
        let name = backup_files::backup_name(&path)?;
        let required = if options.mode == Mode::UsedSpace {
            0
        } else {
            backup_output_bound(scope.size, scope.size, options.encryption)?
        };
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效")?,
            sources,
            required,
        )?;
        let path = folder.join(name);
        match path.symlink_metadata() {
            Ok(_) => return Err("为避免覆盖，请选择新的备份文件名。".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        Ok(Some(Self {
            image,
            scope,
            path,
            destination,
            options,
            parent,
        }))
    }

    pub(super) fn label(&self) -> Result<String, String> {
        let browser = &self.image.browser;
        let mut label = browser.scope_label(self.scope.partition)?;
        if let Some(point) = &browser.info.parallels_snapshot {
            label.push_str(&format!(" · {} ({})", point.name, point.id));
        }
        Ok(label)
    }

    pub(super) fn size(&self) -> u64 {
        self.scope.size
    }

    pub(super) fn warning(&self, locale: &Locale) -> String {
        let options = &self.options;
        locale.sentences(&[
            "备份使用所选镜像的逻辑内容，镜像源保持只读。",
            if options.incremental {
                "只保存相对上次备份变化的数据块；还原必须保留完整依赖链。"
            } else if options.mode == Mode::UsedSpace {
                "创建完整的已用空间备份。"
            } else {
                "完整备份包含空闲空间。"
            },
            if options.mode == Mode::UsedSpace {
                "已确认空闲的区域按零保存，不保留其已删除内容；未知文件系统完整保留。"
            } else {
                ""
            },
            if options.compression {
                "启用并行压缩。"
            } else {
                "不压缩。"
            },
            if options.encryption {
                "使用密码加密；打开及还原时需要密码，程序不保存密码。"
            } else {
                ""
            },
            "完成后重新读取并校验全部逻辑内容。",
        ])
    }

    pub(super) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let Self {
            image,
            scope,
            path,
            destination,
            options,
            mut parent,
        } = self;
        let size = scope.size;
        let tick = |phase, done| progress(id, cancel, phase, done, size);
        let locale = native_locale::current();
        tick("准备备份", 0)?;
        let current = image_browse_tasks::opened(&image.browser.info.id)?;
        current.validate()?;
        if let Some(parent) = &mut parent {
            parent.verify(|done| tick("验证基础备份", done))?;
        }
        let filename = backup_files::backup_name(&path)?;
        let password = if options.encryption {
            let password = backup_secrets::ask(backup_secrets::Purpose::Create, filename, || {
                tick("等待设置备份密码", 0)
            })?;
            if password.is_none() {
                cancel.store(true, Ordering::SeqCst);
                return Err("已取消加密备份，未创建输出。".into());
            }
            password
        } else {
            None
        };
        let mut reader = scope.reader(&image)?;
        let allocation = if options.mode == Mode::UsedSpace {
            tick("检查文件系统分配信息", 0)?;
            let partitions = (scope.kind() == "disk").then(|| {
                image
                    .browser
                    .info
                    .partitions
                    .iter()
                    .map(|part| Span {
                        start: part.start,
                        end: part.start + part.size,
                    })
                    .collect::<Vec<_>>()
            });
            let plan = used_space::inspect(
                &mut reader,
                size,
                scope.sector,
                partitions.as_deref(),
                cancel,
            )?;
            plan.proof.verify(&mut reader, cancel)?;
            Some(plan)
        } else {
            None
        };
        let payload = match &allocation {
            Some(plan) => plan.backup_payload_bound(backup_archive::CHUNK as u64)?,
            None => size,
        };
        let required = backup_output_bound(payload, size, options.encryption)?;
        reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        tick("备份", 0)?;
        image.validate()?;
        let (mut output, publication) = backup_files::create_excluding(
            &path,
            &image.source_disks,
            Some(&destination),
            required,
            password.as_ref(),
        )?;
        drop(password);
        let result = (|| {
            let mut content: Box<dyn Read> = match &allocation {
                Some(plan) => Box::new(plan.masked(reader)),
                None => Box::new(reader),
            };
            let header = backup_archive::create_with_settings(
                &mut content,
                &mut output,
                scope.kind(),
                size,
                scope.sector,
                options.compression(),
                parent.as_ref(),
                allocation.as_ref().map(|plan| AllocationPolicy {
                    skipped_bytes: plan.skipped,
                    map_sha256: plan.digest.clone(),
                }),
                |done| {
                    tick(
                        if options.compression {
                            "并行压缩备份"
                        } else {
                            "备份"
                        },
                        done,
                    )
                },
            )?;
            drop(content);

            image.validate()?;
            let mut reader = scope.reader(&image)?;
            let verify = |done| tick("校验镜像源", done);
            let source_sha = match &allocation {
                Some(plan) => {
                    plan.proof.verify(&mut reader, cancel)?;
                    let sha = data_io::hash_exact(&mut plan.masked(&mut reader), size, verify)?;
                    plan.proof.verify(&mut reader, cancel)?;
                    sha
                }
                None => data_io::hash_exact(&mut reader, size, verify)?,
            };
            if source_sha != header.sha256 {
                return Err("镜像内容与写入前校验不一致。".to_owned());
            }
            backup_files::verify_output(output, &publication, parent, |done| {
                tick("校验备份链", done)
            })?;
            image.validate()?;
            tick("保存已验证备份", size)?;
            let mut summary = locale.sentences(&[
                "备份完成，全部逻辑内容已通过 SHA-256 校验。",
                if options.encryption {
                    "文件已加密。"
                } else {
                    ""
                },
                if options.incremental {
                    "还原时请保留全部基础备份。"
                } else {
                    ""
                },
            ]);
            if let Some(plan) = &allocation {
                summary.push(' ');
                summary.push_str(&locale.format(
                    "按已用空间备份：{free} B 确认空闲区域按零保存，不包含这些区域的已删除内容。",
                    &[("free", &plan.skipped.to_string())],
                ));
                summary.push_str(&super::allocation_notes(plan, &locale));
            }
            let message = format!("{summary}\n{}", path.display());
            complete_verified(id, message.clone(), header.sha256.clone(), || {
                publication.publish()
            })?;
            Ok((message, Some(header.sha256)))
        })();
        result.map_err(|error: String| {
            locale.format(
                "{error} 未完成文件保留为 {name}。",
                &[
                    ("error", locale.text(&error)),
                    ("name", &format!("{filename}.partial")),
                ],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_browser::{Options, Part};

    fn image(kind: Kind, sector: u64) -> Info {
        Info {
            id: String::new(),
            name: "sample.img".into(),
            partition_table: None,
            size: 4 * 1024 * 1024,
            options: Options { kind, sector },
            partitions: vec![Part {
                id: 0,
                name: "data".into(),
                start: 4096,
                size: 1024 * 1024,
            }],
            warning: String::new(),
            verification_notes: vec![],
            format: "RAW".into(),
            files: vec!["sample.img".into()],
            parallels_snapshot: None,
        }
    }

    #[test]
    fn disk_and_partition_backup_scopes_have_distinct_geometry_and_kind() {
        let image = image(Kind::Disk, 4096);
        let disk = Scope::from_image(&image, None, Mode::UsedSpace).unwrap();
        let part = Scope::from_image(&image, Some(0), Mode::AllSectors).unwrap();
        assert_eq!(
            (disk.kind(), disk.start, disk.size, disk.sector),
            ("disk", 0, image.size, 4096)
        );
        assert_eq!(
            (part.kind(), part.start, part.size, part.sector),
            ("partition", 4096, 1024 * 1024, 4096)
        );
        assert!(Scope::from_image(&image, Some(1), Mode::AllSectors).is_err());
    }

    #[test]
    fn naked_partition_keeps_partition_kind_and_search_only_allows_complete_bytes() {
        let mut partition = image(Kind::Partition, 512);
        partition.partitions[0].start = 0;
        partition.partitions[0].size = partition.size;
        let scope = Scope::from_image(&partition, None, Mode::UsedSpace).unwrap();
        assert_eq!(
            (scope.kind(), scope.start, scope.size),
            ("partition", 0, partition.size)
        );

        let mut search = image(Kind::Search, 512);
        search.partitions.clear();
        assert!(Scope::from_image(&search, None, Mode::UsedSpace).is_err());
        assert!(Scope::from_image(&search, None, Mode::AllSectors).is_ok());
    }

    #[test]
    fn image_backup_rejects_overlarge_misaligned_and_out_of_bounds_extents() {
        let mut image = image(Kind::Disk, 4096);
        image.size = backup_archive::MAX_SIZE + 4096;
        assert!(Scope::from_image(&image, None, Mode::AllSectors).is_err());
        image.partitions[0].start = 512;
        assert!(Scope::from_image(&image, Some(0), Mode::AllSectors).is_err());
        image.partitions[0].start = image.size;
        assert!(Scope::from_image(&image, Some(0), Mode::AllSectors).is_err());
    }
}

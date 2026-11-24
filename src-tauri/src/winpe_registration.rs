//! Register checked candidates in a verified copy of the retained image.
use super::{
    image::{active, Source},
    partitions::Volume,
};
use crate::{
    backup_files, data_windows,
    image_browser::Kind,
    image_convert::{feed, Encoder, Format, Identity, Options, Pass, Verified},
    image_table_conversion::{registration, Conversion, Scheme},
    lost_partition_restore::{self, Evidence},
    native_locale::Locale,
    partitions::Target,
};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write,
    fs::File,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex},
};

pub(super) struct Plan {
    source: Arc<Source>,
    conversion: Conversion,
    evidence: Vec<Evidence>,
    output: Options,
}

impl Plan {
    pub(super) fn new(
        source: Arc<Source>,
        volumes: &[Arc<Volume>],
        options: registration::Options,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let selected = volumes
            .iter()
            .map(|volume| {
                if !Arc::ptr_eq(&volume.source, &source) {
                    return Err("分区候选与当前镜像不匹配。");
                }
                volume
                    .registration_candidate()
                    .map(|(range, candidate)| (range, candidate.clone()))
                    .ok_or("该候选仅允许浏览和提取，无法登记分区记录。")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let info = &source.browser.info;
        if info.options.kind == Kind::Partition {
            return Err("请选择整盘镜像以搜索丢失分区。".into());
        }
        let output = Options {
            format: Format::Raw,
            size: info.size,
            sector: info.options.sector,
        };
        let (conversion, evidence) = registration::prepare(
            &mut source.browser.logical_reader()?,
            output.size,
            output.sector,
            format!("{:x}", Sha256::digest(info.id.as_bytes())),
            info.options.kind == Kind::Search,
            options,
            selected,
            cancel,
        )?;
        Ok(Self {
            source,
            conversion,
            evidence,
            output,
        })
    }

    pub(super) fn describe(&self, locale: &Locale) -> String {
        let preview = &self.conversion.preview;
        let rebuilt = preview
            .registration
            .as_ref()
            .is_some_and(|item| item.rebuilt_table);
        let layout = locale.text(if rebuilt {
            "重建分区表"
        } else {
            "保留现有分区条目"
        });
        let mut report = format!(
            "{} · {} · {} B\n{} · {}\n{} {}\n{} {} B\n\n{}\n\n",
            self.source.browser.info.name,
            preview.to.name(),
            preview.sector,
            locale.text("保存含找回分区的新 RAW 镜像"),
            self.output.size,
            locale.text("磁盘标识"),
            preview.disk_id,
            locale.text("分区表写入量"),
            preview.table_bytes,
            layout,
        );
        for entry in &preview.partitions {
            let form = locale.text(if preview.to == Scheme::Gpt {
                "GPT 分区"
            } else if entry.logical {
                "逻辑分区"
            } else {
                "主分区"
            });
            let _ = writeln!(
                report,
                "LBA {} · {} B · {} · {} · {form}",
                entry.offset / preview.sector,
                entry.size,
                entry.old_type,
                entry.new_type,
            );
        }
        report.push('\n');
        report.push_str(
            locale.text("只在新镜像中登记已选候选的文件系统范围，保留现有分区条目和数据。"),
        );
        report
    }

    pub(super) fn bind(
        self,
        path: &Path,
        locale: &Locale,
        cancel: &AtomicBool,
    ) -> Result<Prepared, String> {
        active(cancel)?;
        let name = backup_files::image_name(path, Format::Raw)?;
        self.output.check_filename(name)?;
        let binding = self.source.bind().map_err(|error| {
            format!(
                "{}\n{}",
                locale.text("无法确认来源磁盘。"),
                locale.text(&error)
            )
        })?;
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效")?,
            std::slice::from_ref(binding.target()),
            self.output.minimum_output_bytes()?,
        )?;
        let path = folder.join(name);
        match path.symlink_metadata() {
            Ok(_) => return Err("目标已存在；不会覆盖已有文件。".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        binding.verify()?;
        active(cancel)?;
        Ok(Prepared {
            plan: self,
            path,
            binding,
            destination,
        })
    }

    fn verify_source(&self, cancel: &AtomicBool) -> Result<(), String> {
        let mut reader = self.source.browser.logical_reader()?;
        self.conversion
            .verify_source(&mut reader, || active(cancel))?;
        lost_partition_restore::verify(&mut reader, &self.evidence, cancel)
    }

    fn write(
        &self,
        file: File,
        name: &str,
        cancel: &AtomicBool,
        validate_source: impl Fn() -> Result<(), String>,
        validate_output: impl Fn() -> Result<(), String>,
        progress: &Progress,
    ) -> Result<Verified, String> {
        let size = self.output.size;
        let tick = |phase, done| {
            active(cancel)?;
            progress.update(phase, done, size);
            Ok::<_, String>(())
        };
        let mut encoder = Encoder::new(
            file,
            self.output.clone(),
            Identity::for_output(name.into())?,
        )?;
        for (pass, phase) in [(Pass::Copy, "写入镜像"), (Pass::Check, "重新读取源镜像")]
        {
            tick(phase, 0)?;
            validate_source()?;
            let mut source = self.source.browser.logical_reader()?;
            self.conversion
                .verify_source(&mut source, || active(cancel))?;
            lost_partition_restore::verify(&mut source, &self.evidence, cancel)?;
            feed(
                &mut self
                    .conversion
                    .reader(source)
                    .map_err(|error| error.to_string())?,
                &mut encoder,
                size,
                pass,
                |done| tick(phase, done),
            )?;
            validate_source()?;
        }
        validate_output()?;
        let (mut file, result) = encoder.finish(
            |file| file.sync_all().map_err(|error| error.to_string()),
            |done| tick("回读校验镜像", done),
        )?;
        tick("回读校验镜像分区表", size)?;
        self.conversion
            .verify_output(&mut file, || active(cancel))?;
        lost_partition_restore::verify(&mut file, &self.evidence, cancel)?;
        validate_source()?;
        active(cancel)?;
        Ok(result)
    }
}

pub(super) struct Prepared {
    plan: Plan,
    path: PathBuf,
    binding: super::image::Binding,
    destination: Target,
}

impl Prepared {
    pub(super) fn describe(&self, locale: &Locale) -> String {
        format!(
            "{}\n{}\n\n{} · {} B\n{} {}\n\n{}",
            locale.text("保存含找回分区的新 RAW 镜像"),
            self.path.display(),
            self.plan.conversion.preview.to.name(),
            self.plan.output.size,
            locale.text("分区候选"),
            self.plan.evidence.len(),
            locale.text(
                "分区容量来自当前可核对的文件系统边界。完成后回读核对分区表、候选与全部逻辑内容。"
            ),
        )
    }

    pub(super) fn execute(
        self,
        locale: &Locale,
        cancel: &AtomicBool,
        progress: &Progress,
    ) -> Result<String, String> {
        let source = || self.binding.verify();
        active(cancel)?;
        source()?;
        self.plan.verify_source(cancel)?;
        active(cancel)?;
        let name = backup_files::image_name(&self.path, Format::Raw)?;
        let (file, publication) = backup_files::create_image_output_excluding(
            &self.path,
            Format::Raw,
            std::slice::from_ref(self.binding.target()),
            &self.destination,
            self.plan.output.minimum_output_bytes()?,
        )?;
        let write = (|| {
            let result = self.plan.write(
                file,
                name,
                cancel,
                source,
                || publication.validate(),
                progress,
            )?;
            active(cancel)?;
            publication.publish()?;
            Ok::<_, String>(result)
        })();
        let result = write.map_err(|error| {
            locale.format(
                "{error} 未完成文件保留为 {name}。",
                &[
                    ("error", locale.text(&error)),
                    ("name", &format!("{name}.partial")),
                ],
            )
        })?;
        Ok(format!(
            "{}\n{}\nSHA-256 {}",
            locale.text("已在镜像副本中登记丢失分区，分区表、候选与全部逻辑内容已通过回读校验。"),
            self.path.display(),
            result.sha256,
        ))
    }
}

#[derive(Default)]
pub(super) struct Progress(Mutex<Option<(&'static str, u64, u64)>>);

impl Progress {
    fn update(&self, phase: &'static str, done: u64, total: u64) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some((phase, done, total));
    }

    pub(super) fn message(&self, locale: &Locale) -> String {
        match *self.0.lock().unwrap_or_else(|error| error.into_inner()) {
            Some((phase, done, total)) => format!("{}\n\n{done} / {total} B", locale.text(phase)),
            None => locale.text("正在核对登记来源和目标…").into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{image_browser::Options as OpenOptions, native_locale::Language, table_archive};
    use disk_core::{
        fat_format::{FatType, Plan as FatPlan},
        MIB,
    };
    use std::{fs, io::Cursor};

    #[test]
    fn registered_copy_retains_both_volumes_and_revalidates_512_and_4kn_tables() {
        for (sector, scheme, form) in [
            (512, Scheme::Gpt, registration::Form::Auto),
            (4096, Scheme::Mbr, registration::Form::Logical),
        ] {
            let name = format!("diskstudio-winpe-register-{}", uuid::Uuid::new_v4());
            let root = std::env::temp_dir().join(name);
            let source_path = root.with_extension("img");
            let output_path = root.with_extension("partial");
            let mut image = vec![0; (12 * MIB) as usize];
            let mut fat = Cursor::new(vec![0; (2 * MIB) as usize]);
            FatPlan::new(
                FatType::Fat12,
                2 * MIB,
                sector as u32,
                0,
                "RECOVERED",
                432,
                0,
            )
            .unwrap()
            .format(&mut fat, |_, _| Ok(()))
            .unwrap();
            let expected = [MIB, 6 * MIB];
            for offset in expected {
                image[offset as usize..(offset + 2 * MIB) as usize].copy_from_slice(fat.get_ref());
            }
            let file = |path: &Path| {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .unwrap()
            };
            {
                use std::io::Write;
                file(&source_path).write_all(&image).unwrap();
            }
            let locale = Locale::new(Language::En);
            let cancel = Arc::new(AtomicBool::new(false));
            let opened = super::super::image::open(
                &source_path,
                OpenOptions {
                    kind: Kind::Disk,
                    sector,
                },
                &locale,
                &cancel,
            )
            .unwrap();
            let source = Arc::new(opened.source);
            let found = super::super::partitions::search(
                source.clone(),
                super::super::partitions::Scope::Whole,
                cancel.clone(),
                &super::super::partitions::Progress::default(),
            )
            .unwrap();
            assert_eq!(found.volumes.len(), 2);
            let marked = found.volumes.into_iter().map(Arc::new).collect::<Vec<_>>();
            let plan = Plan::new(
                source,
                &marked,
                registration::Options {
                    new_table: scheme,
                    form,
                },
                &cancel,
            )
            .unwrap();
            assert_eq!(plan.conversion.preview.to, scheme);
            assert_eq!(plan.conversion.preview.partitions.len(), 2);
            assert_eq!(
                plan.conversion.preview.partitions[0].logical,
                scheme == Scheme::Mbr
            );
            assert!(plan
                .describe(&locale)
                .contains(&format!("LBA {}", 6 * MIB / sector)));
            plan.verify_source(&cancel).unwrap();
            let digest = plan
                .write(
                    file(&output_path),
                    "recovered.img",
                    &cancel,
                    || Ok(()),
                    || Ok(()),
                    &Progress::default(),
                )
                .unwrap()
                .sha256;
            let output = fs::read(&output_path).unwrap();
            assert_eq!(digest, format!("{:x}", Sha256::digest(&output)));
            let archive = table_archive::capture(
                &mut Cursor::new(&output),
                output.len() as u64,
                sector,
                digest,
                false,
                || Ok(()),
            )
            .unwrap();
            let info = archive.validate().unwrap();
            assert_eq!(info.scheme, scheme.name());
            assert_eq!(
                info.layout
                    .iter()
                    .map(|entry| (entry.offset, entry.size))
                    .collect::<Vec<_>>(),
                expected.map(|offset| (offset, 2 * MIB))
            );
            let mut unpatched = output;
            for extent in archive.extents {
                let start = extent.offset as usize;
                let end = start + extent.bytes.len();
                unpatched[start..end].copy_from_slice(&image[start..end]);
            }
            assert_eq!(unpatched, image);
            assert_eq!(fs::read(&source_path).unwrap(), image);
            drop(plan);
            drop(marked);
            fs::remove_file(output_path).unwrap();
            fs::remove_file(source_path).unwrap();
        }
    }
}

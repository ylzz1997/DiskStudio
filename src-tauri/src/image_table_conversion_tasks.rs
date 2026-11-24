//! Explicit copy from an immutable image capability to a separate physical disk.
use super::{
    complete_verified,
    image_browse_tasks, progress, retained_partition_tasks,
};
use crate::{
    backup_files, data_windows,
    image_browser::Kind,
    image_convert::{feed, Encoder, Format, Identity, Options, Pass},
    image_table_conversion::{gpt_repair, registration, Conversion, Preview, Scheme},
    lost_partition_restore::{self, Evidence},
    native_locale::{self, Locale},
    partitions::Target,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[path = "image_table_backup_tasks.rs"]
mod table_backup;
use table_backup::RestoreInput;
pub(super) use table_backup::Save;

pub(crate) struct Convert {
    opened: image_browse_tasks::Opened,
    conversion: Conversion,
    path: PathBuf,
    destination: Target,
    output: Options,
    restore: Option<RestoreInput>,
    registration: Option<Register>,
}
struct Register {
    search: String,
    ids: Vec<String>,
    evidence: Vec<Evidence>,
}
pub(super) enum Change {
    Table(Scheme),
    Placement(u32),
    Restore,
    Repair {
        side: gpt_repair::Side,
        digest: String,
    },
    Register {
        search: String,
        ids: Vec<String>,
        options: registration::Options,
    },
}
pub(super) fn inspect_gpt(id: String) -> Result<gpt_repair::Report, String> {
    let opened = image_browse_tasks::opened(&id)?;
    opened.validate()?;
    let info = &opened.browser.info;
    if info.options.kind == Kind::Partition || opened.source_disks.is_empty() {
        return Err("GPT 修复需要已核对来源的整盘镜像。".into());
    }
    let report = gpt_repair::inspect(
        &mut opened.logical_reader()?,
        info.size,
        info.options.sector,
    )?;
    opened.validate()?;
    Ok(report)
}
impl Convert {
    pub(super) fn prepare(id: String, change: Change) -> Result<Option<Self>, String> {
        let opened = image_browse_tasks::opened(&id)?;
        opened.validate()?;
        let info = &opened.browser.info;
        let allowed = info.options.kind == Kind::Disk
            || (matches!(
                change,
                Change::Restore | Change::Repair { .. } | Change::Register { .. }
            ) && info.options.kind == Kind::Search);
        if !allowed || opened.source_disks.is_empty() {
            return Err("分区表转换需要已核对来源的整盘镜像。".into());
        }
        let output = Options {
            format: Format::Raw,
            size: info.size,
            sector: info.options.sector,
        };
        let required = output.minimum_output_bytes()?;
        let key = format!("{:x}", Sha256::digest(id.as_bytes()));
        let mut reader = opened.logical_reader()?;
        let mut restore = None;
        let mut register = None;
        let conversion = match change {
            Change::Table(table) => Conversion::prepare(
                &mut reader,
                info.size,
                info.options.sector,
                key,
                table,
                || Ok(()),
            )?,
            Change::Placement(partition) => {
                let part = opened.browser.part(partition)?;
                Conversion::prepare_placement(
                    &mut reader,
                    info.size,
                    info.options.sector,
                    key,
                    (part.start, part.size),
                    || Ok(()),
                )?
            }
            Change::Restore => {
                let Some(input) = RestoreInput::choose()? else {
                    return Ok(None);
                };
                let conversion = crate::image_table_conversion::recovery::prepare(
                    &mut reader,
                    info.size,
                    info.options.sector,
                    input.archive.clone(),
                    input.name().into(),
                )?;
                restore = Some(input);
                conversion
            }
            Change::Repair { side, ref digest } => {
                gpt_repair::prepare(&mut reader, info.size, info.options.sector, side, digest)?
            }
            Change::Register {
                ref search,
                ref ids,
                options,
            } => {
                let selected = retained_partition_tasks::selected_image(&id, search, ids)?;
                let (conversion, evidence) = registration::prepare(
                    &mut reader,
                    info.size,
                    info.options.sector,
                    key,
                    info.options.kind == Kind::Search,
                    options,
                    selected,
                    &AtomicBool::new(false),
                )?;
                register = Some(Register {
                    search: search.clone(),
                    ids: ids.clone(),
                    evidence,
                });
                conversion
            }
        };
        opened.validate()?;
        let locale = native_locale::current();
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text(if register.is_some() {
                "保存含找回分区的新 RAW 镜像"
            } else if conversion.preview.gpt_repair.is_some() {
                "保存修复 GPT 后的新 RAW 镜像"
            } else if restore.is_some() {
                "保存已恢复分区表的新 RAW 镜像"
            } else {
                "保存已转换分区表的新 RAW 镜像"
            }))
            .add_filter(locale.text("虚拟磁盘镜像"), &["img"])
            .set_file_name(match change {
                Change::Table(table) => format!("DiskStudio-{}.img", table.name()),
                Change::Placement(_) => "DiskStudio-MBR.img".into(),
                Change::Restore => "DiskStudio-restored-table.img".into(),
                Change::Repair { .. } => "DiskStudio-repaired-GPT.img".into(),
                Change::Register { .. } => "DiskStudio-recovered-partitions.img".into(),
            })
            .save_file()
        else {
            return Ok(None);
        };
        let name = backup_files::image_name(&path, Format::Raw)?;
        output.check_filename(name)?;
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效")?,
            &opened.source_disks,
            required,
        )?;
        let path = folder.join(name);
        match path.symlink_metadata() {
            Ok(_) => return Err("目标已存在；不会覆盖已有文件。".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(Some(Self {
            opened,
            conversion,
            path,
            destination,
            output,
            restore,
            registration: register,
        }))
    }

    pub(super) fn preview(&self) -> &Preview {
        &self.conversion.preview
    }
    pub(super) fn size(&self) -> u64 {
        self.conversion.size()
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn title(&self) -> &'static str {
        if self.registration.is_some() {
            "登记丢失分区到镜像副本"
        } else if self.preview().gpt_repair.is_some() {
            "检查并修复镜像 GPT"
        } else if self.restore.is_some() {
            "恢复镜像分区表"
        } else if self.preview().placement.is_some() {
            "转换镜像主分区或逻辑分区"
        } else {
            "转换镜像分区表"
        }
    }
    pub(super) fn source(&self, locale: &Locale) -> String {
        let info = &self.opened.browser.info;
        let name = if let Some(snapshot) = &info.parallels_snapshot {
            format!("{} · {} ({})", info.name, snapshot.name, snapshot.id)
        } else {
            info.name.clone()
        };
        let geometry = locale.format(
            "{size} B 扇区",
            &[("size", &self.preview().sector.to_string())],
        );
        if self.restore.is_some() || self.preview().gpt_repair.is_some() {
            format!("{name} · {geometry}")
        } else {
            format!("{name} · {} · {geometry}", self.preview().from.name())
        }
    }
    pub(super) fn warning(&self, locale: &Locale) -> String {
        if self.registration.is_some() {
            return locale.sentences(&[
                "只在新镜像中登记已选候选的文件系统范围，保留现有分区条目和数据。",
                "分区容量来自当前可核对的文件系统边界。完成后回读核对分区表、候选与全部逻辑内容。",
            ]);
        }
        if self.preview().gpt_repair.is_some() {
            return locale.sentences(&[
                "新 RAW 镜像使用你选择的 GPT 布局。两份表冲突时，另一份表中的分区登记可能不出现在副本中。",
                "只改写预览的分区表扇区，全部逻辑内容经 SHA-256 回读后保存到另一块物理磁盘。",
            ]);
        }
        if self.restore.is_some() {
            return locale.sentences(&[
                "恢复前核对备份表扇区之外的全部数据。数据与备份时不一致则停止。",
                "另一块物理磁盘上的新 RAW 镜像保留备份的磁盘标识和分区表。分区首扇区仅用于核对。",
            ]);
        }
        let identity = if self.preview().placement.is_some() {
            "新镜像使用新的 MBR 磁盘标识，分区编号可能变化。"
        } else {
            "新镜像使用新的磁盘标识。转换为 MBR 会丢弃 GPT 分区名称和 GUID。"
        };
        locale.sentences(&[
            "转换会保留所有分区的起点、容量与内容，在另一块物理磁盘生成新镜像。",
            identity,
            "此操作只转换数据镜像的分区表。引导转换需要单独处理。",
            "完成后重新读取并校验全部逻辑内容。",
        ])
    }

    pub(super) fn execute(
        mut self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let size = self.size();
        let tick = |phase, done| progress(id, cancel, phase, done, size);
        let locale = native_locale::current();
        let message_key = if self.registration.is_some() {
            "已在镜像副本中登记丢失分区，分区表、候选与全部逻辑内容已通过回读校验。"
        } else if self.preview().gpt_repair.is_some() {
            "GPT 镜像副本已修复，分区表和全部逻辑内容已通过回读校验。"
        } else if self.restore.is_some() {
            "镜像分区表已恢复，副本的完整 SHA-256 与备份记录一致。"
        } else {
            "镜像分区表转换完成，分区范围和全部逻辑内容已通过回读校验。"
        };
        let opened = image_browse_tasks::opened(&self.opened.browser.info.id)?;
        opened.validate()?;
        if let Some(register) = &self.registration {
            retained_partition_tasks::selected_image(
                &opened.browser.info.id,
                &register.search,
                &register.ids,
            )?;
            tick("核对候选分区边界", 0)?;
            lost_partition_restore::verify(
                &mut opened.logical_reader()?,
                &register.evidence,
                cancel,
            )?;
        }
        self.conversion
            .verify_source(&mut opened.logical_reader()?, || {
                tick("重新核对源分区表", 0)
            })?;
        if let Some(input) = &mut self.restore {
            input.verify()?;
            crate::image_table_conversion::recovery::verify_binding(
                &mut opened.logical_reader()?,
                &input.archive,
                |done| tick("核对镜像内容", done),
            )?;
            opened.validate()?;
        }
        let name = backup_files::image_name(&self.path, Format::Raw)?;
        let identity = Identity::for_output(name.to_owned())?;
        let (file, publication) = backup_files::create_image_output_excluding(
            &self.path,
            Format::Raw,
            &opened.source_disks,
            &self.destination,
            self.output.minimum_output_bytes()?,
        )?;
        let result = (|| {
            let mut encoder = Encoder::new(file, self.output, identity)?;
            for (pass, phase) in [(Pass::Copy, "写入镜像"), (Pass::Check, "重新读取源镜像")]
            {
                tick(phase, 0)?;
                opened.validate()?;
                let mut source = opened.logical_reader()?;
                self.conversion
                    .verify_source(&mut source, || tick(phase, 0))?;
                if let Some(register) = &self.registration {
                    lost_partition_restore::verify(&mut source, &register.evidence, cancel)?;
                }
                feed(
                    &mut self.conversion.reader(source).map_err(|e| e.to_string())?,
                    &mut encoder,
                    size,
                    pass,
                    |done| tick(phase, done),
                )?;
                opened.validate()?;
            }
            publication.validate()?;
            let (mut verified_file, verified) = encoder.finish(
                |file| file.sync_all().map_err(|e| e.to_string()),
                |n| tick("回读校验镜像", n),
            )?;
            self.conversion
                .verify_output(&mut verified_file, || tick("回读校验镜像分区表", size))?;
            if let Some(register) = &self.registration {
                lost_partition_restore::verify(&mut verified_file, &register.evidence, cancel)?;
            }
            if let Some(input) = &mut self.restore {
                input.verify()?;
                if verified.sha256 != input.archive.source {
                    return Err("恢复副本的完整 SHA-256 与原始镜像不一致。".into());
                }
            }
            opened.validate()?;
            let message = format!("{}\n{}", locale.text(message_key), self.path.display());
            complete_verified(id, message.clone(), verified.sha256.clone(), || {
                publication.publish()
            })?;
            Ok((message, Some(verified.sha256)))
        })();
        result.map_err(|error: String| {
            locale.format(
                "{error} 未完成文件保留为 {name}。",
                &[
                    ("error", locale.text(&error)),
                    ("name", &format!("{name}.partial")),
                ],
            )
        })
    }
}

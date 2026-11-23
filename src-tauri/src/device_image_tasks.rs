//! Explicit export of a locked physical source to a new ordinary image file.
use super::{complete_verified, progress, Selection};
use crate::{
    backup_files,
    data_windows::{self, Resolved},
    image_convert::{Encoder, Format, Identity, Options, CHUNK},
    native_locale::{self, Locale},
    partitions::Target,
    used_space::{self, Mode, View},
};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};

pub(crate) struct Export {
    source: Selection,
    before: Resolved,
    path: PathBuf,
    destination: Target,
    output: Options,
    mode: Mode,
}

impl Export {
    pub(super) fn prepare(
        source: Selection,
        format: Format,
        mode: Mode,
    ) -> Result<Option<Self>, String> {
        if source.partition.is_some() && format != Format::Raw {
            return Err("单分区导出请选择 RAW；虚拟磁盘容器需要整盘布局。".into());
        }
        let before = data_windows::resolve(&source, true, false)?;
        if before.disk.is_read_only.is_none() {
            return Err("设备状态不完整或为复杂存储，已拒绝访问。".into());
        }
        let output = Options {
            format,
            size: before.size,
            sector: before
                .disk
                .logical_sector_size
                .ok_or("源盘逻辑扇区大小未知。")?,
        };
        let required = output.minimum_output_bytes()?;
        let extension = format.extension();
        let locale = native_locale::current();
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text("导出磁盘镜像"))
            .add_filter(locale.text("虚拟磁盘镜像"), &[extension])
            .set_file_name(format!("DiskStudio-export.{extension}"))
            .save_file()
        else {
            return Ok(None);
        };
        let name = backup_files::image_name(&path, format)?;
        output.check_filename(name)?;
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效")?,
            std::slice::from_ref(&source.target),
            required,
        )?;
        let path = folder.join(name);
        match path.symlink_metadata() {
            Ok(_) => return Err("目标已存在；不会覆盖已有文件。".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        Ok(Some(Self {
            source,
            before,
            path,
            destination,
            output,
            mode,
        }))
    }

    pub(super) fn size(&self) -> u64 {
        self.output.size
    }

    pub(super) fn source_label(&self, locale: &Locale) -> String {
        let sector = locale.format(
            "{size} B 扇区",
            &[("size", &self.output.sector.to_string())],
        );
        format!("{} · {sector}", self.before.label)
    }

    pub(super) fn destination_label(&self, locale: &Locale) -> String {
        let format = match self.output.format {
            Format::Raw => "RAW 原始镜像",
            Format::VhdFixed => "VHD 固定容量",
            Format::VhdDynamic => "VHD 动态扩展",
            Format::VdiDynamic => "VDI 动态扩展",
            Format::VdiFixed => "VDI 固定容量",
            Format::VhdxDynamic => "VHDX 动态扩展",
            Format::VhdxFixed => "VHDX 固定容量",
            Format::VmdkSparse => "VMDK 单文件稀疏",
            Format::Parallels => "Parallels HDS 扩展结构",
        };
        format!("{} · {}", self.path.display(), locale.text(format))
    }

    pub(super) fn warning(&self, locale: &Locale) -> String {
        locale.sentences(&[
            "任务期间锁定源卷。",
            if self.mode == Mode::UsedSpace {
                "已确认空闲的区域按零保存，不保留其已删除内容；未知文件系统完整保留。"
            } else {
                "完整备份包含空闲空间。"
            },
            if self.source.partition.is_some() {
                "导出文件包含所选分区的内容，重新打开时请选择分区镜像和源逻辑扇区大小。"
            } else {
                "整盘副本保留分区表和磁盘标识。请避免同时挂载原盘和副本。"
            },
            "两遍读取源盘，再回读并校验全部镜像逻辑内容。",
        ])
    }

    pub(super) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let Self {
            source,
            before,
            path,
            destination,
            output,
            mode,
        } = self;
        let size = output.size;
        let locale = native_locale::current();
        let tick = |phase, done| progress(id, cancel, phase, done, size);
        tick("准备", 0)?;
        let current = data_windows::resolve(&source, true, false)?;
        if !current.same_data_binding(&before) {
            return Err("源设备、布局或卷关联与确认时不同。".into());
        }
        let required = output.minimum_output_bytes()?;
        let name = backup_files::image_name(&path, output.format)?;
        let identity = Identity::for_output(name.to_owned())?;
        tick("锁定源卷", 0)?;
        let locks = data_windows::state_locks(&current)?;
        let bound = data_windows::resolve_locked(&source, false, &current, &locks)?;
        let allocation = if mode == Mode::UsedSpace {
            tick("检查文件系统分配信息", 0)?;
            let mut input = reader(&bound)?;
            let partitions = source
                .partition
                .is_none()
                .then_some(bound.partition_ranges.as_slice());
            let allocation =
                used_space::inspect(&mut input, size, output.sector, partitions, cancel)?;
            allocation.proof.verify(&mut input, cancel)?;
            Some(allocation)
        } else {
            None
        };
        let bound = data_windows::resolve_locked(&source, false, &bound, &locks)?;
        tick("写入镜像", 0)?;
        let (file, publication) = backup_files::create_image_output(
            &path,
            output.format,
            &source.target,
            &destination,
            required,
        )?;
        let result = (|| {
            let mut encoder = Encoder::new(file, output, identity)?;
            for (pass, phase) in [(Pass::Copy, "写入镜像"), (Pass::Check, "重新读取源盘")]
            {
                tick(phase, 0)?;
                // Reopen from the same locked, revalidated selection so the second
                // pass cannot be satisfied by any application buffer from the first.
                let fresh = data_windows::resolve_locked(&source, false, &bound, &locks)?;
                let mut input = reader(&fresh)?;
                match &allocation {
                    Some(allocation) => {
                        allocation.proof.verify(&mut input, cancel)?;
                        feed(
                            &mut allocation.masked(&mut input),
                            &mut encoder,
                            size,
                            pass,
                            |n| tick(phase, n),
                        )?;
                        allocation.proof.verify(&mut input, cancel)?;
                    }
                    None => feed(&mut input, &mut encoder, size, pass, |n| tick(phase, n))?,
                }
            }
            publication.validate()?;
            let (_verified_file, verified) = encoder.finish(
                |file| file.sync_all().map_err(|e| e.to_string()),
                |n| tick("回读校验镜像", n),
            )?;
            data_windows::resolve_locked(&source, false, &bound, &locks)?;
            let mut message = locale
                .text("镜像导出完成，全部逻辑内容已通过 SHA-256 回读校验。")
                .to_owned();
            if let Some(allocation) = &allocation {
                message.push(' ');
                message.push_str(&locale.format(
                    "确认空闲并按零保存 {free} B。",
                    &[("free", &allocation.skipped.to_string())],
                ));
            }
            message.push('\n');
            message.push_str(&path.display().to_string());
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

fn reader(bound: &Resolved) -> Result<View<File>, String> {
    View::bounded(
        data_windows::device(bound, false)?,
        bound.offset,
        bound.size,
        bound.disk.size,
    )
    .map_err(|e| e.to_string())
}

#[derive(Clone, Copy)]
enum Pass {
    Copy,
    Check,
}

fn feed<W: Read + Write + Seek>(
    reader: &mut impl Read,
    encoder: &mut Encoder<W>,
    size: u64,
    pass: Pass,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<(), String> {
    let mut bytes = vec![0; CHUNK];
    let mut done = 0;
    while done < size {
        tick(done)?;
        let len = (size - done).min(CHUNK as u64) as usize;
        let bytes = &mut bytes[..len];
        reader
            .read_exact(bytes)
            .map_err(|e| format!("读取失败，未跳过坏扇区：{e}"))?;
        tick(done)?;
        match pass {
            Pass::Copy => encoder.push(done, bytes)?,
            Pass::Check => {
                encoder.verify_source(done, bytes)?;
            }
        }
        done += len as u64;
    }
    tick(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn full_device_export_feeds_independent_passes_with_the_same_aligned_tail() {
        let size = 4 * CHUNK + 4096;
        let data = vec![0xa5; size];
        let options = Options {
            format: Format::Raw,
            size: size as u64,
            sector: 4096,
        };
        let mut encoder = Encoder::new(
            Cursor::new(Vec::new()),
            options.clone(),
            Identity::for_output("disk.img".into()).unwrap(),
        )
        .unwrap();
        let mut done = Vec::new();
        feed(
            &mut Cursor::new(&data),
            &mut encoder,
            options.size,
            Pass::Copy,
            |_| Ok(()),
        )
        .unwrap();
        feed(
            &mut Cursor::new(&data),
            &mut encoder,
            options.size,
            Pass::Check,
            |n| {
                done.push(n);
                Ok(())
            },
        )
        .unwrap();
        let (output, check) = encoder.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(done.last(), Some(&options.size));
        assert_eq!(check.file_bytes, options.minimum_output_bytes().unwrap());
        assert_eq!(output.into_inner(), data);
    }

    #[test]
    fn short_or_changed_pass_cannot_finish_an_export() {
        let size = 4 * CHUNK;
        let options = Options {
            format: Format::VhdxDynamic,
            size: size as u64,
            sector: 4096,
        };
        let identity = Identity::for_output("disk.vhdx".into()).unwrap();
        let mut encoder = Encoder::new(Cursor::new(Vec::new()), options, identity).unwrap();
        let data = vec![0; size];
        feed(
            &mut Cursor::new(&data),
            &mut encoder,
            size as u64,
            Pass::Copy,
            |_| Ok(()),
        )
        .unwrap();
        assert!(feed(
            &mut Cursor::new(&data[..CHUNK - 1]),
            &mut encoder,
            size as u64,
            Pass::Check,
            |_| Ok(())
        )
        .is_err());
        assert!(encoder.finish(|_| Ok(()), |_| Ok(())).is_err());

        let options = Options {
            format: Format::VhdxDynamic,
            size: size as u64,
            sector: 4096,
        };
        let identity = Identity::for_output("disk.vhdx".into()).unwrap();
        let mut encoder = Encoder::new(Cursor::new(Vec::new()), options, identity).unwrap();
        feed(
            &mut Cursor::new(&data),
            &mut encoder,
            size as u64,
            Pass::Copy,
            |_| Ok(()),
        )
        .unwrap();
        let mut changed = data;
        changed[size - 1] = 1;
        feed(
            &mut Cursor::new(&changed),
            &mut encoder,
            size as u64,
            Pass::Check,
            |_| Ok(()),
        )
        .unwrap();
        assert!(encoder.finish(|_| Ok(()), |_| Ok(())).is_err());
    }
}

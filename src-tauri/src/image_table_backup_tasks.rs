use super::{complete_verified, image_browse_tasks, progress};
use crate::{
    backup_files::{self, Input},
    data_windows::{self, FileBinding},
    image_browser::Kind,
    image_table_conversion::recovery,
    native_locale::{self, Locale},
    partitions::Target,
    table_archive::{self, Archive},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn read_archive(file: &mut File) -> Result<Vec<u8>, String> {
    let size = file.metadata().map_err(data_windows::err)?.len();
    if size > table_archive::MAX_FILE as u64 {
        return Err("备份必须是大小受限的普通文件。".into());
    }
    let mut bytes = vec![0; size as usize];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(data_windows::err)?;
    Ok(bytes)
}

pub(super) struct RestoreInput {
    input: Input,
    digest: [u8; 32],
    pub(super) archive: Archive,
}
impl RestoreInput {
    pub(super) fn choose() -> Result<Option<Self>, String> {
        let locale = native_locale::current();
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text("选择镜像分区表备份"))
            .add_filter(locale.text("DiskStudio 分区表备份"), &["dspt"])
            .pick_file()
        else {
            return Ok(None);
        };
        backup_files::table_name(&path)?;
        let mut input = Input::open_excluding(&path, &[])?;
        let bytes = read_archive(&mut input.file)?;
        let archive = Archive::decode(&bytes)?;
        if archive.version != table_archive::IMAGE_VERSION {
            return Err("请选择从镜像保存的分区表备份。".into());
        }
        Ok(Some(Self {
            input,
            digest: Sha256::digest(&bytes).into(),
            archive,
        }))
    }

    pub(super) fn name(&self) -> &str {
        &self.input.name
    }

    pub(super) fn verify(&mut self) -> Result<(), String> {
        FileBinding::validate_all(std::slice::from_ref(&self.input.binding))?;
        let actual: [u8; 32] = Sha256::digest(read_archive(&mut self.input.file)?).into();
        if actual != self.digest {
            return Err("备份摘要与预览不同。".into());
        }
        Ok(())
    }
}

pub(crate) struct Save {
    opened: image_browse_tasks::Opened,
    path: PathBuf,
    destination: Target,
}
impl Save {
    pub(in crate::data_tasks) fn prepare(id: String) -> Result<Option<Self>, String> {
        let opened = image_browse_tasks::opened(&id)?;
        opened.validate()?;
        if opened.browser.info.options.kind != Kind::Disk || opened.source_disks.is_empty() {
            return Err("分区表备份需要已核对来源和分区表的整盘镜像。".into());
        }
        let locale = native_locale::current();
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text("备份镜像分区表"))
            .add_filter(locale.text("DiskStudio 分区表备份"), &["dspt"])
            .set_file_name("DiskStudio-image-table.dspt")
            .save_file()
        else {
            return Ok(None);
        };
        let name = backup_files::table_name(&path)?;
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效")?,
            &opened.source_disks,
            table_archive::MAX_FILE as u64,
        )?;
        let path = folder.join(name);
        match path.symlink_metadata() {
            Ok(_) => return Err("目标备份已存在，未覆盖。".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(Some(Self {
            opened,
            path,
            destination,
        }))
    }

    pub(in crate::data_tasks) fn source(&self) -> &str {
        &self.opened.browser.info.name
    }
    pub(in crate::data_tasks) fn path(&self) -> &Path {
        &self.path
    }
    pub(in crate::data_tasks) fn size(&self) -> u64 {
        self.opened.browser.info.size
    }
    pub(in crate::data_tasks) fn warning(locale: &Locale) -> String {
        locale
            .text("读取整份镜像以核对内容，只将分区表和分区首扇区保存到另一块物理磁盘。")
            .into()
    }

    pub(in crate::data_tasks) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let size = self.size();
        let opened = image_browse_tasks::opened(&self.opened.browser.info.id)?;
        opened.validate()?;
        let archive = recovery::backup(
            &mut opened.logical_reader()?,
            size,
            opened.browser.info.options.sector,
            |done| progress(id, cancel, "核对镜像内容", done, size),
        )?;
        opened.validate()?;
        let bytes = archive.encode()?;
        let digest = Sha256::digest(&bytes);
        progress(id, cancel, "保存已验证备份", size, size)?;
        let (mut file, publication) = backup_files::create_image_table_archive(
            &self.path,
            &opened.source_disks,
            &self.destination,
            bytes.len() as u64,
        )?;
        (|| {
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(data_windows::err)?;
            publication.validate()?;
            let verified = read_archive(&mut file)?;
            if Sha256::digest(&verified) != digest || Archive::decode(&verified)? != archive {
                return Err("输出文件 SHA-256 回读不一致。".into());
            }
            let message = format!(
                "{}\n{}",
                native_locale::text("镜像分区表备份已通过回读校验。"),
                self.path.display(),
            );
            let digest = format!("{digest:x}");
            complete_verified(id, message.clone(), digest.clone(), || {
                publication.publish()
            })?;
            Ok((message, Some(digest)))
        })()
        .map_err(|error: String| {
            native_locale::current().format(
                "{error} 未完成文件保留为 {name}。",
                &[
                    ("error", &error),
                    ("name", &format!("{}.partial", self.path.display())),
                ],
            )
        })
    }
}

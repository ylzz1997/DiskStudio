//! Native offline source and verified machine-package publication.
use super::{
    complete_verified,
    image_browse_tasks, progress, Selection,
};
use crate::{
    backup_archive::Source as Reader,
    boot_inspection::virtualization::{self as boot, Plan as BootPlan},
    data_windows::{self, FileBinding},
    image_browser::Kind,
    image_convert::{feed, Encoder, Identity, Options as ImageOptions, Pass},
    native_locale::{self, Locale},
    partitions::Target,
    recovery_output,
    system_virtualization::{Options, Preview},
    used_space::{Span, View},
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde::Deserialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Source {
    Image { id: String },
    Disk { selection: Selection },
}
enum Checked {
    Image(image_browse_tasks::Opened),
    Disk {
        selection: Selection,
        disk: Box<data_windows::Resolved>,
        locks: data_windows::Locks,
    },
}
impl Checked {
    fn open(source: &Source) -> Result<Self, String> {
        match source {
            Source::Image { id } => {
                let opened = image_browse_tasks::opened(id)?;
                opened.validate()?;
                if opened.browser.info.options.kind != Kind::Disk || opened.source_disks.is_empty()
                {
                    return Err("系统虚拟化需要已核对来源的整盘镜像。".into());
                }
                Ok(Self::Image(opened))
            }
            Source::Disk { selection } => {
                if selection.partition.is_some() {
                    return Err("系统虚拟化需要整块脱机磁盘。".into());
                }
                let disk = data_windows::resolve(selection, true, false)?;
                if disk.disk.is_offline != Some(true) || disk.disk.is_read_only.is_none() {
                    return Err("系统虚拟化需要状态已核对的脱机磁盘。".into());
                }
                let locks = data_windows::state_locks(&disk)?;
                let disk = data_windows::resolve_locked(selection, false, &disk, &locks)?;
                Ok(Self::Disk {
                    selection: selection.clone(),
                    disk: Box::new(disk),
                    locks,
                })
            }
        }
    }
    fn geometry(&self) -> Result<(u64, u64), String> {
        match self {
            Self::Image(opened) => {
                Ok((opened.browser.info.size, opened.browser.info.options.sector))
            }
            Self::Disk { disk, .. } => Ok((
                disk.size,
                disk.disk
                    .logical_sector_size
                    .ok_or("源盘逻辑扇区大小未知。")?,
            )),
        }
    }
    fn sources(&self) -> &[Target] {
        match self {
            Self::Image(opened) => &opened.source_disks,
            Self::Disk { selection, .. } => std::slice::from_ref(&selection.target),
        }
    }
    fn label(&self) -> &str {
        match self {
            Self::Image(opened) => &opened.browser.info.name,
            Self::Disk { disk, .. } => &disk.label,
        }
    }
    fn expected_ranges(&self) -> Option<&[Span]> {
        match self {
            Self::Disk { disk, .. } => Some(&disk.partition_ranges),
            Self::Image(_) => None,
        }
    }
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Image(opened) => opened.validate(),
            Self::Disk {
                selection,
                disk,
                locks,
            } => {
                let current = data_windows::resolve_locked(selection, false, disk, locks)?;
                if current.disk.is_offline != Some(true) || !current.same_data_binding(disk) {
                    return Err("源设备、布局或卷关联与确认时不同。".into());
                }
                Ok(())
            }
        }
    }
    fn reader(&self) -> Result<Box<dyn Reader>, String> {
        self.validate()?;
        match self {
            Self::Image(opened) => opened.logical_reader(),
            Self::Disk { disk, .. } => Ok(Box::new(
                View::bounded(
                    data_windows::device(disk, false)?,
                    0,
                    disk.size,
                    disk.disk.size,
                )
                .map_err(data_windows::err)?,
            )),
        }
    }
    fn boot(&self, size: u64, sector: u64) -> Result<BootPlan, String> {
        let plan = boot::prepare(&mut self.reader()?, size, sector, self.expected_ranges())?;
        self.validate()?;
        Ok(plan)
    }
}

struct Package {
    folder: Dir,
    bindings: Vec<FileBinding>,
}
impl Package {
    fn create(
        path: &Path,
        sources: &[Target],
        expected: &Target,
        required: u64,
    ) -> Result<Self, String> {
        let leaf = path.file_name().ok_or("输出目录无效。")?;
        let (parent, disk) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效。")?,
            sources,
            required,
        )?;
        if &disk != expected {
            return Err("输出磁盘与确认时不同。".into());
        }
        let parent = Dir::open_ambient_dir(parent, cap_std::ambient_authority())
            .map_err(data_windows::err)?;
        let parent_binding = FileBinding::new(
            &parent
                .try_clone()
                .map_err(data_windows::err)?
                .into_std_file(),
            &disk,
        )?;
        FileBinding::validate_all(std::slice::from_ref(&parent_binding))?;
        parent.create_dir(leaf).map_err(data_windows::err)?;
        let folder = parent.open_dir_nofollow(leaf).map_err(data_windows::err)?;
        let folder_binding = FileBinding::new(
            &folder
                .try_clone()
                .map_err(data_windows::err)?
                .into_std_file(),
            &disk,
        )?;
        let result = Self {
            folder,
            bindings: vec![parent_binding, folder_binding],
        };
        result.verify()?;
        Ok(result)
    }
    fn file(&mut self, name: &str, destination: &Target) -> Result<File, String> {
        self.verify()?;
        let file = recovery_output::create(&self.folder, &format!("{name}.partial"))?;
        self.bindings.push(FileBinding::new(&file, destination)?);
        self.verify()?;
        Ok(file)
    }
    fn verify(&self) -> Result<(), String> {
        FileBinding::validate_all(&self.bindings)
    }
    fn publish(
        self,
        disk: &File,
        disk_name: &str,
        config: &File,
        config_name: &str,
    ) -> Result<(), String> {
        self.verify()?;
        recovery_output::finish(disk, &self.folder, disk_name)?;
        // A machine must become visible only after its verified disk exists.
        recovery_output::finish(config, &self.folder, config_name)
    }
}

pub(crate) struct Export {
    source: Source,
    sources: Vec<Target>,
    label: String,
    out: PathBuf,
    destination: Target,
    layout: ImageOptions,
    options: Options,
    boot: BootPlan,
    machine: Uuid,
}
impl Export {
    pub(super) fn prepare(source: Source, options: Options) -> Result<Option<Self>, String> {
        options.validate()?;
        let checked = Checked::open(&source)?;
        let (size, sector) = checked.geometry()?;
        let layout = ImageOptions {
            format: options.platform.format(),
            size,
            sector,
        };
        let required = layout.minimum_output_bytes()?;
        let boot = checked.boot(size, sector)?;
        let label = checked.label().into();
        let sources = checked.sources().to_vec();
        drop(checked);
        let Some(parent) = rfd::FileDialog::new()
            .set_title(native_locale::text("选择虚拟机保存目录"))
            .pick_folder()
        else {
            return Ok(None);
        };
        let (parent, destination) =
            data_windows::destination_location(&parent, &sources, required)?;
        let machine = Uuid::new_v4();
        let suffix = machine.simple().to_string();
        let out = parent.join(format!("{}-{}", options.name, &suffix[..12]));
        match out.symlink_metadata() {
            Ok(_) => return Err("目标虚拟机目录已存在。".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(Some(Self {
            source,
            sources,
            label,
            out,
            destination,
            layout,
            options,
            boot,
            machine,
        }))
    }
    pub(super) fn source(&self) -> &str {
        &self.label
    }
    pub(super) fn destination(&self) -> String {
        self.out
            .join(self.options.config_name())
            .display()
            .to_string()
    }
    pub(super) fn size(&self) -> u64 {
        self.layout.size
    }
    pub(super) fn preview(&self) -> Preview {
        Preview {
            platform: self.options.platform,
            name: self.options.name.clone(),
            cpus: self.options.cpus,
            memory_mib: self.options.memory_mib,
            firmware: self.boot.firmware,
            controller: self.boot.controller,
            driver: self.boot.driver,
            adds_efi_fallback: self.boot.adds_efi_fallback(),
            config: self.options.config_name(),
            disk: self.options.platform.disk_name(),
        }
    }
    pub(super) fn warning(&self, locale: &Locale) -> String {
        locale.sentences(&[
            "在另一块物理磁盘生成虚拟磁盘及配置。整盘内容两遍读取并通过 SHA-256 回读校验。",
            if self.boot.adds_efi_fallback() {
                "在副本的 EFI 系统分区补齐 Windows x64 回退入口。"
            } else {
                ""
            },
            "虚拟机默认无网络和共享目录。首次启动和 Windows 激活由虚拟化软件处理。",
        ])
    }
    pub(super) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let size = self.size();
        let tick = |phase, done| progress(id, cancel, phase, done, size);
        let locale = native_locale::current();
        tick("核对 Windows 虚拟化来源", 0)?;
        let source = Checked::open(&self.source)?;
        let (actual_size, sector) = source.geometry()?;
        if (actual_size, sector) != (size, self.layout.sector)
            || source.sources() != self.sources
            || source.boot(size, sector)? != self.boot
        {
            return Err("启动链、磁盘布局或存储驱动在预览后发生变化。".into());
        }
        let required = self.layout.minimum_output_bytes()?;
        let disk_name = self.options.platform.disk_name();
        let config_name = self.options.config_name();
        let mut identity = Identity::for_output(disk_name.into())?;
        if self.options.platform == crate::system_virtualization::Platform::VirtualBox {
            identity.created = Uuid::new_v4().to_bytes_le();
            identity.modified = Uuid::new_v4().to_bytes_le();
        }
        let configuration = self.options.config(
            self.boot.firmware,
            self.boot.controller,
            self.machine,
            &identity,
        )?;
        tick("写入虚拟磁盘", 0)?;
        let mut package = Package::create(&self.out, &self.sources, &self.destination, required)?;
        let result = (|| {
            let disk = package.file(disk_name, &self.destination)?;
            let mut encoder = Encoder::new(disk, self.layout, identity)?;
            for (pass, phase) in [
                (Pass::Copy, "写入虚拟磁盘"),
                (Pass::Check, "重新读取源镜像"),
            ] {
                tick(phase, 0)?;
                let mut view = self
                    .boot
                    .reader(source.reader()?)
                    .map_err(data_windows::err)?;
                feed(&mut view, &mut encoder, size, pass, |n| tick(phase, n))?;
            }
            source.validate()?;
            package.verify()?;
            let (disk, verified) = encoder.finish(
                |file| file.sync_all().map_err(data_windows::err),
                |n| tick("回读校验镜像", n),
            )?;
            tick("核对虚拟机配置", size)?;
            let mut config = package.file(&config_name, &self.destination)?;
            config
                .write_all(&configuration)
                .and_then(|_| config.sync_all())
                .map_err(data_windows::err)?;
            let mut read_back = Vec::new();
            config
                .seek(SeekFrom::Start(0))
                .and_then(|_| config.read_to_end(&mut read_back))
                .map_err(data_windows::err)?;
            if configuration != read_back {
                return Err("虚拟机配置未通过回读校验。".into());
            }
            if source.boot(size, sector)? != self.boot {
                return Err("启动链、磁盘布局或存储驱动在预览后发生变化。".into());
            }
            let message = format!(
                "{}\n{}",
                locale.text("已生成虚拟机配置和经过回读校验的系统虚拟磁盘。"),
                self.out.join(&config_name).display(),
            );
            complete_verified(id, message.clone(), verified.sha256.clone(), || {
                package.publish(&disk, disk_name, &config, &config_name)
            })?;
            Ok((message, Some(verified.sha256)))
        })();
        result.map_err(|error: String| {
            locale.format(
                "{error} 未完成的输出位于 {name}。",
                &[
                    ("error", locale.text(&error)),
                    ("name", &self.out.display().to_string()),
                ],
            )
        })
    }
}

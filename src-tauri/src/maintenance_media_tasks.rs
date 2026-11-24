//! Assemble and publish a new, verified WinPE ISO without media or device access.
use super::{complete_verified, progress};
use crate::{
    boot_configuration, data_io,
    data_windows::{self, FileBinding},
    maintenance_iso::{self, Image, Member},
    maintenance_media::{self as media, Mode, Preview},
    native_locale::{self, Locale},
    partitions::Target,
    recovery_output,
};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, MetadataExt as CapMetadataExt, OpenOptions, OpenOptionsExt};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

#[path = "maintenance_usb_tasks.rs"]
pub(super) mod usb;
#[path = "maintenance_winpe_adk.rs"]
mod adk;

const MAX_EFI: usize = 16 * 1024 * 1024;

struct Input {
    file: File,
    member: Member,
}
struct Media {
    image: Image,
    files: Vec<Input>,
    preview: Preview,
}

fn plain(dir: &Dir, name: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = dir
        .open_with(name, &options)
        .map_err(data_windows::err)?
        .into_std();
    let info = file.metadata().map_err(data_windows::err)?;
    if !info.is_file() || info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("WinPE 来源只能包含普通文件和目录。".into());
    }
    Ok(file)
}

fn optional_subdir(dir: &Dir, name: &str) -> Result<Option<Dir>, String> {
    match dir.symlink_metadata(name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(m) if m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 => dir
            .open_dir_nofollow(name)
            .map(Some)
            .map_err(data_windows::err),
        Ok(_) => Err("WinPE 来源只能包含普通文件和目录。".into()),
    }
}

struct Walk<'a, V> {
    files: BTreeMap<String, Input>,
    verify: &'a mut V,
    items: usize,
}
impl<V: FnMut(&File) -> Result<(), String>> Walk<'_, V> {
    fn folder(&mut self, dir: &Dir, prefix: &str, depth: usize) -> Result<(), String> {
        if depth > 7 {
            return Err("WinPE 目录嵌套超过 ISO 范围。".into());
        }
        (self.verify)(&dir.try_clone().map_err(data_windows::err)?.into_std_file())?;
        let entries = dir.entries().map_err(data_windows::err)?;
        for entry in entries {
            self.items += 1;
            if self.items > maintenance_iso::MAX_FILES {
                return Err("WinPE 来源的文件和目录过多。".into());
            }
            let entry = entry.map_err(data_windows::err)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "WinPE 文件名称无效。")?;
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if !maintenance_iso::component(&name) {
                return Err("WinPE 来源含有 ISO 不支持的文件名。".into());
            }
            let kind = entry.file_type().map_err(data_windows::err)?;
            if kind.is_dir() {
                let sub = dir.open_dir_nofollow(&name).map_err(data_windows::err)?;
                self.folder(&sub, &path, depth + 1)?;
            } else if kind.is_file() {
                let file = plain(dir, Path::new(&name))?;
                (self.verify)(&file)?;
                let size = u32::try_from(file.metadata().map_err(data_windows::err)?.len())
                    .map_err(|_| "WinPE ISO 的单个文件必须小于 4 GiB。")?;
                if self
                    .files
                    .insert(
                        path.to_lowercase(),
                        Input {
                            file,
                            member: Member { path, size },
                        },
                    )
                    .is_some()
                {
                    return Err("WinPE 文件路径在 ISO 中不唯一。".into());
                }
            } else {
                return Err("WinPE 来源只能包含普通文件和目录。".into());
            }
        }
        Ok(())
    }
}

fn read_bounded(file: &mut File, limit: usize) -> Result<Vec<u8>, String> {
    let size = file.metadata().map_err(data_windows::err)?.len();
    if size == 0 || size > limit as u64 {
        return Err("WinPE 启动文件超出检查范围。".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(data_windows::err)?;
    let mut bytes = vec![0; size as usize];
    file.read_exact(&mut bytes).map_err(data_windows::err)?;
    Ok(bytes)
}

fn file(files: &mut BTreeMap<String, Input>, path: &str) -> Result<Vec<u8>, String> {
    let item = files
        .get_mut(path)
        .ok_or_else(|| format!("缺少 WinPE 启动文件：{path}"))?;
    read_bounded(&mut item.file, MAX_EFI)
}

fn bootstrap(
    files: &mut BTreeMap<String, Input>,
    firmware: &Option<Dir>,
    name: &str,
    fallback: &str,
    verify: &mut impl FnMut(&File) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    if let Some(dir) = firmware {
        match dir.symlink_metadata(name) {
            Ok(_) => {
                let mut input = plain(dir, Path::new(name))?;
                verify(&input)?;
                return read_bounded(&mut input, MAX_EFI);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    file(files, fallback)
}

fn scan_media(
    root: &Dir,
    verify: &mut impl FnMut(&File) -> Result<(), String>,
) -> Result<(BTreeMap<String, Input>, Preview), String> {
    let media = optional_subdir(root, "media")?;
    let mut walk = Walk {
        files: BTreeMap::new(),
        verify,
        items: 0,
    };
    walk.folder(media.as_ref().unwrap_or(root), "", 0)?;
    let mut files = walk.files;
    let preview = inspect_media(&mut files)?;
    Ok((files, preview))
}

fn inspect_media(files: &mut BTreeMap<String, Input>) -> Result<Preview, String> {
    let wim = files
        .get_mut(media::WIM_PATH)
        .ok_or("找不到 sources\\boot.wim。")?;
    let wim_bytes = u64::from(wim.member.size);
    let mut header = [0; 208];
    wim.file
        .read_exact(&mut header)
        .map_err(data_windows::err)?;
    let wim_image = media::wim_index(&header, wim_bytes)?;
    if !file(files, media::SDI_PATH)?.starts_with(b"$SDI0001") {
        return Err("WinPE 的 boot.sdi 文件头无效。".into());
    }
    media::check_fallback(&file(files, media::FALLBACK)?)?;
    boot_configuration::verify_winpe(file(files, media::EFI_BCD)?, false)?;
    Ok(Preview {
        bios: false,
        wim_image,
        wim_bytes,
        file_count: files.len(),
        app_sha256: None,
        usb: None,
        adk: false,
    })
}

fn open_source(path: &Path) -> Result<(Dir, FileBinding), String> {
    let root =
        Dir::open_ambient_dir(path, cap_std::ambient_authority()).map_err(data_windows::err)?;
    let root_file = root.try_clone().map_err(data_windows::err)?.into_std_file();
    let location = data_windows::file_location(&root_file)?;
    let binding = FileBinding::new(&root_file, &location.target)?;
    Ok((root, binding))
}

fn running_app(include: bool) -> Result<Option<(File, FileBinding)>, String> {
    if !include {
        return Ok(None);
    }
    let path = std::env::current_exe().map_err(data_windows::err)?;
    let (parent, _) = open_source(path.parent().ok_or("无法定位 DiskStudio 程序。")?)?;
    let file = plain(
        &parent,
        Path::new(path.file_name().ok_or("无法定位 DiskStudio 程序。")?),
    )?;
    let location = data_windows::file_location(&file)?;
    let binding = FileBinding::new(&file, &location.target)?;
    Ok(Some((file, binding)))
}

fn attach_app(
    files: &mut BTreeMap<String, Input>,
    preview: &mut Preview,
    mut file: File,
) -> Result<(), String> {
    let size = u32::try_from(file.metadata().map_err(data_windows::err)?.len())
        .map_err(|_| "DiskStudio 程序超出启动介质的文件大小限制。")?;
    let mut header = vec![0; (size as usize).min(128 * 1024)];
    file.read_exact(&mut header).map_err(data_windows::err)?;
    if crate::boot_inspection::pe_machine(&header, &[2, 3]) != Ok(0x8664) {
        return Err("启动介质需要 x64 版 DiskStudio 程序。".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(data_windows::err)?;
    let sha = data_io::hash_exact(&mut file, u64::from(size), |_| Ok(()))?;
    if let Some(existing) = files.get_mut(media::APP_KEY) {
        existing
            .file
            .seek(SeekFrom::Start(0))
            .map_err(data_windows::err)?;
        if existing.member.size != size
            || data_io::hash_exact(&mut existing.file, u64::from(size), |_| Ok(()))? != sha
        {
            return Err("WinPE 来源已有内容不同的 DiskStudio.exe。".into());
        }
    }
    if files.keys().any(|key| key.starts_with("diskstudio.exe/")) {
        return Err("WinPE 来源已有名为 DiskStudio.exe 的目录。".into());
    }
    files.insert(
        media::APP_KEY.into(),
        Input {
            file,
            member: Member {
                path: media::APP_PATH.into(),
                size,
            },
        },
    );
    if files.len() > maintenance_iso::MAX_FILES {
        return Err("WinPE 来源的文件和目录过多。".into());
    }
    preview.file_count = files.len();
    preview.app_sha256 = Some(sha);
    Ok(())
}

impl Media {
    fn open(
        root: &Dir,
        mode: Mode,
        mut verify: impl FnMut(&File) -> Result<(), String>,
        app: Option<File>,
    ) -> Result<Self, String> {
        let firmware = optional_subdir(root, "fwfiles")?;
        if let Some(folder) = &firmware {
            verify(
                &folder
                    .try_clone()
                    .map_err(data_windows::err)?
                    .into_std_file(),
            )?;
        }
        let (mut files, mut preview) = scan_media(root, &mut verify)?;
        if let Some(app) = app {
            attach_app(&mut files, &mut preview, app)?;
        }
        let uefi = bootstrap(
            &mut files,
            &firmware,
            "efisys.bin",
            "efi/microsoft/boot/efisys.bin",
            &mut verify,
        )?;
        media::check_efi(&uefi)?;
        let bios = if mode.bios() {
            let bios = bootstrap(
                &mut files,
                &firmware,
                "etfsboot.com",
                "boot/etfsboot.com",
                &mut verify,
            )?;
            media::check_bios(&bios)?;
            file(&mut files, "bootmgr")?;
            boot_configuration::verify_winpe(file(&mut files, media::BIOS_BCD)?, true)?;
            Some(bios)
        } else {
            None
        };
        let files: Vec<_> = files.into_values().collect();
        let members: Vec<_> = files.iter().map(|f| f.member.clone()).collect();
        let image = Image::prepare(&members, &uefi, bios.as_deref())?;
        preview.bios = mode.bios();
        Ok(Self {
            image,
            preview,
            files,
        })
    }

    fn write(
        &mut self,
        output: &mut File,
        mut tick: impl FnMut(&'static str, u64, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let mut stream = HashOutput {
            writer: &mut *output,
            hash: Sha256::new(),
        };
        let mut buffer = vec![0; 1024 * 1024];
        let file_extents = self.image.file_extents();
        let size = self.image.size;
        self.image.write(&mut stream, |index, bytes, writer| {
            let member = &mut self.files[index];
            let mut app_hash = (member.member.path == media::APP_PATH
                && self.preview.app_sha256.is_some())
            .then(Sha256::new);
            let input = &mut member.file;
            if input.metadata().map_err(data_windows::err)?.len() != u64::from(bytes) {
                return Err("WinPE 来源文件的容量与预览不同。".into());
            }
            input.seek(SeekFrom::Start(0)).map_err(data_windows::err)?;
            let (offset, _) = file_extents[index];
            let mut copied = 0;
            while copied < u64::from(bytes) {
                tick("写入 WinPE 启动 ISO", offset + copied, size)?;
                let count = (u64::from(bytes) - copied).min(buffer.len() as u64) as usize;
                input
                    .read_exact(&mut buffer[..count])
                    .map_err(data_windows::err)?;
                if let Some(hash) = &mut app_hash {
                    hash.update(&buffer[..count]);
                }
                writer
                    .write_all(&buffer[..count])
                    .map_err(data_windows::err)?;
                copied += count as u64;
            }
            if let Some(hash) = app_hash {
                let actual = format!("{:x}", hash.finalize());
                if self.preview.app_sha256.as_deref() != Some(actual.as_str()) {
                    return Err("DiskStudio 程序与预览中的 SHA-256 不一致。".into());
                }
            }
            Ok(())
        })?;
        let written = format!("{:x}", stream.hash.finalize());
        output.sync_all().map_err(data_windows::err)?;
        self.image.verify_prefix(output)?;
        output.seek(SeekFrom::Start(0)).map_err(data_windows::err)?;
        let readback = data_io::hash_exact(output, size, |done| {
            tick("回读校验 WinPE 启动 ISO", done, size)
        })?;
        if readback != written {
            return Err("WinPE ISO 的 SHA-256 写后回读不一致。".into());
        }
        Ok(readback)
    }
}

struct HashOutput<W> {
    writer: W,
    hash: Sha256,
}
impl<W: Write> Write for HashOutput<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let len = self.writer.write(data)?;
        self.hash.update(&data[..len]);
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

pub(super) const TITLE: &str = "创建 WinPE 启动 ISO";
pub(crate) struct Export {
    sources: Vec<FileBinding>,
    source_label: String,
    destination: Target,
    path: PathBuf,
    media: Media,
}

impl Export {
    pub(super) fn prepare(
        mode: Mode,
        include_app: bool,
        locale: &Locale,
    ) -> Result<Option<Self>, String> {
        let Some(source) = rfd::FileDialog::new()
            .set_title(locale.text("选择 WinPE 工作文件夹"))
            .pick_folder()
        else {
            return Ok(None);
        };
        let (root, binding) = open_source(&source)?;
        let (app, app_binding) = running_app(include_app)?.unzip();
        let media = Media::open(&root, mode, |file| binding.verify_same_volume(file), app)?;
        let sources: Vec<_> = std::iter::once(binding).chain(app_binding).collect();
        let source_disks: Vec<_> = sources
            .iter()
            .map(|source| source.target().clone())
            .collect();
        let Some(path) = rfd::FileDialog::new()
            .set_title(locale.text(TITLE))
            .add_filter("ISO", &["iso"])
            .set_file_name("WinPE.iso")
            .save_file()
        else {
            return Ok(None);
        };
        let leaf = path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or("ISO 输出名称无效。")?;
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("iso"))
            || !maintenance_iso::component(leaf)
            || crate::recovery_export::reserved(leaf)
        {
            return Err("ISO 输出名称无效。".into());
        }
        let (folder, destination) = data_windows::destination_location(
            path.parent().ok_or("输出目录无效。")?,
            &source_disks,
            media.image.size,
        )?;
        let path = folder.join(leaf);
        match path.symlink_metadata() {
            Ok(_) => return Err("目标 ISO 已存在，未覆盖。".into()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.to_string()),
        }
        Ok(Some(Self {
            sources,
            source_label: source.display().to_string(),
            destination,
            path,
            media,
        }))
    }
    pub(super) fn source(&self) -> &str {
        &self.source_label
    }
    pub(super) fn destination(&self) -> String {
        self.path.display().to_string()
    }
    pub(super) fn size(&self) -> u64 {
        self.media.image.size
    }
    pub(super) fn preview(&self) -> &Preview {
        &self.media.preview
    }
    pub(super) fn execute(
        mut self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let locale = native_locale::current();
        FileBinding::validate_all(&self.sources)?;
        let source_disks: Vec<_> = self
            .sources
            .iter()
            .map(|source| source.target().clone())
            .collect();
        progress(id, cancel, "写入 WinPE 启动 ISO", 0, self.size())?;
        let (parent, target) = data_windows::destination_location(
            self.path.parent().ok_or("输出目录无效。")?,
            &source_disks,
            self.size(),
        )?;
        if target != self.destination {
            return Err("输出磁盘与确认时不同。".into());
        }
        let root = Dir::open_ambient_dir(parent, cap_std::ambient_authority())
            .map_err(data_windows::err)?;
        let root_file = root.try_clone().map_err(data_windows::err)?.into_std_file();
        let root_binding = FileBinding::new(&root_file, &target)?;
        let name = self
            .path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or("ISO 输出名称无效。")?;
        let partial = format!("{name}.partial");
        if root.symlink_metadata(name).is_ok() {
            return Err("目标 ISO 已存在，未覆盖。".into());
        }
        let mut output = recovery_output::create(&root, &partial)?;
        let output_binding = FileBinding::new(&output, &target)?;
        let mut bindings = std::mem::take(&mut self.sources);
        bindings.extend([root_binding, output_binding]);
        FileBinding::validate_all(&bindings)?;
        let digest = self
            .media
            .write(&mut output, |phase, done, total| {
                progress(id, cancel, phase, done, total)
            })
            .map_err(|error| {
                locale.format(
                    "{error} 未完成的输出位于 {name}。",
                    &[("error", &locale.text(&error)), ("name", &partial)],
                )
            })?;
        FileBinding::validate_all(&bindings)?;
        let message = format!(
            "{}\n{}",
            locale.text("WinPE 启动 ISO 已通过 SHA-256 回读校验。"),
            self.path.display()
        );
        complete_verified(id, message.clone(), digest.clone(), || {
            recovery_output::finish(&output, &root, name)
        })?;
        Ok((message, Some(digest)))
    }
}

#[cfg(test)]
#[test]
#[ignore = "requires an externally prepared ordinary Windows PE media folder"]
fn assembles_real_winpe_files_into_a_verified_boot_iso() {
    let source = std::env::var_os("DISKSTUDIO_WINPE_MEDIA_FIXTURE").unwrap();
    let target = std::env::var_os("DISKSTUDIO_WINPE_ISO_FIXTURE").unwrap();
    let root = Dir::open_ambient_dir(source, cap_std::ambient_authority()).unwrap();
    let app =
        std::env::var_os("DISKSTUDIO_WINPE_APP_FIXTURE").map(|path| File::open(path).unwrap());
    let mut media = Media::open(&root, Mode::UefiAndBios, |_| Ok(()), app).unwrap();
    let mut output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(target)
        .unwrap();
    let hash = media.write(&mut output, |_, _, _| Ok(())).unwrap();
    println!(
        "WINPE ISO SHA-256 {hash} bytes {} source files {}",
        media.image.size, media.preview.file_count
    );
}

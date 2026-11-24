//! Bounded image and verified device sources in the native workspace.
use super::physical::Physical;
use crate::{
    boot_inspection,
    data_windows::{self, FileBinding},
    image_browser::{self, Browser, Kind, Options},
    native_locale::Locale,
    partitions::Target,
    recovery_export,
};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use disk_core::table_check::CheckState;
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::windows::{ffi::OsStrExt, fs::MetadataExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        GetDriveTypeW, GetFileType, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_TYPE_DISK,
    },
    System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE},
};

const INVALID_SOURCE: &str = "请选择本地的普通 RAW 或 IMG 镜像文件。";

fn source_path(path: &Path) -> Result<(PathBuf, Vec<&std::ffi::OsStr>), String> {
    let invalid = INVALID_SOURCE;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(invalid.into());
    };
    if !matches!(prefix.kind(), Prefix::Disk(_)) || components.next() != Some(Component::RootDir) {
        return Err(invalid.into());
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(r"\");
    let parts = components
        .map(|part| {
            let Component::Normal(part) = part else {
                return Err(invalid.to_owned());
            };
            let name = part.to_str().ok_or(invalid)?;
            if name.contains(':')
                || name.chars().any(char::is_control)
                || name.ends_with([' ', '.'])
                || recovery_export::reserved(name)
            {
                return Err(invalid.to_owned());
            }
            Ok(part)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if parts.is_empty()
        || !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("raw") || ext.eq_ignore_ascii_case("img"))
    {
        return Err(invalid.into());
    }
    Ok((root, parts))
}

pub(super) struct Source {
    pub browser: Browser,
    backing: Backing,
}

enum Backing {
    File(File),
    Disk(Arc<Physical>),
}

pub(super) enum Binding {
    File { file: FileBinding, number: u32 },
    Disk(Arc<Physical>),
}

impl Binding {
    pub(super) fn target(&self) -> &Target {
        match self {
            Self::File { file, .. } => file.target(),
            Self::Disk(disk) => disk.target(),
        }
    }

    pub(super) fn number(&self) -> u32 {
        match self {
            Self::File { number, .. } => *number,
            Self::Disk(disk) => disk.number(),
        }
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        match self {
            Self::File { file, .. } => FileBinding::validate_all(std::slice::from_ref(file)),
            Self::Disk(disk) => disk.verify(),
        }
    }

    pub(super) fn verify_with(&self, output: &FileBinding) -> Result<(), String> {
        match self {
            Self::File { file, .. } => FileBinding::validate_all(&[file.clone(), output.clone()]),
            Self::Disk(disk) => {
                disk.verify()?;
                FileBinding::validate_all(std::slice::from_ref(output))
            }
        }
    }
}

pub(super) struct Opened {
    pub source: Source,
    pub report: String,
}

pub(super) fn active(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("读取已取消。".into());
    }
    Ok(())
}

pub(super) fn open(
    path: &Path,
    options: Options,
    locale: &Locale,
    cancel: &AtomicBool,
) -> Result<Opened, String> {
    let (root, parts) = source_path(path)?;
    let root_name: Vec<_> = root.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: the native drive root is a null-terminated string.
    if !matches!(
        unsafe { GetDriveTypeW(root_name.as_ptr()) },
        DRIVE_CDROM | DRIVE_FIXED | DRIVE_RAMDISK | DRIVE_REMOVABLE
    ) {
        return Err(INVALID_SOURCE.into());
    }
    let mut parent =
        Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(|e| e.to_string())?;
    let (name, directories) = parts.split_last().expect("checked image file name");
    for directory in directories {
        parent = parent
            .open_dir_nofollow(directory)
            .map_err(|e| e.to_string())?;
    }
    let mut flags = OpenOptions::new();
    flags
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ);
    let file = parent
        .open_with(name, &flags)
        .map_err(|e| e.to_string())?
        .into_std();
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    // SAFETY: the open ordinary-file handle remains valid throughout the call.
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
    {
        return Err(INVALID_SOURCE.into());
    }
    options.validate(metadata.len())?;
    if options.kind == Kind::Disk && metadata.len() < 4 * disk_core::MIB {
        return Err("整盘镜像至少需要 4 MiB。".into());
    }
    let name = name.to_string_lossy().into_owned();
    let read = file.try_clone().map_err(|e| e.to_string())?;
    let browser = match Browser::open(read, name.clone(), options, || active(cancel)) {
        Ok(browser) => browser,
        Err(error) if options.kind == Kind::Disk => {
            active(cancel)?;
            let read = file.try_clone().map_err(|e| e.to_string())?;
            let browser = Browser::open(
                read,
                name,
                Options {
                    kind: Kind::Search,
                    ..options
                },
                || active(cancel),
            )?;
            return Ok(Opened {
                source: Source {
                    browser,
                    backing: Backing::File(file),
                },
                report: format!(
                    "{}\n\n{}",
                    locale.text("无法核对镜像分区表。可以继续检查分区表、查看扇区或搜索丢失分区。"),
                    locale.text(&error),
                ),
            });
        }
        Err(error) => return Err(error),
    };
    let report = boot_report(&browser, locale, cancel)?;
    Ok(Opened {
        source: Source {
            browser,
            backing: Backing::File(file),
        },
        report,
    })
}

pub(super) fn boot_report(
    browser: &Browser,
    locale: &Locale,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let report = boot_inspection::inspect(
        browser.logical_reader()?,
        browser.info.name.clone(),
        browser.info.size,
        browser.info.options.sector,
        browser.info.options.kind == Kind::Partition,
        None,
    )
    .map(|report| report.winpe_text(locale))
    .unwrap_or_else(|error| locale.text(&error).to_owned());
    active(cancel)?;
    Ok(report)
}

fn check_status(state: CheckState, locale: &Locale) -> &str {
    locale.text(match state {
        CheckState::Passed => "通过",
        CheckState::Warning => "警告",
        CheckState::Failed => "失败",
        CheckState::Unchecked => "未检查",
    })
}

impl Source {
    pub(super) fn physical(browser: Browser, disk: Arc<Physical>) -> Self {
        Self {
            browser,
            backing: Backing::Disk(disk),
        }
    }

    pub(super) fn disk(&self) -> Option<&Physical> {
        match &self.backing {
            Backing::File(_) => None,
            Backing::Disk(disk) => Some(disk),
        }
    }

    pub(super) fn bind(&self) -> Result<Binding, String> {
        match &self.backing {
            Backing::File(file) => {
                let location = data_windows::file_location(file)?;
                Ok(Binding::File {
                    file: FileBinding::exact(file, &location)?,
                    number: location.disk_number(),
                })
            }
            Backing::Disk(disk) => {
                disk.verify()?;
                Ok(Binding::Disk(disk.clone()))
            }
        }
    }

    pub(super) fn verify_disk(&self) -> Result<(), String> {
        self.disk().map_or(Ok(()), Physical::verify)
    }

    pub(super) fn table(&self, locale: &Locale, cancel: &AtomicBool) -> Result<String, String> {
        let info = &self.browser.info;
        if info.options.kind == Kind::Partition {
            return Err("分区镜像不包含整盘分区表。".into());
        }
        let (inspection, _) = image_browser::inspect_layout(
            &mut self.browser.logical_reader()?,
            if self.disk().is_some() {
                super::physical::LOGICAL_NAME
            } else {
                &info.name
            },
            info.size,
            info.options.sector,
            &mut || active(cancel),
        )?;
        self.verify_disk()?;
        let report = inspection.report;
        let mut text = format!(
            "{} · {} · {} B\n{} · {}\n",
            info.name,
            report.scheme,
            info.options.sector,
            locale.text("分区表检查"),
            check_status(report.state, locale),
        );
        for item in report.items {
            let _ = writeln!(
                text,
                "\n{} · {}\n  {}",
                check_status(item.state, locale),
                locale.text(&item.title),
                locale.text(&item.detail),
            );
        }
        if let Some(error) = inspection.parse_error {
            let _ = writeln!(text, "\n{}", locale.text(&error));
        }
        Ok(text)
    }

    pub(super) fn sector(&self, lba: u64, cancel: &AtomicBool) -> Result<String, String> {
        let info = &self.browser.info;
        let size = info.options.sector;
        if lba >= info.size / size {
            return Err("LBA 超出镜像范围。".into());
        }
        active(cancel)?;
        let offset = lba * size;
        let mut reader = self.browser.logical_reader()?;
        let mut sector = vec![0; size as usize];
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        reader.read_exact(&mut sector).map_err(|e| e.to_string())?;
        self.verify_disk()?;
        active(cancel)?;
        let mut text = format!(
            "{}\nLBA {lba} / {} · 0x{offset:X} · {size} B\nSHA-256 {:x}\n\n",
            info.name,
            info.size / size - 1,
            Sha256::digest(&sector),
        );
        for (index, bytes) in sector.chunks_exact(16).enumerate() {
            let _ = write!(text, "{:04X}  ", index * 16);
            for byte in bytes {
                let _ = write!(text, "{byte:02X} ");
            }
            text.push_str("  |");
            text.extend(bytes.iter().map(|&byte| {
                if (b' '..=b'~').contains(&byte) {
                    char::from(byte)
                } else {
                    '.'
                }
            }));
            text.push_str("|\n");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_locale::Language;
    use std::io::Write as _;

    #[test]
    fn native_path_rejects_devices_streams_and_nonlocal_sources() {
        let (root, parts) = source_path(Path::new(r"D:\fixtures\Boot.IMG")).unwrap();
        assert_eq!(root, Path::new(r"D:\"));
        assert_eq!(parts.len(), 2);
        for bad in [
            r"D:boot.img",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\device\harddisk0\boot.img",
            r"\\server\share\boot.img",
            r"D:\fixtures\..\boot.img",
            r"D:\fixtures\boot.img:data.img",
            r"D:\fixtures\NUL.img",
            r"D:\fixtures.\boot.img",
            r"D:\boot.iso",
        ] {
            assert!(source_path(Path::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn damaged_4kn_image_retains_sector_access_without_trusting_partitions() {
        let path =
            std::env::temp_dir().join(format!("diskstudio-winpe-{}.img", uuid::Uuid::new_v4()));
        let mut bytes = vec![0; 4 * disk_core::MIB as usize];
        bytes[4096..4104].copy_from_slice(b"EFI PART");
        let mut fixture = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        fixture.write_all(&bytes).unwrap();
        drop(fixture);
        let locale = Locale::new(Language::En);
        let cancel = AtomicBool::new(false);
        let opened = open(
            &path,
            Options {
                kind: Kind::Disk,
                sector: 4096,
            },
            &locale,
            &cancel,
        )
        .unwrap();
        let source = opened.source;
        assert_eq!(source.browser.info.options.kind, Kind::Search);
        assert!(source.browser.info.partitions.is_empty());
        assert!(source.table(&locale, &cancel).unwrap().contains("GPT"));
        let sector = source.sector(1, &cancel).unwrap();
        assert!(sector.contains("LBA 1 / 1023 · 0x1000 · 4096 B"));
        assert!(sector.contains("45 46 49 20 50 41 52 54"));
        assert!(sector.contains(&format!("{:x}", Sha256::digest(&bytes[4096..8192]))));
        assert!(source.sector(1023, &cancel).is_ok());
        assert!(source.sector(1024, &cancel).is_err());
        assert!(source.sector(u64::MAX, &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert!(source.sector(1, &cancel).is_err());
        drop(source);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }
}

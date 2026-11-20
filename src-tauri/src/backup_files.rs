//! Native file adapter for backups and verified file output. Paths originate
//! only in native dialogs; actual handles are bound to their physical location.
//! Encryption never creates a plaintext temporary file.
use crate::{
    backup_archive::{self, Archive},
    backup_encryption::{self, Password},
    backup_secrets::{self, Purpose},
    data_windows::{self as platform, FileBinding},
    partitions::Target,
};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{fs::MetadataExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::Storage::FileSystem::*;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
pub struct Input {
    pub file: File,
    pub binding: FileBinding,
    pub disk: Target,
    pub name: String,
    pub magic: [u8; 8],
}
impl Input {
    pub fn open(path: &Path, other_disk: Option<&Target>) -> Result<Self, String> {
        let parent = path.parent().ok_or("备份目录无效")?;
        if let Some(other) = other_disk {
            platform::destination_folder(parent, other, 0)?;
        }
        let (parent, disk) = platform::folder_location(parent)?;
        if other_disk == Some(&disk) {
            return Err("备份文件必须位于另一块物理磁盘。".into());
        }
        let leaf = path.file_name().ok_or("备份文件名无效")?;
        if leaf.to_string_lossy().contains(':') {
            return Err("不支持备份数据流路径。".into());
        }
        let directory = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
        let parent_binding =
            FileBinding::new(&directory.try_clone().map_err(err)?.into_std_file(), &disk)?;
        FileBinding::validate_all(std::slice::from_ref(&parent_binding))?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = directory.open_with(leaf, &options).map_err(err)?.into_std();
        let metadata = file.metadata().map_err(err)?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("请选择普通备份文件，不能使用设备或链接。".into());
        }
        let binding = FileBinding::new(&file, &disk)?;
        FileBinding::validate_all(&[parent_binding, binding.clone()])?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic).map_err(err)?;
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        Ok(Self {
            file,
            binding,
            disk,
            name: leaf.to_string_lossy().into_owned(),
            magic,
        })
    }
    pub fn chunked(&self) -> bool {
        backup_archive::is_chunked(&self.magic) || &self.magic == backup_encryption::MAGIC
    }
    /// None means the user cancelled; never reinterpret ciphertext as plaintext.
    pub fn archive(
        self,
        tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Option<Archive>, String> {
        let archive = if &self.magic == backup_encryption::MAGIC {
            let Some(password) = backup_secrets::ask(Purpose::Unlock, &self.name, tick)? else {
                return Ok(None);
            };
            FileBinding::validate_all(std::slice::from_ref(&self.binding))?;
            let reader = backup_encryption::Reader::open(self.file, &password).map_err(err)?;
            drop(password);
            Archive::from_reader(reader)?
        } else {
            Archive::open(self.file)?
        };
        let binding = self.binding;
        Ok(Some(archive.bind_source(move || {
            FileBinding::validate_all(std::slice::from_ref(&binding))
        })?))
    }
}

pub enum Output {
    Plain(File),
    Encrypted(backup_encryption::Writer<File>),
}
impl Output {
    pub fn archive(self) -> Result<Archive, String> {
        match self {
            Self::Plain(file) => Archive::open(file),
            Self::Encrypted(writer) => Archive::from_reader(writer.into_reader().map_err(err)?),
        }
    }
}
impl Write for Output {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(f) => f.write(b),
            Self::Encrypted(f) => f.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(f) => f.flush(),
            Self::Encrypted(f) => f.flush(),
        }
    }
}
impl Seek for Output {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Plain(f) => f.seek(from),
            Self::Encrypted(f) => f.seek(from),
        }
    }
}
impl backup_archive::Output for Output {
    fn reset(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(f) => f.reset(),
            Self::Encrypted(f) => f.reset(),
        }
    }
    fn sync(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(f) => f.sync(),
            Self::Encrypted(f) => f.sync(),
        }
    }
}
pub struct Publication {
    directory: Dir,
    file: File,
    bindings: Vec<FileBinding>,
    name: String,
}
impl Publication {
    pub fn validate(&self) -> Result<(), String> {
        FileBinding::validate_all(&self.bindings)
    }
    pub fn binding(&self) -> FileBinding {
        self.bindings[1].clone()
    }
    pub(crate) fn file_identity(&self) -> Result<[u64; 2], String> {
        sector_file_identity(&self.file)
    }
    /// Caller must have verified the complete output first.
    pub fn publish(self) -> Result<(), String> {
        self.validate()?;
        crate::recovery_output::finish(&self.file, &self.directory, &self.name)
    }
}

pub(crate) fn sector_file_identity(file: &File) -> Result<[u64; 2], String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the live file handle and writable result buffer are valid.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(err(io::Error::last_os_error()));
    }
    if info.nNumberOfLinks != 1
        || info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
    {
        return Err("请选择没有链接的普通扇区输出文件。".into());
    }
    Ok([
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ])
}
pub fn create(
    path: &Path,
    other_disk: &Target,
    required: u64,
    password: Option<&Password>,
) -> Result<(Output, Publication), String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("备份文件名无效")?;
    if name != crate::data_io::safe_name(name) || !name.to_ascii_lowercase().ends_with(".dsbackup")
    {
        return Err("请选择普通 .dsbackup 文件名。".into());
    }
    let (file, publication) = create_output(
        path,
        name,
        other_disk,
        None,
        required,
        "目标备份已存在，未覆盖。",
    )?;
    let output = match password {
        Some(password) => {
            Output::Encrypted(backup_encryption::Writer::new(file, password).map_err(err)?)
        }
        None => Output::Plain(file),
    };
    Ok((output, publication))
}

pub(crate) fn create_sector_output(
    path: &Path,
    source: &Target,
    size: u64,
) -> Result<(File, Publication), String> {
    create_sector_output_excluding(path, std::slice::from_ref(source), size)
}

pub(crate) fn create_sector_output_excluding(
    path: &Path,
    sources: &[Target],
    size: u64,
) -> Result<(File, Publication), String> {
    create_sector_file(path, sources, None, size, ".bin", "请选择普通 .bin 文件名。")
}

pub(crate) fn reopen_sector_output(
    path: &Path,
    source: &Target,
) -> Result<(File, Publication), String> {
    let name = sector_filename(path, ".bin", "请选择普通 .bin 文件名。")?;
    open_output(
        path,
        name,
        std::slice::from_ref(source),
        None,
        0,
        "目标文件已经发布。请选择尚未完成的断点。",
        false,
    )
}

pub(crate) fn create_sector_report(
    path: &Path,
    sources: &[Target],
    destination: Option<&Target>,
    size: u64,
) -> Result<(File, Publication), String> {
    create_sector_file(
        path,
        sources,
        destination,
        size,
        ".json",
        "请选择普通 .json 文件名。",
    )
}

fn create_sector_file(
    path: &Path,
    sources: &[Target],
    destination: Option<&Target>,
    size: u64,
    extension: &str,
    invalid_name: &str,
) -> Result<(File, Publication), String> {
    let name = sector_filename(path, extension, invalid_name)?;
    open_output(
        path,
        name,
        sources,
        destination,
        size,
        "文件已存在，请使用新文件名。",
        true,
    )
}

fn sector_filename<'a>(
    path: &'a Path,
    extension: &str,
    invalid_name: &str,
) -> Result<&'a str, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            *name == crate::data_io::safe_name(name)
                && name.to_ascii_lowercase().ends_with(extension)
                && !crate::recovery_export::reserved(name)
        })
        .ok_or_else(|| invalid_name.into())
}

fn create_output(
    path: &Path,
    name: &str,
    other_disk: &Target,
    destination: Option<&Target>,
    required: u64,
    exists_message: &str,
) -> Result<(File, Publication), String> {
    open_output(
        path,
        name,
        std::slice::from_ref(other_disk),
        destination,
        required,
        exists_message,
        true,
    )
}

fn open_output(
    path: &Path,
    name: &str,
    sources: &[Target],
    destination: Option<&Target>,
    required: u64,
    exists_message: &str,
    create: bool,
) -> Result<(File, Publication), String> {
    let (parent, disk) = platform::destination_location(
        path.parent().ok_or("输出目录无效")?, sources, required,
    )?;
    if destination == Some(&disk) {
        return Err("扇区复制报告需保存到源和目标以外的磁盘。".into());
    }
    let directory = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
    let parent_binding =
        FileBinding::new(&directory.try_clone().map_err(err)?.into_std_file(), &disk)?;
    FileBinding::validate_all(std::slice::from_ref(&parent_binding))?;
    if directory.exists(name) {
        return Err(exists_message.into());
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(create)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = directory
        .open_with(format!("{name}.partial"), &options)
        .map_err(err)?
        .into_std();
    let binding = FileBinding::new(&file, &disk)?;
    let publication = Publication {
        directory,
        file: file.try_clone().map_err(err)?,
        bindings: vec![parent_binding, binding],
        name: name.into(),
    };
    publication.validate()?;
    Ok((file, publication))
}

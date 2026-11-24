//! Native file adapter for backups and verified file output. Paths originate
//! only in native dialogs; actual handles are bound to their physical location.
//! Encryption never creates a plaintext temporary file.
use crate::{
    backup_archive::{self, Archive},
    backup_encryption::{self, Password},
    backup_secrets::{self, Purpose},
    data_tasks::Selection,
    data_windows::{self as platform, FileBinding},
    partitions::Target,
    recovery_output,
};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{fs::MetadataExt, io::AsRawHandle},
    path::{Component, Path, PathBuf},
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
        Self::open_excluding(path, other_disk.map_or(&[], std::slice::from_ref))
    }

    pub(crate) fn open_excluding(path: &Path, sources: &[Target]) -> Result<Self, String> {
        let parent = path.parent().ok_or("备份目录无效")?;
        let (parent, disk) = platform::folder_location(parent)?;
        if sources.contains(&disk) {
            return Err("备份文件必须位于另一块物理磁盘。".into());
        }
        let leaf = path.file_name().ok_or("备份文件名无效")?;
        let directory = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
        let parent_binding =
            FileBinding::new(&directory.try_clone().map_err(err)?.into_std_file(), &disk)?;
        Self::open_bound(&directory, parent_binding, disk, leaf)
    }

    fn open_bound(directory: &Dir, parent_binding: FileBinding, disk: Target, leaf: &std::ffi::OsStr) -> Result<Self, String> {
        if leaf.to_string_lossy().contains(':') {
            return Err("不支持备份数据流路径。".into());
        }
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
        parent_binding.verify_same_volume(&file)?;
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
    create_excluding(path, std::slice::from_ref(other_disk), None, required, password)
}

pub(crate) fn create_excluding(
    path: &Path,
    sources: &[Target],
    expected_disk: Option<&Target>,
    required: u64,
    password: Option<&Password>,
) -> Result<(Output, Publication), String> {
    let name = backup_name(path)?;
    let (file, publication) = open_output(
        path,
        name,
        Placement { sources, excluded: None, expected: expected_disk },
        required,
        "目标备份已存在，未覆盖。",
        true,
    )?;
    let output = match password {
        Some(password) => {
            Output::Encrypted(backup_encryption::Writer::new(file, password).map_err(err)?)
        }
        None => Output::Plain(file),
    };
    Ok((output, publication))
}

pub(crate) fn backup_name(path: &Path) -> Result<&str, String> {
    output_filename(path, ".dsbackup", "请选择普通 .dsbackup 文件名。")
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortableOutput {
    selection: Selection,
    directory: Vec<String>,
    identity: recovery_output::Identity,
    name: String,
}

impl PortableOutput {
    pub(crate) fn prepare(path: &Path, source: &Target, required: u64) -> Result<Self, String> {
        let name = backup_name(path)?.to_owned();
        let (parent, disk) = platform::destination_location(
            path.parent().ok_or("输出目录无效")?, std::slice::from_ref(source), required,
        )?;
        let planned = Self::track(&parent, disk, name, true)?;
        planned.open(source)?;
        Ok(planned)
    }

    fn track(parent: &Path, disk: Target, name: String, writable: bool) -> Result<Self, String> {
        let dir = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
        let location = platform::file_location(&dir)?;
        if location.target != disk {
            return Err("输出磁盘与确认时不同。".into());
        }
        let directory = parent.components().skip(2).map(|component| match component {
            Component::Normal(name) => name.to_str().map(str::to_owned).ok_or("输出目录无效".into()),
            _ => Err("输出目录无效".into()),
        }).collect::<Result<_, String>>()?;
        Ok(Self {
            selection: location.portable_file_selection(writable)?, directory,
            identity: recovery_output::identity(&dir)?, name,
        })
    }

    fn validate_components(&self) -> Result<(), String> {
        if self.directory.len() > 16
            || self.directory.iter().map(String::len).sum::<usize>() > 512
            || backup_name(Path::new(&self.name))? != self.name
        {
            return Err("WinPE 备份输出路径超出限制。".into());
        }
        for component in self.directory.iter().chain(std::iter::once(&self.name)) {
            recovery_output::validate_output_name(component)?;
        }
        Ok(())
    }

    pub(crate) fn open(&self, source: &Target) -> Result<(PathBuf, Dir, FileBinding), String> {
        let (path, dir, binding) = self.open_directory(source, true)?;
        for name in [&self.name, &format!("{}.partial", self.name)] {
            match dir.symlink_metadata(name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Err("目标备份或未完成文件已存在，未覆盖。".into()),
                Err(error) => return Err(err(error)),
            }
        }
        Ok((path, dir, binding))
    }

    fn open_directory(&self, other_disk: &Target, writable: bool) -> Result<(PathBuf, Dir, FileBinding), String> {
        self.validate_components()?;
        if other_disk == &self.selection.target {
            return Err("备份文件必须位于另一块物理磁盘。".into());
        }
        let (volume, root, volume_id) = platform::portable_file_volume(&self.selection, writable)?;
        let mut path = PathBuf::from(root);
        let mut dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(err)?;
        for component in &self.directory {
            dir = dir.open_dir_nofollow(component).map_err(err)?;
            path.push(component);
        }
        if recovery_output::identity(&dir)? != self.identity {
            return Err("WinPE 备份文件目录与确认时不同。".into());
        }
        platform::file_location(&dir)?.confirm_partition(
            &self.selection.target, &self.selection.layout_key, &volume_id,
            (volume.disk.number, volume.offset, volume.size),
        )?;
        let binding = FileBinding::new(&dir.try_clone().map_err(err)?.into_std_file(), &self.selection.target)?;
        Ok((path.join(&self.name), dir, binding))
    }

    pub(crate) fn create(&self, source: &Target, required: u64) -> Result<(Output, Publication), String> {
        let (path, dir, binding) = self.open(source)?;
        let (_, disk) = platform::destination_location(
            path.parent().ok_or("输出目录无效")?, std::slice::from_ref(source), required,
        )?;
        if disk != self.selection.target {
            return Err("输出磁盘与确认时不同。".into());
        }
        let (file, publication) = open_bound_output(dir, binding, &disk, &self.name, true)?;
        Ok((Output::Plain(file), publication))
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortableInput {
    location: PortableOutput,
    file: [u64; 2],
    bytes: u64,
}

impl PortableInput {
    pub(crate) fn prepare(path: &Path, destination: &Target) -> Result<(Self, Input), String> {
        let name = backup_name(path)?.to_owned();
        let (parent, disk) = platform::folder_location(path.parent().ok_or("备份目录无效")?)?;
        let location = PortableOutput::track(&parent, disk, name, false)?;
        let (_, dir, binding) = location.open_directory(destination, false)?;
        let input = Input::open_bound(&dir, binding, location.selection.target.clone(), location.name.as_ref())?;
        let file = sector_file_identity(&input.file)?;
        let bytes = input.file.metadata().map_err(err)?.len();
        Ok((Self { location, file, bytes }, input))
    }

    pub(crate) fn open(&self, destination: &Target) -> Result<Input, String> {
        let (_, dir, binding) = self.location.open_directory(destination, false)?;
        let input = Input::open_bound(&dir, binding, self.location.selection.target.clone(), self.location.name.as_ref())?;
        if sector_file_identity(&input.file)? != self.file || input.file.metadata().map_err(err)?.len() != self.bytes {
            return Err("WinPE 备份文件与确认时不同。".into());
        }
        Ok(input)
    }
}

#[test]
fn portable_backup_output_accepts_only_relative_ordinary_path_components() {
    let probe = |directory: &str, name: &str| {
        let output: PortableOutput = serde_json::from_value(serde_json::json!({
            "selection": {"target": "a".repeat(64), "layoutKey": "b".repeat(64), "partition": 1},
            "directory": [directory], "identity": {"volume": 1, "high": 0, "low": 2},
            "name": name,
        })).unwrap();
        output.validate_components()
    };
    assert!(probe("backup", "source.dsbackup").is_ok());
    assert!(probe("..", "source.dsbackup").is_err());
    assert!(probe("C:\\", "source.dsbackup").is_err());
    assert!(probe("backup", "old\\source.dsbackup").is_err());
    assert!(probe("backup", "source.dsbackup:alternate").is_err());
}
pub(crate) fn table_name(path: &Path) -> Result<&str, String> {
    output_filename(path, ".dspt", "请选择新的 .dspt 文件名。")
}

pub(crate) fn create_image_table_archive(
    path: &Path,
    sources: &[Target],
    destination: &Target,
    size: u64,
) -> Result<(File, Publication), String> {
    open_output(
        path,
        table_name(path)?,
        Placement { sources, excluded: None, expected: Some(destination) },
        size,
        "目标备份已存在，未覆盖。",
        true,
    )
}

pub(crate) fn image_name(path: &Path, format: crate::image_convert::Format) -> Result<&str, String> {
    output_filename(path, &format!(".{}", format.extension()), "请选择扩展名与格式相符的新镜像文件。")
}

pub(crate) fn create_image_output(
    path: &Path,
    format: crate::image_convert::Format,
    source: &Target,
    expected_disk: &Target,
    required: u64,
) -> Result<(File, Publication), String> {
    create_image_output_excluding(path, format, std::slice::from_ref(source), expected_disk, required)
}

pub(crate) fn create_image_output_excluding(
    path: &Path,
    format: crate::image_convert::Format,
    sources: &[Target],
    expected_disk: &Target,
    required: u64,
) -> Result<(File, Publication), String> {
    open_output(
        path,
        image_name(path, format)?,
        Placement { sources, excluded: None, expected: Some(expected_disk) },
        required,
        "目标已存在；不会覆盖已有文件。",
        true,
    )
}

pub(crate) fn verify_output(
    output: Output,
    publication: &Publication,
    parent: Option<backup_archive::Chain>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<(), String> {
    publication.validate()?;
    let binding = publication.binding();
    let archive = output.archive()?.bind_source(move || {
        FileBinding::validate_all(std::slice::from_ref(&binding))
    })?;
    let mut chain = match parent {
        Some(parent) => parent.prepend(archive)?,
        None => backup_archive::Chain::new(archive),
    };
    chain.verify(tick)
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
    let name = output_filename(path, ".bin", "请选择普通 .bin 文件名。")?;
    open_output(
        path,
        name,
        Placement { sources: std::slice::from_ref(source), excluded: None, expected: None },
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

pub(crate) fn create_firmware_entry_backup(
    path: &Path,
    size: u64,
) -> Result<(File, Publication), String> {
    create_sector_file(path, &[], None, size, ".dsuefi", "请选择新的 .dsuefi 文件名。")
}

pub(crate) fn create_boot_repair_backup(
    path: &Path,
    source: &Target,
    source_number: u32,
    size: u64,
    includes_files: bool,
) -> Result<(File, Publication), String> {
    let sources = std::slice::from_ref(source);
    let (_, destination) = platform::destination_location_excluding(
        path.parent().ok_or("输出目录无效")?,
        sources,
        &[source_number],
        size,
    )?;
    let (extension, invalid_name) = if includes_files {
        (".zip", "请选择新的 .zip 文件名。")
    } else {
        (".bcd", "请选择新的 .bcd 文件名。")
    };
    open_output(
        path,
        output_filename(path, extension, invalid_name)?,
        Placement { sources, excluded: None, expected: Some(&destination) },
        size,
        "文件已存在，请使用新文件名。",
        true,
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
    let name = output_filename(path, extension, invalid_name)?;
    open_output(
        path,
        name,
        Placement { sources, excluded: destination, expected: None },
        size,
        "文件已存在，请使用新文件名。",
        true,
    )
}

fn output_filename<'a>(
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

struct Placement<'a> {
    sources: &'a [Target],
    excluded: Option<&'a Target>,
    expected: Option<&'a Target>,
}

fn open_output(
    path: &Path,
    name: &str,
    placement: Placement<'_>,
    required: u64,
    exists_message: &str,
    create: bool,
) -> Result<(File, Publication), String> {
    let (parent, disk) = platform::destination_location(
        path.parent().ok_or("输出目录无效")?, placement.sources, required,
    )?;
    if placement.expected.is_some_and(|expected| expected != &disk) {
        return Err("输出磁盘与确认时不同。".into());
    }
    if placement.excluded == Some(&disk) {
        return Err("扇区复制报告需保存到源和目标以外的磁盘。".into());
    }
    let directory = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
    let parent_binding =
        FileBinding::new(&directory.try_clone().map_err(err)?.into_std_file(), &disk)?;
    FileBinding::validate_all(std::slice::from_ref(&parent_binding))?;
    if directory.exists(name) {
        return Err(exists_message.into());
    }
    open_bound_output(directory, parent_binding, &disk, name, create)
}

fn open_bound_output(
    directory: Dir,
    parent_binding: FileBinding,
    disk: &Target,
    name: &str,
    create: bool,
) -> Result<(File, Publication), String> {
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
    let binding = FileBinding::new(&file, disk)?;
    parent_binding.verify_same_volume(&file)?;
    let publication = Publication {
        directory,
        file: file.try_clone().map_err(err)?,
        bindings: vec![parent_binding, binding],
        name: name.into(),
    };
    publication.validate()?;
    Ok((file, publication))
}

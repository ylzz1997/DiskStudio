//! Windows destination materialization for the verified NTFS merge catalogue.
//! The caller must keep the source volume locked throughout the copy.
use super::{
    check, err,
    metadata::{self, OBJECT_ID},
    validate_component, Catalogue, Node, RecordId, RelativePath, Stream,
};
use crate::{
    data_tasks::Selection,
    data_windows as device,
    partitions::policy::{Disk, Partition},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    mem::{offset_of, size_of},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr::{null, null_mut},
    sync::atomic::AtomicBool,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FileAlternateNameInformation, FileLinkInformation, NtCreateFile,
            NtQueryInformationFile, NtSetEaFile, NtSetInformationFile, FILE_CREATE,
            FILE_DIRECTORY_FILE, FILE_LINK_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        },
    },
    Win32::{
        Foundation::{
            GetLastError, RtlNtStatusToDosError, ERROR_NOT_ALL_ASSIGNED, ERROR_NO_TOKEN,
            ERROR_SUCCESS, HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
            STATUS_OBJECT_NAME_NOT_FOUND, UNICODE_STRING,
        },
        Security::{
            AdjustTokenPrivileges, DuplicateTokenEx, LookupPrivilegeValueW, SecurityImpersonation,
            TokenImpersonation, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
            SE_SECURITY_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
            TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        Storage::FileSystem::*,
        System::{
            Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_OBJECT_ID, FSCTL_SET_SPARSE},
            Threading::{
                GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
                SetThreadToken,
            },
            IO::IO_STATUS_BLOCK,
        },
    },
};

const ENTRY_ACCESS: u32 = FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | FILE_WRITE_DATA
    | FILE_WRITE_EA
    | READ_CONTROL
    | WRITE_DAC
    | WRITE_OWNER
    | SYNCHRONIZE;
const ACCESS_SYSTEM_SECURITY: u32 = 0x0100_0000;
const DIRECTORY_ACCESS: u32 =
    FILE_READ_ATTRIBUTES | FILE_TRAVERSE | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | SYNCHRONIZE;
const SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const SETTABLE: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_NORMAL
    | FILE_ATTRIBUTE_TEMPORARY
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
const PRESERVED_BY_STORAGE: u32 =
    FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED;
// [MS-BKUP] WIN32_STREAM_ID flags.
const STREAM_CONTAINS_SECURITY: u32 = 0x0000_0002;

/// Identifies the created directory in the independent raw destination readback.
pub(crate) struct CopiedDirectory {
    pub name: Vec<u16>,
    pub record: RecordId,
}

impl CopiedDirectory {
    pub(crate) fn name(id: uuid::Uuid) -> String {
        format!("DiskStudio-Merge-{id}")
    }

    /// Read from a fresh bounded reader while the destination volume is locked.
    pub(crate) fn verify<R: Read + Seek>(
        &self,
        source: &Catalogue,
        reader: &mut R,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let copy = super::catalogue(reader, Some((self.record, &self.name)), cancel)?;
        source.verify_copy(&copy)
    }
}

/// The volume name comes from the Windows provider association and is checked
/// again using the root handle. A failure after directory creation leaves the
/// partial copy in its new directory for inspection.
pub(crate) fn copy<R: Read + Seek>(
    selection: &Selection,
    disk: &Disk,
    partition: &Partition,
    volume: &str,
    id: uuid::Uuid,
    source: &Catalogue,
    reader: &mut R,
    cancel: &AtomicBool,
) -> Result<CopiedDirectory, String> {
    if selection.partition != Some(partition.partition_number)
        || disk.number != partition.disk_number
    {
        return Err("合并目标分区与选择不一致。".into());
    }
    let plan = CopyPlan::new(source, cancel)?;
    let restore_token = restore_token(plan.needs_sacl)?;
    let volume = device::volume_root(volume)?;
    let volume_root = OpenOptions::new()
        .access_mode(DIRECTORY_ACCESS)
        .share_mode(SHARING)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(format!("{volume}\\"))
        .map_err(err)?;
    let root_identity = identity(&volume_root, Some(true))?;
    if root_identity.record().0 != super::ROOT {
        return Err("打开的目标目录不是 NTFS 卷根。".into());
    }
    device::file_location(&volume_root)?.confirm_partition(
        &selection.target,
        &selection.layout_key,
        &volume,
        (disk.number, partition.offset, partition.size),
    )?;
    let folder = CopiedDirectory::name(id);
    let name = folder.encode_utf16().collect::<Vec<_>>();
    check(cancel)?;
    let root = open_entry(
        &volume_root,
        &name,
        true,
        true,
        DIRECTORY_ACCESS | ENTRY_ACCESS,
    )?;
    let result = (|| {
        let created = identity(&root, Some(true))?;
        created.on_volume(root_identity)?;
        let mut tree = Tree {
            root: &root,
            identities: BTreeMap::from([(vec![], created)]),
        };
        tree.materialize(&volume_root, &name, &plan, source, reader, cancel)?;
        std::thread::scope(|scope| {
            let volume_root = &volume_root;
            let name = &name;
            std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    // This short-lived worker uses the caller's identity. The
                    // application token and the disk-operation worker stay intact.
                    win32(unsafe { SetThreadToken(null(), restore_token.as_raw_handle()) })?;
                    tree.restore_metadata(volume_root, name, source, cancel)
                })
                .map_err(err)?
                .join()
                .map_err(|_| "NTFS 文件元数据还原任务意外结束。".to_string())?
        })?;
        Ok(CopiedDirectory {
            name,
            record: created.record(),
        })
    })();
    result.map_err(|error: String| format!("目标卷目录 {folder} 已创建，文件迁移中止。{error}"))
}

struct CopyPlan<'a> {
    primary: BTreeMap<RecordId, &'a RelativePath>,
    aliases: BTreeMap<&'a RelativePath, &'a [u16]>,
    needs_sacl: bool,
}

impl<'a> CopyPlan<'a> {
    fn new(source: &'a Catalogue, cancel: &AtomicBool) -> Result<Self, String> {
        let mut primary = BTreeMap::new();
        for (path, &id) in &source.names {
            check(cancel)?;
            primary.entry(id).or_insert(path);
        }
        let aliases = metadata::short_name_links(source, cancel)?;
        let mut needs_sacl = false;
        for (&id, node) in &source.nodes {
            check(cancel)?;
            if node.attributes & !(SETTABLE | PRESERVED_BY_STORAGE) != 0 {
                return Err("源文件包含无法逐一还原的 NTFS 属性。".into());
            }
            for stream in &node.streams {
                if !stream.locator.name.is_empty() {
                    stream_name(&[], &stream.locator.name)?;
                }
            }
            // Inspect every descriptor and EA before the first destination write.
            needs_sacl |= metadata_access(source.descriptor(id)?) & ACCESS_SYSTEM_SECURITY != 0;
            extended_attributes(node)?;
        }
        Ok(Self {
            primary,
            aliases,
            needs_sacl,
        })
    }
}

struct Tree<'a> {
    root: &'a File,
    identities: BTreeMap<RelativePath, Identity>,
}

impl Tree<'_> {
    fn materialize<R: Read + Seek>(
        &mut self,
        volume_root: &File,
        stage_name: &[u16],
        plan: &CopyPlan<'_>,
        source: &Catalogue,
        reader: &mut R,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let root_id = source.names[&Vec::new()];
        copy_data(
            volume_root,
            stage_name,
            self.root,
            source,
            root_id,
            reader,
            cancel,
        )?;
        for (path, &id) in source.names.iter().filter(|(path, _)| !path.is_empty()) {
            check(cancel)?;
            let (name, parent_path) = path.split_last().unwrap();
            let parents = self.parents(parent_path)?;
            let parent = parents.last().unwrap_or(self.root);
            let node = &source.nodes[&id];
            let actual = if plan.primary[&id] == path {
                let access = ENTRY_ACCESS | if node.directory { DIRECTORY_ACCESS } else { 0 };
                let file = open_entry(parent, name, node.directory, true, access)?;
                let actual = identity(&file, Some(node.directory))?;
                actual.on_volume(self.identities[&Vec::new()])?;
                copy_data(parent, name, &file, source, id, reader, cancel)?;
                actual
            } else {
                let canonical = plan.primary[&id];
                let (file, _parents) = self.reopen(
                    canonical,
                    false,
                    FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
                )?;
                link(&file, parent, name)?;
                let linked = open_entry(parent, name, false, false, FILE_READ_ATTRIBUTES)?;
                self.identities[canonical].confirm(&linked, Some(false))?;
                self.identities[canonical]
            };
            self.identities.insert(path.clone(), actual);
        }

        // Remove only generated aliases which would conflict with the source.
        // Doing that before adding the source aliases allows names to swap.
        let mut pending = Vec::new();
        for (path, &id) in source.names.iter().filter(|(path, _)| !path.is_empty()) {
            check(cancel)?;
            let directory = source.nodes[&id].directory;
            let wanted = plan.aliases.get(path).copied().unwrap_or_default();
            let current = {
                let (file, _parents) = self.reopen(path, directory, FILE_READ_ATTRIBUTES)?;
                short_name(&file)?
            };
            if current != wanted {
                if !current.is_empty() && &current != path.last().unwrap() {
                    self.set_short_name(path, directory, &[])?;
                }
                if !wanted.is_empty() {
                    pending.push((path, id, wanted));
                }
            }
        }
        for (path, id, wanted) in pending {
            check(cancel)?;
            self.set_short_name(path, source.nodes[&id].directory, wanted)?;
        }
        Ok(())
    }

    fn restore_metadata(
        &self,
        volume_root: &File,
        stage_name: &[u16],
        source: &Catalogue,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        // Finalize ancestors after descendants so that restored ACLs and
        // timestamps do not inhibit the copy or record later child creation.
        let mut finished = BTreeSet::new();
        for (path, &id) in source
            .names
            .iter()
            .rev()
            .filter(|(path, _)| !path.is_empty())
        {
            check(cancel)?;
            if finished.insert(id) {
                let descriptor = source.descriptor(id)?;
                let node = &source.nodes[&id];
                let (file, _parents) =
                    self.reopen(path, node.directory, metadata_access(descriptor))?;
                finalize(&file, node, descriptor)?;
            }
        }
        check(cancel)?;
        let root_id = source.names[&Vec::new()];
        let descriptor = source.descriptor(root_id)?;
        let root = open_entry(
            volume_root,
            stage_name,
            true,
            false,
            metadata_access(descriptor),
        )?;
        self.identities[&Vec::new()].confirm(&root, Some(true))?;
        finalize(&root, &source.nodes[&root_id], descriptor)
    }

    fn parents(&self, path: &[Vec<u16>]) -> Result<Vec<File>, String> {
        let mut parents = Vec::with_capacity(path.len());
        for (index, name) in path.iter().enumerate() {
            let parent = parents.last().unwrap_or(self.root);
            let file = open_entry(parent, name, true, false, DIRECTORY_ACCESS)?;
            self.identities
                .get(&path[..=index])
                .ok_or("目标父目录缺少创建凭据。")?
                .confirm(&file, Some(true))?;
            parents.push(file);
        }
        Ok(parents)
    }

    fn reopen(
        &self,
        path: &RelativePath,
        directory: bool,
        access: u32,
    ) -> Result<(File, Vec<File>), String> {
        let (name, parent_path) = path.split_last().ok_or("缺少合并目标的相对文件名。")?;
        let parents = self.parents(parent_path)?;
        let file = open_entry(
            parents.last().unwrap_or(self.root),
            name,
            directory,
            false,
            access,
        )?;
        self.identities
            .get(path)
            .ok_or("缺少目标文件的创建凭据。")?
            .confirm(&file, Some(directory))?;
        Ok((file, parents))
    }

    fn set_short_name(
        &self,
        path: &RelativePath,
        directory: bool,
        name: &[u16],
    ) -> Result<(), String> {
        let (file, _parents) = self.reopen(
            path,
            directory,
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | DELETE,
        )?;
        let name = name.iter().copied().chain([0]).collect::<Vec<_>>();
        win32(unsafe { SetFileShortNameW(file.as_raw_handle(), name.as_ptr()) })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    serial: u32,
    reference: u64,
}

impl Identity {
    fn record(self) -> RecordId {
        RecordId(
            self.reference & 0x0000_ffff_ffff_ffff,
            (self.reference >> 48) as u16,
        )
    }

    fn confirm(self, file: &File, directory: Option<bool>) -> Result<(), String> {
        if identity(file, directory)? != self {
            return Err("目标文件与创建时记录的 NTFS 身份不同。".into());
        }
        Ok(())
    }

    fn on_volume(self, parent: Self) -> Result<(), String> {
        if self.serial != parent.serial {
            return Err("目标文件离开了已确认的 NTFS 卷。".into());
        }
        Ok(())
    }
}

fn identity(file: &File, directory: Option<bool>) -> Result<Identity, String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    win32(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || directory.is_some_and(|directory| {
            (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        })
    {
        return Err("合并目标文件类型或重解析属性异常。".into());
    }
    let identity = Identity {
        serial: info.dwVolumeSerialNumber,
        reference: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    };
    if identity.record().1 == 0 {
        return Err("目标文件缺少可回读的 NTFS 记录序号。".into());
    }
    Ok(identity)
}

fn open_entry(
    parent: &File,
    name: &[u16],
    directory: bool,
    create: bool,
    access: u32,
) -> Result<File, String> {
    validate_component(name)?;
    let options = if directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    open_relative(parent, name, create, access, options)
}

fn open_relative(
    parent: &File,
    name: &[u16],
    create: bool,
    access: u32,
    options: u32,
) -> Result<File, String> {
    let length = u16::try_from(name.len() * 2).map_err(err)?;
    let mut name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        ..Default::default()
    };
    let mut handle = null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    ntstatus(unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            SHARING,
            if create { FILE_CREATE } else { FILE_OPEN },
            options
                | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT,
            null(),
            0,
        )
    })?;
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err("Windows 未返回已确认的目标文件句柄。".into());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    // IO_STATUS_BLOCK.Information contains FILE_CREATED or FILE_OPENED.
    if status.Information != if create { 2 } else { 1 } {
        return Err("Windows 返回的文件创建或打开状态与要求不同。".into());
    }
    Ok(file)
}

fn link(file: &File, parent: &File, name: &[u16]) -> Result<(), String> {
    validate_component(name)?;
    #[repr(C)]
    struct LinkInformation {
        flags: u32,
        root: HANDLE,
        length: u32,
        name: [u16; 255],
    }
    const NAME_OFFSET: usize = offset_of!(FILE_LINK_INFORMATION, FileName);
    const _: () = assert!(NAME_OFFSET == offset_of!(LinkInformation, name));
    let mut input = LinkInformation {
        flags: 0,
        root: parent.as_raw_handle(),
        length: (name.len() * 2) as u32,
        name: [0; 255],
    };
    input.name[..name.len()].copy_from_slice(name);
    ntstatus(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut IO_STATUS_BLOCK::default(),
            (&raw const input).cast(),
            size_of::<FILE_LINK_INFORMATION>().max(NAME_OFFSET + name.len() * 2) as u32,
            FileLinkInformation,
        )
    })
}

fn short_name(file: &File) -> Result<Vec<u16>, String> {
    // The information class returns the byte length followed by UTF-16 units.
    let mut buffer = [0u32; 8];
    let mut result = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut result,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            FileAlternateNameInformation,
        )
    };
    if status == STATUS_OBJECT_NAME_NOT_FOUND {
        return Ok(vec![]);
    }
    ntstatus(status)?;
    let bytes = buffer[0] as usize;
    if bytes > 24 || bytes % 2 != 0 || result.Information < bytes + 4 {
        return Err("Windows 返回的 NTFS 短文件名格式无效。".into());
    }
    Ok(buffer[1..]
        .iter()
        .flat_map(|&word| [word as u16, (word >> 16) as u16])
        .take(bytes / 2)
        .collect())
}

fn stream_name(file: &[u16], raw: &[u8]) -> Result<Vec<u16>, String> {
    crate::ntfs_streams::validate_name(raw, false)?;
    let name = raw
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    // The only colons are inserted here. Neither the name nor its type can
    // change the base file, leave this directory, or address metadata streams.
    if name.clone().any(|ch| matches!(ch, 47 | 58 | 92)) {
        return Err("NTFS 数据流名称无法安全地用于 Windows 文件句柄。".into());
    }
    Ok(file
        .iter()
        .copied()
        .chain([58])
        .chain(name)
        .chain(":$DATA".encode_utf16())
        .collect())
}

fn copy_data<R: Read + Seek>(
    parent: &File,
    name: &[u16],
    base: &File,
    source: &Catalogue,
    id: RecordId,
    reader: &mut R,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let node = &source.nodes[&id];
    let base_identity = identity(base, Some(node.directory))?;
    if node.directory {
        compression(base, node.attributes & FILE_ATTRIBUTE_COMPRESSED != 0)?;
    }
    for stream in &node.streams {
        check(cancel)?;
        if stream.locator.name.is_empty() {
            copy_stream(base, source, id, reader, stream, cancel)?;
        } else {
            let stream_name = stream_name(name, &stream.locator.name)?;
            let output = open_relative(
                parent,
                &stream_name,
                true,
                FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
                0,
            )?;
            base_identity.confirm(&output, None)?;
            copy_stream(&output, source, id, reader, stream, cancel)?;
        }
    }
    Ok(())
}

fn compression(file: &File, requested: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_COMPRESSION;
    let mut state = [0; 2];
    let wanted = if requested { 2u16 } else { 0u16 }.to_le_bytes();
    if device::control(file, FSCTL_GET_COMPRESSION, &[], &mut state)? != state.len() {
        return Err("目标 NTFS 压缩状态缺失。".into());
    }
    if state != wanted {
        device::control(file, FSCTL_SET_COMPRESSION, &wanted, &mut [])?;
    }
    Ok(())
}

fn copy_stream<R: Read + Seek>(
    file: &File,
    source: &Catalogue,
    id: RecordId,
    reader: &mut R,
    stream: &Stream,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let sparse = stream.storage_flags & 0x8000 != 0;
    compression(file, stream.storage_flags & 1 != 0)?;
    if sparse {
        device::control(file, FSCTL_SET_SPARSE, &[], &mut [])?;
    }
    let mut output = SparseOutput { file, sparse };
    source.transfer_stream(reader, id, &stream.locator.name, &mut output, cancel)?;
    file.set_len(stream.size)
        .and_then(|_| file.sync_all())
        .map_err(err)
}

struct SparseOutput<'a> {
    file: &'a File,
    sparse: bool,
}

impl Write for SparseOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.sparse && !bytes.is_empty() && bytes.iter().all(|&byte| byte == 0) {
            self.file.seek(SeekFrom::Current(bytes.len() as i64))?;
            Ok(bytes.len())
        } else {
            self.file.write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn metadata_access(descriptor: &[u8]) -> u32 {
    let control = u16::from_le_bytes([descriptor[2], descriptor[3]]);
    ENTRY_ACCESS
        | if control & 0x0010 != 0 {
            ACCESS_SYSTEM_SECURITY
        } else {
            0
        }
}

fn restore_token(needs_sacl: bool) -> Result<OwnedHandle, String> {
    let mut handle = null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 0, &mut handle) } != 0 {
        let _token = unsafe { OwnedHandle::from_raw_handle(handle) };
        return Err("当前线程正在使用其他安全上下文，无法还原 NTFS 所有者。".into());
    }
    let status = unsafe { GetLastError() };
    if status != ERROR_NO_TOKEN {
        return Err(err(io::Error::from_raw_os_error(status as i32)));
    }
    win32(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE, &mut handle) })?;
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut duplicate = null_mut();
    win32(unsafe {
        DuplicateTokenEx(
            process.as_raw_handle(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY | TOKEN_IMPERSONATE,
            null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut duplicate,
        )
    })?;
    let token = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    let mut privileges = vec![(SE_RESTORE_NAME, false)];
    if needs_sacl {
        privileges.push((SE_SECURITY_NAME, true));
    }
    for (privilege, required) in privileges {
        let mut luid = Default::default();
        win32(unsafe { LookupPrivilegeValueW(null(), privilege, &mut luid) })?;
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        win32(unsafe {
            AdjustTokenPrivileges(token.as_raw_handle(), 0, &state, 0, null_mut(), null_mut())
        })?;
        match unsafe { GetLastError() } {
            ERROR_SUCCESS => {}
            ERROR_NOT_ALL_ASSIGNED if !required => {}
            ERROR_NOT_ALL_ASSIGNED => {
                return Err("当前 Windows 账户令牌缺少保留 NTFS 审核 ACL 所需的权限。".into());
            }
            status => return Err(err(io::Error::from_raw_os_error(status as i32))),
        }
    }
    Ok(token)
}

fn finalize(file: &File, node: &Node, descriptor: &[u8]) -> Result<(), String> {
    if let Some(data) = node.metadata.get(&OBJECT_ID) {
        let mut object_id = [0; 64];
        object_id[..data.len()].copy_from_slice(data);
        device::control(file, FSCTL_SET_OBJECT_ID, &object_id, &mut [])?;
    }
    if let Some(ea) = extended_attributes(node)? {
        let aligned = ea
            .chunks(4)
            .map(|chunk| {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                u32::from_le_bytes(word)
            })
            .collect::<Vec<_>>();
        ntstatus(unsafe {
            NtSetEaFile(
                file.as_raw_handle(),
                &mut IO_STATUS_BLOCK::default(),
                aligned.as_ptr().cast(),
                ea.len() as u32,
            )
        })?;
    }
    restore_security(file, descriptor)?;
    let [creation, modified, accessed] = node.times.map(|value| value as i64);
    let attrs = node.attributes & SETTABLE & !FILE_ATTRIBUTE_NORMAL;
    let info = FILE_BASIC_INFO {
        CreationTime: creation,
        LastAccessTime: accessed,
        LastWriteTime: modified,
        ChangeTime: 0,
        FileAttributes: if attrs == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attrs
        },
    };
    win32(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw const info).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    // Directory metadata is flushed by the subsequent successful volume lock.
    if !node.directory {
        file.sync_all().map_err(err)?;
    }
    Ok(())
}

fn restore_security(file: &File, descriptor: &[u8]) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(20 + descriptor.len());
    bytes.extend_from_slice(&BACKUP_SECURITY_DATA.to_le_bytes());
    bytes.extend_from_slice(&STREAM_CONTAINS_SECURITY.to_le_bytes());
    bytes.extend_from_slice(&(descriptor.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(descriptor);
    let mut context = null_mut();
    let mut written = 0;
    let result = win32(unsafe {
        BackupWrite(
            file.as_raw_handle(),
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            0,
            1,
            &mut context,
        )
    });
    let mut ignored = 0;
    let cleanup = if context.is_null() {
        Ok(())
    } else {
        win32(unsafe {
            BackupWrite(
                file.as_raw_handle(),
                null(),
                0,
                &mut ignored,
                1,
                1,
                &mut context,
            )
        })
    };
    result?;
    cleanup?;
    if written as usize != bytes.len() {
        return Err("Windows 未完整还原文件安全描述符。".into());
    }
    Ok(())
}

fn extended_attributes(node: &Node) -> Result<Option<Vec<u8>>, String> {
    if node
        .metadata
        .get(&OBJECT_ID)
        .is_some_and(|bytes| !matches!(bytes.len(), 16 | 64))
    {
        return Err("NTFS 对象标识的长度无效。".into());
    }
    metadata::extended_attributes(node)?
        .map(|(info, ea)| ea_for_windows(info, ea))
        .transpose()
}

fn ea_for_windows(info: &[u8], input: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(input.len());
    metadata::visit_ea(info, input, |record, padded, last| {
        if record[2..4] == [0, 0] {
            return Err("NTFS 扩展属性的记录、长度或名称无效。".into());
        }
        let next = if last { 0 } else { padded as u32 };
        output.extend_from_slice(&next.to_le_bytes());
        output.extend_from_slice(record);
        if !last {
            output.resize(output.len() + padded - record.len() - 4, 0);
        }
        Ok(())
    })?;
    Ok(output)
}

fn win32(code: i32) -> Result<(), String> {
    if code == 0 {
        Err(err(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn ntstatus(status: i32) -> Result<(), String> {
    if status < 0 {
        Err(err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_names_cannot_redirect_a_relative_create_or_change_stream_type() {
        let utf16_bytes = |name: &str| {
            name.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        };
        let base = "a.bin".encode_utf16().collect::<Vec<_>>();
        for wrong in [
            "",
            "path/child",
            "path\\child",
            ":$INDEX_ALLOCATION",
            "null\0tail",
        ] {
            assert!(stream_name(&base, &utf16_bytes(wrong)).is_err());
        }
        assert_eq!(
            stream_name(&base, &utf16_bytes("$DATA")).unwrap(),
            "a.bin:$DATA:$DATA".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            stream_name(&base, &utf16_bytes("标签")).unwrap(),
            "a.bin:标签:$DATA".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn extended_attributes_check_record_boundaries_names_and_counts() {
        let info = [14, 0, 1, 0, 24, 0, 0, 0];
        let ea = [
            12, 0, 0, 0, 0x80, 1, 1, 0, b'A', 0, b'1', 0, 12, 0, 0, 0, 0, 1, 1, 0, b'B', 0, b'2', 0,
        ];
        let converted = ea_for_windows(&info, &ea).unwrap();
        assert_eq!(&converted[..12], &ea[..12]);
        assert_eq!(&converted[12..], &[0, 0, 0, 0, 0, 1, 1, 0, b'B', 0, b'2']);
        for (offset, value) in [(0, 9), (4, 1), (8, b':'), (9, 1), (12, 17), (20, b'a')] {
            let mut corrupt = ea;
            corrupt[offset] = value;
            assert!(ea_for_windows(&info, &corrupt).is_err());
        }
        assert!(ea_for_windows(&[14, 0, 0, 0, 24, 0, 0, 0], &ea).is_err());
    }
}

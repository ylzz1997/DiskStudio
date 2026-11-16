//! Windows output adapter. Only called after native destination selection and
//! cross-disk validation. All descendant access is relative to retained Dir
//! capabilities; names come from the internal export plan, never IPC paths.
use crate::recovery_export::{Entry, Plan, Sink, REPORT, REPORT_PARTIAL};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    },
    Win32::{Foundation::RtlNtStatusToDosError, System::IO::IO_STATUS_BLOCK},
};

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    volume: u32,
    high: u32,
    low: u32,
}
fn identity(dir: &Dir) -> Result<Identity, String> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    if unsafe { GetFileInformationByHandle(dir.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(err(std::io::Error::last_os_error()));
    }
    let info = unsafe { info.assume_init() };
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("输出目录是链接、挂载点或类型不正确。".into());
    }
    Ok(Identity {
        volume: info.dwVolumeSerialNumber,
        high: info.nFileIndexHigh,
        low: info.nFileIndexLow,
    })
}
fn create(dir: &Dir, name: &str) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ);
    dir.open_with(name, &options)
        .map(|f| f.into_std())
        .map_err(err)
}
pub(crate) fn finish(file: &File, dir: &Dir, name: &str) -> Result<(), String> {
    // Rename the exact still-open output handle; no source-path race and no
    // replacement of an existing destination. RootDirectory stays pinned.
    // The Win32 wrapper can reinterpret a relative name against the process
    // directory. Use the documented native relative-root form directly.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || name.encode_utf16().count() > 255
        || crate::recovery_export::reserved(name)
    {
        return Err("输出文件名必须为普通文件名，不能包含路径或特殊设备名称。".into());
    }
    let name: Vec<u16> = name.encode_utf16().collect();
    let length = std::mem::size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
    let mut buffer = vec![0u64; length.div_ceil(8)];
    unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        // This is a flexible array in an aligned allocation. Avoid forming a
        // one-element Rust reference before writing the remaining UTF-16 name.
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        // Zeroed union means ReplaceIfExists=false.
        // All callers create synchronous ordinary files without OVERLAPPED.
        let mut io = IO_STATUS_BLOCK::default();
        let status = NtSetInformationFile(
            file.as_raw_handle(),
            &mut io,
            info.cast(),
            length as u32,
            FileRenameInformation,
        );
        if status < 0 {
            return Err(format!(
                "发布输出文件失败：{}（NTSTATUS 0x{:08X}）",
                std::io::Error::from_raw_os_error(RtlNtStatusToDosError(status) as i32),
                status as u32
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destination_validation_failure_creates_no_directory_or_file() {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join(format!("output-reject-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&parent).unwrap();
        let result = Destination::new_checked(&parent, |dir| {
            assert_eq!(dir.entries().unwrap().count(), 0);
            Err("simulated wrong disk".into())
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        std::fs::remove_dir(parent).unwrap();
    }
    fn fixture(size: usize) -> (PathBuf, Destination, Plan) {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join(format!("recovery-verified-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&parent).unwrap();
        let destination = Destination::new(&parent).unwrap();
        let plan = Plan {
            options: Default::default(),
            layout: crate::recovery_export::Layout::Tree,
            folders: vec![crate::recovery_export::Folder {
                parent: None,
                original_name: "root".into(),
                name: String::new(),
                unknown: false,
                source: None,
            }],
            entries: vec![Entry {
                id: 1,
                folder: 0,
                original_name: "说明.txt".into(),
                name: "说明.txt".into(),
                partial: "one.partial".into(),
                size: size as u64,
            }],
            renamed: 0,
            unknown_files: 0,
            total_bytes: size as u64,
            directory_selection: false,
            skipped: vec![],
            backup_source: None,
            image_source: None,
        };
        (parent, destination, plan)
    }
    fn cleanup(parent: PathBuf, output: Destination, plan: &Plan) {
        // Remove only exact ordinary files created by this fixture; no recursive
        // deletion or hardware enumeration, and never a caller-provided path.
        for entry in &plan.entries {
            for name in [&entry.name, &entry.partial] {
                if output.root.exists(name) {
                    output.root.remove_file(name).unwrap();
                }
            }
        }
        let display = output.display.clone();
        drop(output);
        for name in [REPORT, REPORT_PARTIAL] {
            let p = display.join(name);
            if p.exists() {
                std::fs::remove_file(p).unwrap();
            }
        }
        std::fs::remove_dir(display).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }
    #[test]
    fn verified_output_handles_empty_and_multichunk_files_and_records_digest() {
        for size in [0, 3 * 1024 * 1024 + 17] {
            let bytes: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
            let (parent, mut destination, plan) = fixture(size);
            let outcomes = crate::recovery_export::execute(
                &plan,
                &mut destination,
                |_, out| {
                    out.write_all(&bytes).map_err(err)?;
                    Ok(bytes.len() as u64)
                },
                |_| Ok(()),
            )
            .unwrap();
            let expected = format!("{:x}", Sha256::digest(&bytes));
            assert_eq!(outcomes[0].sha256.as_deref(), Some(expected.as_str()));
            assert_eq!(destination.root.read("说明.txt").unwrap(), bytes);
            assert!(!destination.root.exists("one.partial"));
            let report: serde_json::Value =
                serde_json::from_slice(&destination.root.read(REPORT).unwrap()).unwrap();
            assert_eq!(report["outcomes"][0]["sha256"], expected);
            assert_eq!(report["savedFiles"], 1);
            assert_eq!(report["completed"], true);
            cleanup(parent, destination, &plan);
        }
    }
    #[test]
    fn failed_readback_length_cancel_and_publication_keep_partial_without_success() {
        let bytes = vec![0x2a; 2 * 1024 * 1024 + 1];
        for fault in ["changed", "truncated", "cancel", "exists"] {
            let (parent, mut destination, plan) = fixture(bytes.len());
            let entry = &plan.entries[0];
            let mut output = destination.create(&plan, entry).unwrap();
            output.write_all(&bytes).unwrap();
            match fault {
                "changed" => {
                    output.file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
                    output.file.write_all(b"different").unwrap();
                }
                "truncated" => output.file.set_len(2).unwrap(),
                "exists" => destination
                    .root
                    .write(&entry.name, b"keep existing")
                    .unwrap(),
                _ => (),
            }
            let mut ticks = 0;
            let error = destination
                .commit(entry, output, &mut || {
                    ticks += 1;
                    if fault == "cancel" && ticks == 3 {
                        Err("cancel verification".into())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            if fault == "changed" {
                assert!(error.contains("SHA-256"));
            }
            if fault == "cancel" {
                assert!(error.contains("cancel verification"));
            }
            assert!(destination.root.exists(&entry.partial));
            if fault == "exists" {
                assert_eq!(
                    destination.root.read(&entry.name).unwrap(),
                    b"keep existing"
                );
            } else {
                assert!(!destination.root.exists(&entry.name));
            }
            cleanup(parent, destination, &plan);
        }
    }
    #[test]
    fn parallel_publication_keeps_metadata_warnings_separate_from_verified_contents() {
        use crate::{
            export_metadata::Options,
            file_metadata::{Metadata, Stamp},
        };
        use std::os::windows::fs::MetadataExt;
        for enabled in [true, false] {
            let (parent, mut destination, mut plan) = fixture(8);
            plan.options = Options {
                timestamps: enabled,
                attributes: enabled,
                workers: 4,
            };
            plan.entries = (0..4)
                .map(|i| Entry {
                    id: i,
                    name: format!("file-{i}.bin"),
                    original_name: format!("file-{i}.bin"),
                    partial: format!("file-{i}.partial"),
                    size: 8,
                    folder: 0,
                })
                .collect();
            plan.total_bytes = 32;
            let stamp = Stamp {
                text: "synthetic".into(),
                zone: "UTC".into(),
                precision: "100 ns".into(),
                instant_ns: Some(1_700_000_000_000_000_000),
            };
            let outcomes = crate::recovery_export::execute_parallel(
                &plan,
                &mut destination,
                |entry, out, tick| {
                    out.write_all(&[entry.id as u8; 8]).map_err(err)?;
                    // The final receipt deliberately has no known timestamps or attributes.
                    out.extracted_metadata(if entry.id == 3 {
                        Metadata::default()
                    } else {
                        Metadata {
                            created: Some(stamp.clone()),
                            modified: Some(stamp.clone()),
                            accessed: Some(stamp.clone()),
                            windows_flags: Some(0x22),
                            ..Default::default()
                        }
                    });
                    tick(8)?;
                    Ok(8)
                },
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(outcomes.len(), 4);
            for outcome in &outcomes {
                assert!(outcome.saved && outcome.error.is_none());
                let report = outcome.metadata.as_ref().unwrap();
                assert_eq!(
                    report.warnings(),
                    if enabled && outcome.id == 3 { 4 } else { 0 }
                );
                if !enabled {
                    assert!(report.fields.iter().all(|f| f.status == "disabled"));
                }
                let entry = &plan.entries[outcome.id as usize];
                let after = std::fs::metadata(destination.display.join(&entry.name)).unwrap();
                if enabled && outcome.id != 3 {
                    assert_eq!(after.last_write_time(), 133444736000000000);
                    assert_eq!(after.file_attributes() & 0x22, 0x22);
                }
                assert_eq!(
                    destination.root.read(&entry.name).unwrap(),
                    [outcome.id as u8; 8]
                );
                assert!(!destination.root.exists(&entry.partial));
            }
            let report: serde_json::Value =
                serde_json::from_slice(&destination.root.read(REPORT).unwrap()).unwrap();
            assert_eq!(report["version"], 3);
            assert_eq!(report["options"]["workers"], 4);
            assert_eq!(report["savedFiles"], 4);
            assert_eq!(report["failedFiles"], 0);
            cleanup(parent, destination, &plan);
        }
    }
    #[test]
    fn native_empty_directory_export_creates_observed_hierarchy_without_extracting() {
        use crate::recovery_tree::{Catalog, Directory, ROOT};
        let mut catalog = Catalog::default();
        catalog
            .insert(Directory {
                id: "empty".into(),
                parent: Some(ROOT.into()),
                name: "空目录".into(),
                deleted: false,
                metadata: Default::default(),
            })
            .unwrap();
        let (_, plan) = crate::recovery_export::plan_directory(&[], &catalog, "empty").unwrap();
        let (parent, mut destination, _) = fixture(0);
        let outcomes =
            crate::recovery_export::execute(&plan, &mut destination, |_, _| panic!(), |_| Ok(()))
                .unwrap();
        assert!(outcomes.is_empty());
        let empty = destination.root.open_dir_nofollow("空目录").unwrap();
        assert_eq!(empty.entries().unwrap().count(), 0);
        drop(empty);
        let report: serde_json::Value =
            serde_json::from_slice(&destination.root.read(REPORT).unwrap()).unwrap();
        assert_eq!(report["createdDirectories"], 1);
        destination.root.remove_dir("空目录").unwrap();
        cleanup(parent, destination, &plan);
    }
    #[test]
    fn native_nested_directory_times_survive_child_writes_and_handle_close() {
        use crate::{
            file_metadata::{Metadata, Stamp},
            recovery_export::Folder,
            recovery_tree::Directory,
        };
        use std::os::windows::fs::MetadataExt;
        let (parent, mut destination, mut plan) = fixture(8);
        for (index, parent_index) in [(1, 0), (2, 1)] {
            plan.folders.push(Folder {
                parent: Some(parent_index),
                name: format!("d{index}"),
                original_name: format!("d{index}"),
                unknown: false,
                source: Some(Directory {
                    id: format!("d{index}"),
                    name: format!("d{index}"),
                    ..Default::default()
                }),
            });
        }
        plan.entries[0].folder = 2;
        let stamp = Stamp {
            text: "synthetic".into(),
            zone: "UTC".into(),
            precision: "100 ns".into(),
            instant_ns: Some(1_700_000_000_000_000_000),
        };
        let metadata = Metadata {
            created: Some(stamp.clone()),
            modified: Some(stamp.clone()),
            accessed: Some(stamp),
            windows_flags: Some(0x22),
            ..Default::default()
        };
        let batch = crate::recovery_export::execute_parallel_with_directories(
            &plan,
            &mut destination,
            |_, out, _| {
                out.write_all(b"contents").map_err(err)?;
                Ok(8)
            },
            |_| Ok(metadata.clone()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(batch.files[0].saved);
        for outcome in &batch.directories {
            let report = outcome.metadata.as_ref().unwrap();
            assert_eq!(report.warnings(), 0, "{report:?}");
        }
        // The final child writer and each attribute handle have closed. Observe
        // through independent native metadata, not the production report.
        for name in ["d1", "d1/d2"] {
            let actual = std::fs::metadata(destination.display.join(name)).unwrap();
            assert!(actual.is_dir());
            assert_eq!(actual.creation_time(), 133444736000000000);
            assert_eq!(actual.last_write_time(), 133444736000000000);
            assert_eq!(actual.last_access_time(), 133444736000000000);
            assert_eq!(actual.file_attributes() & 0x22, 0x22);
        }
        assert_eq!(
            destination.root.read("d1/d2/说明.txt").unwrap(),
            b"contents"
        );
        destination.root.remove_file("d1/d2/说明.txt").unwrap();
        destination.root.remove_dir("d1/d2").unwrap();
        destination.root.remove_dir("d1").unwrap();
        plan.entries.clear();
        cleanup(parent, destination, &plan);
    }
    #[test]
    fn native_publish_uses_retained_directory_handle_and_never_replaces_existing_output() {
        // Synthetic ordinary files only. Never queries or opens a disk/volume.
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target");
        let path = parent.join(format!("output-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let root = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
        root.create_dir("from").unwrap();
        root.create_dir("to").unwrap();
        let from = root.open_dir("from").unwrap();
        let to = root.open_dir("to").unwrap();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
            .share_mode(0);
        let mut first = from
            .open_with("first.partial", &options)
            .unwrap()
            .into_std();
        first.write_all(b"synthetic archive").unwrap();
        first.sync_all().unwrap();
        let name = "备份-🗂.dspt";
        finish(&first, &to, name).unwrap();
        // Writes still address the exact original handle after publication.
        first.write_all(b"!").unwrap();
        first.sync_all().unwrap();
        drop(first);
        assert!(!from.exists("first.partial"));
        assert!(!from.exists(name));
        assert_eq!(to.read(name).unwrap(), b"synthetic archive!");
        let mut second = create(&from, "second.partial").unwrap();
        second.write_all(b"must not replace").unwrap();
        second.sync_all().unwrap();
        assert!(finish(&second, &to, name).is_err());
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "C:\\escape",
            "x:y",
            "x\0y",
            "x.",
            "x ",
            "CON.txt",
            "COM¹.bin",
        ] {
            assert!(finish(&second, &to, bad).is_err(), "{bad:?}");
        }
        assert!(finish(&second, &to, &"x".repeat(256)).is_err());
        drop(second);
        assert_eq!(to.read(name).unwrap(), b"synthetic archive!");
        assert_eq!(from.read("second.partial").unwrap(), b"must not replace");
        from.remove_file("second.partial").unwrap();
        to.remove_file(name).unwrap();
        drop(from);
        drop(to);
        root.remove_dir("from").unwrap();
        root.remove_dir("to").unwrap();
        drop(root);
        std::fs::remove_dir(path).unwrap();
    }
}
pub struct Output {
    file: File,
    dir: Dir,
    written: u64,
    hash: Sha256,
    metadata: Option<crate::file_metadata::Metadata>,
    options: crate::export_metadata::Options,
}
impl Output {
    pub fn extracted_metadata(&mut self, metadata: crate::file_metadata::Metadata) {
        self.metadata = Some(metadata);
    }
}
impl Write for Output {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(b)?;
        self.written += n as u64;
        self.hash.update(&b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
pub struct Destination {
    // Keep the selected parent alive as well as the newly created root.
    _parent: Dir,
    root: Dir,
    created: HashMap<usize, Identity>,
    report: File,
    metadata_reports: HashMap<u32, crate::export_metadata::Report>,
    pub display: PathBuf,
}
impl Destination {
    #[cfg(test)]
    pub fn new(validated_parent: &Path) -> Result<Self, String> {
        Self::new_checked(validated_parent, |_| Ok(()))
    }
    pub fn new_checked(
        validated_parent: &Path,
        validate: impl FnOnce(&Dir) -> Result<(), String>,
    ) -> Result<Self, String> {
        let parent =
            Dir::open_ambient_dir(validated_parent, cap_std::ambient_authority()).map_err(err)?;
        identity(&parent)?;
        // Validate the same retained capability used for all subsequent writes,
        // before creating any directory or partial report.
        validate(&parent)?;
        let name = format!("DiskStudio-recovered-{}", uuid::Uuid::new_v4());
        parent.create_dir(&name).map_err(err)?;
        let root = parent.open_dir_nofollow(&name).map_err(err)?;
        let root_id = identity(&root)?;
        let report = create(&root, REPORT_PARTIAL)?;
        Ok(Self {
            _parent: parent,
            root,
            created: HashMap::from([(0, root_id)]),
            report,
            metadata_reports: HashMap::new(),
            display: validated_parent.join(name),
        })
    }
    fn folder(&mut self, plan: &Plan, index: usize) -> Result<Dir, String> {
        let mut dir = self.root.try_clone().map_err(err)?;
        for at in plan.chain(index)? {
            let name = &plan.folders[at].name;
            if !self.created.contains_key(&at) {
                dir.create_dir(name).map_err(err)?;
            }
            let next = dir.open_dir_nofollow(name).map_err(err)?;
            let actual = identity(&next)?;
            if actual.volume != self.created[&0].volume {
                return Err("输出目录跨越到其他卷，已拒绝。".into());
            }
            if let Some(expected) = self.created.get(&at) {
                if *expected != actual {
                    return Err("输出目录身份已改变，已拒绝。".into());
                }
            } else {
                self.created.insert(at, actual);
            }
            dir = next;
        }
        Ok(dir)
    }
}
impl Sink for Destination {
    type Output = Output;
    fn directory(&mut self, plan: &Plan, index: usize) -> Result<(), String> {
        self.folder(plan, index).map(|_| ())
    }
    fn create(&mut self, plan: &Plan, entry: &Entry) -> Result<Output, String> {
        let dir = self.folder(plan, entry.folder)?;
        let file = create(&dir, &entry.partial)?;
        Ok(Output {
            file,
            dir,
            written: 0,
            hash: Sha256::new(),
            metadata: None,
            options: plan.options,
        })
    }
    fn commit(
        &mut self,
        entry: &Entry,
        mut output: Output,
        tick: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<String, String> {
        tick()?;
        output.file.sync_all().map_err(err)?;
        if output.written != entry.size || output.file.metadata().map_err(err)?.len() != entry.size
        {
            return Err("输出文件长度与提取计划不符，已保留 partial 文件。".into());
        }
        let expected = output.hash.finalize();
        output.file.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut readback = Sha256::new();
        let mut remaining = entry.size;
        let mut buffer = vec![0; 1024 * 1024];
        while remaining > 0 {
            tick()?;
            let n = remaining.min(buffer.len() as u64) as usize;
            output.file.read_exact(&mut buffer[..n]).map_err(err)?;
            readback.update(&buffer[..n]);
            remaining -= n as u64;
        }
        if readback.finalize() != expected {
            return Err(
                "输出文件 SHA-256 回读校验失败，未发布为成功文件；已保留 partial 文件。".into(),
            );
        }
        tick()?;
        // The same retained read/write handle is verified and renamed. Share mode
        // denies other writers/deletion throughout; no path-based reopen race.
        finish(&output.file, &output.dir, &entry.name)?;
        if let Some(metadata) = &output.metadata {
            // Data verification and publication precede read-only attributes.
            // Metadata failures remain separate from successfully saved bytes.
            self.metadata_reports.insert(
                entry.id,
                crate::export_metadata::apply(&output.file, metadata, output.options),
            );
        }
        Ok(format!("{expected:x}"))
    }
    fn metadata_report(&self, id: u32) -> Option<crate::export_metadata::Report> {
        self.metadata_reports.get(&id).cloned()
    }
    fn preserve_directory(
        &mut self,
        plan: &Plan,
        index: usize,
        metadata: &crate::file_metadata::Metadata,
    ) -> crate::export_metadata::Report {
        let result = (|| -> Result<crate::export_metadata::Report, String> {
            if index == 0 {
                return Err("不会修改导出根目录的属性。".into());
            }
            let expected = *self.created.get(&index).ok_or("输出目录尚未创建。")?;
            let dir = self.folder(plan, index)?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
            let file = dir.open_with(".", &options).map_err(err)?.into_std();
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(err(std::io::Error::last_os_error()));
            }
            let actual = Identity {
                volume: info.dwVolumeSerialNumber,
                high: info.nFileIndexHigh,
                low: info.nFileIndexLow,
            };
            if actual != expected
                || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err("输出目录身份或类型已改变，未写入目录属性。".into());
            }
            Ok(crate::export_metadata::apply_directory(
                &file,
                metadata,
                plan.options,
            ))
        })();
        result.unwrap_or_else(|error| {
            crate::export_metadata::unmodified(metadata, plan.options, Some(error))
        })
    }
    fn report(&mut self, bytes: &[u8], completed: bool) -> Result<(), String> {
        self.report.seek(SeekFrom::Start(0)).map_err(err)?;
        self.report.set_len(0).map_err(err)?;
        self.report.write_all(bytes).map_err(err)?;
        self.report.sync_all().map_err(err)?;
        if completed {
            finish(&self.report, &self.root, REPORT)?;
        }
        Ok(())
    }
}

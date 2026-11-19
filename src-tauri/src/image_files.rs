//! Native ordinary-file capabilities for container readers. No device opening,
//! paths from IPC, automatic companion lookup, mounts, or writable source handles.
use serde::Serialize;
pub const CHUNK: usize = 1024 * 1024;
const MAX_FILES: usize = 64;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub last_modified: u64,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    pub files: Vec<FileInfo>,
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::{data_windows::FileBinding, partitions::Target};
    use std::{
        collections::{HashMap, HashSet},
        fs::{File, OpenOptions},
        io::{Read, Seek, SeekFrom},
        os::windows::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        },
        path::PathBuf,
        sync::{Arc, Mutex, OnceLock, Weak},
        time::UNIX_EPOCH,
    };
    use windows_sys::Win32::Storage::FileSystem::*;
    struct Entry {
        info: FileInfo,
        file: File,
        binding: FileBinding,
        disk: Target,
    }
    pub struct Group {
        pub id: String,
        entries: Mutex<Vec<Entry>>,
    }
    #[derive(Default)]
    struct Registry {
        staged: Option<Arc<Group>>,
        groups: HashMap<String, Weak<Group>>,
    }
    fn registry() -> std::sync::MutexGuard<'static, Registry> {
        static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
        REGISTRY
            .get_or_init(|| Mutex::new(Registry::default()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    fn opened(path: PathBuf) -> Result<Entry, String> {
        let leaf = path.file_name().ok_or("镜像文件名无效")?;
        let name = leaf
            .to_str()
            .ok_or("镜像文件名不是有效 Unicode")?
            .to_owned();
        if name.is_empty()
            || name.encode_utf16().count() > 255
            || name.contains([':', '/', '\\', '\0'])
        {
            return Err("请选择普通文件，不接受设备或数据流路径。".into());
        }
        let (parent, disk) =
            crate::data_windows::folder_location(path.parent().ok_or("镜像目录无效")?)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(parent.join(leaf))
            .map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file()
            || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
            || meta.len() == 0
            || meta.len() > 1 << 44
        {
            return Err("镜像来源必须是非链接的普通文件，容量不超过 16 TiB。".into());
        }
        let last_modified = meta
            .modified()
            .map_err(|e| e.to_string())?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "镜像文件修改时间无效")?
            .as_millis()
            .try_into()
            .map_err(|_| "镜像文件修改时间越界")?;
        let binding = FileBinding::new(&file, &disk)?;
        Ok(Entry {
            info: FileInfo {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                size: meta.len(),
                last_modified,
            },
            file,
            binding,
            disk,
        })
    }
    impl Group {
        pub fn raw_readers(&self) -> Result<Vec<crate::data_io::Region<File>>, String> {
            self.entries
                .lock()
                .map_err(|_| "镜像文件状态异常".to_string())?
                .iter()
                .map(|entry| {
                    let file = entry.file.try_clone().map_err(|e| e.to_string())?;
                    crate::data_io::Region::new(file, 0, entry.info.size)
                        .map_err(|e| e.to_string())
                })
                .collect()
        }

        pub fn info(&self) -> Info {
            Info {
                id: self.id.clone(),
                files: self
                    .entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .map(|f| f.info.clone())
                    .collect(),
            }
        }
        pub fn validate(&self) -> Result<(), String> {
            let bindings: Vec<_> = self
                .entries
                .lock()
                .map_err(|_| "镜像文件状态异常")?
                .iter()
                .map(|e| e.binding.clone())
                .collect();
            for chunk in bindings.chunks(16) {
                FileBinding::validate_all(chunk)?;
            }
            Ok(())
        }
        pub fn disks(&self) -> Vec<Target> {
            let mut disks = vec![];
            for e in self
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
            {
                if !disks.contains(&e.disk) {
                    disks.push(e.disk.clone());
                }
            }
            disks
        }
        fn append(&self, added: Vec<Entry>) -> Result<(), String> {
            let mut entries = self.entries.lock().map_err(|_| "镜像文件状态异常")?;
            if entries.len() + added.len() > MAX_FILES {
                return Err("镜像主文件、父盘和分卷合计最多 64 个。".into());
            }
            let mut names: HashSet<String> =
                entries.iter().map(|e| e.info.name.to_lowercase()).collect();
            for e in &added {
                if !names.insert(e.info.name.to_lowercase()) {
                    return Err("所选文件存在同名项；请只选择缺少的父盘或分卷。".into());
                }
            }
            entries.extend(added);
            Ok(())
        }
    }
    // Called only with the application's operation lock held and data state idle.
    pub fn choose() -> Result<Option<Info>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("打开镜像以浏览文件"))
            .add_filter(
                crate::native_locale::text("虚拟磁盘镜像"),
                &[
                    "img", "raw", "vhd", "avhd", "vhdx", "vdi", "vmdk", "hdd", "hds",
                ],
            )
            .pick_file()
        else {
            return Ok(None);
        };
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ![
            "img", "raw", "vhd", "avhd", "vhdx", "vdi", "vmdk", "hdd", "hds",
        ]
        .contains(&extension.as_str())
        {
            return Err("请选择受支持的镜像文件。".into());
        }
        let group = Arc::new(Group {
            id: uuid::Uuid::new_v4().to_string(),
            entries: Mutex::new(vec![opened(path)?]),
        });
        let info = group.info();
        let mut registry = registry();
        registry.groups.retain(|_, g| g.strong_count() > 0);
        registry
            .groups
            .insert(group.id.clone(), Arc::downgrade(&group));
        registry.staged = Some(group);
        Ok(Some(info))
    }

    pub fn choose_lvm_members() -> Result<Option<Arc<Group>>, String> {
        let Some(paths) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("选择 LVM2 物理卷的 RAW/IMG 文件"))
            .add_filter(crate::native_locale::text("RAW/IMG 物理卷镜像"), &["img", "raw"])
            .pick_files()
        else {
            return Ok(None);
        };
        if !(1..=disk_core::lvm::MAX_MEMBERS).contains(&paths.len()) {
            return Err("请选择 1–64 个 LVM2 物理卷来源。".into());
        }
        let entries = paths
            .into_iter()
            .map(|path| {
                if !path.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("raw") || ext.eq_ignore_ascii_case("img")
                }) {
                    return Err("请选择 RAW/IMG 物理卷镜像。".into());
                }
                opened(path)
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(Arc::new(Group {
            id: uuid::Uuid::new_v4().to_string(),
            entries: Mutex::new(entries),
        })))
    }
    pub fn add(id: &str) -> Result<Info, String> {
        let group = registry()
            .staged
            .as_ref()
            .filter(|g| g.id == id)
            .cloned()
            .ok_or("镜像选择会话已失效")?;
        let Some(paths) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(
                "选择缺少的父盘或分卷（不会自动查找文件）",
            ))
            .pick_files()
        else {
            return Ok(group.info());
        };
        if paths.is_empty() || paths.len() + group.info().files.len() > MAX_FILES {
            return Err("镜像文件合计最多 64 个。".into());
        }
        let added = paths
            .into_iter()
            .map(opened)
            .collect::<Result<Vec<_>, _>>()?;
        group.append(added)?;
        group.validate()?;
        Ok(group.info())
    }
    pub fn take(id: &str) -> Result<Arc<Group>, String> {
        let mut r = registry();
        if r.staged.as_ref().is_none_or(|g| g.id != id) {
            return Err("镜像选择会话已失效".into());
        }
        Ok(r.staged.take().unwrap())
    }
    pub fn discard(id: &str) -> Result<(), String> {
        let _ = take(id)?;
        Ok(())
    }
    pub fn read(id: &str, file_id: &str, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        if length == 0 || length > CHUNK {
            return Err("镜像文件读取长度须为 1 B–1 MiB。".into());
        }
        let group = registry()
            .groups
            .get(id)
            .and_then(Weak::upgrade)
            .ok_or("镜像文件句柄已关闭")?;
        let mut entries = group.entries.lock().map_err(|_| "镜像文件读取状态异常")?;
        let e = entries
            .iter_mut()
            .find(|e| e.info.id == file_id)
            .ok_or("镜像文件标识已失效")?;
        if offset
            .checked_add(length as u64)
            .is_none_or(|n| n > e.info.size)
        {
            return Err("镜像文件读取越界".into());
        }
        e.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0; length];
        e.file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        Ok(bytes)
    }
}
#[cfg(windows)]
pub use platform::{add, choose, choose_lvm_members, discard, read, take, Group};
#[cfg(not(windows))]
pub fn read(_: &str, _: &str, _: u64, _: usize) -> Result<Vec<u8>, String> {
    Err("当前仅支持 Windows 原生文件选择。".into())
}

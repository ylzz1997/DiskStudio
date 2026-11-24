//! Add verified WinPE files to an existing FAT32 USB volume.
use super::{adk::Adk, attach_app, open_source, optional_subdir, plain, running_app, scan_media, Input};
use crate::{
    data_io,
    data_tasks::{complete_verified, progress, Selection},
    data_windows::{self as device, FileBinding, Resolved},
    maintenance_media::{self as media, Preview, UsbFile, UsbPreview},
    native_locale::{self, Locale},
    recovery_output::{self, Identity},
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Seek, SeekFrom},
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetVolumeInformationByHandleW,
};

pub(in crate::data_tasks) const TITLE: &str = "制作 WinPE 启动 U 盘";
pub(in crate::data_tasks) const ADK_TITLE: &str = "准备 WinPE 启动分区";
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

struct Target {
    resolved: Resolved,
    root: Dir,
    binding: FileBinding,
    root_path: PathBuf,
    cluster: u64,
}

impl Target {
    fn open(selection: &Selection, usb_only: bool) -> Result<Self, String> {
        let resolved = device::winpe_media_target(selection, usb_only)?;
        let volume = &resolved.volumes[0];
        let root_path = PathBuf::from(format!("{volume}\\"));
        let root =
            Dir::open_ambient_dir(&root_path, cap_std::ambient_authority()).map_err(device::err)?;
        recovery_output::identity(&root)?;
        let directory = root.try_clone().map_err(device::err)?.into_std_file();
        let location = device::file_location(&directory)?;
        location.confirm_partition(
            &selection.target,
            &selection.layout_key,
            volume,
            (resolved.disk.number, resolved.offset, resolved.size),
        )?;
        let mut name = [0u16; 32];
        let mut flags = 0;
        // SAFETY: the retained root handle and output buffers belong to this call.
        let reported = unsafe {
            GetVolumeInformationByHandleW(
                directory.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        let name_len = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        if reported == 0
            || !String::from_utf16_lossy(&name[..name_len]).eq_ignore_ascii_case("FAT32")
            || flags & FILE_READ_ONLY_VOLUME != 0
        {
            return Err("原生卷没有确认 FAT32 文件系统或可写状态。".into());
        }
        let wide = wide_root(volume);
        let mut sectors = 0u32;
        let mut bytes = 0u32;
        // SAFETY: the path is a null-terminated provider-derived volume root.
        if unsafe {
            GetDiskFreeSpaceW(
                wide.as_ptr(),
                &mut sectors,
                &mut bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
            || Some(u64::from(bytes)) != resolved.disk.logical_sector_size
        {
            return Err("USB 卷的原生扇区规格与磁盘信息不同。".into());
        }
        let cluster = u64::from(sectors) * u64::from(bytes);
        if !cluster.is_power_of_two() || cluster < u64::from(bytes) || cluster > 64 * 1024 {
            return Err("USB 卷的 FAT32 簇大小无法核对。".into());
        }
        let binding = FileBinding::new(&directory, &selection.target)?;
        Ok(Self {
            resolved,
            root,
            binding,
            root_path,
            cluster,
        })
    }

    fn space(&self, plan: &Layout, files: &BTreeMap<String, Input>) -> Result<(), String> {
        let clusters = plan
            .files
            .iter()
            .filter(|(_, sha)| sha.is_none())
            .map(|(path, _)| u64::from(files[path].member.size).div_ceil(self.cluster) + 2)
            .sum::<u64>()
            + 2 * plan
                .directories
                .values()
                .filter(|identity| identity.is_none())
                .count() as u64;
        let mut available = 0;
        let wide = wide_root(&self.resolved.volumes[0]);
        // SAFETY: the path is a null-terminated provider-derived volume root.
        if unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
            || available < clusters * self.cluster
        {
            return Err("USB 卷可用空间不足。".into());
        }
        Ok(())
    }
}

fn wide_root(volume: &str) -> Vec<u16> {
    volume.encode_utf16().chain([u16::from(b'\\'), 0]).collect()
}

#[derive(PartialEq, Eq)]
struct Layout {
    directories: BTreeMap<String, Option<Identity>>,
    files: BTreeMap<String, Option<String>>,
}

struct Contents {
    dirs: BTreeMap<String, Option<Dir>>,
    reused: BTreeMap<String, File>,
    layout: Layout,
}

fn parts(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

type Verify<'a> = dyn Fn(&File) -> Result<(), String> + 'a;
type Tick<'a> = dyn FnMut(&str, u64, u64) -> Result<(), String> + 'a;

fn verify_dir(directory: &Dir, verify: &Verify<'_>) -> Result<Identity, String> {
    let identity = recovery_output::identity(directory)?;
    verify(&directory.try_clone().map_err(device::err)?.into_std_file())?;
    Ok(identity)
}

fn digest(
    file: &mut File,
    size: u64,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    if file.metadata().map_err(device::err)?.len() != size {
        return Err("WinPE 文件的容量与预览不同。".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(device::err)?;
    data_io::hash_exact(file, size, tick)
}

impl Contents {
    fn directory(&mut self, path: &str, verify: &Verify<'_>) -> Result<(), String> {
        if self.dirs.contains_key(path) {
            return Ok(());
        }
        let (parent, name) = parts(path);
        self.directory(parent, verify)?;
        let folder = match &self.dirs[parent] {
            Some(directory) => {
                optional_subdir(directory, name).map_err(|error| format!("{path}。{error}"))?
            }
            None => None,
        };
        let identity = folder
            .as_ref()
            .map(|directory| verify_dir(directory, verify))
            .transpose()?;
        self.layout.directories.insert(path.into(), identity);
        self.dirs.insert(path.into(), folder);
        Ok(())
    }

    fn open(
        root: &Dir,
        source: &mut BTreeMap<String, Input>,
        verify: &Verify<'_>,
        tick: &mut Tick<'_>,
    ) -> Result<Self, String> {
        let mut result = Self {
            dirs: BTreeMap::from([(String::new(), Some(root.try_clone().map_err(device::err)?))]),
            reused: BTreeMap::new(),
            layout: Layout {
                directories: BTreeMap::from([(String::new(), Some(verify_dir(root, verify)?))]),
                files: BTreeMap::new(),
            },
        };
        let total = source.values().map(|f| u64::from(f.member.size)).sum();
        let mut done = 0;
        for (key, input) in source {
            if input
                .member
                .path
                .split('/')
                .any(crate::recovery_export::reserved)
            {
                return Err("WinPE 来源含有保留的 Windows 设备文件名。".into());
            }
            let (parent, name) = parts(&input.member.path);
            result.directory(parent, verify)?;
            let size = u64::from(input.member.size);
            let mut reused = None;
            if let Some(directory) = &result.dirs[parent] {
                match directory.symlink_metadata(name) {
                    Ok(_) => {
                        let mut target = plain(directory, Path::new(name))
                            .map_err(|err| format!("{}。{err}", input.member.path))?;
                        verify(&target)?;
                        if target.metadata().map_err(device::err)?.len() != size {
                            return Err(format!(
                                "USB 上已有不同的 WinPE 文件：{}",
                                input.member.path
                            ));
                        }
                        let phase = |n| tick("核对 USB 上已有的 WinPE 文件", done + n, total);
                        let target_sha = digest(&mut target, size, phase)?;
                        let phase = |n| tick("核对 USB 上已有的 WinPE 文件", done + n, total);
                        if digest(&mut input.file, size, phase)? != target_sha {
                            return Err(format!(
                                "USB 上已有不同的 WinPE 文件：{}",
                                input.member.path
                            ));
                        }
                        result.reused.insert(key.clone(), target);
                        reused = Some(target_sha);
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.to_string()),
                }
            }
            result.layout.files.insert(key.clone(), reused);
            done += size;
        }
        tick("核对 USB 上已有的 WinPE 文件", total, total)?;
        Ok(result)
    }

    fn create_directories(&mut self, verify: &Verify<'_>) -> Result<(), String> {
        // Every parent is sorted before its descendants. Creating an existing
        // name is an error, including a concurrent directory that was not in the preview.
        let missing: Vec<_> = self
            .dirs
            .iter()
            .filter(|(_, dir)| dir.is_none())
            .map(|(path, _)| path.clone())
            .collect();
        for path in missing {
            let (parent, name) = parts(&path);
            let parent = self.dir(parent)?;
            parent.create_dir(name).map_err(device::err)?;
            let dir = parent.open_dir_nofollow(name).map_err(device::err)?;
            verify_dir(&dir, verify)?;
            self.dirs.insert(path, Some(dir));
        }
        Ok(())
    }

    fn dir(&self, path: &str) -> Result<&Dir, String> {
        self.dirs
            .get(path)
            .and_then(Option::as_ref)
            .ok_or_else(|| "缺少已核对的 USB 目标目录。".into())
    }

    fn stage(
        &mut self,
        source: &mut BTreeMap<String, Input>,
        verify: &Verify<'_>,
        tick: &mut Tick<'_>,
    ) -> Result<BTreeMap<String, File>, String> {
        let mut staged = BTreeMap::new();
        let total = self
            .layout
            .files
            .iter()
            .filter(|(_, sha)| sha.is_none())
            .map(|(key, _)| u64::from(source[key].member.size))
            .sum();
        tick("复制 WinPE 启动文件", 0, total)?;
        self.create_directories(verify)?;
        let mut done = 0;
        for (key, input) in source {
            if self.layout.files[key].is_some() {
                continue;
            }
            tick("复制 WinPE 启动文件", done, total)?;
            let dir = self.dir(parts(&input.member.path).0)?;
            let temporary = format!("diskstudio-{}.partial", uuid::Uuid::new_v4().simple());
            let mut output = recovery_output::create(dir, &temporary)?;
            verify(&output)?;
            let size = u64::from(input.member.size);
            if input.file.metadata().map_err(device::err)?.len() != size {
                return Err("WinPE 来源文件的容量与预览不同。".into());
            }
            input.file.seek(SeekFrom::Start(0)).map_err(device::err)?;
            let sha = data_io::copy_exact(&mut input.file, &mut output, size, |n| {
                tick("复制 WinPE 启动文件", done + n, total)
            })?;
            output.sync_all().map_err(device::err)?;
            if digest(&mut output, size, |n| {
                tick("回读 USB 上的 WinPE 文件", done + n, total)
            })? != sha
            {
                return Err("WinPE 启动文件的 SHA-256 写后回读不一致。".into());
            }
            self.layout.files.insert(key.clone(), Some(sha));
            staged.insert(key.clone(), output);
            done += size;
        }
        Ok(staged)
    }

    fn verify_published(
        &self,
        root: &Dir,
        input: &Input,
        sha: &str,
        verify: &Verify<'_>,
        tick: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let (parent, name) = parts(&input.member.path);
        let dir = self.reopen_parent(root, parent, verify)?;
        let mut file = plain(&dir, Path::new(name))?;
        verify(&file)?;
        if digest(&mut file, u64::from(input.member.size), tick)? != sha {
            return Err("USB 上新发布的 WinPE 启动文件与已校验副本不同。".into());
        }
        Ok(())
    }

    fn reopen_parent(&self, root: &Dir, parent: &str, verify: &Verify<'_>) -> Result<Dir, String> {
        let dir = if parent.is_empty() {
            root.try_clone().map_err(device::err)?
        } else {
            let mut dir = root.try_clone().map_err(device::err)?;
            for component in parent.split('/') {
                dir = dir.open_dir_nofollow(component).map_err(device::err)?;
            }
            dir
        };
        if verify_dir(&dir, verify)? != recovery_output::identity(self.dir(parent)?)? {
            return Err("USB 启动文件所在的目录在制作期间已变化。".into());
        }
        Ok(dir)
    }

    fn digest(&self, source: &BTreeMap<String, Input>) -> Result<String, String> {
        let mut hash = Sha256::new();
        for (key, sha) in &self.layout.files {
            let member = &source[key].member;
            hash.update((member.path.len() as u32).to_le_bytes());
            hash.update(member.path.as_bytes());
            hash.update(member.size.to_le_bytes());
            hash.update(sha.as_ref().ok_or("缺少 WinPE 文件回读摘要。")?.as_bytes());
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

fn publish(
    contents: &Contents,
    staged: &mut BTreeMap<String, File>,
    root: &Dir,
    input: &Input,
    key: &str,
    verify: &Verify<'_>,
) -> Result<bool, String> {
    if let Some(file) = staged.remove(key) {
        let (parent, name) = parts(&input.member.path);
        let destination = contents.reopen_parent(root, parent, verify)?;
        recovery_output::finish(&file, &destination, name)?;
        Ok(true)
    } else {
        let retained = contents
            .reused
            .get(key)
            .ok_or("缺少复用的 USB 启动文件句柄。")?;
        verify(retained)?;
        Ok(false)
    }
}

pub(crate) struct Install {
    sources: Vec<FileBinding>,
    source_label: String,
    source: BTreeMap<String, Input>,
    destination: Selection,
    target: Target,
    expected: Layout,
    preview: Preview,
    adk: Option<Adk>,
}

impl Install {
    pub(in crate::data_tasks) fn prepare(
        selection: Selection,
        include_app: bool,
        locale: &Locale,
    ) -> Result<Option<Self>, String> {
        let target = Target::open(&selection, true)?;
        let Some(source) = rfd::FileDialog::new()
            .set_title(locale.text("选择 WinPE 工作文件夹"))
            .pick_folder()
        else {
            return Ok(None);
        };
        let (root, binding) = open_source(&source)?;
        if binding.target() == &selection.target {
            return Err("WinPE 启动 U 盘必须与来源位于不同物理磁盘。".into());
        }
        let (mut files, mut preview) =
            scan_media(&root, &mut |member| binding.verify_same_volume(member))?;
        let (app, app_binding) = running_app(include_app)?.unzip();
        if let Some(app) = app {
            if app_binding
                .as_ref()
                .is_some_and(|app| app.target() == &selection.target)
            {
                return Err("WinPE 启动 U 盘必须与来源位于不同物理磁盘。".into());
            }
            attach_app(&mut files, &mut preview, app)?;
        }
        Self::plan(
            selection, target, source, files, preview,
            std::iter::once(binding).chain(app_binding).collect(), None,
        ).map(Some)
    }

    pub(in crate::data_tasks) fn prepare_adk(
        selection: Selection,
        locale: &Locale,
    ) -> Result<Option<Self>, String> {
        let target = Target::open(&selection, false)?;
        let Some(source) = rfd::FileDialog::new()
            .set_title(locale.text("选择 Windows ADK 的 amd64 文件夹"))
            .pick_folder()
        else { return Ok(None) };
        let Some(scratch) = rfd::FileDialog::new()
            .set_title(locale.text("选择另一块磁盘上的 NTFS 工作目录"))
            .pick_folder()
        else { return Ok(None) };
        let app = running_app(true)?.ok_or("无法定位 DiskStudio 程序。")?;
        Self::plan_adk(selection, target, source, &scratch, app).map(Some)
    }

    fn plan_adk(
        selection: Selection,
        target: Target,
        source: PathBuf,
        scratch: &Path,
        app: (File, FileBinding),
    ) -> Result<Self, String> {
        let (adk, files, preview) = Adk::prepare(&source, scratch, &selection.target, app)?;
        let bindings = adk.bindings.clone();
        Self::plan(selection, target, source, files, preview, bindings, Some(adk))
    }

    fn plan(
        selection: Selection,
        target: Target,
        source: PathBuf,
        mut files: BTreeMap<String, Input>,
        mut preview: Preview,
        sources: Vec<FileBinding>,
        adk: Option<Adk>,
    ) -> Result<Self, String> {
        let target_layout = Contents::open(
            &target.root,
            &mut files,
            &|f| target.binding.verify_same_volume(f),
            &mut |_, _, _| Ok(()),
        )?
        .layout;
        if preview.adk && target_layout.files.get(media::WIM_PATH).is_some_and(Option::is_some) {
            return Err("目标已有 boot.wim。请选择尚未安装 WinPE 的分区。".into());
        }
        target.space(&target_layout, &files)?;
        let mut summary = UsbPreview {
            new_files: 0,
            reused_files: 0,
            new_bytes: 0,
            files: Vec::with_capacity(files.len()),
        };
        for (key, sha) in &target_layout.files {
            let member = &files[key].member;
            if sha.is_some() {
                summary.reused_files += 1;
            } else {
                summary.new_files += 1;
                summary.new_bytes += u64::from(member.size);
            }
            summary.files.push(UsbFile {
                path: member.path.clone(),
                reused: sha.is_some(),
            });
        }
        preview.usb = Some(summary);
        Ok(Self {
            sources,
            source_label: source.display().to_string(),
            source: files,
            destination: selection,
            target,
            expected: target_layout,
            preview,
            adk,
        })
    }

    pub(in crate::data_tasks) fn source(&self) -> &str {
        &self.source_label
    }
    pub(in crate::data_tasks) fn destination(&self) -> String {
        format!(
            "{} · {}",
            self.target.resolved.label,
            self.target.root_path.display()
        )
    }
    pub(in crate::data_tasks) fn preview(&self) -> &Preview {
        &self.preview
    }
    pub(in crate::data_tasks) fn size(&self) -> u64 {
        self.source.values().map(|f| u64::from(f.member.size)).sum()
    }

    pub(in crate::data_tasks) fn execute(
        mut self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        if let Some(mut adk) = self.adk.take() {
            let stage = adk.build(&mut self.source, &mut self.preview, cancel, |done, total| {
                progress(id, cancel, "准备 WinPE 启动环境", done, total)
            })?;
            let result = self.publish(id, cancel);
            drop(stage);
            result
        } else {
            self.publish(id, cancel)
        }
    }

    fn publish(
        mut self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let locale = native_locale::current();
        let now = Target::open(&self.destination, !self.preview.adk)?;
        if !now.resolved.same_data_binding(&self.target.resolved)
            || now.cluster != self.target.cluster
            || recovery_output::identity(&now.root)?
                != recovery_output::identity(&self.target.root)?
        {
            return Err("USB 目标卷或完整分区布局与确认时不同。".into());
        }
        let target_binding = &now.binding;
        let mut bindings = std::mem::take(&mut self.sources);
        bindings.push(target_binding.clone());
        FileBinding::validate_all(&bindings)?;
        let verify = |file: &File| target_binding.verify_same_volume(file);
        let mut tick = |phase: &str, done, total| {
            if cancel.load(Ordering::Relaxed) {
                return Err("WinPE 启动 U 盘制作已取消。".into());
            }
            progress(id, cancel, phase, done, total)
        };
        let mut contents = Contents::open(&now.root, &mut self.source, &verify, &mut tick)?;
        if contents.layout != self.expected {
            return Err("USB 上的目录或已有文件与预览不同。".into());
        }
        let mut published = 0;
        let result = (|| {
            now.space(&contents.layout, &self.source)?;
            FileBinding::validate_all(&bindings)?;
            let mut staged = contents.stage(&mut self.source, &verify, &mut tick)?;
            let added = staged.len();
            if let Some(expected) = &self.preview.app_sha256 {
                if contents
                    .layout
                    .files
                    .get(media::APP_KEY)
                    .and_then(Option::as_ref)
                    != Some(expected)
                {
                    return Err("DiskStudio 程序与预览中的 SHA-256 不一致。".into());
                }
            }
            FileBinding::validate_all(&bindings)?;
            let summary = contents.digest(&self.source)?;
            let phase = "核对 USB 上最终的 WinPE 启动文件";
            let total = self.size();
            let mut done = 0;
            for (key, input) in &self.source {
                if key == media::FALLBACK {
                    continue;
                }
                tick(phase, done, total)?;
                published += usize::from(publish(
                    &contents,
                    &mut staged,
                    &now.root,
                    input,
                    key,
                    &verify,
                )?);
                let sha = contents.layout.files[key]
                    .as_deref()
                    .ok_or("缺少 WinPE 文件回读摘要。")?;
                contents.verify_published(&now.root, input, sha, &verify, |n| {
                    tick(phase, done + n, total)
                })?;
                done += u64::from(input.member.size);
            }
            tick(phase, done, total)?;
            FileBinding::validate_all(&bindings)?;
            let message = locale.format(
                "WinPE 启动文件已通过 SHA-256 回读，新增 {added} 个，复用 {reused} 个。",
                &[
                    ("added", &added.to_string()),
                    ("reused", &contents.reused.len().to_string()),
                ],
            );
            complete_verified(id, message.clone(), summary.clone(), || {
                // Add the fallback entry only after the paths it needs have been read back.
                let key = media::FALLBACK;
                let input = &self.source[key];
                publish(&contents, &mut staged, &now.root, input, key, &verify)?;
                let sha = contents.layout.files[key]
                    .as_deref()
                    .ok_or("缺少 WinPE 文件回读摘要。")?;
                contents.verify_published(&now.root, input, sha, &verify, |_| Ok(()))
            })?;
            Ok((message, Some(summary)))
        })();
        result.map_err(|err: String| {
            locale.format(
                "{error} 已发布 {count} 个支持文件，可能留有未发布的 .partial 文件。",
                &[
                    ("error", &locale.text(&err)),
                    ("count", &published.to_string()),
                ],
            )
        })
    }
}

#[cfg(test)]
#[path = "maintenance_usb_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) fn install_adk_for_lab(
    selection: Selection,
    source: &Path,
    scratch: &Path,
    app_path: &Path,
) -> Result<(), String> {
    let target = Target::open(&selection, false)?;
    let app = File::open(app_path).map_err(device::err)?;
    let binding = device::file_location(&app)?;
    let binding = FileBinding::new(&app, &binding.target)?;
    let install = Install::plan_adk(selection, target, source.to_owned(), scratch, (app, binding))?;
    assert!(install.preview.adk);
    let (id, cancel) = crate::data_tasks::start(ADK_TITLE)?;
    let (message, digest) = install.execute(&id, &cancel)?;
    println!("{message} [{}]", digest.expect("missing verified file digest"));
    Ok(())
}

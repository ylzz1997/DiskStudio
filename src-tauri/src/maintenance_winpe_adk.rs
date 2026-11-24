//! Build a serviced WinPE WIM in a private ordinary NTFS directory.
//! DISM receives only fixed operations against the private staging paths.
use super::{attach_app, inspect_media, open_source, optional_subdir, plain, Input, Walk};
use crate::{
    data_io,
    data_windows::{self as device, FileBinding},
    maintenance_iso::Member,
    maintenance_media::{self as media, Preview},
    partitions::Target,
    recovery_output::{self, Identity},
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStringExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Storage::FileSystem::GetVolumeInformationByHandleW,
    System::{SystemInformation::GetSystemDirectoryW, Threading::CREATE_NO_WINDOW},
};

const COMPONENTS: [&str; 5] = ["WMI", "NetFx", "Scripting", "PowerShell", "StorageWMI"];
const SHELL: &[u8] = b"[LaunchApps]\r\n%SYSTEMROOT%\\System32\\wpeinit.exe\r\n%SYSTEMDRIVE%\\DiskStudio.exe\r\n%SYSTEMROOT%\\System32\\cmd.exe\r\n";
const WORKING_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
const CANCELLED: &str = "已取消准备 WinPE 启动环境。";

type Files = BTreeMap<String, Input>;

pub(super) struct Adk {
    pub(super) bindings: Vec<FileBinding>,
    packages: Vec<(String, File)>,
    scratch: Scratch,
}

struct Scratch {
    dir: Dir,
    path: PathBuf,
    binding: FileBinding,
    identity: Identity,
}

pub(super) struct Stage<'a> {
    parent: &'a Scratch,
    name: String,
    path: PathBuf,
    identity: Identity,
}

impl Scratch {
    fn open(path: &Path, destination: &Target) -> Result<Self, String> {
        let (canonical, disk) =
            device::destination_location(path, std::slice::from_ref(destination), WORKING_BYTES)?;
        // DISM uses local DOS paths. Reopen that spelling and bind it to the
        // canonical target before using it as an argument.
        let text = canonical.to_str().ok_or("WinPE 工作路径无效。")?;
        let text = text.strip_prefix(r"\\?\").unwrap_or(text);
        let chars = text.as_bytes();
        if chars.len() < 3
            || !chars[0].is_ascii_alphabetic()
            || &chars[1..3] != b":\\"
            || text.encode_utf16().count() > 140
        {
            return Err("WinPE 工作目录需要较短的本地 NTFS 路径。".into());
        }
        let path = PathBuf::from(text);
        let expected =
            Dir::open_ambient_dir(canonical, cap_std::ambient_authority()).map_err(device::err)?;
        let dir =
            Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(device::err)?;
        let identity = recovery_output::identity(&dir)?;
        if recovery_output::identity(&expected)? != identity {
            return Err("WinPE 工作目录的本地路径与选定目录不一致。".into());
        }
        let file = dir.try_clone().map_err(device::err)?.into_std_file();
        let binding = FileBinding::new(&file, &disk)?;
        let mut flags = 0;
        let mut fs = [0; 16];
        if unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        } == 0
            || flags & FILE_READ_ONLY_VOLUME != 0
            || !String::from_utf16_lossy(
                &fs[..fs.iter().position(|&unit| unit == 0).unwrap_or(fs.len())],
            )
            .eq_ignore_ascii_case("NTFS")
        {
            return Err("WinPE 工作目录需要可写的 NTFS 卷。".into());
        }
        Ok(Self {
            dir,
            path,
            binding,
            identity,
        })
    }

    fn stage(&self) -> Result<Stage<'_>, String> {
        let (path, _) = device::destination_location(&self.path, &[], WORKING_BYTES)?;
        let current =
            Dir::open_ambient_dir(path, cap_std::ambient_authority()).map_err(device::err)?;
        if recovery_output::identity(&current)? != self.identity {
            return Err("WinPE 工作目录已变化。".into());
        }
        let name = format!("DiskStudio-WinPE-{}", uuid::Uuid::new_v4().simple());
        self.dir.create_dir(&name).map_err(device::err)?;
        let dir = self.dir.open_dir_nofollow(&name).map_err(device::err)?;
        let identity = recovery_output::identity(&dir)?;
        Ok(Stage {
            path: self.path.join(&name),
            parent: self,
            name,
            identity,
        })
    }
}

impl Stage<'_> {
    fn open(&self) -> Result<Dir, String> {
        let retained = self
            .parent
            .dir
            .open_dir_nofollow(&self.name)
            .map_err(device::err)?;
        let ambient =
            Dir::open_ambient_dir(&self.path, cap_std::ambient_authority()).map_err(device::err)?;
        if recovery_output::identity(&retained)? != self.identity
            || recovery_output::identity(&ambient)? != self.identity
        {
            return Err("WinPE 临时目录已变化。".into());
        }
        self.parent.binding.verify_same_volume(&retained)?;
        Ok(retained)
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if let Ok(dir) = self.parent.dir.open_dir_nofollow(&self.name) {
            if recovery_output::identity(&dir).ok() == Some(self.identity) {
                let _ = dir.remove_open_dir_all();
            }
        }
    }
}

fn required_subdir(dir: &Dir, name: &str) -> Result<Dir, String> {
    optional_subdir(dir, name)?.ok_or_else(|| format!("缺少 WinPE 所需目录 {name}。"))
}

fn package_names() -> Vec<String> {
    COMPONENTS
        .iter()
        .flat_map(|name| {
            [
                format!("WinPE-{name}.cab"),
                format!("WinPE-{name}_en-us.cab"),
            ]
        })
        .collect()
}

impl Adk {
    pub(super) fn prepare(
        input: &Path,
        scratch: &Path,
        destination: &Target,
        app: (File, FileBinding),
    ) -> Result<(Self, Files, Preview), String> {
        let (root, source) = open_source(input)?;
        if [source.target(), app.1.target()].contains(&destination) {
            return Err("WinPE 启动分区必须与来源位于不同物理磁盘。".into());
        }
        let scratch = Scratch::open(scratch, destination)?;
        let template = required_subdir(&root, "media")?;
        let mut verified = |file: &File| source.verify_same_volume(file);
        let mut walk = Walk {
            files: BTreeMap::new(),
            verify: &mut verified,
            items: 0,
        };
        walk.folder(&template, "", 0)?;
        let mut files = walk.files;
        if files.contains_key(media::WIM_PATH) {
            return Err("请选原始 ADK amd64 目录。介质模板中已有 boot.wim。".into());
        }
        let wim = plain(&required_subdir(&root, "en-us")?, Path::new("winpe.wim"))?;
        source.verify_same_volume(&wim)?;
        let size =
            u32::try_from(wim.metadata().map_err(device::err)?.len()).map_err(device::err)?;
        files.insert(
            media::WIM_PATH.into(),
            Input {
                file: wim,
                member: Member {
                    path: media::WIM_PATH.into(),
                    size,
                },
            },
        );
        let mut preview = inspect_media(&mut files)?;
        attach_app(&mut files, &mut preview, app.0)?;
        preview.adk = true;
        let components = required_subdir(&root, "WinPE_OCs")?;
        let language = required_subdir(&components, "en-us")?;
        let packages = package_names()
            .into_iter()
            .map(|name| {
                let dir = if name.ends_with("_en-us.cab") {
                    &language
                } else {
                    &components
                };
                let file = plain(dir, Path::new(&name))?;
                source.verify_same_volume(&file)?;
                Ok((name, file))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let bindings = vec![source, app.1, scratch.binding.clone()];
        FileBinding::validate_all(&bindings)?;
        Ok((
            Self {
                bindings,
                packages,
                scratch,
            },
            files,
            preview,
        ))
    }

    pub(super) fn build<'a>(
        &'a mut self,
        files: &mut Files,
        preview: &mut Preview,
        cancel: &AtomicBool,
        mut progress: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Stage<'a>, String> {
        FileBinding::validate_all(&self.bindings)?;
        let stage = self.scratch.stage()?;
        let work = stage.open()?;
        for name in ["image", "scratch", "packages", "logs"] {
            work.create_dir(name).map_err(device::err)?;
        }
        let packages = work.open_dir_nofollow("packages").map_err(device::err)?;
        for (name, input) in &mut self.packages {
            copy(input, &packages, name, cancel)?;
        }
        copy(
            &mut files
                .get_mut(media::WIM_PATH)
                .ok_or("缺少 WinPE 来源 WIM。")?
                .file,
            &work,
            "base.wim",
            cancel,
        )?;

        let dism = dism_executable()?;
        let image = stage.path.join("image");
        let output = stage.path.join("boot.wim");
        let total = self.packages.len() as u64 + 2;
        progress(0, total)?;
        run_dism(
            &dism,
            &stage,
            "apply",
            &[
                "/Apply-Image".into(),
                path_arg("/ImageFile:", &stage.path.join("base.wim")),
                format!("/Index:{}", preview.wim_image).into(),
                path_arg("/ApplyDir:", &image),
                "/CheckIntegrity".into(),
            ],
            cancel,
        )?;
        let os = required_subdir(&work, "image")?;
        let windows = required_subdir(&os, "Windows")?;
        let system = required_subdir(&windows, "System32")?;
        let mut init = plain(&system, Path::new("wpeinit.exe"))?;
        let mut header = vec![0; 64 * 1024];
        let count = init.read(&mut header).map_err(device::err)?;
        if crate::boot_inspection::pe_machine(&header[..count], &[2, 3]) != Ok(0x8664) {
            return Err("ADK 来源需要 amd64 WinPE 映像。".into());
        }
        drop(init);
        for (index, (name, _)) in self.packages.iter().enumerate() {
            progress(index as u64 + 1, total)?;
            run_dism(
                &dism,
                &stage,
                name,
                &[
                    path_arg("/Image:", &image),
                    "/Add-Package".into(),
                    path_arg("/PackagePath:", &stage.path.join("packages").join(name)),
                ],
                cancel,
            )?;
        }
        let app = files
            .get_mut(media::APP_KEY)
            .ok_or("WinPE 分区中缺少 DiskStudio.exe。")?;
        let mut embedded = copy(&mut app.file, &os, media::APP_PATH, cancel)?;
        embedded.seek(SeekFrom::Start(0)).map_err(device::err)?;
        let digest = data_io::hash_exact(&mut embedded, u64::from(app.member.size), |_| {
            active(cancel)
        })?;
        if preview.app_sha256.as_deref() != Some(digest.as_str()) {
            return Err("DiskStudio 程序与预览中的 SHA-256 不一致。".into());
        }
        drop(embedded);
        let mut shell = recovery_output::create(&system, "winpeshl.ini")?;
        shell
            .write_all(SHELL)
            .and_then(|_| shell.sync_all())
            .map_err(device::err)?;
        drop(shell);
        progress(total - 1, total)?;
        run_dism(
            &dism,
            &stage,
            "capture",
            &[
                "/Capture-Image".into(),
                path_arg("/ImageFile:", &output),
                path_arg("/CaptureDir:", &image),
                "/Name:DiskStudio WinPE".into(),
                "/Bootable".into(),
                "/Compress:fast".into(),
                "/CheckIntegrity".into(),
                "/Verify".into(),
            ],
            cancel,
        )?;

        let mut wim = plain(&stage.open()?, Path::new("boot.wim"))?;
        let size =
            u32::try_from(wim.metadata().map_err(device::err)?.len()).map_err(device::err)?;
        let mut header = [0; 208];
        wim.read_exact(&mut header).map_err(device::err)?;
        preview.wim_image = media::wim_index(&header, u64::from(size))?;
        preview.wim_bytes = u64::from(size);
        progress(total, total)?;
        files.insert(
            media::WIM_PATH.into(),
            Input {
                file: wim,
                member: Member {
                    path: media::WIM_PATH.into(),
                    size,
                },
            },
        );
        Ok(stage)
    }
}

fn active(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn copy(input: &mut File, dir: &Dir, name: &str, cancel: &AtomicBool) -> Result<File, String> {
    active(cancel)?;
    input.seek(SeekFrom::Start(0)).map_err(device::err)?;
    let size = input.metadata().map_err(device::err)?.len();
    let mut output = recovery_output::create(dir, name)?;
    data_io::copy_exact(input, &mut output, size, |_| active(cancel))?;
    output.sync_all().map_err(device::err)?;
    Ok(output)
}

fn path_arg(prefix: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path);
    arg
}

fn dism_executable() -> Result<PathBuf, String> {
    let mut path = [0; 1024];
    let len = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if len == 0 || len >= path.len() {
        return Err("找不到 Windows 系统 DISM。".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])).join("Dism.exe"))
}

fn run_dism(
    dism: &Path,
    stage: &Stage<'_>,
    task: &str,
    args: &[OsString],
    cancel: &AtomicBool,
) -> Result<(), String> {
    active(cancel)?;
    let work = stage.open()?;
    for name in ["image", "packages", "scratch", "logs"] {
        let directory = work.open_dir_nofollow(name).map_err(device::err)?;
        stage.parent.binding.verify_same_volume(&directory)?;
    }
    let system = dism.parent().ok_or("找不到 Windows 系统 DISM。")?;
    let result = Command::new(dism)
        .creation_flags(CREATE_NO_WINDOW)
        .current_dir(system)
        .stdin(Stdio::null())
        .arg(OsStr::new("/English"))
        .arg(path_arg("/ScratchDir:", &stage.path.join("scratch")))
        .arg(path_arg(
            "/LogPath:",
            &stage.path.join("logs").join(format!("{task}.log")),
        ))
        .args(args)
        .output()
        .map_err(device::err)?;
    active(cancel)?;
    if !result.status.success() {
        let output = String::from_utf8_lossy(&result.stdout);
        let detail: String = output
            .lines()
            .map(str::trim)
            .skip_while(|line| !line.starts_with("Error:"))
            .filter(|line| !line.is_empty())
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let status = result.status;
        return Err(format!("DISM {task} ({status}) {detail}")
            .trim_end()
            .to_owned());
    }
    stage.open()?;
    Ok(())
}

#[test]
fn adk_components_and_shell_are_in_startup_order() {
    assert_eq!(
        package_names(),
        [
            "WinPE-WMI.cab",
            "WinPE-WMI_en-us.cab",
            "WinPE-NetFx.cab",
            "WinPE-NetFx_en-us.cab",
            "WinPE-Scripting.cab",
            "WinPE-Scripting_en-us.cab",
            "WinPE-PowerShell.cab",
            "WinPE-PowerShell_en-us.cab",
            "WinPE-StorageWMI.cab",
            "WinPE-StorageWMI_en-us.cab",
        ]
    );
    assert_eq!(
        std::str::from_utf8(SHELL)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "[LaunchApps]",
            r"%SYSTEMROOT%\System32\wpeinit.exe",
            r"%SYSTEMDRIVE%\DiskStudio.exe",
            r"%SYSTEMROOT%\System32\cmd.exe",
        ]
    );
}

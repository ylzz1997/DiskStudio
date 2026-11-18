//! One job lifecycle for resumable recovery engines. No automatic device access.
use super::{finish, idle, start, state, Catalog, ScanMode, ScanSource, Selection};
use crate::{
    recovery_checkpoint::{self, Cursor, Progress},
    signature_checkpoint::Binding,
    signature_scan::Options,
};
use serde::Serialize;
#[derive(Clone)]
pub(super) struct Saved {
    pub id: String,
    source: Selection,
    mode: ScanMode,
    options: Option<Options>,
    size: u64,
    binding: Option<Binding>,
    pub cursor: Option<Cursor>,
    pub pause_requested: bool,
    loaded: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    source: Selection,
    mode: ScanMode,
    engine: &'static str,
    options: Option<Options>,
    done: u64,
    total: u64,
    found: usize,
    running: bool,
    can_resume: bool,
    can_save: bool,
    loaded: bool,
}
pub fn status() -> Option<Info> {
    let s = state();
    let p = s.recovery_progress.as_ref()?;
    let cursor = p.cursor.as_ref()?;
    if !matches!(s.scan.as_ref(),Some((id,ScanSource::Disk(_))) if *id==p.id) {
        return None;
    }
    let job = s.job.as_ref().filter(|j| j.id == p.id);
    let running = job.is_some_and(|j| j.state == "running");
    let valid = cursor.validate(p.size).is_ok()
        && p.binding.is_some()
        && cursor.count() == s.candidates.len();
    Some(Info {
        id: p.id.clone(),
        source: p.source.clone(),
        mode: p.mode,
        engine: cursor.kind(),
        options: cursor.options(),
        done: if running {
            job.unwrap().done
        } else {
            cursor.done()
        },
        total: if running {
            job.unwrap().total
        } else {
            cursor.total(p.size).ok()?
        },
        found: s.candidates.len(),
        running,
        can_resume: !running && valid && !cursor.sealed(),
        can_save: !running && valid,
        loaded: p.loaded,
    })
}
pub fn pause(id: String) -> Result<(), String> {
    let mut s = state();
    if s.job
        .as_ref()
        .is_none_or(|j| j.id != id || j.state != "running")
    {
        return Err("恢复扫描不在运行".into());
    }
    let p = s
        .recovery_progress
        .as_mut()
        .filter(|p| p.id == id && p.cursor.is_some())
        .ok_or("此扫描引擎尚不支持暂停续扫")?;
    p.pause_requested = true;
    s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}
fn stopped(id: &str) -> Result<(Saved, Vec<crate::recovery::Candidate>, Catalog), String> {
    let s = state();
    idle(&s)?;
    let p = s
        .recovery_progress
        .as_ref()
        .filter(|p| p.id == id && s.scan.as_ref().is_some_and(|(scan, _)| scan == id))
        .ok_or("恢复进度已失效")?;
    Ok((p.clone(), s.candidates.clone(), s.directories.clone()))
}
pub fn resume(id: String, source: Selection) -> Result<String, String> {
    let (p, _, _) = stopped(&id)?;
    if source.target != p.source.target
        || source.layout_key != p.source.layout_key
        || source.partition != p.source.partition
    {
        return Err("请选择进度对应的原磁盘或分区".into());
    }
    launch(source, p.mode, p.options.clone(), Some(p))
}
pub fn start_scan(
    source: Selection,
    mode: ScanMode,
    options: Option<Options>,
) -> Result<String, String> {
    if mode != ScanMode::Signatures && (source.partition.is_none() || options.is_some()) {
        return Err("文件系统扫描须选择分区，签名类型选项只适用于签名扫描".into());
    }
    launch(source, mode, options, None)
}
fn launch(
    source: Selection,
    mode: ScanMode,
    options: Option<Options>,
    previous: Option<Saved>,
) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = (source, mode, options, previous);
        Err("恢复扫描当前仅支持 Windows".into())
    }
    #[cfg(windows)]
    {
        use crate::data_windows as platform;
        use std::io::{Read, Seek, SeekFrom};
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘任务正在运行")?;
        idle(&state())?;
        let resolved = platform::resolve(&source, false, false)?;
        let options = if mode == ScanMode::Signatures {
            let o = options.unwrap_or_default();
            o.range(resolved.size)?;
            Some(o)
        } else {
            None
        };
        if let Some(old) = &previous {
            let cursor = old.cursor.as_ref().ok_or("此扫描没有可继续的进度")?;
            cursor.validate(resolved.size)?;
            if cursor.sealed()
                || old.binding.is_none()
                || old.size != resolved.size
                || old.mode != mode
            {
                return Err("扫描已结束或源范围改变，不能继续".into());
            }
            let s = state();
            if s.recovery_progress.as_ref().is_none_or(|p| p.id != old.id) {
                return Err("进度已被替换".into());
            }
            // A callback failure/panic must not resume a partly published record.
            // Validate once under OPERATION, rather than on every status poll.
            Progress::capture(
                source.clone(),
                old.binding.clone().ok_or("恢复源特征缺失")?,
                cursor.clone(),
                &s.candidates,
                &s.directories,
            )?;
        }
        let (id, cancel) = start(if mode == ScanMode::Existing {
            "读取分区目录"
        } else {
            "文件恢复扫描"
        })?;
        let mut saved = previous.clone().unwrap_or(Saved {
            id: id.clone(),
            source: source.clone(),
            mode,
            options: options.clone(),
            size: resolved.size,
            binding: None,
            cursor: options.clone().map(|options| Cursor::Signatures {
                options,
                cursor: Default::default(),
            }),
            pause_requested: false,
            loaded: false,
        });
        saved.id = id.clone();
        saved.pause_requested = false;
        let mut cursor = saved.cursor.clone();
        let mut binding = saved.binding.clone();
        {
            let mut s = state();
            if previous.is_none() {
                s.candidates.clear();
                s.directories = Catalog::default();
            }
            s.scan = Some((id.clone(), source.clone().into()));
            s.scan_mode = mode;
            s.scan_name = resolved.label;
            s.scan_completion = crate::directory_report::Completion::Running;
            s.pending = None;
            if let Some(c) = &cursor {
                let found = s.candidates.len();
                if let Some(j) = &mut s.job {
                    j.done = c.done();
                    j.total = c.total(resolved.size)?;
                    j.found = found;
                }
            }
            s.recovery_progress = Some(saved);
        }
        drop(guard);
        let work_id = id.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<(String, Option<String>), String> {
                    let _guard = crate::OPERATION
                        .try_lock()
                        .map_err(|_| "另一项磁盘任务正在运行")?;
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err("扫描已停止，尚未打开设备".into());
                    }
                    let r = platform::resolve(&source, false, false)?;
                    let sector = r.disk.logical_sector_size.ok_or("逻辑扇区未知")?;
                    let mut reader = crate::ext_recovery::scan_progress::SharedReader::new(
                        platform::reader(&r)?,
                    );
                    if let Some(old) = &binding {
                        old.verify(&mut reader, r.size, r.offset, sector)?;
                        cursor
                            .as_ref()
                            .ok_or("续扫引擎缺失")?
                            .verify_owned(reader.clone(), r.size)?;
                    } else if mode != ScanMode::Signatures {
                        let mut boot = [0; 512];
                        reader.read_exact(&mut boot).map_err(platform::err)?;
                        reader.seek(SeekFrom::Start(0)).map_err(platform::err)?;
                        if &boot[3..11] == b"NTFS    " {
                            cursor = Some(Cursor::Ntfs {
                                mode,
                                cursor: Default::default(),
                            });
                        } else if crate::recovery::fat_boot(&boot) {
                            cursor = Some(Cursor::Fat {
                                mode,
                                cursor: Default::default(),
                            });
                        } else if r.size >= 2048 {
                            reader.seek(SeekFrom::Start(1080)).map_err(platform::err)?;
                            let mut magic = [0; 2];
                            reader.read_exact(&mut magic).map_err(platform::err)?;
                            reader.seek(SeekFrom::Start(0)).map_err(platform::err)?;
                            if magic == [0x53, 0xef] {
                                cursor = Some(Cursor::Ext {
                                    mode,
                                    cursor: Default::default(),
                                });
                            }
                        }
                    }
                    if cursor.is_some() && binding.is_none() {
                        binding = Some(Binding::capture(&mut reader, r.size, r.offset, sector)?);
                    }
                    {
                        let mut s = state();
                        if let Some(p) = s.recovery_progress.as_mut().filter(|p| p.id == work_id) {
                            p.cursor = cursor.clone();
                        }
                    }
                    let tick = |done, total| {
                        super::progress(
                            &work_id,
                            &cancel,
                            if mode == ScanMode::Signatures {
                                "签名扫描"
                            } else {
                                "文件系统扫描"
                            },
                            done,
                            total,
                        )
                    };
                    let found = |c| {
                        let mut s = state();
                        s.directories.add_file(&c);
                        s.candidates.push(c);
                        let n = s.candidates.len();
                        if let Some(j) = &mut s.job {
                            j.found = n;
                        }
                    };
                    let directory = |d| state().directories.insert(d);
                    let message = match &mut cursor {
                        Some(Cursor::Signatures { options, cursor }) => {
                            crate::signature_scan::continue_scan(
                                &mut reader,
                                r.size,
                                options,
                                cursor,
                                tick,
                                found,
                            )?
                        }
                        Some(Cursor::Ntfs { mode, cursor }) => crate::ntfs_scan::continue_scan(
                            &mut reader,
                            *mode,
                            cursor,
                            tick,
                            found,
                            directory,
                        )?,
                        Some(Cursor::Fat { mode, cursor }) => {
                            crate::fat_recovery::scan_progress::continue_scan(
                                &mut reader,
                                *mode,
                                cursor,
                                tick,
                                found,
                                directory,
                            )?
                        }
                        Some(Cursor::Ext { mode, cursor }) => {
                            crate::ext_recovery::scan_progress::continue_scan(
                                reader.clone(),
                                *mode,
                                cursor,
                                cancel.clone(),
                                tick,
                                found,
                                directory,
                            )?
                        }
                        None => {
                            return crate::recovery::scan_filesystem_owned(
                                reader,
                                mode,
                                cancel.clone(),
                                tick,
                                found,
                                directory,
                            )
                            .map(|s| (s, None));
                        }
                    };
                    binding.as_ref().ok_or("恢复源特征缺失")?.verify(
                        &mut reader,
                        r.size,
                        r.offset,
                        sector,
                    )?;
                    cursor
                        .as_ref()
                        .ok_or("恢复游标缺失")?
                        .verify_owned(reader.clone(), r.size)?;
                    platform::resolve(&source, false, false)?;
                    Ok((message, None))
                },
            ))
            .unwrap_or_else(|_| Err("恢复扫描异常结束；源盘未写入，完整记录进度保留".into()));
            {
                let mut s = state();
                if let Some(p) = s.recovery_progress.as_mut().filter(|p| p.id == work_id) {
                    p.cursor = cursor.clone();
                    p.binding = binding;
                }
                if let (Some(c), Some(j)) = (&cursor, s.job.as_mut().filter(|j| j.id == work_id)) {
                    j.done = c.done();
                    j.total = c.total(resolved.size).unwrap_or(j.total);
                }
            }
            finish(&work_id, result);
        });
        Ok(id)
    }
}
pub fn save(id: String) -> Result<Option<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("仅 Windows 支持原生保存".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘任务正在运行")?;
        let (p, files, tree) = stopped(&id)?;
        let cursor = p.cursor.ok_or("此扫描引擎尚未支持保存进度")?;
        let name = match cursor.kind() {
            "ntfs" => "DiskStudio-NTFS-progress.dsrecover",
            "fat" => "DiskStudio-FAT-progress.dsrecover",
            "exfat" => "DiskStudio-exFAT-progress.dsrecover",
            "ext" => "DiskStudio-Ext-progress.dsrecover",
            _ => "DiskStudio-recovery.dsrecover",
        };
        let progress = Progress::capture(
            p.source,
            p.binding.ok_or("尚无可保存的源特征")?,
            cursor,
            &files,
            &tree,
        )?;
        let bytes = recovery_checkpoint::encode(progress.clone())?;
        files::save(&progress.source, &bytes, name)
    }
}
pub fn load(source: Selection) -> Result<Option<Info>, String> {
    #[cfg(not(windows))]
    {
        let _ = source;
        Err("仅 Windows 支持原生加载".into())
    }
    #[cfg(windows)]
    {
        use crate::data_windows as platform;
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘任务正在运行")?;
        idle(&state())?;
        let Some(bytes) = files::load(&source)? else {
            return Ok(None);
        };
        let progress = recovery_checkpoint::decode(&bytes, &source)?; // identity/scope before raw open
        let r = platform::resolve(&source, false, false)?;
        let mut reader =
            crate::ext_recovery::scan_progress::SharedReader::new(platform::reader(&r)?);
        progress.binding.verify(
            &mut reader,
            r.size,
            r.offset,
            r.disk.logical_sector_size.ok_or("逻辑扇区未知")?,
        )?;
        progress.verify_owned(reader.clone(), r.size)?;
        platform::resolve(&source, false, false)?;
        drop(reader);
        let (files, tree) = progress.validate(&source)?;
        let id = uuid::Uuid::new_v4().to_string();
        let c = &progress.cursor;
        let sealed = c.sealed();
        let mode = c.mode();
        {
            let mut s = state();
            idle(&s)?;
            s.candidates = files;
            s.directories = tree;
            s.scan = Some((id.clone(), source.clone().into()));
            s.scan_name = r.label;
            s.scan_mode = mode;
            s.scan_completion = if sealed {
                crate::directory_report::Completion::Ended
            } else {
                crate::directory_report::Completion::Paused
            };
            s.pending = None;
            s.job=Some(super::Job{id:id.clone(),title:if mode==ScanMode::Existing{"读取分区目录"}else{"文件恢复扫描"}.into(),state:if sealed{"completed"}else{"paused"}.into(),phase:"已加载进度".into(),done:c.done(),total:c.total(r.size)?,found:s.candidates.len(),message:"已核对磁盘、布局、内容特征和续扫边界。结果和目录是历史观察；少量特征不能证明源内容全部未变，预览与提取会重新核对候选。加载不会自动继续扫描。".into(),sha256:None});
            s.recovery_progress = Some(Saved {
                id,
                source,
                mode,
                options: c.options(),
                size: r.size,
                binding: Some(progress.binding),
                cursor: Some(progress.cursor),
                pause_requested: false,
                loaded: true,
            });
        }
        drop(guard);
        Ok(status())
    }
}

#[cfg(windows)]
mod files {
    use super::*;
    use crate::data_windows as platform;
    use cap_std::fs::OpenOptionsExt;
    use cap_std::fs::{Dir, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    fn selected(
        path: &std::path::Path,
        source: &Selection,
        size: u64,
    ) -> Result<(Dir, String), String> {
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("dsrecover"))
        {
            return Err("请选择 .dsrecover 恢复进度文件。".into());
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("文件名无效")?;
        if name.len() > 180 || name.contains([':', '/', '\\']) || name.chars().any(char::is_control)
        {
            return Err("进度文件名无效。".into());
        }
        let parent = platform::destination_folder(
            path.parent().ok_or("文件目录无效")?,
            &source.target,
            size,
        )?;
        Ok((
            Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(platform::err)?,
            name.into(),
        ))
    }
    pub fn load(source: &Selection) -> Result<Option<Vec<u8>>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(
                "加载所选磁盘或分区的恢复扫描进度",
            ))
            .add_filter(
                crate::native_locale::text("DiskStudio 恢复进度"),
                &["dsrecover"],
            )
            .pick_file()
        else {
            return Ok(None);
        };
        let (dir, name) = selected(&path, source, 0)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = dir
            .open_with(name, &options)
            .map_err(platform::err)?
            .into_std();
        let m = file.metadata().map_err(platform::err)?;
        if !m.is_file()
            || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || m.len() > crate::signature_checkpoint::MAX_FILE
        {
            return Err("进度须为不超过 64 MiB 的普通文件。".into());
        }
        let mut bytes = vec![0; m.len() as usize];
        file.read_exact(&mut bytes).map_err(platform::err)?;
        Ok(Some(bytes))
    }
    pub fn save(
        source: &Selection,
        bytes: &[u8],
        default_name: &str,
    ) -> Result<Option<String>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(
                "将恢复进度保存到另一块磁盘（使用新文件名）",
            ))
            .add_filter(
                crate::native_locale::text("DiskStudio 恢复进度"),
                &["dsrecover"],
            )
            .set_file_name(default_name)
            .save_file()
        else {
            return Ok(None);
        };
        let (dir, name) = selected(&path, source, bytes.len() as u64)?;
        let partial = format!("DiskStudio-recovery-{}.partial", uuid::Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
            .share_mode(0);
        let mut file = dir
            .open_with(&partial, &options)
            .map_err(platform::err)?
            .into_std();
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(platform::err)?;
        file.seek(SeekFrom::Start(0)).map_err(platform::err)?;
        let mut readback = vec![0; bytes.len()];
        file.read_exact(&mut readback).map_err(platform::err)?;
        use sha2::{Digest, Sha256};
        if Sha256::digest(bytes) != Sha256::digest(&readback) {
            return Err(format!("进度回读校验失败，保留 {partial}"));
        }
        recovery_checkpoint::decode(&readback, source)?;
        crate::recovery_output::finish(&file, &dir, &name)
            .map_err(|e| format!("{e}；未完成文件保留为 {partial}"))?;
        Ok(Some(name))
    }
}

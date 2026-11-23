//! One job lifecycle for resumable recovery engines. No automatic device access.
use super::{finish, idle, start, state, Catalog, ScanMode, ScanSource, Selection};
use crate::{
    recovery_checkpoint::{self, Cursor, Progress},
    signature_checkpoint::Binding,
    signature_scan::Options,
};
use serde::{Deserialize, Serialize};
const JOURNAL_PHASE: &str = "Ext4 日志扫描";

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ProgressSource {
    Disk(Selection),
    Retained(super::retained_signature_tasks::Selection),
}

impl ProgressSource {
    fn matches(&self, source: &ScanSource) -> bool {
        match (self, source) {
            (Self::Disk(saved), ScanSource::Disk(source)) => {
                saved.target == source.target && saved.layout_key == source.layout_key
                    && saved.partition == source.partition
            }
            (Self::Retained(saved), source) => saved.matches(source),
            _ => false,
        }
    }
}

#[derive(Clone)]
pub(super) struct Saved {
    pub id: String,
    pub(super) source: ProgressSource,
    pub(super) mode: ScanMode,
    pub(super) options: Option<Options>,
    pub(super) size: u64,
    pub(super) binding: Option<Binding>,
    pub(super) retained_digest: Option<String>,
    pub cursor: Option<Cursor>,
    pub pause_requested: bool,
    pub(super) loaded: bool,
}

#[cfg(windows)]
fn identify_metadata(
    reader: &mut (impl std::io::Read + std::io::Seek),
    size: u64,
    mode: ScanMode,
) -> Result<Option<Cursor>, String> {
    use crate::data_windows::err;
    use std::io::SeekFrom;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    if crate::bitlocker::boot_marker(&boot) {
        return Err(crate::bitlocker::CIPHERTEXT_NOTICE.into());
    }
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    if &boot[3..11] == b"NTFS    " {
        return Ok(Some(Cursor::Ntfs {
            mode,
            cursor: Default::default(),
        }));
    }
    if crate::recovery::fat_boot(&boot) {
        return Ok(Some(Cursor::Fat {
            mode,
            cursor: Default::default(),
        }));
    }
    if size >= 2048 {
        reader.seek(SeekFrom::Start(1080)).map_err(err)?;
        let mut magic = [0; 2];
        reader.read_exact(&mut magic).map_err(err)?;
        reader.seek(SeekFrom::Start(0)).map_err(err)?;
        if magic == [0x53, 0xef] {
            return Ok(Some(if mode == ScanMode::Deleted {
                Cursor::ExtJournal {
                    cursor: Default::default(),
                }
            } else {
                Cursor::Ext {
                    mode,
                    cursor: Default::default(),
                }
            }));
        }
    }
    Ok(None)
}

#[cfg(any(windows, test))]
fn scan_metadata<R: std::io::Read + std::io::Seek + 'static>(
    reader: &mut crate::ext_recovery::scan_progress::SharedReader<R>,
    cursor: &mut Cursor,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(crate::recovery::Candidate),
    directory: impl FnMut(crate::recovery_tree::Directory) -> Result<(), String>,
) -> Result<String, String> {
    match cursor {
        Cursor::Ntfs { mode, cursor } => {
            crate::ntfs_scan::continue_scan(reader, *mode, cursor, tick, found, directory)
        }
        Cursor::Fat { mode, cursor } => crate::fat_recovery::scan_progress::continue_scan(
            reader, *mode, cursor, tick, found, directory,
        ),
        Cursor::Ext { mode, cursor } => crate::ext_recovery::scan_progress::continue_scan(
            reader.clone(),
            *mode,
            cursor,
            cancel.clone(),
            tick,
            found,
            directory,
        ),
        Cursor::ExtJournal { cursor } => crate::ext_recovery::deleted::continue_scan(
            reader.clone(),
            cursor,
            cancel.clone(),
            tick,
            found,
            directory,
        ),
        _ => Err("目录阶段使用了无效的扫描引擎。".into()),
    }
}

#[cfg(any(windows, test))]
pub(crate) fn scan_combined_metadata<R: std::io::Read + std::io::Seek + 'static>(
    reader: &mut crate::ext_recovery::scan_progress::SharedReader<R>,
    size: u64,
    metadata: &mut Cursor,
    journal: Option<&mut crate::ext_recovery::deleted::Checkpoint>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut tick: impl FnMut(&'static str, u64, u64) -> Result<(), String>,
    mut found: impl FnMut(crate::recovery::Candidate),
    mut directory: impl FnMut(crate::recovery_tree::Directory, bool) -> Result<(), String>,
) -> Result<(), String> {
    if !metadata.sealed() {
        scan_metadata(
            reader,
            metadata,
            cancel,
            |done, total| tick("文件系统扫描", done, total),
            &mut found,
            |node| directory(node, false),
        )?;
    }
    if !metadata.sealed() {
        return Err("完整恢复的目录阶段未结束。".into());
    }
    metadata.verify_owned(reader.clone(), size)?;
    if let Some(journal) = journal.filter(|journal| !journal.sealed) {
        let first = metadata.count() as u32;
        crate::ext_recovery::deleted::continue_scan(
            reader.clone(),
            journal,
            cancel.clone(),
            |done, total| tick(JOURNAL_PHASE, done, total),
            |mut file| {
                // The standalone journal uses one-based IDs. Index into
                // this combined result set immediately after the directory.
                file.id = first + (file.id - 1);
                found(file);
            },
            |node| directory(node, true),
        )?;
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn scan_signatures(
    reader: &mut (impl std::io::Read + std::io::Seek),
    size: u64,
    sector: u64,
    options: &Options,
    cursor: &mut crate::signature_scan::Checkpoint,
    job: &str,
    cancel: &std::sync::atomic::AtomicBool,
    found: impl FnMut(crate::recovery::Candidate),
) -> Result<String, String> {
    let range = options.range(size)?;
    let phase = if options.free_only {
        "核对空闲簇"
    } else {
        "签名扫描"
    };
    super::progress(job, cancel, phase, cursor.consumed, range.end - range.start)?;
    let allocation = options
        .free_only
        .then(|| crate::used_space::inspect(reader, size, sector, None, cancel))
        .transpose()?;
    let message = crate::signature_scan::continue_scan_scoped(
        reader,
        size,
        options,
        cursor,
        sector,
        allocation.as_ref(),
        |done, total| super::progress(job, cancel, "签名扫描", done, total),
        found,
    )?;
    if let Some(plan) = allocation {
        plan.proof.verify(reader, cancel)?;
    }
    Ok(message)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    source: ProgressSource,
    mode: ScanMode,
    combined: bool,
    engine: &'static str,
    options: Option<Options>,
    done: u64,
    total: u64,
    found: usize,
    running: bool,
    can_pause: bool,
    can_resume: bool,
    can_save: bool,
    loaded: bool,
}
pub fn status() -> Option<Info> {
    let s = state();
    let p = s.recovery_progress.as_ref()?;
    let cursor = p.cursor.as_ref()?;
    if !s.scan.as_ref().is_some_and(|(id, source)| *id == p.id && p.source.matches(source)) {
        return None;
    }
    let job = s.job.as_ref().filter(|j| j.id == p.id);
    let running = job.is_some_and(|j| j.state == "running");
    let combined = matches!(cursor, Cursor::Combined { .. });
    let valid = cursor.validate(p.size).is_ok()
        && p.binding.is_some()
        && cursor.count() == s.candidates.len();
    Some(Info {
        id: p.id.clone(),
        source: p.source.clone(),
        mode: p.mode,
        combined,
        engine: if combined && running {
            match job.map(|j| j.phase.as_str()) {
                Some("签名扫描" | "核对空闲簇") => "signatures",
                Some(JOURNAL_PHASE) => "extJournal",
                _ => cursor.kind(),
            }
        } else {
            cursor.kind()
        },
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
        can_pause: running && (matches!(p.source, ProgressSource::Disk(_)) || p.binding.is_some()),
        can_resume: !running && valid && !cursor.sealed(),
        can_save: !running && valid && (matches!(p.source, ProgressSource::Disk(_)) || p.retained_digest.is_some()),
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
        .filter(|p| p.id == id && p.cursor.is_some() && (matches!(p.source, ProgressSource::Disk(_)) || p.binding.is_some()))
        .ok_or("此扫描引擎尚不支持暂停续扫")?;
    p.pause_requested = true;
    s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}
pub(super) fn stopped(id: &str) -> Result<(Saved, Vec<crate::recovery::Candidate>, Catalog), String> {
    let s = state();
    idle(&s)?;
    let p = s
        .recovery_progress
        .as_ref()
        .filter(|p| p.id == id && s.scan.as_ref().is_some_and(|(scan, source)| scan == id && p.source.matches(source)))
        .ok_or("恢复进度已失效")?;
    Ok((p.clone(), s.candidates.clone(), s.directories.clone()))
}
pub fn resume(id: String, source: ProgressSource) -> Result<String, String> {
    let source = match source {
        ProgressSource::Disk(source) => source,
        ProgressSource::Retained(source) => return super::retained_signature_tasks::resume(id, source),
    };
    let (p, _, _) = stopped(&id)?;
    if !p.source.matches(&ScanSource::Disk(source.clone())) {
        return Err("请选择进度对应的原磁盘或分区".into());
    }
    launch(source, p.mode, p.options.clone(), Some(p))
}
pub fn start_scan(
    source: Selection,
    mode: ScanMode,
    options: Option<Options>,
) -> Result<String, String> {
    if (mode != ScanMode::Signatures && source.partition.is_none())
        || (options.is_some() && !matches!(mode, ScanMode::Signatures | ScanMode::AllFiles))
    {
        return Err(
            "请选择分区进行文件系统扫描。签名类型选项适用于完整恢复或单独签名扫描。".into(),
        );
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
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘任务正在运行")?;
        idle(&state())?;
        let resolved = platform::resolve(&source, false, false)?;
        let options = if mode == ScanMode::Signatures || options.is_some() {
            let o = options.unwrap_or_default();
            o.range(resolved.size)?;
            o.alignment(resolved.disk.logical_sector_size.unwrap_or(0))?;
            if o.free_only && source.partition.is_none() {
                return Err("仅空闲簇扫描需要选择实体分区。".into());
            }
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
        let combined = mode == ScanMode::AllFiles && options.is_some();
        let (id, cancel) = start(if mode == ScanMode::Existing {
            "读取分区目录"
        } else if combined {
            "完整恢复"
        } else {
            "文件恢复扫描"
        })?;
        let mut saved = previous.clone().unwrap_or(Saved {
            id: id.clone(),
            source: ProgressSource::Disk(source.clone()),
            mode,
            options: options.clone(),
            size: resolved.size,
            binding: None,
            retained_digest: None,
            cursor: options
                .clone()
                .filter(|_| !combined)
                .map(|options| Cursor::Signatures {
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
                    let mut locked = (options.as_ref().is_some_and(|options| options.free_only)
                        || cursor.as_ref().is_some_and(Cursor::requires_journal_lock))
                    .then(|| platform::StableRead::lock(&source))
                    .transpose()?;
                    let r = if let Some(lock) = &locked {
                        lock.resolved()?
                    } else {
                        platform::resolve(&source, false, false)?
                    };
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
                        let metadata = identify_metadata(&mut reader, r.size, mode)?;
                        let probe_journal = options.is_some()
                            && matches!(metadata, Some(Cursor::Ext { .. }))
                            && crate::ext_recovery::deleted::may_probe(&mut reader)?;
                        if locked.is_none()
                            && (probe_journal
                                || matches!(metadata, Some(Cursor::ExtJournal { .. })))
                        {
                            drop(reader);
                            let lock = platform::StableRead::lock(&source)?;
                            reader = crate::ext_recovery::scan_progress::SharedReader::new(
                                lock.reader()?,
                            );
                            locked = Some(lock);
                        }
                        cursor = if let Some(options) = &options {
                            if metadata.is_none() && options.free_only {
                                return Err("无法确认文件系统，完整恢复的空闲簇范围不可用。".into());
                            }
                            Some(Cursor::Combined {
                                signature_phase: metadata.is_none(),
                                metadata: metadata.map(Box::new),
                                journal: if probe_journal
                                    && crate::ext_recovery::deleted::supports(
                                        reader.clone(),
                                        cancel.clone(),
                                    )? {
                                    Some(Default::default())
                                } else {
                                    None
                                },
                                options: options.clone(),
                                signature_cursor: Default::default(),
                            })
                        } else {
                            metadata
                        };
                    }
                    if cursor.is_some() && binding.is_none() {
                        binding = Some(Binding::capture(&mut reader, r.size, r.offset, sector)?);
                    }
                    {
                        let mut s = state();
                        if matches!(cursor, Some(Cursor::Combined { metadata: None, .. })) {
                            s.scan_mode = ScanMode::Signatures;
                        }
                        if let Some(p) = s.recovery_progress.as_mut().filter(|p| p.id == work_id) {
                            p.cursor = cursor.clone();
                        }
                    }
                    let tick = |done, total| {
                        super::progress(&work_id, &cancel, "文件系统扫描", done, total)
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
                        Some(Cursor::Signatures { options, cursor }) => scan_signatures(
                            &mut reader,
                            r.size,
                            sector,
                            options,
                            cursor,
                            &work_id,
                            &cancel,
                            found,
                        )?,
                        Some(Cursor::Combined {
                            metadata,
                            journal,
                            signature_phase,
                            options,
                            signature_cursor,
                        }) => {
                            if !*signature_phase {
                                let files =
                                    metadata.as_deref_mut().ok_or("完整恢复缺少目录阶段。")?;
                                scan_combined_metadata(
                                    &mut reader,
                                    r.size,
                                    files,
                                    journal.as_mut(),
                                    &cancel,
                                    |phase, done, total| {
                                        super::progress(&work_id, &cancel, phase, done, total)
                                    },
                                    found,
                                    |node, from_journal| {
                                        if from_journal {
                                            state().directories.insert_ext_journal(node)
                                        } else {
                                            directory(node)
                                        }
                                    },
                                )?;
                                binding.as_ref().ok_or("恢复源特征缺失")?.verify(
                                    &mut reader,
                                    r.size,
                                    r.offset,
                                    sector,
                                )?;
                                *signature_phase = true;
                            }
                            let directory_count = metadata.as_deref().map_or(0, Cursor::count);
                            let count = directory_count
                                + journal.as_ref().map_or(0, |cursor| cursor.count());
                            let scanned = scan_signatures(
                                &mut reader,
                                r.size,
                                sector,
                                options,
                                signature_cursor,
                                &work_id,
                                &cancel,
                                |mut candidate| {
                                    candidate.id += count as u32;
                                    found(candidate);
                                },
                            )?;
                            let locale = crate::native_locale::current();
                            let mut directory_summary = if metadata.is_none() {
                                locale
                                    .text("此分区未识别到完整恢复支持的文件记录。")
                                    .to_owned()
                            } else {
                                locale.format(
                                    "文件记录扫描找到 {count} 个候选。",
                                    &[("count", &directory_count.to_string())],
                                )
                            };
                            if let Some(journal) = journal {
                                directory_summary.push(' ');
                                directory_summary.push_str(&journal.summary());
                            }
                            format!("{directory_summary} {scanned}")
                        }
                        Some(cursor) => {
                            scan_metadata(&mut reader, cursor, &cancel, tick, found, directory)?
                        }
                        None => {
                            if disk_core::btrfs::has_magic(&mut reader, r.size)? {
                                drop(reader);
                                let locked = platform::StableRead::lock(&source)?;
                                let message = crate::btrfs_recovery::scan(
                                    locked.reader()?,
                                    mode,
                                    &cancel,
                                    tick,
                                    found,
                                    directory,
                                )?;
                                locked.verify()?;
                                return Ok((message, None));
                            }
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
                    if let Some(lock) = &locked {
                        lock.verify()?;
                    } else {
                        platform::resolve(&source, false, false)?;
                    }
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
        let source = match p.source.clone() {
            ProgressSource::Disk(source) => source,
            ProgressSource::Retained(selection) => return super::retained_signature_tasks::save(selection, p, &files),
        };
        let cursor = p.cursor.ok_or("此扫描引擎尚未支持保存进度")?;
        let name = if matches!(cursor, Cursor::Combined { .. }) {
            "DiskStudio-complete-recovery.dsrecover"
        } else {
            match cursor.kind() {
                "ntfs" => "DiskStudio-NTFS-progress.dsrecover",
                "fat" => "DiskStudio-FAT-progress.dsrecover",
                "exfat" => "DiskStudio-exFAT-progress.dsrecover",
                "ext" => "DiskStudio-Ext-progress.dsrecover",
                "extJournal" => "DiskStudio-Ext4-deleted-progress.dsrecover",
                _ => "DiskStudio-recovery.dsrecover",
            }
        };
        let progress = Progress::capture(
            source.clone(),
            p.binding.ok_or("尚无可保存的源特征")?,
            cursor,
            &files,
            &tree,
        )?;
        let bytes = recovery_checkpoint::encode(progress)?;
        files::save(&ScanSource::Disk(source.clone()), &bytes, name, |readback| {
            recovery_checkpoint::decode(readback, &source).map(|_| ())
        })
    }
}
pub fn load(source: ProgressSource) -> Result<Option<Info>, String> {
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
        let source = match source {
            ProgressSource::Disk(source) => source,
            ProgressSource::Retained(selection) => return super::retained_signature_tasks::load(selection),
        };
        let Some(bytes) = files::load(&ScanSource::Disk(source.clone()))? else {
            return Ok(None);
        };
        let progress = recovery_checkpoint::decode(&bytes, &source)?; // identity/scope before raw open
        let (files, tree) = progress.validate(&source)?;
        let scoped = progress
            .cursor
            .signature_progress()
            .filter(|(options, cursor)| options.free_only && cursor.allocation_digest().is_some());
        let locked = (scoped.is_some() || progress.cursor.requires_journal_lock())
            .then(|| platform::StableRead::lock(&source))
            .transpose()?;
        let r = if let Some(lock) = &locked {
            lock.resolved()?
        } else {
            platform::resolve(&source, false, false)?
        };
        let mut reader =
            crate::ext_recovery::scan_progress::SharedReader::new(platform::reader(&r)?);
        let sector = r.disk.logical_sector_size.ok_or("逻辑扇区未知")?;
        progress
            .binding
            .verify(&mut reader, r.size, r.offset, sector)?;
        progress.verify_owned(reader.clone(), r.size)?;
        if let Some((options, cursor)) = scoped {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let plan = crate::used_space::inspect(&mut reader, r.size, sector, None, &cancel)?;
            cursor.verify_scope(options, &plan)?;
            crate::signature_scan::verify_free_candidates(&plan, &files)?;
        }
        if let Some(lock) = &locked {
            lock.verify()?;
        } else {
            platform::resolve(&source, false, false)?;
        }
        drop(reader);
        let id = uuid::Uuid::new_v4().to_string();
        let c = &progress.cursor;
        let sealed = c.sealed();
        let mode = c.mode();
        let combined = matches!(c, Cursor::Combined { .. });
        {
            let mut s = state();
            idle(&s)?;
            s.candidates = files;
            s.directories = tree;
            s.scan = Some((id.clone(), source.clone().into()));
            s.scan_name = r.label;
            s.scan_mode = if matches!(c, Cursor::Combined { metadata: None, .. }) {
                ScanMode::Signatures
            } else {
                mode
            };
            s.scan_completion = if sealed {
                crate::directory_report::Completion::Ended
            } else {
                crate::directory_report::Completion::Paused
            };
            s.pending = None;
            s.job=Some(super::Job{id:id.clone(),title:if mode==ScanMode::Existing{"读取分区目录"}else if combined{"完整恢复"}else{"文件恢复扫描"}.into(),state:if sealed{"completed"}else{"paused"}.into(),phase:"已加载进度".into(),done:c.done(),total:c.total(r.size)?,found:s.candidates.len(),message:"已核对磁盘、布局、内容特征和续扫边界。结果和目录是历史观察；少量特征不能证明源内容全部未变，预览与提取会重新核对候选。加载不会自动继续扫描。".into(),sha256:None});
            s.recovery_progress = Some(Saved {
                id,
                source: ProgressSource::Disk(source),
                mode,
                options: c.options(),
                size: r.size,
                binding: Some(progress.binding),
                retained_digest: None,
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
pub(super) mod files {
    use super::*;
    use crate::data_windows as platform;
    use cap_std::fs::OpenOptionsExt;
    use cap_std::fs::{Dir, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    fn selected(
        path: &std::path::Path,
        source: &ScanSource,
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
        let parent = source.destination(path.parent().ok_or("文件目录无效")?, size)?;
        let dir = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(platform::err)?;
        let location = platform::file_location(&dir.try_clone().map_err(platform::err)?.into_std_file())?;
        if source.forbidden_disks().contains(&location.target)
            || source.forbidden_disk_numbers().contains(&location.disk_number())
        {
            return Err("输出必须位于另一块物理磁盘，不能写回恢复/备份源盘。".into());
        }
        Ok((dir, name.into()))
    }
    pub fn load(source: &ScanSource) -> Result<Option<Vec<u8>>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text(
                "加载所选扫描范围的恢复进度",
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
        source: &ScanSource,
        bytes: &[u8],
        default_name: &str,
        verify: impl FnOnce(&[u8]) -> Result<(), String>,
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
        verify(&readback)?;
        crate::recovery_output::finish(&file, &dir, &name)
            .map_err(|e| format!("{e}；未完成文件保留为 {partial}"))?;
        Ok(Some(name))
    }
}

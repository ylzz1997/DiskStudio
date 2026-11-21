//! File benchmarks bind a native selection to an online data partition.
//! No raw disk handle is opened and no user file is opened for writing.
use super::{
    canceled, data_tasks, device, digest, engine, model, partition_problem, phase, random, Session,
};
use crate::{native_locale, partitions::policy, surface_scan::Range};
use cap_std::fs::{Dir, OpenOptions as DirOptions, OpenOptionsExt as DirOptionsExt};
use model::{Operation, Preview, Report, Request};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::*};
#[path = "maintenance_file_queue_windows.rs"]
mod queued_reads;

enum Selection {
    Existing(File),
    Directory(Dir),
}

pub(super) struct Prepared {
    selection: Selection,
    resolved: device::Resolved,
    location: device::FileLocation,
}

fn resolve(source: &data_tasks::Selection, write: bool) -> Result<device::Resolved, String> {
    let number = source.partition.ok_or("请选择一个联机的普通数据分区。")?;
    let resolved = device::resolve(source, true, false)?;
    let session = Session::open()?;
    let disk = policy::target_disk(&session.disks, &source.target)?;
    if digest(disk)? != digest(&resolved.disk)?
        || disk.is_offline != Some(false)
        || disk.partition_style != Some(2)
        || write && disk.is_read_only != Some(false)
        || resolved.volumes.len() != 1
        || !resolved.partition_filesystem(number).is_some_and(|fs| {
            ["NTFS", "FAT32", "exFAT"]
                .iter()
                .any(|name| fs.eq_ignore_ascii_case(name))
        })
    {
        return Err("文件测速需要联机的普通 NTFS、FAT32 或 exFAT 数据分区。".into());
    }
    let parts = policy::layout(disk, &session.partitions)?;
    resolved.verify_partition_snapshot(disk, &parts)?;
    let part = parts
        .iter()
        .find(|part| part.partition_number == number)
        .ok_or("所选分区已不存在")?;
    if let Some(reason) = partition_problem(part, disk) {
        return Err(reason);
    }
    Ok(resolved)
}

fn check_entry(handle: &impl AsRawHandle, directory: bool) -> Result<(), String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the retained handle and output structure are valid for this metadata query.
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(device::err(std::io::Error::last_os_error()));
    }
    let unsupported = FILE_ATTRIBUTE_REPARSE_POINT
        | FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_SPARSE_FILE
        | FILE_ATTRIBUTE_COMPRESSED
        | FILE_ATTRIBUTE_ENCRYPTED;
    if (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || info.dwFileAttributes & unsupported != 0
    {
        return Err(
            "文件测速仅支持普通本地文件或目录，不能使用链接、压缩、加密、稀疏或按需下载条目。"
                .into(),
        );
    }
    Ok(())
}

fn check_alignment(file: &File) -> Result<(), String> {
    let mut alignment = FILE_ALIGNMENT_INFO::default();
    // SAFETY: the output is the matching FileAlignmentInfo structure.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAlignmentInfo,
            (&mut alignment as *mut FILE_ALIGNMENT_INFO).cast(),
            std::mem::size_of_val(&alignment) as u32,
        )
    } == 0
        || !alignment
            .AlignmentRequirement
            .checked_add(1)
            .is_some_and(|bytes| bytes.is_power_of_two() && bytes <= 4096)
    {
        return Err("无法确认测速文件符合 4 KiB 无缓存缓冲区对齐要求。".into());
    }
    Ok(())
}

fn location(
    handle: &impl AsRawHandle,
    source: &data_tasks::Selection,
    resolved: &device::Resolved,
) -> Result<device::FileLocation, String> {
    let location = device::file_location(handle)?;
    location.confirm_partition(
        &source.target,
        &source.layout_key,
        &resolved.volumes[0],
        (resolved.disk.number, resolved.offset, resolved.size),
    )?;
    Ok(location)
}

fn space(volume: &str, required: u64) -> Result<(), String> {
    let root = volume
        .encode_utf16()
        .chain([b'\\' as u16, 0])
        .collect::<Vec<_>>();
    let mut available = 0;
    // SAFETY: the null-terminated path comes from the verified volume GUID.
    if unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
        || available < required + 16 * engine::BLOCK as u64
    {
        return Err("测试卷可用空间不足。".into());
    }
    Ok(())
}

struct Plan<'a> {
    first: &'a engine::BenchmarkOptions,
    remaining: &'a [engine::BenchmarkOptions],
    rounds: u8,
    range: Range,
    zero_fill: bool,
}

impl<'a> Plan<'a> {
    fn new(request: &'a Request) -> Result<Self, String> {
        let Operation::FileBenchmark {
            options,
            additional_workloads,
            repetitions,
            file_size,
            zero_fill,
        } = &request.operation
        else {
            return Err("文件测速确认不匹配。".into());
        };
        if additional_workloads.len() > 3 || !(1..=5).contains(repetitions) {
            return Err("文件测速支持 1–4 组负载和 1–5 轮。".into());
        }
        let mib = engine::BLOCK as u64;
        if request.range.is_some()
            || !(mib..=4095 * mib).contains(file_size)
            || file_size % mib != 0
        {
            return Err("文件测速大小须为 1–4095 MiB 的整数，且不能指定原始扇区范围。".into());
        }
        let plan = Self {
            first: options,
            remaining: additional_workloads,
            rounds: *repetitions,
            range: Range {
                offset: 0,
                size: *file_size,
                sector: 4096,
            },
            zero_fill: *zero_fill,
        };
        if plan.zero_fill && !plan.writes() {
            return Err("测试数据填充仅适用于包含写入的文件测速。".into());
        }
        for workload in plan.workloads() {
            workload.validate(plan.range)?;
        }
        Ok(plan)
    }

    fn workloads(&self) -> impl Iterator<Item = &'a engine::BenchmarkOptions> + '_ {
        std::iter::once(self.first).chain(self.remaining)
    }

    fn writes(&self) -> bool {
        self.workloads().any(|options| options.write)
    }
}

fn retained_location(
    selection: &Selection,
    source: &data_tasks::Selection,
    resolved: &device::Resolved,
    size: u64,
) -> Result<device::FileLocation, String> {
    match selection {
        Selection::Existing(file) => {
            check_entry(file, false)?;
            if file.metadata().map_err(device::err)?.len() < size {
                return Err("选定文件小于测速大小。".into());
            }
            location(file, source, resolved)
        }
        Selection::Directory(dir) => {
            check_entry(dir, true)?;
            let location = location(dir, source, resolved)?;
            space(&resolved.volumes[0], size)?;
            Ok(location)
        }
    }
}

pub(super) fn prepare(
    request: &Request,
    cancel: &AtomicBool,
) -> Result<(Preview, Prepared), String> {
    let plan = Plan::new(request)?;
    let range = plan.range;
    let writes = plan.writes();
    let resolved = resolve(&request.source, writes)?;
    canceled(cancel)?;
    let dialog = rfd::FileDialog::new().set_title(native_locale::text(if writes {
        "选择测速临时文件所在目录"
    } else {
        "选择要读取测速的文件"
    }));
    let path = if writes {
        dialog.pick_folder()
    } else {
        dialog.pick_file()
    }
    .ok_or("已取消文件测速选择。")?;
    canceled(cancel)?;
    if !path.has_root()
        || !matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err("文件测速需要本地盘符下的普通文件或目录。".into());
    }
    let selection = if writes {
        Selection::Directory(
            Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(device::err)?,
        )
    } else {
        Selection::Existing(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)
                .map_err(device::err)?,
        )
    };
    let location = retained_location(&selection, &request.source, &resolved, range.size)?;
    canceled(cancel)?;
    let preview = Preview {
        token: uuid::Uuid::new_v4().to_string(),
        title: request.operation.title().into(),
        target: format!("{} · {}", resolved.label, path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()),
        offset: range.offset,
        size: range.size,
        sector_size: range.sector,
        destructive: false,
        requires_acknowledgment: request.operation.requires_acknowledgment(),
        warning: if writes {
            "会先写入并校验新建临时文件。测速只修改该文件，结束后清理。临时文件占用空间，仍可能覆盖可恢复的已删除数据。"
        } else {
            "测量选定文件从开头起指定大小的范围，绕过 Windows 文件缓存。"
        }.into(),
    };
    Ok((
        preview,
        Prepared {
            selection,
            resolved,
            location,
        },
    ))
}

fn fill(bytes: &mut [u8], zero: bool) -> Result<(), String> {
    if zero {
        bytes.fill(0);
        Ok(())
    } else {
        random(bytes)
    }
}

fn initialize(file: &mut File, size: u64, zero: bool, cancel: &AtomicBool) -> Result<(), String> {
    let mut block = engine::buffer();
    let mut written = Sha256::new();
    let mut verified = Sha256::new();
    for _ in 0..size / engine::BLOCK as u64 {
        canceled(cancel)?;
        fill(&mut block.0, zero)?;
        if file.write(&block.0).map_err(device::err)? != block.0.len() {
            return Err("临时文件初始化写入不完整。".into());
        }
        written.update(&block.0);
    }
    file.sync_all().map_err(device::err)?;
    file.seek(SeekFrom::Start(0)).map_err(device::err)?;
    for _ in 0..size / engine::BLOCK as u64 {
        canceled(cancel)?;
        if file.read(&mut block.0).map_err(device::err)? != block.0.len() {
            return Err("临时文件初始化读取不完整。".into());
        }
        verified.update(&block.0);
    }
    if written.finalize() != verified.finalize()
        || file.metadata().map_err(device::err)?.len() != size
    {
        return Err("临时文件初始化 SHA-256 校验失败。".into());
    }
    Ok(())
}

pub(super) fn execute(
    prepared: &Prepared,
    request: &Request,
    expected: Range,
    report: &mut Report,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let plan = Plan::new(request)?;
    let test = plan.range;
    let resolved = resolve(&request.source, plan.writes())?;
    if test != expected
        || !prepared.resolved.same_data_binding(&resolved)
        || retained_location(&prepared.selection, &request.source, &resolved, test.size)?
            != prepared.location
    {
        return Err("文件测速的文件、卷或磁盘布局已变化。".into());
    }
    canceled(cancel)?;
    match &prepared.selection {
        Selection::Existing(file) => {
            let mut measurement_file = reopen_reader(file, FILE_SHARE_READ)?;
            if measurement_file.metadata().map_err(device::err)?.len() < test.size {
                return Err("选定文件小于测速大小。".into());
            }
            run_workloads(&mut measurement_file, false, &plan, report, cancel)?;
            if retained_location(&prepared.selection, &request.source, &resolved, test.size)?
                != prepared.location
            {
                return Err("测速后文件所在卷或磁盘布局已变化。".into());
            }
            Ok(
                "读取测速完成。吞吐量和 IOPS 按整段测量时间计算。设备和虚拟化缓存可能影响结果。"
                    .into(),
            )
        }
        Selection::Directory(dir) => {
            // A single generated leaf under the retained directory, with exclusive
            // create and kernel cleanup on last close, including application exit.
            let name = format!("DiskStudio-benchmark-{}.tmp", uuid::Uuid::new_v4());
            let mut file = dir
                .open_with(
                    &name,
                    DirOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        // The relative NtCreateFile call cannot request append
                        // access together with unbuffered I/O.
                        .access_mode(FILE_GENERIC_READ | FILE_WRITE_DATA | DELETE)
                        .share_mode(if plan.workloads().any(|option| !option.write) {
                            FILE_SHARE_READ
                        } else {
                            0
                        })
                        .custom_flags(
                            FILE_FLAG_DELETE_ON_CLOSE
                                | FILE_FLAG_NO_BUFFERING
                                | FILE_FLAG_WRITE_THROUGH,
                        ),
                )
                .map_err(|error| format!("创建测速临时文件失败。 {error}"))?
                .into_std();
            let measured = (|| {
                check_entry(&file, false)?;
                check_alignment(&file)?;
                if location(&file, &request.source, &resolved)? != prepared.location {
                    return Err("临时文件未在确认的测速卷上创建。".into());
                }
                phase(report, "写入并校验测速临时文件");
                initialize(&mut file, test.size, plan.zero_fill, cancel)?;
                run_workloads(&mut file, true, &plan, report, cancel)
            })();
            drop(file);
            phase(report, "核对测速临时文件已清理");
            if !matches!(dir.symlink_metadata(Path::new(&name)), Err(error) if error.kind() == ErrorKind::NotFound)
            {
                return Err(format!("无法确认测速临时文件已清理。 {name}"));
            }
            measured?;
            if location(dir, &request.source, &resolved)? != prepared.location {
                return Err("测速后目录所在卷或磁盘布局已变化。".into());
            }
            Ok("文件测速完成。SHA-256 校验通过，临时文件已清理，卷保持联机。".into())
        }
    }
}

fn reopen_reader(file: &File, share_mode: u32) -> Result<File, String> {
    // SAFETY: reopen the object referenced by the retained handle. No path is
    // resolved again, and the new handle can only read.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            FILE_GENERIC_READ,
            share_mode,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(device::err(std::io::Error::last_os_error()));
    }
    // SAFETY: successful ReOpenFile returned a separate owned handle.
    let read = unsafe { File::from_raw_handle(handle) };
    check_alignment(&read)?;
    Ok(read)
}

fn run_workloads(
    file: &mut File,
    scratch: bool,
    plan: &Plan<'_>,
    report: &mut Report,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let single = plan.rounds == 1 && plan.remaining.is_empty();
    for round in 1..=plan.rounds {
        for (index, options) in plan.workloads().enumerate() {
            canceled(cancel)?;
            let stage = report.file_benchmark_runs.len();
            report.file_benchmark_runs.push(model::FileBenchmarkRun {
                round,
                workload: index as u8 + 1,
                state: "running",
                stats: None,
            });
            let label = if options.write {
                "文件读写、刷新与回读"
            } else {
                "无缓存读取"
            };
            phase(report, label);
            let mut stats = engine::BenchmarkProgress::default();
            let publish = |progress: &engine::BenchmarkProgress| {
                report.file_benchmark_runs[stage].stats = Some(progress.clone());
                model::publish(report);
                let (done, total) = options.progress(progress);
                data_tasks::record_progress(&report.id, label, done, total);
            };
            let result = if options.write {
                // A suite containing any write always receives a new scratch file.
                if !scratch {
                    return Err("文件写入测速缺少专用临时文件。".into());
                }
                engine::benchmark(
                    file,
                    plan.range,
                    options,
                    cancel,
                    &mut stats,
                    random,
                    |bytes| fill(bytes, plan.zero_fill),
                    |file| file.sync_all(),
                    publish,
                )
            } else {
                (|| {
                    // The original scratch handle denies other writers and deleters.
                    // A second handle shares its existing write and delete access.
                    let share_mode = if scratch {
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                    } else {
                        FILE_SHARE_READ
                    };
                    let readers = options
                        .queue_sizes()
                        .map(|depth| {
                            canceled(cancel)?;
                            let read = reopen_reader(file, share_mode)?;
                            queued_reads::FileQueue::new(read, depth, options.block_size)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    engine::benchmark_file_queues(
                        readers, plan.range, options, cancel, &mut stats, random, publish,
                    )
                })()
            };
            let run = &mut report.file_benchmark_runs[stage];
            run.state = if result.is_ok() {
                "completed"
            } else if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "canceled"
            } else {
                "failed"
            };
            if single {
                report.benchmark = Some(stats.clone());
            }
            run.stats = Some(stats);
            model::publish(report);
            result?;
        }
    }
    Ok(())
}

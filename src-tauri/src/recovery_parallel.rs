//! Bounded extraction workers. The coordinator owns directory creation,
//! publication, verification and the report. No device access in this module.
use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Mutex,
};
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "recovery_parallel_tests.rs"]
mod tests;

pub fn execute_parallel<S: Sink>(
    plan: &Plan,
    sink: &mut S,
    extract: impl Fn(&Entry, &mut S::Output, &mut dyn FnMut(u64) -> Result<(), String>) -> Result<u64, String>
        + Sync,
    tick: impl Fn(u64) -> Result<(), String> + Sync,
) -> Result<Vec<Outcome>, String>
where
    S::Output: Send,
{
    execute_parallel_with_directories(
        plan,
        sink,
        extract,
        |_| Err("本次提取没有目录记录复核器。".into()),
        tick,
    )
    .map(|batch| batch.files)
}

pub fn execute_parallel_with_directories<S: Sink>(
    plan: &Plan,
    sink: &mut S,
    extract: impl Fn(&Entry, &mut S::Output, &mut dyn FnMut(u64) -> Result<(), String>) -> Result<u64, String>
        + Sync,
    directory_metadata: impl Fn(
        &crate::recovery_tree::Directory,
    ) -> Result<crate::file_metadata::Metadata, String>,
    tick: impl Fn(u64) -> Result<(), String> + Sync,
) -> Result<BatchOutcome, String>
where
    S::Output: Send,
{
    let workers = plan
        .options
        .validate()?
        .workers
        .min(plan.entries.len().max(1));
    tick(0)?;
    sink.report(&plan.manifest(&[], false)?, false)?;
    let mut directories = vec![];
    for index in 1..plan.folders.len() {
        let result = tick(0).and_then(|_| sink.directory(plan, index));
        let failure = result.as_ref().err().cloned();
        directories.push(FolderOutcome {
            folder: index,
            created: result.is_ok(),
            error: result.err().map(|e| truncate(&e, 256)),
            metadata: None,
        });
        if let Some(error) = failure {
            let saved = sink.report(
                &plan.manifest_with_folders(&[], &directories, false)?,
                false,
            );
            return Err(format!(
                "创建目录未完成：{error}；已保留部分目录及清单。{}",
                saved.err().unwrap_or_default()
            ));
        }
    }
    let copied = AtomicU64::new(0);
    let stopped = AtomicBool::new(false);
    let stop_reason = Mutex::new(None::<String>);
    // Serialize only progress notifications; all readers and output writers
    // remain independent. A late callback cannot regress visible progress.
    let notification = Mutex::new(());
    let notify = || -> Result<(), String> {
        let _guard = notification.lock().map_err(|_| "提取进度锁失效")?;
        if stopped.load(Ordering::Acquire) {
            return Err("提取已停止，保留未完成文件。".into());
        }
        if let Err(error) = tick(copied.load(Ordering::Acquire)) {
            if let Ok(mut reason) = stop_reason.lock() {
                *reason = Some(error.clone());
            }
            stopped.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    };
    let mut outcomes = vec![];
    let mut report_error = None;
    std::thread::scope(|scope| {
        let (finished, results) = mpsc::sync_channel(workers);
        let mut senders = vec![];
        for worker in 0..workers {
            let (send, receive) = mpsc::sync_channel::<(usize, S::Output)>(1);
            senders.push(send);
            let finished = finished.clone();
            let extract = &extract;
            let notify = &notify;
            let copied = &copied;
            scope.spawn(move || {
                while let Ok((index, mut output)) = receive.recv() {
                    let entry = &plan.entries[index];
                    let mut reported = 0;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        notify()?;
                        let mut progress = |n: u64| -> Result<(), String> {
                            if n < reported || n > entry.size {
                                return Err("提取进度超出文件范围。".into());
                            }
                            copied.fetch_add(n - reported, Ordering::AcqRel);
                            reported = n;
                            notify()
                        };
                        let bytes = extract(entry, &mut output, &mut progress)?;
                        if bytes != entry.size {
                            return Err("提取字节数与候选容量不符。".into());
                        }
                        progress(bytes)?;
                        Ok(())
                    }))
                    .unwrap_or_else(|_| Err("提取工作线程异常，已保留未完成文件。".into()));
                    if finished.send((worker, index, output, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(finished);
        let mut idle = (0..workers).collect::<Vec<_>>();
        let mut next = 0;
        let mut active = 0;
        let mut checkpoint = Instant::now();
        loop {
            while !stopped.load(Ordering::Acquire) && next < plan.entries.len() && !idle.is_empty()
            {
                if notify().is_err() {
                    break;
                }
                let index = next;
                next += 1;
                let entry = &plan.entries[index];
                match sink.create(plan, entry) {
                    Err(error) => outcomes.push(Outcome {
                        id: entry.id,
                        saved: false,
                        error: Some(truncate(&error, 256)),
                        sha256: None,
                        metadata: None,
                    }),
                    Ok(output) => {
                        let worker = idle.pop().unwrap();
                        if senders[worker].send((index, output)).is_err() {
                            stopped.store(true, Ordering::Release);
                            report_error = Some("提取队列异常，保留未完成文件。".to_string());
                            break;
                        }
                        active += 1;
                    }
                }
            }
            if active == 0 {
                break;
            }
            let received = match results.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => value,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = notify();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stopped.store(true, Ordering::Release);
                    report_error = Some("提取队列提前结束。".into());
                    break;
                }
            };
            let (worker, index, output, result) = received;
            active -= 1;
            idle.push(worker);
            let entry = &plan.entries[index];
            let result = result
                .and_then(|_| notify())
                .and_then(|_| sink.commit(entry, output, &mut || notify()));
            outcomes.push(Outcome {
                id: entry.id,
                saved: result.is_ok(),
                sha256: result.as_ref().ok().cloned(),
                error: result.err().map(|e| truncate(&e, 256)),
                metadata: sink.metadata_report(entry.id),
            });
            if checkpoint.elapsed() >= Duration::from_secs(5) {
                let result = plan
                    .manifest_with_folders(&outcomes, &directories, false)
                    .and_then(|bytes| sink.report(&bytes, false));
                if let Err(error) = result {
                    stopped.store(true, Ordering::Release);
                    report_error = Some(error);
                }
                checkpoint = Instant::now();
            }
        }
        drop(senders); // All worker threads are joined before final reporting.
    });
    outcomes.sort_by_key(|o| o.id);
    // All data writers are closed. Plan folders are parent-before-child;
    // restore child metadata first so later child creation cannot reset it.
    for outcome in directories.iter_mut().rev() {
        let Some(source) = plan.folders[outcome.folder].source.as_ref() else {
            continue;
        };
        let report = if !plan.options.timestamps && !plan.options.attributes {
            crate::export_metadata::unmodified(&source.metadata, plan.options, None)
        } else {
            match notify()
                .and_then(|_| directory_metadata(source))
                .and_then(|metadata| {
                    notify()?;
                    Ok(metadata)
                }) {
                Ok(metadata) => sink.preserve_directory(plan, outcome.folder, &metadata),
                Err(error) => crate::export_metadata::unmodified(
                    &source.metadata,
                    plan.options,
                    Some(truncate(&error, 256)),
                ),
            }
        };
        outcome.metadata = Some(report);
    }
    let final_tick = notify();
    let complete = !stopped.load(Ordering::Acquire)
        && report_error.is_none()
        && final_tick.is_ok()
        && outcomes.len() == plan.entries.len();
    let report = sink.report(
        &plan.manifest_with_folders(&outcomes, &directories, complete)?,
        complete,
    );
    if !complete {
        let reason = report_error
            .or_else(|| stop_reason.into_inner().ok().flatten())
            .or_else(|| final_tick.err())
            .unwrap_or_else(|| "提取未完成".into());
        return Err(format!(
            "{reason}；已保存 {} 个文件，其余失败、未处理和 partial 文件见清单。{}",
            outcomes.iter().filter(|o| o.saved).count(),
            report.err().unwrap_or_default()
        ));
    }
    report?;
    Ok(BatchOutcome {
        files: outcomes,
        directories,
    })
}

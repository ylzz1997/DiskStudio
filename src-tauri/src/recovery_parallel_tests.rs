use super::*;
use std::sync::{atomic::AtomicUsize, Arc, Barrier};

fn plan(count: usize, workers: usize) -> Plan {
    Plan {
        options: crate::export_metadata::Options {
            workers,
            ..Default::default()
        },
        layout: Layout::Flat,
        folders: vec![Folder {
            parent: None,
            original_name: "root".into(),
            name: String::new(),
            unknown: false,
            source: None,
        }],
        entries: (0..count)
            .map(|i| Entry {
                id: i as u32,
                folder: 0,
                original_name: format!("f{i}"),
                name: format!("f{i}"),
                partial: format!("f{i}.partial"),
                size: 8,
            })
            .collect(),
        renamed: 0,
        unknown_files: 0,
        total_bytes: count as u64 * 8,
        directory_selection: false,
        skipped: vec![],
        backup_source: None,
        image_source: None,
        image_lvm_source: None,
        physical_lvm_source: None,
        virtual_raid_source: None,
        lvm_degraded_raid: false,
    }
}
#[derive(Default)]
struct Memory {
    created: Vec<u32>,
    saved: HashMap<u32, Vec<u8>>,
    report: Vec<u8>,
    fail_create: Option<u32>,
    fail_commit: Option<u32>,
    directory_order: Vec<usize>,
}
impl Sink for Memory {
    type Output = Vec<u8>;
    fn directory(&mut self, _: &Plan, _: usize) -> Result<(), String> {
        Ok(())
    }
    fn create(&mut self, _: &Plan, e: &Entry) -> Result<Vec<u8>, String> {
        self.created.push(e.id);
        if self.fail_create == Some(e.id) {
            return Err("create failed".into());
        }
        Ok(vec![])
    }
    fn commit(
        &mut self,
        e: &Entry,
        out: Vec<u8>,
        tick: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<String, String> {
        tick()?;
        if self.fail_commit == Some(e.id) {
            return Err("verification failed".into());
        }
        if out.len() != 8 || out != vec![e.id as u8; 8] {
            return Err("contents differ".into());
        }
        if self.saved.contains_key(&e.id) {
            return Err("would overwrite".into());
        }
        self.saved.insert(e.id, out);
        Ok(format!("{:064x}", e.id))
    }
    fn report(&mut self, bytes: &[u8], _: bool) -> Result<(), String> {
        self.report = bytes.into();
        Ok(())
    }
    fn preserve_directory(
        &mut self,
        plan: &Plan,
        index: usize,
        metadata: &crate::file_metadata::Metadata,
    ) -> crate::export_metadata::Report {
        assert_eq!(self.saved.len(), plan.entries.len());
        self.directory_order.push(index);
        crate::export_metadata::unmodified(metadata, plan.options, None)
    }
}
fn directory_plan() -> Plan {
    let mut p = plan(1, 2);
    for (index, parent) in [(1, 0), (2, 1), (3, 2)] {
        p.folders.push(Folder {
            parent: Some(parent),
            name: format!("d{index}"),
            original_name: format!("d{index}"),
            unknown: false,
            source: Some(crate::recovery_tree::Directory {
                id: format!("d{index}"),
                name: format!("d{index}"),
                ..Default::default()
            }),
        });
    }
    p.entries[0].folder = 3;
    p
}
#[test]
fn directory_metadata_runs_after_file_commits_child_before_parent_and_reports_failures() {
    let p = directory_plan();
    let mut sink = Memory::default();
    let result = execute_parallel_with_directories(
        &p,
        &mut sink,
        |_, out, tick| {
            out.extend_from_slice(&[0; 8]);
            tick(8)?;
            Ok(8)
        },
        |d| {
            if d.id == "d2" {
                Err("directory changed".into())
            } else {
                Ok(Default::default())
            }
        },
        |_| Ok(()),
    )
    .unwrap();
    assert!(result.files[0].saved);
    assert_eq!(sink.directory_order, vec![3, 1]);
    assert_eq!(
        result.directories[1]
            .metadata
            .as_ref()
            .unwrap()
            .error
            .as_deref(),
        Some("directory changed")
    );
    let report: serde_json::Value = serde_json::from_slice(&sink.report).unwrap();
    assert_eq!(report["completed"], true);
    assert!(report["directoryOutcomes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|o| o["created"] == true));
}
#[test]
fn disabled_directory_preservation_never_reads_source_or_writes_metadata() {
    let mut p = directory_plan();
    p.options.timestamps = false;
    p.options.attributes = false;
    let mut sink = Memory::default();
    let result = execute_parallel_with_directories(
        &p,
        &mut sink,
        |_, out, _| {
            out.extend_from_slice(&[0; 8]);
            Ok(8)
        },
        |_| panic!("unexpected directory source read"),
        |_| Ok(()),
    )
    .unwrap();
    assert!(sink.directory_order.is_empty());
    assert!(result.directories.iter().all(|o| o
        .metadata
        .as_ref()
        .unwrap()
        .fields
        .iter()
        .all(|f| f.status == "disabled")));
}
#[test]
fn cancel_during_directory_phase_prevents_later_reads_and_metadata_writes() {
    let canceled = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);
    let mut sink = Memory::default();
    let result = execute_parallel_with_directories(
        &directory_plan(),
        &mut sink,
        |_, out, _| {
            out.extend_from_slice(&[0; 8]);
            Ok(8)
        },
        |_| {
            reads.fetch_add(1, Ordering::SeqCst);
            canceled.store(true, Ordering::SeqCst);
            Ok(Default::default())
        },
        |_| {
            if canceled.load(Ordering::SeqCst) {
                Err("cancel directory metadata".into())
            } else {
                Ok(())
            }
        },
    );
    assert!(result.err().unwrap().contains("cancel directory metadata"));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(sink.directory_order.is_empty());
    assert_eq!(sink.saved.len(), 1);
}
#[test]
fn independent_files_overlap_with_bounded_workers_and_monotonic_progress() {
    for workers in [1, 2, 4] {
        let plan = plan(workers * 2, workers);
        let mut sink = Memory::default();
        let barrier = Barrier::new(workers);
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let observed = Mutex::new(vec![]);
        let outcomes = execute_parallel(
            &plan,
            &mut sink,
            |entry, out, tick| {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(n, Ordering::SeqCst);
                barrier.wait();
                out.extend_from_slice(&[entry.id as u8; 4]);
                tick(4)?;
                out.extend_from_slice(&[entry.id as u8; 4]);
                tick(8)?;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(8)
            },
            |n| {
                observed.lock().unwrap().push(n);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), workers);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(outcomes.iter().all(|o| o.saved));
        assert_eq!(sink.saved.len(), workers * 2);
        let progress = observed.into_inner().unwrap();
        assert!(progress.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*progress.last().unwrap(), plan.total_bytes);
        let report: serde_json::Value = serde_json::from_slice(&sink.report).unwrap();
        assert_eq!(report["completed"], true);
        assert_eq!(report["unprocessedFiles"], 0);
    }
}
#[test]
fn cancellation_stops_dispatch_waits_for_workers_and_never_publishes_unverified_output() {
    let mut sink = Memory::default();
    let canceled = Arc::new(AtomicBool::new(false));
    let finished = AtomicBool::new(false);
    let error = execute_parallel(
        &plan(5, 1),
        &mut sink,
        |_, out, tick| {
            out.extend_from_slice(b"part");
            canceled.store(true, Ordering::SeqCst);
            let result = tick(4);
            finished.store(true, Ordering::SeqCst);
            result?;
            Ok(8)
        },
        |_| {
            if canceled.load(Ordering::SeqCst) {
                Err("explicit cancellation".into())
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert!(error.contains("explicit cancellation"));
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(sink.created, vec![0]);
    assert!(sink.saved.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&sink.report).unwrap();
    assert_eq!(report["completed"], false);
    assert_eq!(report["unprocessedFiles"], 4);
    assert_eq!(report["failedFiles"], 1);
}
#[test]
fn cancellation_joins_every_inflight_reader_before_returning() {
    let mut sink = Memory::default();
    let barrier = Barrier::new(4);
    let canceled = AtomicBool::new(false);
    let finished = AtomicUsize::new(0);
    let error = execute_parallel(
        &plan(12, 4),
        &mut sink,
        |_, out, tick| {
            out.extend_from_slice(b"part");
            barrier.wait();
            canceled.store(true, Ordering::SeqCst);
            let result = tick(4);
            finished.fetch_add(1, Ordering::SeqCst);
            result?;
            Ok(8)
        },
        |_| {
            if canceled.load(Ordering::SeqCst) {
                Err("stop all readers".into())
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert!(error.contains("stop all readers"));
    assert_eq!(finished.load(Ordering::SeqCst), 4);
    assert_eq!(sink.created.len(), 4);
    assert!(sink.saved.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&sink.report).unwrap();
    assert_eq!(report["failedFiles"], 4);
    assert_eq!(report["unprocessedFiles"], 8);
    assert_eq!(report["completed"], false);
}
#[test]
fn individual_errors_short_reads_and_panics_do_not_hide_other_results_or_overwrite() {
    let mut sink = Memory {
        fail_create: Some(2),
        fail_commit: Some(3),
        ..Default::default()
    };
    sink.saved.insert(4, b"original".to_vec());
    let result = execute_parallel(
        &plan(6, 2),
        &mut sink,
        |entry, out, tick| {
            if entry.id == 0 {
                panic!("synthetic extractor failure");
            }
            if entry.id == 1 {
                out.extend_from_slice(b"short");
                return Ok(5);
            }
            out.extend_from_slice(&[entry.id as u8; 8]);
            tick(8)?;
            Ok(8)
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.iter().filter(|o| o.saved).count(), 1);
    assert_eq!(result[5].id, 5);
    assert!(result[5].saved);
    assert_eq!(sink.saved[&4], b"original");
    assert_eq!(sink.saved[&5], [5; 8]);
    let report: serde_json::Value = serde_json::from_slice(&sink.report).unwrap();
    assert_eq!(report["failedFiles"], 5);
    assert_eq!(report["completed"], true);
}

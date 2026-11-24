//! Resumable signature scans of already opened read-only images and backup points.
use super::*;
use crate::{recovery_checkpoint::Cursor, signature_checkpoint::Binding, signature_scan};
use recovery_tasks::{ProgressSource, Saved};

#[path = "retained_signature_checkpoint.rs"]
mod checkpoint;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Image,
    Backup,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    kind: Kind,
    id: String,
    partition: Option<u32>,
}

impl Selection {
    pub(super) fn matches(&self, source: &ScanSource) -> bool {
        match (self.kind, source) {
            (Kind::Image, ScanSource::Image { opened, partition }) => {
                self.id == opened.browser.info.id && self.partition == *partition
            }
            (Kind::Backup, ScanSource::Backup { opened, partition }) => {
                self.id == opened.browser.info.id && self.partition == *partition
            }
            _ => false,
        }
    }

    #[cfg(windows)]
    fn resolve(&self) -> Result<(ScanSource, Geometry), String> {
        let part = self.partition;
        match self.kind {
            Kind::Image => {
                let opened = image_browse_tasks::opened(&self.id)?;
                let info = &opened.browser.info;
                let (offset, size) = match part {
                    Some(id) => {
                        let part = opened.browser.part(id)?;
                        (part.start, part.size)
                    }
                    None if info.options.kind != crate::image_browser::Kind::Partition => {
                        (0, info.size)
                    }
                    None => return Err("请选择要扫描的镜像分区。".into()),
                };
                let geometry = Geometry {
                    offset,
                    size,
                    sector: info.options.sector,
                };
                Ok((
                    ScanSource::Image {
                        opened,
                        partition: part,
                    },
                    geometry,
                ))
            }
            Kind::Backup => {
                let opened = state()
                    .opened_backup
                    .as_ref()
                    .filter(|backup| backup.browser.info.id == self.id)
                    .cloned()
                    .ok_or("备份浏览会话已失效")?;
                let (offset, size) = match part {
                    Some(id) => {
                        let part = opened
                            .browser
                            .info
                            .partitions
                            .iter()
                            .find(|p| p.id == id)
                            .ok_or("备份分区选择无效")?;
                        (part.start, part.size)
                    }
                    None => (0, opened.browser.restore_size(None)?),
                };
                let geometry = Geometry {
                    offset,
                    size,
                    sector: opened.browser.info.sector,
                };
                Ok((
                    ScanSource::Backup {
                        opened,
                        partition: part,
                    },
                    geometry,
                ))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    kind: Kind,
    id: String,
    partition: Option<u32>,
    options: signature_scan::Options,
}

#[derive(Clone, Copy)]
struct Geometry {
    offset: u64,
    size: u64,
    sector: u64,
}

impl Geometry {
    fn validate(self, options: &signature_scan::Options, is_partition: bool) -> Result<(), String> {
        if options.free_only && !is_partition {
            return Err("仅空闲簇扫描需要已核对的分区分配信息。".into());
        }
        options.range(self.size)?;
        options.alignment(self.sector)?;
        Ok(())
    }

    fn verify(
        self,
        reader: &mut (impl Read + Seek),
        binding: &Binding,
        options: &signature_scan::Options,
        cursor: &signature_scan::Checkpoint,
        files: &[Candidate],
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        binding.verify(reader, self.size, self.offset, self.sector)?;
        cursor.verify_cursor(reader, options)?;
        if cursor.allocation_digest().is_some() {
            let plan = crate::used_space::inspect(reader, self.size, self.sector, None, cancel)?;
            cursor.verify_scope(options, &plan)?;
            signature_scan::verify_free_candidates(&plan, files)?;
            plan.proof.verify(reader, cancel)?;
        }
        Ok(())
    }
}

const TITLE: &str = "文件签名扫描";

#[cfg(windows)]
fn identity(source: &ScanSource) -> Result<String, String> {
    match source {
        ScanSource::Image { opened, .. } => {
            opened.validate()?;
            let files = opened.group.recovery_identity()?;
            match &opened.browser.info.parallels_snapshot {
                Some(point) if !point.current => Ok(crate::parallels_descriptor::progress_identity(files, point.id)),
                _ => Ok(files),
            }
        }
        ScanSource::Backup { opened, .. } => {
            opened.validate()?;
            opened.browser.recovery_identity()
        }
        _ => Err("镜像或备份的恢复进度来源无效。".into()),
    }
}

#[cfg(windows)]
fn reader(
    source: &ScanSource,
    is_partition: bool,
    cancel: &AtomicBool,
) -> Result<Box<dyn backup_archive::Source>, String> {
    crate::used_space::stop(cancel)?;
    let mut reader = source.reader(cancel)?;
    if is_partition {
        let mut boot = [0; 512];
        reader.read_exact(&mut boot).map_err(platform::err)?;
        if crate::bitlocker::boot_marker(&boot) {
            return Err(crate::bitlocker::CIPHERTEXT_NOTICE.into());
        }
    }
    Ok(reader)
}

pub fn start(request: Request) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("恢复扫描当前仅支持 Windows".into())
    }
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let selection = Selection {
            kind: request.kind,
            id: request.id,
            partition: request.partition,
        };
        let (source, geometry) = selection.resolve()?;
        geometry.validate(&request.options, selection.partition.is_some())?;
        let saved = Saved {
            id: String::new(),
            source: ProgressSource::Retained(selection),
            mode: ScanMode::Signatures,
            options: Some(request.options.clone()),
            size: geometry.size,
            binding: None,
            retained_digest: None,
            cursor: Some(Cursor::Signatures {
                options: request.options,
                cursor: Default::default(),
            }),
            pause_requested: false,
            loaded: false,
        };
        launch(source, geometry, saved, None, guard)
    }
}

pub(super) fn resume(id: String, selection: Selection) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = (id, selection);
        Err("恢复扫描当前仅支持 Windows".into())
    }
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        let (saved, files, _) = recovery_tasks::stopped(&id)?;
        if !matches!(&saved.source, ProgressSource::Retained(source) if source == &selection) {
            return Err("请选择进度对应的原镜像、备份还原点和扫描范围。".into());
        }
        let (source, geometry) = selection.resolve()?;
        let (options, cursor) = cursor(&saved)?;
        if cursor.sealed || geometry.size != saved.size {
            return Err("扫描已结束或源范围改变，不能继续".into());
        }
        let binding = saved.binding.as_ref().ok_or("尚无可保存的源特征")?;
        let records = records(&files)?;
        crate::signature_checkpoint::restore_records(binding, options, cursor, &records)?;
        if let Some(expected) = &saved.retained_digest {
            if identity(&source)? != *expected {
                return Err("请选择进度对应的原镜像、备份还原点和扫描范围。".into());
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = reader(&source, selection.partition.is_some(), &cancel)?;
        geometry.verify(&mut reader, binding, options, cursor, &files, &cancel)?;
        launch(source, geometry, saved, Some(reader), guard)
    }
}

#[cfg(windows)]
fn launch(
    source: ScanSource,
    geometry: Geometry,
    mut saved: Saved,
    verified: Option<Box<dyn backup_archive::Source>>,
    guard: crate::operation_gate::OperationGuard<'_>,
) -> Result<String, String> {
    let label = source.label()?;
    let (options, cursor) = cursor(&saved)?;
    let range = options.range(geometry.size)?;
    let total = options.work(range.end - range.start);
    let options = options.clone();
    let mut cursor = cursor.clone();
    let mut binding = saved.binding.clone();
    let mut fingerprint = saved.retained_digest.clone();
    let ProgressSource::Retained(selection) = &saved.source else {
        return Err("恢复进度已失效".into());
    };
    let is_partition = selection.partition.is_some();
    let (id, cancel) = super::start(TITLE)?;
    saved.id = id.clone();
    saved.pause_requested = false;
    {
        let mut s = state();
        if verified.is_none() {
            s.candidates.clear();
            s.directories = Catalog::default();
        }
        let found = s.candidates.len();
        if let Some(job) = &mut s.job {
            job.done = cursor.work_done(&options);
            job.total = total;
            job.found = found;
        }
        s.scan = Some((id.clone(), source.clone()));
        s.scan_name = label;
        s.scan_mode = ScanMode::Signatures;
        s.scan_completion = crate::directory_report::Completion::Running;
        s.pending = None;
        s.recovery_progress = Some(saved);
    }
    drop(guard);
    let work_id = id.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = crate::OPERATION
                .try_lock()
                .map_err(|_| "另一项磁盘任务正在运行")?;
            let mut reader = match verified {
                Some(reader) => reader,
                None => reader(&source, is_partition, &cancel)?,
            };
            if binding.is_none() {
                binding = Some(Binding::capture(
                    &mut reader,
                    geometry.size,
                    geometry.offset,
                    geometry.sector,
                )?);
                fingerprint = identity(&source).ok();
                if let Some(saved) = state()
                    .recovery_progress
                    .as_mut()
                    .filter(|saved| saved.id == work_id)
                {
                    saved.binding = binding.clone();
                }
            }
            let message = recovery_tasks::scan_signatures(
                &mut reader,
                geometry.size,
                geometry.sector,
                &options,
                &mut cursor,
                &work_id,
                &cancel,
                record_candidate,
            );
            // Check retained handles even after cancellation. Failed validation
            // leaves the observed results but cannot publish a resumable boundary.
            if let Err(error) = source.label().and_then(|_| {
                binding.as_ref().ok_or("尚无可保存的源特征")?.verify(
                    &mut reader,
                    geometry.size,
                    geometry.offset,
                    geometry.sector,
                )?;
                cursor.verify_cursor(&mut reader, &options)
            }) {
                binding = None;
                return Err(error);
            }
            message.map(|message| (message, None))
        }))
        .unwrap_or_else(|_| Err("恢复扫描异常结束。".into()));
        {
            let mut s = state();
            let done = cursor.work_done(&options);
            if let Some(saved) = s
                .recovery_progress
                .as_mut()
                .filter(|saved| saved.id == work_id)
            {
                saved.binding = binding;
                saved.retained_digest = fingerprint;
                saved.cursor = Some(Cursor::Signatures { options, cursor });
            }
            if let Some(job) = s.job.as_mut().filter(|job| job.id == work_id) {
                job.done = done;
            }
        }
        finish(&work_id, result);
    });
    Ok(id)
}

#[cfg(windows)]
fn cursor(
    saved: &Saved,
) -> Result<(&signature_scan::Options, &signature_scan::Checkpoint), String> {
    match &saved.cursor {
        Some(Cursor::Signatures { options, cursor }) => Ok((options, cursor)),
        _ => Err("恢复进度已失效".into()),
    }
}

fn records(files: &[Candidate]) -> Result<Vec<signature_scan::StoredCandidate>, String> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            if index as u32 != file.id {
                return Err("保存的签名候选标识不连续。".into());
            }
            signature_scan::StoredCandidate::from_candidate(file)
        })
        .collect()
}

/// The common recovery command holds the operation gate throughout save/load.
#[cfg(windows)]
pub(super) fn save(
    selection: Selection,
    saved: Saved,
    files: &[Candidate],
) -> Result<Option<String>, String> {
    let (source, _) = selection.resolve()?;
    let digest = identity(&source)?;
    if saved.retained_digest.as_deref() != Some(digest.as_str()) {
        return Err("请选择进度对应的原镜像、备份还原点和扫描范围。".into());
    }
    let (options, cursor) = cursor(&saved)?;
    let bytes = checkpoint::encode(checkpoint::Progress {
        kind: selection.kind,
        partition: selection.partition,
        source_digest: digest,
        binding: saved.binding.clone().ok_or("尚无可保存的源特征")?,
        options: options.clone(),
        cursor: cursor.clone(),
        records: records(files)?,
    })?;
    recovery_tasks::files::save(
        &source,
        &bytes,
        "DiskStudio-image-backup-progress.dsrecover",
        |bytes| checkpoint::decode(bytes).map(|_| ()),
    )
}

#[cfg(windows)]
pub(super) fn load(selection: Selection) -> Result<Option<recovery_tasks::Info>, String> {
    let (source, geometry) = selection.resolve()?;
    let Some(bytes) = recovery_tasks::files::load(&source)? else {
        return Ok(None);
    };
    let (saved, files) = checkpoint::decode(&bytes)?;
    let digest = identity(&source)?;
    saved.verify_source(selection.kind, selection.partition, &digest)?;
    geometry.validate(&saved.options, selection.partition.is_some())?;
    let cancel = AtomicBool::new(false);
    let mut reader = reader(&source, selection.partition.is_some(), &cancel)?;
    geometry.verify(
        &mut reader,
        &saved.binding,
        &saved.options,
        &saved.cursor,
        &files,
        &cancel,
    )?;
    let label = source.label()?;
    let range = saved.options.range(geometry.size)?;
    let total = saved.options.work(range.end - range.start);
    let id = uuid::Uuid::new_v4().to_string();
    let sealed = saved.cursor.sealed;
    let mut catalog = Catalog::default();
    for file in &files {
        catalog.add_file(file);
    }
    {
        let mut s = state();
        idle(&s)?;
        s.job = Some(Job {
            id: id.clone(), title: TITLE.into(), state: if sealed { "completed" } else { "paused" }.into(),
            phase: "已加载进度".into(), done: saved.cursor.work_done(&saved.options), total, found: files.len(),
            message: "已核对原镜像或备份还原点、内容特征和续扫边界。结果为历史观察，预览和导出时会复核候选。".into(),
            sha256: None,
        });
        s.scan = Some((id.clone(), source));
        s.scan_name = label;
        s.scan_mode = ScanMode::Signatures;
        s.scan_completion = if sealed {
            crate::directory_report::Completion::Ended
        } else {
            crate::directory_report::Completion::Paused
        };
        s.candidates = files;
        s.directories = catalog;
        s.pending = None;
        s.recovery_progress = Some(Saved {
            id,
            source: ProgressSource::Retained(selection),
            mode: ScanMode::Signatures,
            options: Some(saved.options.clone()),
            size: geometry.size,
            binding: Some(saved.binding),
            retained_digest: Some(digest),
            cursor: Some(Cursor::Signatures {
                options: saved.options,
                cursor: saved.cursor,
            }),
            pause_requested: false,
            loaded: true,
        });
    }
    Ok(recovery_tasks::status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor as Memory;

    fn options() -> signature_scan::Options {
        signature_scan::Options {
            kinds: vec![signature_scan::Kind::Pdf],
            sector_aligned: true,
            ..Default::default()
        }
    }

    #[test]
    fn retained_progress_reopens_resumes_and_rejects_other_sources() {
        let geometry = Geometry {
            offset: 4096,
            size: 3 << 20,
            sector: 4096,
        };
        let mut bytes = vec![0; geometry.size as usize];
        let pdf = b"%PDF-1.7\nFixture\n%%EOF";
        for offset in [0x1000, 0x201000] {
            bytes[offset..][..pdf.len()].copy_from_slice(pdf);
        }
        for kind in [Kind::Image, Kind::Backup] {
            let mut input = Memory::new(bytes.as_slice());
            let binding =
                Binding::capture(&mut input, geometry.size, geometry.offset, geometry.sector)
                    .unwrap();
            let files = RefCell::new(Vec::new());
            let mut cursor = signature_scan::Checkpoint::default();
            let options = options();
            assert!(signature_scan::continue_scan_scoped(
                &mut input,
                geometry.size,
                &options,
                &mut cursor,
                geometry.sector,
                None,
                |done, _| if done > 0 {
                    Err("paused".into())
                } else {
                    Ok(())
                },
                |file| files.borrow_mut().push(file),
            )
            .is_err());
            assert_eq!(cursor.consumed, 1 << 20);
            assert_eq!(files.borrow().len(), 1);
            let digest = "4".repeat(64);
            let encoded = checkpoint::encode(checkpoint::Progress {
                kind,
                partition: Some(0),
                source_digest: digest.clone(),
                binding,
                options,
                cursor,
                records: records(&files.borrow()).unwrap(),
            })
            .unwrap();
            let (mut saved, files) = checkpoint::decode(&encoded).unwrap();
            assert!(saved.verify_source(kind, Some(0), &digest).is_ok());
            assert!(saved.verify_source(kind, None, &digest).is_err());
            assert!(saved.verify_source(kind, Some(0), &"5".repeat(64)).is_err());
            let other = if kind == Kind::Image {
                Kind::Backup
            } else {
                Kind::Image
            };
            assert!(saved.verify_source(other, Some(0), &digest).is_err());
            let cancel = AtomicBool::new(false);
            let mut reopened = Memory::new(bytes.as_slice());
            geometry
                .verify(
                    &mut reopened,
                    &saved.binding,
                    &saved.options,
                    &saved.cursor,
                    &files,
                    &cancel,
                )
                .unwrap();
            let mut changed = bytes.clone();
            changed[(1 << 20) - 1] = 0xff;
            assert!(geometry
                .verify(
                    &mut Memory::new(changed),
                    &saved.binding,
                    &saved.options,
                    &saved.cursor,
                    &files,
                    &cancel,
                )
                .is_err());
            let mut results = files;
            signature_scan::continue_scan_scoped(
                &mut reopened,
                geometry.size,
                &saved.options,
                &mut saved.cursor,
                geometry.sector,
                None,
                |_, _| Ok(()),
                |file| results.push(file),
            )
            .unwrap();
            assert!(saved.cursor.sealed);
            assert_eq!(
                results.iter().map(|file| file.id).collect::<Vec<_>>(),
                [0, 1]
            );
            assert_eq!(results[1].name, "recovered_0000000000201000.pdf");
            let mut corrupt: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            corrupt["progress"]["sourceDigest"] = serde_json::json!("6".repeat(64));
            assert!(checkpoint::decode(&serde_json::to_vec(&corrupt).unwrap()).is_err());
        }
    }

    #[test]
    fn validates_scope_and_keeps_physical_progress_requests_compatible() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "kind": "backup", "id": "point-session", "partition": 0,
            "options": { "kinds": ["pdf"], "start": 4096, "length": 4096,
                         "freeOnly": true, "sectorAligned": true }
        }))
        .unwrap();
        let geometry = Geometry {
            size: 8192,
            offset: 0,
            sector: 4096,
        };
        assert!(geometry.validate(&request.options, true).is_ok());
        assert!(geometry.validate(&request.options, false).is_err());
        assert!(Geometry {
            size: 8191,
            ..geometry
        }
        .validate(&request.options, true)
        .is_err());
        let original =
            serde_json::json!({ "target": "a".repeat(64), "layoutKey": "b".repeat(64), "partition": 2 });
        let current: ProgressSource = serde_json::from_value(original.clone()).unwrap();
        assert!(matches!(current, ProgressSource::Disk(_)));
        assert_eq!(serde_json::to_value(current).unwrap(), original);
        let image = serde_json::json!({ "kind": "image", "id": "session", "partition": 0 });
        let retained: ProgressSource = serde_json::from_value(image.clone()).unwrap();
        assert!(matches!(retained, ProgressSource::Retained(_)));
        assert_eq!(serde_json::to_value(retained).unwrap(), image);
        assert!(serde_json::from_value::<ProgressSource>(serde_json::json!({
            "kind": "image", "id": "session", "partition": null, "path": "raw.img"
        }))
        .is_err());
    }
}

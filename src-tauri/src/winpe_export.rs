//! Export from verified WinPE sources using the desktop recovery sink.
use super::{
    files::{Listing, Row},
    image::Binding,
    partitions::Volume,
};
use crate::{
    data_windows::{self, FileBinding, FileLocation},
    directory_metadata,
    native_locale::Locale,
    recovery::{self, Candidate},
    recovery_export::{self, BatchOutcome, Layout, Plan},
    recovery_output::{self, Destination, Identity},
};
use cap_std::fs::Dir;
use std::{
    collections::HashMap,
    fs::File,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;

const READ_ONLY_VOLUME: u32 = 0x0008_0000;
const SAME_DISK: &str = "输出必须位于另一块物理磁盘，不能写回恢复/备份源盘。";

struct Batch {
    volume: Arc<Volume>,
    selected: String,
    files: HashMap<u32, Candidate>,
    plan: Plan,
}

impl Batch {
    fn select(listing: &Listing, row: &Row) -> Result<Self, String> {
        if !listing.exportable(row) {
            return Err("请选择可导出的文件或目录。".into());
        }
        let (selected, files, mut plan) = match row {
            Row::File(index) => {
                let file = listing.files.get(*index).ok_or("文件不存在")?;
                let (files, plan) = recovery_export::select(
                    &listing.files,
                    &listing.catalog,
                    &[file.id],
                    None,
                    Layout::Tree,
                )?;
                (file.name.clone(), files, plan)
            }
            Row::Folder(id) => {
                let path = listing.catalog.path(id)?;
                let name = path
                    .last()
                    .ok_or("目录不存在于当前扫描结果。")?
                    .name
                    .clone();
                let (files, plan) =
                    recovery_export::plan_directory(&listing.files, &listing.catalog, id)?;
                (name, files, plan)
            }
            Row::Up(_) | Row::Offset(_) => return Err("请选择可导出的文件或目录。".into()),
        };
        plan.image_source = Some(listing.volume.provenance()?);
        // Readers share the same retained image. One worker avoids competing
        // seeks and keeps memory use bounded in Windows PE.
        plan.options.workers = 1;
        Ok(Self {
            volume: listing.volume.clone(),
            selected,
            files: files.into_iter().map(|file| (file.id, file)).collect(),
            plan,
        })
    }

    fn execute(
        &self,
        sink: &mut Destination,
        cancel: Arc<AtomicBool>,
    ) -> Result<BatchOutcome, String> {
        recovery_export::execute_parallel_with_directories(
            &self.plan,
            sink,
            |entry, output, tick| {
                let file = self.files.get(&entry.id).ok_or("导出文件身份缺失")?;
                if file.size != entry.size {
                    return Err("导出容量与计划不一致".into());
                }
                let result = recovery::extract_owned_with_metadata(
                    self.volume.reader(&cancel)?,
                    file,
                    output,
                    entry.size,
                    cancel.clone(),
                    tick,
                )?;
                self.volume.source.verify_disk()?;
                output.extracted_metadata(result.metadata);
                Ok(result.bytes)
            },
            |directory| {
                directory_metadata::read(self.volume.reader(&cancel)?, directory, cancel.clone())
            },
            |_| active(&cancel),
        )
    }

    fn required(&self) -> u64 {
        self.plan.total_bytes.saturating_add(32 * disk_core::MIB)
    }
}

fn active(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("已停止导出。".into());
    }
    Ok(())
}

fn bind_destination(dir: &Dir, source: &Binding) -> Result<(File, FileLocation), String> {
    let file = dir.try_clone().map_err(data_windows::err)?.into_std_file();
    let location = data_windows::file_location(&file)?;
    if source.target() == &location.target || source.number() == location.disk_number() {
        return Err(SAME_DISK.into());
    }
    let mut flags = 0;
    // SAFETY: the retained directory handle and the flags buffer are valid.
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
        || flags & READ_ONLY_VOLUME != 0
    {
        return Err("无法确认导出目标卷可写。".into());
    }
    Ok((file, location))
}

pub(super) struct Prepared {
    batch: Batch,
    source: Binding,
    destination: FileBinding,
    dest_path: PathBuf,
    name: String,
    dest_location: FileLocation,
    dest_identity: Identity,
}

pub(super) fn prepare(
    listing: &Listing,
    row: &Row,
    path: &Path,
    locale: &Locale,
    cancel: &AtomicBool,
) -> Result<Prepared, String> {
    let batch = Batch::select(listing, row)?;
    drop(batch.volume.reader(cancel)?);
    let source = batch.volume.source.bind().map_err(|error| {
        format!(
            "{}\n{}",
            locale.text("无法确认来源磁盘。"),
            locale.text(&error)
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("导出目录名称无效。")?;
    recovery_output::validate_output_name(name)?;
    let parent_path = path.parent().ok_or("导出目录名称无效。")?;
    let (dest_path, expected) = data_windows::destination_location(
        parent_path,
        std::slice::from_ref(source.target()),
        batch.required(),
    )?;
    let dir = Dir::open_ambient_dir(&dest_path, cap_std::ambient_authority())
        .map_err(data_windows::err)?;
    let dest_identity = recovery_output::identity(&dir)?;
    match dir.symlink_metadata(name) {
        Ok(_) => return Err("导出目录已存在。请选择新目录。".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let (parent, dest_location) = bind_destination(&dir, &source)?;
    if dest_location.target != expected {
        return Err("输出目录实际所在磁盘已变化。".into());
    }
    let destination = FileBinding::exact(&parent, &dest_location)?;
    source.verify_with(&destination)?;
    active(cancel)?;
    Ok(Prepared {
        batch,
        source,
        destination,
        dest_path,
        name: name.into(),
        dest_location,
        dest_identity,
    })
}

impl Prepared {
    pub(super) fn describe(&self, locale: &Locale) -> String {
        let display = |value: &str| {
            value
                .chars()
                .map(|c| if c.is_control() { '�' } else { c })
                .collect::<String>()
        };
        let plan = &self.batch.plan;
        let origin = self.batch.plan.image_source.as_ref();
        let partition = origin.map_or_else(String::new, |origin| {
            self.batch
                .volume
                .display_partition(&origin.partition, locale)
        });
        let source = &self.batch.volume.source.browser.info.name;
        let dest = self.dest_path.join(&self.name).display().to_string();
        format!(
            "{}  {} / {} / {}\n{}  {}\n\n{}\n{}",
            locale.text("来源"), display(source), display(&partition), display(&self.batch.selected),
            locale.text("目标"), display(&dest),
            locale.format("导出 {files} 个文件和 {folders} 个子目录，共 {bytes} 字节。清单记录跳过的 {skipped} 个文件。", &[
                ("files", &plan.entries.len().to_string()),
                ("folders", &plan.folders.len().saturating_sub(1).to_string()),
                ("bytes", &plan.total_bytes.to_string()),
                ("skipped", &plan.skipped.len().to_string()),
            ]),
            locale.text("在目标下新建目录并逐文件回读 SHA-256。删除候选的完整性取决于源数据。"),
        )
    }

    pub(super) fn execute(
        self,
        locale: &Locale,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        active(&cancel)?;
        let (root, expected) = data_windows::destination_location(
            &self.dest_path,
            std::slice::from_ref(self.source.target()),
            self.batch.required(),
        )?;
        if expected != self.dest_location.target || root != self.dest_path {
            return Err("输出目录实际所在磁盘已变化。".into());
        }
        let mut destination = Destination::new_named_checked(&root, &self.name, |parent| {
            let (_, location) = bind_destination(parent, &self.source)?;
            if location != self.dest_location
                || recovery_output::identity(parent)? != self.dest_identity
            {
                return Err("输出目录与确认时不同。".into());
            }
            self.source.verify_with(&self.destination)?;
            drop(self.batch.volume.reader(&cancel)?);
            active(&cancel)
        })?;
        let base = destination.display.display().to_string();
        let result = self
            .batch
            .execute(&mut destination, cancel)
            .map_err(|e| format!("{}\n{base}", locale.text(&e)))?;
        let saved = result.files.iter().filter(|file| file.saved).count();
        let failed = result.files.len() - saved;
        let folders = result.directories.iter().filter(|dir| dir.created).count();
        let details = locale.format(
            "已导出 {saved} 个文件和 {folders} 个子目录。失败 {failed} 个文件，跳过 {skipped} 个文件。逐项结果见 DiskStudio-export.json。",
            &[
                ("saved", &saved.to_string()), ("folders", &folders.to_string()),
                ("failed", &failed.to_string()), ("skipped", &self.batch.plan.skipped.len().to_string()),
            ],
        );
        Ok(format!("{details}\n{base}"))
    }
}

#[cfg(test)]
#[test]
#[ignore = "requires the WinPE FAT12 image fixture and a new ordinary output directory"]
fn winpe_export_from_retained_image_verifies_directory_and_deleted_file() {
    use super::{files, image, partitions};
    use crate::{
        image_browser::{Kind, Options},
        native_locale::Language,
        recovery::ScanMode,
        recovery_tree::ROOT,
    };
    use sha2::{Digest, Sha256};
    use std::fs;

    let path = PathBuf::from(std::env::var_os("DISKSTUDIO_WINPE_FILE_FIXTURE").unwrap());
    let parent = PathBuf::from(std::env::var_os("DISKSTUDIO_WINPE_EXPORT_FIXTURE").unwrap());
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    let image_before = Sha256::digest(fs::read(&path).unwrap());
    let locale = Locale::new(Language::En);
    let open = image::open(
        &path,
        Options {
            kind: Kind::Disk,
            sector: 512,
        },
        &locale,
        &AtomicBool::new(false),
    )
    .unwrap();
    let source = Arc::new(open.source);
    let volume = if std::env::var_os("DISKSTUDIO_WINPE_SEARCH_FIXTURE").is_some() {
        assert!(source.browser.info.partitions.is_empty());
        let found = partitions::search(
            source,
            partitions::Scope::Whole,
            Arc::new(AtomicBool::new(false)),
            &partitions::Progress::default(),
        )
        .unwrap();
        assert_eq!(found.volumes.len(), 1);
        found.volumes.into_iter().next().unwrap()
    } else {
        Volume::table(source, 0)
    };
    let volume = Arc::new(volume);
    for (mode, label, expected, export_path) in [
        (
            ScanMode::Existing,
            "DOCS",
            b"Known nested WinPE text.\r\nLine two.\r\n".as_slice(),
            "DOCS/NOTE.TXT",
        ),
        (
            ScanMode::Deleted,
            "OST.TXT",
            b"Known deleted WinPE text.\r\n".as_slice(),
            "_OST.TXT",
        ),
    ] {
        let cancel = Arc::new(AtomicBool::new(false));
        let listing = files::scan(volume.clone(), mode, cancel.clone()).unwrap();
        let rows = listing.page(ROOT, 0, &locale).1;
        let row = rows.iter().find(|(name, _)| name.contains(label)).unwrap();
        let batch = Batch::select(&listing, &row.1).unwrap();
        let directory = if mode == ScanMode::Existing {
            "existing-export"
        } else {
            "deleted-export"
        };
        let mut destination =
            Destination::new_named_checked(&parent, directory, |_| Ok(())).unwrap();
        assert_eq!(destination.display, parent.join(directory));
        let result = batch.execute(&mut destination, cancel).unwrap();
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].saved);
        assert_eq!(
            fs::read(destination.display.join(export_path)).unwrap(),
            expected
        );
        let sha = format!("{:x}", Sha256::digest(expected));
        assert_eq!(result.files[0].sha256.as_deref(), Some(sha.as_str()));
        let manifest =
            fs::read_to_string(destination.display.join(recovery_export::REPORT)).unwrap();
        assert!(manifest.contains(&sha));
        drop(destination);
        assert!(Destination::new_named_checked(&parent, directory, |_| Ok(())).is_err());
        assert_eq!(
            fs::read(parent.join(directory).join(export_path)).unwrap(),
            expected
        );
        println!("{export_path}: {} bytes, SHA-256 {sha}", expected.len());
    }
    assert_eq!(Sha256::digest(fs::read(&path).unwrap()), image_before);
}

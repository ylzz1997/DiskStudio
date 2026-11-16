//! Read-only acceptance of the unchanged production FAT checkpoint engines.
//! Only fixed existing A partitions. Ordinary progress files go through native
//! dialogs to another disk; no fixture creation, format, mount or disk writes.
use super::*;
use crate::fat_recovery::scan_progress::{continue_scan, Checkpoint};
use crate::recovery::{Candidate, ScanMode};
use crate::recovery_checkpoint::{self as portable, Cursor, Progress};
use crate::recovery_tree::Catalog;
use crate::signature_checkpoint::Binding;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const PAUSE: &str = "guest-checkpoint-boundary-pause";
const MIB: u64 = 1024 * 1024;
fn resolved(source: &Selection, part: u32, size: u64) -> Result<data_windows::Resolved, String> {
    inventory()?;
    if source.partition != Some(part) {
        return Err("STOP: wrong partition scope".into());
    }
    let r = data_windows::resolve(source, false, false)?;
    if r.disk.number != 1
        || r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(A)
        || r.disk.size != 16 * GIB
        || r.size != size
        || r.disk.logical_sector_size != Some(512)
        || r.disk.is_offline != Some(false)
        || r.disk.is_boot != Some(false)
        || r.disk.is_system != Some(false)
    {
        return Err("STOP: source is not the expected online A test partition".into());
    }
    Ok(r)
}
fn scan(
    r: &data_windows::Resolved,
    mode: ScanMode,
    stop: u64,
) -> Result<(Checkpoint, Vec<Candidate>, Catalog, Binding), String> {
    let mut reader = data_windows::reader(r)?; // Native number/capacity/sector/serial verified.
    let binding = Binding::capture(&mut reader, r.size, r.offset, 512)?;
    let mut cp = Checkpoint::default();
    let mut files = vec![];
    let mut tree = Catalog::default();
    let result = continue_scan(
        &mut reader,
        mode,
        &mut cp,
        |done, _| {
            if done >= stop {
                Err(PAUSE.into())
            } else {
                Ok(())
            }
        },
        |c| files.push(c),
        |d| tree.insert(d),
    );
    if stop == u64::MAX {
        result?;
        if !cp.sealed {
            return Err("Baseline did not complete".into());
        }
    } else if result.as_ref().err().map(String::as_str) != Some(PAUSE)
        || cp.sealed
        || cp.records < stop
    {
        return Err("Did not pause at a complete unfinished entry-group boundary".into());
    }
    for file in &files {
        tree.add_file(file);
    }
    binding.verify(&mut reader, r.size, r.offset, 512)?;
    cp.verify(&mut reader, mode, r.size)?;
    Ok((cp, files, tree, binding))
}
fn comparable(files: &[Candidate]) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(files).map_err(err)?;
    for c in value.as_array_mut().ok_or("Candidates not an array")? {
        c.as_object_mut()
            .ok_or("Candidate not an object")?
            .remove("note");
    }
    Ok(value)
}
struct DigestSink {
    sha: Sha256,
    bytes: u64,
}
impl Write for DigestSink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.sha.update(b);
        self.bytes += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn contents(
    r: &data_windows::Resolved,
    files: &[Candidate],
) -> Result<Vec<(u32, u64, String)>, String> {
    let mut reader = data_windows::reader(r)?;
    let mut result = vec![];
    for c in files.iter().filter(|c| c.supported) {
        if c.size > 8 * MIB {
            return Err(
                "Fixture exceeds complete-content check budget; no partial hash acceptance".into(),
            );
        }
        let mut out = DigestSink {
            sha: Sha256::new(),
            bytes: 0,
        };
        let actual = recovery::extract(&mut reader, c, &mut out, u64::MAX, |_| Ok(()))?;
        if actual != c.size || out.bytes != c.size {
            return Err("Short extraction".into());
        }
        result.push((c.id, c.size, format!("{:x}", out.sha.finalize())));
    }
    Ok(result)
}
pub(super) fn save(progress: Progress, name: &str) -> Result<String, String> {
    use cap_std::fs::{Dir, OpenOptions as CapOptions, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::*;
    let bytes = portable::encode(progress.clone())?;
    let path = rfd::FileDialog::new()
        .set_title("VM 验收：保存暂停进度到另一块磁盘")
        .add_filter("DiskStudio 恢复进度", &["dsrecover"])
        .set_file_name(name)
        .save_file()
        .ok_or("Progress save cancelled")?;
    if path.extension().and_then(|x| x.to_str()) != Some("dsrecover") {
        return Err("Unexpected extension".into());
    }
    let filename = path
        .file_name()
        .and_then(|x| x.to_str())
        .ok_or("Invalid filename")?;
    if filename.len() > 180
        || filename.contains([':', '/', '\\'])
        || filename.chars().any(char::is_control)
    {
        return Err("Invalid filename".into());
    }
    let parent = data_windows::destination_folder(
        path.parent().ok_or("No parent")?,
        &progress.source.target,
        bytes.len() as u64,
    )?;
    let dir = Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(err)?;
    let partial = format!("DiskStudio-FAT-checkpoint-{}.partial", uuid::Uuid::new_v4());
    let mut options = CapOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
        .share_mode(0);
    let mut file = dir.open_with(&partial, &options).map_err(err)?.into_std();
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(err)?;
    file.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut readback = vec![0; bytes.len()];
    file.read_exact(&mut readback).map_err(err)?;
    if Sha256::digest(&readback) != Sha256::digest(&bytes) {
        return Err("Progress readback SHA-256 mismatch".into());
    }
    portable::decode(&readback, &progress.source)?;
    recovery_output::finish(&file, &dir, filename)?;
    Ok(filename.into())
}
pub fn check() -> Result<String, String> {
    inventory()?;
    let exe = std::env::current_exe().map_err(err)?;
    let digest = format!("{:x}", Sha256::digest(fs::read(exe).map_err(err)?));
    let log_path =
        PathBuf::from(r"C:\ProgramData").join(format!("DiskStudio-FAT-resume-{digest}.txt"));
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path)
        .map_err(err)?;
    writeln!(
        log,
        "START {digest}\nRead-only A partition scans. No mutation or source file writes."
    )
    .map_err(err)?;
    log.sync_all().map_err(err)?;
    let mut pending = vec![];
    let mut cases = 0;
    let mut lines = vec![];
    for (part, size, bits) in [
        (3, 256 * MIB, 32),
        (4, 512 * MIB, 64),
        (17, 2 * MIB, 12),
        (18, 128 * MIB, 16),
        (19, 512 * MIB, 32),
    ] {
        let source = selection(A, Some(part))?;
        let initial = resolved(&source, part, size)?;
        for mode in [ScanMode::AllFiles, ScanMode::Deleted, ScanMode::Existing] {
            let (base, expected, expected_tree, _) = scan(&initial, mode, u64::MAX)?;
            let encoded = serde_json::to_value(&base).map_err(err)?;
            if encoded["bits"].as_u64() != Some(bits) || base.records < 3 {
                return Err("Unexpected filesystem width or insufficient directory records".into());
            }
            let expected_content = contents(&initial, &expected)?;
            let mut stops = vec![1, base.records / 2, base.records - 1];
            stops.sort_unstable();
            stops.dedup();
            for stop in stops {
                let r = resolved(&source, part, size)?;
                let (cp, files, tree, binding) = scan(&r, mode, stop)?;
                let progress = Progress::capture(
                    source.clone(),
                    binding,
                    Cursor::Fat { mode, cursor: cp },
                    &files,
                    &tree,
                )?;
                let bytes = portable::encode(progress)?;
                let mut loaded = portable::decode(&bytes, &source)?;
                let mut reader = data_windows::reader(&r)?;
                loaded.binding.verify(&mut reader, r.size, r.offset, 512)?;
                loaded.verify_cursor(&mut reader, r.size)?;
                let (mut files, mut tree) = loaded.validate(&source)?;
                if mode == ScanMode::AllFiles && stop == base.records / 2 && part <= 4 {
                    pending.push((part, loaded.clone()));
                }
                let Cursor::Fat { cursor, .. } = &mut loaded.cursor else {
                    return Err("Wrong progress engine".into());
                };
                continue_scan(
                    &mut reader,
                    mode,
                    cursor,
                    |_, _| Ok(()),
                    |c| files.push(c),
                    |d| tree.insert(d),
                )?;
                if !cursor.sealed
                    || cursor.records != base.records
                    || comparable(&files)? != comparable(&expected)?
                    || tree.snapshot() != expected_tree.snapshot()
                    || contents(&r, &files)? != expected_content
                {
                    return Err(format!("Resume mismatch A/{part} {mode:?} stop {stop}"));
                }
                cursor.verify(&mut reader, mode, r.size)?;
                resolved(&source, part, size)?;
                cases += 1;
                writeln!(log,"PASS A/{part} bits={bits} {mode:?} stop={stop} records={} candidates={} full-file-digests={}",cursor.records,files.len(),expected_content.len()).map_err(err)?;
                log.sync_all().map_err(err)?;
            }
            lines.push(format!(
                "A/{part} {} {mode:?}: {} records / {} files",
                if bits == 64 {
                    "exFAT".into()
                } else {
                    format!("FAT{bits}")
                },
                base.records,
                expected.len()
            ));
        }
    }
    for (part, progress) in pending {
        let size = if part == 3 { 256 * MIB } else { 512 * MIB };
        let r = resolved(&progress.source, part, size)?;
        let mut reader = data_windows::reader(&r)?;
        progress
            .binding
            .verify(&mut reader, r.size, r.offset, 512)?;
        progress.verify_cursor(&mut reader, r.size)?;
        drop(reader);
        let done = progress.cursor.done();
        let count = progress.cursor.count();
        let name = save(
            progress,
            if part == 3 {
                "DiskStudio-FAT-partial.dsrecover"
            } else {
                "DiskStudio-exFAT-partial.dsrecover"
            },
        )?;
        writeln!(log, "SAVED {name}: {done} records / {count} candidates").map_err(err)?;
        log.sync_all().map_err(err)?;
        lines.push(format!("Saved {name}: {done} records / {count} files"));
    }
    inventory()?;
    writeln!(log, "ALL {cases} PARTIAL RESUME CASES PASSED").map_err(err)?;
    log.sync_all().map_err(err)?;
    Ok(format!("PASS: {cases} partial resume cases on fixed A partitions.\n\n{}\n\nAll supported candidate contents compared in full with SHA-256. No source writes, no OS raw reads, no partition/storage methods.\n\nLoad the two partial progress files in unchanged DiskStudio.exe and click Continue to verify the native UI.",lines.join("\n")))
}

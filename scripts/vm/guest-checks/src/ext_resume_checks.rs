//! Read-only progress acceptance on the existing populated A/5 Ext4 fixture.
//! No source creation, mounts, storage methods, writes or OS raw reads.
use super::*;
use crate::ext_recovery::scan_progress::{continue_scan, Checkpoint, SharedReader};
use crate::recovery::{Candidate, ScanMode};
use crate::recovery_checkpoint::{self as portable, Cursor, Progress};
use crate::recovery_tree::Catalog;
use crate::signature_checkpoint::Binding;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
const PAUSE: &str = "guest-ext-boundary-pause";
fn resolved(source: &Selection) -> Result<data_windows::Resolved, String> {
    inventory()?;
    if source.partition != Some(5) {
        return Err("STOP: wrong partition".into());
    }
    let r = data_windows::resolve(source, false, false)?;
    if r.disk.number != 1
        || r.disk.serial_number.as_deref().map(disk_identity::clean) != Some(A)
        || r.disk.size != 16 * GIB
        || r.size != 3 * GIB
        || r.disk.logical_sector_size != Some(512)
        || r.disk.is_boot != Some(false)
        || r.disk.is_system != Some(false)
    {
        return Err("STOP: not authorized A/5 fixture".into());
    }
    Ok(r)
}
fn scan(
    r: &data_windows::Resolved,
    mode: ScanMode,
    stop: u64,
) -> Result<(Checkpoint, Vec<Candidate>, Catalog, Binding), String> {
    let mut reader = SharedReader::new(data_windows::reader(r)?);
    let binding = Binding::capture(&mut reader, r.size, r.offset, 512)?;
    let mut cp = Checkpoint::default();
    let mut files = vec![];
    let mut tree = Catalog::default();
    let result = continue_scan(
        reader.clone(),
        mode,
        &mut cp,
        Arc::new(AtomicBool::new(false)),
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
            return Err("Baseline incomplete".into());
        }
    } else if result.as_ref().err().map(String::as_str) != Some(PAUSE)
        || cp.sealed
        || cp.entries != stop
    {
        return Err("Did not pause at requested complete entry".into());
    }
    for c in &files {
        tree.add_file(c);
    }
    binding.verify(&mut reader, r.size, r.offset, 512)?;
    cp.verify(reader, mode, r.size)?;
    Ok((cp, files, tree, binding))
}
fn comparable(files: &[Candidate]) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(files).map_err(err)?;
    for c in value.as_array_mut().ok_or("Candidate array missing")? {
        c.as_object_mut()
            .ok_or("Candidate object missing")?
            .remove("note");
    }
    Ok(value)
}
struct Sink {
    hash: Sha256,
    bytes: u64,
}
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.hash.update(b);
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
    if files
        .iter()
        .filter(|c| c.supported)
        .try_fold(0u64, |n, c| n.checked_add(c.size))
        .is_none_or(|n| n > 2 * GIB)
    {
        return Err("Full content check exceeds 2 GiB budget; no prefix acceptance".into());
    }
    let reader = SharedReader::new(data_windows::reader(r)?);
    let mut result = vec![];
    for c in files.iter().filter(|c| c.supported) {
        let mut out = Sink {
            hash: Sha256::new(),
            bytes: 0,
        };
        let n = recovery::extract_owned(
            reader.clone(),
            c,
            &mut out,
            u64::MAX,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )?;
        if n != c.size || out.bytes != c.size {
            return Err("Short extraction".into());
        }
        result.push((c.id, c.size, format!("{:x}", out.hash.finalize())));
    }
    Ok(result)
}
pub fn check() -> Result<String, String> {
    inventory()?;
    let digest = format!(
        "{:x}",
        Sha256::digest(fs::read(std::env::current_exe().map_err(err)?).map_err(err)?)
    );
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(PathBuf::from(r"C:\ProgramData").join(format!("DiskStudio-Ext-resume-{digest}.txt")))
        .map_err(err)?;
    writeln!(log, "START {digest}; fixed A/5 read-only; no source writes").map_err(err)?;
    log.sync_all().map_err(err)?;
    let source = selection(A, Some(5))?;
    let r = resolved(&source)?;
    // Prove the actual filesystem family; a Linux GPT entry is insufficient.
    let mut source_reader = data_windows::reader(&r)?;
    source_reader.seek(SeekFrom::Start(1024)).map_err(err)?;
    let mut superblock = [0; 1024];
    source_reader.read_exact(&mut superblock).map_err(err)?;
    let word = |at| u32::from_le_bytes(superblock[at..at + 4].try_into().unwrap());
    if superblock[0x38..0x3a] != [0x53, 0xef]
        || word(0x18) != 2
        || word(0x60) & 0x40 == 0
        || word(0x5c) & 4 == 0
    {
        return Err("STOP: A/5 is not the expected 4 KiB Ext4 filesystem".into());
    }
    drop(source_reader);
    let mut saved = None;
    let mut lines = vec![];
    let mut cases = 0;
    for mode in [ScanMode::AllFiles, ScanMode::Existing] {
        let (baseline, expected, tree, _) = scan(&r, mode, u64::MAX)?;
        if baseline.entries < 4 || expected.is_empty() {
            return Err("Missing populated fixture".into());
        }
        let hashes = contents(&r, &expected)?;
        for stop in [1, baseline.entries / 2, baseline.entries - 1] {
            let r = resolved(&source)?;
            let (cp, files, dirs, binding) = scan(&r, mode, stop)?;
            let p = Progress::capture(
                source.clone(),
                binding,
                Cursor::Ext { mode, cursor: cp },
                &files,
                &dirs,
            )?;
            let mut loaded = portable::decode(&portable::encode(p)?, &source)?;
            let mut reader = SharedReader::new(data_windows::reader(&r)?);
            loaded.binding.verify(&mut reader, r.size, r.offset, 512)?;
            loaded.verify_owned(reader.clone(), r.size)?;
            if mode == ScanMode::AllFiles && stop == baseline.entries / 2 {
                saved = Some(loaded.clone());
            }
            let (mut files, mut dirs) = loaded.validate(&source)?;
            let Cursor::Ext { cursor, .. } = &mut loaded.cursor else {
                return Err("Wrong engine".into());
            };
            let mut added = vec![];
            continue_scan(
                reader.clone(),
                mode,
                cursor,
                Arc::new(AtomicBool::new(false)),
                |_, _| Ok(()),
                |c| added.push(c),
                |d| dirs.insert(d),
            )?;
            for c in added {
                dirs.add_file(&c);
                files.push(c);
            }
            if !cursor.sealed
                || cursor.entries != baseline.entries
                || comparable(&files)? != comparable(&expected)?
                || dirs.snapshot() != tree.snapshot()
            {
                return Err("Resume result mismatch".into());
            }
            cursor.verify(reader, mode, r.size)?;
            if stop == baseline.entries - 1 && contents(&r, &files)? != hashes {
                return Err("Full file SHA-256 mismatch".into());
            }
            resolved(&source)?;
            cases += 1;
            writeln!(
                log,
                "PASS {mode:?} stop={stop} entries={} files={}",
                cursor.entries,
                files.len()
            )
            .map_err(err)?;
            log.sync_all().map_err(err)?;
        }
        lines.push(format!(
            "{mode:?}: {} entries, {} files; {} full-file hashes matched",
            baseline.entries,
            expected.len(),
            hashes.len()
        ));
    }
    let saved = saved.ok_or("Partial checkpoint missing")?;
    let r = resolved(&source)?;
    let mut reader = SharedReader::new(data_windows::reader(&r)?);
    saved.binding.verify(&mut reader, r.size, r.offset, 512)?;
    saved.verify_owned(reader, r.size)?;
    let done = saved.cursor.done();
    let count = saved.cursor.count();
    let name = super::fat_resume_checks::save(saved, "DiskStudio-Ext-partial.dsrecover")?;
    writeln!(
        log,
        "SAVED {name}: {done} entries / {count} files\nALL {cases} CASES PASSED"
    )
    .map_err(err)?;
    log.sync_all().map_err(err)?;
    inventory()?;
    Ok(format!("PASS: {cases} Ext4 partial resume cases on A/5.\n\n{}\n\nSaved {name}: {done} entries / {count} files.\n\nNo source writes, mounts or OS raw reads.\nNext: load and continue in unchanged production DiskStudio.",lines.join("\n")))
}

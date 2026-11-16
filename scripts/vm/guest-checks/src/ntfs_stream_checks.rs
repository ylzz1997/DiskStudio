//! Only isolated A/B. Adds one unique source file and named streams on A/NTFS;
//! copies candidates to a unique B directory. No format/delete or old fixture edits.
use super::*;
use allocation_batch::{record, set_online, tables};
use cap_std::fs::OpenOptions as CapOptions;
use recovery_output_checks::{bind, folder, selected};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_COMPRESSION, FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE,
};

pub fn check() -> Result<String, String> {
    check_inner(false)
}
pub fn continue_existing() -> Result<String, String> {
    check_inner(true)
}
fn existing_folder(source: &Selection) -> Result<cap_std::fs::Dir, String> {
    let resolved = data_windows::resolve(source, true, false)?;
    let path = allocation_batch::root(&resolved)?;
    let root =
        cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).map_err(err)?;
    bind(&root, source)?;
    let mut found = None;
    for (index, entry) in root.entries().map_err(err)?.enumerate() {
        if index >= 4096 {
            return Err("Too many source directories".into());
        }
        let entry = entry.map_err(err)?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("Named-Streams-Source-")
        {
            continue;
        }
        let dir = root.open_dir(entry.file_name()).map_err(err)?;
        bind(&dir, source)?;
        match dir.open(ntfs_stream_evidence::BASE) {
            Ok(file) => {
                data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
                    &file.into_std(),
                    &source.target,
                )?])?;
                if found.replace(dir).is_some() {
                    return Err("Frozen source folder is ambiguous".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
    }
    found.ok_or("Frozen source folder absent".into())
}
fn check_inner(continued: bool) -> Result<String, String> {
    if continued {
        ntfs_stream_evidence::frozen()?;
    }
    let mut log = backup_protection::begin(if continued {
        "ntfs-streams-continue"
    } else {
        "ntfs-streams"
    })?;
    if continued {
        // Fixed across helper builds: never dispatch the new export phase twice.
        let path = format!(
            r"C:\ProgramData\DiskStudio-ntfs-streams-continue-{}.once",
            ntfs_stream_evidence::FAILED
        );
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(err)?;
        marker
            .write_all(
                b"Verify retained source without rewriting; then first flat/tree export. No retry.",
            )
            .map_err(err)?;
        marker.sync_all().map_err(err)?;
    }
    let result = (|| {
        if !continued {
            set_online(A)?;
            set_online(B)?;
        }
        let gpt = tables()?;
        let layout = format_checks::layouts(&[])?;
        let source = selected(A, "NTFS", 2 * GIB)?;
        let target = selected(B, "NTFS", 6 * GIB)?;
        let dir = if continued {
            existing_folder(&source)?
        } else {
            folder(&source, "Named-Streams-Source")?.0
        };
        let base = if continued {
            ntfs_stream_evidence::BASE.to_owned()
        } else {
            format!("DS-streams-{}.bin", uuid::Uuid::new_v4().simple())
        };
        let mut cases = vec![(base.clone(), b"NTFS main stream".to_vec(), 0u16, false)];
        cases.push((
            format!("{base}:说明"),
            b"named resident text".to_vec(),
            0,
            false,
        ));
        cases.push((format!("{base}:empty"), vec![], 0, false));
        cases.push((
            format!("{base}:plain"),
            (0..65553usize)
                .map(|i| ((i * 29 + i / 257) % 251) as u8)
                .collect(),
            0,
            false,
        ));
        let mut sparse = vec![0; 262161];
        sparse[..4096].fill(31);
        sparse[258065..].fill(47);
        cases.push((format!("{base}:sparse"), sparse, 0, true));
        cases.push((
            format!("{base}:packed"),
            (0..524305usize)
                .map(|i| b'A' + (i / 16384 % 19) as u8)
                .collect(),
            2,
            false,
        ));
        // Enough named attributes to require extension records in ordinary NTFS.
        for i in 0..18 {
            cases.push((format!("{base}:extra-{i:02}"), vec![i as u8; 113], 0, false));
        }
        record(
            &mut log,
            serde_json::json!({"event":if continued {"retained-source-check-begin"}else{"seed-begin"},"base":base,"items":cases.len()}),
        )?;
        for (name, bytes, compression, sparse) in &cases {
            bind(&dir, &source)?;
            if !continued {
                let mut options = CapOptions::new();
                options.read(true).write(true).create_new(true);
                let mut file = dir.open_with(name, &options).map_err(err)?.into_std();
                let binding = data_windows::FileBinding::new(&file, &source.target)?;
                data_windows::FileBinding::validate_all(&[binding])?;
                data_windows::control(
                    &file,
                    FSCTL_SET_COMPRESSION,
                    &compression.to_le_bytes(),
                    &mut [],
                )?;
                if *sparse {
                    data_windows::control(&file, FSCTL_SET_SPARSE, &[], &mut [])?;
                    file.set_len(bytes.len() as u64).map_err(err)?;
                    file.write_all(&bytes[..4096]).map_err(err)?;
                    file.seek(SeekFrom::Start(258065)).map_err(err)?;
                    file.write_all(&bytes[258065..]).map_err(err)?;
                } else {
                    file.write_all(bytes).map_err(err)?;
                }
                file.sync_all().map_err(err)?;
                drop(file);
            }
            let mut file = dir.open(name).map_err(err)?.into_std();
            data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
                &file,
                &source.target,
            )?])?;
            let mut compression_actual = [0; 2];
            let n =
                data_windows::control(&file, FSCTL_GET_COMPRESSION, &[], &mut compression_actual)?;
            if n != 2 || u16::from_le_bytes(compression_actual) != *compression {
                return Err("Native source compression differs".into());
            }
            let mut actual = vec![];
            (&mut file)
                .take(bytes.len() as u64 + 1)
                .read_to_end(&mut actual)
                .map_err(err)?;
            if actual != *bytes {
                return Err("Native seed readback differs".into());
            }
        }
        drop(dir);
        record(
            &mut log,
            serde_json::json!({"event":if continued {"retained-source-check-passed"}else{"seed-passed"},"items":cases.len(),"sourceRewritten":!continued}),
        )?;
        let resolved = data_windows::resolve(&source, true, false)?;
        let locks = data_windows::lock(&resolved)?;
        let fresh = data_windows::resolve_locked(&source, false, &resolved, &locks)?;
        let mut reader = data_windows::reader(&fresh)?;
        let mut files = vec![];
        let mut catalog = recovery_tree::Catalog::default();
        let mut cursor = ntfs_scan::Checkpoint::default();
        ntfs_scan::continue_scan(
            &mut reader,
            recovery::ScanMode::Existing,
            &mut cursor,
            |_, _| Ok(()),
            |c| files.push(c),
            |d| catalog.insert(d),
        )?;
        let records = files
            .iter()
            .map(ntfs_checkpoint::Record::capture)
            .collect::<Result<Vec<_>, _>>()?;
        let records: Vec<ntfs_checkpoint::Record> =
            serde_json::from_slice(&serde_json::to_vec(&records).map_err(err)?).map_err(err)?;
        let (restored, _) = ntfs_checkpoint::restore(
            &records,
            &catalog.snapshot(),
            &cursor,
            recovery::ScanMode::Existing,
        )?;
        let selected = restored
            .into_iter()
            .filter(|c| cases.iter().any(|(n, _, _, _)| n == &c.name))
            .collect::<Vec<_>>();
        if selected.len() != cases.len() {
            return Err(format!(
                "Expected {} candidates, received {}",
                cases.len(),
                selected.len()
            ));
        }
        for (name, bytes, _, _) in &cases {
            let matched = selected
                .iter()
                .filter(|c| &c.name == name)
                .collect::<Vec<_>>();
            if matched.len() != 1 || !matched[0].supported || matched[0].size != bytes.len() as u64
            {
                return Err(format!(
                    "Candidate differs: {name}: {:?}",
                    matched.iter().map(|c| &c.note).collect::<Vec<_>>()
                ));
            }
            let mut actual = vec![];
            recovery::extract(&mut reader, matched[0], &mut actual, u64::MAX, |_| Ok(()))?;
            if actual != *bytes {
                return Err(format!("Raw stream differs: {name}"));
            }
        }
        drop(reader);
        drop(locks);
        record(
            &mut log,
            serde_json::json!({"event":"scan-extract-checkpoint-passed","items":selected.len()}),
        )?;
        for export_layout in [recovery_export::Layout::Flat, recovery_export::Layout::Tree] {
            let mut plan = recovery_export::plan(&selected, &catalog, export_layout)?;
            plan.options.workers = 2;
            let (parent, path) = folder(&target, "Named-Streams-Output")?;
            record(
                &mut log,
                serde_json::json!({"event":"export-begin","layout":export_layout}),
            )?;
            run(Work::Recover {
                source: source.clone().into(),
                files: selected.clone(),
                folder: path,
                export: plan.clone(),
            })?;
            bind(&parent, &target)?;
            let children = parent
                .entries()
                .map_err(err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?;
            if children.len() != 1 {
                return Err("Expected one output root".into());
            }
            let output = parent.open_dir(children[0].file_name()).map_err(err)?;
            let manifest: serde_json::Value =
                serde_json::from_slice(&output.read(recovery_export::REPORT).map_err(err)?)
                    .map_err(err)?;
            if manifest["completed"] != true
                || manifest["savedFiles"] != cases.len()
                || manifest["failedFiles"] != 0
            {
                return Err("Export manifest did not complete".into());
            }
            for entry in &plan.entries {
                if entry.name.contains(':') {
                    return Err("Export attempted an ADS destination".into());
                }
                let expected = &cases
                    .iter()
                    .find(|(n, _, _, _)| n == &entry.original_name)
                    .ok_or("Unexpected file")?
                    .1;
                let mut dir = output.try_clone().map_err(err)?;
                for i in plan.chain(entry.folder)? {
                    dir = dir.open_dir(&plan.folders[i].name).map_err(err)?;
                }
                bind(&dir, &target)?;
                let mut file = dir.open(&entry.name).map_err(err)?.into_std();
                data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
                    &file,
                    &target.target,
                )?])?;
                let mut actual = vec![];
                (&mut file)
                    .take(expected.len() as u64 + 1)
                    .read_to_end(&mut actual)
                    .map_err(err)?;
                let hash = format!("{:x}", Sha256::digest(&actual));
                if actual != *expected
                    || !manifest["outcomes"]
                        .as_array()
                        .ok_or("Outcomes missing")?
                        .iter()
                        .any(|o| o["id"] == entry.id && o["saved"] == true && o["sha256"] == hash)
                {
                    return Err("Independent output/hash differs".into());
                }
            }
            if tables()? != gpt || format_checks::layouts(&[])? != layout {
                return Err("GPT or layout changed".into());
            }
            record(
                &mut log,
                serde_json::json!({"event":"export-passed","layout":export_layout,"items":selected.len()}),
            )?;
        }
        Ok(format!("PASS: {} native main/named-stream fixtures (resident, empty, nonresident, sparse, LZNT1 and extension records); raw extraction and checkpoint roundtrip; flat/tree output to B verified independently byte-for-byte and SHA-256; all GPT/layout unchanged. Source files retained. No format/delete/retry.",cases.len()))
    })();
    record(
        &mut log,
        serde_json::json!({"event":if result.is_ok(){"passed"}else{"failed"},"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )?;
    result
}

//! One consolidated guest batch. Adds unique ordinary fixtures on A and output
//! folders on B; never formats a partition or overwrites historical fixtures.
use super::*;
use allocation_batch::{record, root, set_online, tables};
use cap_std::fs::{Dir, OpenOptions as CapOptions, OpenOptionsExt as CapOptionsExt};
use recovery::{Candidate, ScanMode};
use recovery_export::Plan;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    os::windows::{fs::MetadataExt, io::AsRawHandle},
};
use windows_sys::Win32::Storage::FileSystem::*;

struct Fixture {
    name: String,
    bytes: Vec<u8>,
    times: [u64; 3],
    attributes: u32,
}
struct DirectoryFixture {
    name: String,
    times: [u64; 3],
    attributes: u32,
}
struct Seed {
    files: Vec<Fixture>,
    directories: Vec<DirectoryFixture>,
}
pub(super) fn bind(dir: &Dir, source: &Selection) -> Result<(), String> {
    data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
        &dir.try_clone().map_err(err)?.into_std_file(),
        &source.target,
    )?])
}
pub(super) fn selected(serial: &str, filesystem: &str, size: u64) -> Result<Selection, String> {
    let source = selection(serial, None)?;
    let snapshot = storage::read_snapshot()?;
    let disk = snapshot
        .disks
        .iter()
        .find(|d| d.hardware.identity.as_deref() == Some(&source.target.0))
        .ok_or("Fixture disk absent")?;
    let found = disk
        .partitions
        .iter()
        .filter(|p| {
            p.base.size == size && !p.base.system && p.base.fs.eq_ignore_ascii_case(filesystem)
        })
        .collect::<Vec<_>>();
    if found.len() != 1 {
        return Err(format!(
            "Expected exactly one {filesystem} {size}-byte fixture"
        ));
    }
    selection(serial, Some(found[0].number))
}
pub(super) fn folder(source: &Selection, prefix: &str) -> Result<(Dir, PathBuf), String> {
    let resolved = data_windows::resolve(source, true, false)?;
    let serial = resolved
        .disk
        .serial_number
        .as_deref()
        .map(disk_identity::clean);
    if !matches!(serial, Some(A | B)) {
        return Err("Folder is not on A/B".into());
    }
    let path = root(&resolved)?;
    let parent = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).map_err(err)?;
    bind(&parent, source)?;
    let name = format!("{prefix}-{}", uuid::Uuid::new_v4().simple());
    parent.create_dir(&name).map_err(err)?;
    let dir = parent.open_dir(&name).map_err(err)?;
    bind(&dir, source)?;
    Ok((dir, path.join(name)))
}
fn seed(source: &Selection) -> Result<Seed, String> {
    let (dir, path) = folder(source, "Directory-Output-Source")?;
    let prefix = uuid::Uuid::new_v4().simple().to_string();
    let nested_name = format!("DS-{prefix}-nested");
    let empty_name = format!("DS-{prefix}-empty");
    dir.create_dir(&nested_name).map_err(err)?;
    let nested = dir.open_dir(&nested_name).map_err(err)?;
    bind(&nested, source)?;
    nested.create_dir(&empty_name).map_err(err)?;
    let empty = nested.open_dir(&empty_name).map_err(err)?;
    bind(&empty, source)?;
    let mut fixtures = vec![];
    for index in 0..8usize {
        let dir = if index < 4 { &dir } else { &nested };
        let name = format!("DS-{prefix}-{index}.bin");
        let bytes = (0..if index == 0 { 0 } else { 65553 + index * 512 })
            .map(|n| ((n * 17 + n / 31 + index) % 251) as u8)
            .collect::<Vec<_>>();
        let mut options = CapOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ);
        bind(&dir, source)?;
        let mut file = dir.open_with(&name, &options).map_err(err)?.into_std();
        let binding = data_windows::FileBinding::new(&file, &source.target)?;
        data_windows::FileBinding::validate_all(&[binding])?;
        file.write_all(&bytes).map_err(err)?;
        file.sync_all().map_err(err)?;
        // Independent Windows fixture setup, not the production output adapter.
        let info = FILE_BASIC_INFO {
            CreationTime: 133444736000000000,
            LastWriteTime: 133444736020000000,
            LastAccessTime: 133444736040000000,
            ChangeTime: 0,
            FileAttributes: FILE_ATTRIBUTE_ARCHIVE
                | FILE_ATTRIBUTE_HIDDEN
                | if index % 2 == 0 {
                    FILE_ATTRIBUTE_READONLY
                } else {
                    0
                },
        };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileBasicInfo,
                (&info as *const FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(err(std::io::Error::last_os_error()));
        }
        drop(file);
        let meta = dir.metadata(&name).map_err(err)?;
        // cap-std exposes portable timestamps; use a verified ordinary read handle
        // to capture native values after the last writer has closed.
        if meta.len() != bytes.len() as u64 {
            return Err("Seed length differs".into());
        }
        let file = dir.open(&name).map_err(err)?.into_std();
        data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
            &file,
            &source.target,
        )?])?;
        let meta = file.metadata().map_err(err)?;
        fixtures.push(Fixture {
            name,
            bytes,
            times: [
                meta.creation_time(),
                meta.last_write_time(),
                meta.last_access_time(),
            ],
            attributes: meta.file_attributes() & export_metadata::ATTRIBUTE_MASK,
        });
    }
    let root_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Seed name unavailable")?
        .to_string();
    let mut directories = vec![];
    // Seed directory properties only after every child/file writer has closed.
    for (name, folder) in [
        (&empty_name, &empty),
        (&nested_name, &nested),
        (&root_name, &dir),
    ] {
        let mut options = CapOptions::new();
        options
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let file = folder.open_with(".", &options).map_err(err)?.into_std();
        data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
            &file,
            &source.target,
        )?])?;
        let info = FILE_BASIC_INFO {
            CreationTime: 133444736000000000,
            LastWriteTime: 133444736020000000,
            LastAccessTime: 133444736040000000,
            ChangeTime: 0,
            FileAttributes: FILE_ATTRIBUTE_DIRECTORY
                | FILE_ATTRIBUTE_HIDDEN
                | FILE_ATTRIBUTE_ARCHIVE,
        };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileBasicInfo,
                (&info as *const FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(err(std::io::Error::last_os_error()));
        }
        drop(file);
        let metadata = folder
            .try_clone()
            .map_err(err)?
            .into_std_file()
            .metadata()
            .map_err(err)?;
        directories.push(DirectoryFixture {
            name: name.clone(),
            times: [
                metadata.creation_time(),
                metadata.last_write_time(),
                metadata.last_access_time(),
            ],
            attributes: metadata.file_attributes() & export_metadata::ATTRIBUTE_MASK,
        });
    }
    directories.reverse();
    Ok(Seed {
        files: fixtures,
        directories,
    })
}
fn scan(
    source: &Selection,
    fixtures: &[Fixture],
) -> Result<(Vec<Candidate>, recovery_tree::Catalog), String> {
    let resolved = data_windows::resolve(source, true, false)?;
    let locks = data_windows::lock(&resolved)?;
    let fresh = data_windows::resolve_locked(source, false, &resolved, &locks)?;
    let mut files = vec![];
    let mut catalog = recovery_tree::Catalog::default();
    recovery::scan_filesystem_owned(
        data_windows::reader(&fresh)?,
        ScanMode::Existing,
        Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| {
            if !c.deleted && fixtures.iter().any(|f| f.name == c.name) {
                files.push(c);
            }
        },
        |directory| catalog.insert(directory),
    )?;
    for fixture in fixtures {
        let found = files
            .iter()
            .filter(|c| c.name == fixture.name)
            .collect::<Vec<_>>();
        if found.len() != 1 || !found[0].supported || found[0].size != fixture.bytes.len() as u64 {
            return Err(format!(
                "Fixture candidate identity/size differs: {} count={} sizes={:?}",
                fixture.name,
                found.len(),
                found.iter().map(|c| c.size).collect::<Vec<_>>()
            ));
        }
    }
    drop(locks);
    Ok((files, catalog))
}
fn verify(
    parent: &Dir,
    target: &Selection,
    plan: &Plan,
    fixtures: &[Fixture],
    directory_fixtures: &[DirectoryFixture],
) -> Result<String, String> {
    bind(parent, target)?;
    let children = parent
        .entries()
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    if children.len() != 1 {
        return Err("Expected one new output root".into());
    }
    let dir = parent.open_dir(children[0].file_name()).map_err(err)?;
    bind(&dir, target)?;
    let manifest = dir.read(recovery_export::REPORT).map_err(err)?;
    let report: serde_json::Value = serde_json::from_slice(&manifest).map_err(err)?;
    if report["version"] != 3
        || report["completed"] != true
        || report["savedFiles"] != 8
        || report["failedFiles"] != 0
        || report["unprocessedFiles"] != 0
        || report["options"]["workers"] != plan.options.workers
    {
        return Err("Export completion report differs".into());
    }
    let wanted = fixtures
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect::<HashMap<_, _>>();
    for entry in &plan.entries {
        let expected = wanted
            .get(entry.original_name.as_str())
            .ok_or("Unexpected output entry")?;
        let mut file_dir = dir.try_clone().map_err(err)?;
        for index in plan.chain(entry.folder)? {
            file_dir = file_dir.open_dir(&plan.folders[index].name).map_err(err)?;
        }
        bind(&file_dir, target)?;
        let mut file = file_dir.open(&entry.name).map_err(err)?.into_std();
        data_windows::FileBinding::validate_all(&[data_windows::FileBinding::new(
            &file,
            &target.target,
        )?])?;
        let meta = file.metadata().map_err(err)?;
        if plan.options.timestamps
            && [
                meta.creation_time(),
                meta.last_write_time(),
                meta.last_access_time(),
            ] != expected.times
        {
            return Err(format!("Copied timestamps differ for {}", entry.name));
        }
        if plan.options.attributes
            && meta.file_attributes() & export_metadata::ATTRIBUTE_MASK != expected.attributes
        {
            return Err("Copied attributes differ".into());
        }
        let mut bytes = vec![];
        (&mut file)
            .take(expected.bytes.len() as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(err)?;
        if bytes != expected.bytes {
            return Err("Independent content comparison failed".into());
        }
        let outcomes = report["outcomes"].as_array().ok_or("Outcomes missing")?;
        let saved = outcomes
            .iter()
            .filter(|o| o["id"] == entry.id)
            .collect::<Vec<_>>();
        if saved.len() != 1
            || saved[0]["saved"] != true
            || saved[0]["sha256"] != format!("{:x}", Sha256::digest(&bytes))
        {
            return Err("Per-file content result differs".into());
        }
        let fields = saved[0]["metadata"]["fields"]
            .as_array()
            .ok_or("Metadata fields missing")?;
        if fields.len() != 4
            || fields.iter().any(|f| {
                f["status"]
                    != if plan.options.timestamps {
                        "preserved"
                    } else {
                        "disabled"
                    }
            })
        {
            return Err("Per-field metadata result differs".into());
        }
        if file_dir.exists(&entry.partial) {
            return Err("Successful file retained partial alias".into());
        }
    }
    if plan.folders.len() != 4
        || report["createdDirectories"] != 3
        || report["failedDirectories"] != 0
        || report["unprocessedDirectories"] != 0
    {
        return Err("Directory completion counts differ".into());
    }
    for (index, entry) in plan.folders.iter().enumerate().skip(1) {
        let expected = directory_fixtures
            .iter()
            .find(|f| f.name == entry.original_name)
            .ok_or("Unexpected directory")?;
        let mut folder = dir.try_clone().map_err(err)?;
        for at in plan.chain(index)? {
            folder = folder.open_dir(&plan.folders[at].name).map_err(err)?;
        }
        bind(&folder, target)?;
        let metadata = folder.into_std_file().metadata().map_err(err)?;
        if !metadata.is_dir()
            || (plan.options.timestamps
                && [
                    metadata.creation_time(),
                    metadata.last_write_time(),
                    metadata.last_access_time(),
                ] != expected.times)
            || (plan.options.attributes
                && metadata.file_attributes() & export_metadata::ATTRIBUTE_MASK
                    != expected.attributes)
        {
            return Err(format!(
                "Independent directory metadata differs: {}",
                entry.original_name
            ));
        }
        let outcomes = report["directoryOutcomes"]
            .as_array()
            .ok_or("Directory results missing")?;
        let outcome = outcomes
            .iter()
            .find(|o| o["folder"] == index)
            .ok_or("Directory outcome missing")?;
        let fields = outcome["metadata"]["fields"]
            .as_array()
            .ok_or("Directory metadata missing")?;
        if outcome["created"] != true
            || !outcome["metadata"]["error"].is_null()
            || fields.len() != 4
            || fields.iter().any(|f| {
                f["status"]
                    != if plan.options.timestamps {
                        "preserved"
                    } else {
                        "disabled"
                    }
            })
        {
            return Err("Per-directory metadata results differ".into());
        }
    }
    Ok(format!("{:x}", Sha256::digest(manifest)))
}
pub fn check() -> Result<String, String> {
    let mut log = backup_protection::begin("recovery-output")?;
    let result = (|| {
        set_online(A)?;
        set_online(B)?;
        let layouts = format_checks::layouts(&[])?;
        let gpt = tables()?;
        let target = selected(B, "NTFS", 6 * GIB)?;
        for (filesystem, size) in [("NTFS", 2 * GIB), ("exFAT", 512 * 1024 * 1024)] {
            let source = selected(A, filesystem, size)?;
            record(
                &mut log,
                serde_json::json!({"event":"seed-begin","fs":filesystem,"source":source}),
            )?;
            let fixtures = seed(&source)?;
            record(
                &mut log,
                serde_json::json!({"event":"scan-begin","fs":filesystem,"files":8}),
            )?;
            let (files, catalog) = scan(&source, &fixtures.files)?;
            let snapshot = catalog.snapshot();
            let roots = snapshot
                .iter()
                .filter(|d| d.name == fixtures.directories[0].name)
                .collect::<Vec<_>>();
            if roots.len() != 1 {
                return Err("Seed directory identity ambiguous".into());
            }
            for (workers, preserve) in [(1, true), (2, true), (4, true), (2, false)] {
                let (_, mut plan) =
                    recovery_export::plan_directory(&files, &catalog, &roots[0].id)?;
                plan.options = export_metadata::Options {
                    workers,
                    timestamps: preserve,
                    attributes: preserve,
                };
                let (parent, path) = folder(&target, "Recovery-Output-Destination")?;
                record(
                    &mut log,
                    serde_json::json!({"event":"copy-begin","fs":filesystem,"workers":workers,"preserve":preserve}),
                )?;
                run(Work::Recover {
                    source: source.clone().into(),
                    files: files.clone(),
                    folder: path,
                    export: plan.clone(),
                })?;
                let manifest_sha = verify(
                    &parent,
                    &target,
                    &plan,
                    &fixtures.files,
                    &fixtures.directories,
                )?;
                record(
                    &mut log,
                    serde_json::json!({"event":"case-pass","fs":filesystem,"workers":workers,"preserve":preserve,"files":8,"directories":3,"manifestSha256":manifest_sha,"independentBytesAndMetadata":true}),
                )?;
            }
        }
        if tables()? != gpt || format_checks::layouts(&[])? != layouts {
            return Err("GPT or partition layout changed".into());
        }
        record(
            &mut log,
            serde_json::json!({"event":"passed","cases":8,"files":64,"directories":24,"gptUnchanged":true,"allLayoutsUnchanged":true,"sourceFixtureRewrite":false}),
        )?;
        Ok("PASS: NTFS/exFAT -> B NTFS; 1/2/4 extraction workers, metadata on/off, 64 complete file comparisons and 24 nested directory metadata checks after writer close. GPT and all layouts unchanged. New fixture/output folders retained. Production-module integration; desktop UI acceptance is separate.".into())
    })();
    if let Err(error) = &result {
        record(
            &mut log,
            serde_json::json!({"event":"failed","error":error,"automaticRetry":false,"rollback":false}),
        )?;
    }
    result
}

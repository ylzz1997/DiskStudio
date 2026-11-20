//! Native sidecar records for explicit sector-copy continuation.
use super::{range_copy::ResumeState, Report, Request};
use crate::{
    data_tasks::Selection,
    data_windows::{self, FileBinding},
    partitions::Target,
    surface_scan::Range,
};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use std::os::windows::io::AsRawHandle;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT,
};

const SLOT: u64 = 4 * 1024 * 1024;
const HEADER: usize = 12;
const MAX_PAYLOAD: u32 = SLOT as u32 - HEADER as u32;
const MAGIC: &[u8; 8] = b"DSCOPY\0\x01";
const DOMAIN: &[u8] = b"DiskStudio sector copy checkpoint";

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Plan {
    pub request: Request,
    pub range: Range,
    pub source_capacity: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_id: Option<[u64; 2]>,
    pub output_disk: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DevicePlan>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DevicePlan {
    pub selection: Selection,
    pub start_sector: u64,
    pub range: Range,
    pub capacity: u64,
    pub table_digest: Option<[u8; 32]>,
}

#[derive(Deserialize, Serialize)]
struct Record {
    job: [u8; 16],
    sequence: u64,
    plan: Plan,
    state: ResumeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified: Option<Report>,
}

pub(super) struct Journal {
    file: File,
    bindings: [FileBinding; 2],
    pub(super) disk: Target,
    path: PathBuf,
    job: [u8; 16],
    sequence: Option<u64>,
}

pub(super) struct Location {
    directory: Dir,
    binding: FileBinding,
    path: PathBuf,
    leaf: String,
    pub disk: Target,
}

impl Location {
    pub(super) fn bind(
        path: &Path,
        source: &Target,
        destination: Option<&Target>,
    ) -> Result<Self, String> {
        let leaf = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|n| {
                !crate::recovery_export::reserved(n)
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| stem == crate::data_io::safe_name(stem))
                    && n.to_ascii_lowercase().ends_with(".dscopy")
            })
            .ok_or("请选择普通 .dscopy 文件。")?
            .to_owned();
        let parent = path.parent().ok_or("断点目录无效。")?;
        let (dir_path, disk) = data_windows::folder_location(parent)?;
        if &disk == source || Some(&disk) == destination {
            return Err("断点记录须保存在另一块物理磁盘上。".into());
        }
        let directory = Dir::open_ambient_dir(&dir_path, cap_std::ambient_authority())
            .map_err(data_windows::err)?;
        let binding = FileBinding::new(
            &directory
                .try_clone()
                .map_err(data_windows::err)?
                .into_std_file(),
            &disk,
        )?;
        FileBinding::validate_all(std::slice::from_ref(&binding))?;
        Ok(Self {
            directory,
            binding,
            path: dir_path.join(&leaf),
            leaf,
            disk,
        })
    }

    pub(super) fn ensure_unused(&self) -> Result<(), String> {
        match self.directory.symlink_metadata(&self.leaf) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err("所选断点文件已存在，请选择新位置。".into()),
            Err(error) => Err(data_windows::err(error)),
        }
    }

    pub(super) fn create(self) -> Result<Journal, String> {
        self.open(true)
    }

    fn open(self, create: bool) -> Result<Journal, String> {
        FileBinding::validate_all(std::slice::from_ref(&self.binding))?;
        let file = self
            .directory
            .open_with(
                &self.leaf,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(create)
                    .share_mode(0)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT),
            )
            .map_err(data_windows::err)?
            .into_std();
        let metadata = file.metadata().map_err(data_windows::err)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the live file handle and writable result buffer are valid.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(data_windows::err(std::io::Error::last_os_error()));
        }
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.len() > 2 * SLOT
            || info.nNumberOfLinks != 1
        {
            return Err("断点记录须为大小受限的普通文件。".into());
        }
        let bindings = [self.binding, FileBinding::new(&file, &self.disk)?];
        FileBinding::validate_all(&bindings)?;
        Ok(Journal {
            file,
            bindings,
            disk: self.disk,
            path: self.path,
            job: *uuid::Uuid::new_v4().as_bytes(),
            sequence: None,
        })
    }
}

impl Journal {
    pub(super) fn create_for_file(output: &Path, source: &Target) -> Result<Self, String> {
        let name = output
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("输出文件名无效")?;
        let path = output.with_file_name(format!("{name}.dscopy"));
        Location::bind(&path, source, None)?.create()
    }

    pub(super) fn open(path: &Path, source: &Target) -> Result<Opened, String> {
        let mut journal = Location::bind(path, source, None)?.open(false)?;
        let latest = read_latest(&mut journal.file)
            .ok_or("扇区复制断点损坏或无法由当前 Windows 用户打开。")?;
        if &latest.plan.request.source.target != source
            || latest.plan.output_disk != journal.disk
            || latest
                .plan
                .device
                .as_ref()
                .is_some_and(|device| device.selection.target == journal.disk)
            || latest.plan.device.is_some() == latest.plan.output_id.is_some()
        {
            return Err("扇区复制断点与当前源盘或输出磁盘不一致。".into());
        }
        journal.sequence = Some(latest.sequence);
        journal.job = latest.job;
        Ok(Opened {
            journal,
            plan: latest.plan,
            state: latest.state,
            verified: latest.verified,
        })
    }

    pub(super) fn output_path(&self) -> PathBuf {
        self.path.with_extension("")
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        FileBinding::validate_all(&self.bindings)
    }

    pub(super) fn has_state(&self) -> bool {
        self.sequence.is_some()
    }

    pub(super) fn commit(&mut self, plan: &Plan, state: &ResumeState) -> Result<(), String> {
        self.store(plan, state, None)
    }

    pub(super) fn finish(&mut self, plan: &Plan, report: &Report) -> Result<(), String> {
        self.validate()?;
        let latest = read_latest(&mut self.file).ok_or("扇区复制断点回读失败。")?;
        if Some(latest.sequence) != self.sequence
            || latest.job != self.job
            || latest.state.done() != plan.range.size
        {
            return Err("扇区复制断点回读失败。".into());
        }
        self.store(plan, &latest.state, Some(report.clone()))
    }

    fn store(
        &mut self,
        plan: &Plan,
        state: &ResumeState,
        verified: Option<Report>,
    ) -> Result<(), String> {
        let sequence = match self.sequence {
            Some(value) => value.checked_add(1).ok_or("扇区复制断点序号超出范围。")?,
            None => 0,
        };
        let bytes = encode_record(&Record {
            job: self.job,
            sequence,
            plan: plan.clone(),
            state: state.clone(),
            verified,
        })?;
        let offset = sequence % 2 * SLOT;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(data_windows::err)?;
        self.file.write_all(&bytes).map_err(data_windows::err)?;
        self.file.sync_all().map_err(data_windows::err)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(data_windows::err)?;
        let mut readback = vec![0; bytes.len()];
        self.file
            .read_exact(&mut readback)
            .map_err(data_windows::err)?;
        if readback != bytes {
            return Err("扇区复制断点回读失败。".into());
        }
        self.sequence = Some(sequence);
        Ok(())
    }
}

pub(super) struct Opened {
    pub journal: Journal,
    pub plan: Plan,
    pub state: ResumeState,
    pub verified: Option<Report>,
}

fn encode_record(record: &Record) -> Result<Vec<u8>, String> {
    let plaintext = serde_json::to_vec(record).map_err(data_windows::err)?;
    let protected = protect(&plaintext, true)?;
    let mut bytes = Vec::with_capacity(HEADER + protected.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(protected.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&protected);
    Ok(bytes)
}

fn read_latest(file: &mut (impl Read + Seek)) -> Option<Record> {
    let left = read_slot(file, 0);
    let right = read_slot(file, 1);
    match (left, right) {
        (Some(a), Some(b)) if a.job == b.job && a.sequence.abs_diff(b.sequence) == 1 => {
            Some(if a.sequence > b.sequence { a } else { b })
        }
        (Some(record), None) | (None, Some(record)) => Some(record),
        _ => None,
    }
}

fn read_slot(file: &mut (impl Read + Seek), slot: u64) -> Option<Record> {
    file.seek(SeekFrom::Start(slot * SLOT)).ok()?;
    let mut header = [0; HEADER];
    file.read_exact(&mut header).ok()?;
    let len = u32::from_le_bytes(header[8..].try_into().ok()?);
    if &header[..8] != MAGIC || !(1..=MAX_PAYLOAD).contains(&len) {
        return None;
    }
    let mut protected = vec![0; len as usize];
    file.read_exact(&mut protected).ok()?;
    let decoded = protect(&protected, false).ok()?;
    let record: Record = serde_json::from_slice(&decoded).ok()?;
    (record.sequence % 2 == slot).then_some(record)
}

#[cfg(test)]
#[test]
fn torn_slot_falls_back_and_two_unrelated_slots_are_rejected() {
    use super::{range_copy, Direction, Errors, Options, Selection};
    use std::io::Cursor;

    let range = Range {
        offset: 0,
        size: 4096,
        sector: 4096,
    };
    let plan = Plan {
        request: Request {
            source: Selection {
                target: Target("1".repeat(64)),
                layout_key: "layout".into(),
                partition: None,
            },
            options: Options {
                start_sector: 0,
                sector_count: 1,
            },
            direction: Direction::Forward,
            errors: Errors::Stop,
        },
        range,
        source_capacity: range.size,
        output_id: Some([23, 19]),
        output_disk: Target("2".repeat(64)),
        device: None,
    };
    let mut states = Vec::new();
    range_copy::copy_resumable(
        Cursor::new([0x55; 4096]),
        &mut Cursor::new(Vec::new()),
        range,
        Direction::Forward,
        range_copy::Policy::Stop,
        range_copy::Output::File,
        |_| Ok(()),
        Some(range_copy::Checkpoint {
            state: None,
            commit: &mut |state| {
                states.push(state.clone());
                Ok(())
            },
        }),
        |_, _| Ok(()),
    )
    .unwrap();
    let mut file = Cursor::new(Vec::new());
    let write = |file: &mut Cursor<Vec<u8>>, job, sequence| {
        let state = states[sequence as usize].clone();
        let record = Record {
            job,
            sequence,
            plan: plan.clone(),
            state,
            verified: None,
        };
        file.seek(SeekFrom::Start(sequence % 2 * SLOT)).unwrap();
        file.write_all(&encode_record(&record).unwrap()).unwrap();
    };
    write(&mut file, [1; 16], 0);
    write(&mut file, [1; 16], 1);
    assert_eq!(read_latest(&mut file).unwrap().state.done(), 4096);
    file.get_mut()[SLOT as usize + HEADER + 80] ^= 1;
    assert_eq!(read_latest(&mut file).unwrap().state.done(), 0);
    write(&mut file, [2; 16], 1);
    assert!(read_latest(&mut file).is_none());
}

fn protect(bytes: &[u8], seal: bool) -> Result<Vec<u8>, String> {
    if bytes.len() > MAX_PAYLOAD as usize {
        return Err("扇区复制断点超出大小限制。".into());
    }
    crate::windows_protection::protect(bytes, seal, Some(DOMAIN), MAX_PAYLOAD)
        .map_err(|_| "无法核对当前 Windows 用户的扇区复制断点凭据。".into())
}

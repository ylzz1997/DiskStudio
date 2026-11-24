//! Read the closed 4Kn registration QCOW2 fixtures as ordinary host files.
use ntfs::{indexes::NtfsFileNameIndex, Ntfs, NtfsReadSeek};
use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, ExitCode};

const SECTOR: u64 = 4096;
const DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
struct Mapping {
    start: u64,
    length: u64,
    depth: u32,
    zero: bool,
    data: bool,
    compressed: bool,
    offset: Option<u64>,
}

struct Readback {
    file: File,
    map: Vec<Mapping>,
    base: u64,
    size: u64,
    pos: u64,
}

impl Read for Readback {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let at = self.base + self.pos;
        let limit = bytes.len().min((self.size - self.pos) as usize);
        if limit == 0 {
            return Ok(0);
        }
        let i = self
            .map
            .partition_point(|entry| entry.start + entry.length <= at);
        let entry = &self.map[i];
        let count = limit.min((entry.start + entry.length - at) as usize);
        if entry.zero {
            bytes[..count].fill(0);
        } else {
            self.file
                .seek(SeekFrom::Start(entry.offset.unwrap() + at - entry.start))?;
            self.file.read_exact(&mut bytes[..count])?;
        }
        self.pos += count as u64;
        Ok(count)
    }
}

impl Seek for Readback {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.size) + i128::from(delta),
        };
        if !(0..=i128::from(self.size)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fixture seek out of range",
            ));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

fn readback(qemu: &Path, path: &Path) -> Result<Readback> {
    let metadata = path.symlink_metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
        return Err("Expected an ordinary QCOW2 fixture file".into());
    }
    let output = Command::new(qemu)
        .args(["map", "--output=json", "-f", "qcow2"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot map closed fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let map: Vec<Mapping> = serde_json::from_slice(&output.stdout)?;
    let mut end = 0;
    for entry in &map {
        if entry.start != end
            || entry.length == 0
            || entry.length > DISK_BYTES - end
            || entry.depth != 0
            || entry.compressed
            || (!entry.zero
                && (!entry.data
                    || entry.offset.is_none_or(|at| {
                        at.checked_add(entry.length)
                            .is_none_or(|end| end > metadata.len())
                    })))
        {
            return Err("Unexpected QCOW2 extent map".into());
        }
        end += entry.length;
    }
    if end != DISK_BYTES {
        return Err("Unexpected fixture virtual size".into());
    }
    let mut reader = Readback {
        file: File::open(path)?,
        map,
        base: 0,
        size: DISK_BYTES,
        pos: 0,
    };
    let mut header = [0; 92];
    reader.seek(SeekFrom::Start(SECTOR))?;
    reader.read_exact(&mut header)?;
    let wide = |data: &[u8], at| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    if &header[..8] != b"EFI PART" || wide(&header, 24) != 1 || wide(&header, 72) != 2 {
        return Err("Expected the primary 4Kn GPT fixture table".into());
    }
    let mut entry = [0; 128];
    reader.seek(SeekFrom::Start(2 * SECTOR))?;
    reader.read_exact(&mut entry)?;
    const BASIC_DATA: [u8; 16] = [
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ];
    let (first, last) = (wide(&entry, 32), wide(&entry, 40));
    if entry[..16] != BASIC_DATA || first != 256 || last <= first || last >= DISK_BYTES / SECTOR - 1
    {
        return Err("Unexpected NTFS fixture partition bounds".into());
    }
    reader.base = first * SECTOR;
    reader.size = (last - first + 1) * SECTOR;
    reader.pos = 0;
    Ok(reader)
}

fn extract(
    qemu: &Path,
    fixture: &Path,
    output: &Path,
    files: &[&str],
    absent: &[&str],
) -> Result<()> {
    let mut reader = readback(qemu, fixture)?;
    let mut ntfs = Ntfs::new(&mut reader)?;
    if u64::from(ntfs.sector_size()) != SECTOR {
        return Err("Expected a 4Kn NTFS fixture volume".into());
    }
    ntfs.read_upcase_table(&mut reader)?;
    for (paths, must_exist) in [(files, true), (absent, false)] {
        for path in paths {
            let components = path.split('/').collect::<Vec<_>>();
            let mut file = Some(ntfs.root_directory(&mut reader)?);
            for name in components {
                let Some(parent) = file else { break };
                let index = parent.directory_index(&mut reader)?;
                file = match NtfsFileNameIndex::find(&mut index.finder(), &ntfs, &mut reader, name)
                {
                    Some(entry) => Some(entry?.to_file(&ntfs, &mut reader)?),
                    None => None,
                };
            }
            match (file, must_exist) {
                (None, false) => continue,
                (Some(file), true) => {
                    let item = file.data(&mut reader, "").ok_or("Missing file stream")??;
                    let mut data = item.to_attribute()?.value(&mut reader)?;
                    let size = usize::try_from(data.len())?;
                    if size > 16 * 1024 * 1024 {
                        return Err("Unexpected readback file size".into());
                    }
                    let mut bytes = vec![0; size];
                    let mut done = 0;
                    while done < size {
                        let read = data.read(&mut reader, &mut bytes[done..])?;
                        if read == 0 {
                            return Err("Truncated file stream".into());
                        }
                        done += read;
                    }
                    let out = output.join(path);
                    fs::create_dir_all(out.parent().unwrap())?;
                    match OpenOptions::new().create_new(true).write(true).open(&out) {
                        Ok(mut output) => output.write_all(&bytes)?,
                        Err(error)
                            if error.kind() == io::ErrorKind::AlreadyExists
                                && fs::read(&out)? == bytes => {}
                        Err(error) => return Err(error.into()),
                    }
                    println!("{path} · {size} bytes");
                }
                _ => return Err(format!("Unexpected presence of fixture file {path}").into()),
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Inputs {
    disks: Vec<Disk>,
}

#[derive(Deserialize)]
struct Disk {
    file: String,
}

fn run(physical: bool) -> Result<()> {
    let vm = std::path::absolute(".vm")?;
    let (log, out, prefix) = if physical {
        (
            "winpe-physical-live.log",
            "winpe-physical-readback",
            "winpe-physical-4kn",
        )
    } else {
        (
            "winpe-4kn-live.log",
            "winpe-4kn-readback",
            "winpe-register-4kn",
        )
    };
    let log = fs::read_to_string(vm.join(log))?;
    if log.lines().last() != Some(r#"{"shutdown":true,"exitCode":0}"#) {
        return Err("The WinPE fixture has no recorded clean shutdown".into());
    }
    let first: serde_json::Value =
        serde_json::from_str(log.lines().next().ok_or("Missing fixture run")?)?;
    if first["name"] != "DiskStudio-Test"
        || first["uuid"] != "e20adb9f-f665-432e-9a62-dd5d897b06a0"
        || first["images"]
            .as_array()
            .is_none_or(|images| images.len() != 5)
        || first["disks"]
            .as_array()
            .is_none_or(|disks| disks.len() != 2)
    {
        return Err("Unexpected closed WinPE fixture identity or inventory".into());
    }
    let run_id = uuid::Uuid::parse_str(first["runId"].as_str().ok_or("Missing fixture identity")?)?;
    let disks: Vec<Disk> = if physical {
        serde_json::from_slice(&fs::read(vm.join("winpe-physical-disks.json"))?)?
    } else {
        let inputs: Inputs =
            serde_json::from_slice(&fs::read(vm.join("winpe-registration-4kn-inputs.json"))?)?;
        inputs.disks
    };
    let [a, b] = disks.as_slice() else {
        return Err("Expected exactly two fixture files".into());
    };
    let output = vm.join(format!("{out}-{run_id}"));
    let qemu = vm.join("tools/qemu/qemu-img.exe");
    for (index, side, disk) in [(0, "a", a), (1, "b", b)] {
        let suffix = disk
            .file
            .strip_prefix(&format!("test-{side}-{prefix}-"))
            .and_then(|value| value.strip_suffix(".qcow2"))
            .ok_or("Unexpected fixture filename")?;
        uuid::Uuid::parse_str(suffix)?;
        let file = vm.join("windows").join(&disk.file);
        let expected = &first["disks"][index];
        let serial = format!("DS-TEST-{}-0001", side.to_ascii_uppercase());
        if expected["file"].as_str().map(Path::new) != Some(file.as_path())
            || expected["serial"] != serial.as_str()
            || expected["virtualBytes"].as_u64() != Some(DISK_BYTES)
            || expected["logicalSector"].as_u64() != Some(SECTOR)
            || expected["physicalSector"].as_u64() != Some(SECTOR)
        {
            return Err("The fixture file differs from the QMP preflight".into());
        }
        let dir = output.join(side);
        if physical {
            if side == "b" {
                extract(
                    &qemu,
                    &file,
                    &dir,
                    &[
                        "output/existing.img",
                        "output/physical-existing/DOCS/NOTE.TXT",
                        "output/physical-existing/DiskStudio-export.json",
                        "output/physical-deleted/_OST.TXT",
                        "output/physical-deleted/DiskStudio-export.json",
                    ],
                    &[
                        "output/physical-existing/DOCS/DiskStudio-file-000000.partial",
                        "output/physical-existing/DOCS/DiskStudio-file-000001.partial",
                        "output/physical-deleted/DiskStudio-file-000000.partial",
                        "output/physical-deleted/DiskStudio-file-000001.partial",
                    ],
                )?;
            }
        } else if side == "a" {
            extract(
                &qemu,
                &file,
                &dir,
                &[
                    "cases/library.img",
                    "cases/searchboth.img",
                    "cases/searchfree.img",
                ],
                &["cases/same-disk.img"],
            )?;
        } else {
            extract(
                &qemu,
                &file,
                &dir,
                &[
                    "output/existing.img",
                    "output/registered-gpt.img",
                    "output/registered-mbr.img",
                    "output/registered-existing.img",
                    "output/existing-export/DOCS/NOTE.TXT",
                    "output/existing-export/DiskStudio-export.json",
                    "output/deleted-export/_OST.TXT",
                    "output/deleted-export/DiskStudio-export.json",
                ],
                &[
                    "output/registered-gpt.img.partial",
                    "output/registered-mbr.img.partial",
                    "output/registered-existing.img.partial",
                    "output/existing-export/DOCS/DiskStudio-file-000001.partial",
                    "output/deleted-export/DiskStudio-file-000001.partial",
                ],
            )?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let physical = match (args.next(), args.next()) {
        (None, None) => false,
        (Some(mode), None) if mode == "--physical" => true,
        _ => return ExitCode::FAILURE,
    };
    match run(physical) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("4Kn fixture readback: {error}");
            ExitCode::FAILURE
        }
    }
}

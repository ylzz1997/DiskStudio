//! Inspect conventional Windows boot paths using a bounded read-only view.
use crate::{
    backup_archive::Source,
    boot_configuration::{self, Device, Entry},
    data_io::Region,
    fat_recovery, image_browser,
};
use disk_core::table_check::GptIdentity;
use ntfs::{indexes::NtfsFileNameIndex, Ntfs, NtfsFile, NtfsFileFlags, NtfsReadSeek};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom};
use uuid::Uuid;

#[path = "boot_repair.rs"]
pub(crate) mod repair;

const MIB: usize = 1024 * 1024;
const MAX_EXECUTABLE: usize = 16 * MIB;
const MAX_VOLUMES: usize = 16;
const EFI_BCD: &[&str] = &["EFI", "Microsoft", "Boot", "BCD"];
const BIOS_BCD: &[&str] = &["Boot", "BCD"];
const EFI_MANAGER: &[&str] = &["EFI", "Microsoft", "Boot", "bootmgfw.efi"];
const BIOS_MANAGER: &[&str] = &["bootmgr"];
const FALLBACKS: [(&[&str], u16); 3] = [
    (&["EFI", "Boot", "bootx64.efi"], 0x8664),
    (&["EFI", "Boot", "bootia32.efi"], 0x014c),
    (&["EFI", "Boot", "bootaa64.efi"], 0xaa64),
];

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Mode {
    Uefi,
    Bios,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    source: String,
    scheme: String,
    layout_warning: String,
    mbr_code_present: Option<bool>,
    repair_partitions: Vec<repair::Target>,
    volumes: Vec<VolumeReport>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VolumeReport {
    name: String,
    offset: u64,
    size: u64,
    mode: Mode,
    filesystem: Option<&'static str>,
    boot_signature: bool,
    repair_volume: Option<Uuid>,
    error: Option<String>,
    artifacts: Vec<Artifact>,
    store: Option<boot_configuration::Store>,
    references: Vec<EntryCheck>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    start: u64,
    size: u64,
}

enum Layout {
    Partition,
    Gpt(GptIdentity),
    Mbr {
        signature: u32,
        extents: Vec<Extent>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum BindingState {
    Matched,
    Relative,
    OutsideSource,
    MissingPartition,
    NoDiskIdentity,
    Unresolved,
    Invalid,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    device: Device,
    state: BindingState,
    offset: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryCheck {
    id: Uuid,
    application: Option<Binding>,
    system: Option<Binding>,
    loader: Option<Artifact>,
    system_root: Option<Artifact>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    path: String,
    state: &'static str,
    bytes: Option<usize>,
    sha256: Option<String>,
    machine: Option<u16>,
    detail: Option<String>,
}

impl Artifact {
    fn new(path: &[&str]) -> Self {
        Self::at(format!("\\{}", path.join("\\")))
    }

    fn at(path: String) -> Self {
        Self {
            path,
            state: "missing",
            bytes: None,
            sha256: None,
            machine: None,
            detail: None,
        }
    }

    fn read(&mut self, result: Result<Option<Vec<u8>>, String>) -> Option<Vec<u8>> {
        match result {
            Ok(Some(bytes)) => {
                self.bytes = Some(bytes.len());
                self.sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
                self.state = "present";
                Some(bytes)
            }
            Ok(None) => None,
            Err(error) => {
                self.state = "unreadable";
                self.detail = Some(error);
                None
            }
        }
    }

    fn invalid(&mut self, detail: impl Into<String>) {
        self.state = "invalid";
        self.detail = Some(detail.into());
    }
}

impl Layout {
    fn bind(&self, device: Device, boot: Extent) -> (Binding, Option<Extent>) {
        use BindingState::*;
        let (state, extent) = match (self, device) {
            (_, Device::Boot) => (Relative, Some(boot)),
            (_, Device::Invalid) => (Invalid, None),
            (_, Device::Unresolved { .. }) => (Unresolved, None),
            (Self::Partition | Self::Mbr { signature: 0, .. }, _) => (NoDiskIdentity, None),
            (Self::Gpt(layout), Device::Gpt { disk, partition })
                if disk.to_bytes_le() == layout.disk_guid =>
            {
                layout
                    .partitions
                    .iter()
                    .find(|entry| entry.guid == partition.to_bytes_le())
                    .map(|entry| {
                        (
                            Matched,
                            Some(Extent {
                                start: entry.range.start,
                                size: entry.range.end - entry.range.start,
                            }),
                        )
                    })
                    .unwrap_or((MissingPartition, None))
            }
            (
                Self::Mbr { signature, extents },
                Device::Mbr {
                    signature: id,
                    offset,
                },
            ) if *signature == id => extents
                .iter()
                .find(|extent| extent.start == offset)
                .map(|extent| (Matched, Some(*extent)))
                .unwrap_or((MissingPartition, None)),
            _ => (OutsideSource, None),
        };
        let binding = Binding {
            device,
            state,
            offset: extent.map(|range| range.start),
        };
        (binding, extent)
    }
}

struct Budget<R> {
    reader: R,
    bytes: usize,
    calls: usize,
}

impl<R> Budget<R> {
    fn charge(&mut self) -> io::Result<()> {
        self.calls = self
            .calls
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("Boot inspection read budget exhausted"))?;
        Ok(())
    }
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.charge()?;
        if bytes.len() > self.bytes {
            return Err(io::Error::other("Boot inspection byte budget exhausted"));
        }
        let count = self.reader.read(bytes)?;
        self.bytes -= count;
        Ok(count)
    }
}

impl<R: Seek> Seek for Budget<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.charge()?;
        self.reader.seek(from)
    }
}

enum Filesystem {
    Fat(fat_recovery::boot_files::Volume),
    Ntfs(Box<Ntfs>),
}

impl Filesystem {
    fn open(
        reader: &mut (impl Read + Seek),
        boot: &[u8; 512],
        sector: u64,
        allow_ntfs: bool,
    ) -> Result<Self, String> {
        reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        if &boot[3..11] == b"NTFS    " && allow_ntfs {
            let mut ntfs = Ntfs::new(reader).map_err(|e| e.to_string())?;
            if u64::from(ntfs.sector_size()) != sector {
                return Err("NTFS boot geometry differs from the logical sector size".into());
            }
            ntfs.read_upcase_table(reader).map_err(|e| e.to_string())?;
            return Ok(Self::Ntfs(Box::new(ntfs)));
        }
        if &boot[3..11] == b"EXFAT   "
            || !crate::recovery::fat_boot(boot)
            || u64::from(u16::from_le_bytes([boot[11], boot[12]])) != sector
        {
            return Err(
                "Boot partition filesystem is unsupported or its sector size differs".into(),
            );
        }
        Ok(Self::Fat(fat_recovery::boot_files::Volume::open(reader)?))
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Fat(volume) => match volume.bits() {
                12 => "FAT12",
                16 => "FAT16",
                _ => "FAT32",
            },
            Self::Ntfs(_) => "NTFS",
        }
    }

    fn read_file(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
        limit: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::Fat(volume) => volume.read_file(reader, path, limit),
            Self::Ntfs(ntfs) => ntfs_file(ntfs, reader, path, limit),
        }
    }

    fn directory_exists(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
    ) -> Result<bool, String> {
        match self {
            Self::Fat(volume) => volume.directory_exists(reader, path),
            Self::Ntfs(ntfs) => match ntfs_lookup(ntfs, reader, path)? {
                Some(file) if file.is_directory() => file
                    .directory_index(reader)
                    .map(|_| true)
                    .map_err(|error| error.to_string()),
                Some(_) => Err("NTFS boot path refers to a file".into()),
                None => Ok(false),
            },
        }
    }
}

fn ntfs_lookup<'n>(
    ntfs: &'n Ntfs,
    reader: &mut (impl Read + Seek),
    path: &[&str],
) -> Result<Option<NtfsFile<'n>>, String> {
    let err = |e: ntfs::NtfsError| e.to_string();
    let mut file = ntfs.root_directory(reader).map_err(err)?;
    for component in path {
        file = {
            let index = file.directory_index(reader).map_err(err)?;
            let mut finder = index.finder();
            let Some(entry) = NtfsFileNameIndex::find(&mut finder, ntfs, reader, component)
                .transpose()
                .map_err(err)?
            else {
                return Ok(None);
            };
            let reference = entry.file_reference();
            let file = reference.to_file(ntfs, reader).map_err(err)?;
            if reference.sequence_number() != file.sequence_number() {
                return Err("NTFS boot path points to a reused file record".into());
            }
            file
        };
        if !file.flags().contains(NtfsFileFlags::IN_USE)
            || crate::ntfs_compression::is_reparse(reader, &file)?
        {
            return Err("NTFS boot path is stale or traverses a reparse point".into());
        }
    }
    Ok(Some(file))
}

fn ntfs_file(
    ntfs: &Ntfs,
    reader: &mut (impl Read + Seek),
    path: &[&str],
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let err = |e: ntfs::NtfsError| e.to_string();
    let Some(file) = ntfs_lookup(ntfs, reader, path)? else {
        return Ok(None);
    };
    if file.is_directory() {
        return Err("NTFS boot path refers to a directory".into());
    }
    let item = file
        .data(reader, "")
        .transpose()
        .map_err(err)?
        .ok_or("NTFS boot file has no unnamed data stream")?;
    let attribute = item.to_attribute().map_err(err)?;
    if !attribute.flags().is_empty() || attribute.value_length() > limit as u64 {
        return Err(
            "NTFS boot file exceeds the limit or uses unsupported data stream flags".into(),
        );
    }
    let mut bytes = vec![0; attribute.value_length() as usize];
    let mut value = attribute.value(reader).map_err(err)?;
    value.read_exact(reader, &mut bytes).map_err(err)?;
    Ok(Some(bytes))
}

fn pe_machine(bytes: &[u8], subsystems: &[u16]) -> Result<u16, &'static str> {
    let dos = bytes.get(..64).ok_or("Truncated DOS header")?;
    if &dos[..2] != b"MZ" {
        return Err("Missing DOS executable signature");
    }
    let at = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as usize;
    let pe = bytes
        .get(at..)
        .filter(|rest| at >= 64 && rest.len() >= 24)
        .ok_or("Truncated PE header")?;
    if &pe[..4] != b"PE\0\0" {
        return Err("Missing PE signature");
    }
    let word = |pos| u16::from_le_bytes([pe[pos], pe[pos + 1]]);
    let size = usize::from(word(20));
    let sections = usize::from(word(6));
    let machine = word(4);
    if size < 70
        || 24 + size + sections * 40 > pe.len()
        || !(1..=96).contains(&sections)
        || word(22) & 2 == 0
    {
        return Err("Malformed PE optional header or section table");
    }
    let magic = word(24);
    if !subsystems.contains(&word(24 + 68))
        || !matches!(
            (machine, magic),
            (0x014c | 0x01c2 | 0x01c4, 0x10b) | (0x8664 | 0xaa64, 0x20b)
        )
    {
        return Err("PE machine type or subsystem does not match the boot application");
    }
    Ok(machine)
}

fn files(
    report: &mut VolumeReport,
    reader: &mut (impl Read + Seek),
    sector: u64,
) -> Result<(), String> {
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(|e| e.to_string())?;
    report.boot_signature = boot[510..] == [0x55, 0xaa];
    let filesystem = Filesystem::open(reader, &boot, sector, report.mode == Mode::Bios)?;
    report.filesystem = Some(filesystem.name());
    let (manager, config) = if report.mode == Mode::Uefi {
        (EFI_MANAGER, EFI_BCD)
    } else {
        (BIOS_MANAGER, BIOS_BCD)
    };
    let mut candidates = vec![(manager, None)];
    if report.mode == Mode::Uefi {
        candidates.extend(FALLBACKS.map(|(name, machine)| (name, Some(machine))));
    }
    for (path, expected) in candidates {
        let mut artifact = Artifact::new(path);
        if let Some(bytes) = artifact.read(filesystem.read_file(reader, path, MAX_EXECUTABLE)) {
            if report.mode == Mode::Uefi {
                match pe_machine(&bytes, &[10]) {
                    Ok(machine) if expected.is_none_or(|ty| ty == machine) => {
                        artifact.machine = Some(machine)
                    }
                    Ok(_) => artifact.invalid("EFI fallback filename and machine type disagree"),
                    Err(error) => artifact.invalid(error),
                }
            } else if bytes.is_empty() {
                artifact.invalid("The Windows boot manager file is empty");
            }
        }
        report.artifacts.push(artifact);
    }
    let mut artifact = Artifact::new(config);
    if let Some(bytes) =
        artifact.read(filesystem.read_file(reader, config, boot_configuration::MAX_STORE))
    {
        match boot_configuration::parse(bytes) {
            Ok(store) => report.store = Some(store),
            Err(error) => artifact.invalid(error),
        }
    }
    report.artifacts.push(artifact);
    Ok(())
}

fn boot_path(path: &str) -> Result<Vec<&str>, &'static str> {
    let rest = path
        .strip_prefix('\\')
        .filter(|_| path.is_ascii() && path.len() <= 1024)
        .ok_or("BCD path is outside the supported partition-relative syntax")?;
    let parts: Vec<_> = rest.split('\\').collect();
    if parts.len() > 24
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 255
                || part.ends_with(['.', ' '])
                || part.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
                || part.bytes().any(|b| b.is_ascii_control())
        })
    {
        return Err("BCD path is outside the supported partition-relative syntax");
    }
    Ok(parts)
}

fn open_extent(
    reader: &mut (impl Read + Seek),
    extent: Extent,
    sector: u64,
) -> Result<(impl Read + Seek + '_, Filesystem), String> {
    let mut view = Region::for_file_view(reader, extent.start, extent.size)
        .map_err(|error| error.to_string())?;
    let mut boot = [0; 512];
    view.read_exact(&mut boot)
        .map_err(|error| error.to_string())?;
    if boot[510..] != [0x55, 0xaa] {
        return Err("Referenced partition has no boot sector signature".into());
    }
    let filesystem = Filesystem::open(&mut view, &boot, sector, true)?;
    Ok((view, filesystem))
}

fn check_path(
    reader: &mut (impl Read + Seek),
    extent: Extent,
    sector: u64,
    path: &str,
    loader_mode: Option<Mode>,
    directory: bool,
) -> Artifact {
    let mut artifact = Artifact::at(path.into());
    let components = match boot_path(path) {
        Ok(path) => path,
        Err(error) => {
            artifact.invalid(error);
            return artifact;
        }
    };
    let subsystems: &[u16] = match (loader_mode, path.rsplit_once('.')) {
        (None, _) => &[],
        (Some(Mode::Uefi), Some((_, extension))) if extension.eq_ignore_ascii_case("efi") => {
            &[10, 16]
        }
        (Some(Mode::Bios), Some((_, extension))) if extension.eq_ignore_ascii_case("exe") => &[16],
        _ => {
            artifact.invalid("BCD loader path extension does not match the boot mode");
            return artifact;
        }
    };
    match open_extent(reader, extent, sector) {
        Ok((mut view, filesystem)) if directory => {
            match filesystem.directory_exists(&mut view, &components) {
                Ok(true) => artifact.state = "present",
                Ok(false) => (),
                Err(error) => {
                    artifact.read(Err(error));
                }
            }
        }
        Ok((mut view, filesystem)) => {
            if let Some(bytes) =
                artifact.read(filesystem.read_file(&mut view, &components, MAX_EXECUTABLE))
            {
                if bytes.is_empty() {
                    artifact.invalid("The BCD application file is empty");
                } else if !subsystems.is_empty() {
                    match pe_machine(&bytes, subsystems) {
                        Ok(machine) => artifact.machine = Some(machine),
                        Err(error) => artifact.invalid(error),
                    }
                }
            }
        }
        Err(error) => {
            artifact.read(Err(error));
        }
    }
    artifact
}

fn check_entry(
    reader: &mut (impl Read + Seek),
    layout: &Layout,
    boot: Extent,
    sector: u64,
    mode: Mode,
    entry: &Entry,
) -> EntryCheck {
    let (application, application_extent) = entry
        .device
        .map(|device| layout.bind(device, boot))
        .map_or((None, None), |(result, extent)| (Some(result), extent));
    let (system, system_extent) = entry
        .os_device
        .map(|device| layout.bind(device, boot))
        .map_or((None, None), |(result, extent)| (Some(result), extent));
    let application_type = entry.object_type & 0x000f_ffff;
    let loader_mode = matches!(application_type, 3..=5)
        .then_some(mode)
        .or_else(|| (application_type == 2 && mode == Mode::Uefi).then_some(mode));
    let loader = entry
        .path
        .as_deref()
        .zip(application_extent)
        .map(|(path, extent)| check_path(reader, extent, sector, path, loader_mode, false));
    let system_root = entry
        .system_root
        .as_deref()
        .zip(system_extent)
        .map(|(path, extent)| check_path(reader, extent, sector, path, None, true));
    EntryCheck {
        id: entry.id,
        application,
        system,
        loader,
        system_root,
    }
}

pub(crate) fn inspect(
    mut source: impl Source,
    name: String,
    size: u64,
    sector: u64,
    partition: bool,
    expected_ranges: Option<&[crate::used_space::Span]>,
) -> Result<Report, String> {
    if !matches!(sector, 512 | 4096) || size < sector || size % sector != 0 {
        return Err("Invalid boot inspection source geometry".into());
    }
    let (scheme, layout_warning, mbr_code_present, volumes, layout, repair_partitions) = if partition {
        (
            String::from("partition"),
            String::new(),
            None,
            vec![
                (name.clone(), 0, size, Mode::Uefi),
                (name.clone(), 0, size, Mode::Bios),
            ],
            Layout::Partition,
            Vec::new(),
        )
    } else {
        // This reader is already a logical disk, regardless of the display
        // label, the archive name, or the original image container.
        let image_browser::CheckedLayout {
            disk,
            warning,
            head,
            gpt_identity,
        } = image_browser::checked_layout(
            &mut source,
            "boot-source.img",
            size,
            sector,
            &mut || Ok(()),
        )?;
        let mut parts: Vec<_> = disk
            .partitions
            .into_iter()
            .filter(|p| p.color != "free")
            .collect();
        if let Some(expected) = expected_ranges {
            parts.sort_by_key(|p| p.start);
            if parts.len() != expected.len()
                || !parts.iter().zip(expected).all(|(p, expected)| {
                    p.start == expected.start && p.start + p.size == expected.end
                })
            {
                return Err("Boot disk table differs from the verified native layout".into());
            }
        }
        let (mbr, mode) = match disk.scheme.as_str() {
            "GPT" => (None, Mode::Uefi),
            "MBR" => (Some(head[..440].iter().any(|&b| b != 0)), Mode::Bios),
            _ => return Err("System boot inspection requires a checked GPT or MBR layout".into()),
        };
        let layout = match mode {
            Mode::Uefi => Layout::Gpt(gpt_identity.ok_or("GPT identity could not be verified")?),
            Mode::Bios => Layout::Mbr {
                signature: u32::from_le_bytes(head[440..444].try_into().unwrap()),
                extents: parts
                    .iter()
                    .map(|part| Extent {
                        start: part.start,
                        size: part.size,
                    })
                    .collect(),
            },
        };
        let repair_partitions = match &layout {
            Layout::Gpt(identity) if expected_ranges.is_some() => repair::targets(identity, &parts),
            _ => Vec::new(),
        };
        let active = |offset, len| {
            head[446..510].chunks_exact(16).any(|entry| {
                let lba = u32::from_le_bytes(entry[8..12].try_into().unwrap());
                let sectors = u32::from_le_bytes(entry[12..16].try_into().unwrap());
                entry[0] == 0x80
                    && entry[4] != 0
                    && u64::from(lba) * sector == offset
                    && u64::from(sectors) * sector == len
            })
        };
        let volumes = parts
            .into_iter()
            .filter(|p| match mode {
                Mode::Uefi => p.system,
                Mode::Bios => active(p.start, p.size),
            })
            .map(|p| (p.name, p.start, p.size, mode))
            .collect();
        (disk.scheme, warning, mbr, volumes, layout, repair_partitions)
    };
    if volumes.len() > MAX_VOLUMES {
        return Err("Too many candidate boot partitions".into());
    }
    let mut results = Vec::new();
    // A budget is shared by all candidates, including repeated traversal of a
    // standalone partition and reads in partitions named by qualified BCD
    // references. A path stays inside its checked extent.
    let mut budget = Budget {
        reader: source,
        bytes: 192 * MIB,
        calls: 262_144,
    };
    for (name, offset, size, mode) in volumes {
        let repair_volume = match &layout {
            Layout::Gpt(identity) if expected_ranges.is_some() => repair::boot_partition(identity, offset),
            _ => None,
        };
        let mut report = VolumeReport {
            name,
            offset,
            size,
            mode,
            filesystem: None,
            boot_signature: false,
            repair_volume,
            error: None,
            artifacts: Vec::new(),
            store: None,
            references: Vec::new(),
        };
        let view = Region::for_file_view(&mut budget, offset, size).map_err(|e| e.to_string())?;
        let mut volume = Budget {
            reader: view,
            bytes: 96 * MIB,
            calls: 262_144,
        };
        if let Err(error) = files(&mut report, &mut volume, sector) {
            report.error = Some(error);
        }
        drop(volume);
        if let Some(store) = &report.store {
            let boot = Extent {
                start: offset,
                size,
            };
            report.references = store
                .manager
                .iter()
                .chain(&store.entries)
                .map(|entry| check_entry(&mut budget, &layout, boot, sector, mode, entry))
                .collect();
        }
        results.push(report);
    }
    Ok(Report {
        source: name,
        scheme,
        layout_warning,
        mbr_code_present,
        repair_partitions,
        volumes: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn qualified_devices_require_matching_disk_and_exact_partition_identity() {
        let boot = Extent {
            start: 1024,
            size: 512,
        };
        let disk = Uuid::from_u128(0x11223344_5566_7788_99aa_bbccddee0011);
        let partition = Uuid::from_u128(0x22334455_6677_8899_aabb_ccddee001122);
        let layout = Layout::Gpt(GptIdentity {
            disk_guid: disk.to_bytes_le(),
            partitions: vec![disk_core::table_check::GptPartitionIdentity {
                guid: partition.to_bytes_le(), kind: [0; 16], attributes: 0, range: 4096..8192,
            }],
        });
        let device = Device::Gpt { disk, partition };
        assert_eq!(
            layout.bind(device, boot).1,
            Some(Extent {
                start: 4096,
                size: 4096
            })
        );
        assert_eq!(
            layout
                .bind(
                    Device::Gpt {
                        disk: partition,
                        partition
                    },
                    boot
                )
                .0
                .state,
            BindingState::OutsideSource
        );
        assert_eq!(
            layout
                .bind(
                    Device::Gpt {
                        disk,
                        partition: disk
                    },
                    boot
                )
                .0
                .state,
            BindingState::MissingPartition
        );
        assert_eq!(
            Layout::Partition.bind(device, boot).0.state,
            BindingState::NoDiskIdentity
        );
        assert_eq!(Layout::Partition.bind(Device::Boot, boot).1, Some(boot));

        let layout = Layout::Mbr {
            signature: 123,
            extents: vec![boot],
        };
        assert_eq!(
            layout
                .bind(
                    Device::Mbr {
                        signature: 123,
                        offset: 1024
                    },
                    boot
                )
                .1,
            Some(boot)
        );
        assert_eq!(
            layout
                .bind(
                    Device::Mbr {
                        signature: 123,
                        offset: 1025
                    },
                    boot
                )
                .0
                .state,
            BindingState::MissingPartition
        );
        assert_eq!(
            layout
                .bind(
                    Device::Mbr {
                        signature: 124,
                        offset: 1024
                    },
                    boot
                )
                .0
                .state,
            BindingState::OutsideSource
        );
    }

    #[test]
    fn checked_partition_paths_refuse_windows_namespace_and_traversal_syntax() {
        assert_eq!(
            boot_path(r"\Windows\System32\winload.efi").unwrap(),
            ["Windows", "System32", "winload.efi"]
        );
        for path in [
            r"\..\winload.exe",
            r"\Windows\.\winload.exe",
            r"\Windows\boot.",
            r"\Windows\",
            r"\Windows\loader.exe:stream",
            r"\Windows/loader.exe",
            r"\\?\C:\Windows",
            r"C:\Windows",
            r"\",
        ] {
            assert!(boot_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn logical_layout_is_independent_of_the_device_or_restore_point_label() {
        let mut disk = vec![0; 8 * MIB];
        disk[510..512].copy_from_slice(&[0x55, 0xaa]);
        disk[450] = 0x06;
        disk[454..458].copy_from_slice(&2048u32.to_le_bytes());
        disk[458..462].copy_from_slice(&8192u32.to_le_bytes());
        for name in ["Disk 2 · DS-TEST-B", "Archive.dsbackup / Earlier point"] {
            let result = inspect(
                Cursor::new(&disk),
                name.into(),
                disk.len() as u64,
                512,
                false,
                None,
            )
            .unwrap();
            assert_eq!(result.source, name);
            assert_eq!(result.scheme, "MBR");
            assert!(result.volumes.is_empty());
        }
    }
}

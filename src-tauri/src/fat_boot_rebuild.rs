//! Plan offline replacement of fixed UEFI boot files on checked FAT32.
//! Data and directory changes are staged separately while both FATs are dirty.
use super::directory_entry;
use crate::fat_recovery::{
    allocation::ClassicTable, at, resize::checked_boot_fat32, u16at, u32at, Fs,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek},
};

const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY: u64 = 8 * 1024 * 1024;
const CLEAN: u32 = 0x0800_0000;
const EOC: u32 = 0x0fff_ffff;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Target {
    Manager,
    FallbackX64,
    FallbackIa32,
    FallbackAa64,
    Bcd,
    BcdLog,
    BcdLog1,
    BcdLog2,
}

pub(crate) const BCD_LOGS: [Target; 3] = [Target::BcdLog, Target::BcdLog1, Target::BcdLog2];

#[derive(Clone, Copy)]
struct Name {
    text: &'static str,
    short: [u8; 11],
    long: bool,
}

const fn short(text: &'static str, short: [u8; 11]) -> Name {
    Name {
        text,
        short,
        long: false,
    }
}

const EFI: Name = short("EFI", *b"EFI        ");
const BOOT: Name = short("Boot", *b"BOOT       ");
const MICROSOFT: Name = Name {
    text: "Microsoft",
    short: *b"MICROS~1   ",
    long: true,
};
const MANAGER_DIR: &[Name] = &[EFI, MICROSOFT, BOOT];
const FALLBACK_DIR: &[Name] = &[EFI, BOOT];
const MANAGER: Name = short("bootmgfw.efi", *b"BOOTMGFWEFI");

impl Target {
    fn location(self) -> (&'static [Name], Name) {
        match self {
            Self::Manager => (MANAGER_DIR, MANAGER),
            Self::FallbackX64 => (FALLBACK_DIR, short("bootx64.efi", *b"BOOTX64 EFI")),
            Self::FallbackIa32 => (FALLBACK_DIR, short("bootia32.efi", *b"BOOTIA32EFI")),
            Self::FallbackAa64 => (FALLBACK_DIR, short("bootaa64.efi", *b"BOOTAA64EFI")),
            Self::Bcd => (MANAGER_DIR, short("BCD", *b"BCD        ")),
            Self::BcdLog => (MANAGER_DIR, short("BCD.LOG", *b"BCD     LOG")),
            Self::BcdLog1 => (
                MANAGER_DIR,
                Name {
                    text: "BCD.LOG1",
                    short: *b"BCDLOG~1LOG",
                    long: true,
                },
            ),
            Self::BcdLog2 => (
                MANAGER_DIR,
                Name {
                    text: "BCD.LOG2",
                    short: *b"BCDLOG~1LOG",
                    long: true,
                },
            ),
        }
    }

    pub(crate) fn path(self) -> Vec<&'static str> {
        let (dirs, file) = self.location();
        dirs.iter()
            .map(|name| name.text)
            .chain([file.text])
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Dirty,
    Data,
    Allocate,
    Retire,
    Publish,
    Reclaim,
    FreeSpace,
    Clean,
}

pub(crate) struct Edit {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub(crate) struct File {
    pub target: Target,
    pub bytes: usize,
    pub sha256: String,
    pub original: Option<Vec<u8>>,
}

pub(crate) struct Plan {
    pub phases: Vec<(Phase, Vec<Edit>)>,
    pub files: Vec<File>,
    pub retired: Vec<Retired>,
    pub ownership_before: [u8; 32],
}

pub(crate) struct Retired {
    pub target: Target,
    pub original: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) enum BcdUpdate<'a> {
    Keep,
    Create,
    Replace(&'a [u8]),
}

struct Directory {
    chain: Vec<u32>,
    original: Vec<u8>,
    bytes: Vec<u8>,
}

struct Builder {
    fs: Fs,
    fat: ClassicTable,
    free_from: u32,
    directories: BTreeMap<u32, Directory>,
    data: Vec<Edit>,
    retire: Vec<Edit>,
    freed_after_publish: Vec<u32>,
}

fn first(entry: &[u8]) -> u32 {
    (u32::from(u16at(entry, 20)) << 16) | u32::from(u16at(entry, 26))
}

fn set_location(entry: &mut [u8], cluster: u32, bytes: u32) {
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&bytes.to_le_bytes());
}

fn short_record(name: &[u8; 11], attrs: u8, cluster: u32, bytes: u32) -> [u8; 32] {
    let mut out = [0; 32];
    out[..11].copy_from_slice(name);
    out[11] = attrs;
    for offset in [16, 18, 24] {
        out[offset..offset + 2].copy_from_slice(&0x21u16.to_le_bytes());
    }
    set_location(&mut out, cluster, bytes);
    out
}

fn long_record(name: Name, short: &[u8; 11]) -> [u8; 32] {
    let mut entry = [0xff; 32];
    entry[0] = 0x41;
    entry[11] = 0x0f;
    entry[12] = 0;
    entry[13] = short
        .iter()
        .fold(0u8, |a, b| a.rotate_right(1).wrapping_add(*b));
    entry[26..28].fill(0);
    let units = name
        .text
        .encode_utf16()
        .chain([0])
        .chain(std::iter::repeat(0xffff));
    for (offset, unit) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
        .into_iter()
        .zip(units)
    {
        entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    entry
}

impl Builder {
    fn open_directory(&mut self, reader: &mut (impl Read + Seek), id: u32) -> Result<(), String> {
        if self.directories.contains_key(&id) {
            return Ok(());
        }
        let chain = self.fs.chain(reader, id, None, false, false)?;
        let size = chain.len() as u64 * self.fs.cluster;
        if size > MAX_DIRECTORY {
            return Err("An EFI directory exceeds the boot repair size limit".into());
        }
        let mut bytes = Vec::with_capacity(size as usize);
        for &cluster in &chain {
            bytes.extend(at(
                reader,
                self.fs.offset(cluster)?,
                self.fs.cluster as usize,
            )?);
        }
        self.directories.insert(
            id,
            Directory {
                chain,
                original: bytes.clone(),
                bytes,
            },
        );
        Ok(())
    }

    fn allocate(&mut self, count: usize) -> Result<Vec<u32>, String> {
        let mut selected = Vec::with_capacity(count);
        while selected.len() != count {
            let cluster = (self.free_from..self.fs.count + 2)
                .find(|&id| self.fat.entry(id) == 0)
                .ok_or("The EFI partition has insufficient free clusters for boot repair")?;
            selected.push(cluster);
            self.free_from = cluster + 1;
        }
        for pair in selected.windows(2) {
            self.fat.set_entry(pair[0], pair[1]);
        }
        if let Some(&last) = selected.last() {
            self.fat.set_entry(last, EOC);
        }
        Ok(selected)
    }

    fn add_entry(
        &mut self,
        directory: u32,
        name: Name,
        attrs: u8,
        cluster: u32,
        size: u32,
    ) -> Result<(), String> {
        let bytes = &self.directories[&directory].bytes;
        let occupied = |candidate: &[u8; 11]| {
            bytes
                .chunks_exact(32)
                .take_while(|record| record[0] != 0)
                .any(|record| {
                    record[0] != 0xe5
                        && record[11] != 0x0f
                        && record[..11].eq_ignore_ascii_case(candidate)
                })
        };
        let mut alias = name.short;
        if name.long {
            let available = (b'1'..=b'9').find(|&n| {
                alias[7] = n;
                !occupied(&alias)
            });
            if available.is_none() {
                return Err("No available short alias for the EFI entry".into());
            }
        } else if occupied(&alias) {
            return Err("The EFI short name is already in use".into());
        }
        let end = bytes
            .chunks_exact(32)
            .position(|row| row[0] == 0)
            .map_or(bytes.len(), |slot| slot * 32);
        let needed = (2 + usize::from(name.long)) * 32;
        if end + needed > bytes.len() {
            let extended = bytes.len() + self.fs.cluster as usize;
            if extended as u64 > MAX_DIRECTORY {
                return Err(
                    "An EFI directory cannot be extended within the boot repair size limit".into(),
                );
            }
            let next = self.allocate(1)?[0];
            let folder = self.directories.get_mut(&directory).unwrap();
            self.fat.set_entry(*folder.chain.last().unwrap(), next);
            folder.chain.push(next);
            folder.bytes.resize(extended, 0);
        }
        let bytes = &mut self.directories.get_mut(&directory).unwrap().bytes;
        let mut offset = end;
        if name.long {
            bytes[offset..offset + 32].copy_from_slice(&long_record(name, &alias));
            offset += 32;
        }
        bytes[offset..offset + 32].copy_from_slice(&short_record(&alias, attrs, cluster, size));
        bytes[offset + 32] = 0;
        Ok(())
    }

    fn directory(&mut self, reader: &mut (impl Read + Seek), path: &[Name]) -> Result<u32, String> {
        let mut parent = self.fs.root;
        self.open_directory(reader, parent)?;
        for &name in path {
            let bytes = &self.directories[&parent].bytes;
            if let Some(slot) = directory_entry(bytes, name.text)? {
                let entry = &bytes[slot.offset..slot.offset + 32];
                if !slot.primary || entry[11] & 16 == 0 {
                    return Err("An EFI directory name is already in use by another entry".into());
                }
                parent = first(entry);
                self.open_directory(reader, parent)?;
            } else {
                let cluster = self.allocate(1)?[0];
                let mut bytes = vec![0; self.fs.cluster as usize];
                let parent_dot = if parent == self.fs.root { 0 } else { parent };
                bytes[..32].copy_from_slice(&short_record(b".          ", 16, cluster, 0));
                bytes[32..64].copy_from_slice(&short_record(b"..         ", 16, parent_dot, 0));
                self.directories.insert(
                    cluster,
                    Directory {
                        chain: vec![cluster],
                        original: Vec::new(),
                        bytes,
                    },
                );
                self.add_entry(parent, name, 16, cluster, 0)?;
                parent = cluster;
            }
        }
        Ok(parent)
    }

    fn install(
        &mut self,
        reader: &mut (impl Read + Seek),
        target: Target,
        content: &[u8],
        bcd: BcdUpdate<'_>,
    ) -> Result<Option<File>, String> {
        let (folders, name) = target.location();
        let directory = self.directory(reader, folders)?;
        let dir = &self.directories[&directory].bytes;
        let matched = directory_entry(dir, name.text)?;
        let original = if let Some(slot) = &matched {
            let entry = &dir[slot.offset..slot.offset + 32];
            if !slot.primary || entry[11] & 0xd8 != 0 {
                return Err("An EFI boot filename is already in use by another entry".into());
            }
            let size = u32at(entry, 28) as u64;
            if size > MAX_FILE as u64 {
                return Err("The existing EFI boot file exceeds the repair size limit".into());
            }
            let id = first(entry);
            let bytes = self.fs.content(reader, id, size, false, false)?;
            if target == Target::Bcd
                && !matches!(bcd, BcdUpdate::Replace(original) if original == bytes)
            {
                return Err("The existing BCD differs from the approved repair".into());
            }
            if bytes == content {
                return Ok(None);
            }
            self.freed_after_publish
                .extend(self.fs.chain(reader, id, Some(size), false, false)?);
            Some(bytes)
        } else {
            if target == Target::Bcd && matches!(bcd, BcdUpdate::Replace(_)) {
                return Err("The BCD is missing from its approved repair location".into());
            }
            None
        };
        let cluster_size = self.fs.cluster as usize;
        let chain = self.allocate(content.len().div_ceil(cluster_size))?;
        for (&cluster, chunk) in chain.iter().zip(content.chunks(cluster_size)) {
            let mut bytes = vec![0; cluster_size];
            bytes[..chunk.len()].copy_from_slice(chunk);
            self.data.push(Edit {
                offset: self.fs.offset(cluster)?,
                bytes,
            });
        }
        if let Some(slot) = matched {
            let bytes = &mut self.directories.get_mut(&directory).unwrap().bytes;
            set_location(
                &mut bytes[slot.offset..slot.offset + 32],
                chain[0],
                content.len() as u32,
            );
        } else {
            let attrs = if target == Target::Bcd { 0x26 } else { 0x20 };
            self.add_entry(directory, name, attrs, chain[0], content.len() as u32)?;
        }
        Ok(Some(File {
            target,
            bytes: content.len(),
            sha256: crate::sector_edit::hash(content),
            original,
        }))
    }

    fn retire_bcd_log(
        &mut self,
        reader: &mut (impl Read + Seek),
        target: Target,
    ) -> Result<Option<Retired>, String> {
        let (folders, name) = target.location();
        let id = self.directory(reader, folders)?;
        let directory = self.directories.get_mut(&id).unwrap();
        let Some(slot) = directory_entry(&directory.bytes, name.text)? else {
            return Ok(None);
        };
        let record = &directory.bytes[slot.offset..slot.offset + 32];
        if !slot.primary || record[11] & 0xd8 != 0 {
            return Err("A BCD log name is in use by an unexpected EFI entry".into());
        }
        let size = u32at(record, 28) as u64;
        if size > MAX_FILE as u64 {
            return Err("The existing BCD log exceeds the repair size limit".into());
        }
        let cluster = first(record);
        let original = self.fs.content(reader, cluster, size, false, false)?;
        self.freed_after_publish.extend(self.fs.chain(
            reader,
            cluster,
            Some(size),
            false,
            false,
        )?);
        let cluster_size = self.fs.cluster as usize;
        for index in (slot.first..=slot.offset).step_by(32) {
            directory.bytes[index] = 0xe5;
            let cluster = directory.chain[index / cluster_size];
            self.retire.push(Edit {
                offset: self.fs.offset(cluster)? + (index % cluster_size) as u64,
                bytes: vec![0xe5],
            });
        }
        Ok(Some(Retired { target, original }))
    }
}

fn fat_edits(fs: &Fs, before: &ClassicTable, after: &ClassicTable) -> Vec<Edit> {
    let changes = before
        .bytes()
        .chunks_exact(4)
        .zip(after.bytes().chunks_exact(4))
        .enumerate()
        .filter(|(_, (a, b))| a != b);
    // Update the second copy before publishing these entries in the first.
    [fs.fat + fs.fat_len, fs.fat]
        .into_iter()
        .flat_map(|base| {
            changes.clone().map(move |(index, (_, bytes))| Edit {
                offset: base + index as u64 * 4,
                bytes: bytes.to_vec(),
            })
        })
        .collect()
}

fn fsinfo_edits(
    reader: &mut (impl Read + Seek),
    boot: &[u8],
    sector: u64,
    fs: &Fs,
    table: &ClassicTable,
) -> Result<Vec<Edit>, String> {
    let primary = u64::from(u16at(boot, 48));
    let backup_boot = u64::from(u16at(boot, 50));
    let backup = backup_boot + primary;
    let reserved = u64::from(u16at(boot, 14));
    if primary == 0 || primary == backup_boot || backup >= reserved {
        return Err("FAT32 FSInfo locations are absent or outside the reserved region".into());
    }
    let mut count = 0u32;
    let mut hint = u32::MAX;
    for cluster in 2..fs.count + 2 {
        if table.entry(cluster) == 0 {
            count += 1;
            hint = hint.min(cluster);
        }
    }
    let mut edits = Vec::new();
    for lba in [backup, primary] {
        let bytes = at(reader, lba * sector, sector as usize)?;
        // Some FAT32 formatters omit the backup allocation hint. Do not
        // initialize an unused reserved sector or accept a damaged hint.
        if lba == backup && bytes.iter().all(|&byte| byte == 0) {
            continue;
        }
        if u32at(&bytes, 0) != 0x4161_5252
            || u32at(&bytes, 484) != 0x6141_7272
            || u32at(&bytes, 508) != 0xaa55_0000
        {
            return Err("A FAT32 FSInfo sector is invalid".into());
        }
        edits.push(Edit {
            offset: lba * sector + 488,
            bytes: [count.to_le_bytes(), hint.to_le_bytes()].concat(),
        });
    }
    Ok(edits)
}

pub(crate) fn plan(
    reader: &mut (impl Read + Seek),
    sector: u64,
    files: &[(Target, &[u8])],
    bcd: BcdUpdate<'_>,
) -> Result<Plan, String> {
    let mut names = BTreeSet::new();
    if files.is_empty()
        || files.iter().any(|(target, bytes)| {
            bytes.is_empty() || bytes.len() > MAX_FILE || !names.insert(*target)
        })
    {
        return Err("Invalid or repeated EFI boot files in the repair plan".into());
    }
    let changes_bcd = !matches!(bcd, BcdUpdate::Keep);
    if names.contains(&Target::Bcd) != changes_bcd {
        return Err("The BCD file list does not match the selected repair operation".into());
    }
    let checked = checked_boot_fat32(reader, sector)?;
    let original = checked.table;
    let mut dirty = original.clone();
    dirty.set_entry(1, dirty.entry(1) & !CLEAN);
    let mut builder = Builder {
        fs: checked.fs,
        fat: dirty.clone(),
        free_from: 2,
        directories: BTreeMap::new(),
        data: Vec::new(),
        retire: Vec::new(),
        freed_after_publish: Vec::new(),
    };
    let mut retired = Vec::new();
    if changes_bcd {
        for target in BCD_LOGS {
            retired.extend(builder.retire_bcd_log(reader, target)?);
        }
    }
    let mut installed = Vec::new();
    for &(target, bytes) in files {
        if let Some(file) = builder.install(reader, target, bytes, bcd)? {
            installed.push(file);
        }
    }
    if installed.is_empty() {
        return Err("The EFI boot files already match their Windows source".into());
    }
    let mut publish = Vec::new();
    let cluster_size = builder.fs.cluster as usize;
    for directory in builder.directories.values() {
        for (index, (&cluster, bytes)) in directory
            .chain
            .iter()
            .zip(directory.bytes.chunks_exact(cluster_size))
            .enumerate()
        {
            let offset = builder.fs.offset(cluster)?;
            if original.entry(cluster) == 0 {
                builder.data.push(Edit {
                    offset,
                    bytes: bytes.to_vec(),
                });
            } else if directory
                .original
                .get(index * cluster_size..(index + 1) * cluster_size)
                != Some(bytes)
            {
                publish.push(Edit {
                    offset,
                    bytes: bytes.to_vec(),
                });
            }
        }
    }
    let mut reclaimed = builder.fat.clone();
    for &cluster in &builder.freed_after_publish {
        reclaimed.set_entry(cluster, 0);
    }
    let info = fsinfo_edits(reader, &checked.boot, sector, &builder.fs, &reclaimed)?;
    let mut clean = reclaimed.clone();
    clean.set_entry(1, original.entry(1));
    let mut mark_dirty = fat_edits(&builder.fs, &original, &dirty);
    // Mark the primary first. Clean is restored in the opposite order.
    mark_dirty.reverse();
    Ok(Plan {
        phases: vec![
            (Phase::Dirty, mark_dirty),
            (Phase::Data, builder.data),
            (
                Phase::Allocate,
                fat_edits(&builder.fs, &dirty, &builder.fat),
            ),
            (Phase::Retire, builder.retire),
            (Phase::Publish, publish),
            (
                Phase::Reclaim,
                fat_edits(&builder.fs, &builder.fat, &reclaimed),
            ),
            (Phase::FreeSpace, info),
            (Phase::Clean, fat_edits(&builder.fs, &reclaimed, &clean)),
        ],
        files: installed,
        retired,
        ownership_before: checked.digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fat_recovery::{boot_files::Volume, resize::check_boot_fat32};
    use disk_core::{
        fat_format::{FatType, Plan as Format},
        MIB,
    };
    use std::io::Cursor;

    fn blank() -> Cursor<Vec<u8>> {
        let len = 40 * MIB;
        let mut disk = Cursor::new(vec![0; len as usize]);
        Format::new(FatType::Fat32, len, 512, 512, "BOOT", 11, 2048)
            .unwrap()
            .format(&mut disk, |_, _| Ok(()))
            .unwrap();
        disk
    }

    fn apply(disk: &mut Cursor<Vec<u8>>, edits: &[Edit]) {
        for edit in edits {
            let offset = edit.offset as usize;
            disk.get_mut()[offset..offset + edit.bytes.len()].copy_from_slice(&edit.bytes);
        }
    }

    fn read(disk: &mut Cursor<Vec<u8>>, target: Target) -> Option<Vec<u8>> {
        Volume::open(disk)
            .unwrap()
            .read_file(disk, &target.path(), MAX_FILE)
            .unwrap()
    }

    #[test]
    fn installs_files_and_creates_missing_directories_even_when_root_must_grow() {
        let mut disk = blank();
        let fs = Fs::load(&mut disk).unwrap();
        let root = fs.offset(fs.root).unwrap() as usize;
        // Fill the original root cluster with empty files. Their short names,
        // attributes and slots must be preserved when the directory grows.
        for (i, slot) in disk.get_mut()[root..root + 512]
            .chunks_exact_mut(32)
            .enumerate()
        {
            let name: [u8; 11] = format!("FIL{i:05}TMP").into_bytes().try_into().unwrap();
            slot.copy_from_slice(&short_record(&name, 0x20, 0, 0));
        }
        let root_before = disk.get_ref()[root..root + 512].to_vec();
        let data = [0x62; 695];
        let requested = [
            (Target::Manager, &data[..]),
            (Target::FallbackX64, &data[..]),
        ];
        let patch = plan(&mut disk, 512, &requested, BcdUpdate::Keep).unwrap();
        assert_eq!(patch.files.len(), 2);
        assert!(patch.files.iter().all(|file| file.original.is_none()));
        assert_eq!(disk.get_ref()[root..root + 512], root_before);
        for (phase, edits) in patch.phases {
            apply(&mut disk, &edits);
            if phase != Phase::Clean {
                assert!(check_boot_fat32(&mut disk, 512).is_err());
            }
        }
        assert_eq!(disk.get_ref()[root..root + 512], root_before);
        check_boot_fat32(&mut disk, 512).unwrap();
        assert_eq!(read(&mut disk, Target::Manager).unwrap(), data);
        assert_eq!(read(&mut disk, Target::FallbackX64).unwrap(), data);
        assert!(plan(&mut disk, 512, &requested, BcdUpdate::Keep)
            .err()
            .unwrap()
            .contains("already match"));
    }

    #[test]
    fn replaces_with_copy_on_write_and_reclaims_only_the_previous_file_chain() {
        let mut disk = blank();
        let old = [0xdd; 1400];
        let other = [0xdf; 730];
        let patch = plan(
            &mut disk,
            512,
            &[(Target::Manager, &old), (Target::FallbackX64, &other)],
            BcdUpdate::Keep,
        )
        .unwrap();
        for (_, edits) in patch.phases {
            apply(&mut disk, &edits);
        }
        let volume = Volume::open(&mut disk).unwrap();
        let mapped = volume
            .mapped_file(&mut disk, &Target::Manager.path(), MAX_FILE)
            .unwrap();
        let new = [0x8f; 123];
        let patch = plan(&mut disk, 512, &[(Target::Manager, &new)], BcdUpdate::Keep).unwrap();
        assert_eq!(patch.files[0].original.as_deref(), Some(&old[..]));
        for (phase, edits) in patch.phases {
            for edit in &edits {
                assert!(mapped.clusters.iter().all(|&start| {
                    edit.offset + edit.bytes.len() as u64 <= start || edit.offset >= start + 512
                }));
            }
            apply(&mut disk, &edits);
            if phase == Phase::Allocate {
                assert_eq!(read(&mut disk, Target::Manager).unwrap(), old);
            }
            if phase == Phase::Publish {
                assert_eq!(read(&mut disk, Target::Manager).unwrap(), new);
                let fs = Fs::load(&mut disk).unwrap();
                for &start in &mapped.clusters {
                    assert_ne!(
                        fs.fat(&mut disk, ((start - fs.heap) / fs.cluster) as u32 + 2)
                            .unwrap(),
                        0
                    );
                }
            }
        }
        let checked = checked_boot_fat32(&mut disk, 512).unwrap();
        for start in mapped.clusters {
            assert_eq!(
                checked
                    .table
                    .entry(((start - checked.fs.heap) / 512) as u32 + 2),
                0
            );
        }
        assert_eq!(read(&mut disk, Target::Manager).unwrap(), new);
        assert_eq!(read(&mut disk, Target::FallbackX64).unwrap(), other);
        let free = (2..checked.fs.count + 2)
            .filter(|&id| checked.table.entry(id) == 0)
            .count() as u32;
        for offset in [512, 7 * 512] {
            assert_eq!(u32at(disk.get_ref(), offset + 488), free);
        }
    }

    #[test]
    fn backs_up_and_retires_all_stale_logs_before_publishing_a_new_bcd() {
        let mut disk = blank();
        let old = [
            (Target::BcdLog, &[5; 3][..]),
            (Target::BcdLog1, &[7; 600][..]),
            (Target::BcdLog2, &[9; 2][..]),
        ];
        let fixtures = plan(&mut disk, 512, &old, BcdUpdate::Keep).unwrap();
        for (_, edits) in fixtures.phases {
            apply(&mut disk, &edits);
        }
        for &(target, data) in &old {
            assert_eq!(read(&mut disk, target).as_deref(), Some(data));
        }
        let bcd = &[0x72; 8192];
        let patch = plan(&mut disk, 512, &[(Target::Bcd, bcd)], BcdUpdate::Create).unwrap();
        assert_eq!(patch.retired.len(), old.len());
        for (retired, (target, data)) in patch.retired.iter().zip(old) {
            assert_eq!(retired.target, target);
            assert_eq!(retired.original, data);
        }
        for (phase, edits) in patch.phases {
            apply(&mut disk, &edits);
            if phase == Phase::Retire {
                assert!(read(&mut disk, Target::Bcd).is_none());
                for target in BCD_LOGS {
                    assert!(read(&mut disk, target).is_none());
                }
            }
        }
        assert_eq!(read(&mut disk, Target::Bcd).as_deref(), Some(&bcd[..]));
        check_boot_fat32(&mut disk, 512).unwrap();
        assert!(plan(
            &mut disk,
            512,
            &[(Target::Bcd, &[0x55; 1024])],
            BcdUpdate::Create
        )
        .is_err());
    }

    #[test]
    fn refuses_invalid_fsinfo_and_duplicate_boot_paths_before_writing() {
        let mut disk = blank();
        let requests = [(Target::Manager, b"EFI".as_slice())];
        disk.get_mut()[7 * 512] ^= 1;
        assert!(plan(&mut disk, 512, &requests, BcdUpdate::Keep)
            .err()
            .unwrap()
            .contains("FSInfo"));
        disk.get_mut()[7 * 512] ^= 1;
        let fs = Fs::load(&mut disk).unwrap();
        let root = fs.offset(fs.root).unwrap() as usize;
        for i in 1..=2 {
            disk.get_mut()[root + i * 32..root + (i + 1) * 32].copy_from_slice(&short_record(
                b"EFI        ",
                0x20,
                0,
                0,
            ));
        }
        assert!(plan(&mut disk, 512, &requests, BcdUpdate::Keep)
            .err()
            .unwrap()
            .contains("ambiguous"));
    }
}

//! A read-only NTFS view over independently corroborated redundant metadata.
use super::view::{Patch, Reader};
use ntfs::{attribute_value::NtfsAttributeValue, Ntfs, NtfsAttributeType, NtfsFile};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

const SECTOR_HEADER: usize = 512;
const BOOTSTRAP_NAMES: [&str; 4] = ["$MFT", "$MFTMirr", "$LogFile", "$Volume"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Proof {
    pub(crate) backup_boot: bool,
    pub(crate) mirrored_records: Vec<u32>,
    pub(crate) sector_size: u64,
    pub(crate) bytes: u64,
    #[serde(skip)]
    pub(crate) backup_verified: bool,
    evidence_sha256: String,
}

impl Proof {
    #[cfg(windows)]
    pub(crate) fn description(&self) -> String {
        let locale = crate::native_locale::current();
        let mut parts = Vec::new();
        if self.backup_boot {
            parts.push(locale.text("已采用 NTFS 备用引导扇区。").to_owned());
        }
        if !self.mirrored_records.is_empty() {
            parts.push(locale.format(
                "已采用 {count} 条 MFT 镜像启动记录。",
                &[("count", &self.mirrored_records.len().to_string())],
            ));
        }
        parts.join(" ")
    }

    #[cfg(test)]
    pub(crate) fn reopen<R: Read + Seek>(
        &self,
        reader: R,
        cancel: &AtomicBool,
    ) -> Result<Reader<R>, String> {
        let (reader, observed) = open(reader, self.sector_size, cancel)?;
        if observed != *self {
            return Err("NTFS 重建依据已变化，请重新读取。".into());
        }
        Ok(reader)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Geometry {
    bytes: u64,
    cluster: u64,
    record: usize,
    mft: u64,
    mirror: u64,
    serial: u64,
    index_record: i8,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn geometry(boot: &[u8], sector: u64, length: u64) -> Option<Geometry> {
    if &boot[3..11] != b"NTFS    " || boot[510..512] != [0x55, 0xaa] {
        return None;
    }
    let fs = Ntfs::new(&mut io::Cursor::new(boot)).ok()?;
    let bytes = fs.size();
    let cluster = fs.cluster_size() as u64;
    let record = fs.file_record_size() as usize;
    let serial = fs.serial_number();
    if u64::from(fs.sector_size()) != sector
        || bytes.checked_add(sector)? > length
        || bytes < cluster.checked_mul(16)?
        || !matches!(record, 1024 | 2048 | 4096)
        || serial == 0
    {
        return None;
    }
    let mft = fs.mft_position().value()?.get();
    let mirror = u64_at(boot, 56).checked_mul(cluster)?;
    let prefix = (record * BOOTSTRAP_NAMES.len()) as u64;
    if mft < cluster
        || mirror < cluster
        || mft.checked_add(prefix)? > bytes
        || mirror.checked_add(prefix)? > bytes
        || mft.abs_diff(mirror) < prefix
    {
        return None;
    }
    Some(Geometry {
        bytes,
        cluster,
        record,
        mft,
        mirror,
        serial,
        index_record: boot[68] as i8,
    })
}

pub(crate) fn backup_extent(boot: &[u8], sector: u64) -> Option<(u64, u64)> {
    if boot.len() < SECTOR_HEADER || &boot[3..11] != b"NTFS    " {
        return None;
    }
    let distance = u64_at(boot, 40).checked_mul(sector)?;
    let bytes = distance.checked_add(sector)?;
    let geom = geometry(&boot[..SECTOR_HEADER], sector, bytes)?;
    (geom.bytes == distance).then_some((distance, bytes))
}

fn read_at(reader: &mut (impl Read + Seek), at: u64, bytes: &mut [u8]) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(at))
        .map_err(|e| e.to_string())?;
    reader.read_exact(bytes).map_err(|e| e.to_string())
}

pub(crate) fn has_tail_boot(
    reader: &mut (impl Read + Seek),
    sector: u64,
    length: u64,
) -> Result<bool, String> {
    let at = length - sector;
    let mut boot = [0; SECTOR_HEADER];
    read_at(reader, at, &mut boot)?;
    Ok(geometry(&boot, sector, length).is_some_and(|geom| geom.bytes == at))
}

fn record_valid(bytes: &[u8], number: u32) -> bool {
    let offset = usize::from(u16_at(bytes, 4));
    let count = usize::from(u16_at(bytes, 6));
    let attr = usize::from(u16_at(bytes, 20));
    let used = u32_at(bytes, 24) as usize;
    &bytes[..4] == b"FILE"
        && u16_at(bytes, 16) != 0
        && u16_at(bytes, 22) & 3 == 1
        && u64_at(bytes, 32) == 0
        && u32_at(bytes, 44) == number
        && u32_at(bytes, 28) as usize == bytes.len()
        && offset >= 48
        && count == bytes.len() / SECTOR_HEADER + 1
        && offset + count * 2 <= attr
        && offset + count * 2 <= SECTOR_HEADER - 2
        && attr % 8 == 0
        && attr + 4 <= used
        && used <= bytes.len()
        && bytes
            .chunks_exact(SECTOR_HEADER)
            .all(|block| block[510..] == bytes[offset..offset + 2])
}

#[derive(PartialEq, Eq)]
struct Stream {
    bytes: u64,
    initialized: u64,
    runs: Vec<(u64, u64)>,
}

fn bootstrap_stream(
    reader: &mut (impl Read + Seek),
    file: &NtfsFile<'_>,
    geom: &Geometry,
    first: u64,
) -> Result<Stream, String> {
    let invalid = || "NTFS MFT 启动记录的数据映射无法核对。".to_owned();
    let mut attrs = file.attributes_raw();
    let mut stream = None;
    for item in &mut attrs {
        let attr = item.map_err(|_| invalid())?;
        match attr.ty().map_err(|_| invalid())? {
            NtfsAttributeType::AttributeList => return Err(invalid()),
            NtfsAttributeType::Data if attr.name_length() == 0 && stream.is_none() => {
                stream = Some(attr);
            }
            NtfsAttributeType::Data => return Err(invalid()),
            _ => {}
        }
    }
    let attr = stream.ok_or_else(invalid)?;
    let raw = attr.record_bytes();
    let bytes = attr.value_length();
    let prefix = (geom.record * BOOTSTRAP_NAMES.len()) as u64;
    if raw.len() < 64
        || u16_at(raw, 12) != 0
        || u64_at(raw, 16) != 0
        || bytes < prefix
        || bytes > geom.bytes
    {
        return Err(invalid());
    }
    let NtfsAttributeValue::NonResident(value) = attr.value(reader).map_err(|_| invalid())? else {
        return Err(invalid());
    };
    let initialized = u64_at(raw, 56);
    if initialized < prefix
        || initialized > bytes
        || initialized % geom.record as u64 != 0
        || bytes % geom.record as u64 != 0
    {
        return Err(invalid());
    }
    let mut runs = Vec::new();
    let mut allocated = 0u64;
    for run in value.data_runs() {
        let run = run.map_err(|_| invalid())?;
        let start = run.data_position().value().ok_or_else(invalid)?.get();
        let size = run.allocated_size();
        if size == 0
            || start < geom.cluster
            || start.checked_add(size).is_none_or(|end| end > geom.bytes)
        {
            return Err(invalid());
        }
        allocated = allocated.checked_add(size).ok_or_else(invalid)?;
        runs.push((start, size));
    }
    if !matches!(runs.first(), Some(&(at, size)) if at == first && size >= prefix)
        || allocated != u64_at(raw, 40)
        || u64_at(raw, 24)
            .checked_add(1)
            .and_then(|n| n.checked_mul(geom.cluster))
            != Some(allocated)
        || allocated < bytes
    {
        return Err(invalid());
    }
    let mut ordered = runs.clone();
    ordered.sort_unstable();
    if ordered
        .windows(2)
        .any(|pair| pair[0].0 + pair[0].1 > pair[1].0)
    {
        return Err(invalid());
    }
    Ok(Stream {
        bytes,
        initialized,
        runs,
    })
}

fn stop(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("扫描已取消。".into())
    } else {
        Ok(())
    }
}

pub(crate) fn open<R: Read + Seek>(
    mut reader: R,
    sector: u64,
    cancel: &AtomicBool,
) -> Result<(Reader<R>, Proof), String> {
    stop(cancel)?;
    let length = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if !matches!(sector, 512 | 4096) || length % sector != 0 || length < 32 * sector {
        return Err("所选范围不适合核对 NTFS 冗余元数据。".into());
    }
    let mut boot = [0; SECTOR_HEADER];
    read_at(&mut reader, 0, &mut boot)?;
    let primary = geometry(&boot, sector, length);
    if primary.is_none() {
        let mut ext_magic = [0; 2];
        read_at(&mut reader, 1080, &mut ext_magic)?;
        if ext_magic == [0x53, 0xef]
            || super::fat_boot(&boot)
            || crate::bitlocker::boot_marker(&boot)
            || disk_core::btrfs::has_magic(&mut reader, length)?
        {
            return Err("主引导区域有其他文件系统或加密证据，不能采用 NTFS 备用引导。".into());
        }
    }
    let backup_at = primary.as_ref().map_or(length - sector, |geom| geom.bytes);
    let mut backup = [0; SECTOR_HEADER];
    read_at(&mut reader, backup_at, &mut backup)?;
    let secondary = geometry(&backup, sector, length);
    let backup_verified = secondary.is_some();
    let backup_boot = primary.is_none();
    let geom = match (primary, secondary) {
        (Some(main), Some(standby)) if main != standby => {
            return Err("NTFS 主备引导的几何或身份不一致。".into());
        }
        (Some(main), _) => main,
        (None, Some(standby)) if standby.bytes == backup_at => standby,
        _ => return Err("找不到与所选范围末端吻合的 NTFS 备用引导。".into()),
    };
    stop(cancel)?;
    let prefix = geom.record * BOOTSTRAP_NAMES.len();
    let mut main_records = vec![0; prefix];
    let mut mirror_records = vec![0; prefix];
    read_at(&mut reader, geom.mft, &mut main_records)?;
    read_at(&mut reader, geom.mirror, &mut mirror_records)?;
    let size = geom.bytes + sector;
    let mut patches = vec![];
    if backup_boot {
        patches.push(Patch {
            at: 0,
            bytes: backup.to_vec(),
        });
    }
    let mut mirrored_records = vec![];
    for number in 0..BOOTSTRAP_NAMES.len() {
        let start = number * geom.record;
        let raw = start..start + geom.record;
        if !record_valid(&mirror_records[raw.clone()], number as u32) {
            return Err("NTFS MFT 镜像的启动记录或更新序列无效。".into());
        }
        if !record_valid(&main_records[raw.clone()], number as u32) {
            patches.push(Patch {
                at: geom.mft + start as u64,
                bytes: mirror_records[raw].to_vec(),
            });
            mirrored_records.push(number as u32);
        }
    }
    if patches.is_empty() {
        return Err("NTFS 主引导和 MFT 启动记录可读，请使用常规目录读取。".into());
    }
    let invalid = |e: ntfs::NtfsError| format!("NTFS 冗余视图无法核对：{e}");
    let mut view = Reader::new(&mut reader, patches.clone(), size);
    let fs = Ntfs::new(&mut view).map_err(invalid)?;
    let mut evidence = Sha256::new();
    evidence.update(length.to_le_bytes());
    evidence.update(boot);
    evidence.update(backup_at.to_le_bytes());
    evidence.update(backup);
    evidence.update(&main_records);
    evidence.update(&mirror_records);
    for (number, expected) in BOOTSTRAP_NAMES.iter().enumerate() {
        stop(cancel)?;
        let file = fs.file(&mut view, number as u64).map_err(invalid)?;
        if file.position().value().map(|v| v.get())
            != Some(geom.mft + (number * geom.record) as u64)
            || !crate::ntfs_names::read(&mut view, &file)?
                .iter()
                .any(|name| name.display() == *expected)
        {
            return Err("NTFS 启动记录的身份与 MFT 映射不一致。".into());
        }
    }
    let mft = fs.file(&mut view, 0).map_err(invalid)?;
    let mirror = fs.file(&mut view, 1).map_err(invalid)?;
    let mft = bootstrap_stream(&mut view, &mft, &geom, geom.mft)?;
    let mirror = bootstrap_stream(&mut view, &mirror, &geom, geom.mirror)?;
    if mft
        .runs
        .iter()
        .any(|&(a, n)| mirror.runs.iter().any(|&(b, m)| a < b + m && b < a + n))
    {
        return Err("NTFS MFT 与其镜像共享或重叠分配范围。".into());
    }
    let volume = fs.volume_info(&mut view).map_err(invalid)?;
    if volume.major_version() != 3 || volume.minor_version() != 1 {
        return Err("需使用可确认的 NTFS 3.1 元数据。".into());
    }
    let root = fs.root_directory(&mut view).map_err(invalid)?;
    if !root.is_directory()
        || !root.flags().contains(ntfs::NtfsFileFlags::IN_USE)
        || crate::ntfs_names::base_reference(&mut view, &root)? != 0
    {
        return Err("NTFS 根目录身份无效。".into());
    }
    let at = root.position().value().ok_or("NTFS 根目录位置缺失")?.get();
    let mut raw = vec![0; geom.record];
    read_at(&mut view, at, &mut raw)?;
    evidence.update(at.to_le_bytes());
    evidence.update(raw);

    // Both record-zero copies must describe the same run map when primary is usable.
    if !mirrored_records.contains(&0) {
        let mut mirror_patch = patches.clone();
        mirror_patch.push(Patch {
            at: geom.mft,
            bytes: mirror_records[..geom.record].to_vec(),
        });
        mirror_patch.sort_unstable_by_key(|patch| patch.at);
        let mut check = Reader::new(&mut reader, mirror_patch, size);
        let mirrored_mft = fs.file(&mut check, 0).map_err(invalid)?;
        if bootstrap_stream(&mut check, &mirrored_mft, &geom, geom.mft)? != mft
            || !crate::ntfs_names::read(&mut check, &mirrored_mft)?
                .iter()
                .any(|name| name.display() == "$MFT")
        {
            return Err("NTFS 主 MFT 与镜像记录中的数据映射不一致。".into());
        }
    }
    stop(cancel)?;
    let proof = Proof {
        backup_boot,
        mirrored_records,
        sector_size: sector,
        bytes: size,
        backup_verified,
        evidence_sha256: format!("{:x}", evidence.finalize()),
    };
    Ok((Reader::new(reader, patches, size), proof))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recovery::{self, ScanMode},
        recovery_tree::Catalog,
    };
    use std::io::Cursor;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");

    #[test]
    fn damaged_boot_and_bootstrap_records_recover_the_same_file_and_directory() {
        let geometry = geometry(FIXTURE, 512, FIXTURE.len() as u64).unwrap();
        let cancel = AtomicBool::new(false);
        for (backup_boot, records) in [(true, vec![]), (false, vec![0]), (true, vec![0, 1, 2, 3])] {
            let mut damaged = FIXTURE.to_vec();
            if backup_boot {
                damaged[..512].fill(0);
            }
            for &number in &records {
                let at = geometry.mft as usize + number as usize * geometry.record;
                damaged[at..at + geometry.record].fill(0);
            }
            let (mut view, proof) = open(Cursor::new(damaged.as_slice()), 512, &cancel).unwrap();
            assert_eq!(proof.backup_boot, backup_boot);
            assert_eq!(proof.mirrored_records, records);
            let mut files = vec![];
            let mut directories = Catalog::default();
            crate::ntfs_scan::scan_reconstructed(
                &mut view,
                ScanMode::Existing,
                |_, _| Ok(()),
                |file| files.push(file),
                |dir| directories.insert(dir),
            )
            .unwrap();
            assert!(files.iter().all(|file| matches!(file.locator, recovery::Locator::Ntfs { record, .. } if record >= 16)));
            let candidate = files
                .iter()
                .find(|file| file.name == "file-with-12345")
                .unwrap();
            assert!(candidate.directory.is_some());
            let mut fresh = proof
                .reopen(Cursor::new(damaged.as_slice()), &cancel)
                .unwrap();
            let mut out = vec![];
            recovery::extract(&mut fresh, candidate, &mut out, u64::MAX, |_| Ok(())).unwrap();
            assert_eq!(out, b"12345");
            assert!(fresh.seek(SeekFrom::End(1)).is_err());
            if backup_boot {
                assert_eq!(damaged[..512], [0; 512]);
            }
        }
    }

    #[test]
    fn stale_or_ambiguous_evidence_and_cancellation_fail_closed() {
        let geom = geometry(FIXTURE, 512, FIXTURE.len() as u64).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(open(Cursor::new(FIXTURE), 512, &cancel).is_err());
        let mut damaged = FIXTURE.to_vec();
        damaged[0] ^= 1;
        damaged[510] = 0;
        let (_, proof) = open(Cursor::new(damaged.as_slice()), 512, &cancel).unwrap();
        damaged[77] ^= 1;
        assert!(proof.reopen(Cursor::new(&damaged), &cancel).is_err());
        let mut competing = damaged.clone();
        competing[3..11].copy_from_slice(b"-FVE-FS-");
        assert!(open(Cursor::new(competing), 512, &cancel).is_err());
        let mut torn = damaged.clone();
        torn[geom.mirror as usize + 510] ^= 1;
        assert!(open(Cursor::new(torn), 512, &cancel).is_err());
        damaged.extend_from_slice(&[0; 512]);
        assert!(open(Cursor::new(damaged), 512, &cancel).is_err());
        let mut conflict = FIXTURE.to_vec();
        conflict[geom.bytes as usize + 72] ^= 1;
        assert!(open(Cursor::new(conflict), 512, &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert!(open(Cursor::new(FIXTURE), 512, &cancel).is_err());
    }
}

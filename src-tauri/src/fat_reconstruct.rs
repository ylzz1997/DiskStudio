//! Read FAT32 or single-FAT exFAT through a corroborated backup boot region.
use super::{at, e, exfat_boot_checksum, u16at, u32at, u64at, Fs};
use crate::{
    recovery::{
        ntfs_reconstruct,
        view::{Patch, Reader},
    },
    used_space,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    ops::Range,
    sync::atomic::AtomicBool,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Format {
    #[serde(rename = "FAT32")]
    Fat32,
    #[serde(rename = "exFAT")]
    ExFat,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Proof {
    pub(crate) filesystem: Format,
    backup_start_sector: u64,
    boot_sectors: u64,
    pub(crate) sector_size: u64,
    bytes: u64,
    evidence_sha256: String,
}

pub(crate) struct BootCopy {
    pub(crate) proof: Proof,
    patch: Patch,
}

impl BootCopy {
    pub(crate) fn reader<R>(self, source: R) -> Reader<R> {
        Reader::new(source, vec![self.patch], self.proof.bytes)
    }
}

fn jump(boot: &[u8]) -> bool {
    boot[0] == 0xe9 || (boot[0] == 0xeb && boot[2] == 0x90)
}

fn fat32_boot(boot: &[u8], sector: u64, bytes: u64) -> bool {
    let spc = u64::from(boot[13]);
    let reserved = u64::from(u16at(boot, 14));
    let copies = u64::from(boot[16]);
    let fat = u64::from(u32at(boot, 36));
    let volume = u64::from(u32at(boot, 32));
    if !jump(boot)
        || boot[510..512] != [0x55, 0xaa]
        || u64::from(u16at(boot, 11)) != sector
        || !spc.is_power_of_two()
        || spc > 128
        || !(1..=2).contains(&copies)
        || reserved < 9
        || u16at(boot, 17) != 0
        || u16at(boot, 19) != 0
        || !(boot[21] == 0xf0 || boot[21] >= 0xf8)
        || u16at(boot, 22) != 0
        || u16at(boot, 40) != 0
        || u16at(boot, 42) != 0
        || u16at(boot, 48) != 1
        || u16at(boot, 50) != 6
        || boot[66] != 0x29
        || volume != bytes / sector
    {
        return false;
    }
    let count = volume.saturating_sub(reserved + copies * fat) / spc;
    let root = u64::from(u32at(boot, 44));
    (65_525..=0x0fff_fff5).contains(&count)
        && fat * sector >= (count + 2) * 4
        && (2..count + 2).contains(&root)
}

fn exfat_header(boot: &[u8], sector: usize, bytes: u64) -> bool {
    let shift = boot[108];
    let cluster_shift = boot[109];
    let count = u64::from(u32at(boot, 92));
    let fat = u64::from(u32at(boot, 80));
    let fat_len = u64::from(u32at(boot, 84));
    let heap = u64::from(u32at(boot, 88));
    let volume = u64at(boot, 72);
    let root = u64::from(u32at(boot, 96));
    if boot[..3] != [0xeb, 0x76, 0x90]
        || boot[3..11] != *b"EXFAT   "
        || boot[11..64].iter().any(|&b| b != 0)
        || boot[510..512] != [0x55, 0xaa]
        || u16at(boot, 104) != 0x0100
        || boot[110] != 1
        || shift != sector.trailing_zeros() as u8
        || cluster_shift > 25 - shift
        || volume != bytes / sector as u64
        || bytes < 1024 * 1024
        || count == 0
        || count > 0x7fff_fffd
        || fat < 24
        || fat + fat_len > heap
        || fat_len * (sector as u64) < (count + 2) * 4
        || volume.checked_sub(heap).map(|n| n >> cluster_shift) != Some(count)
        || !(2..count + 2).contains(&root)
    {
        return false;
    }
    true
}

fn exfat_boot(region: &[u8], sector: usize, bytes: u64) -> bool {
    if !exfat_header(&region[..sector], sector, bytes)
        || region[sector..9 * sector]
            .chunks_exact(sector)
            .any(|boot| boot[sector - 4..] != [0, 0, 0x55, 0xaa])
    {
        return false;
    }
    let checksum = exfat_boot_checksum(&region[..11 * sector]).to_le_bytes();
    region[11 * sector..]
        .chunks_exact(4)
        .all(|word| word == checksum)
}

impl Format {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fat32 => "FAT32",
            Self::ExFat => "exFAT",
        }
    }
}

pub(crate) fn backup_extent(boot: &[u8], sector: u64) -> Option<(Format, u64, u64)> {
    if boot.len() < 512 || boot[510..512] != [0x55, 0xaa] {
        return None;
    }
    if boot[3..11] == *b"EXFAT   " {
        let bytes = u64at(boot, 72).checked_mul(sector)?;
        return exfat_header(boot, sector as usize, bytes).then_some((
            Format::ExFat,
            12 * sector,
            bytes,
        ));
    }
    let bytes = u64::from(u32at(boot, 32)) * sector;
    fat32_boot(boot, sector, bytes).then_some((Format::Fat32, 6 * sector, bytes))
}

fn agrees(main: &[u8], copy: &[u8], fields: &[Range<usize>]) -> bool {
    fields.iter().all(|field| {
        let primary = &main[field.clone()];
        primary == &copy[field.clone()] || primary.iter().all(|&byte| byte == 0)
    })
}

fn corroborate_fat32(reader: &mut (impl Read + Seek), fs: &Fs, media: u8) -> Result<(), String> {
    let reserved = at(reader, fs.fat, 8)?;
    if u32at(&reserved, 0) & 0x0fff_ffff != 0x0fff_ff00 | u32::from(media)
        || u32at(&reserved, 4) & 0x03ff_ffff != 0x03ff_ffff
        || (fs.copies == 2 && at(reader, fs.fat + fs.fat_len, 8)? != reserved)
    {
        return Err("FAT32 保留表项与备用引导不一致。".into());
    }
    // Check the allocated, bounded root chain through the same parser used
    // for normal extraction, including per-entry mirror agreement.
    fs.directory(reader, fs.root, false, 0)?;
    Ok(())
}

fn corroborate_exfat(reader: &mut (impl Read + Seek), fs: &Fs) -> Result<(), String> {
    let reserved = at(reader, fs.fat, 8)?;
    if u32at(&reserved, 0) != 0xffff_fff8 || u32at(&reserved, 4) != u32::MAX {
        return Err("exFAT 保留表项与备用引导不一致。".into());
    }
    let mut allocated: HashSet<_> = fs
        .chain(reader, fs.root, None, false, false)?
        .into_iter()
        .collect();
    let root = fs.directory(reader, fs.root, false, 0)?;
    for kind in [0x81, 0x82] {
        let mut entries = root
            .chunks_exact(32)
            .take_while(|entry| entry[0] != 0)
            .filter(|entry| entry[0] == kind);
        let entry = entries.next().ok_or("exFAT 分配位图或大小写表缺失。")?;
        if entries.next().is_some() || (kind == 0x81 && entry[1] != 0) {
            return Err("exFAT 分配位图或大小写表不唯一。".into());
        }
        let length = u64at(entry, 24);
        let first = u32at(entry, 20);
        if length == 0 || (kind == 0x82 && (length > 131_072 || length % 2 != 0)) {
            return Err("exFAT 大小写表长度异常。".into());
        }
        for cluster in fs.chain(reader, first, Some(length), false, false)? {
            if !allocated.insert(cluster) {
                return Err("exFAT 根目录、位图或大小写表的分配重叠。".into());
            }
        }
        if kind == 0x82 {
            let upcase = fs.content(reader, first, length, false, false)?;
            let actual = upcase.iter().fold(0u32, |hash, &byte| {
                hash.rotate_right(1).wrapping_add(u32::from(byte))
            });
            if actual != u32at(entry, 4) {
                return Err("exFAT 大小写表校验失败。".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn inspect(
    reader: &mut (impl Read + Seek),
    sector: u64,
    cancel: &AtomicBool,
) -> Result<Option<BootCopy>, String> {
    used_space::stop(cancel)?;
    let bytes = reader.seek(SeekFrom::End(0)).map_err(e)?;
    if !matches!(sector, 512 | 4096) || bytes % sector != 0 || bytes < 24 * sector {
        return Ok(None);
    }
    let size = sector as usize;
    let primary = at(reader, 0, size)?;
    // A recognizable NTFS primary is handled by its own bootstrap verifier.
    if primary[3..11] == *b"NTFS    " {
        return Ok(None);
    }
    let fat_copy = at(reader, 6 * sector, 2 * size)?;
    let exfat_copy = at(reader, 12 * sector, 12 * size)?;
    let format = match (
        fat32_boot(&fat_copy, sector, bytes),
        exfat_boot(&exfat_copy, size, bytes),
    ) {
        (false, false) => return Ok(None),
        (true, false) => Format::Fat32,
        (false, true) => Format::ExFat,
        (true, true) => return Err("检测到互相冲突的备用引导依据。".into()),
    };
    if crate::bitlocker::boot_marker(&primary)
        || disk_core::btrfs::has_magic(reader, bytes)?
        || ntfs_reconstruct::has_tail_boot(reader, sector, bytes)?
        || (primary[3..11] != *b"EXFAT   " && at(reader, 1080, 2)? == [0x53, 0xef])
    {
        return Err("检测到其他文件系统或加密证据，不能采用备用引导。".into());
    }
    let conflict = "主备引导的文件系统、卷范围或身份不一致。";
    let healthy = "主引导可读，请使用常规目录读取。";
    let (start, main, copy, patch_len) = match format {
        Format::Fat32 => {
            if primary[3..11] == *b"EXFAT   "
                || !agrees(&primary, &fat_copy, &[11..36, 36..52, 67..71])
            {
                return Err(conflict.into());
            }
            if crate::recovery::fat_boot(primary[..512].try_into().unwrap()) {
                return Err(healthy.into());
            }
            let fsinfo = &fat_copy[size..];
            if u32at(fsinfo, 0) != 0x4161_5252
                || u32at(fsinfo, 484) != 0x6141_7272
                || u32at(fsinfo, 508) != 0xaa55_0000
            {
                return Err("FAT32 备用 FSInfo 签名无效。".into());
            }
            (6, primary, fat_copy, size)
        }
        Format::ExFat => {
            if !agrees(
                &primary,
                &exfat_copy,
                &[3..11, 11..64, 64..72, 72..104, 104..106, 108..111],
            ) {
                return Err(conflict.into());
            }
            let main = at(reader, 0, 12 * size)?;
            if exfat_boot(&main, size, bytes) {
                return Err(healthy.into());
            }
            (12, main, exfat_copy, 12 * size)
        }
    };
    let patch = Patch {
        at: 0,
        bytes: copy[..patch_len].to_vec(),
    };
    let mut metadata = used_space::Proof::default();
    {
        let mut view = Reader::new(reader, vec![patch.clone()], bytes);
        let mut audit =
            used_space::Audit::aligned(&mut view, 0, bytes, sector, &mut metadata, cancel)?;
        let fs = Fs::load(&mut audit)?;
        match format {
            Format::Fat32 => corroborate_fat32(&mut audit, &fs, copy[21])?,
            Format::ExFat => corroborate_exfat(&mut audit, &fs)?,
        }
    }
    used_space::stop(cancel)?;
    let mut evidence = Sha256::new();
    evidence.update(&main);
    evidence.update(&copy);
    evidence.update(metadata.digest());
    let proof = Proof {
        filesystem: format,
        backup_start_sector: start,
        boot_sectors: patch_len as u64 / sector,
        sector_size: sector,
        bytes,
        evidence_sha256: format!("{:x}", evidence.finalize()),
    };
    Ok(Some(BootCopy { proof, patch }))
}

#[cfg(test)]
#[path = "../../tests/fixtures/fat_view.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{self, ScanMode};
    use std::io::Cursor;

    fn check_file(image: &[u8], sector: usize, format: Format, expected_bytes: &[u8]) {
        let cancel = AtomicBool::new(false);
        let (mut view, proof) =
            recovery::view::Proof::open_boot(Cursor::new(image), sector as u64, &cancel).unwrap();
        assert!(matches!(&proof, recovery::view::Proof::Fat(proof) if proof.filesystem == format));
        let mut files = Vec::new();
        super::super::scan(
            &mut view,
            ScanMode::Existing,
            |_, _| Ok(()),
            |file| files.push(file),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        let mut reopened = proof.reopen_boot(Cursor::new(image), &cancel).unwrap();
        let mut content = Vec::new();
        recovery::extract(&mut reopened, &files[0], &mut content, u64::MAX, |_| Ok(())).unwrap();
        assert_eq!(content, expected_bytes);
        assert!(reopened.seek(SeekFrom::End(1)).is_err());
    }

    #[test]
    fn fat32_reconstructs_at_512_and_4096_bytes_with_verified_root() {
        for sector in [512, 4096] {
            let mut image = fixture::fat32(sector);
            let cancel = AtomicBool::new(false);
            assert!(inspect(&mut Cursor::new(&image), sector as u64, &cancel).is_err());
            image[..sector].fill(0);
            check_file(&image, sector, Format::Fat32, fixture::FAT_CONTENT);
            assert!(image[..sector].iter().all(|&b| b == 0));
            // Conflicting reserved FAT entries make the independent proof fail.
            let at = (u16at(&image[6 * sector..], 14) as usize
                + u32at(&image[6 * sector..], 36) as usize)
                * sector;
            image[at + 8] = 0;
            assert!(inspect(&mut Cursor::new(&image), sector as u64, &cancel).is_err());
        }
    }

    #[test]
    fn exfat_reconstructs_wiped_boot_and_checksum_only_damage_at_both_sector_sizes() {
        for sector in [512, 4096] {
            let mut image = fixture::exfat(sector);
            let cancel = AtomicBool::new(false);
            assert!(inspect(&mut Cursor::new(&image), sector as u64, &cancel).is_err());
            image[11 * sector] ^= 1;
            check_file(&image, sector, Format::ExFat, fixture::EXFAT_CONTENT);
            image[..12 * sector].fill(0);
            check_file(&image, sector, Format::ExFat, fixture::EXFAT_CONTENT);
            assert!(image[..12 * sector].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn fat_boot_rejects_conflicting_stale_or_unallocated_metadata() {
        let sector = 512;
        let cancel = AtomicBool::new(false);
        let mut image = fixture::exfat(sector);
        image[11 * sector] ^= 1;
        let (_, proof) = recovery::view::Proof::open_boot(Cursor::new(&image), 512, &cancel).unwrap();
        image[106] = 2;
        assert!(proof.reopen_boot(Cursor::new(&image), &cancel).is_err());
        image[100] ^= 1;
        assert!(inspect(&mut Cursor::new(&image), 512, &cancel).is_err());
        image[..12 * sector].fill(0);
        let mut foreign = image.clone();
        foreign[3..11].copy_from_slice(b"-FVE-FS-");
        assert!(inspect(&mut Cursor::new(&foreign), 512, &cancel).is_err());
        foreign[3..11].fill(0);
        foreign[1080..1082].copy_from_slice(&[0x53, 0xef]);
        assert!(inspect(&mut Cursor::new(&foreign), 512, &cancel).is_err());
        image[41 * sector] &= !1; // mark the root cluster free
        assert!(inspect(&mut Cursor::new(&image), 512, &cancel).is_err());
        image[41 * sector] |= 1;
        image[43 * sector] ^= 1;
        assert!(inspect(&mut Cursor::new(&image), 512, &cancel).is_err());
        image[43 * sector] ^= 1;
        image[23 * sector] ^= 1;
        assert!(inspect(&mut Cursor::new(&image), 512, &cancel)
            .unwrap()
            .is_none());
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(inspect(&mut Cursor::new(&image), 512, &cancel).is_err());
    }
}

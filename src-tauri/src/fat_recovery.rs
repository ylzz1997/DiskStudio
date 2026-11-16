//! Read-only FAT12/16/32 and exFAT directory recovery. Deleted allocation chains
//! are not guessed: only contiguous free extents are offered as candidates.
#[path = "fat_allocation.rs"]
pub mod allocation;
use crate::recovery::{Candidate, Locator, ScanMode, MAX_RESULTS};
use crate::recovery_tree::{Directory, MAX_DEPTH, ROOT};
pub(crate) use scan_progress::directory_metadata;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
};
fn e(v: impl std::fmt::Display) -> String {
    v.to_string()
}
fn u16at(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes(b[n..n + 2].try_into().unwrap())
}
fn u32at(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap())
}
fn u64at(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}
fn at<R: Read + Seek>(r: &mut R, offset: u64, n: usize) -> Result<Vec<u8>, String> {
    r.seek(SeekFrom::Start(offset)).map_err(e)?;
    let mut b = vec![0; n];
    r.read_exact(&mut b).map_err(e)?;
    Ok(b)
}
#[derive(Clone, Debug)]
pub struct Location {
    boot: [u8; 32],
    entries: Vec<(u64, Vec<u8>)>,
    first: u32,
    contiguous: bool,
    valid: u64,
}
struct Fs {
    bits: u8,
    fat: u64,
    fat_len: u64,
    copies: u8,
    mirror: bool,
    heap: u64,
    cluster: u64,
    count: u32,
    root: u32,
    root_fixed: Option<(u64, usize)>,
    boot: [u8; 32],
    bitmap: Vec<u8>,
}
impl Fs {
    fn load<R: Read + Seek>(r: &mut R) -> Result<Self, String> {
        let size = r.seek(SeekFrom::End(0)).map_err(e)?;
        let b = at(r, 0, 512)?;
        if b[510..] != [0x55, 0xaa] {
            return Err("引导扇区签名无效".into());
        }
        let boot = Sha256::digest(&b).into();
        if &b[3..11] == b"EXFAT   " {
            let ss = b[108];
            let cs = b[109];
            if !(9..=12).contains(&ss) || cs > 25 - ss || b[110] != 1 || u16at(&b, 104) != 0x100 {
                return Err("不支持该 exFAT 几何布局、版本或 TexFAT 双 FAT".into());
            }
            let sector = 1u64 << ss;
            let cluster = sector << cs;
            let count = u32at(&b, 92);
            let fat = u32at(&b, 80) as u64 * sector;
            let fat_len = u32at(&b, 84) as u64 * sector;
            let heap = u32at(&b, 88) as u64 * sector;
            let volume = u64at(&b, 72).checked_mul(sector).ok_or("卷容量溢出")?;
            if count == 0
                || count > 0x7ffffffd
                || volume > size
                || fat < 24 * sector
                || fat_len < (count as u64 + 2) * 4
                || fat + fat_len > heap
                || heap + count as u64 * cluster > volume
                || cluster > 32 * 1024 * 1024
            {
                return Err("exFAT 范围越界".into());
            }
            // Validate the main boot region checksum; mutable flags and PercentInUse are excluded.
            let boot_region = at(r, 0, (sector * 12) as usize)?;
            let mut checksum = 0u32;
            for (i, v) in boot_region[..sector as usize * 11].iter().enumerate() {
                if ![106, 107, 112].contains(&i) {
                    checksum = checksum.rotate_right(1).wrapping_add(*v as u32);
                }
            }
            if boot_region[sector as usize * 11..]
                .chunks_exact(4)
                .any(|x| u32at(x, 0) != checksum)
            {
                return Err("exFAT 引导区校验失败".into());
            }
            let mut fs = Self {
                bits: 64,
                fat,
                fat_len,
                copies: 1,
                mirror: false,
                heap,
                cluster,
                count,
                root: u32at(&b, 96),
                root_fixed: None,
                boot,
                bitmap: vec![],
            };
            let root = fs.directory(r, fs.root, false, 0)?;
            let maps: Vec<_> = root
                .chunks_exact(32)
                .filter(|x| x[0] == 0x81 && x[1] & 1 == 0)
                .collect();
            if maps.len() != 1 {
                return Err("exFAT 分配位图缺失或不唯一".into());
            }
            let m = maps[0];
            let length = u64at(m, 24);
            if length != (count as u64).div_ceil(8) || length > 64 * 1024 * 1024 {
                return Err("exFAT 位图长度异常或过大".into());
            }
            fs.bitmap = fs.content(r, u32at(m, 20), length, false, false)?;
            Ok(fs)
        } else {
            let sector = u16at(&b, 11) as u64;
            let spc = b[13] as u64;
            let reserved = u16at(&b, 14) as u64;
            let copies = b[16];
            if !matches!(sector, 512 | 1024 | 2048 | 4096)
                || spc == 0
                || !spc.is_power_of_two()
                || spc > 128
                || reserved == 0
                || !(1..=2).contains(&copies)
            {
                return Err("不是支持的 FAT 文件系统".into());
            }
            let sectors = if u16at(&b, 19) != 0 {
                u16at(&b, 19) as u64
            } else {
                u32at(&b, 32) as u64
            };
            let fats = if u16at(&b, 22) != 0 {
                u16at(&b, 22) as u64
            } else {
                u32at(&b, 36) as u64
            };
            let root_bytes = u16at(&b, 17) as u64 * 32;
            let root_sectors = root_bytes.div_ceil(sector);
            let first = reserved + copies as u64 * fats + root_sectors;
            if sectors <= first || sectors * sector > size {
                return Err("FAT 卷范围无效".into());
            }
            let count = (sectors - first) / spc;
            let bits = if count < 4085 {
                12
            } else if count < 65525 {
                16
            } else {
                32
            };
            if count > 0x0ffffff5
                || fats * sector < (count + 2) * bits as u64 / 8
                || (bits == 32 && (root_bytes != 0 || u16at(&b, 22) != 0 || u16at(&b, 42) != 0))
                || (bits != 32 && root_bytes == 0)
            {
                return Err("FAT 表容量或根目录不一致".into());
            }
            let flags = if bits == 32 { u16at(&b, 40) } else { 0 };
            let mirror = flags & 0x80 == 0;
            let active = if mirror { 0 } else { flags & 15 } as u64;
            if active >= copies as u64 {
                return Err("活动 FAT 编号越界".into());
            }
            Ok(Self {
                bits,
                fat: (reserved + active * fats) * sector,
                fat_len: fats * sector,
                copies,
                mirror,
                heap: first * sector,
                cluster: sector * spc,
                count: count as u32,
                root: if bits == 32 { u32at(&b, 44) } else { 0 },
                root_fixed: if bits == 32 {
                    None
                } else {
                    Some((
                        (reserved + copies as u64 * fats) * sector,
                        root_bytes as usize,
                    ))
                },
                boot,
                bitmap: vec![],
            })
        }
    }
    fn offset(&self, c: u32) -> Result<u64, String> {
        if c < 2 || c as u64 >= self.count as u64 + 2 {
            Err("簇编号越界".into())
        } else {
            Ok(self.heap + (c - 2) as u64 * self.cluster)
        }
    }
    fn fat<R: Read + Seek>(&self, r: &mut R, c: u32) -> Result<u32, String> {
        self.offset(c)?;
        let pos = match self.bits {
            12 => c as u64 * 3 / 2,
            16 => c as u64 * 2,
            _ => c as u64 * 4,
        };
        let n = if self.bits <= 16 { 2 } else { 4 };
        if pos + n as u64 > self.fat_len {
            return Err("FAT 项越界".into());
        }
        let b = at(r, self.fat + pos, n)?;
        if self.mirror && self.copies == 2 && at(r, self.fat + self.fat_len + pos, n)? != b {
            return Err("两份 FAT 表不一致".into());
        }
        Ok(match self.bits {
            12 => {
                let v = u16at(&b, 0) as u32;
                if c & 1 == 0 {
                    v & 0xfff
                } else {
                    v >> 4
                }
            }
            16 => u16at(&b, 0) as u32,
            32 => u32at(&b, 0) & 0x0fffffff,
            _ => u32at(&b, 0),
        })
    }
    fn end(&self, c: u32) -> bool {
        c >= match self.bits {
            12 => 0xff8,
            16 => 0xfff8,
            32 => 0xffffff8,
            _ => 0xfffffff8,
        }
    }
    fn allocated(&self, c: u32) -> Result<bool, String> {
        self.offset(c)?;
        let i = (c - 2) as usize;
        Ok(self.bitmap.get(i / 8).ok_or("位图越界")? & (1 << (i % 8)) != 0)
    }
    fn chain<R: Read + Seek>(
        &self,
        r: &mut R,
        first: u32,
        size: Option<u64>,
        contiguous: bool,
        deleted: bool,
    ) -> Result<Vec<u32>, String> {
        let needed = size.map(|s| s.div_ceil(self.cluster));
        if needed == Some(0) {
            return Ok(vec![]);
        }
        if needed.is_some_and(|n| n > 1_048_576 || n > self.count as u64) {
            return Err("文件簇链超过上限".into());
        }
        let mut result = vec![];
        let mut seen = HashSet::new();
        let mut c = first;
        loop {
            self.offset(c)?;
            if !seen.insert(c) || result.len() >= 1_048_576 {
                return Err("簇链循环或过长".into());
            }
            if self.bits == 64 && !self.bitmap.is_empty() && self.allocated(c)? == deleted {
                return Err("文件簇的分配状态已变化".into());
            }
            if deleted && self.bits != 64 && self.fat(r, c)? != 0 {
                return Err("已删除文件的数据簇已重新分配".into());
            }
            result.push(c);
            if needed == Some(result.len() as u64) {
                if !contiguous && !deleted && !self.end(self.fat(r, c)?) {
                    return Err("文件长度与簇链不一致".into());
                }
                break;
            }
            if contiguous || deleted {
                c = c.checked_add(1).ok_or("簇编号溢出")?;
            } else {
                let next = self.fat(r, c)?;
                if self.end(next) {
                    if needed.is_some() {
                        return Err("簇链截断".into());
                    }
                    break;
                }
                c = next;
            }
        }
        Ok(result)
    }
    fn content<R: Read + Seek>(
        &self,
        r: &mut R,
        first: u32,
        size: u64,
        contiguous: bool,
        deleted: bool,
    ) -> Result<Vec<u8>, String> {
        if size > 64 * 1024 * 1024 {
            return Err("目录或位图超过 64 MiB 上限".into());
        }
        let chain = self.chain(r, first, Some(size), contiguous, deleted)?;
        let mut bytes = Vec::with_capacity(size as usize);
        for c in chain {
            let n = (size - bytes.len() as u64).min(self.cluster) as usize;
            bytes.extend(at(r, self.offset(c)?, n)?);
        }
        Ok(bytes)
    }
    fn directory<R: Read + Seek>(
        &self,
        r: &mut R,
        first: u32,
        contiguous: bool,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        if first == 0 {
            let (off, n) = self.root_fixed.ok_or("无固定根目录")?;
            return at(r, off, n);
        }
        if contiguous {
            return self.content(r, first, size, true, false);
        }
        let chain = self.chain(r, first, None, false, false)?;
        if chain.len() as u64 * self.cluster > 64 * 1024 * 1024 {
            return Err("目录超过 64 MiB 上限".into());
        }
        let mut out = vec![];
        for c in chain {
            out.extend(at(r, self.offset(c)?, self.cluster as usize)?);
        }
        Ok(out)
    }
    fn entry_offsets<R: Read + Seek>(
        &self,
        r: &mut R,
        first: u32,
        contiguous: bool,
        size: u64,
    ) -> Result<Vec<u64>, String> {
        if first == 0 {
            let (off, n) = self.root_fixed.ok_or("根目录缺失")?;
            return Ok((0..n / 32).map(|i| off + i as u64 * 32).collect());
        }
        let chain = self.chain(
            r,
            first,
            if contiguous { Some(size) } else { None },
            contiguous,
            false,
        )?;
        Ok(chain
            .into_iter()
            .flat_map(|c| {
                let start = self.offset(c).unwrap();
                (0..self.cluster / 32).map(move |i| start + i * 32)
            })
            .collect())
    }
}

#[cfg(test)]
pub fn scan<R: Read + Seek>(
    r: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    scan_with_directories(r, mode, tick, found, |_| Ok(()))
}
#[path = "fat_scan.rs"]
pub mod scan_progress;
pub fn scan_with_directories<R: Read + Seek>(
    r: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory_found: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    scan_progress::continue_scan(
        r,
        mode,
        &mut Default::default(),
        tick,
        found,
        directory_found,
    )
}

fn candidate(
    id: usize,
    name: String,
    size: u64,
    deleted: bool,
    method: &str,
    metadata: crate::file_metadata::Metadata,
    support: Result<(), String>,
    locator: Location,
    directory: String,
) -> Candidate {
    let supported = support.is_ok();
    Candidate {
        id: id as u32,
        name,
        size,
        deleted,
        method: format!("{method} 目录"),
        metadata,
        note: support.err().unwrap_or_else(|| {
            if deleted {
                "连续空闲簇候选；原文件可能碎片化、已覆盖或已被 TRIM".into()
            } else {
                "现存文件；导出前重新校验目录项与簇链".into()
            }
        }),
        supported,
        directory: Some(directory),
        directory_note: String::new(),
        locator: Locator::Fat(locator),
    }
}
fn fat_name(b: &[u8], long: &[(u64, Vec<u8>)], deleted: bool) -> String {
    let checksum = b[..11]
        .iter()
        .fold(0u8, |a, x| a.rotate_right(1).wrapping_add(*x));
    let valid = !long.is_empty()
        && long.iter().enumerate().all(|(i, (_, x))| {
            x[11] == 15
                && x[12] == 0
                && u16at(x, 26) == 0
                && x[13] == long[0].1[13]
                && if deleted {
                    x[0] == 0xe5
                } else {
                    x[13] == checksum
                        && x[0] & 31 == (long.len() - i) as u8
                        && (i != 0 || x[0] & 0x40 != 0)
                }
        });
    if valid {
        let chars: Vec<u16> = long
            .iter()
            .rev()
            .flat_map(|(_, x)| [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].map(|n| u16at(x, n)))
            .take_while(|c| *c != 0 && *c != 0xffff)
            .collect();
        if !chars.is_empty() {
            return String::from_utf16_lossy(&chars);
        }
    }
    let mut short = b[..11].to_vec();
    if deleted {
        short[0] = b'_';
    } else if short[0] == 5 {
        short[0] = 0xe5;
    }
    let base = String::from_utf8_lossy(&short[..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&short[8..]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}
pub fn extract<R: Read + Seek, W: Write>(
    r: &mut R,
    c: &Candidate,
    l: &Location,
    out: &mut W,
    limit: u64,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    extract_with_metadata(r, c, l, out, limit, tick).map(|e| e.bytes)
}
pub fn extract_with_metadata<R: Read + Seek, W: Write>(
    r: &mut R,
    c: &Candidate,
    l: &Location,
    out: &mut W,
    limit: u64,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<crate::recovery::Extraction, String> {
    let fs = Fs::load(r)?;
    if fs.boot != l.boot {
        return Err("文件系统引导信息已变化，请重新扫描".into());
    }
    for (offset, bytes) in &l.entries {
        if at(r, *offset, bytes.len())? != *bytes {
            return Err("目录项已变化或被复用，请重新扫描".into());
        }
    }
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let metadata = scan_progress::extraction_metadata(fs.bits, l, size, c)?;
    let chain = fs.chain(r, l.first, Some(c.size), l.contiguous, c.deleted)?;
    let take = c.size.min(limit);
    let mut done = 0;
    for cluster in chain {
        if done == take {
            break;
        }
        tick(done)?;
        let n = (take - done).min(fs.cluster) as usize;
        let valid = l.valid.saturating_sub(done).min(n as u64) as usize;
        let mut bytes = vec![0; n];
        if valid > 0 {
            bytes[..valid].copy_from_slice(&at(r, fs.offset(cluster)?, valid)?);
        }
        out.write_all(&bytes).map_err(e)?;
        done += n as u64;
    }
    tick(done)?;
    Ok(crate::recovery::Extraction {
        bytes: done,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn metadata_fat_exfat_flows_through_checked_directory_records() {
        for (mut bytes, at, exfat) in [
            (fixture(), 1024, false),
            (exfat_fixture(), 512 * 25 + 32, true),
        ] {
            let date = (44u16 << 9) | (2 << 5) | 29;
            let clock = (12u16 << 11) | (34 << 5) | 28;
            let (d, t, inc) = if exfat { (10, 8, 20) } else { (16, 14, 13) };
            bytes[at + d..at + d + 2].copy_from_slice(&date.to_le_bytes());
            bytes[at + t..at + t + 2].copy_from_slice(&clock.to_le_bytes());
            bytes[at + inc] = 150;
            bytes[at + if exfat { 4 } else { 11 }] = 0x27;
            if exfat {
                bytes[at + 22] = 0x97;
                let mut sum = 0u16;
                for (i, v) in bytes[at..at + 96].iter().enumerate() {
                    if i != 2 && i != 3 {
                        sum = sum.rotate_right(1).wrapping_add(*v as u16);
                    }
                }
                bytes[at + 2..at + 4].copy_from_slice(&sum.to_le_bytes());
            }
            let mut found = vec![];
            scan(
                &mut Cursor::new(&bytes),
                ScanMode::Existing,
                |_, _| Ok(()),
                |c| found.push(c),
            )
            .unwrap();
            assert_eq!(found.len(), 1);
            let m = &found[0].metadata;
            assert_eq!(m.created.as_ref().unwrap().text, "2024-02-29 12:34:57.5");
            assert_eq!(
                m.created.as_ref().unwrap().zone,
                if exfat {
                    "UTC+05:45"
                } else {
                    "本地时间（时区未知）"
                }
            );
            assert_eq!(m.attributes, ["只读", "隐藏", "系统", "归档"]);
            found[0].metadata = crate::file_metadata::Metadata::default();
            let mut out = vec![];
            let receipt = crate::recovery::extract_with_metadata(
                &mut Cursor::new(bytes),
                &found[0],
                &mut out,
                u64::MAX,
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(receipt.metadata.windows_flags, Some(0x27));
            assert_eq!(
                receipt.metadata.created.unwrap().instant_ns.is_some(),
                exfat
            );
            assert_eq!(out, b"hello");
        }
    }
    #[test]
    fn browse_existing_fat_and_exfat_preserves_bytes_and_excludes_deleted_entries() {
        for (mut bytes, entry, exfat) in [
            (fixture(), 1024, false),
            (exfat_fixture(), 512 * 25 + 32, true),
        ] {
            let mut files = vec![];
            scan(
                &mut Cursor::new(&bytes),
                ScanMode::Existing,
                |_, _| Ok(()),
                |c| files.push(c),
            )
            .unwrap();
            assert_eq!(files.len(), 1);
            assert!(!files[0].deleted && files[0].supported);
            let mut out = vec![];
            crate::recovery::extract(
                &mut Cursor::new(&bytes),
                &files[0],
                &mut out,
                u64::MAX,
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(out, b"hello");
            if exfat {
                for at in [entry, entry + 32, entry + 64] {
                    bytes[at] &= 0x7f;
                }
            } else {
                bytes[entry] = 0xe5;
            }
            files.clear();
            scan(
                &mut Cursor::new(&bytes),
                ScanMode::Existing,
                |_, _| Ok(()),
                |c| files.push(c),
            )
            .unwrap();
            assert!(files.is_empty());
            scan(
                &mut Cursor::new(&bytes),
                ScanMode::AllFiles,
                |_, _| Ok(()),
                |c| files.push(c),
            )
            .unwrap();
            assert_eq!(files.len(), 1);
            assert!(files[0].deleted);
        }
    }
    pub(super) fn fixture() -> Vec<u8> {
        let mut b = vec![0; 512 * 100];
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 1;
        b[14] = 1;
        b[16] = 1;
        b[17] = 16;
        b[19] = 100;
        b[22] = 1;
        b[510] = 0x55;
        b[511] = 0xaa;
        b[512..515].copy_from_slice(&[0xf8, 0xff, 0xff]);
        b[515] = 0xff;
        b[516] = 0x0f;
        b[1024..1035].copy_from_slice(b"HELLO   TXT");
        b[1035] = 0x20;
        b[1050] = 2;
        b[1052] = 5;
        b[1536..1541].copy_from_slice(b"hello");
        b
    }
    #[test]
    fn nested_fat_directory_keeps_identical_names_separate_and_rejects_revisits() {
        let mut b = fixture();
        let file = b[1024..1056].to_vec();
        b[1056..1067].copy_from_slice(b"FOLDER     ");
        b[1067] = 0x10;
        b[1082] = 3;
        b[516] = 0xff;
        b[517] = 0xff; // Cluster 3 EOF.
        b[2048..2080].copy_from_slice(&file);
        // Subdirectory entry points back to its own cluster; never traversed twice.
        let folder = b[1056..1088].to_vec();
        b[2080..2112].copy_from_slice(&folder);
        let mut catalog = crate::recovery_tree::Catalog::default();
        let mut found = vec![];
        let message = scan_with_directories(
            &mut Cursor::new(&b),
            ScanMode::AllFiles,
            |_, _| Ok(()),
            |c| found.push(c),
            |d| catalog.insert(d),
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, found[1].name);
        assert_eq!(found[0].directory.as_deref(), Some(ROOT));
        assert_ne!(found[0].directory, found[1].directory);
        assert_eq!(
            catalog
                .path(found[1].directory.as_deref().unwrap())
                .unwrap()[1]
                .name,
            "FOLDER"
        );
        assert!(message.contains("跳过 1"));
        for file in found {
            let mut output = Vec::new();
            crate::recovery::extract(&mut Cursor::new(&b), &file, &mut output, 99, |_| Ok(()))
                .unwrap();
            assert_eq!(output, b"hello");
        }
    }
    #[test]
    fn fat12_live_and_deleted_export_and_stale_rejection() {
        let mut bytes = fixture();
        let mut found = vec![];
        scan(
            &mut Cursor::new(&bytes),
            ScanMode::AllFiles,
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "HELLO.TXT");
        let mut out = vec![];
        crate::recovery::extract(
            &mut Cursor::new(&bytes),
            &found[0],
            &mut out,
            99,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(out, b"hello");
        bytes[1024] = 0xe5;
        bytes[515] = 0;
        bytes[516] = 0;
        assert!(crate::recovery::extract(
            &mut Cursor::new(&bytes),
            &found[0],
            &mut vec![],
            99,
            |_| Ok(())
        )
        .is_err());
        found.clear();
        scan(
            &mut Cursor::new(&bytes),
            ScanMode::Deleted,
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        assert!(found[0].supported);
        out.clear();
        crate::recovery::extract(
            &mut Cursor::new(&bytes),
            &found[0],
            &mut out,
            99,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(out, b"hello");
        bytes[515] = 0xff;
        assert!(crate::recovery::extract(
            &mut Cursor::new(&bytes),
            &found[0],
            &mut vec![],
            99,
            |_| Ok(())
        )
        .is_err());
    }
    #[test]
    fn invalid_geometry_and_loop_are_rejected() {
        let mut b = fixture();
        b[13] = 3;
        assert!(Fs::load(&mut Cursor::new(&b)).is_err());
        b[13] = 1;
        b[1052..1056].copy_from_slice(&1024u32.to_le_bytes());
        b[515] = 2;
        b[516] = 0;
        let mut found = vec![];
        scan(
            &mut Cursor::new(b),
            ScanMode::AllFiles,
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        assert!(!found[0].supported);
    }
    pub(super) fn exfat_fixture() -> Vec<u8> {
        let mut b = vec![0; 512 * 125];
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[72..80].copy_from_slice(&125u64.to_le_bytes());
        b[80] = 24;
        b[84] = 1;
        b[88] = 25;
        b[92] = 100;
        b[96] = 2;
        b[105] = 1;
        b[108] = 9;
        b[110] = 1;
        b[510] = 0x55;
        b[511] = 0xaa;
        let mut sum = 0u32;
        for (i, v) in b[..512 * 11].iter().enumerate() {
            if ![106, 107, 112].contains(&i) {
                sum = sum.rotate_right(1).wrapping_add(*v as u32);
            }
        }
        for x in b[512 * 11..512 * 12].chunks_exact_mut(4) {
            x.copy_from_slice(&sum.to_le_bytes());
        }
        b[512 * 24 + 8..512 * 24 + 16].fill(0xff);
        let root = 512 * 25;
        b[root] = 0x81;
        b[root + 20] = 3;
        b[root + 24] = 13;
        b[512 * 26] = 7;
        let f = root + 32;
        b[f] = 0x85;
        b[f + 1] = 2;
        b[f + 4] = 0x20;
        b[f + 32] = 0xc0;
        b[f + 33] = 3;
        b[f + 35] = 5;
        b[f + 40] = 5;
        b[f + 52] = 4;
        b[f + 56] = 5;
        b[f + 64] = 0xc1;
        for (i, c) in "a.txt".encode_utf16().enumerate() {
            b[f + 66 + i * 2..f + 68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let mut s = 0u16;
        for (i, v) in b[f..f + 96].iter().enumerate() {
            if i != 2 && i != 3 {
                s = s.rotate_right(1).wrapping_add(*v as u16);
            }
        }
        b[f + 2..f + 4].copy_from_slice(&s.to_le_bytes());
        b[512 * 27..512 * 27 + 5].copy_from_slice(b"hello");
        b
    }
    #[test]
    fn owned_dispatch_does_not_confuse_fat_data_or_exfat_boot_code_with_ext() {
        use std::sync::{atomic::AtomicBool, Arc};
        for mut b in [fixture(), exfat_fixture()] {
            b[1080..1082].copy_from_slice(&[0x53, 0xef]);
            if &b[3..11] == b"EXFAT   " {
                let mut sum = 0u32;
                for (i, v) in b[..512 * 11].iter().enumerate() {
                    if ![106, 107, 112].contains(&i) {
                        sum = sum.rotate_right(1).wrapping_add(*v as u32);
                    }
                }
                for x in b[512 * 11..512 * 12].chunks_exact_mut(4) {
                    x.copy_from_slice(&sum.to_le_bytes());
                }
            }
            let mut files = vec![];
            crate::recovery::scan_filesystem_owned(
                Cursor::new(b.clone()),
                ScanMode::AllFiles,
                Arc::new(AtomicBool::new(false)),
                |_, _| Ok(()),
                |c| files.push(c),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(files.len(), 1);
            assert!(matches!(files[0].locator, Locator::Fat(_)));
            let mut output = vec![];
            crate::recovery::extract_owned(
                Cursor::new(b),
                &files[0],
                &mut output,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(output, b"hello");
        }
    }
    #[test]
    fn nested_exfat_directories_retain_paths_and_stop_cluster_revisits() {
        let mut b = exfat_fixture();
        let root_file = 512 * 25 + 32;
        let file = b[root_file..root_file + 96].to_vec();
        let mut directory = file.clone();
        directory[4] = 0x10;
        directory[35] = 6;
        directory[40..48].copy_from_slice(&512u64.to_le_bytes());
        directory[52..56].copy_from_slice(&5u32.to_le_bytes());
        directory[56..64].copy_from_slice(&512u64.to_le_bytes());
        directory[66..96].fill(0);
        for (i, c) in "Folder".encode_utf16().enumerate() {
            directory[66 + i * 2..68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let checksum = directory
            .iter()
            .enumerate()
            .filter(|(i, _)| ![2, 3].contains(i))
            .fold(0u16, |sum, (_, byte)| {
                sum.rotate_right(1).wrapping_add(*byte as u16)
            });
        directory[2..4].copy_from_slice(&checksum.to_le_bytes());
        b[root_file + 96..root_file + 192].copy_from_slice(&directory);
        b[512 * 26] = 15; // Root, allocation bitmap, file, directory are allocated.
        let nested = 512 * 28;
        b[nested..nested + 96].copy_from_slice(&file);
        b[nested + 96..nested + 192].copy_from_slice(&directory);
        let mut catalog = crate::recovery_tree::Catalog::default();
        let mut files = vec![];
        let message = scan_with_directories(
            &mut Cursor::new(&b),
            ScanMode::AllFiles,
            |_, _| Ok(()),
            |c| files.push(c),
            |d| catalog.insert(d),
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, files[1].name);
        assert_eq!(files[0].directory.as_deref(), Some(ROOT));
        assert_eq!(
            catalog
                .path(files[1].directory.as_deref().unwrap())
                .unwrap()[1]
                .name,
            "Folder"
        );
        assert!(message.contains("跳过 1"));
        for c in files {
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&b), &c, &mut out, 99, |_| Ok(())).unwrap();
            assert_eq!(out, b"hello");
        }
    }
    #[test]
    fn exfat_live_deleted_and_bitmap_reuse_are_checked() {
        let mut b = exfat_fixture();
        let f = 512 * 25 + 32;
        for deleted in [false, true] {
            if deleted {
                b[f] &= 0x7f;
                b[f + 32] &= 0x7f;
                b[f + 64] &= 0x7f;
                b[512 * 26] = 3;
            }
            let mut found = vec![];
            scan(
                &mut Cursor::new(&b),
                ScanMode::AllFiles,
                |_, _| Ok(()),
                |c| found.push(c),
            )
            .unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].deleted, deleted);
            assert!(found[0].supported);
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&b), &found[0], &mut out, 99, |_| Ok(()))
                .unwrap();
            assert_eq!(out, b"hello");
            if deleted {
                b[512 * 26] = 7;
                assert!(crate::recovery::extract(
                    &mut Cursor::new(&b),
                    &found[0],
                    &mut vec![],
                    99,
                    |_| Ok(())
                )
                .is_err());
            }
        }
        b[120] ^= 1;
        assert!(Fs::load(&mut Cursor::new(b)).is_err());
    }
}

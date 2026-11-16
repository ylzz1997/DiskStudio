//! Find filesystem boundaries without consulting or modifying a partition table.
//! All I/O is confined to the selected range; signatures alone are not results.
use crate::surface_scan::Range;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};
const BLOCK: usize = 1024 * 1024;
pub const MAX_CANDIDATES: usize = 256;
const MAX_PROBES: usize = 65536;
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    pub filesystem: String,
    pub offset: u64,
    pub size: u64,
    pub evidence: String,
    pub warning: String,
    #[serde(skip)]
    fingerprint: String,
}
fn u16at(b: &[u8], n: usize) -> u64 {
    u16::from_le_bytes(b[n..n + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], n: usize) -> u64 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap()) as u64
}
fn u64at(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}
fn canceled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("搜索已停止；保留已检查候选，尚未修改分区表。".into())
    } else {
        Ok(())
    }
}
struct Probe<'a, R> {
    reader: &'a mut R,
    start: u64,
    size: u64,
    hash: Sha256,
    cancel: &'a AtomicBool,
}
impl<R: Read + Seek> Probe<'_, R> {
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        canceled(self.cancel)?;
        if len > 128 * 1024
            || offset
                .checked_add(len as u64)
                .is_none_or(|end| end > self.size)
        {
            return Err("文件系统证据读取超出搜索范围".into());
        }
        let mut bytes = vec![0; len];
        self.reader
            .seek(SeekFrom::Start(self.start + offset))
            .map_err(|e| e.to_string())?;
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| format!("候选证据读取失败，偏移 {}：{e}", self.start + offset))?;
        self.hash.update(offset.to_le_bytes());
        self.hash.update((len as u64).to_le_bytes());
        self.hash.update(&bytes);
        Ok(bytes)
    }
    fn limit(&mut self, size: u64, sector: u64) -> bool {
        if size < 4096 || size > self.size || size % sector != 0 {
            return false;
        }
        self.size = size;
        true
    }
}
fn record_valid(b: &[u8], sector: usize) -> bool {
    if b.len() < 512 || &b[..4] != b"FILE" || b.len() % sector != 0 {
        return false;
    }
    let at = u16at(b, 4) as usize;
    let count = u16at(b, 6) as usize;
    if count != b.len() / sector + 1
        || at < 8
        || at + count * 2 > b.len()
        || u32at(b, 24) > b.len() as u64
    {
        return false;
    }
    b.chunks_exact(sector)
        .all(|s| s[sector - 2..] == b[at..at + 2])
}
fn ntfs<R: Read + Seek>(
    p: &mut Probe<R>,
    b: &[u8],
    sector: u64,
) -> Result<Option<(String, String, String)>, String> {
    let Ok(fs) = ntfs::Ntfs::new(&mut std::io::Cursor::new(b)) else {
        return Ok(None);
    };
    let bs = u16at(b, 11);
    let size = fs.size();
    let cluster = fs.cluster_size() as u64;
    if bs != sector || !p.limit(size, sector) {
        return Ok(None);
    }
    let record = if (b[64] as i8) < 0 {
        1u64.checked_shl((b[64] as i8).unsigned_abs() as u32)
    } else {
        cluster.checked_mul(b[64] as u64)
    };
    let Some(record) = record.filter(|n| (512..=65536).contains(n) && n % sector == 0) else {
        return Ok(None);
    };
    for lcn in [u64at(b, 48), u64at(b, 56)] {
        let Some(at) = lcn
            .checked_mul(cluster)
            .filter(|at| *at >= sector && at.checked_add(record).is_some_and(|end| end <= size))
        else {
            return Ok(None);
        };
        if !record_valid(&p.read(at, record as usize)?, sector as usize) {
            return Ok(None);
        }
    }
    Ok(Some((
        "NTFS".into(),
        "引导几何、MFT 与 MFTMirr 文件记录及更新序列".into(),
        "容量取自文件系统记录，不代表原分区精确末端；未检查完整 MFT。".into(),
    )))
}
fn fat<R: Read + Seek>(
    p: &mut Probe<R>,
    b: &[u8],
    sector: u64,
) -> Result<Option<(String, String, String)>, String> {
    let bs = u16at(b, 11);
    let spc = b[13] as u64;
    let reserved = u16at(b, 14);
    let copies = b[16] as u64;
    if bs != sector
        || spc == 0
        || !spc.is_power_of_two()
        || spc > 128
        || reserved == 0
        || !(1..=2).contains(&copies)
    {
        return Ok(None);
    }
    let sectors = if u16at(b, 19) != 0 {
        u16at(b, 19)
    } else {
        u32at(b, 32)
    };
    let fats = if u16at(b, 22) != 0 {
        u16at(b, 22)
    } else {
        u32at(b, 36)
    };
    let roots = u16at(b, 17);
    let first = reserved + copies * fats + (roots * 32).div_ceil(bs);
    if sectors <= first || fats == 0 || !p.limit(sectors * bs, sector) {
        return Ok(None);
    }
    let count = (sectors - first) / spc;
    let bits = if count < 4085 {
        12
    } else if count < 65525 {
        16
    } else {
        32
    };
    if count == 0
        || count > 0x0ffffff5
        || fats * bs < ((count + 2) * bits).div_ceil(8)
        || (bits == 32
            && (roots != 0
                || u16at(b, 22) != 0
                || u16at(b, 42) != 0
                || u32at(b, 44) < 2
                || u32at(b, 44) >= count + 2))
        || (bits != 32 && (roots == 0 || u16at(b, 22) == 0))
    {
        return Ok(None);
    }
    let mut first_fat = None;
    for copy in 0..copies {
        let header = p.read((reserved + copy * fats) * bs, 8)?;
        let valid = match bits {
            12 => header[0] == b[21] && header[1..3] == [255, 255],
            16 => {
                u16at(&header, 0) == 0xff00 + b[21] as u64 && u16at(&header, 2) & 0x3fff == 0x3fff
            }
            _ => {
                u32at(&header, 0) & 0x0fffffff == 0x0fffff00 + b[21] as u64
                    && u32at(&header, 4) & 0x03ffffff == 0x03ffffff
            }
        };
        if !valid {
            return Ok(None);
        }
        if let Some(first) = &first_fat {
            if first != &header {
                return Ok(None);
            }
        } else {
            first_fat = Some(header);
        }
    }
    Ok(Some((
        format!("FAT{bits}"),
        "BPB 几何、实际簇数与各 FAT 保留项".into(),
        "只核对文件系统边界和 FAT 保留项，未检查完整簇链。".into(),
    )))
}
fn exfat<R: Read + Seek>(
    p: &mut Probe<R>,
    b: &[u8],
    sector: u64,
) -> Result<Option<(String, String, String)>, String> {
    let ss = b[108];
    let cs = b[109];
    if !(9..=12).contains(&ss)
        || (1u64 << ss) != sector
        || cs > 25 - ss
        || b[110] != 1
        || u16at(b, 104) != 0x100
        || b[11..64].iter().any(|b| *b != 0)
    {
        return Ok(None);
    }
    let Some(size) = u64at(b, 72).checked_mul(sector) else {
        return Ok(None);
    };
    let fat = u32at(b, 80) * sector;
    let fat_len = u32at(b, 84) * sector;
    let heap = u32at(b, 88) * sector;
    let count = u32at(b, 92);
    let root = u32at(b, 96);
    let cluster = sector << cs;
    if !p.limit(size, sector)
        || size < 24 * sector
        || count == 0
        || count > 0xfffffff5
        || fat < 24 * sector
        || fat_len < (count + 2) * 4
        || fat + fat_len > heap
        || heap + count * cluster > size
        || root < 2
        || root >= count + 2
    {
        return Ok(None);
    }
    let region = p.read(0, (12 * sector) as usize)?;
    let mut checksum = 0u32;
    for (i, v) in region[..11 * sector as usize].iter().enumerate() {
        if ![106, 107, 112].contains(&i) {
            checksum = checksum.rotate_right(1).wrapping_add(*v as u32);
        }
    }
    if region[11 * sector as usize..]
        .chunks_exact(4)
        .any(|b| u32at(b, 0) != checksum as u64)
    {
        return Ok(None);
    }
    let fat_header = p.read(fat, 8)?;
    if u32at(&fat_header, 0) != 0xfffffff8 || u32at(&fat_header, 4) != 0xffffffff {
        return Ok(None);
    }
    Ok(Some((
        "exFAT".into(),
        "主引导区校验和、簇堆边界与 FAT 保留项".into(),
        "未检查完整分配位图、目录与备份引导区。".into(),
    )))
}
fn crc32c(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for b in bytes {
        c ^= *b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(c & 1));
        }
    }
    c
}
fn ext<R: Read + Seek>(
    p: &mut Probe<R>,
    b: &[u8],
    sector: u64,
) -> Result<Option<(String, String, String)>, String> {
    let s = &b[1024..2048];
    let inc = u32at(s, 0x60);
    let ro = u32at(s, 0x64);
    let log = u32at(s, 0x18);
    if log > 6 || u16at(s, 0x5a) != 0 || u32at(s, 0x4c) > 1 {
        return Ok(None);
    }
    let block = 1024u64 << log;
    let blocks = u32at(s, 4)
        | if inc & 0x80 != 0 {
            u32at(s, 0x150) << 32
        } else {
            0
        };
    let Some(size) = blocks.checked_mul(block) else {
        return Ok(None);
    };
    let first = u32at(s, 0x14);
    let bpg = u32at(s, 0x20);
    let ipg = u32at(s, 0x28);
    let inodes = u32at(s, 0);
    let inode = if u32at(s, 0x4c) == 0 {
        128
    } else {
        u16at(s, 0x58)
    };
    if !p.limit(size, sector)
        || first != u64::from(block == 1024)
        || bpg == 0
        || bpg > block * 8
        || ipg < 2
        || ipg > block * 8
        || inodes < 2
        || inode < 128
        || inode > block
        || !inode.is_power_of_two()
        || blocks <= first
        || (blocks - first)
            .div_ceil(bpg)
            .checked_mul(ipg)
            .is_none_or(|n| inodes > n)
    {
        return Ok(None);
    }
    if ro & 0x400 != 0 && (s[0x175] != 1 || crc32c(&s[..0x3fc]) != u32at(s, 0x3fc) as u32) {
        return Ok(None);
    }
    // Confirm the first group's inode table is inside this filesystem.
    let descriptor_size = if inc & 0x80 != 0 { u16at(s, 0xfe) } else { 32 };
    if ![32, 64].contains(&descriptor_size) {
        return Ok(None);
    }
    let at = (first + 1) * block;
    if at + descriptor_size > size {
        return Ok(None);
    }
    let d = p.read(at, descriptor_size as usize)?;
    let table = u32at(&d, 8)
        | if descriptor_size == 64 {
            u32at(&d, 40) << 32
        } else {
            0
        };
    if table <= first
        || table >= blocks
        || table
            .checked_mul(block)
            .and_then(|x| x.checked_add(ipg * inode))
            .is_none_or(|end| end > size)
    {
        return Ok(None);
    }
    let name = if inc & (0x40 | 0x80) != 0 || ro & 0x400 != 0 {
        "Ext4"
    } else if u32at(s, 0x5c) & 4 != 0 {
        "Ext3"
    } else {
        "Ext2"
    };
    Ok(Some((
        name.into(),
        "主超级块几何、组号与首组 inode 表范围".into(),
        "识别不代表可读取全部特性；脏卷、日志和未知特性由目录引擎另行拒绝。".into(),
    )))
}
fn hint(b: &[u8]) -> bool {
    (b.len() >= 512
        && b[510..512] == [0x55, 0xaa]
        && (&b[3..11] == b"NTFS    "
            || &b[3..11] == b"EXFAT   "
            || matches!(u16at(b, 11), 512 | 4096)))
        || (b.len() >= 2048 && u16at(b, 1080) == 0xef53)
}
pub fn probe<R: Read + Seek>(
    reader: &mut R,
    range: Range,
    at: u64,
    cancel: &AtomicBool,
) -> Result<Option<Candidate>, String> {
    if !matches!(range.sector, 512 | 4096)
        || range.size == 0
        || range
            .offset
            .checked_add(range.size)
            .is_none_or(|e| e > i64::MAX as u64)
    {
        return Err("候选范围无效".into());
    }
    if at < range.offset
        || at % range.sector != 0
        || at
            .checked_add(512)
            .is_none_or(|end| end > range.offset + range.size)
    {
        return Err("候选位置超出范围".into());
    }
    let mut p = Probe {
        reader,
        start: at,
        size: range.offset + range.size - at,
        hash: Sha256::new(),
        cancel,
    };
    let b = p.read(0, p.size.min(2048) as usize)?;
    let found = if b.len() >= 512 && b[510..512] == [0x55, 0xaa] {
        if &b[3..11] == b"NTFS    " {
            ntfs(&mut p, &b, range.sector)?
        } else if &b[3..11] == b"EXFAT   " {
            exfat(&mut p, &b, range.sector)?
        } else if b.len() >= 2048 && u16at(&b, 1080) == 0xef53 {
            ext(&mut p, &b, range.sector)?
        } else {
            fat(&mut p, &b, range.sector)?
        }
    } else if b.len() >= 2048 && u16at(&b, 1080) == 0xef53 {
        ext(&mut p, &b, range.sector)?
    } else {
        None
    };
    Ok(found.map(|(filesystem, evidence, warning)| Candidate {
        id: uuid::Uuid::new_v4().to_string(),
        filesystem,
        offset: at,
        size: p.size,
        evidence,
        warning,
        fingerprint: format!("{:x}", p.hash.finalize()),
    }))
}
pub fn revalidate<R: Read + Seek>(
    reader: &mut R,
    range: Range,
    candidate: &Candidate,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let current =
        probe(reader, range, candidate.offset, cancel)?.ok_or("候选文件系统证据已失效")?;
    if current.filesystem != candidate.filesystem
        || current.size != candidate.size
        || current.fingerprint != candidate.fingerprint
    {
        return Err("候选文件系统证据已改变，请重新搜索。".into());
    }
    Ok(())
}
// Bound the raw reader before adding any cache. A whole-disk cache here would
// prefetch bytes outside an unaligned candidate despite higher-level bounds.
pub fn candidate_reader<R: Read + Seek>(
    reader: R,
    source_size: u64,
    sector: u64,
    range: Range,
    candidate: &Candidate,
    cancel: &AtomicBool,
) -> Result<crate::data_io::Region<R>, String> {
    if sector != range.sector
        || !matches!(sector, 512 | 4096)
        || candidate.offset % sector != 0
        || candidate.size % sector != 0
        || candidate.offset < range.offset
        || range
            .offset
            .checked_add(range.size)
            .is_none_or(|end| end > source_size)
        || candidate
            .offset
            .checked_add(candidate.size)
            .is_none_or(|end| end > range.offset + range.size)
    {
        return Err("候选范围或设备几何已变化".into());
    }
    let mut reader = crate::data_io::Region::new(reader, candidate.offset, candidate.size)
        .map_err(|e| e.to_string())?;
    let local = Candidate {
        offset: 0,
        ..candidate.clone()
    };
    revalidate(
        &mut reader,
        Range {
            offset: 0,
            size: candidate.size,
            sector,
        },
        &local,
        cancel,
    )?;
    reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    Ok(reader)
}
pub fn scan<R: Read + Seek>(
    reader: &mut R,
    range: Range,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<Candidate>) -> Result<(), String>,
) -> Result<(), String> {
    if !matches!(range.sector, 512 | 4096)
        || range.size == 0
        || range.offset % range.sector != 0
        || range.size % range.sector != 0
        || range
            .offset
            .checked_add(range.size)
            .is_none_or(|e| e > i64::MAX as u64)
    {
        return Err("搜索范围无效".into());
    }
    let mut done = 0u64;
    let mut probes = 0usize;
    let mut found = 0usize;
    progress(0, None)?;
    while done < range.size {
        canceled(cancel)?;
        let n = (range.size - done).min(BLOCK as u64) as usize;
        let read = (range.size - done).min((BLOCK + 2048) as u64) as usize;
        let mut bytes = vec![0; read];
        reader
            .seek(SeekFrom::Start(range.offset + done))
            .map_err(|e| e.to_string())?;
        reader
            .read_exact(&mut bytes)
            .map_err(|e| format!("搜索读取失败，已检查 {done} 字节：{e}"))?;
        for i in (0..n).step_by(range.sector as usize) {
            if !hint(&bytes[i..]) {
                continue;
            }
            canceled(cancel)?;
            probes += 1;
            if probes > MAX_PROBES {
                return Err("引导特征过多，达到证据检查上限；请缩小搜索范围。".into());
            }
            if let Some(candidate) = probe(reader, range, range.offset + done + i as u64, cancel)? {
                found += 1;
                progress(done + i as u64, Some(candidate))?;
                if found == MAX_CANDIDATES {
                    return Err("已达到 256 个候选上限；结果不完整，请缩小范围。".into());
                }
            }
        }
        done += n as u64;
        progress(done, None)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "lost_partitions_tests.rs"]
mod tests;

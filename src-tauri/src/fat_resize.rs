//! Plan FAT end-boundary changes over an already bounded volume reader.
//! This module never opens devices or changes partition tables.
use super::{allocation::ClassicTable, at, e, u16at, u32at, Fs};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};

#[path = "fat_clone.rs"]
pub(crate) mod clone;
#[path = "fat_file_clone.rs"]
pub(crate) mod file_clone;
#[path = "fat_exfat_resize.rs"]
mod exfat;
pub use exfat::prepare_exfat;

const MAX_DIRECTORY_BYTES: u64 = 128 * 1024 * 1024;

pub(super) struct CheckedBootFat32 {
    pub(super) fs: Fs,
    pub(super) boot: Vec<u8>,
    pub(super) table: ClassicTable,
    pub(super) digest: [u8; 32],
}

/// Validate exclusive live-cluster ownership and both copies of the metadata
/// used by offline EFI file repairs.
pub(super) fn checked_boot_fat32<R: Read + Seek>(
    r: &mut R,
    sector: u64,
) -> Result<CheckedBootFat32, String> {
    if !matches!(sector, 512 | 4096) {
        return Err("Invalid EFI FAT sector geometry".into());
    }
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let fs = Fs::load(r)?;
    let boot = at(r, 0, sector as usize)?;
    let backup = u64::from(u16at(&boot, 50));
    if fs.bits != 32 || fs.copies != 2 || !fs.mirror || fs.cluster > 32768
        || u64::from(u16at(&boot, 11)) != sector || u16at(&boot, 40) != 0
        || u16at(&boot, 19) != 0 || u64::from(u32at(&boot, 32)) * sector != size
        || backup == 0 || backup >= u64::from(u16at(&boot, 14))
        || at(r, backup * sector, boot.len())? != boot
    {
        return Err("EFI repair requires a checked FAT32 partition with matching boot sectors and FATs".into());
    }
    let table = ClassicTable::read(r, &fs, &boot, fs.count)?;
    let relocation = Relocation { pairs: Vec::new(), sector: sector as usize, heap_shift: 0 };
    let tree = check_classic_tree(r, &fs, &table, &relocation)?;
    let mut hash = Sha256::new();
    hash.update(&boot);
    hash.update(table.bytes());
    hash.update(tree.digest);
    Ok(CheckedBootFat32 {
        fs,
        boot,
        table,
        digest: hash.finalize().into(),
    })
}

pub(crate) fn check_boot_fat32<R: Read + Seek>(r: &mut R, sector: u64) -> Result<[u8; 32], String> {
    Ok(checked_boot_fat32(r, sector)?.digest)
}

#[derive(Debug)]
struct SectorEdit {
    offset: u64,
    source_offset: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl SectorEdit {
    fn at(offset: u64, before: Vec<u8>, after: Vec<u8>) -> Self {
        Self {
            offset,
            source_offset: offset,
            before,
            after,
        }
    }

    fn initialize(offset: u64, after: Vec<u8>) -> Self {
        Self::at(offset, Vec::new(), after)
    }
}

#[derive(Debug)]
struct ClusterCopy {
    from: u64,
    to: u64,
}

#[derive(Debug)]
enum ClusterCopies {
    Relocated(Vec<ClusterCopy>),
    Shifted {
        heap: u64,
        delta: u64,
        allocated: Vec<u8>,
        used: usize,
    },
}

#[derive(Debug)]
pub struct Plan {
    edits: Vec<SectorEdit>,
    tree_digest: [u8; 32],
    copies: ClusterCopies,
    cluster_size: usize,
    pub filesystem: String,
    pub old_size: u64,
    pub new_size: u64,
    pub old_clusters: u32,
    pub new_clusters: u32,
}

impl Plan {
    pub fn moved_clusters(&self) -> usize {
        match &self.copies {
            ClusterCopies::Relocated(copies) => copies.len(),
            ClusterCopies::Shifted { used, .. } => *used,
        }
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.old_size.to_le_bytes());
        hash.update(self.new_size.to_le_bytes());
        hash.update(self.old_clusters.to_le_bytes());
        hash.update(self.new_clusters.to_le_bytes());
        hash.update(self.filesystem.as_bytes());
        hash.update(self.tree_digest);
        hash.update((self.cluster_size as u64).to_le_bytes());
        match &self.copies {
            ClusterCopies::Relocated(copies) => {
                hash.update([0]);
                hash.update((copies.len() as u64).to_le_bytes());
                for copy in copies {
                    hash.update(copy.from.to_le_bytes());
                    hash.update(copy.to.to_le_bytes());
                }
            }
            ClusterCopies::Shifted {
                heap,
                delta,
                allocated,
                ..
            } => {
                hash.update([1]);
                hash.update(heap.to_le_bytes());
                hash.update(delta.to_le_bytes());
                hash.update(Sha256::digest(allocated));
            }
        }
        for edit in &self.edits {
            hash.update(edit.offset.to_le_bytes());
            hash.update(edit.source_offset.to_le_bytes());
            hash.update(Sha256::digest(&edit.before));
            hash.update(Sha256::digest(&edit.after));
        }
        hash.finalize().into()
    }

    /// Recheck each sector before writing any. The caller must retain the
    /// volume locks, verify the filesystem proof and flush before verification.
    pub fn apply<R: Read + Write + Seek>(&self, r: &mut R) -> Result<(), String> {
        if r.seek(SeekFrom::End(0)).map_err(e)? != self.old_size.max(self.new_size) {
            return Err("卷写入范围与计划的调整容量范围不符。".into());
        }
        self.compare(r, false)?;
        match &self.copies {
            ClusterCopies::Relocated(copies) => {
                for copy in copies {
                    copy_clusters(r, copy.from, copy.to, self.cluster_size)?;
                }
            }
            ClusterCopies::Shifted {
                heap,
                delta,
                allocated,
                ..
            } => {
                let mut end = self.old_clusters as usize;
                let clusters_per_read = (1024 * 1024 / self.cluster_size).max(1);
                // Move from high to low addresses. Each batch is buffered
                // before writing because its source and target may overlap.
                while end > 0 {
                    let limit = end.saturating_sub(clusters_per_read);
                    let mut start = end;
                    while start > limit && bitmap_test(allocated, start - 1) {
                        start -= 1;
                    }
                    if start == end {
                        end -= 1;
                        continue;
                    }
                    let from = heap + start as u64 * self.cluster_size as u64;
                    copy_clusters(r, from, from + delta, (end - start) * self.cluster_size)?;
                    end = start;
                }
            }
        }
        for edit in &self.edits {
            r.seek(SeekFrom::Start(edit.offset)).map_err(e)?;
            r.write_all(&edit.after).map_err(e)?;
        }
        r.flush().map_err(e)
    }

    pub fn verify<R: Read + Seek>(&self, r: &mut R) -> Result<(), String> {
        if r.seek(SeekFrom::End(0)).map_err(e)? != self.new_size {
            return Err("后验分区范围与计划容量不符。".into());
        }
        self.compare(r, true)?;
        let fs = Fs::load(r)?;
        let filesystem = if fs.bits == 64 {
            "exFAT".into()
        } else {
            format!("FAT{}", fs.bits)
        };
        if fs.count != self.new_clusters || filesystem != self.filesystem {
            return Err("FAT 类型或簇数核对失败。".into());
        }
        let digest = if fs.bits == 64 {
            exfat::check(r, &fs, fs.count, &Relocation::default(), None)?.digest
        } else {
            let boot = at(r, 0, 512)?;
            let table = ClassicTable::read(r, &fs, &boot, fs.count)?;
            check_classic_tree(r, &fs, &table, &Relocation::default())?.digest
        };
        if digest != self.tree_digest {
            return Err("调整尺寸后 FAT 活动目录或文件簇链发生变化。".into());
        }
        Ok(())
    }

    fn compare<R: Read + Seek>(&self, r: &mut R, after: bool) -> Result<(), String> {
        for edit in &self.edits {
            let (offset, expected) = if after {
                (edit.offset, &edit.after)
            } else {
                (edit.source_offset, &edit.before)
            };
            if expected.is_empty() {
                continue;
            }
            let actual = at(r, offset, expected.len())?;
            if Sha256::digest(&actual) != Sha256::digest(expected) {
                return Err(format!("FAT 元数据核对失败，卷内字节偏移 {}。", offset));
            }
        }
        Ok(())
    }
}

fn copy_clusters<R: Read + Write + Seek>(
    r: &mut R,
    from: u64,
    to: u64,
    size: usize,
) -> Result<(), String> {
    let bytes = at(r, from, size)?;
    r.seek(SeekFrom::Start(to)).map_err(e)?;
    r.write_all(&bytes).map_err(e)?;
    let readback = at(r, to, size)?;
    if Sha256::digest(&bytes) != Sha256::digest(&readback) {
        return Err(format!(
            "搬移簇的 SHA-256 读回核对失败，卷内字节偏移 {to}。"
        ));
    }
    Ok(())
}

fn bitmap_test(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (1 << (index % 8)) != 0
}

fn least_fat_sectors(mut low: u64, mut high: u64, sufficient: impl Fn(u64) -> bool) -> u64 {
    while low < high {
        let middle = low + (high - low) / 2;
        if sufficient(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

fn classic_limit(bits: u8) -> (u32, u32) {
    match bits {
        12 => (1, 0x0fed),
        16 => (4085, 0xffed),
        _ => (65525, 0x0fffffed),
    }
}

/// Grow the mirrored FATs when necessary, retaining cluster numbering. A
/// shrink can move allocated clusters to free slots below the new boundary.
pub fn prepare_classic<R: Read + Seek>(
    r: &mut R,
    sector: u64,
    new_size: u64,
) -> Result<Plan, String> {
    if !matches!(sector, 512 | 4096) || new_size % sector != 0 {
        return Err("FAT 调整容量要求 512 或 4096 字节扇区及对齐的容量。".into());
    }
    let old_size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let mut boot = at(r, 0, sector as usize)?;
    if &boot[3..11] == b"EXFAT   " {
        return Err("需要 FAT12/16/32 文件系统。".into());
    }
    let fs = Fs::load(r)?;
    if !matches!(fs.bits, 12 | 16 | 32)
        || u64::from(u16at(&boot, 11)) != sector
        || fs.cluster > 32768
        || fs.copies != 2
        || (fs.bits == 32 && u16at(&boot, 40) != 0)
    {
        return Err("需要扇区一致、两份镜像 FAT 及常规几何布局的 FAT12/16/32 卷。".into());
    }
    let total16 = u16at(&boot, 19);
    let total32 = u32at(&boot, 32);
    let declared_sectors = match (total16, total32) {
        (0, n) => u64::from(n),
        (n, 0) if fs.bits != 32 => u64::from(n),
        _ => return Err("FAT 引导扇区中的总扇区数字段不一致。".into()),
    };
    if declared_sectors * sector != old_size || new_size == old_size {
        return Err("FAT 文件系统须占满原分区，且新容量必须与原容量不同。".into());
    }
    let new_total = u32::try_from(new_size / sector).map_err(|_| "FAT 总扇区数溢出。")?;
    let initial_count = new_size
        .checked_sub(fs.heap)
        .map(|bytes| bytes / fs.cluster)
        .ok_or("新容量不足以保存 FAT 元数据或簇数溢出。")?;
    let fat_sectors = least_fat_sectors(
        fs.fat_len / sector,
        (((initial_count + 2) * u64::from(fs.bits)).div_ceil(sector * 8)).max(fs.fat_len / sector),
        |sectors| {
            let end = fs.heap + 2 * (sectors * sector - fs.fat_len);
            new_size.checked_sub(end).is_some_and(|data| {
                (data / fs.cluster + 2) * u64::from(fs.bits) <= sectors * sector * 8
            })
        },
    );
    let fat_len = fat_sectors * sector;
    let heap_shift = 2 * (fat_len - fs.fat_len);
    let new_count = new_size
        .checked_sub(fs.heap + heap_shift)
        .and_then(|bytes| u32::try_from(bytes / fs.cluster).ok())
        .ok_or("新容量不足以保存 FAT 元数据或簇数溢出。")?;
    let (minimum, maximum) = classic_limit(fs.bits);
    if !(minimum..=maximum).contains(&fs.count) || !(minimum..=maximum).contains(&new_count) {
        return Err("新旧簇数必须在同一 FAT 类型的有效范围内。".into());
    }
    let table = if heap_shift == 0 {
        let table = ClassicTable::read(r, &fs, &boot, fs.count.max(new_count))?;
        for cluster in fs.count + 2..new_count + 2 {
            if table.entry(cluster) != 0 {
                return Err(format!("扩容范围包含未清空的 FAT 项 {cluster}。"));
            }
        }
        table
    } else {
        if new_count < fs.count || fat_len > 64 * 1024 * 1024 {
            return Err("扩展后的 FAT 无法保留原有簇编号或超过 64 MiB 分析预算。".into());
        }
        ClassicTable::read(r, &fs, &boot, fs.count)?
    };
    let mut relocation = Relocation::new(&fs, new_count, sector as usize, |c| table.entry(c) != 0)?;
    relocation.heap_shift = heap_shift;
    let checked = check_classic_tree(r, &fs, &table, &relocation)?;
    let original = boot.clone();
    if fs.bits != 32 && u16::try_from(new_total).is_ok() {
        boot[19..21].copy_from_slice(&(new_total as u16).to_le_bytes());
        boot[32..36].fill(0);
    } else {
        boot[19..21].fill(0);
        boot[32..36].copy_from_slice(&new_total.to_le_bytes());
    }
    let mut edits = Vec::new();
    if heap_shift == 0 {
        relocation.update_classic_tables(r, &fs, &table, &mut edits)?;
    } else {
        if fs.bits == 32 {
            let sectors = u32::try_from(fat_sectors).map_err(|_| "FAT32 表扇区数越界。")?;
            boot[36..40].copy_from_slice(&sectors.to_le_bytes());
        } else {
            let sectors = u16::try_from(fat_sectors).map_err(|_| "FAT12/16 表扇区数越界。")?;
            boot[22..24].copy_from_slice(&sectors.to_le_bytes());
        }
        expand_classic_tables(r, &fs, &table, sector, fat_len, &mut edits)?;
    }
    edits.extend(checked.directory_edits);
    if fs.bits == 32 {
        boot[44..48].copy_from_slice(&relocation.cluster(fs.root).to_le_bytes());
        let reserved = u64::from(u16at(&original, 14));
        let info = u64::from(u16at(&original, 48));
        let backup = u64::from(u16at(&original, 50));
        if info == 0 || backup <= info || backup + info >= reserved {
            return Err("FAT32 主备引导与 FSInfo 扇区的位置不可验证。".into());
        }
        let backup_boot = at(r, backup * sector, sector as usize)?;
        if backup_boot != original {
            return Err("FAT32 主备引导扇区不一致。".into());
        }
        for offset in [(backup + info) * sector, info * sector] {
            let before = at(r, offset, sector as usize)?;
            if u32at(&before, 0) != 0x41615252
                || u32at(&before, 484) != 0x61417272
                || u32at(&before, 508) != 0xaa550000
            {
                return Err("FAT32 FSInfo 签名无效。".into());
            }
            let mut after = before.clone();
            after[488..492].copy_from_slice(&(new_count - checked.used).to_le_bytes());
            after[492..496].fill(0xff); // next-free hint is unknown
            edits.push(SectorEdit::at(offset, before, after));
        }
        edits.push(SectorEdit::at(backup * sector, backup_boot, boot.clone()));
    }
    edits.push(SectorEdit::at(0, original, boot));
    Ok(Plan {
        edits,
        tree_digest: checked.digest,
        copies: relocation.copies(&fs, &checked.claimed, checked.used)?,
        cluster_size: fs.cluster as usize,
        filesystem: format!("FAT{}", fs.bits),
        old_size,
        new_size,
        old_clusters: fs.count,
        new_clusters: new_count,
    })
}

fn expand_classic_tables<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    table: &ClassicTable,
    sector: u64,
    fat_len: u64,
    edits: &mut Vec<SectorEdit>,
) -> Result<(), String> {
    let mut expanded = table.bytes().to_vec();
    let original_bits = (u64::from(fs.count) + 2) * u64::from(fs.bits);
    if original_bits % 8 == 4 {
        expanded[original_bits as usize / 8] &= 0x0f;
    }
    let original_aligned = (expanded.len() as u64).div_ceil(sector) * sector;
    expanded.resize(fat_len as usize, 0);
    for copy in [1, 0] {
        let source_offset = fs.fat + copy * fs.fat_len;
        let before = at(r, source_offset, original_aligned as usize)?;
        if !before.starts_with(table.bytes()) {
            return Err("FAT 镜像在扩容准备期间变化。".into());
        }
        edits.push(SectorEdit {
            offset: fs.fat + copy * fat_len,
            source_offset,
            before,
            after: expanded.clone(),
        });
    }
    Ok(())
}

#[derive(Default)]
struct Relocation {
    pairs: Vec<(u32, u32)>,
    sector: usize,
    heap_shift: u64,
}

impl Relocation {
    fn new(
        fs: &Fs,
        count: u32,
        sector: usize,
        allocated: impl Fn(u32) -> bool,
    ) -> Result<Self, String> {
        let mut free = (2..count + 2).filter(|&c| !allocated(c));
        let mut pairs = Vec::new();
        for cluster in (count + 2..fs.count + 2).filter(|&c| allocated(c)) {
            let target = free
                .next()
                .ok_or("保留范围内的空闲簇不足，无法缩小 FAT 卷。")?;
            pairs.push((cluster, target));
        }
        Ok(Self {
            pairs,
            sector,
            heap_shift: 0,
        })
    }

    fn cluster(&self, cluster: u32) -> u32 {
        self.pairs
            .binary_search_by_key(&cluster, |&(from, _)| from)
            .map_or(cluster, |index| self.pairs[index].1)
    }

    fn copies(&self, fs: &Fs, allocated: &[u8], used: u32) -> Result<ClusterCopies, String> {
        if self.heap_shift != 0 {
            return Ok(ClusterCopies::Shifted {
                heap: fs.heap,
                delta: self.heap_shift,
                allocated: allocated.to_vec(),
                used: used as usize,
            });
        }
        let copies = self
            .pairs
            .iter()
            .map(|&(from, to)| {
                Ok(ClusterCopy {
                    from: fs.offset(from)?,
                    to: fs.offset(to)?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(ClusterCopies::Relocated(copies))
    }

    fn update_classic_tables<R: Read + Seek>(
        &self,
        r: &mut R,
        fs: &Fs,
        table: &ClassicTable,
        edits: &mut Vec<SectorEdit>,
    ) -> Result<(), String> {
        if self.pairs.is_empty() {
            return Ok(());
        }
        let mut updated = table.clone();
        for cluster in 2..fs.count + 2 {
            let link = table.entry(cluster);
            if link == 0 {
                continue;
            }
            let target = self.cluster(cluster);
            let next = self.cluster(link);
            if target != cluster {
                updated.set_entry(cluster, 0);
            }
            if target != cluster || next != link {
                updated.set_entry(target, next);
            }
        }
        for offset in [fs.fat + fs.fat_len, fs.fat] {
            self.patch_sectors(r, edits, offset, offset, table.bytes(), updated.bytes())?;
        }
        Ok(())
    }

    fn patch_sectors<R: Read + Seek>(
        &self,
        r: &mut R,
        edits: &mut Vec<SectorEdit>,
        source: u64,
        target: u64,
        before: &[u8],
        after: &[u8],
    ) -> Result<(), String> {
        for (i, (old, new)) in before
            .chunks(self.sector)
            .zip(after.chunks(self.sector))
            .enumerate()
        {
            if old == new {
                continue;
            }
            let delta = (i * self.sector) as u64;
            let source_offset = source + delta;
            let original = if old.len() == self.sector {
                old.to_vec()
            } else {
                let original = at(r, source_offset, self.sector)?;
                if &original[..old.len()] != old {
                    return Err("FAT 扇区内容在准备期间发生变化。".into());
                }
                original
            };
            let mut updated = original.clone();
            updated[..new.len()].copy_from_slice(new);
            edits.push(SectorEdit {
                offset: target + delta,
                source_offset,
                before: original,
                after: updated,
            });
        }
        Ok(())
    }
}

struct ClassicTree<'a> {
    fs: &'a Fs,
    table: &'a ClassicTable,
    relocation: &'a Relocation,
    claimed: Vec<u8>,
    used: u32,
    digest: Sha256,
}

impl ClassicTree<'_> {
    fn claim(&mut self, cluster: u32) -> Result<u32, String> {
        if !(2..self.fs.count + 2).contains(&cluster) {
            return Err(format!("活动目录或文件引用越界簇 {cluster}。"));
        }
        let index = cluster as usize - 2;
        let bit = 1 << (index % 8);
        let byte = &mut self.claimed[index / 8];
        if *byte & bit != 0 {
            return Err("FAT 簇链循环或多个文件引用同一簇。".into());
        }
        *byte |= bit;
        self.used += 1;
        let next = self.table.entry(cluster);
        if !self.fs.end(next) && !(2..self.fs.count + 2).contains(&next) {
            return Err("FAT 簇链引用空闲、保留或越界簇。".into());
        }
        self.digest
            .update(self.relocation.cluster(cluster).to_le_bytes());
        self.digest
            .update(self.relocation.cluster(next).to_le_bytes());
        Ok(next)
    }

    fn claim_file(&mut self, mut cluster: u32, size: u64) -> Result<(), String> {
        let needed = size.div_ceil(self.fs.cluster);
        if needed == 0 && cluster != 0 {
            return Err("空文件包含非空 FAT 簇链。".into());
        }
        for i in 1..=needed {
            let next = self.claim(cluster)?;
            if self.fs.end(next) != (i == needed) {
                return Err("FAT 文件长度与簇链不一致。".into());
            }
            cluster = next;
        }
        Ok(())
    }
}

struct TreeCheck {
    used: u32,
    digest: [u8; 32],
    directory_edits: Vec<SectorEdit>,
    claimed: Vec<u8>,
}

fn check_classic_tree<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    table: &ClassicTable,
    relocation: &Relocation,
) -> Result<TreeCheck, String> {
    let mut tree = ClassicTree {
        fs,
        table,
        relocation,
        claimed: vec![0; (fs.count as usize).div_ceil(8)],
        used: 0,
        digest: Sha256::new(),
    };
    let mut directories = vec![(fs.root, 0, true)];
    let mut directory_edits = Vec::new();
    let mut bytes_read = 0u64;
    while let Some((first, parent, root)) = directories.pop() {
        let mut cluster = first;
        let mut end_of_entries = false;
        loop {
            let (offset, target, length, last) = if root && fs.root_fixed.is_some() {
                let (offset, length) = fs.root_fixed.ok_or("缺少 FAT 固定根目录。")?;
                (offset, offset + relocation.heap_shift, length, true)
            } else {
                let offset = fs.offset(cluster)?;
                let target = fs.offset(relocation.cluster(cluster))? + relocation.heap_shift;
                let next = tree.claim(cluster)?;
                cluster = next;
                (offset, target, fs.cluster as usize, fs.end(next))
            };
            if !end_of_entries {
                bytes_read += length as u64;
                if bytes_read > MAX_DIRECTORY_BYTES {
                    return Err("FAT 目录检查超过 128 MiB 预算。".into());
                }
                let bytes = at(r, offset, length)?;
                let mut updated = None;
                for (index, entry) in bytes.chunks_exact(32).enumerate() {
                    if entry[0] == 0 {
                        end_of_entries = true;
                        break;
                    }
                    let flags = entry[11];
                    if entry[0] == 0xe5 || flags == 0x0f {
                        continue;
                    }
                    let low = u32::from(u16at(entry, 26));
                    let high = if fs.bits == 32 {
                        u32::from(u16at(entry, 20))
                    } else {
                        0
                    };
                    let child = (high << 16) | low;
                    let length = u64::from(u32at(entry, 28));
                    if flags & 0xc0 != 0 || flags & 0x18 == 0x18 {
                        return Err("FAT 活动目录项属性无效。".into());
                    }
                    if flags & 0x08 != 0 {
                        if !root || child != 0 || length != 0 {
                            return Err("FAT 卷标目录项无效。".into());
                        }
                    } else if flags & 0x10 != 0 {
                        if length != 0 {
                            return Err("FAT 子目录包含异常长度。".into());
                        }
                        if &entry[..11] == b".          " {
                            if root || child != first {
                                return Err("FAT 当前目录引用无效。".into());
                            }
                        } else if &entry[..11] == b"..         " {
                            if root || (child != parent && !(parent == fs.root && child == 0)) {
                                return Err("FAT 父目录引用无效。".into());
                            }
                        } else {
                            if !(2..fs.count + 2).contains(&child) || directories.len() >= 65536 {
                                return Err("FAT 子目录簇无效或待核对目录过多。".into());
                            }
                            directories.push((child, first, false));
                        }
                    } else {
                        tree.claim_file(child, length)?;
                    }
                    let relocated = relocation.cluster(child);
                    if relocated != child {
                        let bytes = updated.get_or_insert_with(|| bytes.clone());
                        let start = index * 32;
                        bytes[start + 26..start + 28]
                            .copy_from_slice(&(relocated as u16).to_le_bytes());
                        if fs.bits == 32 {
                            bytes[start + 20..start + 22]
                                .copy_from_slice(&((relocated >> 16) as u16).to_le_bytes());
                        }
                    }
                }
                tree.digest.update(target.to_le_bytes());
                tree.digest.update((length as u64).to_le_bytes());
                if let Some(updated) = updated {
                    relocation.patch_sectors(
                        r,
                        &mut directory_edits,
                        offset,
                        target,
                        &bytes,
                        &updated,
                    )?;
                    tree.digest.update(&updated);
                } else {
                    tree.digest.update(&bytes);
                    if fs.root_fixed.is_some_and(|(root, _)| offset == root) && target != offset {
                        let length = length.div_ceil(relocation.sector) * relocation.sector;
                        let original = at(r, offset, length)?;
                        if !original.starts_with(&bytes) {
                            return Err("FAT 固定根目录在扩容准备期间变化。".into());
                        }
                        directory_edits.push(SectorEdit {
                            offset: target,
                            source_offset: offset,
                            before: original.clone(),
                            after: original,
                        });
                    }
                }
            }
            if last {
                break;
            }
        }
    }
    let actual = (2..fs.count + 2).filter(|c| table.entry(*c) != 0).count() as u32;
    if tree.used != actual {
        return Err("FAT 存在未被活动目录引用的分配簇，需先检查文件系统。".into());
    }
    Ok(TreeCheck {
        used: actual,
        digest: tree.digest.finalize().into(),
        directory_edits,
        claimed: tree.claimed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::fat_format::{FatType, Plan as FormatPlan};
    use std::io::Cursor;

    const MIB: u64 = 1024 * 1024;

    fn volume(kind: FatType, mib: u64, cluster: u32) -> Cursor<Vec<u8>> {
        let mut image = Cursor::new(vec![0u8; (mib * MIB) as usize]);
        FormatPlan::new(kind, mib * MIB, 512, cluster, "RESIZE", 123, 2048)
            .unwrap()
            .format(&mut image, |_, _| Ok(()))
            .unwrap();
        image
    }

    fn add_fat16_file(image: &mut Cursor<Vec<u8>>, cluster: u16, allocate: bool) -> u64 {
        let fs = Fs::load(image).unwrap();
        let root = fs.root_fixed.unwrap().0 as usize;
        let offset = fs.offset(u32::from(cluster)).unwrap();
        let file = &mut image.get_mut()[root + 32..root + 64];
        file[..11].copy_from_slice(b"PAYLOAD BIN");
        file[11] = 0x20;
        file[26..28].copy_from_slice(&cluster.to_le_bytes());
        file[28..32].copy_from_slice(&650u32.to_le_bytes());
        if allocate {
            for table in [fs.fat, fs.fat + fs.fat_len] {
                let entry = table as usize + usize::from(cluster) * 2;
                image.get_mut()[entry..entry + 2].copy_from_slice(&0xffffu16.to_le_bytes());
            }
        }
        image.get_mut()[offset as usize..offset as usize + 650].fill(0x7b);
        offset
    }

    #[test]
    fn fat12_fat16_and_fat32_shrink_and_grow_with_same_heap() {
        for (kind, old_mib, new_mib, cluster) in [
            (FatType::Fat12, 2, 1, 512),
            (FatType::Fat16, 16, 12, 2048),
            (FatType::Fat32, 40, 36, 512),
        ] {
            let mut image = volume(kind, old_mib, cluster);
            let offset = (kind == FatType::Fat16).then(|| add_fat16_file(&mut image, 2, true));
            let heap = Fs::load(&mut image).unwrap().heap;
            let shrink = prepare_classic(&mut image, 512, new_mib * MIB).unwrap();
            assert!(shrink.new_clusters < shrink.old_clusters);
            shrink.apply(&mut image).unwrap();
            image.get_mut().truncate((new_mib * MIB) as usize);
            shrink.verify(&mut image).unwrap();
            assert_eq!(Fs::load(&mut image).unwrap().heap, heap);
            if kind == FatType::Fat32 {
                // Root directory occupies one cluster. Both FSInfo copies
                // report the new free count after each operation.
                assert_eq!(u32at(image.get_ref(), 512 + 488), shrink.new_clusters - 1);
                assert_eq!(
                    u32at(image.get_ref(), 7 * 512 + 488),
                    shrink.new_clusters - 1
                );
            }
            let grow = prepare_classic(&mut image, 512, old_mib * MIB).unwrap();
            image.get_mut().resize((old_mib * MIB) as usize, 0);
            grow.apply(&mut image).unwrap();
            grow.verify(&mut image).unwrap();
            assert_eq!(Fs::load(&mut image).unwrap().heap, heap);
            if let Some(offset) = offset {
                assert_eq!(at(&mut image, offset, 650).unwrap(), vec![0x7b; 650]);
            }
        }
    }

    #[test]
    fn grows_mirrored_tables_and_preserves_file_contents_and_cluster_numbers() {
        for (kind, old_mib, new_mib, sector, cluster_size) in [
            (FatType::Fat12, 1, 2, 512, 512),
            (FatType::Fat16, 16, 24, 512, 2048),
            (FatType::Fat32, 40, 46, 512, 512),
            (FatType::Fat16, 24, 28, 4096, 4096),
        ] {
            let mut image = Cursor::new(vec![0; (old_mib * MIB) as usize]);
            FormatPlan::new(
                kind,
                old_mib * MIB,
                sector,
                cluster_size,
                "RESIZE",
                123,
                2048,
            )
            .unwrap()
            .format(&mut image, |_, _| Ok(()))
            .unwrap();
            let fs = Fs::load(&mut image).unwrap();
            let root = fs
                .root_fixed
                .map_or_else(|| fs.offset(fs.root).unwrap(), |(offset, _)| offset)
                as usize;
            let boot = at(&mut image, 0, 512).unwrap();
            let mut table = ClassicTable::read(&mut image, &fs, &boot, fs.count).unwrap();
            let head = 3;
            let tail = head + 1;
            table.set_entry(head, tail);
            table.set_entry(tail, (1 << fs.bits.min(28)) - 1);
            for offset in [fs.fat, fs.fat + fs.fat_len] {
                let start = offset as usize;
                image.get_mut()[start..start + table.bytes().len()].copy_from_slice(table.bytes());
            }
            image.get_mut()[root + 32..root + 64].copy_from_slice(&directory_entry(
                b"PAYLOAD BIN",
                0x20,
                head,
                cluster_size + 7,
            ));
            let start = fs.offset(head).unwrap() as usize;
            let length = cluster_size as usize + 7;
            let expected = (0..length)
                .map(|i| (i * 47 % 251) as u8)
                .collect::<Vec<_>>();
            image.get_mut()[start..start + length].copy_from_slice(&expected);
            let plan = prepare_classic(&mut image, u64::from(sector), new_mib * MIB).unwrap();
            assert!(plan.new_clusters > plan.old_clusters);
            assert_eq!(plan.moved_clusters(), if fs.bits == 32 { 3 } else { 2 });
            super::clone::assert_materializes(image.get_ref(), u64::from(sector), new_mib * MIB);
            image.get_mut().resize((new_mib * MIB) as usize, 0xec);
            plan.apply(&mut image).unwrap();
            plan.verify(&mut image).unwrap();
            let grown = Fs::load(&mut image).unwrap();
            assert!(grown.heap > fs.heap && grown.fat_len > fs.fat_len);
            assert_eq!(grown.bits, fs.bits);
            assert_eq!(grown.cluster, fs.cluster);
            assert_eq!(grown.fat(&mut image, head).unwrap(), tail);
            assert_eq!(grown.fat(&mut image, fs.count + 2).unwrap(), 0);
            assert_eq!(
                at(&mut image, grown.offset(head).unwrap(), length).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn relocates_tail_payload_and_preserves_it_after_growth() {
        let mut tail = volume(FatType::Fat16, 16, 2048);
        let last = Fs::load(&mut tail).unwrap().count + 1;
        add_fat16_file(&mut tail, last as u16, true);
        let plan = prepare_classic(&mut tail, 512, 12 * MIB).unwrap();
        assert_eq!(plan.moved_clusters(), 1);
        plan.apply(&mut tail).unwrap();
        tail.get_mut().truncate(12 * MIB as usize);
        plan.verify(&mut tail).unwrap();
        let fs = Fs::load(&mut tail).unwrap();
        let entry = at(&mut tail, fs.root_fixed.unwrap().0 + 32, 32).unwrap();
        assert_eq!(u16at(&entry, 26), 2);
        assert_eq!(
            at(&mut tail, fs.offset(2).unwrap(), 650).unwrap(),
            vec![0x7b; 650]
        );
        let grow = prepare_classic(&mut tail, 512, 16 * MIB).unwrap();
        tail.get_mut().resize(16 * MIB as usize, 0);
        grow.apply(&mut tail).unwrap();
        grow.verify(&mut tail).unwrap();
    }

    fn directory_entry(name: &[u8; 11], flags: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut entry = [0; 32];
        entry[..11].copy_from_slice(name);
        entry[11] = flags;
        entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        entry[28..].copy_from_slice(&size.to_le_bytes());
        entry
    }

    #[test]
    fn moves_directories_and_updates_links_in_each_classic_fat_type() {
        for (kind, old_mib, new_mib, cluster_size) in [
            (FatType::Fat12, 2, 1, 512),
            (FatType::Fat16, 16, 12, 2048),
            (FatType::Fat32, 40, 36, 512),
        ] {
            let mut image = volume(kind, old_mib, cluster_size);
            let fs = Fs::load(&mut image).unwrap();
            let tail = fs.count - 2;
            let dir = tail + 1;
            let file_tail = tail + 2;
            let file_head = 2;
            let mut boot = at(&mut image, 0, 512).unwrap();
            let mut table = ClassicTable::read(&mut image, &fs, &boot, fs.count).unwrap();
            let end = (1u32 << fs.bits.min(28)) - 1;
            let root = if let Some((offset, _)) = fs.root_fixed {
                offset
            } else {
                boot[44..48].copy_from_slice(&tail.to_le_bytes());
                image.get_mut()[..512].copy_from_slice(&boot);
                image.get_mut()[6 * 512..7 * 512].copy_from_slice(&boot);
                table.set_entry(tail, end);
                fs.offset(tail).unwrap()
            } as usize;
            table.set_entry(dir, end);
            table.set_entry(file_head, file_tail);
            table.set_entry(file_tail, end);
            for offset in [fs.fat, fs.fat + fs.fat_len] {
                let at = offset as usize;
                image.get_mut()[at..at + table.bytes().len()].copy_from_slice(table.bytes());
            }
            image.get_mut()[root..root + 32].copy_from_slice(&directory_entry(
                b"DIR        ",
                0x10,
                dir,
                0,
            ));
            let offset = fs.offset(dir).unwrap() as usize;
            for (i, entry) in [
                directory_entry(b".          ", 0x10, dir, 0),
                directory_entry(
                    b"..         ",
                    0x10,
                    if fs.bits == 32 { tail } else { 0 },
                    0,
                ),
                directory_entry(b"PAYLOAD BIN", 0x20, file_head, cluster_size + 7),
            ]
            .into_iter()
            .enumerate()
            {
                image.get_mut()[offset + i * 32..offset + (i + 1) * 32].copy_from_slice(&entry);
            }
            let data = fs.offset(file_tail).unwrap();
            image.get_mut()[data as usize..data as usize + cluster_size as usize].fill(0xaa);
            let plan = prepare_classic(&mut image, 512, new_mib * MIB).unwrap();
            let expected_moves = if fs.bits == 32 { 3 } else { 2 };
            assert_eq!(plan.moved_clusters(), expected_moves);
            super::clone::assert_materializes(image.get_ref(), 512, new_mib * MIB);
            plan.apply(&mut image).unwrap();
            image.get_mut().truncate((new_mib * MIB) as usize);
            plan.verify(&mut image).unwrap();
            let fs = Fs::load(&mut image).unwrap();
            let boot = at(&mut image, 0, 512).unwrap();
            let table = ClassicTable::read(&mut image, &fs, &boot, fs.count).unwrap();
            let file_tail = table.entry(file_head);
            assert!(fs.end(table.entry(file_tail)));
            assert_eq!(
                at(&mut image, fs.offset(file_tail).unwrap(), 7).unwrap(),
                vec![0xaa; 7]
            );
        }
    }

    #[test]
    fn refuses_dangling_directory_and_stale_boot_sector() {
        let mut dangling = volume(FatType::Fat16, 16, 2048);
        add_fat16_file(&mut dangling, 2, false);
        assert!(prepare_classic(&mut dangling, 512, 12 * MIB).is_err());

        let mut changed = volume(FatType::Fat32, 40, 512);
        let plan = prepare_classic(&mut changed, 512, 36 * MIB).unwrap();
        changed.get_mut()[7 * 512 + 488] ^= 1;
        let before = changed.get_ref().clone();
        assert!(plan.apply(&mut changed).is_err());
        assert_eq!(changed.into_inner(), before);
    }
}

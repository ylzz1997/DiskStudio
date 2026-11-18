//! exFAT end-boundary changes, including FAT capacity and bitmap sizing.
use super::super::{
    allocation, at, e, exfat_boot_checksum, scan_progress::exfat_live_stream, u16at, u32at, u64at,
    Fs,
};
use super::{bitmap_test, least_fat_sectors, Plan, Relocation, SectorEdit, MAX_DIRECTORY_BYTES};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};

fn update_boot(boot: &mut [u8], sector: usize, size: u64, count: u32, used: u32) {
    boot[72..80].copy_from_slice(&(size / sector as u64).to_le_bytes());
    boot[92..96].copy_from_slice(&count.to_le_bytes());
    boot[112] = (u64::from(used) * 100 / u64::from(count)) as u8;
    let sum = exfat_boot_checksum(&boot[..sector * 11]);
    for word in boot[sector * 11..].chunks_exact_mut(4) {
        word.copy_from_slice(&sum.to_le_bytes());
    }
}

fn update_entry_checksum(bytes: &mut [u8]) {
    let sum = bytes
        .iter()
        .enumerate()
        .filter(|(i, _)| ![2, 3].contains(i))
        .fold(0u16, |acc, (_, &byte)| {
            acc.rotate_right(1).wrapping_add(u16::from(byte))
        });
    bytes[2..4].copy_from_slice(&sum.to_le_bytes());
}

pub fn prepare_exfat<R: Read + Seek>(
    r: &mut R,
    sector: u64,
    new_size: u64,
) -> Result<Plan, String> {
    if !matches!(sector, 512 | 4096) || new_size % sector != 0 || new_size < 1024 * 1024 {
        return Err("exFAT 要求至少 1 MiB、512 或 4096 字节扇区及对齐的容量。".into());
    }
    let old_size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let boot = at(r, 0, 512)?;
    if &boot[3..11] != b"EXFAT   "
        || 1u64.checked_shl(u32::from(boot[108])) != Some(sector)
        || u64at(&boot, 72).checked_mul(sector) != Some(old_size)
        || old_size == new_size
    {
        return Err("需要扇区一致、占满原分区的 exFAT 卷和不同的新容量。".into());
    }
    // Reuse independent primary/backup checksum, clean flags, and upcase
    // verification. The tree check additionally ties every live cluster to
    // a directory or mandatory system stream.
    allocation::inspect(r, &AtomicBool::new(false))?;
    let fs = Fs::load(r)?;
    let original_count = new_size
        .checked_sub(fs.heap)
        .ok_or("exFAT 新容量小于数据区起点。")?
        / fs.cluster;
    // Consume existing alignment padding before moving the cluster heap. If it
    // must move, keep its original alignment relative to the partition.
    let heap_for = |fat: u64| {
        let end = fs.fat + fat * sector;
        fs.heap + end.saturating_sub(fs.heap).div_ceil(fs.cluster) * fs.cluster
    };
    let upper = original_count.min(u64::from(0x7ffffffd_u32));
    let fat_sectors = least_fat_sectors(
        fs.fat_len / sector,
        ((upper + 2) * 4).div_ceil(sector).max(fs.fat_len / sector),
        |fat| {
            new_size
                .checked_sub(heap_for(fat))
                .is_some_and(|data| (data / fs.cluster + 2) * 4 <= fat * sector)
        },
    );
    let heap = heap_for(fat_sectors);
    let count = new_size
        .checked_sub(heap)
        .and_then(|bytes| u32::try_from(bytes / fs.cluster).ok())
        .filter(|&n| n > 0 && n <= 0x7ffffffd)
        .ok_or("exFAT 新容量或簇数越界。")?;
    if (u64::from(count) + 2) * 4 > fat_sectors * sector || (heap != fs.heap && count < fs.count) {
        return Err("exFAT 扩展后的分配表无法保留原有簇编号。".into());
    }
    let fat_field = u32::try_from(fat_sectors).map_err(|_| "exFAT FAT 扇区数溢出。")?;
    let heap_field = u32::try_from(heap / sector).map_err(|_| "exFAT 数据区起点溢出。")?;
    let new_bytes = u64::from(count).div_ceil(8);
    if new_bytes > 64 * 1024 * 1024
        || u64::from(count.saturating_sub(fs.count)) * 4 > 64 * 1024 * 1024
    {
        return Err("exFAT 新位图或待初始化的 FAT 项超过 64 MiB 分析预算。".into());
    }
    let old_bitmap = bitmap_chain(r, &fs)?;
    let bitmap_clusters = new_bytes.div_ceil(fs.cluster) as usize;
    let kept = old_bitmap.len().min(bitmap_clusters);
    let retired = old_bitmap[kept..].iter().copied().collect::<BTreeSet<_>>();
    let mut relocation = Relocation::new(&fs, count, sector as usize, |cluster| {
        bitmap_test(&fs.bitmap, (cluster - 2) as usize) && !retired.contains(&cluster)
    })?;
    relocation.heap_shift = heap - fs.heap;
    for &(_, to) in &relocation.pairs {
        if fs.fat(r, to)? == 0xfffffff7 {
            return Err("exFAT 空闲位图与 FAT 坏簇标记矛盾。".into());
        }
    }
    let mut next_bitmap = old_bitmap[..kept]
        .iter()
        .map(|&cluster| relocation.cluster(cluster))
        .collect::<Vec<_>>();
    let extra = (bitmap_clusters - kept) as u32;
    if extra > count.saturating_sub(fs.count) {
        return Err("exFAT 扩展区无法容纳所需的新位图簇。".into());
    }
    next_bitmap.extend(fs.count + 2..fs.count + 2 + extra);
    let mut checked = check(
        r,
        &fs,
        count,
        &relocation,
        Some((&old_bitmap, &next_bitmap)),
    )?;
    replace_bitmap_links(&old_bitmap, &next_bitmap, &mut checked.links);
    let mut edits = fat_edits(r, &fs, sector, count, checked.links)?;
    edits.extend(checked.directory_edits);
    update_bitmap(
        r,
        &fs,
        count,
        &old_bitmap,
        &next_bitmap,
        &relocation,
        &mut edits,
    )?;
    for offset in [12 * sector, 0] {
        let before = at(r, offset, (12 * sector) as usize)?;
        let mut after = before.clone();
        after[84..88].copy_from_slice(&fat_field.to_le_bytes());
        after[88..92].copy_from_slice(&heap_field.to_le_bytes());
        after[96..100].copy_from_slice(&relocation.cluster(fs.root).to_le_bytes());
        update_boot(&mut after, sector as usize, new_size, count, checked.used);
        edits.push(SectorEdit::at(offset, before, after));
    }
    Ok(Plan {
        edits,
        tree_digest: checked.digest,
        copies: relocation.copies(&fs, &fs.bitmap, checked.original_used)?,
        cluster_size: fs.cluster as usize,
        filesystem: "exFAT".into(),
        old_size,
        new_size,
        old_clusters: fs.count,
        new_clusters: count,
    })
}

fn bitmap_chain<R: Read + Seek>(r: &mut R, fs: &Fs) -> Result<Vec<u32>, String> {
    let root = fs.directory(r, fs.root, false, 0)?;
    let entry = root
        .chunks_exact(32)
        .take_while(|entry| entry[0] != 0)
        .find(|entry| entry[0] == 0x81 && entry[1] == 0)
        .ok_or("缺少 exFAT 活动位图记录。")?;
    fs.chain(r, u32at(entry, 20), Some(u64at(entry, 24)), false, false)
}

fn replace_bitmap_links(old: &[u32], new: &[u32], links: &mut BTreeMap<u32, u32>) {
    for (index, &cluster) in old.iter().enumerate() {
        if new.get(index) != Some(&cluster) {
            links.entry(cluster).or_insert(0);
        }
    }
    for (index, &cluster) in new.iter().enumerate() {
        let next = new.get(index + 1).copied().unwrap_or(u32::MAX);
        let previous_next = old.get(index + 1).copied().unwrap_or(u32::MAX);
        if old.get(index) != Some(&cluster) || next != previous_next {
            links.insert(cluster, next);
        }
    }
}

fn fat_edits<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    sector: u64,
    count: u32,
    links: BTreeMap<u32, u32>,
) -> Result<Vec<SectorEdit>, String> {
    let old_end = (u64::from(fs.count) + 2) * 4;
    let new_end = (u64::from(count) + 2) * 4;
    let mut touched = links
        .keys()
        .map(|&cluster| u64::from(cluster) * 4 / sector)
        .collect::<BTreeSet<_>>();
    if count > fs.count {
        touched.extend(old_end / sector..new_end.div_ceil(sector));
    }
    touched
        .into_iter()
        .map(|position| {
            let pos = position * sector;
            let offset = fs.fat + pos;
            let mut edit = if pos < old_end {
                let before = at(r, offset, sector as usize)?;
                SectorEdit::at(offset, before.clone(), before)
            } else {
                SectorEdit::initialize(offset, vec![0; sector as usize])
            };
            if count > fs.count && pos <= old_end && old_end < pos + sector {
                edit.after[(old_end - pos) as usize..].fill(0);
            }
            for (&cluster, &next) in links.range((pos / 4) as u32..((pos + sector) / 4) as u32) {
                let index = (u64::from(cluster) * 4 - pos) as usize;
                edit.after[index..index + 4].copy_from_slice(&next.to_le_bytes());
            }
            Ok(edit)
        })
        .collect()
}

fn update_bitmap<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    count: u32,
    old_bitmap: &[u32],
    new_bitmap: &[u32],
    relocation: &Relocation,
    edits: &mut Vec<SectorEdit>,
) -> Result<(), String> {
    let retained = count.min(fs.count) as usize;
    let length = (count as usize).div_ceil(8);
    let mut desired = fs.bitmap[..retained.div_ceil(8)].to_vec();
    desired.resize(length, 0);
    if retained % 8 != 0 {
        desired[retained / 8] &= (1 << (retained % 8)) - 1;
    }
    for &from in &old_bitmap[new_bitmap.len().min(old_bitmap.len())..] {
        let index = (from - 2) as usize;
        if index < retained {
            desired[index / 8] &= !(1 << (index % 8));
        }
    }
    for &(_, to) in &relocation.pairs {
        let index = (to - 2) as usize;
        desired[index / 8] |= 1 << (index % 8);
    }
    for &to in new_bitmap {
        let index = (to - 2) as usize;
        desired[index / 8] |= 1 << (index % 8);
    }
    for (index, (&to, bytes)) in new_bitmap
        .iter()
        .zip(desired.chunks(fs.cluster as usize))
        .enumerate()
    {
        let target = fs.heap + relocation.heap_shift + u64::from(to - 2) * fs.cluster;
        if let Some(&from) = old_bitmap.get(index) {
            let source = fs.offset(from)?;
            let before = at(r, source, bytes.len())?;
            relocation.patch_sectors(r, edits, source, target, &before, bytes)?;
        } else {
            let mut after = bytes.to_vec();
            after.resize(
                after.len().div_ceil(relocation.sector) * relocation.sector,
                0,
            );
            edits.push(SectorEdit::initialize(target, after));
        }
    }
    Ok(())
}

pub(super) struct Check {
    pub digest: [u8; 32],
    used: u32,
    original_used: u32,
    links: BTreeMap<u32, u32>,
    directory_edits: Vec<SectorEdit>,
}

struct ExfatTree<'a> {
    claimed: Vec<u8>,
    limit: u32,
    used: u32,
    digest: Sha256,
    relocation: &'a Relocation,
    links: BTreeMap<u32, u32>,
}

impl ExfatTree<'_> {
    fn mark(&mut self, cluster: u32) -> Result<(), String> {
        if !(2..self.limit + 2).contains(&cluster) {
            return Err(format!("exFAT 活动记录引用越界簇 {cluster}。"));
        }
        let index = (cluster - 2) as usize;
        let bit = 1 << (index % 8);
        let byte = &mut self.claimed[index / 8];
        if *byte & bit != 0 {
            return Err("exFAT 多个活动记录引用同一簇。".into());
        }
        *byte |= bit;
        self.used += 1;
        Ok(())
    }

    fn claim(&mut self, chain: &[u32], contiguous: bool) -> Result<(u32, bool), String> {
        let relocation = self.relocation;
        let cluster = |c| relocation.cluster(c);
        let still_contiguous = contiguous
            && chain
                .windows(2)
                .all(|pair| cluster(pair[0]) + 1 == cluster(pair[1]));
        self.digest.update((chain.len() as u64).to_le_bytes());
        for (i, &from) in chain.iter().enumerate() {
            self.mark(from)?;
            let to = cluster(from);
            self.digest.update(to.to_le_bytes());
            let next = chain.get(i + 1).copied().unwrap_or(u32::MAX);
            let after = cluster(next);
            if !still_contiguous && (contiguous || from != to || next != after) {
                self.links.insert(to, after);
            }
            if from != to && !contiguous {
                self.links.insert(from, 0);
            }
        }
        Ok((chain.first().copied().map_or(0, cluster), still_contiguous))
    }
}

pub(super) fn check<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    count: u32,
    relocation: &Relocation,
    bitmap_projection: Option<(&[u32], &[u32])>,
) -> Result<Check, String> {
    let volume = r.seek(SeekFrom::End(0)).map_err(e)?;
    let mut tree = ExfatTree {
        claimed: vec![0; fs.bitmap.len()],
        limit: fs.count,
        used: 0,
        digest: Sha256::new(),
        relocation,
        links: BTreeMap::new(),
    };
    tree.claim(&fs.chain(r, fs.root, None, false, false)?, false)?;
    let mut directories = vec![(fs.root, None, false)];
    let mut directory_edits = Vec::new();
    let mut total_directory_bytes = 0u64;
    let mut bitmap = None;
    let mut upcase = false;
    let mut label = false;
    while let Some((first, size, contiguous)) = directories.pop() {
        let root = size.is_none();
        let chain = fs.chain(r, first, size, contiguous, false)?;
        let directory_bytes = chain.len() as u64 * fs.cluster;
        total_directory_bytes += directory_bytes;
        if total_directory_bytes > MAX_DIRECTORY_BYTES || directory_bytes > MAX_DIRECTORY_BYTES / 2
        {
            return Err("exFAT 目录读取超过资源预算。".into());
        }
        let mut bytes = Vec::with_capacity(directory_bytes as usize);
        for &cluster in &chain {
            bytes.extend(at(r, fs.offset(cluster)?, fs.cluster as usize)?);
        }
        let mut updated = bytes.clone();
        let mut bitmap_length = None;
        let slot_offset = |i: usize| -> Result<u64, String> {
            let byte = i as u64 * 32;
            Ok(fs.offset(chain[(byte / fs.cluster) as usize])? + byte % fs.cluster)
        };
        let mut i = 0;
        while i < bytes.len() / 32 {
            let entry = &bytes[i * 32..(i + 1) * 32];
            match entry[0] {
                0 => break,
                t if t & 0x80 == 0 => {}
                0x81 if root && bitmap.is_none() && entry[1] == 0 => {
                    let bitmap_chain =
                        fs.chain(r, u32at(entry, 20), Some(u64at(entry, 24)), false, false)?;
                    let projected = if let Some((original, next)) = bitmap_projection {
                        if bitmap_chain != original {
                            return Err("exFAT 位图簇链在准备期间已改变。".into());
                        }
                        next
                    } else {
                        &bitmap_chain
                    };
                    let first = *projected.first().ok_or("exFAT 位图簇链为空。")?;
                    for &cluster in &bitmap_chain {
                        tree.mark(cluster)?;
                    }
                    tree.digest.update((projected.len() as u64).to_le_bytes());
                    for &cluster in projected {
                        tree.digest.update(cluster.to_le_bytes());
                    }
                    bitmap = Some((bitmap_chain.len(), projected.len()));
                    updated[i * 32 + 20..i * 32 + 24].copy_from_slice(&first.to_le_bytes());
                    let range = i * 32 + 24..(i + 1) * 32;
                    updated[range.clone()]
                        .copy_from_slice(&u64::from(count).div_ceil(8).to_le_bytes());
                    bitmap_length = Some(range);
                }
                0x82 if root && !upcase => {
                    upcase = true;
                    let length = u64at(entry, 24);
                    if length == 0 || length > 131072 {
                        return Err("exFAT 大小写表长度无效。".into());
                    }
                    let table_chain = fs.chain(r, u32at(entry, 20), Some(length), false, false)?;
                    let (first, _) = tree.claim(&table_chain, false)?;
                    updated[i * 32 + 20..i * 32 + 24].copy_from_slice(&first.to_le_bytes());
                    let content = fs.content(r, u32at(entry, 20), length, false, false)?;
                    let sum = content
                        .iter()
                        .fold(0u32, |h, &v| h.rotate_right(1).wrapping_add(u32::from(v)));
                    if sum != u32at(entry, 4) {
                        return Err("exFAT 大小写表校验失败。".into());
                    }
                    tree.digest.update(Sha256::digest(&content));
                }
                0x83 if root && !label && entry[1] <= 11 => label = true,
                0x85 => {
                    let end = i + usize::from(entry[1]) + 1;
                    if !(2..=18).contains(&entry[1])
                        || end > bytes.len() / 32
                        || u16at(entry, 4) & !0x37 != 0
                    {
                        return Err("exFAT 活动文件项组无效。".into());
                    }
                    let entries = (i..end)
                        .map(|n| Ok((slot_offset(n)?, bytes[n * 32..(n + 1) * 32].to_vec())))
                        .collect::<Result<Vec<_>, String>>()?;
                    let stream = exfat_live_stream(&entries, volume)?;
                    let (first, contiguous) = tree.claim(
                        &fs.chain(
                            r,
                            stream.first,
                            Some(stream.length),
                            stream.contiguous,
                            false,
                        )?,
                        stream.contiguous,
                    )?;
                    if stream.directory {
                        if stream.length % fs.cluster != 0 || directories.len() >= 65536 {
                            return Err("exFAT 子目录长度无效或目录过多。".into());
                        }
                        directories.push((stream.first, Some(stream.length), stream.contiguous));
                    }
                    if first != stream.first || contiguous != stream.contiguous {
                        let group = &mut updated[i * 32..end * 32];
                        group[33] = 1 | (u8::from(contiguous) << 1);
                        group[52..56].copy_from_slice(&first.to_le_bytes());
                        update_entry_checksum(group);
                    }
                    i = end;
                    continue;
                }
                _ => return Err("exFAT 存在无效、重复或不支持的活动目录项。".into()),
            }
            i += 1;
        }
        if bytes != updated {
            for (i, &cluster) in chain.iter().enumerate() {
                let start = i * fs.cluster as usize;
                let end = start + fs.cluster as usize;
                relocation.patch_sectors(
                    r,
                    &mut directory_edits,
                    fs.offset(cluster)?,
                    fs.offset(relocation.cluster(cluster))? + relocation.heap_shift,
                    &bytes[start..end],
                    &updated[start..end],
                )?;
            }
        }
        if let Some(range) = bitmap_length {
            updated[range].fill(0);
        }
        tree.digest.update(Sha256::digest(&updated));
    }
    let (old_bitmap, new_bitmap) = bitmap.ok_or("缺少 exFAT 活动位图记录。")?;
    if !upcase {
        return Err("缺少 exFAT 大小写表。".into());
    }
    for index in 0..fs.count {
        let assigned = tree.claimed[index as usize / 8] & (1 << (index % 8)) != 0;
        if fs.allocated(index + 2)? != assigned {
            return Err(format!(
                "exFAT 存在未核对的分配或待移除范围占用，簇 {}。",
                index + 2
            ));
        }
    }
    let used = tree.used - old_bitmap as u32 + new_bitmap as u32;
    if used > count {
        return Err("exFAT 新空间不足以保存文件与系统簇。".into());
    }
    Ok(Check {
        digest: tree.digest.finalize().into(),
        used,
        original_used: tree.used,
        links: tree.links,
        directory_edits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fat_recovery::tests::exfat_fixture;
    use std::io::Cursor;

    const SECTOR: usize = 512;
    const HEAP: usize = 64 * SECTOR;
    const OLD_SIZE: usize = 2 * 1024 * 1024;
    const NEW_SIZE: usize = 1536 * 1024;

    fn image() -> Cursor<Vec<u8>> {
        let fixture = exfat_fixture();
        let mut bytes = vec![0; OLD_SIZE];
        bytes[..25 * SECTOR].copy_from_slice(&fixture[..25 * SECTOR]);
        bytes[HEAP..HEAP + 3 * SECTOR].copy_from_slice(&fixture[25 * SECTOR..28 * SECTOR]);
        bytes[84] = 40;
        bytes[88] = 64;
        let clusters = (OLD_SIZE - HEAP) / SECTOR;
        bytes[HEAP + 24..HEAP + 32].copy_from_slice(&(clusters.div_ceil(8) as u64).to_le_bytes());
        // Put the mandatory upcase stream in cluster 5.
        bytes[HEAP + 128] = 0x82;
        bytes[HEAP + 148] = 5;
        bytes[HEAP + 152] = 4;
        bytes[24 * SECTOR + 20..24 * SECTOR + 24].fill(0xff);
        bytes[HEAP + SECTOR] |= 1 << 3;
        update_boot(
            &mut bytes[..12 * SECTOR],
            SECTOR,
            OLD_SIZE as u64,
            clusters as u32,
            4,
        );
        bytes.copy_within(..12 * SECTOR, 12 * SECTOR);
        Cursor::new(bytes)
    }

    #[test]
    fn exfat_shrink_and_grow_preserve_file_and_boot_copies() {
        let mut disk = image();
        let shrink = prepare_exfat(&mut disk, SECTOR as u64, NEW_SIZE as u64).unwrap();
        shrink.apply(&mut disk).unwrap();
        disk.get_mut().truncate(NEW_SIZE);
        shrink.verify(&mut disk).unwrap();
        assert_eq!(
            u64at(disk.get_ref(), HEAP + 24),
            u64::from(shrink.new_clusters).div_ceil(8)
        );
        assert_eq!(
            &disk.get_ref()[..12 * SECTOR],
            &disk.get_ref()[12 * SECTOR..24 * SECTOR]
        );
        // Unused bytes in the bitmap cluster can contain stale bits. Growing
        // must clear them before those bits describe real clusters again.
        let offset = HEAP + SECTOR + shrink.new_clusters.div_ceil(8) as usize;
        disk.get_mut()[offset] = 0xff;
        let grow = prepare_exfat(&mut disk, SECTOR as u64, OLD_SIZE as u64).unwrap();
        disk.get_mut().resize(OLD_SIZE, 0);
        grow.apply(&mut disk).unwrap();
        grow.verify(&mut disk).unwrap();
        assert_eq!(
            &disk.get_ref()[HEAP + 2 * SECTOR..HEAP + 2 * SECTOR + 5],
            b"hello"
        );
        assert_eq!(disk.get_ref()[offset], 0);
    }

    #[test]
    fn exfat_relocates_system_streams_and_converts_a_split_contiguous_file() {
        let mut disk = image();
        let fs = Fs::load(&mut disk).unwrap();
        let file = ((NEW_SIZE - HEAP) / SECTOR) as u32 + 1;
        let root = fs.count - 3;
        let bitmap = fs.count - 2;
        let upcase = fs.count - 1;
        let length = 2 * SECTOR + 7;
        let data = (0..length).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let root_offset = fs.offset(root).unwrap() as usize;
        let bitmap_offset = fs.offset(bitmap).unwrap() as usize;
        let data_offset = fs.offset(file).unwrap() as usize;
        let bytes = disk.get_mut();
        bytes.copy_within(HEAP..HEAP + SECTOR, root_offset);
        bytes[root_offset + 20..root_offset + 24].copy_from_slice(&bitmap.to_le_bytes());
        bytes[root_offset + 148..root_offset + 152].copy_from_slice(&upcase.to_le_bytes());
        let group = &mut bytes[root_offset + 32..root_offset + 128];
        group[52..56].copy_from_slice(&file.to_le_bytes());
        group[40..48].copy_from_slice(&(length as u64).to_le_bytes());
        group[56..64].copy_from_slice(&(length as u64).to_le_bytes());
        update_entry_checksum(group);
        bytes[data_offset..data_offset + length].copy_from_slice(&data);
        for c in [root, bitmap, upcase, file, file + 1, file + 2] {
            let bit = (c - 2) as usize;
            bytes[bitmap_offset + bit / 8] |= 1 << (bit % 8);
        }
        for c in [root, bitmap, upcase] {
            let offset = 24 * SECTOR + c as usize * 4;
            bytes[offset..offset + 4].fill(0xff);
        }
        bytes[96..100].copy_from_slice(&root.to_le_bytes());
        update_boot(
            &mut bytes[..12 * SECTOR],
            SECTOR,
            OLD_SIZE as u64,
            fs.count,
            6,
        );
        bytes.copy_within(..12 * SECTOR, 12 * SECTOR);
        let plan = prepare_exfat(&mut disk, SECTOR as u64, NEW_SIZE as u64).unwrap();
        assert_eq!(plan.moved_clusters(), 5);
        plan.apply(&mut disk).unwrap();
        disk.get_mut().truncate(NEW_SIZE);
        plan.verify(&mut disk).unwrap();
        let fs = Fs::load(&mut disk).unwrap();
        let directory = fs.directory(&mut disk, fs.root, false, 0).unwrap();
        assert_eq!(directory[65], 1);
        assert_eq!(u32at(&directory, 84), file);
        assert_eq!(
            fs.content(&mut disk, file, length as u64, false, false)
                .unwrap(),
            data
        );
        assert_ne!(fs.root, root);
        assert_ne!(u32at(&directory, 20), bitmap);
        assert_ne!(u32at(&directory, 148), upcase);
    }

    #[test]
    fn exfat_rejects_unexplained_allocation_and_a_mismatched_backup() {
        let mut disk = image();
        let last_cluster_bit = (OLD_SIZE - HEAP) / SECTOR - 1;
        disk.get_mut()[HEAP + SECTOR + last_cluster_bit / 8] |= 1 << (last_cluster_bit % 8);
        assert!(prepare_exfat(&mut disk, SECTOR as u64, NEW_SIZE as u64)
            .unwrap_err()
            .contains("分配"));

        let mut disk = image();
        disk.get_mut()[12 * SECTOR + 72] ^= 1;
        assert!(prepare_exfat(&mut disk, SECTOR as u64, NEW_SIZE as u64).is_err());
    }

    #[test]
    fn exfat_extends_the_fat_and_adds_then_releases_a_bitmap_cluster() {
        for (size, use_padding) in [(4 * 1024 * 1024, false), (OLD_SIZE + 256 * 1024, true)] {
            let mut disk = image();
            let old = Fs::load(&mut disk).unwrap();
            if use_padding {
                let boot = disk.get_mut();
                boot[84..88].copy_from_slice(&32u32.to_le_bytes());
                update_boot(
                    &mut boot[..12 * SECTOR],
                    SECTOR,
                    OLD_SIZE as u64,
                    old.count,
                    4,
                );
                boot.copy_within(..12 * SECTOR, 12 * SECTOR);
            }
            let old = Fs::load(&mut disk).unwrap();
            let grow = prepare_exfat(&mut disk, SECTOR as u64, size as u64).unwrap();
            assert_eq!(grow.moved_clusters(), if use_padding { 0 } else { 4 });
            disk.get_mut().resize(size, 0xba);
            grow.apply(&mut disk).unwrap();
            grow.verify(&mut disk).unwrap();
            let fs = Fs::load(&mut disk).unwrap();
            assert!(fs.fat_len > old.fat_len);
            assert_eq!(fs.heap > old.heap, !use_padding);
            let successors = bitmap_chain(&mut disk, &fs).unwrap();
            assert_eq!(successors, vec![3, old.count + 2]);
            assert!(fs.allocated(old.count + 2).unwrap());
            assert_eq!(at(&mut disk, fs.offset(4).unwrap(), 5).unwrap(), b"hello");

            let shrink = prepare_exfat(&mut disk, SECTOR as u64, OLD_SIZE as u64).unwrap();
            assert_eq!(shrink.moved_clusters(), 0);
            shrink.apply(&mut disk).unwrap();
            disk.get_mut().truncate(OLD_SIZE);
            shrink.verify(&mut disk).unwrap();
            let fs = Fs::load(&mut disk).unwrap();
            assert_eq!(bitmap_chain(&mut disk, &fs).unwrap(), vec![3]);
            assert_eq!(at(&mut disk, fs.offset(4).unwrap(), 5).unwrap(), b"hello");
        }
    }
}

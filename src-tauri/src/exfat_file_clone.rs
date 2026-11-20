//! Pack live exFAT directory sets and the initialized bytes of each file into
//! a fresh exFAT volume. Paths and physical devices are handled by callers.
use super::super::{exfat as resize, least_fat_sectors, Relocation};
use super::{
    append_source, at, checkpoint, e, u16at, u32at, Content, Fs, Plan, Span, DIRECTORY_LIMIT,
    ENTRY_LIMIT, LABEL,
};
use crate::fat_recovery::{
    allocation,
    scan_progress::{exfat_live_stream, ExfatStream},
    u64at,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};

const TYPE: &str = "exFAT";
const FAT_START: u64 = 24;
const MAX_CLUSTERS: u64 = 16_777_214;
const MAX_CHAIN: u64 = 1_048_576;

pub(super) fn inspect<R: Read + Seek>(
    r: &mut R,
    sector: u64,
    cancel: &AtomicBool,
) -> Result<(Fs, [u8; 32]), String> {
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let boot = at(r, 0, 512)?;
    if &boot[3..11] != b"EXFAT   "
        || 1u64.checked_shl(u32::from(boot[108])) != Some(sector)
        || u64at(&boot, 72).checked_mul(sector) != Some(size)
        || u64::from(u32at(&boot, 92)) > MAX_CLUSTERS
    {
        return Err(
            "按文件克隆需要扇区一致、容量与分区匹配的 exFAT 卷，且源簇数不得超过分析上限。".into(),
        );
    }
    allocation::inspect(r, cancel)?;
    let fs = Fs::load(r)?;
    let checked = resize::check(r, &fs, fs.count, &Relocation::default(), None)?;
    let mut hash = Sha256::new();
    hash.update(at(r, 0, (FAT_START * sector) as usize)?);
    hash.update(&fs.bitmap);
    hash.update(checked.digest);
    Ok((fs, hash.finalize().into()))
}

fn upcase_map(bytes: &[u8]) -> Result<Vec<u16>, String> {
    let invalid = "exFAT 大小写表的 Unicode 映射不完整或无效。";
    if bytes.len() % 2 != 0 {
        return Err(invalid.into());
    }
    let mut words = bytes.chunks_exact(2).map(|pair| u16at(pair, 0));
    let mut mapping = Vec::with_capacity(65_536);
    while let Some(value) = words.next() {
        if mapping.len() == 65_536 {
            return Err(invalid.into());
        }
        if value == 0xffff && bytes.len() != 131_072 && mapping.len() < 65_535 {
            let count = usize::from(words.next().filter(|&n| n != 0).ok_or(invalid)?);
            let end = mapping.len() + count;
            if end > 65_536 {
                return Err(invalid.into());
            }
            mapping.extend((mapping.len()..end).map(|n| n as u16));
        } else {
            mapping.push(value);
        }
    }
    if mapping.len() != 65_536
        || (0u8..128).any(|c| mapping[c as usize] != u16::from(c.to_ascii_uppercase()))
    {
        return Err(invalid.into());
    }
    Ok(mapping)
}

fn upcased_name(group: &[u8], mapping: &[u16]) -> Result<Vec<u16>, String> {
    let length = usize::from(group[35]);
    if length == 0 || length > 255 || group.len() / 32 != 2 + length.div_ceil(15) {
        return Err("exFAT 文件名目录项数量与长度不符。".into());
    }
    let chars: Vec<_> = group[64..]
        .chunks_exact(32)
        .flat_map(|record| (2..32).step_by(2).map(|i| u16at(record, i)))
        .collect();
    let name = &chars[..length];
    if name == [0x2e]
        || name == [0x2e, 0x2e]
        || name
            .iter()
            .any(|&c| c <= 0x1f || (c <= 127 && b"\"*/:<>?\\|".contains(&(c as u8))))
        || String::from_utf16(name).is_err()
    {
        return Err("exFAT 文件名的 UTF-16 或字符无效。".into());
    }
    let folded: Vec<_> = name.iter().map(|&c| mapping[usize::from(c)]).collect();
    let hash = folded
        .iter()
        .flat_map(|c| c.to_le_bytes())
        .fold(0u16, |sum, byte| {
            sum.rotate_right(1).wrapping_add(u16::from(byte))
        });
    if hash != u16at(group, 36) {
        return Err("exFAT 文件名哈希与大小写表不一致。".into());
    }
    Ok(folded)
}

struct Entry {
    records: Vec<u8>,
    stream: ExfatStream,
    child: Option<usize>,
}
struct Directory {
    bytes: u64,
    depth: usize,
    entries: Vec<Entry>,
}

impl Directory {
    fn output_bytes(&self) -> u64 {
        self.bytes + if self.depth == 0 { 96 } else { 32 }
    }
}

fn catalog<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    root: Vec<u8>,
    mapping: &[u16],
    cancel: &AtomicBool,
) -> Result<(Vec<Directory>, usize), String> {
    let volume = r.seek(SeekFrom::End(0)).map_err(e)?;
    let mut children = vec![(fs.root, false, 0, 0)];
    let mut result = Vec::new();
    let mut root = Some(root);
    let mut bytes_read = 0;
    let mut slots = 0;
    let mut files = 0;
    while let Some(&(first, contiguous, size, depth)) = children.get(result.len()) {
        checkpoint(cancel)?;
        let bytes = match root.take() {
            Some(bytes) => bytes,
            None => fs.directory(r, first, contiguous, size)?,
        };
        bytes_read += bytes.len();
        if bytes_read > DIRECTORY_LIMIT {
            return Err("按文件克隆的源目录超过 32 MiB 检查预算。".into());
        }
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        let mut at = 0;
        let mut active_bytes = 0;
        while at < bytes.len() {
            checkpoint(cancel)?;
            let ty = bytes[at];
            if ty == 0 {
                break;
            }
            if ty != 0x85 {
                if ty & 0x80 != 0 && !(depth == 0 && matches!(ty, 0x81..=0x83)) {
                    return Err("exFAT 存在不支持的活动目录项。".into());
                }
                at += 32;
                continue;
            }
            let end = at + (1 + usize::from(bytes[at + 1])) * 32;
            let group = bytes.get(at..end).ok_or("exFAT 活动文件项组越界。")?;
            let records: Vec<_> = group
                .chunks_exact(32)
                .enumerate()
                .map(|(i, row)| ((at + i * 32) as u64, row.to_vec()))
                .collect();
            let stream = exfat_live_stream(&records, volume)?;
            if !keys.insert(upcased_name(group, mapping)?) {
                return Err("exFAT 同一目录包含重复的活动文件名。".into());
            }
            slots += 1;
            if slots > ENTRY_LIMIT {
                return Err("按文件克隆的活动条目超过 250,000 项。".into());
            }
            let child = if stream.directory {
                if depth + 2 >= crate::recovery_tree::MAX_DEPTH {
                    return Err("按文件克隆的目录深度超过上限。".into());
                }
                let i = children.len();
                children.push((stream.first, stream.contiguous, stream.length, depth + 1));
                Some(i)
            } else {
                files += 1;
                None
            };
            active_bytes += (end - at) as u64;
            entries.push(Entry {
                records: group.to_vec(),
                stream,
                child,
            });
            at = end;
        }
        result.push(Directory {
            bytes: active_bytes,
            depth,
            entries,
        });
    }
    Ok((result, files))
}

struct Geometry {
    cluster: u64,
    fat_sectors: u32,
    heap: u32,
    count: u32,
    sector: u64,
}

impl Geometry {
    fn new(size: u64, sector: u64, cluster: u64) -> Option<Self> {
        let total = size / sector;
        let spc = cluster / sector;
        let heap_for = |fat: u64| (FAT_START + fat).div_ceil(spc) * spc;
        let max = total.checked_sub(heap_for(1))? / spc;
        if max > MAX_CLUSTERS + (MAX_CLUSTERS + 2) * 4 / cluster {
            return None;
        }
        let fat = least_fat_sectors(1, ((max + 2) * 4).div_ceil(sector), |fat| {
            total
                .checked_sub(heap_for(fat))
                .is_some_and(|left| (left / spc + 2) * 4 <= fat * sector)
        });
        let heap = heap_for(fat);
        let count = total.checked_sub(heap)? / spc;
        if !(1..=MAX_CLUSTERS).contains(&count) || (count + 2) * 4 > fat * sector {
            return None;
        }
        Some(Self {
            cluster,
            fat_sectors: u32::try_from(fat).ok()?,
            heap: u32::try_from(heap).ok()?,
            count: count as u32,
            sector,
        })
    }

    fn offset(&self, cluster: u32) -> u64 {
        u64::from(self.heap) * self.sector + u64::from(cluster - 2) * self.cluster
    }

    fn take(&self, next: &mut u32, length: u64) -> Result<u32, String> {
        let count = length.div_ceil(self.cluster);
        if count == 0 {
            return Ok(0);
        }
        let first = *next;
        *next = u64::from(first)
            .checked_add(count)
            .filter(|&end| end <= u64::from(self.count) + 2)
            .ok_or("目标容量不足以存放全部活动文件和目录。")? as u32;
        Ok(first)
    }

    fn boot(&self, size: u64, offset: u64, serial: u32, root: u32, used: u32) -> Vec<u8> {
        let sector = self.sector as usize;
        let mut boot = vec![0u8; 12 * sector];
        boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[64..72].copy_from_slice(&(offset / self.sector).to_le_bytes());
        boot[80..84].copy_from_slice(&(FAT_START as u32).to_le_bytes());
        boot[84..88].copy_from_slice(&self.fat_sectors.to_le_bytes());
        boot[88..92].copy_from_slice(&self.heap.to_le_bytes());
        boot[96..100].copy_from_slice(&root.to_le_bytes());
        boot[100..104].copy_from_slice(&serial.to_le_bytes());
        boot[104..106].copy_from_slice(&0x100u16.to_le_bytes());
        boot[108] = self.sector.trailing_zeros() as u8;
        boot[109] = (self.cluster / self.sector).trailing_zeros() as u8;
        boot[110] = 1;
        boot[111] = 0x80;
        boot[120..124].copy_from_slice(&[0xfa, 0xf4, 0xeb, 0xfd]);
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        for end in (2..=9).map(|index| index * sector) {
            boot[end - 4..end].copy_from_slice(&[0, 0, 0x55, 0xaa]);
        }
        resize::update_boot(&mut boot, sector, size, self.count, used);
        boot.extend_from_within(..);
        boot
    }
}

fn metadata(plan: &mut Plan, offset: u64, bytes: Vec<u8>) {
    plan.spans.push(Span {
        offset,
        content: Content::Metadata(bytes),
    });
}

fn append_chain(fat: &mut Vec<u8>, first: u32, count: u32) {
    for cluster in first..first + count {
        let next = if cluster + 1 == first + count {
            u32::MAX
        } else {
            cluster + 1
        };
        fat.extend_from_slice(&next.to_le_bytes());
    }
}

fn fit(size: u64, sector: u64, dirs: &[Directory], upcase_len: u64) -> Result<Geometry, String> {
    let preferred = sector.max(4096);
    let candidates = std::iter::successors(Some(preferred), |n| {
        (n < &(32 * 1024 * 1024)).then(|| n * 2)
    })
    .chain(std::iter::successors(
        preferred.checked_div(2).filter(|&n| n >= sector),
        |n| Some(n / 2).filter(|&n| n >= sector),
    ));
    for cluster in candidates {
        let Some(g) = Geometry::new(size, sector, cluster) else {
            continue;
        };
        let directory_clusters = dirs.iter().map(|dir| dir.output_bytes().div_ceil(cluster));
        let file_clusters = dirs
            .iter()
            .flat_map(|dir| &dir.entries)
            .filter(|entry| entry.child.is_none())
            .map(|entry| entry.stream.length.div_ceil(cluster));
        let mut total = 0u64;
        let streams_fit = directory_clusters.chain(file_clusters).all(|clusters| {
            total += clusters;
            clusters <= MAX_CHAIN && total <= u64::from(g.count)
        });
        let overhead =
            u64::from(g.count).div_ceil(8).div_ceil(cluster) + upcase_len.div_ceil(cluster);
        if streams_fit && total + overhead <= u64::from(g.count) {
            return Ok(g);
        }
    }
    Err("目标 exFAT 容量或簇数不足以保存全部活动文件及目录。".into())
}

pub(super) fn prepare<R: Read + Seek>(
    r: &mut R,
    sector: u64,
    size: u64,
    offset: u64,
    serial: u32,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    if size < 1024 * 1024 || size % sector != 0 {
        return Err("exFAT 目标容量须至少为 1 MiB 且按扇区对齐。".into());
    }
    let (fs, proof) = inspect(r, sector, cancel)?;
    let source_serial = u32at(&at(r, 100, 4)?, 0);
    let serial = if serial == source_serial {
        serial ^ 1
    } else {
        serial
    };
    let root = fs.directory(r, fs.root, false, 0)?;
    let upcase = root
        .chunks_exact(32)
        .take_while(|row| row[0] != 0)
        .find(|row| row[0] == 0x82)
        .ok_or("缺少 exFAT 大小写表。")?;
    let upcase_len = u64at(upcase, 24);
    let upcase_bytes = fs.content(r, u32at(upcase, 20), upcase_len, false, false)?;
    let mut upcase_record: [u8; 32] = upcase.try_into().unwrap();
    let (dirs, files) = catalog(r, &fs, root, &upcase_map(&upcase_bytes)?, cancel)?;
    let g = fit(size, sector, &dirs, upcase_len)?;
    let mut plan = Plan {
        spans: vec![],
        size,
        sector,
        proof,
        target_proof: [0; 32],
        source_format: TYPE,
        destination_format: TYPE,
        files,
        directories: dirs.len() - 1,
    };
    let mut next = 2;
    let bitmap_len = u64::from(g.count).div_ceil(8);
    let bitmap = g.take(&mut next, bitmap_len)?;
    let upcase = g.take(&mut next, upcase_len)?;
    let dir_lengths: Vec<_> = dirs
        .iter()
        .map(|dir| dir.output_bytes().div_ceil(g.cluster) * g.cluster)
        .collect();
    let starts = dir_lengths
        .iter()
        .map(|&bytes| g.take(&mut next, bytes))
        .collect::<Result<Vec<_>, _>>()?;
    let root_first = starts[0];
    let mut fat = vec![0xf8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    append_chain(&mut fat, bitmap, upcase - bitmap);
    append_chain(&mut fat, upcase, root_first - upcase);
    append_chain(&mut fat, root_first, (dir_lengths[0] / g.cluster) as u32);
    metadata(&mut plan, FAT_START * sector, fat);
    metadata(&mut plan, g.offset(upcase), upcase_bytes);
    for (i, dir) in dirs.into_iter().enumerate() {
        checkpoint(cancel)?;
        let mut bytes = Vec::new();
        if i == 0 {
            let mut label = [0u8; 32];
            label[0] = 0x83;
            label[1] = LABEL.len() as u8;
            for (at, ch) in LABEL.encode_utf16().enumerate() {
                label[2 + 2 * at..4 + 2 * at].copy_from_slice(&ch.to_le_bytes());
            }
            let mut map = [0u8; 32];
            map[0] = 0x81;
            map[20..24].copy_from_slice(&bitmap.to_le_bytes());
            map[24..32].copy_from_slice(&bitmap_len.to_le_bytes());
            upcase_record[20..24].copy_from_slice(&upcase.to_le_bytes());
            for entry in [label, map, upcase_record] {
                bytes.extend_from_slice(&entry);
            }
        }
        for Entry {
            mut records,
            stream,
            child,
        } in dir.entries
        {
            let (first, length, valid) = if let Some(child) = child {
                (starts[child], dir_lengths[child], dir_lengths[child])
            } else {
                let first = g.take(&mut next, stream.length)?;
                let valid = u64at(&records, 40);
                let mut left = valid;
                if left > 0 {
                    let chain = fs.chain(
                        r,
                        stream.first,
                        Some(stream.length),
                        stream.contiguous,
                        false,
                    )?;
                    let mut offset = g.offset(first);
                    for cluster in chain {
                        if left == 0 {
                            break;
                        }
                        checkpoint(cancel)?;
                        let bytes = left.min(fs.cluster);
                        append_source(&mut plan.spans, offset, fs.offset(cluster)?, bytes)?;
                        left -= bytes;
                        offset += bytes;
                    }
                    if left != 0 {
                        return Err("exFAT 源文件未提供声明的有效数据。".into());
                    }
                }
                (first, stream.length, valid)
            };
            records[33] = if first == 0 { 1 } else { 3 };
            records[40..48].copy_from_slice(&valid.to_le_bytes());
            records[52..56].copy_from_slice(&first.to_le_bytes());
            records[56..64].copy_from_slice(&length.to_le_bytes());
            let last = usize::from(records[35]) % 15;
            if last != 0 {
                let padding = records.len() - (15 - last) * 2;
                records[padding..].fill(0);
            }
            resize::update_entry_checksum(&mut records);
            bytes.extend_from_slice(&records);
        }
        if bytes.is_empty() {
            bytes.resize(32, 0);
        }
        metadata(&mut plan, g.offset(starts[i]), bytes);
    }
    let used = next - 2;
    let mut bits = vec![0; bitmap_len as usize];
    let full = (used / 8) as usize;
    bits[..full].fill(0xff);
    if used % 8 != 0 {
        bits[full] = (1 << (used % 8)) - 1;
    }
    metadata(&mut plan, g.offset(bitmap), bits);
    metadata(&mut plan, 0, g.boot(size, offset, serial, root_first, used));
    plan.validate_spans()?;
    plan.target_proof = inspect(&mut plan.reader(r), sector, cancel)?.1;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::MIB;
    use std::io::Cursor;

    fn group(name: &str, attributes: u16, first: u32, length: u64, valid: u64) -> [u8; 96] {
        let mut group = [0u8; 96];
        group[0] = 0x85;
        group[1] = 2;
        group[4..6].copy_from_slice(&attributes.to_le_bytes());
        group[8..12].copy_from_slice(&0x56e9_2401u32.to_le_bytes());
        group[32] = 0xc0;
        group[33] = 1;
        let chars: Vec<_> = name.encode_utf16().collect();
        group[35] = chars.len() as u8;
        let hash = chars
            .iter()
            .flat_map(|&c| {
                if c < 128 {
                    (c as u8).to_ascii_uppercase() as u16
                } else {
                    c
                }
                .to_le_bytes()
            })
            .fold(0u16, |sum, byte| {
                sum.rotate_right(1).wrapping_add(u16::from(byte))
            });
        group[36..38].copy_from_slice(&hash.to_le_bytes());
        group[40..48].copy_from_slice(&valid.to_le_bytes());
        group[52..56].copy_from_slice(&first.to_le_bytes());
        group[56..64].copy_from_slice(&length.to_le_bytes());
        group[64] = 0xc1;
        for (i, c) in chars.into_iter().enumerate() {
            group[66 + 2 * i..68 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        resize::update_entry_checksum(&mut group);
        group
    }

    fn fixture(sector: u64) -> (Cursor<Vec<u8>>, u64) {
        let length = 8 * MIB;
        let g = Geometry::new(length, sector, sector).unwrap();
        let valid = sector + 19;
        let mut bytes = vec![0u8; length as usize];
        let upcase: Vec<_> = (0u8..128)
            .map(|v| u16::from(v.to_ascii_uppercase()))
            .chain([0xffff, 65_408])
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let bitmap_len = u64::from(g.count).div_ceil(8);
        let bitmap_clusters = bitmap_len.div_ceil(sector);
        let boot = g.boot(length, 0, 17, 9, 6 + bitmap_clusters as u32);
        bytes[..boot.len()].copy_from_slice(&boot);
        let map = g.offset(2) as usize;
        let upcase_start = g.offset(7) as usize;
        let root = g.offset(9) as usize;
        bytes[upcase_start..upcase_start + upcase.len()].copy_from_slice(&upcase);
        let fat = (FAT_START * sector) as usize;
        bytes[fat..fat + 8].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let bits = (2..2 + bitmap_clusters as u32).map(|i| {
            (
                i,
                if i + 1 == 2 + bitmap_clusters as u32 {
                    u32::MAX
                } else {
                    i + 1
                },
            )
        });
        for (i, link) in bits.chain([
            (7, u32::MAX),
            (9, u32::MAX),
            (15, u32::MAX),
            (31, 104),
            (104, 40),
            (40, u32::MAX),
        ]) {
            bytes[fat + 4 * i as usize..fat + 4 * i as usize + 4]
                .copy_from_slice(&link.to_le_bytes());
            bytes[map + (i as usize - 2) / 8] |= 1 << ((i as usize - 2) % 8);
        }
        bytes[root] = 0x81;
        bytes[root + 20..root + 24].copy_from_slice(&2u32.to_le_bytes());
        bytes[root + 24..root + 32].copy_from_slice(&bitmap_len.to_le_bytes());
        bytes[root + 32] = 0x82;
        let upcase_checksum = upcase.iter().fold(0u32, |sum, &v| {
            sum.rotate_right(1).wrapping_add(u32::from(v))
        });
        bytes[root + 36..root + 40].copy_from_slice(&upcase_checksum.to_le_bytes());
        bytes[root + 52..root + 56].copy_from_slice(&7u32.to_le_bytes());
        bytes[root + 56..root + 64].copy_from_slice(&(upcase.len() as u64).to_le_bytes());
        bytes[root + 64..root + 160].copy_from_slice(&group(
            "資料.bin",
            0x27,
            31,
            3 * sector - 5,
            valid,
        ));
        bytes[root + 160..root + 256].copy_from_slice(&group("empty", 0x10, 15, sector, sector));
        bytes[root + 256] = 0x05;
        for (cluster, byte) in [(31, 0x23), (104, 0xe7), (40, 0xe7)] {
            let at = g.offset(cluster) as usize;
            bytes[at..at + sector as usize].fill(byte);
        }
        let at = g.offset(104) as usize;
        bytes[at..at + 19].fill(0x24);
        (Cursor::new(bytes), root as u64)
    }

    #[test]
    fn exfat_file_clone_rebuilds_fragmented_files_empty_dirs_and_valid_data_on_both_sector_sizes() {
        for sector in [512, 4096] {
            let (mut input, _) = fixture(sector);
            let cancel = AtomicBool::new(false);
            let plan = crate::fat_recovery::resize::file_clone::prepare(
                &mut input,
                sector,
                2 * MIB,
                3 * MIB,
                17,
                &cancel,
            )
            .unwrap();
            assert_eq!(
                (
                    plan.source_format,
                    plan.destination_format,
                    plan.files,
                    plan.directories
                ),
                ("exFAT", "exFAT", 1, 1)
            );
            let mut target = plan.reader(&mut input);
            plan.verify(&mut target).unwrap();
            let fs = Fs::load(&mut target).unwrap();
            let boot = at(&mut target, 0, (24 * sector) as usize).unwrap();
            assert_eq!(u64at(&boot, 64), 3 * MIB / sector);
            assert_ne!(u32at(&boot, 100), 17);
            assert_eq!(
                &boot[..(12 * sector) as usize],
                &boot[(12 * sector) as usize..]
            );
            let root = fs.directory(&mut target, fs.root, false, 0).unwrap();
            let file = &root[96..192];
            assert_eq!(file[33], 3);
            assert_eq!(u16at(file, 4), 0x27);
            assert_eq!(u32at(file, 8), 0x56e9_2401);
            assert_eq!(u64at(file, 40), sector + 19);
            let first = u32at(file, 52);
            assert_eq!(u64at(file, 56), 3 * sector - 5);
            let payload = at(
                &mut target,
                fs.offset(first).unwrap(),
                (3 * sector) as usize,
            )
            .unwrap();
            assert!(payload[..sector as usize].iter().all(|&byte| byte == 0x23));
            assert!(payload[sector as usize..sector as usize + 19]
                .iter()
                .all(|&byte| byte == 0x24));
            assert!(payload[sector as usize + 19..]
                .iter()
                .all(|&byte| byte == 0));
            assert_eq!(fs.fat(&mut target, first).unwrap(), 0);
            let child = &root[192..288];
            let dir = fs
                .directory(&mut target, u32at(child, 52), true, u64at(child, 56))
                .unwrap();
            assert!(dir.iter().all(|&byte| byte == 0));
            assert!(root[288..].iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn exfat_file_clone_rejects_a_checksum_valid_name_hash_mismatch() {
        let (mut input, root) = fixture(512);
        let record = &mut input.get_mut()[(root as usize + 64)..(root as usize + 160)];
        record[36] ^= 1;
        resize::update_entry_checksum(record);
        let Err(error) = prepare(&mut input, 512, 2 * MIB, 0, 1, &AtomicBool::new(false)) else {
            panic!("incorrect name hash was accepted");
        };
        assert!(error.contains("文件名哈希"));
    }
}

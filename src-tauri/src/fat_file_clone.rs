//! Rebuild a FAT or exFAT partition from its live files and directories.
//! Only bounded readers and in-memory metadata are used here.
use super::{at, check_classic_tree, classic_limit, e, u16at, u32at, ClassicTable, Fs, Relocation};
use disk_core::fat_format::{FatType, Plan as Format};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

#[path = "exfat_file_clone.rs"]
mod exfat;

const LABEL: &str = "DISKCLONE";
const METADATA_LIMIT: usize = 128 * 1024 * 1024;
const DIRECTORY_LIMIT: usize = 32 * 1024 * 1024;
const ENTRY_LIMIT: usize = 250_000;
const SPAN_LIMIT: usize = 2_000_000;

fn checkpoint(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("操作已取消。".into());
    }
    Ok(())
}

fn inspect<R: Read + Seek>(r: &mut R, sector: u64) -> Result<(Fs, ClassicTable, [u8; 32]), String> {
    let size = r.seek(SeekFrom::End(0)).map_err(e)?;
    let boot = at(r, 0, sector as usize)?;
    let marker: &[u8; 512] = boot[..512].try_into().unwrap();
    if matches!(&boot[3..11], b"EXFAT   " | b"NTFS    ") || !crate::recovery::fat_boot(marker) {
        return Err("按文件克隆目前支持可核对的 FAT12/16/32 数据分区。".into());
    }
    let fs = Fs::load(r)?;
    let (minimum, maximum) = classic_limit(fs.bits);
    let total = match (u16at(&boot, 19), u32at(&boot, 32)) {
        (0, total) => u64::from(total),
        (total, 0) if fs.bits != 32 => u64::from(total),
        _ => return Err("FAT 引导扇区中的总扇区数字段不一致。".into()),
    };
    if u64::from(u16at(&boot, 11)) != sector
        || fs.copies != 2
        || !fs.mirror
        || total * sector != size
        || fs.cluster > 32768
        || !(minimum..=maximum).contains(&fs.count)
    {
        return Err("按文件克隆要求扇区一致、两份镜像 FAT 和完整分区范围。".into());
    }
    if fs.bits == 32 {
        let backup = u64::from(u16at(&boot, 50));
        if backup == 0
            || backup >= u64::from(u16at(&boot, 14))
            || u16at(&boot, 40) != 0
            || at(r, backup * sector, sector as usize)? != boot
        {
            return Err("FAT32 主备引导扇区不一致或镜像设置不受支持。".into());
        }
    }
    let table = ClassicTable::read(r, &fs, &boot, fs.count)?;
    let tree = check_classic_tree(r, &fs, &table, &Relocation::default())?;
    let mut hash = Sha256::new();
    hash.update(boot);
    hash.update(table.bytes());
    hash.update(tree.digest);
    Ok((fs, table, hash.finalize().into()))
}

#[derive(Clone, Copy)]
enum EntryKind {
    Current,
    Parent,
    Directory(usize),
    File { first: u32, size: u32 },
}

struct Directory {
    source: u32,
    parent: usize,
    depth: usize,
    records: Vec<[u8; 32]>,
    entries: Vec<(usize, EntryKind)>,
}

fn validate_long(entries: &[[u8; 32]], short: &[u8; 32]) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let checksum = short[..11]
        .iter()
        .fold(0u8, |a, b| a.rotate_right(1).wrapping_add(*b));
    if entries.len() > 20
        || entries.iter().enumerate().any(|(i, record)| {
            let ordinal = (entries.len() - i) as u8 | if i == 0 { 0x40 } else { 0 };
            record[0] != ordinal
                || record[12] != 0
                || record[13] != checksum
                || u16at(record, 26) != 0
        })
    {
        return Err("FAT 活动长文件名记录或校验和无效。".into());
    }
    let chars: Vec<_> = entries
        .iter()
        .rev()
        .flat_map(|record| {
            [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].map(|at| u16at(record, at))
        })
        .collect();
    let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    if end == 0
        || end > 255
        || chars
            .get(end + 1..)
            .is_some_and(|tail| tail.iter().any(|&c| c != 0xffff))
        || chars[..end].contains(&0xffff)
        || String::from_utf16(&chars[..end]).is_err()
    {
        return Err("FAT 活动长文件名的 UTF-16 或填充无效。".into());
    }
    Ok(())
}

fn directories<R: Read + Seek>(
    r: &mut R,
    fs: &Fs,
    cancel: &AtomicBool,
) -> Result<Vec<Directory>, String> {
    let mut queue = vec![Directory {
        source: fs.root,
        parent: 0,
        depth: 0,
        records: vec![],
        entries: vec![],
    }];
    let mut index = 0;
    let mut slots = 0;
    let mut bytes = 0;
    while index < queue.len() {
        checkpoint(cancel)?;
        let records = fs.directory(r, queue[index].source, false, 0)?;
        bytes += records.len();
        if bytes > DIRECTORY_LIMIT {
            return Err("按文件克隆的源目录超过 32 MiB 检查预算。".into());
        }
        let mut long = Vec::new();
        let mut aliases = HashSet::new();
        let mut dot = [false; 2];
        for entry in records.chunks_exact(32) {
            let record: [u8; 32] = entry.try_into().unwrap();
            match (record[0], record[11]) {
                (0, _) => break,
                (0xe5, _) => {
                    long.clear();
                    continue;
                }
                (_, 15) => {
                    if long.len() == 20 {
                        return Err("FAT 长文件名记录数量超限。".into());
                    }
                    long.push(record);
                    continue;
                }
                _ => {}
            }
            validate_long(&long, &record)?;
            if record[11] & 8 != 0 {
                if !long.is_empty() {
                    return Err("FAT 卷标含有异常长文件名。".into());
                }
                continue;
            }
            let alias: [u8; 11] = record[..11].try_into().unwrap();
            if !aliases.insert(alias.map(|byte| byte.to_ascii_uppercase())) {
                return Err("FAT 目录包含重复短文件名。".into());
            }
            let low = u32::from(u16at(&record, 26));
            let high = u32::from(u16at(&record, 20));
            if fs.bits != 32 && high != 0 {
                return Err("按文件克隆不支持 FAT 扩展属性索引。".into());
            }
            let first = low | (high << 16);
            let kind = if record[11] & 16 == 0 {
                EntryKind::File {
                    first,
                    size: u32at(&record, 28),
                }
            } else if &alias == b".          " || &alias == b"..         " {
                if !long.is_empty() {
                    return Err("FAT 当前目录或父目录含有长文件名。".into());
                }
                let parent = alias[1] == b'.';
                dot[usize::from(parent)] = true;
                if parent {
                    EntryKind::Parent
                } else {
                    EntryKind::Current
                }
            } else {
                let depth = queue[index].depth + 1;
                if depth >= crate::recovery_tree::MAX_DEPTH || queue.len() >= ENTRY_LIMIT {
                    return Err("按文件克隆的目录深度或数量超过上限。".into());
                }
                let child = queue.len();
                queue.push(Directory {
                    source: first,
                    parent: index,
                    depth,
                    records: vec![],
                    entries: vec![],
                });
                EntryKind::Directory(child)
            };
            let current = &mut queue[index];
            current.records.append(&mut long);
            current.entries.push((current.records.len(), kind));
            current.records.push(record);
            slots += 1;
            if slots > ENTRY_LIMIT {
                return Err("按文件克隆的活动条目超过 250,000 项。".into());
            }
        }
        if !long.is_empty() || (index != 0 && dot != [true, true]) {
            return Err("FAT 目录含有孤立长文件名记录，或缺少当前目录和父目录项。".into());
        }
        index += 1;
    }
    Ok(queue)
}

enum Content {
    Metadata(Vec<u8>),
    Source { start: u64, bytes: u64 },
}

struct Span {
    offset: u64,
    content: Content,
}

impl Span {
    fn size(&self) -> u64 {
        match &self.content {
            Content::Metadata(bytes) => bytes.len() as u64,
            Content::Source { bytes, .. } => *bytes,
        }
    }
}

pub(crate) struct Plan {
    spans: Vec<Span>,
    size: u64,
    sector: u64,
    proof: [u8; 32],
    target_proof: [u8; 32],
    pub source_format: &'static str,
    pub destination_format: &'static str,
    pub files: usize,
    pub directories: usize,
}

fn clusters(first: &mut u32, count: u64, fs: &Fs, table: &mut ClassicTable) -> Result<u32, String> {
    let count = u32::try_from(count).map_err(|_| "目标 FAT 簇数越界。")?;
    if count == 0 {
        return Ok(0);
    }
    let start = *first;
    let end = start
        .checked_add(count)
        .filter(|&n| n <= fs.count + 2)
        .ok_or("目标容量不足以存放全部活动文件和目录。")?;
    for cluster in start..end {
        let next = if cluster + 1 == end {
            (1u32 << fs.bits.min(28)) - 1
        } else {
            cluster + 1
        };
        table.set_entry(cluster, next);
    }
    *first = end;
    Ok(start)
}

fn write_cluster(entry: &mut [u8; 32], first: u32, bits: u8) {
    entry[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    entry[20..22]
        .copy_from_slice(&(if bits == 32 { (first >> 16) as u16 } else { 0 }).to_le_bytes());
}

fn append_file(
    spans: &mut Vec<Span>,
    mut offset: u64,
    mut first: u32,
    mut size: u64,
    source: &Fs,
    table: &ClassicTable,
    cancel: &AtomicBool,
) -> Result<(), String> {
    while size > 0 {
        let pos = source.offset(first)?;
        let bytes = size.min(source.cluster);
        append_source(spans, offset, pos, bytes)?;
        checkpoint(cancel)?;
        offset += bytes;
        size -= bytes;
        first = table.entry(first);
    }
    Ok(())
}

fn append_source(spans: &mut Vec<Span>, offset: u64, pos: u64, bytes: u64) -> Result<(), String> {
    if let Some(Span {
        offset: target,
        content: Content::Source {
            start,
            bytes: length,
        },
    }) = spans.last_mut()
    {
        if *target + *length == offset && *start + *length == pos {
            *length += bytes;
            return Ok(());
        }
    }
    if spans.len() >= SPAN_LIMIT {
        return Err("源文件碎片范围超过按文件克隆预算。".into());
    }
    spans.push(Span {
        offset,
        content: Content::Source { start: pos, bytes },
    });
    Ok(())
}

fn populate<R: Read + Seek>(
    r: &mut R,
    plan: &mut Plan,
    format: &Format,
    source: &Fs,
    source_table: &ClassicTable,
    dirs: &[Directory],
    cancel: &AtomicBool,
) -> Result<(), String> {
    let blank = format.initialized_prefix(METADATA_LIMIT).map_err(e)?;
    plan.spans = vec![Span {
        offset: 0,
        content: Content::Metadata(blank),
    }];
    let mut view = plan.reader(&mut *r);
    let dst = Fs::load(&mut view)?;
    let boot = at(&mut view, 0, 512)?;
    let mut fat = ClassicTable::read(&mut view, &dst, &boot, dst.count)?;
    let lengths: Vec<_> = dirs
        .iter()
        .enumerate()
        .map(|(i, dir)| (dir.records.len() as u64 + u64::from(i == 0)) * 32)
        .collect();
    let mut next = 2;
    let mut starts = Vec::with_capacity(dirs.len());
    for (i, &length) in lengths.iter().enumerate() {
        if let Some((_, capacity)) = dst.root_fixed.filter(|_| i == 0) {
            if length > capacity as u64 {
                return Err("目标 FAT 固定根目录无法容纳源目录项。".into());
            }
            starts.push(0);
        } else {
            starts.push(clusters(
                &mut next,
                length.div_ceil(dst.cluster),
                &dst,
                &mut fat,
            )?);
        }
    }
    let Some(Span {
        content: Content::Metadata(mut metadata),
        ..
    }) = plan.spans.pop()
    else {
        unreachable!()
    };
    metadata.truncate(dst.heap as usize);
    for (i, dir) in dirs.iter().enumerate() {
        checkpoint(cancel)?;
        let first = starts[i];
        let mut records = dir.records.clone();
        for &(slot, kind) in &dir.entries {
            let target = match kind {
                EntryKind::Current => first,
                EntryKind::Parent => {
                    if dir.parent == 0 {
                        0
                    } else {
                        starts[dir.parent]
                    }
                }
                EntryKind::Directory(child) => starts[child],
                EntryKind::File { first, size } => {
                    let size = u64::from(size);
                    let target = clusters(&mut next, size.div_ceil(dst.cluster), &dst, &mut fat)?;
                    if target != 0 {
                        append_file(
                            &mut plan.spans,
                            dst.offset(target)?,
                            first,
                            size,
                            source,
                            source_table,
                            cancel,
                        )?;
                    }
                    plan.files += 1;
                    target
                }
            };
            write_cluster(&mut records[slot], target, dst.bits);
        }
        let mut bytes = Vec::with_capacity(lengths[i] as usize);
        if i == 0 {
            let mut label = [0u8; 32];
            label[..11].fill(b' ');
            label[..LABEL.len()].copy_from_slice(LABEL.as_bytes());
            label[11] = 8;
            bytes.extend_from_slice(&label);
        }
        for record in records {
            bytes.extend_from_slice(&record);
        }
        if let Some((offset, capacity)) = dst.root_fixed.filter(|_| i == 0) {
            let offset = offset as usize;
            metadata[offset..offset + capacity].fill(0);
            metadata[offset..offset + bytes.len()].copy_from_slice(&bytes);
        } else {
            plan.spans.push(Span {
                offset: dst.offset(first)?,
                content: Content::Metadata(bytes),
            });
        }
    }
    for offset in [dst.fat, dst.fat + dst.fat_len] {
        let offset = offset as usize;
        metadata[offset..offset + fat.bytes().len()].copy_from_slice(fat.bytes());
    }
    if dst.bits == 32 {
        for offset in [plan.sector, 7 * plan.sector] {
            let offset = offset as usize;
            let free = dst.count + 2 - next;
            metadata[offset + 488..offset + 492].copy_from_slice(&free.to_le_bytes());
            metadata[offset + 492..offset + 496]
                .copy_from_slice(&(if free == 0 { u32::MAX } else { next }).to_le_bytes());
        }
    }
    plan.spans.push(Span {
        offset: 0,
        content: Content::Metadata(metadata),
    });
    plan.validate_spans()?;
    let mut view = plan.reader(r);
    let (_, _, verified) = inspect(&mut view, plan.sector)?;
    plan.target_proof = verified;
    Ok(())
}

pub(crate) fn prepare<R: Read + Seek>(
    r: &mut R,
    sector: u64,
    size: u64,
    offset: u64,
    serial: u32,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    if !matches!(sector, 512 | 4096) || offset % sector != 0 {
        return Err("目标分区与逻辑扇区未对齐。".into());
    }
    if at(r, 3, 8)? == b"EXFAT   " {
        return exfat::prepare(r, sector, size, offset, serial, cancel);
    }
    let hidden =
        u32::try_from(offset / sector).map_err(|_| "目标起始 LBA 超出 FAT 引导字段范围。")?;
    let (fs, table, proof) = inspect(r, sector)?;
    let source = match fs.bits {
        12 => FatType::Fat12,
        16 => FatType::Fat16,
        _ => FatType::Fat32,
    };
    let dirs = directories(r, &fs, cancel)?;
    let mut error = "目标容量不能创建 FAT12/16/32 文件系统。".to_owned();
    for kind in std::iter::once(source).chain(
        [FatType::Fat12, FatType::Fat16, FatType::Fat32]
            .into_iter()
            .filter(|&kind| kind != source),
    ) {
        checkpoint(cancel)?;
        let Ok(format) = Format::new(kind, size, sector as u32, 0, LABEL, serial, hidden) else {
            continue;
        };
        let mut plan = Plan {
            spans: vec![],
            size,
            sector,
            proof,
            target_proof: [0; 32],
            source_format: source.name(),
            destination_format: kind.name(),
            files: 0,
            directories: dirs.len() - 1,
        };
        match populate(r, &mut plan, &format, &fs, &table, &dirs, cancel) {
            Ok(()) => return Ok(plan),
            Err(detail) => error = detail,
        }
    }
    Err(error)
}

impl Plan {
    fn validate_spans(&mut self) -> Result<(), String> {
        self.spans.sort_unstable_by_key(|span| span.offset);
        let mut end = 0;
        for span in &self.spans {
            if span.offset < end || span.size() == 0 {
                return Err("目标文件系统逻辑复制范围重叠或为空。".into());
            }
            end = span
                .offset
                .checked_add(span.size())
                .filter(|&end| end <= self.size)
                .ok_or("目标文件系统逻辑复制范围越界。")?;
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.proof);
        hash.update(self.target_proof);
        hash.update(self.size.to_le_bytes());
        hash.finalize().into()
    }

    pub fn verify<R: Read + Seek>(&self, r: &mut R) -> Result<(), String> {
        let proof = if self.destination_format == "exFAT" {
            exfat::inspect(r, self.sector, &AtomicBool::new(false))?.1
        } else {
            inspect(r, self.sector)?.2
        };
        if proof != self.target_proof {
            return Err("目标 FAT 文件目录、引导或分配表与计划不同。".into());
        }
        Ok(())
    }

    pub fn reader<R: Read + Seek>(&self, source: R) -> Reader<'_, R> {
        Reader {
            plan: self,
            source,
            pos: 0,
        }
    }
}

pub(crate) struct Reader<'a, R> {
    plan: &'a Plan,
    source: R,
    pos: u64,
}

impl<R: Read + Seek> Read for Reader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let at = self.pos;
        let max = out
            .len()
            .min((self.plan.size - at).min(usize::MAX as u64) as usize);
        if max == 0 {
            return Ok(0);
        }
        let index = self
            .plan
            .spans
            .partition_point(|span| span.offset + span.size() <= at);
        let next = self.plan.spans.get(index);
        let len = match next {
            Some(span) if span.offset <= at => {
                let delta = at - span.offset;
                let len = (span.size() - delta).min(max as u64) as usize;
                match &span.content {
                    Content::Metadata(bytes) => {
                        out[..len].copy_from_slice(&bytes[delta as usize..delta as usize + len])
                    }
                    Content::Source { start, .. } => {
                        self.source.seek(SeekFrom::Start(*start + delta))?;
                        self.source.read_exact(&mut out[..len])?;
                    }
                }
                len
            }
            _ => {
                let len = next.map_or(max, |span| (span.offset - at).min(max as u64) as usize);
                out[..len].fill(0);
                len
            }
        };
        self.pos += len as u64;
        Ok(len)
    }
}

impl<R> Seek for Reader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let size = i128::from(self.plan.size);
        let pos = match from {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::End(delta) => size + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
        };
        if !(0..=size).contains(&pos) {
            return Err(io::Error::other("定位超出目标 FAT 文件克隆的分区范围。"));
        }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn record(name: &[u8; 11], attributes: u8, first: u32, size: u32) -> [u8; 32] {
        let mut entry = [0; 32];
        entry[..11].copy_from_slice(name);
        entry[11] = attributes;
        write_cluster(&mut entry, first, 16);
        entry[28..32].copy_from_slice(&size.to_le_bytes());
        entry
    }

    fn long_record(name: &str, short: &[u8; 32]) -> [u8; 32] {
        let mut long = [0xff; 32];
        long[0] = 0x41;
        long[11] = 15;
        long[12] = 0;
        long[13] = short[..11]
            .iter()
            .fold(0u8, |a, b| a.rotate_right(1).wrapping_add(*b));
        long[26..28].fill(0);
        let units = name
            .encode_utf16()
            .chain([0])
            .chain(std::iter::repeat(0xffff));
        for (at, c) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
            .into_iter()
            .zip(units)
        {
            long[at..at + 2].copy_from_slice(&c.to_le_bytes());
        }
        long
    }

    #[test]
    fn repacks_fragmented_files_and_retains_names_dates_attributes_and_empty_folders() {
        use disk_core::MIB;
        let mut image = Cursor::new(vec![0; (12 * MIB) as usize]);
        Format::new(FatType::Fat16, 12 * MIB, 512, 2048, "SOURCE", 41, 2048)
            .unwrap()
            .format(&mut image, |_, _| Ok(()))
            .unwrap();
        let (src, mut table, _) = inspect(&mut image, 512).unwrap();
        let mut short = record(b"ARCHIV~1BIN", 0x27, 30, 2119);
        short[13..20].copy_from_slice(&[9, 0, 0x56, 0x48, 0x57, 0x49, 0x57]);
        short[22..26].copy_from_slice(&[1, 2, 0x49, 0x57]);
        let long = long_record("测试資料.bin", &short);
        for (first, next) in [(8, 0xffff), (13, 0xffff), (30, 18), (18, 0xffff)] {
            table.set_entry(first, next);
        }
        let fat = src.fat as usize;
        let mirror = (src.fat + src.fat_len) as usize;
        let root = src.root_fixed.unwrap().0 as usize;
        let data = image.get_mut();
        for at in [fat, mirror] {
            data[at..at + table.bytes().len()].copy_from_slice(table.bytes());
        }
        data[root + 32..root + 64].copy_from_slice(&record(b"FOLDER     ", 16, 8, 0));
        data[root + 64] = 0xe5; // Existing deleted slot never appears in the target.
        data[root + 65..root + 96].fill(0xa7);
        let nested = src.offset(8).unwrap() as usize;
        let empty = src.offset(13).unwrap() as usize;
        for (at, records) in [
            (
                nested,
                vec![
                    record(b".          ", 16, 8, 0),
                    record(b"..         ", 16, 0, 0),
                    long,
                    short,
                    record(b"EMPTY      ", 16, 13, 0),
                ],
            ),
            (
                empty,
                vec![
                    record(b".          ", 16, 13, 0),
                    record(b"..         ", 16, 8, 0),
                ],
            ),
        ] {
            for (i, record) in records.iter().enumerate() {
                data[at + i * 32..at + i * 32 + 32].copy_from_slice(record);
            }
        }
        let first = src.offset(30).unwrap() as usize;
        let last = src.offset(18).unwrap() as usize;
        data[first..first + 2048].fill(41);
        data[last..last + 2048].fill(0xe7);
        data[last..last + 71].fill(42);
        let cancel = AtomicBool::new(false);
        let plan = prepare(&mut image, 512, MIB, 16 * MIB, 42, &cancel).unwrap();
        assert_eq!(
            (
                plan.source_format,
                plan.destination_format,
                plan.files,
                plan.directories
            ),
            ("FAT16", "FAT12", 1, 2)
        );
        let mut bytes = vec![];
        plan.reader(&mut image).read_to_end(&mut bytes).unwrap();
        let mut target = Cursor::new(bytes);
        plan.verify(&mut target).unwrap();
        let (dst, fat, _) = inspect(&mut target, 512).unwrap();
        assert_eq!(&target.get_ref()[28..32], &(16 * 2048u32).to_le_bytes());
        let root = dst.directory(&mut target, 0, false, 0).unwrap();
        assert!(root[64..].iter().all(|&b| b == 0));
        let folder = u32::from(u16at(&root, 58));
        let records = dst.directory(&mut target, folder, false, 0).unwrap();
        assert_eq!(records[64..96], long);
        let cloned = &records[96..128];
        assert_eq!(&cloned[..26], &short[..26]);
        assert_eq!(&cloned[28..], &short[28..]);
        let start = u32::from(u16at(cloned, 26));
        assert_eq!(fat.entry(start), start + 1);
        assert_eq!(fat.entry(start + 1), start + 2);
        assert_eq!(fat.entry(start + 2), start + 3);
        assert_eq!(fat.entry(start + 3), start + 4);
        assert!(dst.end(fat.entry(start + 4)));
        let content = dst.offset(start).unwrap() as usize;
        let output = &target.get_ref()[content..];
        assert_eq!(&output[..2048], &[41; 2048]);
        assert_eq!(&output[2048..2119], &[42; 71]);
        assert!(output[2119..].iter().all(|&b| b == 0));
        let empty = u32::from(u16at(&records, 154));
        let records = dst.directory(&mut target, empty, false, 0).unwrap();
        assert_eq!(u32::from(u16at(&records, 58)), folder);
        assert!(records[64..].iter().all(|&b| b == 0));
        assert!(prepare(&mut image, 512, MIB - 512, 0, 42, &cancel).is_err());
        image.get_mut()[nested + 64 + 13] ^= 1;
        assert!(prepare(&mut image, 512, MIB, 0, 42, &cancel).is_err());
    }

    #[test]
    fn rebuilds_fat32_on_4kn_with_mirrored_boot_and_free_count() {
        use disk_core::MIB;
        let size = 288 * MIB;
        let from = Format::new(FatType::Fat32, size, 4096, 4096, "SOURCE", 8, 256).unwrap();
        let source = Plan {
            spans: vec![Span {
                offset: 0,
                content: Content::Metadata(from.initialized_prefix(METADATA_LIMIT).unwrap()),
            }],
            size,
            sector: 4096,
            proof: [0; 32],
            target_proof: [0; 32],
            source_format: "FAT32",
            destination_format: "FAT32",
            files: 0,
            directories: 0,
        };
        let mut source_reader = source.reader(Cursor::new([]));
        let plan = prepare(
            &mut source_reader,
            4096,
            272 * MIB,
            4 * MIB,
            9,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            (plan.source_format, plan.destination_format),
            ("FAT32", "FAT32")
        );
        let mut output = plan.reader(source_reader);
        let (fs, table, _) = inspect(&mut output, 4096).unwrap();
        assert!(fs.end(table.entry(2)));
        let fsinfo = at(&mut output, 4096, 4096).unwrap();
        assert_eq!(fsinfo, at(&mut output, 7 * 4096, 4096).unwrap());
        assert_eq!(u32at(&fsinfo, 488), fs.count - 1);
        assert_eq!(u32at(&fsinfo, 492), 3);
        plan.verify(&mut output).unwrap();
    }
}

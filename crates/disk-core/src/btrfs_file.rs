use super::compression::{Codec, LIMIT as COMPRESSION_LIMIT};
use super::*;
use std::io::Write;

pub(super) const EXTENT_DATA: u8 = 108;
const CSUM_ITEM: u8 = 128;
const CSUM_OWNER: u64 = 7;
const CSUM_OBJECT: u64 = u64::MAX - 9;
const EXTENT_ITEM: u8 = 168;
const EXTENT_OWNER: u64 = 2;
const WINDOW: usize = 1024 * 1024;
const MAX_EXTENTS: usize = 131_072;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u64,
    pub generation: u64,
    pub mode: u32,
}

enum Extent {
    Inline(Vec<u8>),
    Data {
        allocation: (u64, u64),
        start: u64,
        length: u64,
        compressed: Option<Compressed>,
    },
    Hole(u64),
}

struct Compressed {
    codec: Codec,
    decoded: usize,
    offset: usize,
}

impl Extent {
    fn length(&self) -> u64 {
        match self {
            Self::Inline(bytes) => bytes.len() as u64,
            Self::Data { length, .. } | Self::Hole(length) => *length,
        }
    }

    fn parse(item: &[u8], sector: u64) -> Result<Self, String> {
        let ram_bytes = le64(item, 8)?;
        let encoding = word::<4>(item, 16)?;
        if encoding[1..] != [0; 3] {
            return Err("Btrfs 加密或其他编码的文件范围尚不能读取。".into());
        }
        let codec = Codec::from_byte(encoding[0])?;
        let kind = word::<1>(item, 20)?[0];
        if kind == 0 {
            let data = &item[21..];
            if ram_bytes > sector || (codec.is_none() && data.len() as u64 != ram_bytes) {
                return Err("Btrfs 内联文件范围长度无效。".into());
            }
            let bytes = if let Some(codec) = codec {
                codec.decode(data, ram_bytes as usize, sector as usize, true)?
            } else {
                data.to_vec()
            };
            return Ok(Self::Inline(bytes));
        }
        if !matches!(kind, 1 | 2) || item.len() != 53 {
            return Err("Btrfs 文件范围类型或长度无效。".into());
        }
        let disk = le64(item, 21)?;
        let disk_len = le64(item, 29)?;
        let offset = le64(item, 37)?;
        let length = le64(item, 45)?;
        if length == 0
            || length % sector != 0
            || [disk, disk_len, offset].iter().any(|n| n % sector != 0)
            || disk.checked_add(disk_len).is_none()
            || (disk == 0 && (kind != 1 || disk_len != 0 || offset != 0))
        {
            return Err("Btrfs 文件范围越界或未对齐。".into());
        }
        if let Some(codec) = codec {
            let bound = COMPRESSION_LIMIT as u64;
            if kind != 1
                || disk == 0
                || disk_len == 0
                || disk_len > bound
                || ram_bytes > bound
                || ram_bytes % sector != 0
                || offset.checked_add(length).is_none_or(|end| end > ram_bytes)
            {
                return Err("Btrfs 压缩文件范围越界或未对齐。".into());
            }
            return Ok(Self::Data {
                allocation: (disk, disk_len),
                start: disk,
                length,
                compressed: Some(Compressed {
                    codec,
                    decoded: ram_bytes as usize,
                    offset: offset as usize,
                }),
            });
        }
        if disk != 0 && offset.checked_add(length).is_none_or(|end| end > disk_len) {
            return Err("Btrfs 文件范围越界。".into());
        }
        if disk == 0 || kind == 2 {
            Ok(Self::Hole(length))
        } else {
            Ok(Self::Data {
                allocation: (disk, disk_len),
                start: disk + offset,
                length,
                compressed: None,
            })
        }
    }
}

impl<R: Read + Seek> Volume<R> {
    /// Stream a committed, checksummed file or a preview prefix.
    /// Return the full inode size and its identity so the caller can verify
    /// that a preview or export still refers to the expected inode.
    pub fn read_file(
        &mut self,
        tree: impl Into<Tree>,
        inode: u64,
        limit: u64,
        output: &mut impl Write,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<FileInfo, String> {
        let tree = tree.into();
        let trees = self.trees(tree)?;
        let root = *trees
            .roots
            .get(&tree.subvolume)
            .ok_or("Btrfs 子卷不存在。")?;
        let (extent_root, checksum_root, committed) =
            (trees.extent, trees.checksum, trees.generation);
        let sector = u64::from(self.superblock.sector_size);
        let first = Key {
            object_id: inode,
            kind: INODE_ITEM,
            offset: 0,
        };
        let last = Key {
            object_id: inode,
            kind: EXTENT_DATA,
            offset: u64::MAX,
        };
        let mut info = None;
        let mut extents = Vec::new();
        let mut extent_end = 0u64;
        scan_tree(
            &mut self.reader,
            &self.superblock,
            &self.mapping,
            root,
            tree.subvolume,
            Some((first, last)),
            |key, item| {
                if key == first {
                    let record = Inode::parse(item, committed)?;
                    if record.mode() & 0xf000 != 0x8000 {
                        return Err("Btrfs 指定 inode 不是普通文件。".into());
                    }
                    if le64(item, 64)? & 1 != 0 {
                        return Err("Btrfs 文件禁用了数据校验，当前无法提取。".into());
                    }
                    info = Some(FileInfo {
                        size: record.size(),
                        generation: le64(item, 0)?,
                        mode: record.mode(),
                    });
                } else if key.kind == EXTENT_DATA {
                    let file = info.as_ref().ok_or("Btrfs 文件范围缺少 inode。")?;
                    if key.offset >= file.size.min(limit) {
                        return Ok(());
                    }
                    if extents.len() >= MAX_EXTENTS
                        || key.offset < extent_end
                        || key.offset % sector != 0
                    {
                        return Err("Btrfs 文件范围重叠、未对齐或超过条目上限。".into());
                    }
                    if !(1..=committed).contains(&le64(item, 0)?) {
                        return Err("Btrfs 文件范围的代次无效。".into());
                    }
                    let extent = Extent::parse(item, sector)?;
                    if (matches!(extent, Extent::Inline(_))
                        && (key.offset != 0 || extent.length() != file.size))
                        || extents
                            .iter()
                            .take(1)
                            .any(|(_, prior)| matches!(prior, Extent::Inline(_)))
                    {
                        return Err("Btrfs 内联范围与文件布局不一致。".into());
                    }
                    extent_end = key
                        .offset
                        .checked_add(extent.length())
                        .ok_or("Btrfs 文件偏移溢出。")?;
                    extents.push((key.offset, extent));
                }
                Ok(())
            },
            &mut checkpoint,
        )?;
        let info = info.ok_or("未找到 Btrfs 文件 inode。")?;
        let bytes = info.size.min(limit);
        if bytes == 0 {
            return Ok(info);
        }
        let mut checked = HashSet::new();
        for (_, extent) in &extents {
            if let Extent::Data { allocation, .. } = extent {
                if checked.insert(*allocation) {
                    self.verify_allocation(extent_root, committed, *allocation, &mut checkpoint)?;
                }
            }
        }
        let mut position = 0;
        let mut buffer = vec![0; WINDOW];
        for (offset, extent) in extents {
            checkpoint()?;
            if offset > position {
                write_zeroes(output, &mut buffer, offset - position, &mut checkpoint)?;
            }
            let length = extent.length().min(bytes - offset);
            match extent {
                Extent::Inline(data) => output.write_all(&data[..length as usize]).map_err(err)?,
                Extent::Hole(_) => write_zeroes(output, &mut buffer, length, &mut checkpoint)?,
                Extent::Data {
                    start,
                    allocation,
                    compressed: Some(packed),
                    ..
                } => {
                    let packed_bytes = &mut buffer[..allocation.1 as usize];
                    self.read_checked_data(checksum_root, start, packed_bytes, &mut checkpoint)?;
                    let bytes = packed.codec.decode(
                        packed_bytes,
                        packed.decoded,
                        sector as usize,
                        false,
                    )?;
                    output
                        .write_all(&bytes[packed.offset..packed.offset + length as usize])
                        .map_err(err)?;
                }
                Extent::Data { start, .. } => {
                    let mut copied = 0;
                    while copied < length {
                        checkpoint()?;
                        let count = (length - copied).min(WINDOW as u64);
                        let aligned = count.div_ceil(sector) * sector;
                        self.read_checked_data(
                            checksum_root,
                            start + copied,
                            &mut buffer[..aligned as usize],
                            &mut checkpoint,
                        )?;
                        output.write_all(&buffer[..count as usize]).map_err(err)?;
                        copied += count;
                    }
                }
            }
            position = offset + length;
        }
        write_zeroes(output, &mut buffer, bytes - position, &mut checkpoint)?;
        Ok(info)
    }

    fn verify_allocation(
        &mut self,
        root: Option<BlockRef>,
        committed: u64,
        (start, length): (u64, u64),
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let root = root.ok_or("Btrfs 缺少文件范围分配树。")?;
        let key = Key {
            object_id: start,
            kind: EXTENT_ITEM,
            offset: length,
        };
        let mut valid = false;
        scan_tree(
            &mut self.reader,
            &self.superblock,
            &self.mapping,
            root,
            EXTENT_OWNER,
            Some((key, key)),
            |_, item| {
                let generation = le64(item, 8)?;
                valid = le64(item, 0)? > 0
                    && (1..=committed).contains(&generation)
                    && le64(item, 16)? == 1;
                Ok(())
            },
            checkpoint,
        )?;
        if !valid {
            return Err("Btrfs 文件范围缺少当前有效的数据分配记录。".into());
        }
        Ok(())
    }

    fn read_checked_data(
        &mut self,
        root: Option<BlockRef>,
        logical: u64,
        buffer: &mut [u8],
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let sb = &self.superblock;
        let sector = u64::from(sb.sector_size);
        let end = logical
            .checked_add(buffer.len() as u64)
            .ok_or("Btrfs 数据范围溢出。")?;
        let root = root.ok_or("Btrfs 缺少文件校验树。")?;
        let checksum_len = sb.algorithm.size();
        let first = Key {
            object_id: CSUM_OBJECT,
            kind: CSUM_ITEM,
            offset: logical.saturating_sub((sb.node_size as usize / checksum_len) as u64 * sector),
        };
        let last = Key {
            object_id: CSUM_OBJECT,
            kind: CSUM_ITEM,
            offset: end - 1,
        };
        let mut expected = vec![None; buffer.len() / sector as usize];
        scan_tree(
            &mut self.reader,
            sb,
            &self.mapping,
            root,
            CSUM_OWNER,
            Some((first, last)),
            |key, payload| {
                if key.offset % sector != 0
                    || payload.is_empty()
                    || payload.len() % checksum_len != 0
                {
                    return Err("Btrfs 校验条目长度或位置无效。".into());
                }
                for (i, digest) in payload.chunks_exact(checksum_len).enumerate() {
                    let at = key
                        .offset
                        .checked_add(i as u64 * sector)
                        .ok_or("Btrfs 校验范围溢出。")?;
                    if (logical..end).contains(&at) {
                        let slot = ((at - logical) / sector) as usize;
                        let mut stored = [0; 32];
                        stored[..checksum_len].copy_from_slice(digest);
                        if expected[slot].replace(stored).is_some() {
                            return Err("Btrfs 数据扇区的校验条目重叠。".into());
                        }
                    }
                }
                Ok(())
            },
            &mut checkpoint,
        )?;
        if expected.iter().any(Option::is_none) {
            return Err("Btrfs 部分数据缺少可核对的校验值。".into());
        }
        let mut pos = 0;
        while pos < buffer.len() {
            checkpoint()?;
            let at = logical + pos as u64;
            let (&start, chunk) = self
                .mapping
                .range(..=at)
                .next_back()
                .ok_or("Btrfs 数据块缺少块组映射。")?;
            let within = at - start;
            if within >= chunk.length || chunk.kind & 1 == 0 {
                return Err("Btrfs 文件范围位于错误类型的块组。".into());
            }
            let limit = (chunk.length - within).min((buffer.len() - pos) as u64) as usize;
            let run = chunk.run(within, limit);
            let count = run.len;
            let dest = &mut buffer[pos..pos + count];
            let hashes = &expected[pos / sector as usize..(pos + count) / sector as usize];
            let verified = |bytes: &[u8], hashes: &[Option<[u8; 32]>]| {
                bytes
                    .chunks_exact(sector as usize)
                    .zip(hashes)
                    .all(|(bytes, expected)| {
                        expected
                            .as_ref()
                            .is_some_and(|expected| checksum_matches(sb.algorithm, bytes, expected))
                    })
            };
            let mut direct_read = false;
            let mut valid = run.stripes.iter().any(|&stripe| {
                direct_read = self.reader.read(stripe, run.delta, dest).is_ok();
                direct_read && verified(dest, hashes)
            });
            if let Some(parity) = run.parity.filter(|_| !valid) {
                let mut rebuild = |delta, dest: &mut [u8], hashes: &[Option<[u8; 32]>]| {
                    for prefer_q in [false, true] {
                        if let Some(bytes) =
                            parity.rebuild(&mut self.reader, delta, dest.len(), prefer_q)
                        {
                            if verified(&bytes, hashes) {
                                dest.copy_from_slice(&bytes);
                                return true;
                            }
                        }
                    }
                    false
                };
                valid = rebuild(run.delta, dest, hashes);
                if !valid && hashes.len() > 1 {
                    // Within a stripe, healthy sectors may use the direct
                    // bytes and damaged sectors may need different parity.
                    valid = true;
                    for (i, (part, hash)) in dest
                        .chunks_exact_mut(sector as usize)
                        .zip(hashes)
                        .enumerate()
                    {
                        checkpoint()?;
                        let hash = std::slice::from_ref(hash);
                        if (!direct_read || !verified(part, hash))
                            && !rebuild(run.delta + i as u64 * sector, part, hash)
                        {
                            valid = false;
                            break;
                        }
                    }
                }
            }
            if !valid {
                return Err(format!(
                    "Btrfs 文件数据读取失败或 {} 不一致。",
                    sb.algorithm.to_string().to_uppercase()
                ));
            }
            pos += count;
        }
        Ok(())
    }
}

fn write_zeroes(
    output: &mut impl Write,
    buffer: &mut [u8],
    mut length: u64,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if length != 0 {
        buffer.fill(0);
    }
    while length > 0 {
        checkpoint()?;
        let count = length.min(buffer.len() as u64) as usize;
        output.write_all(&buffer[..count]).map_err(err)?;
        length -= count as u64;
    }
    Ok(())
}

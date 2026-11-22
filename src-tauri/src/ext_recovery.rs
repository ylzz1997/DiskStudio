//! Read-only Ext directory traversal/extraction over an already bounded source.
//! No mounts, journal replay, host paths, device discovery or writes to source.
use crate::{
    recovery::{Candidate, Locator, ScanMode, MAX_RESULTS},
    recovery_tree::{Directory, MAX_DEPTH, ROOT},
};
use ext4_view::{Ext4, Ext4Read, Metadata};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{Read, Seek, SeekFrom, Write},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn crc(seed: u32, bytes: &[u8]) -> u32 {
    // Reflected CRC32C, with the ext filesystem's uncomplemented final value.
    let mut value = seed;
    for &byte in bytes {
        value ^= byte as u32;
        for _ in 0..8 {
            value = (value >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(value & 1));
        }
    }
    value
}
struct Source<R> {
    reader: R,
    length: u64,
    cancel: Arc<AtomicBool>,
    reads: u64,
}
impl<R: Read + Seek> Source<R> {
    fn read_at(&mut self, at: u64, bytes: &mut [u8]) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("Ext 扫描或提取已取消。".into());
        }
        if bytes.len() > 1024 * 1024
            || at
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > self.length)
        {
            return Err("Ext 元数据请求超出已验证的源范围。".into());
        }
        self.reads += 1;
        if self.reads > 100_000_000 {
            return Err("Ext 读取次数达到上限，已停止。".into());
        }
        self.reader.seek(SeekFrom::Start(at)).map_err(err)?;
        self.reader.read_exact(bytes).map_err(err)
    }
}
struct Adapter<R>(Rc<RefCell<Source<R>>>);
impl<R: Read + Seek> Ext4Read for Adapter<R> {
    fn read(
        &mut self,
        at: u64,
        bytes: &mut [u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0
            .borrow_mut()
            .read_at(at, bytes)
            .map_err(|e| Box::new(std::io::Error::other(e)) as _)
    }
}
#[derive(Clone)]
struct Geometry {
    creator_os: u32,
    fingerprint: [u8; 32],
    block: u64,
    size: u64,
    inodes: u32,
    inode_size: usize,
    first_normal_inode: u32,
    journal_inode: u32,
    uuid: [u8; 16],
    per_group: u32,
    descriptors: Vec<u8>,
    descriptor_size: usize,
    metadata_csum: bool,
    checksum_seed: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SuperblockLayout {
    block: u64,
    size: u64,
    inodes: u32,
    first: u64,
    bpg: u64,
    per_group: u32,
    groups: u64,
    descriptor_size: usize,
    metadata_csum: bool,
    checksum_seed: u32,
}

pub(crate) fn validate_existing<R: Read + Seek>(mut reader: R) -> Result<(), String> {
    let length = reader.seek(SeekFrom::End(0)).map_err(err)?;
    geometry(&mut Source { reader, length, cancel: Arc::new(AtomicBool::new(false)), reads: 0 }).map(|_| ())
}

fn superblock_layout(s: &[u8; 1024], length: u64) -> Result<SuperblockLayout, String> {
    let inc = u32le(s, 0x60);
    let ro = u32le(s, 0x64);
    if inc & !(0x2 | 0x40 | 0x80 | 0x200 | 0x2000 | 0x10000) != 0
        || inc & 2 == 0
        || ro & !(0x1 | 0x2 | 0x4 | 0x8 | 0x20 | 0x40 | 0x100 | 0x400) != 0
    {
        return Err(format!(
            "Ext 文件系统包含当前未支持的特性（incompat={inc:#x}, ro_compat={ro:#x}）。"
        ));
    }
    let log = u32le(s, 0x18);
    if log > 2 || u32le(s, 0x1c) != log {
        return Err("Ext 读取当前支持 1/2/4 KiB 块。".into());
    }
    let block = 1024u64 << log;
    let blocks = u32le(s, 4) as u64 | ((u32le(s, 0x150) as u64) << 32);
    let size = blocks.checked_mul(block).ok_or("Ext 容量溢出。")?;
    let first = u32le(s, 0x14) as u64;
    let bpg = u32le(s, 0x20) as u64;
    let per_group = u32le(s, 0x28);
    let inodes = u32le(s, 0);
    let inode_size = u16le(s, 0x58) as u64;
    if size > length
        || size < 4096
        || first != u64::from(block == 1024)
        || bpg == 0
        || bpg > block * 8
        || per_group < 8
        || per_group as u64 > block * 8
        || inodes < 2
        || inodes > 16_000_000
        || inode_size < 128
        || inode_size > block
        || !inode_size.is_power_of_two()
        || (ro & 0x400 != 0 && inode_size < 256)
        || (inc & 0x80 == 0 && u32le(s, 0x150) != 0)
    {
        return Err("Ext 容量、inode 或块组几何无效或超过支持范围。".into());
    }
    let groups = (blocks - first).div_ceil(bpg);
    let descriptor_size = if inc & 0x80 != 0 {
        u16le(s, 0xfe) as usize
    } else {
        32
    };
    if groups == 0
        || groups > 65536
        || inodes as u64 > groups * per_group as u64
        || ![32, 64].contains(&descriptor_size)
        || (inc & 0x80 != 0 && descriptor_size != 64)
    {
        return Err("Ext 块组数量或描述符大小不支持。".into());
    }
    Ok(SuperblockLayout {
        block,
        size,
        inodes,
        first,
        bpg,
        per_group,
        groups,
        descriptor_size,
        metadata_csum: ro & 0x400 != 0,
        checksum_seed: if inc & 0x2000 != 0 {
            u32le(s, 0x270)
        } else {
            crc(!0, &s[0x68..0x78])
        },
    })
}

fn geometry<R: Read + Seek>(source: &mut Source<R>) -> Result<Geometry, String> {
    let mut s = [0; 1024];
    source.read_at(1024, &mut s)?;
    if u16le(&s, 0x38) != 0xef53 {
        return Err("不是可识别的 Ext 文件系统。".into());
    }
    if u16le(&s, 0x3a) != 1 || u32le(&s, 0x60) & 4 != 0 || u32le(&s, 0xe8) != 0 {
        return Err("Ext 文件系统未干净卸载或需要日志恢复；只读扫描不会重放日志。".into());
    }
    let layout = superblock_layout(&s, source.length)?;
    source.length = layout.size;
    let mut descriptors = vec![0; layout.groups as usize * layout.descriptor_size];
    let start = (layout.first + 1) * layout.block;
    for (i, chunk) in descriptors.chunks_mut(1024 * 1024).enumerate() {
        source.read_at(start + i as u64 * 1024 * 1024, chunk)?;
    }
    let mut hash = Sha256::new();
    hash.update(s);
    hash.update(&descriptors);
    Ok(Geometry {
        creator_os: u32le(&s, 0x48),
        fingerprint: hash.finalize().into(),
        block: layout.block,
        size: layout.size,
        inodes: layout.inodes,
        inode_size: usize::from(u16le(&s, 0x58)),
        first_normal_inode: u32le(&s, 0x54),
        journal_inode: if u32le(&s, 0x5c) & 4 != 0 { u32le(&s, 0xe0) } else { 0 },
        uuid: s[0x68..0x78].try_into().unwrap(),
        per_group: layout.per_group,
        descriptors,
        descriptor_size: layout.descriptor_size,
        metadata_csum: layout.metadata_csum,
        checksum_seed: layout.checksum_seed,
    })
}

#[path = "ext_reconstruct.rs"]
pub(crate) mod reconstruct;
#[path = "ext_journal.rs"]
mod journal;
#[path = "ext_deleted.rs"]
pub(crate) mod deleted;
struct Session<R> {
    fs: Ext4,
    source: Rc<RefCell<Source<R>>>,
    geometry: Geometry,
    bitmaps: HashMap<u32, Vec<u8>>,
}
impl<R: Read + Seek + 'static> Session<R> {
    fn new(mut reader: R, cancel: Arc<AtomicBool>) -> Result<Self, String> {
        let length = reader.seek(SeekFrom::End(0)).map_err(err)?;
        let mut source = Source {
            reader,
            length,
            cancel,
            reads: 0,
        };
        let geometry = geometry(&mut source)?;
        let source = Rc::new(RefCell::new(source));
        let fs = Ext4::load(Box::new(Adapter(source.clone())))
            .map_err(|e| format!("Ext 元数据校验失败：{e}"))?;
        Ok(Self {
            fs,
            source,
            geometry,
            bitmaps: HashMap::new(),
        })
    }
    fn inode_allocated(&mut self, inode: u32) -> Result<bool, String> {
        if inode == 0 || inode > self.geometry.inodes {
            return Err("Ext inode 超出范围。".into());
        }
        let group = (inode - 1) / self.geometry.per_group;
        let within = (inode - 1) % self.geometry.per_group;
        if !self.bitmaps.contains_key(&group) {
            let at = group as usize * self.geometry.descriptor_size;
            let d = self
                .geometry
                .descriptors
                .get(at..at + self.geometry.descriptor_size)
                .ok_or("Ext inode 块组不存在。")?;
            if u16le(d, 0x12) & 1 != 0 {
                return Err("Ext 目录引用未初始化的 inode 块组。".into());
            }
            let block = u32le(d, 4) as u64
                | if d.len() == 64 {
                    (u32le(d, 0x24) as u64) << 32
                } else {
                    0
                };
            if block == 0 || block >= self.geometry.size / self.geometry.block {
                return Err("Ext inode 位图地址无效。".into());
            }
            let mut bitmap = vec![0; self.geometry.block as usize];
            self.source
                .borrow_mut()
                .read_at(block * self.geometry.block, &mut bitmap)?;
            if self.geometry.metadata_csum {
                let actual = crc(
                    self.geometry.checksum_seed,
                    &bitmap[..self.geometry.per_group.div_ceil(8) as usize],
                );
                let expected = u16le(d, 0x1a) as u32
                    | if d.len() == 64 {
                        (u16le(d, 0x3a) as u32) << 16
                    } else {
                        0
                    };
                if (if d.len() == 64 {
                    actual
                } else {
                    actual & 0xffff
                }) != expected
                {
                    return Err("Ext inode 位图校验失败。".into());
                }
            }
            if self.bitmaps.len() >= 4096 {
                self.bitmaps.clear();
            }
            self.bitmaps.insert(group, bitmap);
        }
        Ok(self.bitmaps[&group][within as usize / 8] & (1 << (within % 8)) != 0)
    }

    fn allocated(&mut self, metadata: &Metadata) -> Result<(), String> {
        if !self.inode_allocated(metadata.inode_number())? {
            return Err("Ext 目录引用已释放的 inode。".into());
        }
        let record = metadata.inode_record();
        if u32le(record, 0x14) != 0 || u16le(record, 0x1a) == 0 {
            return Err("Ext inode 已删除或没有有效链接。".into());
        }
        Ok(())
    }
    fn prove(&mut self, proof: &Proof) -> Result<(), String> {
        let metadata = self
            .fs
            .symlink_metadata(proof.path.as_slice())
            .map_err(err)?;
        if metadata.inode_number() != proof.inode
            || Sha256::digest(metadata.inode_record()).as_slice() != proof.record
        {
            return Err("Ext 目录或文件记录已改变，请重新扫描。".into());
        }
        self.allocated(&metadata)
    }
}
#[derive(Clone, Debug)]
struct Proof {
    path: Vec<u8>,
    inode: u32,
    record: [u8; 32],
}
#[derive(Clone, Debug)]
struct Ancestor {
    proof: Proof,
    parent: Option<Arc<Ancestor>>,
    depth: usize,
}
#[derive(Clone)]
pub struct Location {
    record: Vec<u8>,
    entry: usize,
    filesystem: [u8; 32],
    proof: Proof,
    parent: Arc<Ancestor>,
}
#[derive(Clone, Debug)]
pub(crate) struct DirectoryLocation {
    filesystem: [u8; 32],
    node: Arc<Ancestor>,
}
pub(crate) fn directory_metadata<R: Read + Seek + 'static>(
    reader: R,
    location: &DirectoryLocation,
    name: &str,
    cancel: Arc<AtomicBool>,
) -> Result<crate::file_metadata::Metadata, String> {
    let mut session = Session::new(reader, cancel)?;
    if session.geometry.fingerprint != location.filesystem {
        return Err("Ext 文件系统已变化。".into());
    }
    let mut chain = vec![];
    let mut current = Some(location.node.as_ref());
    while let Some(node) = current {
        chain.push(node);
        current = node.parent.as_deref();
        if chain.len() > MAX_DEPTH {
            return Err("Ext 目录凭据过深。".into());
        }
    }
    for node in chain.iter().rev() {
        session.prove(&node.proof)?;
    }
    let proof = &location.node.proof;
    if String::from_utf8_lossy(proof.path.rsplit(|b| *b == b'/').next().unwrap_or_default()) != name
    {
        return Err("Ext 目录名称与凭据不符。".into());
    }
    let metadata = session
        .fs
        .symlink_metadata(proof.path.as_slice())
        .map_err(err)?;
    if !metadata.is_dir() {
        return Err("Ext 目录类型已变化。".into());
    }
    Ok(crate::file_metadata::ext(
        metadata.inode_record(),
        session.geometry.creator_os == 0,
    ))
}

#[path = "ext_scan.rs"]
pub mod scan_progress;

pub fn scan<R: Read + Seek + 'static>(
    reader: R,
    mode: ScanMode,
    cancel: Arc<AtomicBool>,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    if mode == ScanMode::Deleted {
        return deleted::scan(reader, cancel, tick, found, directory);
    }
    scan_progress::continue_scan(
        reader,
        mode,
        &mut Default::default(),
        cancel,
        tick,
        found,
        directory,
    )
}

pub fn extract<R: Read + Seek + 'static, W: Write>(
    reader: R,
    c: &Candidate,
    location: &Location,
    out: &mut W,
    limit: u64,
    cancel: Arc<AtomicBool>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    extract_with_metadata(reader, c, location, out, limit, cancel, tick).map(|e| e.bytes)
}
pub fn extract_with_metadata<R: Read + Seek + 'static, W: Write>(
    reader: R,
    c: &Candidate,
    location: &Location,
    out: &mut W,
    limit: u64,
    cancel: Arc<AtomicBool>,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<crate::recovery::Extraction, String> {
    tick(0)?;
    if !c.supported || c.deleted {
        return Err("不支持此 Ext 候选的提取。".into());
    }
    let mut session = Session::new(reader, cancel.clone())?;
    if session.geometry.fingerprint != location.filesystem {
        return Err("Ext 文件系统元数据已改变，请重新扫描。".into());
    }
    let mut chain = Vec::new();
    let mut current = Some(location.parent.as_ref());
    while let Some(node) = current {
        chain.push(node);
        current = node.parent.as_deref();
    }
    for ancestor in chain.iter().rev() {
        session.prove(&ancestor.proof)?;
    }
    session.prove(&location.proof)?;
    let mut file = session
        .fs
        .open(location.proof.path.as_slice())
        .map_err(err)?;
    if file.metadata().len() != c.size
        || file.metadata().inode_number() != location.proof.inode
        || Sha256::digest(file.metadata().inode_record()).as_slice() != location.proof.record
    {
        return Err("Ext 文件数据属性已经变化。".into());
    }
    let metadata = crate::file_metadata::ext(
        file.metadata().inode_record(),
        session.geometry.creator_os == 0,
    );
    let take = c.size.min(limit);
    let mut done = 0;
    let mut buffer = vec![0; 1024 * 1024];
    while done < take {
        tick(done)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Ext 提取已取消。".into());
        }
        let n = file
            .read_bytes(&mut buffer[..(take - done).min(1024 * 1024) as usize])
            .map_err(err)?;
        if n == 0 {
            return Err("Ext 文件数据提前结束。".into());
        }
        out.write_all(&buffer[..n]).map_err(err)?;
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
    use crate::recovery_tree::Catalog;
    use std::{io::Cursor, sync::OnceLock};
    pub(super) fn fixture(kind: usize) -> Arc<[u8]> {
        static IMAGES: [OnceLock<Arc<[u8]>>; 3] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new()];
        IMAGES[kind]
            .get_or_init(|| {
                let compressed: &[u8] = match kind {
                    0 => include_bytes!("../../tests/fixtures/test_disk_ext2.bin.zst"),
                    1 => include_bytes!("../../tests/fixtures/test_disk_ext3.bin.zst"),
                    _ => include_bytes!("../../tests/fixtures/test_disk1.bin.zst"),
                };
                let mut bytes = vec![];
                zstd::stream::read::Decoder::new(compressed)
                    .unwrap()
                    .take(256 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert!(bytes.len() <= 256 * 1024 * 1024);
                bytes.into()
            })
            .clone()
    }
    fn collect(bytes: Arc<[u8]>) -> (Vec<Candidate>, Catalog) {
        collect_mode(bytes, ScanMode::AllFiles)
    }
    fn collect_mode(bytes: Arc<[u8]>, mode: ScanMode) -> (Vec<Candidate>, Catalog) {
        let mut files = vec![];
        let mut directories = vec![];
        let result = crate::recovery::scan_filesystem_owned(
            Cursor::new(bytes),
            mode,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |c| files.push(c),
            |d| {
                directories.push(d);
                Ok(())
            },
        )
        .unwrap();
        assert!(result.starts_with("Ext scan complete."), "{result}");
        let mut catalog = Catalog::default();
        for d in directories {
            catalog.insert(d).unwrap();
        }
        for c in &files {
            catalog.add_file(c);
        }
        (files, catalog)
    }
    #[test]
    fn browse_existing_ext2_ext3_ext4_keeps_directory_relations_and_file_contents() {
        for kind in 0..3 {
            let bytes = fixture(kind);
            let (files, catalog) = collect_mode(bytes.clone(), ScanMode::Existing);
            assert!(!files.is_empty() && files.iter().all(|c| !c.deleted));
            let file = files
                .iter()
                .find(|c| c.name == if kind == 1 { "42" } else { "small_file" })
                .unwrap();
            assert!(catalog.path(file.directory.as_deref().unwrap()).is_ok());
            let mut out = vec![];
            crate::recovery::extract_owned(
                Cursor::new(bytes),
                file,
                &mut out,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(
                out.as_slice(),
                if kind == 1 {
                    b"42".as_slice()
                } else {
                    b"hello, world!".as_slice()
                }
            );
        }
    }
    #[test]
    fn upstream_ext2_ext3_ext4_images_scan_and_export_known_file_bytes() {
        for kind in 0..3 {
            let bytes = fixture(kind);
            let (files, catalog) = collect(bytes.clone());
            let expected: &[u8] = if kind == 1 { b"42" } else { b"hello, world!" };
            let file = files
                .iter()
                .find(|c| c.name == if kind == 1 { "42" } else { "small_file" })
                .unwrap();
            assert!(!file.deleted && file.supported);
            let mut out = vec![];
            crate::recovery::extract_owned(
                Cursor::new(bytes),
                file,
                &mut out,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(out, expected);
            if kind == 1 {
                assert_eq!(
                    catalog
                        .path(file.directory.as_deref().unwrap())
                        .unwrap()
                        .last()
                        .unwrap()
                        .name,
                    "medium_dir"
                );
            }
            if kind == 2 {
                assert!(catalog.snapshot().iter().any(|d| d.name == "empty_dir"));
            }
        }
    }
    pub(super) fn inode_at(bytes: &[u8], inode: u32) -> usize {
        let s = &bytes[1024..2048];
        let block = 1024usize << u32le(s, 0x18);
        let group = (inode - 1) / u32le(s, 0x28);
        let descriptor = (u32le(s, 0x14) as usize + 1) * block + group as usize * 32;
        u32le(bytes, descriptor + 8) as usize * block
            + ((inode - 1) % u32le(s, 0x28)) as usize * u16le(s, 0x58) as usize
    }
    #[test]
    fn metadata_ext_inode_padding_does_not_create_fake_birth_times() {
        let original = fixture(0);
        let (files, _) = collect(original.clone());
        let c = files.iter().find(|c| c.name == "small_file").unwrap();
        let Locator::Ext(l) = &c.locator else {
            panic!()
        };
        let at = inode_at(&original, l.proof.inode);
        assert!(u16le(&original, 1024 + 0x58) >= 152);
        let mut bytes = original.to_vec();
        bytes[at + 128..at + 130].fill(0);
        bytes[at + 132..at + 152].fill(255);
        bytes[at + 16..at + 20].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[at..at + 2].copy_from_slice(&0o100640u16.to_le_bytes());
        let (files, _) = collect(bytes.clone().into());
        let c = files.iter().find(|c| c.name == "small_file").unwrap();
        assert!(c.metadata.created.is_none());
        assert_eq!(
            c.metadata.modified.as_ref().unwrap().text,
            "1969-12-31 23:59:59"
        );
        assert_eq!(c.metadata.modified.as_ref().unwrap().precision, "1 s");
        assert_eq!(c.metadata.permissions.as_deref(), Some("0640"));
        let mut out = vec![];
        let mut candidate = c.clone();
        candidate.metadata = Default::default();
        let receipt = crate::recovery::extract_owned_with_metadata(
            Cursor::new(bytes.clone()),
            &candidate,
            &mut out,
            u64::MAX,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(receipt.bytes, 13);
        assert_eq!(
            receipt.metadata.modified.unwrap().instant_ns,
            Some(-1_000_000_000)
        );
        assert_eq!(receipt.metadata.permissions.as_deref(), Some("0640"));
        assert!(receipt.metadata.created.is_none());
        assert!(receipt.metadata.windows_flags.is_none());
        assert_eq!(out, b"hello, world!");
        bytes[at + 128..at + 130].copy_from_slice(&24u16.to_le_bytes());
        bytes[at + 132..at + 152].fill(0);
        bytes[at + 136..at + 140].copy_from_slice(&((123456789u32 << 2) | 1).to_le_bytes());
        let (future, _) = collect(bytes.clone().into());
        let future = future.iter().find(|c| c.name == "small_file").unwrap();
        assert_eq!(
            future.metadata.modified.as_ref().unwrap().text,
            "2106-02-07 06:28:15.123456789"
        );
        bytes[at + 128..at + 130].copy_from_slice(&2u16.to_le_bytes());
        assert!(scan(
            Cursor::new(bytes),
            ScanMode::Existing,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |_| {},
            |_| Ok(())
        )
        .is_err());
    }
    pub(super) fn root_name_at(bytes: &[u8], name: &[u8]) -> usize {
        let block = 1024usize << u32le(bytes, 1024 + 0x18);
        let root_block = u32le(bytes, inode_at(bytes, 2) + 0x28) as usize * block;
        root_block
            + bytes[root_block..root_block + block]
                .windows(name.len())
                .position(|b| b == name)
                .unwrap()
    }
    #[test]
    fn extraction_revalidates_inode_parent_superblock_and_allocation_before_output() {
        let bytes = fixture(0);
        let (files, _) = collect(bytes.clone());
        let c = files.iter().find(|c| c.name == "small_file").unwrap();
        let Locator::Ext(location) = &c.locator else {
            panic!()
        };
        for variant in 0..4 {
            let mut changed = bytes.to_vec();
            match variant {
                0 => changed[inode_at(&bytes, location.proof.inode) + 0x64] ^= 1,
                1 => changed[inode_at(&bytes, 2) + 0x64] ^= 1,
                2 => changed[1024 + 0x68] ^= 1,
                _ => {
                    let bitmap = u32le(&bytes, 2048 + 4) as usize * 1024;
                    let bit = location.proof.inode - 1;
                    changed[bitmap + bit as usize / 8] &= !(1 << (bit % 8));
                }
            }
            let mut output = vec![];
            assert!(
                crate::recovery::extract_owned(
                    Cursor::new(changed),
                    c,
                    &mut output,
                    u64::MAX,
                    Arc::new(AtomicBool::new(false)),
                    |_| Ok(())
                )
                .is_err(),
                "variant {variant}"
            );
            assert!(output.is_empty());
        }
    }
    #[test]
    fn directory_cycles_and_non_utf8_names_are_handled_without_path_loss() {
        let mut ext3 = fixture(1).to_vec();
        let at = root_name_at(&ext3, b"medium_dir");
        ext3[at - 8..at - 4].copy_from_slice(&2u32.to_le_bytes());
        let result = scan(
            Cursor::new(ext3),
            ScanMode::AllFiles,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |_| {},
            |_| Ok(()),
        );
        assert!(result.unwrap_err().contains("循环"));
        let mut bytes = fixture(0).to_vec();
        let at = root_name_at(&bytes, b"small_file");
        bytes[at] = 0xff;
        let bytes: Arc<[u8]> = bytes.into();
        let (files, _) = collect(bytes.clone());
        let c = files.iter().find(|c| c.name == "�mall_file").unwrap();
        assert!(c.directory_note.contains("UTF-8"));
        let mut output = vec![];
        crate::recovery::extract_owned(
            Cursor::new(bytes),
            c,
            &mut output,
            u64::MAX,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(output, b"hello, world!");
    }
    #[test]
    fn indirect_blocks_sparse_holes_and_preview_limits_preserve_bytes() {
        struct HashWriter {
            hash: Sha256,
            size: u64,
        }
        impl Write for HashWriter {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.hash.update(b);
                self.size += b.len() as u64;
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for kind in [0, 2] {
            let bytes = fixture(kind);
            let (files, _) = collect(bytes.clone());
            let c = files.iter().find(|c| c.name == "holes").unwrap();
            let mut out = vec![];
            crate::recovery::extract_owned(
                Cursor::new(bytes.clone()),
                c,
                &mut out,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            let expected: Vec<u8> = [0, 0, 0xa1, 0xa2, 0, 0, 0xa3, 0xa4, 0, 0]
                .into_iter()
                .flat_map(|b| vec![b; 1024])
                .collect();
            assert_eq!(out, expected);
            let c = files.iter().find(|c| c.name == "small_file").unwrap();
            let mut out = vec![];
            crate::recovery::extract_owned(
                Cursor::new(bytes.clone()),
                c,
                &mut out,
                5,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(out, b"hello");
            if kind == 0 {
                let c = files.iter().find(|c| c.name == "big_file").unwrap();
                let mut out = HashWriter {
                    hash: Sha256::new(),
                    size: 0,
                };
                crate::recovery::extract_owned(
                    Cursor::new(bytes),
                    c,
                    &mut out,
                    u64::MAX,
                    Arc::new(AtomicBool::new(false)),
                    |_| Ok(()),
                )
                .unwrap();
                let blocks = 12 + 256 + 256 * 256 + 256 * 16;
                let mut expected = Sha256::new();
                for i in 0u32..blocks {
                    let mut block = [0; 1024];
                    block[..4].copy_from_slice(&i.to_le_bytes());
                    block[1020..].copy_from_slice(&i.to_le_bytes());
                    expected.update(block);
                }
                assert_eq!(out.size, blocks as u64 * 1024);
                assert_eq!(out.hash.finalize(), expected.finalize());
            }
        }
    }
    #[test]
    fn clean_state_geometry_checksum_and_deleted_mode_fail_closed() {
        let original = fixture(0);
        for variant in 0..5 {
            let mut b = original.to_vec();
            match variant {
                0 => b[1024 + 0x3a] = 0,
                1 => b[1024 + 0x60] |= 4,
                2 => b[1024 + 0x18] = 31,
                3 => b[1024 + 0x28..1024 + 0x2c].fill(0),
                _ => b[1024 + 4..1024 + 8].fill(0xff),
            }
            let e = Session::new(Cursor::new(b), Arc::new(AtomicBool::new(false)))
                .err()
                .unwrap();
            assert!(
                e.contains(if variant < 2 {
                    "未干净卸载"
                } else if variant == 2 {
                    "1/2/4"
                } else {
                    "几何"
                }),
                "{e}"
            );
        }
        assert!(Session::new(
            Cursor::new(original[..4096].to_vec()),
            Arc::new(AtomicBool::new(false))
        )
        .is_err());
        let result = scan(
            Cursor::new(original),
            ScanMode::Deleted,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |_| panic!("deleted Ext candidate"),
            |_| Ok(()),
        );
        assert!(result.unwrap_err().contains("内部日志"));
        let mut corrupt = fixture(2).to_vec();
        corrupt[1024 + 0x78] ^= 1;
        assert!(Session::new(Cursor::new(corrupt), Arc::new(AtomicBool::new(false))).is_err());
    }
    #[test]
    fn canceled_and_failed_reads_stop_before_publishing_or_writing() {
        struct BadRead {
            read: bool,
        }
        impl Read for BadRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.read = true;
                Err(std::io::Error::other("injected read failure"))
            }
        }
        impl Seek for BadRead {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                Ok(4096)
            }
        }
        let result = crate::recovery::scan_filesystem_owned(
            BadRead { read: false },
            ScanMode::AllFiles,
            Arc::new(AtomicBool::new(true)),
            |_, _| Ok(()),
            |_| panic!(),
            |_| Ok(()),
        );
        assert!(result.unwrap_err().contains("取消"));
        let result = scan(
            BadRead { read: false },
            ScanMode::AllFiles,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |_| panic!(),
            |_| Ok(()),
        );
        assert!(result.unwrap_err().contains("injected"));
        let bytes = fixture(0);
        let (files, _) = collect(bytes.clone());
        let c = files.iter().find(|c| c.name == "small_file").unwrap();
        let mut out = vec![];
        assert!(crate::recovery::extract_owned(
            Cursor::new(bytes),
            c,
            &mut out,
            u64::MAX,
            Arc::new(AtomicBool::new(true)),
            |_| Ok(())
        )
        .is_err());
        assert!(out.is_empty());
    }
    #[test]
    fn hardlink_aliases_remain_separate_candidates_with_identical_contents() {
        let mut bytes = fixture(0).to_vec();
        let name_at = root_name_at(&bytes, b"small_file");
        let inode = u32le(&bytes, name_at - 8);
        let root = inode_at(&bytes, 2);
        let block = u32le(&bytes, root + 0x28) as usize * 1024;
        let mut entry = block;
        while entry + (u16le(&bytes, entry + 4) as usize) < block + 1024 {
            entry += u16le(&bytes, entry + 4) as usize;
        }
        let old_len = u16le(&bytes, entry + 4) as usize;
        let used = (8 + bytes[entry + 6] as usize + 3) & !3;
        assert!(old_len - used >= 16);
        bytes[entry + 4..entry + 6].copy_from_slice(&(used as u16).to_le_bytes());
        let alias = entry + used;
        bytes[alias..alias + 4].copy_from_slice(&inode.to_le_bytes());
        bytes[alias + 4..alias + 6].copy_from_slice(&((old_len - used) as u16).to_le_bytes());
        bytes[alias + 6] = 5;
        bytes[alias + 7] = 1;
        bytes[alias + 8..alias + 13].copy_from_slice(b"alias");
        let record = inode_at(&bytes, inode);
        let links = u16le(&bytes, record + 0x1a);
        bytes[record + 0x1a..record + 0x1c].copy_from_slice(&(links + 1).to_le_bytes());
        let bytes: Arc<[u8]> = bytes.into();
        let (files, _) = collect(bytes.clone());
        let a = files.iter().find(|c| c.name == "alias").unwrap();
        let b = files.iter().find(|c| c.name == "small_file").unwrap();
        assert_ne!(a.id, b.id);
        for c in [a, b] {
            let mut out = vec![];
            crate::recovery::extract_owned(
                Cursor::new(bytes.clone()),
                c,
                &mut out,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(out, b"hello, world!");
        }
    }
    #[test]
    fn bounds_mid_copy_cancellation_and_output_failures_propagate() {
        struct NoIo;
        impl Read for NoIo {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("out-of-range read")
            }
        }
        impl Seek for NoIo {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                panic!("out-of-range seek")
            }
        }
        let mut source = Source {
            reader: NoIo,
            length: 4096,
            cancel: Arc::new(AtomicBool::new(false)),
            reads: 0,
        };
        assert!(source.read_at(4095, &mut [0; 2]).is_err());
        assert!(source.read_at(u64::MAX, &mut [0; 1]).is_err());
        assert_eq!(source.reads, 0);
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = fixture(0);
        let (files, _) = collect(bytes.clone());
        let c = files.iter().find(|c| c.name == "big_file").unwrap();
        let e = crate::recovery::extract_owned(
            Cursor::new(bytes.clone()),
            c,
            &mut Broken,
            u64::MAX,
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(e.contains("injected output"));
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let mut out = vec![];
        let e = crate::recovery::extract_owned(
            Cursor::new(bytes),
            c,
            &mut out,
            u64::MAX,
            cancel,
            |done| {
                if done >= 2048 {
                    flag.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(e.contains("取消"));
        assert_eq!(out.len(), 2048);
    }
}

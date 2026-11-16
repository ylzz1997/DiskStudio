//! Version 2: bounded parallel compression, independently hashed chunks and
//! explicit immutable parent manifests. No paths or device access in this module.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

const HEADER: u64 = 4096;
pub const CHUNK: usize = 4 * 1024 * 1024;
const ENTRY: usize = 56;
const MAX_CHUNKS: u64 = 1_048_576;
pub const MAX_CHAIN: usize = 16;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hash_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .clamp(1, 4)
}

#[derive(Clone, Copy, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Compression {
    None,
    Fast,
    #[default]
    Balanced,
    High,
}
impl Compression {
    fn level(self) -> Option<i32> {
        match self {
            Self::None => None,
            Self::Fast => Some(1),
            Self::Balanced => Some(3),
            Self::High => Some(9),
        }
    }
    fn workers(self) -> usize {
        // High compression has larger codec state; keep its concurrency bounded.
        workers().min(if self == Self::High { 2 } else { 4 })
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Header {
    pub version: u32,
    pub id: String,
    pub kind: String,
    pub size: u64,
    pub sector: u64,
    pub sha256: String,
    pub parent: Option<String>,
    pub parent_manifest: Option<String>,
    pub chunk_size: u32,
    pub index_offset: u64,
    pub index_sha256: String,
    // Omitting this field for historical v2 headers preserves their canonical
    // serialization and parent manifest digest exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<AllocationPolicy>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationPolicy {
    pub skipped_bytes: u64,
    pub map_sha256: String,
}
impl AllocationPolicy {
    fn valid(&self, size: u64, sector: u64) -> bool {
        self.skipped_bytes < size && self.skipped_bytes % sector == 0 && hash_ok(&self.map_sha256)
    }
}
pub fn is_chunked(magic: &[u8; 8]) -> bool {
    matches!(magic, b"DSBK0002" | b"DSBK0003")
}
#[derive(Clone)]
struct Entry {
    offset: u64,
    stored: u32,
    raw: u32,
    codec: u8,
    hash: [u8; 32],
}
pub struct Archive {
    file: Box<dyn Source>,
    pub header: Header,
    entries: Vec<Entry>,
    pub manifest: String,
    source_validation: Option<Box<dyn Fn() -> Result<(), String> + Send>>,
}
pub trait Source: Read + Seek + Send {}
impl<T: Read + Seek + Send> Source for T {}
impl Archive {
    pub fn bind_source(
        mut self,
        validate: impl Fn() -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        validate()?;
        self.source_validation = Some(Box::new(validate));
        Ok(self)
    }
    pub fn open(file: File) -> Result<Self, String> {
        Self::from_reader(file)
    }
    pub fn from_reader(mut file: impl Source + 'static) -> Result<Self, String> {
        let len = file.seek(SeekFrom::End(0)).map_err(err)?;
        let mut head = vec![0; HEADER as usize];
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        file.read_exact(&mut head).map_err(err)?;
        if !is_chunked(head[..8].try_into().unwrap()) {
            return Err("需要 DiskStudio 分块备份文件（版本 2 或 3）。".into());
        }
        let n = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        if n > 4084 {
            return Err("备份头长度异常".into());
        }
        let h: Header = serde_json::from_slice(&head[12..12 + n]).map_err(err)?;
        let count = h.size.div_ceil(CHUNK as u64);
        if !matches!(h.version, 2 | 3)
            || (h.version == 2 && (&head[..8] != b"DSBK0002" || h.allocation.is_some()))
            || (h.version == 3 && (&head[..8] != b"DSBK0003" || h.allocation.is_none()))
            || uuid::Uuid::parse_str(&h.id).is_err()
            || !matches!(h.kind.as_str(), "disk" | "partition")
            || !matches!(h.sector, 512 | 4096)
            || h.size == 0
            || h.size % h.sector != 0
            || h.chunk_size as usize != CHUNK
            || count > MAX_CHUNKS
            || h.index_offset < HEADER
            || !hash_ok(&h.sha256)
            || !hash_ok(&h.index_sha256)
            || h.parent.is_some() != h.parent_manifest.is_some()
            || h.parent
                .as_ref()
                .is_some_and(|p| uuid::Uuid::parse_str(p).is_err() || p == &h.id)
            || h.parent_manifest.as_ref().is_some_and(|p| !hash_ok(p))
            || h.index_offset.checked_add(count * ENTRY as u64) != Some(len)
            || h.allocation
                .as_ref()
                .is_some_and(|p| !p.valid(h.size, h.sector))
        {
            return Err("备份结构无效或文件不完整".into());
        }
        let mut index = vec![0; count as usize * ENTRY];
        file.seek(SeekFrom::Start(h.index_offset)).map_err(err)?;
        file.read_exact(&mut index).map_err(err)?;
        if digest(&index) != h.index_sha256 {
            return Err("备份索引校验失败".into());
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut end = HEADER;
        for (i, b) in index.chunks_exact(ENTRY).enumerate() {
            let e = Entry {
                offset: u64::from_le_bytes(b[..8].try_into().unwrap()),
                stored: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                raw: u32::from_le_bytes(b[12..16].try_into().unwrap()),
                codec: b[16],
                hash: b[24..56].try_into().unwrap(),
            };
            let raw = (h.size - i as u64 * CHUNK as u64).min(CHUNK as u64) as u32;
            if e.raw != raw
                || b[17..24] != [0; 7]
                || e.codec > if h.version == 3 { 3 } else { 2 }
                || (e.codec == 2 && (h.parent.is_none() || e.stored != 0 || e.offset != 0))
                || (e.codec == 3 && (e.stored != 0 || e.offset != 0))
                || (e.codec < 2
                    && (e.offset != end
                        || e.stored == 0
                        || e.stored > e.raw
                        || (e.codec == 0 && e.stored != e.raw)))
            {
                return Err("备份块边界或压缩信息无效".into());
            }
            if e.codec < 2 {
                end = end.checked_add(e.stored as u64).ok_or("块范围溢出")?;
            }
            entries.push(e);
        }
        if end != h.index_offset {
            return Err("备份数据范围不连续".into());
        }
        let manifest = digest(&serde_json::to_vec(&h).map_err(err)?);
        Ok(Self {
            file: Box::new(file),
            header: h,
            entries,
            manifest,
            source_validation: None,
        })
    }
}

/// Newest layer first. Handles stay open without write/delete sharing on Windows.
pub struct Chain {
    layers: Vec<Archive>,
}
impl Chain {
    pub fn new(first: Archive) -> Self {
        Self {
            layers: vec![first],
        }
    }
    pub fn header(&self) -> &Header {
        &self.layers[0].header
    }
    pub fn manifest(&self) -> &str {
        &self.layers[0].manifest
    }
    pub fn depth(&self) -> usize {
        self.layers.len()
    }
    pub fn headers(&self) -> Vec<Header> {
        self.layers.iter().map(|a| a.header.clone()).collect()
    }
    pub fn validate_sources(&self) -> Result<(), String> {
        for layer in &self.layers {
            if let Some(validate) = &layer.source_validation {
                validate()?;
            }
        }
        Ok(())
    }
    fn header_at(&self, layer: usize) -> Result<&Header, String> {
        self.complete()?;
        self.layers
            .get(layer)
            .map(|a| &a.header)
            .ok_or("备份还原点不存在".into())
    }
    pub fn missing_parent(&self) -> Option<&str> {
        self.layers.last().unwrap().header.parent.as_deref()
    }
    pub fn add_parent(&mut self, parent: Archive) -> Result<(), String> {
        let previous = self.layers.last().unwrap();
        let p = &parent.header;
        if self.depth() >= MAX_CHAIN
            || self.layers.iter().any(|l| l.header.id == p.id)
            || previous.header.parent.as_deref() != Some(&p.id)
            || previous.header.parent_manifest.as_deref() != Some(&parent.manifest)
            || p.kind != self.header().kind
            || p.size != self.header().size
            || p.sector != self.header().sector
            || self
                .layers
                .iter()
                .map(|l| l.entries.len() * ENTRY)
                .sum::<usize>()
                + parent.entries.len() * ENTRY
                > 256 * 1024 * 1024
        {
            return Err("基础备份不匹配、备份链循环或超出 16 层 / 256 MiB 索引上限".into());
        }
        self.layers.push(parent);
        Ok(())
    }
    pub fn complete(&self) -> Result<(), String> {
        if self.missing_parent().is_some() {
            Err("缺少基础备份，不能继续".into())
        } else {
            Ok(())
        }
    }
    pub fn prepend(self, child: Archive) -> Result<Self, String> {
        let mut next = Self::new(child);
        for layer in self.layers {
            next.add_parent(layer)?;
        }
        next.complete()?;
        Ok(next)
    }
    fn block(&mut self, i: usize) -> Result<Vec<u8>, String> {
        self.block_at(0, i)
    }
    fn block_at(&mut self, first: usize, i: usize) -> Result<Vec<u8>, String> {
        self.header_at(first)?;
        let expected = self.layers[first].entries.get(i).ok_or("块编号越界")?.hash;
        for layer in &mut self.layers[first..] {
            let e = &layer.entries[i];
            if e.hash != expected {
                return Err("增量引用的基础块不匹配".into());
            }
            if e.codec == 2 {
                continue;
            }
            let mut stored = vec![0; e.stored as usize];
            layer.file.seek(SeekFrom::Start(e.offset)).map_err(err)?;
            layer.file.read_exact(&mut stored).map_err(err)?;
            let raw = if e.codec == 3 {
                vec![0; e.raw as usize]
            } else if e.codec == 0 {
                stored
            } else {
                zstd::bulk::decompress(&stored, e.raw as usize).map_err(err)?
            };
            if raw.len() != e.raw as usize || Sha256::digest(&raw).as_slice() != e.hash {
                return Err("备份数据块 SHA-256 校验失败".into());
            }
            return Ok(raw);
        }
        Err("增量备份缺少数据块".into())
    }
    pub fn verify(&mut self, tick: impl FnMut(u64) -> Result<(), String>) -> Result<(), String> {
        self.verify_at(0, tick)
    }
    pub fn verify_at(
        &mut self,
        first: usize,
        mut tick: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_sources()?;
        let size = self.header_at(first)?.size;
        let depth = (self.layers.len() - first) as u64;
        // Verify every retained dependency, including blocks superseded by the
        // newest restore point. A corrupt parent must not pass simply because
        // that particular chunk is no longer referenced by the child.
        for layer in first..self.layers.len() {
            let expected = self.layers[layer].header.sha256.clone();
            let mut hash = Sha256::new();
            let mut done = 0;
            let completed = (layer - first) as u64 * size;
            for i in 0..self.layers[layer].entries.len() {
                tick((completed + done) / depth)?;
                let bytes = self.block_at(layer, i)?;
                done += bytes.len() as u64;
                hash.update(bytes);
            }
            tick((completed + done) / depth)?;
            if format!("{:x}", hash.finalize()) != expected {
                return Err("完整备份链 SHA-256 校验失败".into());
            }
        }
        self.validate_sources()?;
        Ok(())
    }
    pub fn reader(&mut self) -> ChainReader<'_> {
        ChainReader {
            chain: self,
            index: None,
            buffer: vec![],
            pos: 0,
        }
    }
}
pub struct ChainReader<'a> {
    chain: &'a mut Chain,
    index: Option<usize>,
    buffer: Vec<u8>,
    pos: u64,
}
impl Read for ChainReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos == self.chain.header().size {
            return Ok(0);
        }
        let index = (self.pos / CHUNK as u64) as usize;
        if self.index != Some(index) {
            self.index = None;
            self.buffer.clear();
            self.buffer = self.chain.block(index).map_err(io::Error::other)?;
            self.index = Some(index);
        }
        let offset = (self.pos % CHUNK as u64) as usize;
        let n = out.len().min(self.buffer.len() - offset);
        out[..n].copy_from_slice(&self.buffer[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

fn seek_position(pos: u64, size: u64, from: SeekFrom) -> io::Result<u64> {
    let next = match from {
        SeekFrom::Start(n) => n as i128,
        SeekFrom::Current(n) => pos as i128 + n as i128,
        SeekFrom::End(n) => size as i128 + n as i128,
    };
    if next < 0 || next > size as i128 {
        return Err(io::Error::other("Seek outside backup content"));
    }
    Ok(next as u64)
}
impl Seek for ChainReader<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.chain.header().size, from)?;
        Ok(self.pos)
    }
}

/// Retains immutable archive handles; each consumer has an independent logical
/// position and at most one decompressed chunk. No temporary-disk expansion.
pub struct SharedReader {
    chain: Arc<Mutex<Chain>>,
    layer: usize,
    size: u64,
    pos: u64,
    index: Option<usize>,
    buffer: Vec<u8>,
}
impl SharedReader {
    #[cfg(test)]
    pub fn new(chain: Arc<Mutex<Chain>>) -> Result<Self, String> {
        Self::at(chain, 0)
    }
    pub fn at(chain: Arc<Mutex<Chain>>, layer: usize) -> Result<Self, String> {
        let size = {
            let locked = chain.lock().map_err(|_| "备份读取状态异常")?;
            locked.header_at(layer)?.size
        };
        Ok(Self {
            chain,
            layer,
            size,
            pos: 0,
            index: None,
            buffer: vec![],
        })
    }
}
impl Read for SharedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos == self.size {
            return Ok(0);
        }
        let index = (self.pos / CHUNK as u64) as usize;
        if self.index != Some(index) {
            self.index = None;
            self.buffer.clear();
            self.buffer = self
                .chain
                .lock()
                .map_err(|_| io::Error::other("备份读取状态异常"))?
                .block_at(self.layer, index)
                .map_err(io::Error::other)?;
            self.index = Some(index);
        }
        let offset = (self.pos % CHUNK as u64) as usize;
        let n = out.len().min(self.buffer.len() - offset);
        out[..n].copy_from_slice(&self.buffer[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for SharedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek_position(self.pos, self.size, from)?;
        Ok(self.pos)
    }
}

pub trait Output: Write + Seek {
    fn reset(&mut self) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}
impl Output for File {
    fn reset(&mut self) -> io::Result<()> {
        self.set_len(0)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}
#[cfg(test)]
impl Output for io::Cursor<Vec<u8>> {
    fn reset(&mut self) -> io::Result<()> {
        self.get_mut().clear();
        self.set_position(0);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[allow(dead_code)] // Compatibility entry points used by synthetic/guest fixtures.
pub fn create<R: Read>(
    reader: &mut R,
    file: &mut impl Output,
    kind: &str,
    size: u64,
    sector: u64,
    compression: bool,
    parent: Option<&Chain>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Header, String> {
    create_with_policy(
        reader,
        file,
        kind,
        size,
        sector,
        compression,
        parent,
        None,
        tick,
    )
}
#[allow(dead_code)]
pub fn create_with_policy<R: Read>(
    reader: &mut R,
    file: &mut impl Output,
    kind: &str,
    size: u64,
    sector: u64,
    compression: bool,
    parent: Option<&Chain>,
    allocation: Option<AllocationPolicy>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Header, String> {
    create_with_settings(
        reader,
        file,
        kind,
        size,
        sector,
        if compression {
            Compression::Balanced
        } else {
            Compression::None
        },
        parent,
        allocation,
        tick,
    )
}
pub fn create_with_settings<R: Read>(
    reader: &mut R,
    file: &mut impl Output,
    kind: &str,
    size: u64,
    sector: u64,
    compression: Compression,
    parent: Option<&Chain>,
    allocation: Option<AllocationPolicy>,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Header, String> {
    if size == 0
        || size.div_ceil(CHUNK as u64) > MAX_CHUNKS
        || !matches!(sector, 512 | 4096)
        || size % sector != 0
        || !matches!(kind, "disk" | "partition")
        || allocation.as_ref().is_some_and(|p| !p.valid(size, sector))
    {
        return Err("备份范围不支持".into());
    }
    if let Some(p) = parent {
        p.complete()?;
        if p.depth() >= MAX_CHAIN
            || p.header().size != size
            || p.header().kind != kind
            || p.header().sector != sector
        {
            return Err("基础备份类型或容量不匹配，或已达到链长度上限".into());
        }
    }
    file.seek(SeekFrom::Start(0)).map_err(err)?;
    file.reset().map_err(err)?;
    file.write_all(&vec![0; HEADER as usize]).map_err(err)?;
    let mut index = vec![];
    let mut done = 0;
    let mut hash = Sha256::new();
    let mut offset = HEADER;
    // Batches bound input/output memory to ~32 MiB at four workers. Each
    // worker owns its codec context. Results are written in original order.
    while done < size {
        tick(done)?;
        let mut batch = vec![];
        for _ in 0..compression.workers() {
            if done == size {
                break;
            }
            tick(done)?;
            let n = (size - done).min(CHUNK as u64) as usize;
            let mut bytes = vec![0; n];
            reader.read_exact(&mut bytes).map_err(err)?;
            hash.update(&bytes);
            done += n as u64;
            let i = index.len() / ENTRY + batch.len();
            let previous = parent.map(|p| p.layers[0].entries[i].hash);
            batch.push((bytes, previous));
        }
        let zero_blocks = allocation.is_some();
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = batch
                .into_iter()
                .map(|(bytes, previous)| {
                    scope.spawn(move || {
                        let raw = bytes.len() as u32;
                        let block_hash: [u8; 32] = Sha256::digest(&bytes).into();
                        if previous == Some(block_hash) {
                            return Ok((raw, 2, block_hash, vec![]));
                        }
                        if zero_blocks && bytes.iter().all(|&b| b == 0) {
                            return Ok((raw, 3, block_hash, vec![]));
                        }
                        if let Some(level) = compression.level() {
                            let packed = zstd::bulk::compress(&bytes, level).map_err(err)?;
                            if packed.len() < bytes.len() {
                                return Ok((raw, 1, block_hash, packed));
                            }
                        }
                        Ok::<_, String>((raw, 0, block_hash, bytes))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().map_err(|_| "压缩线程异常退出".to_string())?)
                .collect::<Result<Vec<_>, String>>()
        })?;
        tick(done)?;
        for (raw, codec, block_hash, bytes) in results {
            let mut entry = [0; ENTRY];
            entry[..8].copy_from_slice(&(if codec >= 2 { 0 } else { offset }).to_le_bytes());
            entry[8..12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            entry[12..16].copy_from_slice(&raw.to_le_bytes());
            entry[16] = codec;
            entry[24..].copy_from_slice(&block_hash);
            index.extend_from_slice(&entry);
            file.write_all(&bytes).map_err(err)?;
            offset += bytes.len() as u64;
        }
    }
    let h = Header {
        version: if allocation.is_some() { 3 } else { 2 },
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.into(),
        size,
        sector,
        sha256: format!("{:x}", hash.finalize()),
        parent: parent.map(|p| p.header().id.clone()),
        parent_manifest: parent.map(|p| p.manifest().into()),
        chunk_size: CHUNK as u32,
        index_offset: offset,
        index_sha256: digest(&index),
        allocation,
    };
    file.write_all(&index).map_err(err)?;
    let json = serde_json::to_vec(&h).map_err(err)?;
    if json.len() > 4084 {
        return Err("备份头过大".into());
    }
    let mut header = vec![0; HEADER as usize];
    header[..8].copy_from_slice(if h.version == 3 {
        b"DSBK0003"
    } else {
        b"DSBK0002"
    });
    header[8..12].copy_from_slice(&(json.len() as u32).to_le_bytes());
    header[12..12 + json.len()].copy_from_slice(&json);
    tick(size)?;
    file.seek(SeekFrom::Start(0)).map_err(err)?;
    file.write_all(&header).map_err(err)?;
    file.sync().map_err(err)?;
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chain_revalidates_sources_and_checks_superseded_parent_content() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let valid = Arc::new(AtomicBool::new(true));
        let check = valid.clone();
        let mut base = io::Cursor::new(vec![]);
        create(
            &mut &[1; 512][..],
            &mut base,
            "partition",
            512,
            512,
            false,
            None,
            |_| Ok(()),
        )
        .unwrap();
        let base_bytes = base.get_ref().clone();
        let base = Archive::from_reader(base)
            .unwrap()
            .bind_source(move || {
                if check.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("source moved".into())
                }
            })
            .unwrap();
        let mut chain = Chain::new(base);
        valid.store(false, Ordering::SeqCst);
        assert_eq!(
            chain
                .verify(|_| panic!("must reject before reading"))
                .unwrap_err(),
            "source moved"
        );
        valid.store(true, Ordering::SeqCst);
        assert_eq!(
            chain
                .verify(|_| {
                    valid.store(false, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap_err(),
            "source moved"
        );
        valid.store(true, Ordering::SeqCst);
        let mut child = io::Cursor::new(vec![]);
        create(
            &mut &[2; 512][..],
            &mut child,
            "partition",
            512,
            512,
            false,
            Some(&chain),
            |_| Ok(()),
        )
        .unwrap();
        let child = Archive::from_reader(child).unwrap();
        assert_eq!(child.entries[0].codec, 0); // no parent data is referenced
        let mut damaged = base_bytes;
        damaged[HEADER as usize] ^= 1;
        let base = Archive::from_reader(io::Cursor::new(damaged)).unwrap();
        let mut chain = Chain::new(child);
        chain.add_parent(base).unwrap();
        assert!(chain.verify(|_| Ok(())).is_err());
    }
    #[test]
    fn compression_presets_keep_logical_content_and_bound_parallelism() {
        let input = (0..CHUNK + 512)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let mut expected = None;
        for compression in [
            Compression::None,
            Compression::Fast,
            Compression::Balanced,
            Compression::High,
        ] {
            assert!((1..=if compression == Compression::High {
                2
            } else {
                4
            })
                .contains(&compression.workers()));
            let mut output = io::Cursor::new(vec![]);
            let header = create_with_settings(
                &mut input.as_slice(),
                &mut output,
                "partition",
                input.len() as u64,
                512,
                compression,
                None,
                None,
                |_| Ok(()),
            )
            .unwrap();
            if let Some(hash) = &expected {
                assert_eq!(&header.sha256, hash);
            } else {
                expected = Some(header.sha256.clone());
            }
            let mut chain =
                Chain::new(Archive::from_reader(io::Cursor::new(output.into_inner())).unwrap());
            chain.verify(|_| Ok(())).unwrap();
            let mut actual = vec![];
            chain.reader().read_to_end(&mut actual).unwrap();
            assert_eq!(actual, input);
        }
        assert!(serde_json::from_str::<Compression>("999").is_err());
        assert!(serde_json::from_str::<Compression>("\"maximumUnbounded\"").is_err());
    }
    #[test]
    fn allocation_v3_zero_chunks_and_mixed_v2_incremental_chains_round_trip() {
        let original = vec![7; CHUNK * 2];
        let mut base = io::Cursor::new(vec![]);
        let old = create(
            &mut original.as_slice(),
            &mut base,
            "partition",
            original.len() as u64,
            512,
            false,
            None,
            |_| Ok(()),
        )
        .unwrap();
        let old_json = serde_json::to_string(&old).unwrap();
        assert!(!old_json.contains("allocation"));
        let base = Archive::from_reader(base).unwrap();
        assert_eq!(base.manifest, digest(old_json.as_bytes()));
        let mut chain = Chain::new(base);
        let mut normalized = original.clone();
        normalized[CHUNK..].fill(0);
        let mut out = io::Cursor::new(vec![]);
        let h = create_with_policy(
            &mut normalized.as_slice(),
            &mut out,
            "partition",
            normalized.len() as u64,
            512,
            false,
            Some(&chain),
            Some(AllocationPolicy {
                skipped_bytes: CHUNK as u64,
                map_sha256: "a".repeat(64),
            }),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(h.version, 3);
        assert_eq!(out.get_ref().len(), 4096 + 2 * ENTRY);
        let child = Archive::from_reader(out).unwrap();
        assert_eq!(child.entries[0].codec, 2);
        assert_eq!(child.entries[1].codec, 3);
        chain = chain.prepend(child).unwrap();
        chain.verify(|_| Ok(())).unwrap();
        let mut bytes = vec![];
        chain.reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, normalized);
        let mut next = normalized.clone();
        next[CHUNK + 123] = 99;
        let mut out = io::Cursor::new(vec![]);
        create(
            &mut next.as_slice(),
            &mut out,
            "partition",
            next.len() as u64,
            512,
            true,
            Some(&chain),
            |_| Ok(()),
        )
        .unwrap();
        let mut chain = chain.prepend(Archive::from_reader(out).unwrap()).unwrap();
        chain.verify(|_| Ok(())).unwrap();
        let mut restored = vec![];
        chain.reader().read_to_end(&mut restored).unwrap();
        assert_eq!(restored, next);
        chain.verify_at(1, |_| Ok(())).unwrap();
        chain.verify_at(2, |_| Ok(())).unwrap();
    }
    #[test]
    fn allocation_v3_rejects_invalid_policy_marker_and_corrupt_zero_chunk_hash() {
        let bytes = vec![0; 512];
        let mut out = io::Cursor::new(vec![]);
        assert!(create_with_policy(
            &mut bytes.as_slice(),
            &mut out,
            "partition",
            512,
            512,
            false,
            None,
            Some(AllocationPolicy {
                skipped_bytes: 512,
                map_sha256: "a".repeat(64)
            }),
            |_| Ok(())
        )
        .is_err());
        create_with_policy(
            &mut bytes.as_slice(),
            &mut out,
            "partition",
            512,
            512,
            false,
            None,
            Some(AllocationPolicy {
                skipped_bytes: 0,
                map_sha256: "a".repeat(64),
            }),
            |_| Ok(()),
        )
        .unwrap();
        let mut bad = out.clone();
        bad.get_mut()[7] = b'2';
        assert!(Archive::from_reader(bad).is_err());
        let mut archive = Archive::from_reader(out).unwrap();
        archive.entries[0].hash[0] ^= 1;
        assert!(Chain::new(archive).verify(|_| Ok(())).is_err());
    }
    #[test]
    fn allocation_backup_browser_labels_each_selected_restore_point() {
        let mut base = io::Cursor::new(vec![]);
        create(
            &mut &[7; 512][..],
            &mut base,
            "partition",
            512,
            512,
            false,
            None,
            |_| Ok(()),
        )
        .unwrap();
        let chain = Chain::new(Archive::from_reader(base).unwrap());
        let original_id = chain.header().id.clone();
        let mut child = io::Cursor::new(vec![]);
        create_with_policy(
            &mut &[0; 512][..],
            &mut child,
            "partition",
            512,
            512,
            false,
            Some(&chain),
            Some(AllocationPolicy {
                skipped_bytes: 0,
                map_sha256: "b".repeat(64),
            }),
            |_| Ok(()),
        )
        .unwrap();
        let browser = crate::backup_browser::Browser::from_chain(
            chain.prepend(Archive::from_reader(child).unwrap()).unwrap(),
            "test.dsbackup".into(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(browser.info.version, 3);
        assert!(browser.info.warning.contains("按已用空间"));
        let prior = browser.select_point(&original_id, |_, _| Ok(())).unwrap();
        assert_eq!(prior.info.version, 2);
        assert!(!prior.info.warning.contains("按已用空间"));
    }
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> (Self, File) {
            let p = std::env::temp_dir()
                .join(format!("diskstudio-synthetic-{}.bin", uuid::Uuid::new_v4()));
            let f = File::options()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&p)
                .unwrap();
            (Self(p), f)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[test]
    fn compressed_incremental_chain_restores_exact_bytes_and_detects_damage() {
        let (_base, mut f) = Temp::new();
        let mut bytes = vec![7; CHUNK * 2 + 512];
        create(
            &mut &bytes[..],
            &mut f,
            "partition",
            bytes.len() as u64,
            512,
            true,
            None,
            |_| Ok(()),
        )
        .unwrap();
        assert!(f.metadata().unwrap().len() < 10000);
        let mut damage = f.try_clone().unwrap();
        let mut chain = Chain::new(Archive::open(f).unwrap());
        chain.verify(|_| Ok(())).unwrap();
        bytes[CHUNK + 33] = 42;
        let (_delta, mut d) = Temp::new();
        create(
            &mut &bytes[..],
            &mut d,
            "partition",
            bytes.len() as u64,
            512,
            true,
            Some(&chain),
            |_| Ok(()),
        )
        .unwrap();
        let delta = Archive::open(d).unwrap();
        assert_eq!(delta.entries[0].codec, 2);
        assert_eq!(delta.entries[2].codec, 2);
        let mut combined = Chain::new(delta);
        assert!(combined.verify(|_| Ok(())).is_err());
        combined.add_parent(chain.layers.remove(0)).unwrap();
        combined.verify(|_| Ok(())).unwrap();
        let mut out = vec![];
        combined.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes);
        damage.seek(SeekFrom::Start(HEADER)).unwrap();
        damage.write_all(&[0; 16]).unwrap();
        assert!(combined.verify(|_| Ok(())).is_err());
    }
    #[test]
    fn cancel_leaves_unreadable_partial_and_corrupt_index_is_rejected() {
        let (_p, mut f) = Temp::new();
        assert!(create(
            &mut &[0; 512][..],
            &mut f,
            "disk",
            512,
            512,
            true,
            None,
            |_| Err("cancel".into())
        )
        .is_err());
        assert!(Archive::open(f.try_clone().unwrap()).is_err());
        create(
            &mut &[0; 512][..],
            &mut f,
            "disk",
            512,
            512,
            false,
            None,
            |_| Ok(()),
        )
        .unwrap();
        f.seek(SeekFrom::End(-1)).unwrap();
        f.write_all(&[1]).unwrap();
        assert!(Archive::open(f).is_err());
    }
    #[test]
    fn unrelated_parent_and_oversized_geometry_are_rejected() {
        let (_p, mut f) = Temp::new();
        create(
            &mut &[0; 512][..],
            &mut f,
            "disk",
            512,
            512,
            false,
            None,
            |_| Ok(()),
        )
        .unwrap();
        let mut c = Chain::new(Archive::open(f.try_clone().unwrap()).unwrap());
        assert!(c.add_parent(Archive::open(f).unwrap()).is_err());
        let (_q, mut q) = Temp::new();
        assert!(create(
            &mut &[][..],
            &mut q,
            "disk",
            u64::MAX,
            512,
            true,
            None,
            |_| Ok(())
        )
        .is_err());
    }
}

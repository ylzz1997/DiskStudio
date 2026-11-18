//! Resumable same-device transfers over caller-owned, independently secured I/O.
//! The caller keeps the device offline and the journal durable on another volume.
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};

const BUFFER_BYTES: u64 = 4 * crate::MIB;
const MAGIC: &[u8; 8] = b"DSPMOVE1";
const HEADER_BYTES: usize = 116;
const RECORD_BYTES: usize = 40;
const SLOT_BYTES: usize = 4096;
const MAX_METADATA_BYTES: usize = 2 * crate::MIB as usize;

type DigestBytes = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    source: u64,
    destination: u64,
    length: u64,
    capacity: u64,
    sector: u64,
}

impl Range {
    pub fn new(
        source: u64,
        destination: u64,
        length: u64,
        capacity: u64,
        sector: u64,
    ) -> Result<Self, String> {
        if !matches!(sector, 512 | 4096)
            || length == 0
            || source == destination
            || [source, destination, length, capacity]
                .iter()
                .any(|n| n % sector != 0)
            || [source, destination]
                .iter()
                .any(|n| n.checked_add(length).is_none_or(|end| end > capacity))
        {
            return Err("移动范围相同、未对齐或超出磁盘容量。".into());
        }
        Ok(Self {
            source,
            destination,
            length,
            capacity,
            sector,
        })
    }

    pub fn source(self) -> u64 {
        self.source
    }
    pub fn destination(self) -> u64 {
        self.destination
    }
    pub fn length(self) -> u64 {
        self.length
    }
    pub fn capacity(self) -> u64 {
        self.capacity
    }
    pub fn sector(self) -> u64 {
        self.sector
    }

    pub fn overlaps(self) -> bool {
        self.source.abs_diff(self.destination) < self.length
    }

    fn backwards(self) -> bool {
        self.overlaps() && self.destination > self.source
    }

    // A not-yet-checkpointed write must leave its own input untouched. Copies
    // in the correct direction may overwrite only already checkpointed input.
    fn chunk_bytes(self) -> u64 {
        if self.overlaps() {
            BUFFER_BYTES.min(self.source.abs_diff(self.destination))
        } else {
            BUFFER_BYTES
        }
    }

    fn chunks(self) -> u64 {
        self.length.div_ceil(self.chunk_bytes())
    }

    fn copied(self, chunks: u64) -> u64 {
        chunks.saturating_mul(self.chunk_bytes()).min(self.length)
    }

    pub fn source_sha256(
        self,
        disk: &mut (impl Read + Seek),
        progress: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<DigestBytes, String> {
        self.original_sha256(disk, 0, progress)
    }

    fn original_sha256(
        self,
        disk: &mut (impl Read + Seek),
        copied: u64,
        mut progress: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<DigestBytes, String> {
        let mut hash = Sha256::new();
        let mut buffer = vec![0; BUFFER_BYTES.min(self.length) as usize];
        let spans = if self.backwards() {
            [
                (self.source, self.length - copied),
                (self.destination + self.length - copied, copied),
            ]
        } else {
            [
                (self.destination, copied),
                (self.source + copied, self.length - copied),
            ]
        };
        let mut checked = 0;
        for (start, length) in spans {
            let mut offset = 0;
            while offset < length {
                progress(checked)?;
                let n = (length - offset).min(buffer.len() as u64) as usize;
                read_at(disk, start + offset, &mut buffer[..n])?;
                hash.update(&buffer[..n]);
                offset += n as u64;
                checked += n as u64;
            }
        }
        progress(checked)?;
        Ok(hash.finalize().into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    CheckSource,
    Copy,
    CheckDestination,
}

/// Immutable plan bytes and two independently checksummed progress slots. A
/// torn new slot leaves its preceding slot available, subject to content proof.
pub struct Journal<I> {
    io: I,
    sync: fn(&mut I) -> io::Result<()>,
    range: Range,
    metadata: Vec<u8>,
    source_hash: DigestBytes,
    context: DigestBytes,
    sequence: u64,
}

impl<I: Read + Write + Seek> Journal<I> {
    pub fn create(
        mut io: I,
        sync: fn(&mut I) -> io::Result<()>,
        range: Range,
        source_hash: DigestBytes,
        metadata: Vec<u8>,
    ) -> Result<Self, String> {
        if metadata.is_empty()
            || metadata.len() > MAX_METADATA_BYTES
            || io.seek(SeekFrom::End(0)).map_err(error)? != 0
        {
            return Err("移动记录需要空白文件和有界的计划。".into());
        }
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        for n in [
            range.source,
            range.destination,
            range.length,
            range.capacity,
            range.sector,
        ] {
            header.extend_from_slice(&n.to_le_bytes());
        }
        header.extend_from_slice(&source_hash);
        header.extend_from_slice(&Sha256::digest(&metadata));
        let context = Sha256::digest(&header).into();
        let start = slot_start(metadata.len());
        let mut initial = [0; SLOT_BYTES];
        initial[..RECORD_BYTES].copy_from_slice(&record(&context, 0));
        io.write_all(&header)
            .and_then(|_| io.write_all(&metadata))
            .and_then(|_| io.write_all(&[0; SLOT_BYTES][..start - HEADER_BYTES - metadata.len()]))
            .and_then(|_| io.write_all(&initial))
            .and_then(|_| io.write_all(&[0; SLOT_BYTES]))
            .and_then(|_| sync(&mut io))
            .map_err(error)?;
        Self::open(io, sync)
    }

    pub fn open(mut io: I, sync: fn(&mut I) -> io::Result<()>) -> Result<Self, String> {
        let mut header = [0; HEADER_BYTES];
        read_at(&mut io, 0, &mut header)?;
        let metadata_length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if &header[..8] != MAGIC
            || !(1..=MAX_METADATA_BYTES).contains(&metadata_length)
            || io.seek(SeekFrom::End(0)).map_err(error)?
                != (slot_start(metadata_length) + 2 * SLOT_BYTES) as u64
        {
            return Err("移动记录版本或文件长度无效。".into());
        }
        let value = |offset| u64::from_le_bytes(header[offset..offset + 8].try_into().unwrap());
        let range = Range::new(value(12), value(20), value(28), value(36), value(44))?;
        let source_hash = header[52..84].try_into().unwrap();
        let context = Sha256::digest(header).into();
        let mut metadata = vec![0; metadata_length];
        read_at(&mut io, HEADER_BYTES as u64, &mut metadata)?;
        if Sha256::digest(&metadata).as_slice() != &header[84..116] {
            return Err("移动计划的 SHA-256 不匹配。".into());
        }
        let mut slots = [[0; RECORD_BYTES]; 2];
        for (i, bytes) in slots.iter_mut().enumerate() {
            read_at(
                &mut io,
                (slot_start(metadata_length) + i * SLOT_BYTES) as u64,
                bytes,
            )?;
        }
        let sequences = slots.map(|bytes| {
            let n = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            (n <= range.chunks() + 1 && record(&context, n) == bytes).then_some(n)
        });
        let sequence = match sequences {
            [Some(even), Some(odd)] if even % 2 == 0 && odd % 2 == 1 && even.abs_diff(odd) == 1 => {
                even.max(odd)
            }
            [Some(even), None] if even % 2 == 0 => even,
            [None, Some(odd)] if odd % 2 == 1 => odd,
            _ => return Err("移动记录的完成位置无法验证。".into()),
        };
        Ok(Self {
            io,
            sync,
            range,
            metadata,
            source_hash,
            context,
            sequence,
        })
    }

    pub fn range(&self) -> Range {
        self.range
    }
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }
    pub fn source_hash(&self) -> DigestBytes {
        self.source_hash
    }
    pub fn copied_bytes(&self) -> u64 {
        self.range.copied(self.sequence)
    }
    pub fn copy_finished(&self) -> bool {
        self.sequence >= self.range.chunks()
    }
    pub fn completed(&self) -> bool {
        self.sequence > self.range.chunks()
    }

    fn advance(&mut self) -> Result<(), String> {
        let next = self.sequence + 1;
        let at = slot_start(self.metadata.len()) + (next % 2) as usize * SLOT_BYTES;
        let bytes = record(&self.context, next);
        let mut slot = [0; SLOT_BYTES];
        slot[..RECORD_BYTES].copy_from_slice(&bytes);
        self.io.seek(SeekFrom::Start(at as u64)).map_err(error)?;
        self.io.write_all(&slot).map_err(error)?;
        (self.sync)(&mut self.io).map_err(error)?;
        let mut stored = [0; RECORD_BYTES];
        read_at(&mut self.io, at as u64, &mut stored)?;
        if stored != bytes {
            return Err("移动记录回读失败。".into());
        }
        self.sequence = next;
        Ok(())
    }

    pub fn copy<D: Read + Write + Seek>(
        &mut self,
        disk: &mut D,
        mut sync_disk: impl FnMut(&mut D) -> io::Result<()>,
        mut progress: impl FnMut(Stage, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.copy_finished() {
            return Err("数据已搬移，需核对新范围及分区元数据。".into());
        }
        let range = self.range;
        let mut copied = self.copied_bytes();
        let source = range.original_sha256(disk, copied, |n| progress(Stage::CheckSource, n))?;
        if source != self.source_hash {
            return Err("已搬移数据与剩余源数据无法重建原始 SHA-256。".into());
        }
        let chunk = range.chunk_bytes().min(range.length) as usize;
        let mut input = vec![0; chunk];
        let mut output = vec![0; chunk];
        while copied < range.length {
            progress(Stage::Copy, copied)?;
            let n = (range.length - copied).min(chunk as u64) as usize;
            let relative = if range.backwards() {
                range.length - copied - n as u64
            } else {
                copied
            };
            read_at(disk, range.source + relative, &mut input[..n])?;
            disk.seek(SeekFrom::Start(range.destination + relative))
                .map_err(error)?;
            if disk.write(&input[..n]).map_err(error)? != n {
                return Err("移动数据时短写，目标可能部分写入。".into());
            }
            sync_disk(disk).map_err(error)?;
            read_at(disk, range.destination + relative, &mut output[..n])?;
            if Sha256::digest(&input[..n]) != Sha256::digest(&output[..n]) {
                return Err("移动数据块的 SHA-256 回读不匹配。".into());
            }
            self.advance()?;
            copied += n as u64;
        }
        progress(Stage::Copy, copied)
    }

    /// Hash the whole destination before publishing its GPT entry. The caller
    /// may reverse only independently verified old/new boot-sector changes in
    /// this buffer, so a crash after a boot patch can still be checked.
    pub fn verify_destination(
        &self,
        disk: &mut (impl Read + Seek),
        mut normalize: impl FnMut(u64, &mut [u8]) -> Result<(), String>,
        mut progress: impl FnMut(Stage, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.copy_finished() {
            return Err("移动数据尚未完成。".into());
        }
        let mut buffer = vec![0; BUFFER_BYTES.min(self.range.length) as usize];
        let mut hash = Sha256::new();
        let mut offset = 0;
        while offset < self.range.length {
            progress(Stage::CheckDestination, offset)?;
            let n = (self.range.length - offset).min(buffer.len() as u64) as usize;
            read_at(disk, self.range.destination + offset, &mut buffer[..n])?;
            normalize(offset, &mut buffer[..n])?;
            hash.update(&buffer[..n]);
            offset += n as u64;
        }
        progress(Stage::CheckDestination, offset)?;
        if <DigestBytes>::from(hash.finalize()) != self.source_hash {
            return Err("移动目标的完整 SHA-256 与原始范围不匹配。".into());
        }
        Ok(())
    }

    /// Call only after re-reading and verifying all boot and GPT changes.
    pub fn mark_completed(&mut self) -> Result<(), String> {
        if self.sequence != self.range.chunks() {
            return Err("移动记录的完成阶段无效。".into());
        }
        self.advance()
    }
}

fn slot_start(metadata_length: usize) -> usize {
    // Keep the two alternating updates in separate 4 KiB file blocks and
    // away from the immutable header. Never overwrite the surviving slot.
    (HEADER_BYTES + metadata_length).div_ceil(SLOT_BYTES) * SLOT_BYTES
}

fn record(context: &DigestBytes, sequence: u64) -> [u8; RECORD_BYTES] {
    let mut bytes = [0; RECORD_BYTES];
    bytes[..8].copy_from_slice(&sequence.to_le_bytes());
    let mut hash = Sha256::new();
    hash.update(context);
    hash.update(&bytes[..8]);
    bytes[8..].copy_from_slice(&hash.finalize());
    bytes
}

fn read_at(input: &mut (impl Read + Seek), offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    input
        .seek(SeekFrom::Start(offset))
        .and_then(|_| input.read_exact(bytes))
        .map_err(error)
}

fn error(e: io::Error) -> String {
    e.to_string()
}

#[cfg(test)]
#[path = "partition_move_tests.rs"]
mod tests;

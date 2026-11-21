//! Read-only recovery of NTFS file-provider WOF compressed content.
use crate::ntfs_streams::WofStreams;
use disk_core::wof::Algorithm;
use ntfs::{
    structured_values::NtfsFileAttributeFlags, Ntfs, NtfsAttributeType, NtfsFile, NtfsReadSeek,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, Write};

pub(crate) const TAG: u32 = 0x80000017;
pub(crate) const STREAM: &[u8] = b"W\0o\0f\0C\0o\0m\0p\0r\0e\0s\0s\0e\0d\0D\0a\0t\0a\0";

pub(crate) fn wof_chunk_size(data: &[u8]) -> Result<u64, String> {
    Ok(algorithm(data)?.chunk_size() as u64)
}

fn algorithm(data: &[u8]) -> Result<Algorithm, String> {
    let invalid = "NTFS WOF 压缩的提供程序、版本或算法不受支持。";
    if !matches!(data.len(), 24 | 28)
        || data[..4] != TAG.to_le_bytes()
        || data[4..8] != ((data.len() - 8) as u32).to_le_bytes()
        || data[8..12] != 1u32.to_le_bytes()
        || data[12..16] != 2u32.to_le_bytes()
        || data[16..20] != 1u32.to_le_bytes()
        || data.len() == 28 && data[24..28] != [0; 4]
    {
        return Err(invalid.into());
    }
    Algorithm::from_id(u32::from_le_bytes(data[20..24].try_into().unwrap()))
        .ok_or_else(|| invalid.into())
}

/// Check WOF's offset table in the same pass that hashes the stored stream.
/// The first chunk starts immediately after the table. The last ends at EOF.
pub(crate) struct WofOffsets {
    remaining: u64,
    width: usize,
    chunk: u64,
    last: u64,
    payload: u64,
    previous: u64,
    pending: [u8; 8],
    filled: usize,
}

impl WofOffsets {
    pub(crate) fn new(size: u64, stored: u64, chunk: u64) -> Result<Self, String> {
        let invalid = "NTFS WOF 压缩流的区块表长度无效。";
        if !matches!(chunk, 4096 | 8192 | 16384 | 32768) {
            return Err(invalid.into());
        }
        let remaining = size.div_ceil(chunk).checked_sub(1).ok_or(invalid)?;
        let width = if size < 1 << 32 { 4 } else { 8 };
        let payload = stored
            .checked_sub(remaining * width as u64)
            .ok_or(invalid)?;
        if payload <= remaining || payload > size {
            return Err(invalid.into());
        }
        Ok(Self {
            remaining,
            width,
            chunk,
            last: (size - 1) % chunk + 1,
            payload,
            previous: 0,
            pending: [0; 8],
            filled: 0,
        })
    }

    pub(crate) fn consume(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while self.remaining != 0 && !data.is_empty() {
            let count = (self.width - self.filled).min(data.len());
            self.pending[self.filled..self.filled + count].copy_from_slice(&data[..count]);
            data = &data[count..];
            self.filled += count;
            if self.filled == self.width {
                let offset = u64::from_le_bytes(self.pending);
                if offset <= self.previous
                    || offset >= self.payload
                    || offset - self.previous > self.chunk
                {
                    return Err(std::io::Error::other("NTFS WOF 压缩流的区块偏移无效。"));
                }
                self.previous = offset;
                self.filled = 0;
                self.remaining -= 1;
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        if self.remaining != 0 || self.payload - self.previous > self.last {
            return Err("NTFS WOF 压缩流末端区块的范围无效。".into());
        }
        Ok(())
    }
}

pub(crate) struct Plan {
    pub signature: [u8; 32],
    streams: WofStreams,
    algorithm: Algorithm,
}

impl Plan {
    pub(crate) fn read<R: Read + Seek>(
        reader: &mut R,
        file: &NtfsFile<'_>,
    ) -> Result<Self, String> {
        let invalid = "NTFS 重解析文件不含可核对的卷内 WOF 数据。";
        let flags = file.info().map_err(|e| e.to_string())?.file_attributes();
        if file.is_directory()
            || !flags.contains(NtfsFileAttributeFlags::REPARSE_POINT)
            || flags.intersects(NtfsFileAttributeFlags::ENCRYPTED | NtfsFileAttributeFlags::OFFLINE)
        {
            return Err(invalid.into());
        }
        let mut reparse = None;
        let mut attributes = file.attributes();
        let mut count = 0;
        while let Some(item) = attributes.next(reader) {
            count += 1;
            if count > 4096 {
                return Err(invalid.into());
            }
            let item = item.map_err(|e| e.to_string())?;
            let attribute = item.to_attribute().map_err(|e| e.to_string())?;
            if attribute.ty().map_err(|e| e.to_string())? != NtfsAttributeType::ReparsePoint {
                continue;
            }
            if reparse.is_some()
                || !attribute.is_resident()
                || attribute.name_length() != 0
                || !attribute.flags().is_empty()
                || !matches!(attribute.value_length(), 24 | 28)
            {
                return Err(invalid.into());
            }
            let mut data = vec![0; attribute.value_length() as usize];
            attribute
                .value(reader)
                .map_err(|e| e.to_string())?
                .read_exact(reader, &mut data)
                .map_err(|e| e.to_string())?;
            reparse = Some(data);
        }
        let data = reparse.ok_or(invalid)?;
        let algorithm = algorithm(&data)?;
        let streams = WofStreams::read(reader, file)?;
        WofOffsets::new(streams.size, streams.stored, algorithm.chunk_size() as u64)?;
        let mut h = Sha256::new();
        h.update(&data);
        h.update(streams.identity);
        Ok(Self {
            signature: h.finalize().into(),
            streams,
            algorithm,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.streams.size
    }

    fn table_digest(
        &self,
        reader: &mut (impl Read + Seek),
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<[u8; 32], String> {
        let mut offsets = WofOffsets::new(
            self.size(),
            self.streams.stored,
            self.algorithm.chunk_size() as u64,
        )?;
        let prefix = offsets.remaining * offsets.width as u64;
        let mut digest = Sha256::new();
        let mut buffer = [0; 16384];
        let mut position = 0;
        while position < prefix {
            tick()?;
            let take = (prefix - position).min(buffer.len() as u64) as usize;
            let bytes = &mut buffer[..take];
            self.streams.read_at(reader, position, bytes)?;
            offsets.consume(bytes).map_err(|e| e.to_string())?;
            digest.update(bytes);
            position += take as u64;
        }
        offsets.finish()?;
        Ok(digest.finalize().into())
    }

    pub(crate) fn extract<R: Read + Seek, W: Write>(
        &self,
        reader: &mut R,
        file: &NtfsFile<'_>,
        out: &mut W,
        limit: u64,
        mut tick: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<u64, String> {
        self.streams.verify_allocation(reader, file, || tick(0))?;
        let table = self.table_digest(reader, || tick(0))?;
        let chunk = self.algorithm.chunk_size();
        let offsets = WofOffsets::new(self.size(), self.streams.stored, chunk as u64)?;
        let prefix = offsets.remaining * offsets.width as u64;
        let mut stored = vec![0; chunk];
        let mut decoded = vec![0; chunk];
        let mut index_buffer = [0; 4096];
        let mut index_start = prefix;
        let mut before = 0;
        let take = self.size().min(limit);
        let mut done = 0;
        while done < take {
            tick(done)?;
            let width = offsets.width as u64;
            let index = done / chunk as u64 * width;
            let after = if index >= prefix {
                offsets.payload
            } else {
                let start = index / index_buffer.len() as u64 * index_buffer.len() as u64;
                if start != index_start {
                    let count = (prefix - start).min(index_buffer.len() as u64) as usize;
                    self.streams
                        .read_at(reader, start, &mut index_buffer[..count])?;
                    index_start = start;
                }
                let at = (index - index_start) as usize;
                let mut value = [0; 8];
                value[..offsets.width].copy_from_slice(&index_buffer[at..at + offsets.width]);
                u64::from_le_bytes(value)
            };
            let len = (self.size() - done).min(chunk as u64) as usize;
            let stored_len = after
                .checked_sub(before)
                .filter(|&n| n != 0 && n <= len as u64 && after <= offsets.payload)
                .ok_or("NTFS WOF 压缩流的区块偏移无效。")? as usize;
            self.streams
                .read_at(reader, prefix + before, &mut stored[..stored_len])?;
            self.algorithm
                .decode(&stored[..stored_len], &mut decoded[..len])?;
            let write = (take - done).min(len as u64) as usize;
            out.write_all(&decoded[..write])
                .map_err(|e| e.to_string())?;
            before = after;
            done += write as u64;
        }
        let ntfs = Ntfs::new(reader).map_err(|e| e.to_string())?;
        let fresh = ntfs
            .file(reader, file.file_record_number())
            .map_err(|e| e.to_string())?;
        let plan = Self::read(reader, &fresh)?;
        if fresh.sequence_number() != file.sequence_number()
            || fresh.flags() != file.flags()
            || plan.signature != self.signature
            || plan.table_digest(reader, || tick(done))? != table
        {
            return Err("读取期间 NTFS WOF 文件记录、卷身份或区块表已变化。".into());
        }
        plan.streams
            .verify_allocation(reader, &fresh, || tick(done))?;
        tick(done)?;
        Ok(done)
    }
}

#[cfg(test)]
#[test]
fn wof_offsets_check_monotonic_bounds_and_partial_reads() {
    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    let data = [400u32.to_le_bytes(), 887u32.to_le_bytes()].concat();
    for piece in data.chunks(3) {
        offsets.consume(piece).unwrap();
    }
    offsets.finish().unwrap();

    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    offsets.consume(&400u32.to_le_bytes()).unwrap();
    assert!(offsets.consume(&399u32.to_le_bytes()).is_err());
    let mut offsets = WofOffsets::new(8200, 901, 4096).unwrap();
    offsets.consume(&400u32.to_le_bytes()).unwrap();
    offsets.consume(&884u32.to_le_bytes()).unwrap();
    assert!(offsets.finish().is_err());

    let count = (1u64 << 32) / 32768 - 1;
    let mut wide = WofOffsets::new(1 << 32, count * 8 + 524288, 32768).unwrap();
    wide.consume(&400u64.to_le_bytes()).unwrap();
    assert_eq!(wide.remaining, count - 1);
    assert_eq!(wide.previous, 400);
}

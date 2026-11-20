//! Allocation-aware copies over already-authorized streams. This module never
//! opens files/devices, resolves paths, locks volumes or changes disk state.
//! Only proven free ranges may be omitted; everything else is retained.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
pub const CHUNK: usize = 1024 * 1024;
const MAX_RANGES: usize = 1_048_576;
const MAX_PROOF_READS: usize = 131_072;
const MAX_METADATA_BYTES: u64 = 512 * 1024 * 1024;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
pub(crate) fn stop(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("已用空间分析或复制已停止。".into())
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    #[default]
    AllSectors,
    UsedSpace,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}
impl Span {
    pub fn size(self) -> u64 {
        self.end - self.start
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReport {
    pub offset: u64,
    pub size: u64,
    pub filesystem: String,
    pub skipped_bytes: u64,
    pub note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_count: Option<u64>,
}
#[derive(Clone)]
pub struct Mark {
    offset: u64,
    length: usize,
    hash: [u8; 32],
}
#[derive(Clone, Default)]
pub struct Proof {
    marks: Vec<Mark>,
    bytes: u64,
}
impl Proof {
    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for mark in &self.marks {
            digest.update(mark.offset.to_le_bytes());
            digest.update((mark.length as u64).to_le_bytes());
            digest.update(mark.hash);
        }
        digest.finalize().into()
    }

    fn record(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.marks.len() >= MAX_PROOF_READS
            || self
                .bytes
                .checked_add(bytes.len() as u64)
                .is_none_or(|n| n > MAX_METADATA_BYTES)
        {
            return Err(io::Error::other("分配信息读取超过资源预算。"));
        }
        self.bytes += bytes.len() as u64;
        self.marks.push(Mark {
            offset,
            length: bytes.len(),
            hash: Sha256::digest(bytes).into(),
        });
        Ok(())
    }
    pub fn verify<R: Read + Seek>(
        &self,
        source: &mut R,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut bytes = vec![0; CHUNK];
        for m in &self.marks {
            stop(cancel)?;
            source.seek(SeekFrom::Start(m.offset)).map_err(err)?;
            source.read_exact(&mut bytes[..m.length]).map_err(err)?;
            if Sha256::digest(&bytes[..m.length]).as_slice() != m.hash {
                return Err("文件系统分配信息发生变化，已停止；不会重新尝试写入。".into());
            }
        }
        Ok(())
    }
}
/// Exact bounded view without read-ahead. Copying an allocated extent must not
/// inadvertently fetch an adjacent free cluster through the usual scan cache.
pub struct View<R> {
    inner: R,
    start: u64,
    size: u64,
    pos: u64,
}
impl<R: Seek> View<R> {
    pub fn new(mut inner: R, start: u64, size: u64) -> io::Result<Self> {
        let available = inner.seek(SeekFrom::End(0))?;
        Self::bounded(inner, start, size, available)
    }
    /// `available` must come from the caller's verified native device geometry,
    /// or an already bounded stream. Windows devices need not support SEEK_END.
    pub fn bounded(inner: R, start: u64, size: u64, available: u64) -> io::Result<Self> {
        if size == 0 || start.checked_add(size).is_none_or(|end| end > available) {
            return Err(io::Error::other("所选数据范围越界。"));
        }
        Ok(Self {
            inner,
            start,
            size,
            pos: 0,
        })
    }
    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }
}
impl<R: Read + Seek> Read for View<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = (self.size - self.pos).min(out.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let read = self.inner.read(&mut out[..n])?;
        self.pos += read as u64;
        Ok(read)
    }
}
impl<R: Write + Seek> Write for View<R> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.size - self.pos {
            return Err(io::Error::other("写入超出已确认的目标范围。"));
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.inner.write(bytes)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn seek(pos: u64, size: u64, from: SeekFrom) -> io::Result<u64> {
    let next = match from {
        SeekFrom::Start(n) => n as i128,
        SeekFrom::Current(n) => pos as i128 + n as i128,
        SeekFrom::End(n) => size as i128 + n as i128,
    };
    if next < 0 || next > size as i128 {
        return Err(io::Error::other("定位超出已确认的数据范围。"));
    }
    Ok(next as u64)
}
impl<R> Seek for View<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek(self.pos, self.size, from)?;
        Ok(self.pos)
    }
}
/// Records the exact bytes used to decide what may be skipped. Callers retain
/// source locks and verify this proof before and after the copy/backup.
pub struct Audit<'a, R> {
    view: View<&'a mut R>,
    base: u64,
    sector: u64,
    proof: &'a mut Proof,
    cancel: &'a AtomicBool,
}
impl<'a, R: Seek> Audit<'a, R> {
    pub fn new(
        source: &'a mut R,
        start: u64,
        size: u64,
        proof: &'a mut Proof,
        cancel: &'a AtomicBool,
    ) -> Result<Self, String> {
        Self::aligned(source, start, size, 512, proof, cancel)
    }
    pub fn aligned(
        source: &'a mut R,
        start: u64,
        size: u64,
        sector: u64,
        proof: &'a mut Proof,
        cancel: &'a AtomicBool,
    ) -> Result<Self, String> {
        if !matches!(sector, 512 | 4096) || start % sector != 0 || size % sector != 0 {
            return Err("分配信息读取范围未对齐设备扇区".into());
        }
        Ok(Self {
            view: View::new(source, start, size).map_err(err)?,
            base: start,
            sector,
            proof,
            cancel,
        })
    }
}
impl<R: Read + Seek> Read for Audit<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        stop(self.cancel).map_err(io::Error::other)?;
        let pos = self.view.pos;
        let n = out
            .len()
            .min(CHUNK - 2 * self.sector as usize)
            .min((self.view.size - pos) as usize);
        if n == 0 {
            return Ok(0);
        }
        let start = pos / self.sector * self.sector;
        let end = (pos + n as u64).div_ceil(self.sector) * self.sector;
        let mut bytes = vec![0; (end - start) as usize];
        self.view.seek(SeekFrom::Start(start))?;
        self.view.read_exact(&mut bytes)?;
        self.proof.record(start + self.base, &bytes)?;
        let offset = (pos - start) as usize;
        out[..n].copy_from_slice(&bytes[offset..offset + n]);
        self.view.seek(SeekFrom::Start(pos + n as u64))?;
        Ok(n)
    }
}
impl<R: Seek> Seek for Audit<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.view.seek(from)
    }
}
pub struct Builder {
    size: u64,
    sector: u64,
    free: Vec<Span>,
    pub proof: Proof,
    pub volumes: Vec<VolumeReport>,
}
impl Builder {
    pub fn new(size: u64, sector: u64) -> Result<Self, String> {
        if size == 0 || size > 1 << 43 || !matches!(sector, 512 | 4096) || size % sector != 0 {
            return Err("已用空间处理范围或扇区大小无效。".into());
        }
        Ok(Self {
            size,
            sector,
            free: vec![],
            proof: Proof::default(),
            volumes: vec![],
        })
    }
    /// Adjacent free clusters are merged immediately; the budget counts actual
    /// fragmented ranges, not the total number of clusters on a large volume.
    pub fn free(&mut self, start: u64, length: u64) -> Result<(), String> {
        let end = start
            .checked_add(length)
            .filter(|&end| end <= self.size)
            .ok_or("空闲区超出所选范围")?;
        let start = start.div_ceil(self.sector) * self.sector;
        let end = end / self.sector * self.sector;
        if start >= end {
            return Ok(());
        }
        if let Some(last) = self.free.last_mut() {
            if start < last.end {
                return Err("分配信息出现重复、重叠或非顺序空闲区。".into());
            }
            if start == last.end {
                last.end = end;
                return Ok(());
            }
        }
        if self.free.len() >= MAX_RANGES {
            return Err("文件系统过于碎片化，超过已用空间处理范围上限。".into());
        }
        self.free.push(Span { start, end });
        Ok(())
    }
    pub fn finish(self) -> Result<Plan, String> {
        // A bitmap claiming that its own records or other inspected metadata
        // are free is inconsistent. Do not silently skip those bytes.
        for m in &self.proof.marks {
            let index = self.free.partition_point(|f| f.end <= m.offset);
            if self
                .free
                .get(index)
                .is_some_and(|f| f.start < m.offset + m.length as u64)
            {
                return Err("分配位图将已读取的文件系统元数据标为空闲，已拒绝复制。".into());
            }
        }
        let skipped = self.free.iter().map(|s| s.size()).sum::<u64>();
        if skipped == self.size {
            return Err("已用空间计划没有保留任何文件系统数据。".into());
        }
        let mut digest = Sha256::new();
        digest.update(b"DiskStudio allocation plan v1\0");
        digest.update(self.size.to_le_bytes());
        digest.update(self.sector.to_le_bytes());
        for span in &self.free {
            digest.update(span.start.to_le_bytes());
            digest.update(span.end.to_le_bytes());
        }
        for m in &self.proof.marks {
            digest.update(m.offset.to_le_bytes());
            digest.update((m.length as u64).to_le_bytes());
            digest.update(m.hash);
        }
        Ok(Plan {
            size: self.size,
            sector: self.sector,
            free: self.free,
            proof: self.proof,
            volumes: self.volumes,
            skipped,
            digest: format!("{:x}", digest.finalize()),
        })
    }
}
pub struct Plan {
    pub size: u64,
    pub sector: u64,
    free: Vec<Span>,
    pub proof: Proof,
    pub volumes: Vec<VolumeReport>,
    pub skipped: u64,
    pub digest: String,
}

/// An independently parsed volume allocation map, expressed relative to the
/// volume. Required ranges include metadata that need not be read in full.
pub struct Allocation {
    pub filesystem: String,
    pub note: String,
    pub note_count: Option<u64>,
    pub free: Vec<Span>,
    required: Vec<Span>,
    size: u64,
}
impl Allocation {
    pub fn new(filesystem: impl Into<String>, size: u64) -> Self {
        Self {
            filesystem: filesystem.into(),
            note: String::new(),
            note_count: None,
            free: vec![],
            required: vec![],
            size,
        }
    }
    pub fn free(&mut self, start: u64, length: u64) -> Result<(), String> {
        let end = start
            .checked_add(length)
            .filter(|&n| n <= self.size)
            .ok_or("文件系统空闲范围越界")?;
        if start == end {
            return Ok(());
        }
        if let Some(last) = self.free.last_mut() {
            if start < last.end {
                return Err("文件系统空闲范围重叠或乱序".into());
            }
            if start == last.end {
                last.end = end;
                return Ok(());
            }
        }
        if self.free.len() >= MAX_RANGES {
            return Err("文件系统空闲碎片超过处理预算".into());
        }
        self.free.push(Span { start, end });
        Ok(())
    }
    pub fn require(&mut self, start: u64, length: u64) -> Result<(), String> {
        let end = start
            .checked_add(length)
            .filter(|&n| n <= self.size)
            .ok_or("文件系统元数据范围越界")?;
        if start == end || self.required.len() >= MAX_RANGES {
            return Err("文件系统元数据范围无效或超过预算".into());
        }
        self.required.push(Span { start, end });
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        for span in &self.required {
            let i = self.free.partition_point(|f| f.end <= span.start);
            if self.free.get(i).is_some_and(|f| f.start < span.end) {
                return Err("文件系统将必须保留的元数据标为空闲".into());
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Copying,
    Verifying,
}
impl Plan {
    /// Immutable, validated ranges; never accept replacement ranges from IPC.
    pub fn free_ranges(&self) -> &[Span] {
        &self.free
    }

    /// Digest every byte outside the proven-free ranges, including metadata.
    pub fn retained_digest<R: Read + Seek>(
        &self,
        source: &mut R,
        cancel: &AtomicBool,
        mut tick: impl FnMut(u64, u64),
    ) -> Result<String, String> {
        let mut digest = self.hash();
        let mut bytes = vec![0; CHUNK];
        let mut done = 0;
        for range in self.retained() {
            source.seek(SeekFrom::Start(range.start)).map_err(err)?;
            let mut left = range.size();
            while left > 0 {
                stop(cancel)?;
                let n = left.min(CHUNK as u64) as usize;
                source.read_exact(&mut bytes[..n]).map_err(err)?;
                digest.update(&bytes[..n]);
                left -= n as u64;
                done += n as u64;
                tick(done, self.copied());
            }
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    pub fn copied(&self) -> u64 {
        self.size - self.skipped
    }
    /// Worst-case payload with v3 zero-block markers, even with compression
    /// disabled. Chunks mixing allocated and free bytes can be stored in full.
    pub fn backup_payload_bound(&self, chunk: u64) -> Result<u64, String> {
        if chunk == 0 || chunk % self.sector != 0 {
            return Err("备份分块未对齐扇区".into());
        }
        let mut zero = 0;
        for free in &self.free {
            let start = free.start.div_ceil(chunk) * chunk;
            let end = free.end / chunk * chunk;
            zero += end.saturating_sub(start);
            if free.end == self.size && self.size % chunk != 0 && start <= end {
                zero += self.size % chunk;
            }
        }
        Ok(self.size - zero)
    }
    pub fn retained(&self) -> impl Iterator<Item = Span> + '_ {
        let mut position = 0;
        self.free
            .iter()
            .map(|f| f.start)
            .chain(std::iter::once(self.size))
            .enumerate()
            .filter_map(move |(i, end)| {
                let start = position;
                position = self.free.get(i).map_or(self.size, |f| f.end);
                (start < end).then_some(Span { start, end })
            })
    }
    fn hash(&self) -> Sha256 {
        let mut h = Sha256::new();
        h.update(b"DiskStudio retained extents SHA256 v1\0");
        h.update(self.digest.as_bytes());
        h
    }
    /// One ordered writer, one read-ahead worker, two queued 1MiB buffers.
    /// Gaps are never read or written. The digest binds the map and every copied
    /// byte; it is deliberately not described as a whole-device digest.
    pub fn copy_verified<R: Read + Seek + Send, W: Read + Write + Seek>(
        &self,
        source: &mut R,
        target: &mut W,
        cancel: Arc<AtomicBool>,
        sync: impl FnOnce(&mut W) -> Result<(), String>,
        mut tick: impl FnMut(Phase, u64, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        stop(&cancel)?;
        tick(Phase::Copying, 0, self.copied())?;
        let source_hash = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            let reader_cancel = cancel.clone();
            let worker = scope.spawn(move || {
                let read = || -> Result<(), String> {
                    for range in self.retained() {
                        let mut at = range.start;
                        source.seek(SeekFrom::Start(at)).map_err(err)?;
                        while at < range.end {
                            stop(&reader_cancel)?;
                            let len = (range.end - at).min(CHUNK as u64) as usize;
                            let mut bytes = vec![0; len];
                            source.read_exact(&mut bytes).map_err(err)?;
                            if tx.send(Ok((at, bytes))).is_err() {
                                return Ok(());
                            }
                            at += len as u64;
                        }
                    }
                    Ok(())
                };
                let mut read = read;
                if let Err(error) = read() {
                    let _ = tx.send(Err(error));
                }
            });
            let result = (|| {
                let mut hash = self.hash();
                let mut done = 0;
                while done < self.copied() {
                    stop(&cancel)?;
                    tick(Phase::Copying, done, self.copied())?;
                    let (at, bytes) = rx.recv().map_err(|_| "已用空间读取线程提前结束")??;
                    stop(&cancel)?;
                    target.seek(SeekFrom::Start(at)).map_err(err)?;
                    target.write_all(&bytes).map_err(err)?;
                    hash.update(&bytes);
                    done += bytes.len() as u64;
                }
                tick(Phase::Copying, done, self.copied())?;
                Ok::<_, String>(format!("{:x}", hash.finalize()))
            })();
            drop(rx);
            worker.join().map_err(|_| "已用空间读取线程异常退出")?;
            result
        })?;
        target.flush().map_err(err)?;
        sync(target)?;
        let mut digest = self.hash();
        let mut bytes = vec![0; CHUNK];
        let mut done = 0;
        for range in self.retained() {
            target.seek(SeekFrom::Start(range.start)).map_err(err)?;
            let mut left = range.size();
            while left > 0 {
                stop(&cancel)?;
                tick(Phase::Verifying, done, self.copied())?;
                let n = left.min(CHUNK as u64) as usize;
                target.read_exact(&mut bytes[..n]).map_err(err)?;
                digest.update(&bytes[..n]);
                done += n as u64;
                left -= n as u64;
            }
        }
        tick(Phase::Verifying, done, self.copied())?;
        let actual = format!("{:x}", digest.finalize());
        if actual != source_hash {
            return Err("已复制范围的 SHA-256 完整回读不一致。".into());
        }
        Ok(actual)
    }
    pub fn masked<'a, R: Read + Seek>(&'a self, source: R) -> Masked<'a, R> {
        Masked {
            source,
            plan: self,
            pos: 0,
        }
    }
}
/// Backup logical view: proven free ranges become zeroes, without source I/O.
/// The archive must record this policy and still hash its entire logical image.
pub struct Masked<'a, R> {
    source: R,
    plan: &'a Plan,
    pos: u64,
}
impl<R: Read + Seek> Read for Masked<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos == self.plan.size {
            return Ok(0);
        }
        let i = self.plan.free.partition_point(|f| f.end <= self.pos);
        let f = self.plan.free.get(i);
        let zero = f.is_some_and(|f| f.start <= self.pos);
        let end = f.map_or(self.plan.size, |f| if zero { f.end } else { f.start });
        let n = out
            .len()
            .min((end - self.pos).min(usize::MAX as u64) as usize);
        if zero {
            out[..n].fill(0);
        } else {
            self.source.seek(SeekFrom::Start(self.pos))?;
            self.source.read_exact(&mut out[..n])?;
        }
        self.pos += n as u64;
        Ok(n)
    }
}
impl<R> Seek for Masked<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek(self.pos, self.plan.size, from)?;
        Ok(self.pos)
    }
}

#[path = "allocation_plan.rs"]
mod inspection;
#[cfg(test)]
#[path = "used_space_tests.rs"]
mod tests;
pub use inspection::inspect;

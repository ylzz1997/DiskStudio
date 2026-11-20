//! Builds an NTFS partition from a checked live namespace and bounded streams.
use crate::{
    extfs::{self, Engine},
    ntfs_streams::{ENCRYPTED, LZNT1, SPARSE},
    partitions::merge::{self, metadata, Catalogue, RecordId, Stream},
    surface_scan::Range,
};
use std::{
    collections::BTreeMap,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread::{self, JoinHandle},
};

#[path = "ntfs_clone_target.rs"]
mod target;
use target::Target;

const TOOL: &str = "diskstudio-ntfs.exe";
const ASSETS: [(&str, &[u8]); 1] = [(TOOL, include_bytes!("../ntfs-engine/diskstudio-ntfs.exe"))];
const LIMIT: usize = 1024 * 1024;
const PACKET: usize = 65536;
const CREATE_DIRECTORY: u8 = 1;
const CREATE_FILE: u8 = 2;
const LINK_FILE: u8 = 3;
const SET_METADATA: u8 = 4;
const ROOT_STREAMS: u8 = 5;
const SET_SHORT_NAME: u8 = 6;
const PORTABLE_FLAGS: u32 = 0x2bb7;
const HAS_EXTENDED_ATTRIBUTES: u32 = 0x0004_0000;
const I30_INDEX_PRESENT: u32 = 0x1000_0000;

pub(crate) struct Plan {
    source: Catalogue,
    range: Range,
    serial: u64,
    cluster: u32,
    pub(crate) files: usize,
    pub(crate) directories: usize,
    pub(crate) bytes: u64,
}

fn err(error: impl std::fmt::Display) -> String {
    format!("NTFS 克隆：{error}")
}
fn check(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("操作已取消。".into());
    }
    Ok(())
}

pub(crate) fn is_ntfs(reader: &mut (impl Read + Seek)) -> Result<bool, String> {
    let mut boot = [0; 11];
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    reader.read_exact(&mut boot).map_err(err)?;
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    Ok(&boot[3..] == b"NTFS    ")
}

impl Plan {
    pub(crate) fn prepare(
        reader: &mut (impl Read + Seek),
        range: Range,
        serial: u64,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        if !matches!(range.sector, 512 | 4096)
            || range.offset % range.sector != 0
            || range.size % range.sector != 0
            || !(16 * 1024 * 1024..=2 * 1024_u64.pow(4)).contains(&range.size)
            || range.offset / range.sector > u64::from(u32::MAX)
            || serial == 0
        {
            return Err("目标范围超出 NTFS 按文件克隆支持的几何。".into());
        }
        let fs = ntfs::Ntfs::new(reader).map_err(err)?;
        if u64::from(fs.sector_size()) != range.sector || fs.serial_number() == serial {
            return Err("NTFS 源扇区大小或卷标识与克隆计划不一致。".into());
        }
        let source = merge::catalogue_file_clone(reader, cancel)?;
        Self::from_catalogue(source, range, serial, fs.cluster_size(), cancel)
    }

    fn from_catalogue(
        source: Catalogue,
        range: Range,
        serial: u64,
        source_cluster: u32,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        for alias in metadata::short_name_links(&source, cancel)?.into_values() {
            metadata::validate_short_name(alias)?;
        }
        let mut bytes = 0u64;
        let mut allocated_bytes = 0u64;
        let mut use_source_cluster = false;
        let mut extended_bytes = 0u64;
        let mut directory_count = 0;
        for (&id, node) in &source.nodes {
            check(cancel)?;
            let ea = metadata::extended_attributes(node)?;
            let object_id = node.metadata.get(&metadata::OBJECT_ID);
            let reparse = node.metadata.get(&metadata::REPARSE);
            let efs = node.metadata.get(&metadata::EFS);
            // Windows can also persist the $I30 flag in standard information.
            let allowed_flags = PORTABLE_FLAGS
                | metadata::REPARSE_ATTRIBUTE
                | metadata::ENCRYPTED_ATTRIBUTE
                | if node.directory { I30_INDEX_PRESENT } else { 0 }
                | if ea.is_some() {
                    HAS_EXTENDED_ATTRIBUTES
                } else {
                    0
                };
            if node.attributes & !allowed_flags != 0
                || object_id.is_some_and(|id| id.len() != 64)
                || node
                    .streams
                    .iter()
                    .any(|stream| !matches!(stream.storage_flags, 0 | LZNT1 | ENCRYPTED | SPARSE))
            {
                return Err("NTFS 按文件克隆遇到不受支持的属性或无效的对象 ID。".into());
            }
            if node.metadata.len()
                != 2 * usize::from(ea.is_some())
                    + usize::from(object_id.is_some())
                    + usize::from(reparse.is_some())
                    + usize::from(efs.is_some())
            {
                return Err("NTFS 按文件克隆遇到无法保留的元数据。".into());
            }
            if let Some((info, data)) = ea {
                metadata::visit_ea(info, data, |_, _, _| Ok(()))?;
                extended_bytes += (info.len() + data.len()) as u64;
            }
            extended_bytes += (object_id.map_or(0, Vec::len)
                + reparse.map_or(0, Vec::len)
                + efs.map_or(0, Vec::len)) as u64;
            let descriptor = source.descriptor(id)?;
            if descriptor.len() < 20 || descriptor[4..8] == [0; 4] || descriptor[8..12] == [0; 4] {
                return Err("NTFS 安全描述符缺少可迁移的所有者或组。".into());
            }
            directory_count += usize::from(node.directory);
            use_source_cluster |= node.index_compressed;
            for stream in &node.streams {
                if !stream.locator.name.is_empty() {
                    let units = stream
                        .locator
                        .name
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>();
                    merge::validate_component(&units)?;
                } else if node.directory {
                    return Err("NTFS 目录的无名数据流无法完整迁移。".into());
                }
                bytes = bytes
                    .checked_add(stream.payload_size())
                    .ok_or("NTFS 数据流容量溢出。")?;
                use_source_cluster |= stream.storage_flags != 0;
                let cluster = u64::from(source_cluster);
                let stored = match stream.storage_flags {
                    SPARSE => stream
                        .allocated_ranges
                        .iter()
                        .map(|range| (range.end - range.start).div_ceil(cluster) * cluster)
                        .sum(),
                    LZNT1 if cluster <= 4096 => {
                        // Reserve room for incompressible units, including the last one.
                        let unit = 16 * cluster;
                        stream.size.div_ceil(unit) * unit
                    }
                    LZNT1 => return Err("NTFS 压缩流的簇大小超过 4 KiB。".into()),
                    ENCRYPTED if stream.size != 0 => {
                        // The formatter temporarily stores the two-byte EFS padding trailer.
                        (stream.payload_size() + 2).next_multiple_of(cluster)
                    }
                    _ => stream.size,
                };
                allocated_bytes = allocated_bytes
                    .checked_add(stored)
                    .ok_or("NTFS 数据流分配容量溢出。")?;
            }
        }
        if allocated_bytes
            .checked_add(extended_bytes + 2 * 1024 * 1024)
            .is_none_or(|at| at >= range.size)
        {
            return Err("目标分区不足以存放 NTFS 活动内容和文件系统元数据。".into());
        }
        let files = source.names.len() - directory_count;
        Ok(Self {
            source,
            range,
            serial,
            cluster: if use_source_cluster {
                source_cluster
            } else {
                4096
            },
            files,
            directories: directory_count - 1,
            bytes,
        })
    }

    pub(crate) fn revalidate(
        &self,
        reader: &mut (impl Read + Seek),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let current = merge::catalogue_file_clone(reader, cancel)?;
        self.source.same_source(&current)
    }

    pub(crate) fn verify(
        &self,
        reader: &mut (impl Read + Seek),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let fs = ntfs::Ntfs::new(reader).map_err(err)?;
        reader.seek(SeekFrom::Start(28)).map_err(err)?;
        let mut start = [0; 4];
        reader.read_exact(&mut start).map_err(err)?;
        if fs.serial_number() != self.serial
            || u64::from(fs.sector_size()) != self.range.sector
            || fs.cluster_size() != self.cluster
            || fs.size().checked_add(self.range.sector) != Some(self.range.size)
            || u64::from(u32::from_le_bytes(start)) != self.range.offset / self.range.sector
            || !fs
                .volume_name(reader)
                .transpose()
                .map_err(err)?
                .is_some_and(|name| name.name() == "DISKCLONE")
        {
            return Err("目标 NTFS 的卷标识、几何或卷标与克隆计划不一致。".into());
        }
        self.source
            .verify_file_clone(&merge::catalogue_file_clone(reader, cancel)?)
    }

    pub(crate) fn engine() -> Result<Engine, String> {
        Engine::load("ntfs", &ASSETS)
    }

    pub(crate) fn write<R: Read + Seek, O: Read + Write + Seek>(
        &self,
        engine: &Engine,
        source: &mut R,
        output: O,
        sync: fn(&mut O) -> io::Result<()>,
        cancel: &AtomicBool,
        mut progress: impl FnMut(&str, u64, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let target = Target::new(output, self.range.size, sync);
        let mut pipe = Pipe::spawn(engine, target, cancel)?;
        let result = (|| {
            let mut header = Vec::from(*b"DSN6");
            header.extend_from_slice(&(self.range.sector as u32).to_le_bytes());
            header.extend_from_slice(&self.range.size.to_le_bytes());
            header.extend_from_slice(&(self.range.offset / self.range.sector).to_le_bytes());
            header.extend_from_slice(&self.serial.to_le_bytes());
            header.extend_from_slice(&self.cluster.to_le_bytes());
            pipe.answer(&header)?;
            self.populate(source, &mut pipe, &mut |done| {
                progress("写入 NTFS 文件", done, self.bytes)
            })?;
            pipe.request(None)?;
            Ok(())
        })();
        pipe.finish(result, |stage, now| progress(stage, now, self.range.size))
    }

    fn populate<R: Read + Seek, O: Read + Write + Seek>(
        &self,
        reader: &mut R,
        pipe: &mut Pipe<O>,
        tick: &mut impl FnMut(u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let root = self.source.names[&Vec::new()];
        let root_streams = &self.source.nodes[&root].streams;
        let mut primary = BTreeMap::from([(root, 0u32)]);
        let count = (self.source.names.len() - 1
            + self.source.nodes.len()
            + self.source.short_names.len()
            + usize::from(!root_streams.is_empty())) as u32;
        tick(0)?;
        pipe.send(&count.to_le_bytes())?;
        let mut index = 0;
        let mut done = 0;
        if !root_streams.is_empty() {
            index += 1;
            pipe.record(ROOT_STREAMS, &[], root_streams.len(), 0, 0)?;
            for stream in root_streams {
                self.stream(reader, root, stream, pipe, &mut done, tick)?;
            }
        }
        for (path, &id) in &self.source.names {
            let Some((name, parent)) = path.split_last() else {
                continue;
            };
            check(pipe.cancel)?;
            index += 1;
            let parent = primary[&self.source.names[parent]];
            if let Some(&existing) = primary.get(&id) {
                pipe.record(LINK_FILE, name, 0, parent, existing)?;
                continue;
            }
            primary.insert(id, index);
            let node = &self.source.nodes[&id];
            pipe.record(
                if node.directory {
                    CREATE_DIRECTORY
                } else {
                    CREATE_FILE
                },
                name,
                node.streams.len(),
                parent,
                0,
            )?;
            for stream in &node.streams {
                self.stream(reader, id, stream, pipe, &mut done, tick)?;
            }
        }
        for (path, &id) in &self.source.short_names {
            let (alias, parent) = path.split_last().ok_or("NTFS 短名缺少父目录。")?;
            pipe.record(
                SET_SHORT_NAME,
                alias,
                0,
                primary[&self.source.names[parent]],
                primary[&id],
            )?;
        }
        for (&id, &reference) in &primary {
            let node = &self.source.nodes[&id];
            let descriptor = self.source.descriptor(id)?;
            let ea = metadata::extended_attributes(node)?;
            let object_id = node.metadata.get(&metadata::OBJECT_ID);
            let reparse = node.metadata.get(&metadata::REPARSE);
            let efs = node.metadata.get(&metadata::EFS);
            pipe.record(SET_METADATA, &[], 0, 0, reference)?;
            let mut header = [0; 42];
            header[..4].copy_from_slice(&node.attributes.to_le_bytes());
            header[4..8].copy_from_slice(&(descriptor.len() as u32).to_le_bytes());
            for (field, time) in header[8..32].chunks_exact_mut(8).zip(node.times) {
                field.copy_from_slice(&time.to_le_bytes());
            }
            header[32..36]
                .copy_from_slice(&(ea.map_or(0, |(_, data)| data.len()) as u32).to_le_bytes());
            header[36] = u8::from(node.index_compressed);
            header[37] = u8::from(object_id.is_some())
                | (u8::from(node.link_version_flag) << 1)
                | (u8::from(efs.is_some()) << 2);
            header[38..42].copy_from_slice(&(reparse.map_or(0, Vec::len) as u32).to_le_bytes());
            pipe.send(&header)?;
            if let Some(data) = efs {
                pipe.send(&(data.len() as u32).to_le_bytes())?;
            }
            pipe.send(descriptor)?;
            if let Some((info, data)) = ea {
                pipe.send(info)?;
                pipe.send(data)?;
            }
            if let Some(id) = object_id {
                pipe.send(id)?;
            }
            if let Some(data) = reparse {
                pipe.send(data)?;
            }
            if let Some(data) = efs {
                pipe.send(data)?;
            }
        }
        if done != self.bytes {
            return Err("NTFS 数据流传输量与克隆计划不一致。".into());
        }
        tick(done)
    }

    fn stream<R: Read + Seek, O: Read + Write + Seek>(
        &self,
        reader: &mut R,
        id: RecordId,
        stream: &Stream,
        pipe: &mut Pipe<O>,
        done: &mut u64,
        tick: &mut impl FnMut(u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let name = &stream.locator.name;
        let mut header = [0; 12];
        header[..2].copy_from_slice(&((name.len() / 2) as u16).to_le_bytes());
        header[2..4].copy_from_slice(&stream.storage_flags.to_le_bytes());
        header[4..].copy_from_slice(&stream.size.to_le_bytes());
        pipe.send(&header)?;
        if !name.is_empty() {
            pipe.send(name)?;
        }
        let payload = stream.payload_size();
        let dense = (payload != 0).then_some(0..payload);
        let sparse = stream.storage_flags == SPARSE;
        let ranges = if sparse {
            pipe.send(&(stream.allocated_ranges.len() as u32).to_le_bytes())?;
            stream.allocated_ranges.as_slice()
        } else {
            dense.as_slice()
        };
        let cancel = pipe.cancel;
        let mut output = StreamOutput {
            pipe,
            size: payload,
            position: 0,
            sparse,
            ranges,
            buffer: Vec::with_capacity(PACKET),
            done,
            tick,
        };
        self.source
            .transfer_stream(reader, id, name, &mut output, cancel)?;
        if output.position != payload || !output.buffer.is_empty() || !output.ranges.is_empty() {
            return Err("NTFS 克隆工具未收到完整数据流。".into());
        }
        Ok(())
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Pipe<'a, O> {
    child: ChildGuard,
    stdout: BufReader<ChildStdout>,
    stdin: BufWriter<ChildStdin>,
    stderr: JoinHandle<Result<String, String>>,
    target: Target<O>,
    cancel: &'a AtomicBool,
    buffer: Vec<u8>,
}

impl<'a, O: Read + Write + Seek> Pipe<'a, O> {
    fn spawn(engine: &Engine, target: Target<O>, cancel: &'a AtomicBool) -> Result<Self, String> {
        check(cancel)?;
        let mut child = ChildGuard(
            engine
                .command(TOOL)?
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(err)?,
        );
        let stdout = BufReader::new(child.0.stdout.take().ok_or("NTFS 输出管道不可用。")?);
        let stdin = BufWriter::new(child.0.stdin.take().ok_or("NTFS 输入管道不可用。")?);
        let errors = child.0.stderr.take().ok_or("NTFS 错误管道不可用。")?;
        let stderr = thread::Builder::new()
            .spawn(move || extfs::collect(errors))
            .map_err(err)?;
        Ok(Self {
            child,
            stdout,
            stdin,
            stderr,
            target,
            cancel,
            buffer: vec![0; LIMIT],
        })
    }

    fn answer(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stdin.write_all(bytes).map_err(err)?;
        self.stdin.flush().map_err(err)
    }

    fn send(&mut self, record: &[u8]) -> Result<(), String> {
        self.request(Some(record))
    }

    fn record(
        &mut self,
        operation: u8,
        name: &[u16],
        streams: usize,
        parent: u32,
        source: u32,
    ) -> Result<(), String> {
        let mut header = [0; 12];
        header[..2].copy_from_slice(&[operation, name.len() as u8]);
        header[2..4].copy_from_slice(&(streams as u16).to_le_bytes());
        header[4..8].copy_from_slice(&parent.to_le_bytes());
        header[8..].copy_from_slice(&source.to_le_bytes());
        self.send(&header)?;
        if !name.is_empty() {
            self.send(
                &name
                    .iter()
                    .copied()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            )?;
        }
        Ok(())
    }

    /// Service target requests until the engine asks for this exact next packet.
    fn request(&mut self, packet: Option<&[u8]>) -> Result<(), String> {
        let mut header = [0; 13];
        loop {
            check(self.cancel)?;
            if self.stdout.read(&mut header[..1]).map_err(err)? == 0 {
                return if packet.is_none() {
                    Ok(())
                } else {
                    Err("NTFS 工具提前结束。".into())
                };
            }
            self.stdout.read_exact(&mut header[1..]).map_err(err)?;
            let offset = u64::from_le_bytes(header[1..9].try_into().unwrap());
            let count = u32::from_le_bytes(header[9..].try_into().unwrap()) as usize;
            match (header[0], offset, count) {
                (1, _, 1..=LIMIT) => {
                    self.target.read(offset, &mut self.buffer[..count])?;
                    self.stdin.write_all(&[0]).map_err(err)?;
                    self.stdin.write_all(&self.buffer[..count]).map_err(err)?;
                    self.stdin.flush().map_err(err)?;
                }
                (2, _, 1..=LIMIT) => {
                    self.stdout
                        .read_exact(&mut self.buffer[..count])
                        .map_err(err)?;
                    check(self.cancel)?;
                    self.target.write(offset, &self.buffer[..count])?;
                    self.answer(&[0])?;
                }
                (3, 0, 0) => {
                    self.target.flush()?;
                    self.answer(&[0])?;
                }
                (4, 0, 1..=LIMIT) if packet.is_some_and(|bytes| bytes.len() == count) => {
                    self.stdin.write_all(&[0]).map_err(err)?;
                    return self.answer(packet.unwrap());
                }
                _ => return Err("NTFS 工具返回未预期的请求。".into()),
            }
        }
    }

    fn finish(
        self,
        result: Result<(), String>,
        progress: impl FnMut(&str, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let Self {
            mut child,
            stdin,
            stdout,
            stderr,
            target,
            ..
        } = self;
        if result.is_err() {
            let _ = child.0.kill();
        }
        drop(stdin);
        drop(stdout);
        let status = child.0.wait().map_err(err)?;
        let logs = stderr.join().map_err(|_| "NTFS 工具日志读取失败。")??;
        if result.is_err() || !status.success() {
            return Err(format!(
                "{} {}",
                result.err().unwrap_or_else(|| "NTFS 工具未完成。".into()),
                extfs::tail(&logs)
            ));
        }
        target.finish(progress)
    }
}

struct StreamOutput<'p, 'a, O, F> {
    pipe: &'p mut Pipe<'a, O>,
    size: u64,
    position: u64,
    sparse: bool,
    ranges: &'p [std::ops::Range<u64>],
    buffer: Vec<u8>,
    done: &'p mut u64,
    tick: &'p mut F,
}
impl<O: Read + Write + Seek, F: FnMut(u64) -> Result<(), String>> Write
    for StreamOutput<'_, '_, O, F>
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() || self.position == self.size {
            return Ok(0);
        }
        let n = match self.ranges.split_first() {
            Some((range, remaining)) if self.position >= range.start => {
                if self.sparse && self.position == range.start {
                    let mut segment = [0; 16];
                    segment[..8].copy_from_slice(&range.start.to_le_bytes());
                    segment[8..].copy_from_slice(&(range.end - range.start).to_le_bytes());
                    self.pipe.send(&segment).map_err(io::Error::other)?;
                }
                let packet_start = self.position - self.buffer.len() as u64;
                let wanted = (range.end - packet_start).min(PACKET as u64) as usize;
                let n = bytes.len().min(wanted - self.buffer.len());
                self.buffer.extend_from_slice(&bytes[..n]);
                if self.buffer.len() == wanted {
                    self.pipe.send(&self.buffer).map_err(io::Error::other)?;
                    self.buffer.clear();
                }
                if self.position + n as u64 == range.end {
                    self.ranges = remaining;
                }
                n
            }
            _ => {
                let end = self.ranges.first().map_or(self.size, |range| range.start);
                let n = (end - self.position).min(bytes.len() as u64) as usize;
                if bytes[..n].iter().any(|&b| b != 0) {
                    return Err(io::Error::other("NTFS 稀疏范围中出现非零数据。"));
                }
                n
            }
        };
        self.position += n as u64;
        *self.done += n as u64;
        if self.buffer.is_empty() {
            (self.tick)(*self.done).map_err(io::Error::other)?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

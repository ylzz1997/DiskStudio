//! Bounded NTFS namespace and content proof for partition merge. The platform
//! must retain volume locks whenever a catalogue authorizes removal.

use crate::{ntfs_names, ntfs_scan::ScanRead, ntfs_streams};
use ntfs::structured_values::NtfsFileAttributeFlags;
use ntfs::{Ntfs, NtfsAttributeType, NtfsFile, NtfsFileFlags, NtfsReadSeek};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Read, Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(windows)]
#[path = "partition_merge_output_windows.rs"]
pub(crate) mod output;
#[path = "partition_merge_security.rs"]
mod security;

const ROOT: u64 = 5;
const ROOT_TXF_DATA_SIZE: usize = 56;
const EXTEND: u64 = 11;
const FIRST_USER_RECORD: u64 = 16;
const MAX_RECORDS: u64 = 4 * 1024 * 1024;
const MAX_LINKS: usize = 262144;
const MAX_ATTRIBUTES: usize = 4096;
const MAX_METADATA: u64 = 128 * 1024 * 1024;
const MAX_PATH_UNITS: usize = 32767;
const MAX_DEPTH: usize = 128;

pub(crate) type RelativePath = Vec<Vec<u16>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordId(pub u64, pub u16);

impl RecordId {
    fn of(file: &NtfsFile<'_>) -> Self {
        Self(file.file_record_number(), file.sequence_number())
    }
}

#[derive(Debug)]
pub(crate) struct Catalogue {
    volume_serial: u64,
    boot: [u8; 32],
    pub names: BTreeMap<RelativePath, RecordId>,
    pub short_names: BTreeMap<RelativePath, RecordId>,
    pub nodes: BTreeMap<RecordId, Node>,
    security: BTreeMap<u32, security::Descriptor>,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub directory: bool,
    pub attributes: u32,
    /// Creation, modified, and accessed FILETIME values.
    pub times: [u64; 3],
    checkpoint: (u64, Option<u64>),
    root_txf_data: Option<[u8; ROOT_TXF_DATA_SIZE]>,
    security: NodeSecurity,
    pub metadata: BTreeMap<u32, Vec<u8>>,
    pub streams: Vec<Stream>,
}

#[derive(Debug)]
enum NodeSecurity {
    Indexed(u32),
    Inline(security::Descriptor),
}

impl NodeSecurity {
    fn id(&self) -> Option<u32> {
        match self {
            Self::Indexed(id) => Some(*id),
            Self::Inline(_) => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Stream {
    pub locator: ntfs_streams::Identity,
    pub size: u64,
    pub sha256: [u8; 32],
    pub storage_flags: u16,
}

impl Catalogue {
    pub(crate) fn descriptor(&self, id: RecordId) -> Result<&[u8], String> {
        let node = self.nodes.get(&id).ok_or("合并清单中缺少文件记录。")?;
        Ok(self.security_for(node)?.bytes())
    }

    fn security_for<'a>(&'a self, node: &'a Node) -> Result<&'a security::Descriptor, String> {
        match &node.security {
            NodeSecurity::Indexed(id) => self
                .security
                .get(id)
                .ok_or_else(|| "合并清单中缺少安全描述符。".into()),
            NodeSecurity::Inline(descriptor) => Ok(descriptor),
        }
    }

    /// Windows must hold the bound source volume lock and provide an already
    /// opened destination stream. Path resolution belongs to the caller.
    pub(crate) fn transfer_stream<R: Read + Seek, W: Write>(
        &self,
        reader: &mut R,
        file_id: RecordId,
        name: &[u8],
        output: &mut W,
        cancel: &AtomicBool,
    ) -> Result<u64, String> {
        check(cancel)?;
        let source = self.nodes.get(&file_id).ok_or("缺少已核对的合并源文件。")?;
        let stream = source
            .streams
            .iter()
            .find(|stream| stream.locator.name == name)
            .ok_or("数据流不在已核对的合并清单中。")?;
        let ntfs = Ntfs::new(reader).map_err(err)?;
        if ntfs.serial_number() != self.volume_serial
            || crate::directory_metadata::boot(reader)? != self.boot
        {
            return Err("复制时源 NTFS 卷身份与合并清单不同。".into());
        }
        let file = ntfs.file(reader, file_id.0).map_err(err)?;
        let info = file.info().map_err(err)?;
        if RecordId::of(&file) != file_id
            || file.is_directory() != source.directory
            || !file.flags().contains(NtfsFileFlags::IN_USE)
            || (
                info.mft_record_modification_time().nt_timestamp(),
                info.usn(),
            ) != source.checkpoint
            || info.security_id().filter(|&id| id != 0) != source.security.id()
        {
            return Err("复制时源文件记录或安全标识已变化。".into());
        }
        let mut sink = DigestWriter::new(output);
        let done =
            ntfs_streams::extract_for_merge(reader, &file, &stream.locator, &mut sink, |_| {
                check(cancel)
            })?;
        let digest: [u8; 32] = sink.digest.finalize().into();
        if done != stream.size || digest != stream.sha256 {
            return Err("复制时源数据流 SHA-256 或字节数与合并清单不同。".into());
        }
        sink.output.flush().map_err(err)?;
        Ok(done)
    }

    pub(crate) fn same_source(&self, current: &Self) -> Result<(), String> {
        if self.volume_serial != current.volume_serial
            || self.boot != current.boot
            || self.names != current.names
            || self.short_names != current.short_names
        {
            return Err("源卷身份、名称或文件记录已经改变。".into());
        }
        self.verify_copy(current)?;
        if self.nodes.iter().any(|(id, node)| {
            current.nodes.get(id).is_none_or(|fresh| {
                node.checkpoint != fresh.checkpoint
                    || node.root_txf_data != fresh.root_txf_data
                    || node.security.id() != fresh.security.id()
                    || !node
                        .streams
                        .iter()
                        .map(|stream| &stream.locator)
                        .eq(fresh.streams.iter().map(|stream| &stream.locator))
            })
        }) {
            return Err("源文件记录或数据流布局已经改变。".into());
        }
        Ok(())
    }

    /// Compare an independently reread copy. File record numbers and security
    /// table IDs legitimately change across NTFS volumes.
    pub(crate) fn verify_copy(&self, copy: &Self) -> Result<(), String> {
        let mut identity = BTreeMap::new();
        let mut used = BTreeSet::new();
        if self.names.len() != copy.names.len() || self.short_names.len() != copy.short_names.len()
        {
            return Err("目标目录的完整路径或短名数量不同。".into());
        }
        for (path, source_id) in &self.names {
            let copy_id = *copy.names.get(path).ok_or("目标目录缺少源路径。")?;
            match identity.get(source_id) {
                Some(previous) if *previous == copy_id => continue,
                Some(_) => return Err("一个源文件的多个硬链接未保留同一身份。".into()),
                None if !used.insert(copy_id) => {
                    return Err("不同源文件在目标被错误地视为同一文件。".into())
                }
                None => {
                    identity.insert(*source_id, copy_id);
                }
            }
            let source = self.nodes.get(source_id).ok_or("缺少源文件记录。")?;
            let target = copy.nodes.get(&copy_id).ok_or("缺少目标文件记录。")?;
            let source_security = self.security_for(source)?;
            let target_security = copy.security_for(target)?;
            if source.directory != target.directory
                || source.attributes != target.attributes
                || source.times != target.times
                || source.metadata != target.metadata
                || !source_security.agrees(target_security)
                || !source
                    .streams
                    .iter()
                    .map(Stream::content)
                    .eq(target.streams.iter().map(Stream::content))
            {
                return Err("目标文件的数据流、属性、时间或安全描述符与源不同。".into());
            }
        }
        if self
            .short_names
            .iter()
            .any(|(path, id)| identity.get(id) != copy.short_names.get(path))
        {
            return Err("目标短名未绑定到正确的文件记录。".into());
        }
        Ok(())
    }
}

impl Stream {
    fn content(&self) -> (&[u8], u64, u16, &[u8; 32]) {
        (
            &self.locator.name,
            self.size,
            self.storage_flags,
            &self.sha256,
        )
    }
}

struct DigestWriter<W> {
    output: W,
    digest: Sha256,
}

impl<W> DigestWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: Sha256::new(),
        }
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn check(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("合并文件核验已停止。".into())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct Record {
    directory: bool,
    attributes: NtfsFileAttributeFlags,
    names: Vec<ntfs_names::Name>,
}

pub(crate) fn catalogue<R: Read + Seek>(
    reader: &mut R,
    subtree: Option<(RecordId, &[u16])>,
    cancel: &AtomicBool,
) -> Result<Catalogue, String> {
    crate::ntfs_allocation::inspect(reader, cancel)?;
    let boot = crate::directory_metadata::boot(reader)?;
    let mut reader = ScanRead::new(reader).map_err(err)?;
    let ntfs = Ntfs::new(&mut reader).map_err(err)?;
    let root = ntfs.root_directory(&mut reader).map_err(err)?;
    let root_id = RecordId::of(&root);
    let records = allocated_records(&ntfs, &mut reader, cancel)?;
    let extend = ntfs.file(&mut reader, EXTEND).map_err(err)?;
    let extend_id = RecordId::of(&extend);
    let metadata = records
        .get(&extend_id)
        .ok_or("NTFS $Extend 缺少活动目录记录。")?;
    if !metadata.directory
        || !metadata.attributes.contains(NtfsFileAttributeFlags::SYSTEM)
        || !matches!(metadata.names.as_slice(), [name]
        if name.parent == (root_id.0, root_id.1) && name.namespace != 2 && name.display() == "$Extend")
    {
        return Err("NTFS $Extend 的保留目录身份不匹配。".into());
    }
    let selected = subtree.map_or(root_id, |(id, _)| id);
    let namespace = build_namespace(
        &records,
        selected,
        subtree.is_none(),
        Some(extend_id),
        || check(cancel),
    )?;
    if let Some((id, name)) = subtree {
        verify_staged_directory(root_id, id, name, &records)?;
        verify_index(&mut reader, &root, &namespace, true, cancel)?;
    } else {
        verify_index(&mut reader, &extend, &namespace, true, cancel)?;
        for (&metadata_id, record) in &records {
            if record.directory && is_extend_metadata(metadata_id, extend_id, &records) {
                let file = ntfs.file(&mut reader, metadata_id.0).map_err(err)?;
                verify_index(&mut reader, &file, &namespace, true, cancel)?;
            }
        }
    }
    let mut budget = MAX_METADATA;
    let mut nodes = BTreeMap::new();
    for id in namespace.names.values().copied().collect::<BTreeSet<_>>() {
        check(cancel)?;
        let file = ntfs.file(&mut reader, id.0).map_err(err)?;
        if RecordId::of(&file) != id || !file.flags().contains(NtfsFileFlags::IN_USE) {
            return Err("合并清单中的活动文件记录已变化。".into());
        }
        if file.is_directory() {
            verify_index(&mut reader, &file, &namespace, subtree.is_none(), cancel)?;
        }
        let node = inspect_node(&mut reader, &file, &mut budget, cancel)
            .map_err(|error| format!("NTFS 文件记录 {}：{error}", id.0))?;
        nodes.insert(id, node);
    }
    let ids = nodes
        .values()
        .filter_map(|node| node.security.id())
        .collect::<BTreeSet<_>>();
    let security = if ids.is_empty() {
        BTreeMap::new()
    } else {
        read_security(&ntfs, &mut reader, &ids, cancel)?
    };
    Ok(Catalogue {
        volume_serial: ntfs.serial_number(),
        boot,
        names: namespace.names,
        short_names: namespace.short_names,
        nodes,
        security,
    })
}

fn verify_staged_directory(
    root: RecordId,
    directory: RecordId,
    expected_name: &[u16],
    records: &BTreeMap<RecordId, Record>,
) -> Result<(), String> {
    let record = records.get(&directory).ok_or("回读时缺少目标目录记录。")?;
    let mut long_names = record.names.iter().filter(|name| name.namespace != 2);
    if !record.directory
        || record
            .names
            .iter()
            .any(|name| name.parent != (root.0, root.1))
        || !long_names.next().is_some_and(|name| {
            name.raw
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .eq(expected_name.iter().copied())
        })
        || long_names.next().is_some()
    {
        return Err("回读时目标目录不在已确认卷根下或目录名称已变更。".into());
    }
    Ok(())
}

fn read_security<R: Read + Seek>(
    ntfs: &Ntfs,
    reader: &mut R,
    ids: &BTreeSet<u32>,
    cancel: &AtomicBool,
) -> Result<BTreeMap<u32, security::Descriptor>, String> {
    let secure = ntfs.file(reader, 9).map_err(err)?;
    if !secure.flags().contains(NtfsFileFlags::IN_USE) {
        return Err("NTFS 安全文件记录或属性标志无效。".into());
    }
    let mut attributes = secure.attributes();
    let mut descriptors = None;
    while let Some(item) = attributes.next(reader) {
        check(cancel)?;
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.ty().map_err(err)? == NtfsAttributeType::Data
            && ntfs_streams::attribute_name(attr.record_bytes())?
                == [b'$', 0, b'S', 0, b'D', 0, b'S', 0]
        {
            if !attr.flags().is_empty() || descriptors.is_some() {
                return Err("NTFS 安全描述符流重复或属性标志无效。".into());
            }
            let value = attr.value(reader).map_err(err)?;
            descriptors = Some(security::read_sds(&mut value.attach(reader), ids, || {
                check(cancel)
            })?);
        }
    }
    descriptors.ok_or_else(|| "NTFS 缺少 $Secure:$SDS 数据流。".into())
}

fn allocated_records<R: Read + Seek>(
    ntfs: &Ntfs,
    reader: &mut R,
    cancel: &AtomicBool,
) -> Result<BTreeMap<RecordId, Record>, String> {
    let mft = ntfs.file(reader, 0).map_err(err)?;
    let record_bytes = u64::from(ntfs.file_record_size());
    if !matches!(record_bytes, 512..=65536) || !record_bytes.is_power_of_two() {
        return Err("NTFS 文件记录大小超出分析范围。".into());
    }
    let mut data_bytes = None;
    let mut bitmap = None;
    for item in mft.attributes_raw() {
        let attribute = item.map_err(err)?;
        match attribute.ty().map_err(err)? {
            NtfsAttributeType::AttributeList => {
                return Err("合并暂不能验证使用属性列表的 $MFT。".into())
            }
            NtfsAttributeType::Data
                if attribute.name_length() == 0
                    && !attribute.is_resident()
                    && attribute.flags().is_empty() =>
            {
                if data_bytes.replace(attribute.value_length()).is_some() {
                    return Err("$MFT 数据范围重复。".into());
                }
            }
            NtfsAttributeType::Bitmap
                if attribute.name_length() == 0 && attribute.flags().is_empty() =>
            {
                if bitmap.is_some() || attribute.value_length() > MAX_RECORDS.div_ceil(8) {
                    return Err("$MFT 分配位图重复或超过分析容量。".into());
                }
                let mut bytes = vec![0; attribute.value_length() as usize];
                attribute
                    .value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut bytes)
                    .map_err(err)?;
                bitmap = Some(bytes);
            }
            _ => {}
        }
    }
    let bytes = data_bytes.ok_or("缺少 $MFT 数据范围。")?;
    let count = bytes / record_bytes;
    let bitmap = bitmap.ok_or("缺少 $MFT 分配位图。")?;
    if bytes % record_bytes != 0
        || !(FIRST_USER_RECORD..=MAX_RECORDS).contains(&count)
        || bitmap.len() < count.div_ceil(8) as usize
    {
        return Err("$MFT 的记录范围和分配位图不一致。".into());
    }
    let mut records = BTreeMap::new();
    let mut count_links = 0;
    let mut count_name_bytes = 0;
    for record in active_records(&bitmap) {
        check(cancel)?;
        if record >= count {
            return Err("$MFT 位图标记了超出文件表的记录。".into());
        }
        if record < FIRST_USER_RECORD && !matches!(record, ROOT | EXTEND) {
            continue;
        }
        let file = ntfs.file(reader, record).map_err(err)?;
        if !file.flags().contains(NtfsFileFlags::IN_USE) || file.sequence_number() == 0 {
            return Err("$MFT 位图与活动记录身份不一致。".into());
        }
        if ntfs_names::merge_base_reference(reader, &file)? != 0 {
            continue;
        }
        let names = if record == ROOT {
            vec![]
        } else {
            ntfs_names::read_for_merge(reader, &file)?
        };
        count_links += names.len();
        count_name_bytes += names.iter().map(|name| name.raw.len()).sum::<usize>();
        if count_links > MAX_LINKS
            || count_name_bytes as u64 > MAX_METADATA
            || names.is_empty() && record != ROOT
        {
            return Err("活动 NTFS 名称缺失或超过合并清单的容量。".into());
        }
        records.insert(
            RecordId::of(&file),
            Record {
                directory: file.is_directory(),
                attributes: file.info().map_err(err)?.file_attributes(),
                names,
            },
        );
    }
    Ok(records)
}

fn active_records(bitmap: &[u8]) -> impl Iterator<Item = u64> + '_ {
    bitmap.iter().enumerate().flat_map(|(byte, &value)| {
        (0..8)
            .filter(move |bit| value >> bit & 1 != 0)
            .map(move |bit| byte as u64 * 8 + bit)
    })
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Child {
    id: RecordId,
    raw: Vec<u16>,
    namespace: u8,
}

struct Namespace {
    names: BTreeMap<RelativePath, RecordId>,
    short_names: BTreeMap<RelativePath, RecordId>,
    children: BTreeMap<RecordId, BTreeSet<Child>>,
    extend: Option<RecordId>,
}

fn build_namespace(
    records: &BTreeMap<RecordId, Record>,
    selected: RecordId,
    full_volume: bool,
    extend: Option<RecordId>,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Namespace, String> {
    if !records
        .get(&selected)
        .is_some_and(|record| record.directory)
    {
        return Err("合并清单根目录标识无效。".into());
    }
    let mut children = BTreeMap::<RecordId, BTreeSet<Child>>::new();
    for (&id, record) in records {
        checkpoint()?;
        // The reserved $Extend root link is checked separately in the root index.
        if id.0 == EXTEND {
            continue;
        }
        for name in &record.names {
            let parent = RecordId(name.parent.0, name.parent.1);
            if parent.0 >= FIRST_USER_RECORD || matches!(parent.0, ROOT | EXTEND) {
                if !records.get(&parent).is_some_and(|record| record.directory) {
                    return Err("NTFS 文件名的父目录身份无法核对。".into());
                }
            }
            let raw = name
                .raw
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            if !children.entry(parent).or_default().insert(Child {
                id,
                raw,
                namespace: name.namespace,
            }) {
                return Err("NTFS 目录包含重复名称记录。".into());
            }
        }
    }
    let mut names = BTreeMap::new();
    let mut short_names = BTreeMap::new();
    names.insert(Vec::new(), selected);
    let mut queue = VecDeque::from([(Vec::new(), selected)]);
    let mut directories = BTreeSet::from([selected]);
    let mut visited = BTreeSet::from([selected]);
    let mut expanded_units = 0;
    while let Some((prefix, directory)) = queue.pop_front() {
        let Some(entries) = children.get(&directory) else {
            continue;
        };
        for child in entries {
            checkpoint()?;
            validate_component(&child.raw)?;
            let mut path = prefix.clone();
            path.push(child.raw.clone());
            let units = path.iter().map(|name| name.len() + 1).sum::<usize>();
            expanded_units += units;
            if units > MAX_PATH_UNITS
                || path.len() > MAX_DEPTH
                || (expanded_units as u64) * 2 > MAX_METADATA
            {
                return Err("NTFS 路径深度、长度或合并清单超过分析上限。".into());
            }
            if child.namespace == 2 {
                if short_names.insert(path, child.id).is_some() {
                    return Err("NTFS 目录中的短名重复。".into());
                }
                continue;
            }
            if names.insert(path.clone(), child.id).is_some() {
                return Err("NTFS 目录中的长名重复。".into());
            }
            visited.insert(child.id);
            if records[&child.id].directory {
                if !directories.insert(child.id) {
                    return Err("NTFS 目录存在多父项或循环。".into());
                }
                queue.push_back((path, child.id));
            }
        }
    }
    if short_names
        .iter()
        .any(|(path, id)| names.contains_key(path) || !visited.contains(id))
    {
        return Err("NTFS 短名冲突或未对应可迁移的文件。".into());
    }
    for (&id, record) in records {
        checkpoint()?;
        if id != selected
            && visited.contains(&id)
            && record
                .names
                .iter()
                .any(|name| !directories.contains(&RecordId(name.parent.0, name.parent.1)))
        {
            return Err("目标目录以外存在共享文件链接。".into());
        }
        if full_volume
            && !visited.contains(&id)
            && Some(id) != extend
            && !extend.is_some_and(|extend| is_extend_metadata(id, extend, records))
        {
            return Err(format!(
                "存在无法由卷根目录完整核验的活动 NTFS 文件记录 {}。",
                id.0
            ));
        }
    }
    Ok(Namespace {
        names,
        short_names,
        children,
        extend,
    })
}

// The vendor index iterator follows child VCNs itself. Bound its I/O, even if
// damaged entries contain a cycle that never yields an item to the caller.
struct IndexReader<'a, R> {
    reader: &'a mut R,
    remaining: usize,
    cancel: &'a AtomicBool,
}

impl<R> IndexReader<'_, R> {
    fn step(&mut self) -> io::Result<()> {
        check(self.cancel).map_err(io::Error::other)?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("NTFS 目录索引遍历超过分析预算。"))?;
        Ok(())
    }
}

impl<R: Read> Read for IndexReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.step()?;
        self.reader.read(bytes)
    }
}

impl<R: Seek> Seek for IndexReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.step()?;
        self.reader.seek(position)
    }
}

fn verify_index<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    namespace: &Namespace,
    full_volume: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let id = RecordId::of(file);
    let expected = namespace.children.get(&id);
    let expected_count = expected.map_or(0, BTreeSet::len);
    let mut bounded = IndexReader {
        reader,
        remaining: 65536 + expected_count * 96,
        cancel,
    };
    let index = file.directory_index(&mut bounded).map_err(err)?;
    let size = index.index_record_size();
    if size < u32::from(file.ntfs().sector_size()) || size > 65536 || !size.is_power_of_two() {
        return Err("NTFS 目录索引记录大小越界。".into());
    }
    let mut entries = index.entries();
    let mut seen = BTreeSet::new();
    let mut system = BTreeSet::new();
    while let Some(entry) = entries.next(&mut bounded) {
        check(cancel)?;
        let entry = entry.map_err(err)?;
        let key = entry
            .key()
            .ok_or("NTFS 目录索引缺少文件名键。")?
            .map_err(err)?;
        let reference = entry.file_reference();
        let child_id = RecordId(reference.file_record_number(), reference.sequence_number());
        let parent = key.parent_directory_reference();
        if RecordId(parent.file_record_number(), parent.sequence_number()) != id {
            return Err("NTFS 目录索引键引用了不同的父目录。".into());
        }
        let raw = key
            .name()
            .0
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        if full_volume && id.0 == ROOT && child_id.0 < FIRST_USER_RECORD {
            if !reserved_root_object(child_id.0, &raw)
                || child_id.0 == EXTEND && Some(child_id) != namespace.extend
                || !system.insert(child_id.0)
            {
                return Err("NTFS 根目录的保留文件标识不匹配。".into());
            }
            continue;
        }
        let child = Child {
            id: child_id,
            raw,
            namespace: key.namespace() as u8,
        };
        if !expected.is_some_and(|entries| entries.contains(&child)) || !seen.insert(child) {
            return Err("NTFS 目录索引与活动名称记录不同。".into());
        }
    }
    if seen.len() != expected_count || full_volume && id.0 == ROOT && !system.contains(&EXTEND) {
        return Err("NTFS 目录索引未覆盖所有活动名称。".into());
    }
    Ok(())
}

fn reserved_root_object(number: u64, name: &[u16]) -> bool {
    let expected = match number {
        0 => "$MFT",
        1 => "$MFTMirr",
        2 => "$LogFile",
        3 => "$Volume",
        4 => "$AttrDef",
        ROOT => ".",
        6 => "$Bitmap",
        7 => "$Boot",
        8 => "$BadClus",
        9 => "$Secure",
        10 => "$UpCase",
        11 => "$Extend",
        _ => return false,
    };
    name.iter().copied().eq(expected.encode_utf16())
}

fn validate_component(name: &[u16]) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 255
        || name.last().is_some_and(|ch| matches!(ch, 0x20 | 0x2e))
        || name
            .iter()
            .any(|&ch| ch < 32 || matches!(ch, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124))
    {
        return Err("NTFS 名称无法作为保留原名的安全 Windows 路径。".into());
    }
    // Windows reserves the stem of these names even when an extension exists.
    let basename = name.split(|ch| *ch == 0x2e).next().unwrap_or_default();
    let basename = String::from_utf16_lossy(basename)
        .trim_end()
        .to_ascii_uppercase();
    if matches!(
        basename.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        basename.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }) {
        return Err("NTFS 名称使用 Windows 保留设备名。".into());
    }
    Ok(())
}

fn is_extend_metadata(
    mut id: RecordId,
    extend: RecordId,
    records: &BTreeMap<RecordId, Record>,
) -> bool {
    for _ in 0..MAX_DEPTH {
        let Some(item) = records.get(&id) else {
            return false;
        };
        let [name] = item.names.as_slice() else {
            return false;
        };
        if name.namespace == 2 {
            return false;
        }
        let parent = RecordId(name.parent.0, name.parent.1);
        if !item.attributes.contains(NtfsFileAttributeFlags::SYSTEM) {
            // Windows also formats the TxF base log and CLFS log containers
            // with ARCHIVE alone. Bind their exact names below both reserved
            // directories, whose indexes are verified like other directories.
            return !item.directory
                && item.attributes == NtfsFileAttributeFlags::ARCHIVE
                && txf_log_name(&name.display())
                && system_directory_parent(parent, "$TxfLog", records)
                    .and_then(|id| system_directory_parent(id, "$RmMetadata", records))
                    == Some(extend);
        }
        if parent == extend {
            return matches!(
                name.display().as_str(),
                "$ObjId" | "$Quota" | "$Reparse" | "$UsnJrnl" | "$RmMetadata" | "$Deleted"
            );
        }
        id = parent;
    }
    false
}

fn txf_log_name(name: &str) -> bool {
    name == "$TxfLog.blf"
        || name
            .strip_prefix("$TxfLogContainer")
            .is_some_and(|id| id.len() == 20 && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn system_directory_parent(
    id: RecordId,
    expected: &str,
    records: &BTreeMap<RecordId, Record>,
) -> Option<RecordId> {
    let directory = records.get(&id)?;
    let [name] = directory.names.as_slice() else {
        return None;
    };
    (directory.directory
        && directory
            .attributes
            .contains(NtfsFileAttributeFlags::SYSTEM)
        && name.namespace != 2
        && name.display() == expected)
        .then_some(RecordId(name.parent.0, name.parent.1))
}

fn inspect_node<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    budget: &mut u64,
    cancel: &AtomicBool,
) -> Result<Node, String> {
    let info = file.info().map_err(err)?;
    let mut attributes = None;
    let mut inline_security = None;
    let mut root_txf_data = None;
    if crate::ntfs_compression::is_reparse(reader, file)? {
        return Err("合并无法迁移重解析点、离线文件或 EFS 加密文件。".into());
    }
    let mut metadata = BTreeMap::new();
    let mut attrs = file.attributes();
    let mut standard_count = 0;
    let mut seen = 0;
    while let Some(item) = attrs.next(reader) {
        check(cancel)?;
        seen += 1;
        if seen > MAX_ATTRIBUTES {
            return Err("NTFS 文件属性超过分析上限。".into());
        }
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        let ty = attr.ty().map_err(err)?;
        match ty {
            NtfsAttributeType::StandardInformation => {
                standard_count += 1;
                if !attr.is_resident() || !matches!(attr.value_length(), 48 | 72) {
                    return Err("NTFS 3.1 文件标准属性大小无效。".into());
                }
                let length = attr.value_length() as usize;
                let mut raw = [0; 72];
                attr.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut raw[..length])
                    .map_err(err)?;
                attributes = Some(u32::from_le_bytes(raw[32..36].try_into().unwrap()));
                if raw[36..length.min(52)].iter().any(|byte| *byte != 0) {
                    return Err("文件使用了暂未迁移的 NTFS 版本、类或配额标识。".into());
                }
            }
            NtfsAttributeType::AttributeList
            | NtfsAttributeType::FileName
            | NtfsAttributeType::Data => {}
            NtfsAttributeType::LoggedUtilityStream
                if file.file_record_number() == ROOT && file.is_directory() =>
            {
                // The default TxF resource manager uses the volume root. Its
                // transaction record remains on that volume and is compared
                // only when checking that the locked source has not changed.
                if root_txf_data.is_some()
                    || !attr.is_resident()
                    || !attr.flags().is_empty()
                    || attr.value_length() != ROOT_TXF_DATA_SIZE as u64
                    || ntfs_streams::attribute_name(attr.record_bytes())?
                        != [
                            b'$', 0, b'T', 0, b'X', 0, b'F', 0, b'_', 0, b'D', 0, b'A', 0, b'T', 0,
                            b'A', 0,
                        ]
                {
                    return Err("NTFS 卷根的 TxF 元数据无法核验。".into());
                }
                let mut payload = [0; ROOT_TXF_DATA_SIZE];
                attr.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut payload)
                    .map_err(err)?;
                root_txf_data = Some(payload);
            }
            NtfsAttributeType::SecurityDescriptor => {
                let size = attr.value_length();
                if attr.name_length() != 0
                    || !attr.flags().is_empty()
                    || !(20..=128 * 1024).contains(&size)
                    || size > *budget
                    || inline_security.is_some()
                {
                    return Err("文件的内嵌安全描述符缺失、重复或范围无效。".into());
                }
                let mut payload = vec![0; size as usize];
                attr.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut payload)
                    .map_err(err)?;
                inline_security = Some(security::Descriptor::read(payload)?);
                *budget -= size;
            }
            NtfsAttributeType::IndexRoot
            | NtfsAttributeType::IndexAllocation
            | NtfsAttributeType::Bitmap
                if file.is_directory()
                    && ntfs_streams::attribute_name(attr.record_bytes())?
                        == [b'$', 0, b'I', 0, b'3', 0, b'0', 0] => {}
            NtfsAttributeType::ObjectId
            | NtfsAttributeType::EAInformation
            | NtfsAttributeType::EA => {
                let len = attr.value_length();
                if attr.name_length() != 0
                    || !attr.flags().is_empty()
                    || len > 65536
                    || len > *budget
                {
                    return Err("要迁移的 NTFS 扩展元数据范围无效。".into());
                }
                let mut contents = vec![0; len as usize];
                attr.value(reader)
                    .map_err(err)?
                    .read_exact(reader, &mut contents)
                    .map_err(err)?;
                if metadata.insert(ty as u32, contents).is_some() {
                    return Err("NTFS 扩展元数据属性重复。".into());
                }
                *budget -= len;
            }
            _ => return Err(format!("合并无法完整核对 NTFS 文件属性 {ty}。")),
        }
    }
    if standard_count != 1 {
        return Err("NTFS 文件标准属性缺失或重复。".into());
    }
    let attributes = attributes.ok_or("缺少 NTFS 文件属性标志。")?;
    if attributes & 0x5400 != 0 {
        return Err("合并无法迁移重解析点、离线文件或 EFS 加密文件。".into());
    }
    let mut streams = Vec::new();
    for stream in ntfs_streams::enumerate_for_merge(reader, file)? {
        check(cancel)?;
        if let Some(reason) = stream.reason {
            return Err(reason);
        }
        let mut hash = DigestWriter::new(io::sink());
        let len =
            ntfs_streams::extract_for_merge(reader, file, &stream.identity, &mut hash, |_| {
                check(cancel)
            })?;
        if len != stream.size {
            return Err("NTFS 数据流实际字节数与清单不一致。".into());
        }
        streams.push(Stream {
            locator: stream.identity,
            size: stream.size,
            sha256: hash.digest.finalize().into(),
            storage_flags: stream.storage_flags,
        });
    }
    if !file.is_directory() && !streams.iter().any(|stream| stream.locator.name.is_empty()) {
        return Err("普通 NTFS 文件的未命名数据流缺失。".into());
    }
    Ok(Node {
        directory: file.is_directory(),
        attributes,
        times: [
            info.creation_time().nt_timestamp(),
            info.modification_time().nt_timestamp(),
            info.access_time().nt_timestamp(),
        ],
        checkpoint: (
            info.mft_record_modification_time().nt_timestamp(),
            info.usn(),
        ),
        root_txf_data,
        security: match (info.security_id().filter(|&id| id != 0), inline_security) {
            (Some(id), None) if id >= 0x100 => NodeSecurity::Indexed(id),
            (None, Some(descriptor)) => NodeSecurity::Inline(descriptor),
            _ => return Err("文件没有唯一且可核验的 NTFS 安全描述符。".into()),
        },
        metadata,
        streams,
    })
}

#[cfg(test)]
#[path = "partition_merge_tests.rs"]
mod tests;

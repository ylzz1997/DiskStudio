//! Btrfs metadata and checksummed data over caller-bounded device streams.
use btrfs_disk::{superblock::ChecksumType, util::btrfs_csum_data as crc32c};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

#[path = "btrfs_allocation.rs"]
mod allocation;
#[path = "btrfs_compression.rs"]
mod compression;
#[path = "btrfs_file.rs"]
mod file;
#[path = "btrfs_members.rs"]
mod members;
#[path = "btrfs_resize.rs"]
mod resize;
pub use allocation::{allocation_for_copy, CopyAllocation};
pub use file::FileInfo;
pub use members::MAX_MEMBERS;
use members::{check_chunk_ranges, chunks_consistent, Chunk, Device, Members};
pub use resize::{plan_resize, resize, ResizePlan};

const SUPER_BYTES: usize = 4096;
const SUPER_OFFSETS: [u64; 3] = [64 * 1024, 64 * 1024 * 1024, 256 * 1024 * 1024 * 1024];
pub const RECOVERY_SUPERBLOCK_BYTES: usize = SUPER_BYTES;
const MAGIC: &[u8; 8] = b"_BHRfS_M";
const SYSTEM_ARRAY: usize = 0x32b;
const BACKUP_ARRAY: usize = SYSTEM_ARRAY + 2048;
const BACKUP_BYTES: usize = 168;
const HEADER: usize = 0x65;
const KEY_BYTES: usize = 17;
const MAX_NODES: usize = 131_072;
const CHUNK_TREE: u64 = 3;
const DEVICE_TREE: u64 = 4;
const ROOT_TREE: u64 = 1;
const CHUNK_ITEM: u8 = 228;
const DEVICE_ITEM: u8 = 216;
const DEVICE_EXTENT: u8 = 204;
const PERSISTENT_ITEM: u8 = 249;
const ROOT_ITEM: u8 = 132;
const INODE_ITEM: u8 = 1;
const DIR_INDEX: u8 = 96;
const SYSTEM: u64 = 2;
const METADATA: u64 = 4;
const DUP: u64 = 32;

fn checksum_matches(algorithm: ChecksumType, data: &[u8], expected: &[u8]) -> bool {
    match algorithm {
        ChecksumType::Crc32 => expected.starts_with(&crc32c(data).to_le_bytes()),
        ChecksumType::Unknown(_) => false,
        _ => expected.starts_with(&algorithm.compute(data)),
    }
}

fn word<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    bytes
        .get(offset..offset + N)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "Btrfs 元数据长度不足。".into())
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(word(bytes, offset)?))
}

fn le32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(word(bytes, offset)?))
}

fn le64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(word(bytes, offset)?))
}

fn file_tree(id: u64) -> bool {
    id == 5 || (256..=u64::MAX - 255).contains(&id)
}

fn err(error: std::io::Error) -> String {
    error.to_string()
}

fn read_at(reader: &mut (impl Read + Seek), at: u64, out: &mut [u8]) -> Result<(), String> {
    reader.seek(SeekFrom::Start(at)).map_err(err)?;
    reader.read_exact(out).map_err(err)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub object_id: u64,
    pub kind: u8,
    pub offset: u64,
}

impl Key {
    fn parse(bytes: &[u8], at: usize) -> Result<Self, String> {
        Ok(Self {
            object_id: le64(bytes, at)?,
            kind: word::<1>(bytes, at + 8)?[0],
            offset: le64(bytes, at + 9)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockRef {
    logical: u64,
    generation: u64,
    level: u8,
}

#[derive(Clone, Copy)]
struct Backup {
    root: BlockRef,
    chunk: BlockRef,
    extent: BlockRef,
    checksum: BlockRef,
    size: u64,
    devices: u64,
}

impl Backup {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let reference = |index: usize| -> Result<BlockRef, String> {
            Ok(BlockRef {
                logical: le64(bytes, index * 16)?,
                generation: le64(bytes, index * 16 + 8)?,
                level: word::<1>(bytes, 152 + index)?[0],
            })
        };
        Ok(Self {
            root: reference(0)?,
            chunk: reference(1)?,
            extent: reference(2)?,
            checksum: reference(5)?,
            size: le64(bytes, 96)?,
            devices: le64(bytes, 112)?,
        })
    }
}

struct Super {
    identity: [u8; 32],
    shared: [u8; 32],
    fsid: [u8; 16],
    algorithm: ChecksumType,
    device: Device,
    device_count: usize,
    total_size: u64,
    generation: u64,
    sector_size: u32,
    node_size: u32,
    chunk_root: BlockRef,
    root: BlockRef,
    bootstrap: BTreeMap<u64, Chunk>,
    backups: Vec<Backup>,
}

impl Super {
    fn parse(bytes: &[u8; SUPER_BYTES], at: u64, length: u64) -> Result<Self, String> {
        let algorithm = ChecksumType::from_raw(le16(bytes, 0xc4)?);
        if le64(bytes, 0x30)? != at || matches!(algorithm, ChecksumType::Unknown(_)) {
            return Err("Btrfs 超级块位置或校验算法不受支持。".into());
        }
        if !checksum_matches(algorithm, &bytes[32..], &bytes[..32]) {
            return Err("Btrfs 超级块校验值不一致。".into());
        }
        if le64(bytes, 0x60)? != 0 {
            return Err("Btrfs 存在待回放日志，请先在 Linux 干净卸载。".into());
        }
        // Unknown features that alter metadata layout cannot be opened by an
        // older reader. Known compression flags don't change tree geometry.
        const SUPPORTED_INCOMPAT: u64 = 0x37f
            | btrfs_disk::raw::BTRFS_FEATURE_INCOMPAT_RAID1C34 as u64
            | btrfs_disk::raw::BTRFS_FEATURE_INCOMPAT_RAID56 as u64;
        let device_count = le64(bytes, 0x88)?;
        if !(1..=MAX_MEMBERS as u64).contains(&device_count)
            || le64(bytes, 0xbc)? & !SUPPORTED_INCOMPAT != 0
        {
            return Err("Btrfs 成员数量或文件系统特性暂不支持。".into());
        }
        let fsid = word(bytes, 0x20)?;
        let generation = le64(bytes, 0x48)?;
        let sector_size = le32(bytes, 0x90)?;
        let node_size = le32(bytes, 0x94)?;
        let device = Device::parse(&bytes[0xc9..0x12b])?;
        let total_size = le64(bytes, 0x70)?;
        if fsid == [0; 16]
            || device.uuid == [0; 16]
            || word::<16>(bytes, 0x11b)? != fsid
            || generation == 0
            || device.id == 0
            || !(SUPER_OFFSETS[0] + SUPER_BYTES as u64..=length).contains(&device.size)
            || total_size < device.size
            || device.allocated > device.size
            || le64(bytes, 0x78)? > total_size
            || !matches!(sector_size, 4096 | 8192 | 16384 | 32768 | 65536)
            || !node_size.is_power_of_two()
            || !(sector_size..=65536).contains(&node_size)
            || device.size % u64::from(sector_size) != 0
        {
            return Err("Btrfs 设备身份、容量或块大小无效。".into());
        }
        let chunk_root = BlockRef {
            logical: le64(bytes, 0x58)?,
            generation: le64(bytes, 0xa4)?,
            level: bytes[0xc7],
        };
        let root = BlockRef {
            logical: le64(bytes, 0x50)?,
            generation,
            level: bytes[0xc6],
        };
        let array_len = le32(bytes, 0xa0)? as usize;
        if !(KEY_BYTES + 80..=2048).contains(&array_len) {
            return Err("Btrfs 系统块组数组长度无效。".into());
        }
        let mut raw = &bytes[SYSTEM_ARRAY..SYSTEM_ARRAY + array_len];
        let mut bootstrap = BTreeMap::new();
        while !raw.is_empty() {
            let key = Key::parse(raw, 0)?;
            let stripes = usize::from(le16(raw, KEY_BYTES + 44)?);
            let size = KEY_BYTES + 48 + 32 * stripes;
            let entry = raw.get(KEY_BYTES..size).ok_or("Btrfs 系统块组截断。")?;
            let chunk = Chunk::parse(key, entry, sector_size)?;
            if chunk.kind & SYSTEM == 0 || bootstrap.insert(key.offset, chunk).is_some() {
                return Err("Btrfs 系统块组重复或类型错误。".into());
            }
            raw = &raw[size..];
        }
        let mut identity = Sha256::new();
        identity.update(&bytes[32..48]);
        identity.update(&bytes[56..]);
        // The checksum, physical mirror position and embedded device item
        // differ between members. All volume-wide fields must agree.
        let mut shared = Sha256::new();
        shared.update(&bytes[32..48]);
        shared.update(&bytes[56..0xc9]);
        shared.update(&bytes[0x12b..]);
        let backups = bytes[BACKUP_ARRAY..BACKUP_ARRAY + 4 * BACKUP_BYTES]
            .chunks_exact(BACKUP_BYTES)
            .map(Backup::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            identity: identity.finalize().into(),
            shared: shared.finalize().into(),
            fsid,
            algorithm,
            device,
            device_count: device_count as usize,
            total_size,
            generation,
            sector_size,
            node_size,
            chunk_root,
            root,
            bootstrap,
            backups,
        })
    }
}

struct Item {
    key: Key,
    start: usize,
    end: usize,
}

enum Entries {
    Children(Vec<(Key, BlockRef)>),
    Items(Vec<Item>),
}

struct Node {
    bytes: Vec<u8>,
    entries: Entries,
}

impl Node {
    fn parse(
        bytes: Vec<u8>,
        expected: BlockRef,
        owner: u64,
        superblock: &Super,
    ) -> Result<Self, String> {
        let actual_owner = le64(&bytes, 88)?;
        // Snapshot file trees may still reference nodes from another file tree.
        let owner_matches = actual_owner == owner || (file_tree(owner) && file_tree(actual_owner));
        if !checksum_matches(superblock.algorithm, &bytes[32..], &bytes[..32])
            || word::<16>(&bytes, 32)? != superblock.fsid
            || le64(&bytes, 48)? != expected.logical
            || le64(&bytes, 80)? != expected.generation
            || !owner_matches
            || bytes[100] != expected.level
        {
            return Err("Btrfs 节点校验、归属或代次不一致。".into());
        }
        let count = le32(&bytes, 96)? as usize;
        let payload = &bytes[HEADER..];
        let leaf = expected.level == 0;
        let stride = if leaf { 25 } else { 33 };
        if count > payload.len() / stride || (!leaf && count == 0) {
            return Err("Btrfs 节点条目数量无效。".into());
        }
        let mut last_key = None;
        let mut upper = payload.len();
        let mut children = Vec::new();
        let mut items = Vec::new();
        for i in 0..count {
            let at = i * stride;
            let key = Key::parse(payload, at)?;
            if last_key.is_some_and(|last| key <= last) {
                return Err("Btrfs 节点键重复或乱序。".into());
            }
            last_key = Some(key);
            if leaf {
                let start = le32(payload, at + 17)? as usize;
                let len = le32(payload, at + 21)? as usize;
                let stop = start.checked_add(len).ok_or("Btrfs 叶条目溢出。")?;
                if start < count * stride || stop != upper {
                    return Err("Btrfs 叶条目越界或数据范围不连续。".into());
                }
                upper = start;
                items.push(Item {
                    key,
                    start: HEADER + start,
                    end: HEADER + stop,
                });
            } else {
                children.push((
                    key,
                    BlockRef {
                        logical: le64(payload, at + 17)?,
                        generation: le64(payload, at + 25)?,
                        level: expected.level - 1,
                    },
                ));
            }
        }
        Ok(Self {
            bytes,
            entries: if leaf {
                Entries::Items(items)
            } else {
                Entries::Children(children)
            },
        })
    }

    fn first_key(&self) -> Option<Key> {
        match &self.entries {
            Entries::Items(items) => items.first().map(|item| item.key),
            Entries::Children(children) => children.first().map(|(key, _)| *key),
        }
    }

    fn last_key(&self) -> Option<Key> {
        match &self.entries {
            Entries::Items(items) => items.last().map(|item| item.key),
            Entries::Children(children) => children.last().map(|(key, _)| *key),
        }
    }
}

fn read_node(
    reader: &mut Members<impl Read + Seek>,
    superblock: &Super,
    mapping: &BTreeMap<u64, Chunk>,
    reference: BlockRef,
    owner: u64,
) -> Result<Node, String> {
    let size = u64::from(superblock.node_size);
    if reference.logical % u64::from(superblock.sector_size) != 0
        || reference.generation == 0
        || reference.generation > superblock.generation
        || reference.level >= 8
    {
        return Err("Btrfs 节点地址、层级或代次无效。".into());
    }
    let (&start, chunk) = mapping
        .range(..=reference.logical)
        .next_back()
        .ok_or("Btrfs 节点缺少块组映射。")?;
    let offset = reference.logical - start;
    if offset
        .checked_add(size)
        .is_none_or(|end| end > chunk.length)
        || chunk.kind & (SYSTEM | METADATA) == 0
        || (owner == CHUNK_TREE && chunk.kind & SYSTEM == 0)
    {
        return Err("Btrfs 节点跨越块组或位于错误类型的块组。".into());
    }
    let mut verified = None;
    let mut last_error = "缺少可用的 Btrfs 节点副本。".to_owned();
    for copy in 0..chunk.copies() {
        let mut bytes = vec![0; size as usize];
        let mut done = 0;
        let mut read = Ok(());
        while done < bytes.len() {
            let run = chunk.run(offset + done as u64, bytes.len() - done);
            read = reader.read(
                run.stripes[copy],
                run.delta,
                &mut bytes[done..done + run.len],
            );
            if read.is_err() {
                break;
            }
            done += run.len;
        }
        let node = read.and_then(|()| Node::parse(bytes, reference, owner, superblock));
        match node {
            Ok(node) => {
                if verified
                    .as_ref()
                    .is_some_and(|first: &Node| first.bytes != node.bytes)
                {
                    return Err("Btrfs 同代次节点镜像内容冲突。".into());
                }
                verified = Some(node);
            }
            Err(error) => last_error = error,
        }
    }
    if let Some(node) = verified {
        return Ok(node);
    }
    let first = chunk.run(offset, size as usize);
    if let Some(parity) = first.parity {
        // A node is at most 64 KiB, so it can touch at most two 64 KiB
        // stripes. Try available versions of each fragment before parsing.
        let candidates = |reader: &mut Members<_>, run: members::Run<'_>| {
            let mut candidates = Vec::new();
            let mut direct = vec![0; run.len];
            if reader.read(run.stripes[0], run.delta, &mut direct).is_ok() {
                candidates.push(direct);
            }
            if let Some(parity) = run.parity {
                for prefer_q in [false, true] {
                    if let Some(bytes) = parity.rebuild(reader, run.delta, run.len, prefer_q) {
                        candidates.push(bytes);
                    }
                }
            }
            candidates
        };
        if first.len == size as usize {
            for prefer_q in [false, true] {
                if let Some(bytes) = parity.rebuild(reader, first.delta, first.len, prefer_q) {
                    if let Ok(node) = Node::parse(bytes, reference, owner, superblock) {
                        return Ok(node);
                    }
                }
            }
        } else {
            let second = chunk.run(offset + first.len as u64, size as usize - first.len);
            let tails = candidates(reader, second);
            for head in candidates(reader, first) {
                for tail in &tails {
                    let mut bytes = Vec::with_capacity(size as usize);
                    bytes.extend_from_slice(&head);
                    bytes.extend_from_slice(tail);
                    if let Ok(node) = Node::parse(bytes, reference, owner, superblock) {
                        return Ok(node);
                    }
                }
            }
        }
    }
    Err(last_error)
}

fn scan_tree(
    reader: &mut Members<impl Read + Seek>,
    sb: &Super,
    mapping: &BTreeMap<u64, Chunk>,
    root: BlockRef,
    owner: u64,
    filter: Option<(Key, Key)>,
    mut visit: impl FnMut(Key, &[u8]) -> Result<(), String>,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut visited = HashSet::new();
    let mut stack = vec![(root, None, None)];
    while let Some((reference, lower, upper)) = stack.pop() {
        if filter.is_some_and(|(first, last)| {
            lower.is_some_and(|lower| lower > last) || upper.is_some_and(|upper| upper <= first)
        }) {
            continue;
        }
        checkpoint()?;
        if visited.len() >= MAX_NODES || !visited.insert(reference.logical) {
            return Err("Btrfs 树超过遍历上限，或包含重复节点。".into());
        }
        let node = read_node(reader, sb, mapping, reference, owner)?;
        if lower.is_some_and(|key| node.first_key() != Some(key))
            || upper.is_some_and(|key| node.last_key().is_some_and(|last| last >= key))
        {
            return Err("Btrfs 父子节点的键范围不一致。".into());
        }
        match node.entries {
            Entries::Items(items) => {
                for item in items {
                    if filter.is_none_or(|(first, last)| (first..=last).contains(&item.key)) {
                        visit(item.key, &node.bytes[item.start..item.end])?;
                    }
                }
            }
            Entries::Children(children) => {
                for (index, &(key, child)) in children.iter().enumerate().rev() {
                    if child.generation > reference.generation {
                        return Err("Btrfs 子节点代次晚于父节点。".into());
                    }
                    let limit = children.get(index + 1).map(|(next, _)| *next).or(upper);
                    stack.push((child, Some(key), limit));
                }
            }
        }
    }
    Ok(())
}

/// Checked subvolume records from the root tree of a single-device filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subvolume {
    pub id: u64,
    pub generation: u64,
    pub root_directory: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Inventory {
    pub sector_size: u32,
    pub node_size: u32,
    pub generation: u64,
    pub subvolumes: Vec<Subvolume>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub subvolume: u64,
    pub origin: u64,
    pub created: u64,
}

/// A subvolume in a committed filesystem view or a retained transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tree {
    pub subvolume: u64,
    retained: Option<u64>,
}

impl From<u64> for Tree {
    fn from(subvolume: u64) -> Self {
        Self {
            subvolume,
            retained: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetainedRoot {
    pub subvolume: u64,
    pub transaction: u64,
    pub root_directory: u64,
}

impl From<RetainedRoot> for Tree {
    fn from(root: RetainedRoot) -> Self {
        Self {
            subvolume: root.subvolume,
            retained: Some(root.transaction),
        }
    }
}

struct RootIdentity {
    uuid: [u8; 16],
    parent: [u8; 16],
    received: bool,
    read_only: bool,
    created: u64,
}

fn unique_uuids(identities: &BTreeMap<u64, RootIdentity>) -> BTreeMap<[u8; 16], Option<u64>> {
    let mut by_uuid = BTreeMap::new();
    for (&subvolume, root) in identities {
        by_uuid
            .entry(root.uuid)
            .and_modify(|id| *id = None)
            .or_insert(Some(subvolume));
    }
    by_uuid
}

fn recoverable_snapshots(
    roots: &BTreeMap<u64, BlockRef>,
    identities: &BTreeMap<u64, RootIdentity>,
) -> Vec<Snapshot> {
    let by_uuid = unique_uuids(identities);
    identities
        .iter()
        .filter_map(|(&subvolume, snapshot)| {
            // An unchanged read-only root still represents the source at the
            // snapshot transaction. Received or later-edited roots don't.
            if !snapshot.read_only
                || snapshot.received
                || snapshot.parent == [0; 16]
                || snapshot.created == 0
                || snapshot.created != roots[&subvolume].generation
                || by_uuid[&snapshot.uuid] != Some(subvolume)
            {
                return None;
            }
            let origin = by_uuid.get(&snapshot.parent).copied().flatten()?;
            let live = &identities[&origin];
            if origin == subvolume
                || live.read_only
                || live.received
                || live.created > snapshot.created
                || roots[&origin].generation < snapshot.created
            {
                return None;
            }
            Some(Snapshot {
                subvolume,
                origin,
                created: snapshot.created,
            })
        })
        .collect()
}

struct Trees {
    generation: u64,
    roots: BTreeMap<u64, BlockRef>,
    identities: BTreeMap<u64, RootIdentity>,
    subvolumes: Vec<Subvolume>,
    extent: Option<BlockRef>,
    device: Option<BlockRef>,
    checksum: Option<BlockRef>,
}

/// A checked device mapping and committed root table over a supplied stream.
pub struct Volume<R> {
    reader: Members<R>,
    superblock: Super,
    recover_mirror: bool,
    devices: BTreeMap<u64, Device>,
    view: [u8; 32],
    mapping: BTreeMap<u64, Chunk>,
    current: Trees,
    retained: BTreeMap<u64, Trees>,
    inventory: Inventory,
    snapshots: Vec<Snapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub index: u64,
    pub transaction: u64,
    pub name: Vec<u8>,
    pub target: Key,
    pub file_kind: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inode([u8; 160]);

impl Inode {
    fn parse(item: &[u8], committed: u64) -> Result<Self, String> {
        let generation = le64(item, 0)?;
        let transaction = le64(item, 8)?;
        if item.len() != 160 || generation == 0 || generation > committed || transaction > committed
        {
            return Err("Btrfs inode 长度或代次无效。".into());
        }
        Ok(Self(word(item, 0)?))
    }

    pub fn record(&self) -> &[u8; 160] {
        &self.0
    }

    pub fn size(&self) -> u64 {
        u64::from_le_bytes(self.0[16..24].try_into().unwrap())
    }

    pub fn mode(&self) -> u32 {
        u32::from_le_bytes(self.0[52..56].try_into().unwrap())
    }

    pub fn generation(&self) -> u64 {
        u64::from_le_bytes(self.0[..8].try_into().unwrap())
    }

    pub fn links(&self) -> u32 {
        u32::from_le_bytes(self.0[40..44].try_into().unwrap())
    }
}

impl<R: Read + Seek> Volume<R> {
    /// Number of validated device records without a supplied stream.
    pub fn missing_members(&self) -> usize {
        self.devices.len() - 1 - self.reader.others.len()
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }

    /// Use only past transactions whose mapping is identical to today's.
    /// A reused block fails the historic generation/owner/checksum checks.
    pub fn retained_roots(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<Vec<RetainedRoot>, String> {
        let sb = &self.superblock;
        let mut eligible = Vec::new();
        if !sb.backups.iter().any(|entry| entry.root == sb.root) {
            return Ok(eligible);
        }
        let mut backups = sb.backups.clone();
        backups.sort_by_key(|entry| std::cmp::Reverse(entry.root.generation));
        let current_uuids = unique_uuids(&self.current.identities);
        self.retained.clear();
        for backup in &backups {
            checkpoint()?;
            let tx = backup.root.generation;
            if tx == 0
                || tx >= sb.generation
                || backup.chunk != sb.chunk_root
                || tx < backup.chunk.generation
                || backup.size != sb.total_size
                || backup.devices != sb.device_count as u64
                || backups
                    .iter()
                    .filter(|other| other.root.generation == tx)
                    .count()
                    != 1
            {
                continue;
            }
            let trees = read_roots(
                &mut self.reader,
                sb,
                &self.mapping,
                backup.root,
                &mut checkpoint,
            );
            let Ok(trees) = trees else {
                // An expired root is normal. Preserve cancellation from the caller.
                checkpoint()?;
                continue;
            };
            if trees.extent != Some(backup.extent) || trees.checksum != Some(backup.checksum) {
                continue;
            }
            let past_uuids = unique_uuids(&trees.identities);
            for subvolume in &trees.subvolumes {
                let id = subvolume.id;
                let Some((past, live)) = trees
                    .identities
                    .get(&id)
                    .zip(self.current.identities.get(&id))
                else {
                    continue;
                };
                if past.uuid != live.uuid
                    || past.created != live.created
                    || past.received
                    || live.received
                    || live.read_only
                    || past.read_only
                    || trees.roots.get(&id) == self.current.roots.get(&id)
                    || past_uuids[&past.uuid] != Some(id)
                    || current_uuids[&live.uuid] != Some(id)
                {
                    continue;
                }
                eligible.push(RetainedRoot {
                    subvolume: id,
                    transaction: tx,
                    root_directory: subvolume.root_directory,
                });
            }
            self.retained.insert(tx, trees);
        }
        Ok(eligible)
    }

    fn trees(&self, tree: Tree) -> Result<&Trees, String> {
        if let Some(transaction) = tree.retained {
            self.retained
                .get(&transaction)
                .ok_or_else(|| "Btrfs 已保留事务不可读取。".into())
        } else {
            Ok(&self.current)
        }
    }

    /// Look up the exact inode record, distinguishing proven absence from a
    /// read or validation failure. Used to compare a snapshot and its origin.
    pub fn find_inode(
        &mut self,
        subvolume: u64,
        inode: u64,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<Option<Inode>, String> {
        let key = Key {
            object_id: inode,
            kind: INODE_ITEM,
            offset: 0,
        };
        let committed = self.superblock.generation;
        let mut record = None;
        self.scan_keys(
            subvolume,
            key,
            key,
            |_, item| {
                record = Some(Inode::parse(item, committed)?);
                Ok(())
            },
            checkpoint,
        )?;
        Ok(record)
    }

    pub fn identity(&self) -> [u8; 32] {
        self.view
    }

    pub fn verify_view(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let recover_mirror = self.recover_mirror;
        let views = std::iter::once(&mut self.reader.primary)
            .chain(self.reader.others.values_mut())
            .map(|reader| {
                read_super_with_mirror_policy(reader, recover_mirror, &mut checkpoint)
                    .map(|sb| sb.identity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if view_identity(&views) != self.view {
            return Err("Btrfs 已提交视图发生变化，请重新读取目录。".into());
        }
        Ok(())
    }

    /// Return the inode and whether the reader handles its data encoding.
    /// Data blocks are left unread until a preview or extraction is requested.
    pub fn inode(
        &mut self,
        tree: impl Into<Tree>,
        inode: u64,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(Inode, bool), String> {
        let first = Key {
            object_id: inode,
            kind: INODE_ITEM,
            offset: 0,
        };
        let last = Key {
            object_id: inode,
            kind: file::EXTENT_DATA,
            offset: u64::MAX,
        };
        let tree = tree.into();
        let committed = self.trees(tree)?.generation;
        let mut info = None;
        let mut readable = true;
        self.scan_keys(
            tree,
            first,
            last,
            |key, item| {
                if key == first {
                    let record = Inode::parse(item, committed)?;
                    readable = le64(record.record(), 64)? & 1 == 0;
                    info = Some(record);
                } else if key.kind == file::EXTENT_DATA {
                    let encoding = word::<4>(item, 16)?;
                    readable &= encoding[0] <= 3 && encoding[1..] == [0; 3];
                }
                Ok(())
            },
            checkpoint,
        )?;
        Ok((info.ok_or("未找到 Btrfs inode。")?, readable))
    }

    pub fn read_directory_entry(
        &mut self,
        tree: impl Into<Tree>,
        parent: u64,
        index: u64,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<DirectoryEntry, String> {
        let key = Key {
            object_id: parent,
            kind: DIR_INDEX,
            offset: index,
        };
        let mut entry = None;
        let tree = tree.into();
        let committed = self.trees(tree)?.generation;
        self.scan_keys(
            tree,
            key,
            key,
            |_, item| {
                entry = Some(directory_entry(index, item, committed)?);
                Ok(())
            },
            checkpoint,
        )?;
        entry.ok_or_else(|| "Btrfs 目录记录已不存在。".into())
    }

    fn scan_keys(
        &mut self,
        tree: impl Into<Tree>,
        first: Key,
        last: Key,
        visit: impl FnMut(Key, &[u8]) -> Result<(), String>,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let tree = tree.into();
        let root = *self
            .trees(tree)?
            .roots
            .get(&tree.subvolume)
            .ok_or("Btrfs 子卷不存在。")?;
        scan_tree(
            &mut self.reader,
            &self.superblock,
            &self.mapping,
            root,
            tree.subvolume,
            Some((first, last)),
            visit,
            checkpoint,
        )
    }

    /// Names are retained as bytes because Linux names need not be UTF-8.
    pub fn read_directory(
        &mut self,
        tree: impl Into<Tree>,
        inode: u64,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<Vec<DirectoryEntry>, String> {
        let tree = tree.into();
        let committed = self.trees(tree)?.generation;
        let first = Key {
            object_id: inode,
            kind: INODE_ITEM,
            offset: 0,
        };
        let last = Key {
            object_id: inode,
            kind: DIR_INDEX,
            offset: u64::MAX,
        };
        let mut found_inode = false;
        let mut entries = Vec::new();
        let mut names = HashSet::new();
        self.scan_keys(
            tree,
            first,
            last,
            |key, item| {
                if key == first {
                    if Inode::parse(item, committed)?.mode() & 0xf000 != 0x4000 {
                        return Err("Btrfs 指定 inode 不是有效目录。".into());
                    }
                    found_inode = true;
                } else if key.kind == DIR_INDEX {
                    if !found_inode || entries.len() >= 100_000 {
                        return Err("Btrfs 目录缺少 inode 或超过条目上限。".into());
                    }
                    let entry = directory_entry(key.offset, item, committed)?;
                    if !names.insert(entry.name.clone()) {
                        return Err("Btrfs 目录含重复文件名。".into());
                    }
                    entries.push(entry);
                }
                Ok(())
            },
            checkpoint,
        )?;
        if !found_inode {
            return Err("未找到 Btrfs 目录 inode。".into());
        }
        Ok(entries)
    }
}

fn directory_entry(index: u64, item: &[u8], committed: u64) -> Result<DirectoryEntry, String> {
    let target = Key::parse(item, 0)?;
    let transaction = le64(item, 17)?;
    let data_len = usize::from(le16(item, 25)?);
    let name_len = usize::from(le16(item, 27)?);
    let file_kind = word::<1>(item, 29)?[0];
    let name = item.get(30..).ok_or("Btrfs 目录条目被截断。")?;
    if name.len() != name_len
        || data_len != 0
        || !(1..=255).contains(&name_len)
        || name == b"."
        || name == b".."
        || name.iter().any(|&b| matches!(b, 0 | b'/'))
        || index < 2
        || transaction > committed
        || !(1..=7).contains(&file_kind)
        || !matches!((target.kind, file_kind), (INODE_ITEM, _) | (ROOT_ITEM, 2))
        || (target.kind == ROOT_ITEM && !file_tree(target.object_id))
        || (target.kind == INODE_ITEM
            && (target.offset != 0
                || (target.object_id < 256 && (target.object_id, file_kind) != (2, 2))))
    {
        return Err("Btrfs 目录条目的名称、类型或目标无效。".into());
    }
    Ok(DirectoryEntry {
        index,
        transaction,
        name: name.to_vec(),
        target,
        file_kind,
    })
}

fn read_super(
    reader: &mut (impl Read + Seek),
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Super, String> {
    read_super_with_mirror_policy(reader, false, checkpoint)
}

fn read_super_with_mirror_policy(
    reader: &mut (impl Read + Seek),
    recover_mirror: bool,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Super, String> {
    let size = reader.seek(SeekFrom::End(0)).map_err(err)?;
    let mut candidates = Vec::new();
    let mut primary_error = None;
    for at in SUPER_OFFSETS {
        if at + SUPER_BYTES as u64 > size {
            continue;
        }
        checkpoint()?;
        let mut bytes = [0; SUPER_BYTES];
        let read = read_at(reader, at, &mut bytes);
        if at == SUPER_OFFSETS[0] && !recover_mirror {
            read?;
            if &bytes[64..72] != MAGIC {
                return Err("未找到 Btrfs 主超级块。".into());
            }
        } else if read.is_err() || &bytes[64..72] != MAGIC {
            continue;
        }
        // Distinguish a torn superblock from valid unsupported metadata.
        // A newer valid copy cannot be ignored to bypass a format constraint.
        let algorithm = ChecksumType::from_raw(le16(&bytes, 0xc4)?);
        if !matches!(algorithm, ChecksumType::Unknown(_))
            && !checksum_matches(algorithm, &bytes[32..], &bytes[..32])
        {
            primary_error = Some("Btrfs 超级块校验值不一致。");
            continue;
        }
        candidates.push((at, bytes));
    }
    let (_, top) = candidates
        .iter()
        .max_by_key(|(_, bytes)| u64::from_le_bytes(bytes[72..80].try_into().unwrap()))
        .ok_or(primary_error.unwrap_or("未找到 Btrfs 超级块。"))?;
    let mut sb = None;
    for (at, bytes) in &candidates {
        if bytes[32..48] != top[32..48]
            || bytes[0x10b..0x12b] != top[0x10b..0x12b]
            || bytes[0xc4..0xc6] != top[0xc4..0xc6]
        {
            return Err("Btrfs 超级块镜像的设备身份冲突。".into());
        }
        if bytes[72..80] != top[72..80] {
            continue;
        }
        if bytes[56..] != top[56..] {
            return Err("Btrfs 同代次超级块镜像内容冲突。".into());
        }
        if sb.is_none() {
            sb = Some(Super::parse(bytes, *at, size)?);
        }
    }
    sb.ok_or_else(|| "未找到有效的 Btrfs 超级块。".into())
}

/// Recognize the primary on-disk magic only when the caller requests a scan.
/// All format, bounds and checksum checks still take place in `open`.
pub fn has_magic(reader: &mut (impl Read + Seek), size: u64) -> Result<bool, String> {
    let at = SUPER_OFFSETS[0] + 64;
    if size < at + 8 {
        return Ok(false);
    }
    let mut magic = [0; 8];
    read_at(reader, at, &mut magic)?;
    Ok(&magic == b"_BHRfS_M")
}

/// Verify both mapping and root trees without mounting or replaying a log.
pub fn open<R: Read + Seek>(
    reader: R,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    open_members(vec![reader], checkpoint)
}

fn view_identity(views: &[[u8; 32]]) -> [u8; 32] {
    if let [single] = views {
        return *single;
    }
    let mut binding = Sha256::new();
    binding.update(b"DiskStudio Btrfs member set");
    for view in views {
        binding.update(view);
    }
    binding.finalize().into()
}

/// Open explicitly supplied members of the same committed filesystem.
/// The order of the handles is irrelevant. No other devices are discovered.
pub fn open_members<R: Read + Seek>(
    readers: Vec<R>,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    open_member_set(readers, false, false, checkpoint)
}

/// Open a checked committed view using the explicitly supplied streams.
/// Missing devices are never discovered or accessed. Each accessed metadata
/// node and file sector must be readable and independently checksum verified.
pub fn open_degraded<R: Read + Seek>(
    readers: Vec<R>,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    open_member_set(readers, true, false, checkpoint)
}

/// A cheap signature hint at a possible superblock position. A caller must
/// validate the full superblock and all derived bounds before trusting it.
pub fn recovery_superblock_address(header: &[u8]) -> Option<u64> {
    if header.get(64..72)? != MAGIC {
        return None;
    }
    let address = le64(header, 0x30).ok()?;
    SUPER_OFFSETS.contains(&address).then_some(address)
}

/// Get a bound for one member from a checksummed superblock on that member.
pub fn recovery_member_size(
    bytes: &[u8; RECOVERY_SUPERBLOCK_BYTES],
    address: u64,
    available: u64,
    device_sector: u64,
) -> Result<u64, String> {
    if recovery_superblock_address(bytes) != Some(address) || !matches!(device_sector, 512 | 4096) {
        return Err("Btrfs 超级块位置或设备扇区无效。".into());
    }
    let sb = Super::parse(bytes, address, available)?;
    if address + SUPER_BYTES as u64 > sb.device.size
        || u64::from(sb.sector_size) < device_sector
        || sb.device.size % device_sector != 0
    {
        return Err("Btrfs 成员容量或扇区大小与候选范围不符。".into());
    }
    Ok(sb.device.size)
}

/// Open a single recovered member using checked mirrors and mapped redundant
/// stripes if available. External members are never discovered or opened.
pub fn open_recovered<R: Read + Seek>(
    reader: R,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    open_recovered_members(vec![reader], true, checkpoint)
}

/// Open a committed view from bounded, explicitly supplied recovered members.
pub fn open_recovered_members<R: Read + Seek>(
    readers: Vec<R>,
    allow_missing: bool,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    open_member_set(readers, allow_missing, true, checkpoint)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryMember {
    pub size: u64,
    pub id: u64,
    pub uuid: [u8; 16],
    pub total_members: usize,
    pub committed_set: [u8; 32],
    pub identity: [u8; 32],
}

/// Check a member's superblock mirrors and initial stripe geometry before the
/// other members are available. This does not prove the trees are readable.
pub fn inspect_recovery_member(
    reader: &mut (impl Read + Seek),
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<RecoveryMember, String> {
    let sb = read_super_with_mirror_policy(reader, true, checkpoint)?;
    let member = RecoveryMember {
        size: sb.device.size,
        id: sb.device.id,
        uuid: sb.device.uuid,
        total_members: sb.device_count,
        committed_set: sb.shared,
        identity: sb.identity,
    };
    check_chunk_ranges(
        &sb.bootstrap,
        &BTreeMap::from([(sb.device.id, sb.device)]),
        sb.device_count > 1,
    )?;
    Ok(member)
}

impl<R: Read + Seek> Volume<R> {
    /// Confirm the primary boot location represents this exact committed view.
    /// A clone of an older or damaged primary cannot make a GPT entry usable.
    pub fn has_current_primary(&mut self) -> Result<bool, String> {
        let mut raw = [0; SUPER_BYTES];
        read_at(&mut self.reader.primary, SUPER_OFFSETS[0], &mut raw)?;
        if &raw[64..72] != MAGIC || le64(&raw, 0x38)? != 1 {
            return Ok(false);
        }
        Ok(
            Super::parse(&raw, SUPER_OFFSETS[0], self.superblock.device.size)
                .is_ok_and(|primary| primary.identity == self.superblock.identity),
        )
    }

    pub fn member_size(&self) -> u64 {
        self.superblock.device.size
    }
}

fn open_member_set<R: Read + Seek>(
    readers: Vec<R>,
    allow_missing: bool,
    recover_mirror: bool,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Volume<R>, String> {
    if readers.is_empty() || readers.len() > MAX_MEMBERS {
        return Err("Btrfs 成员数量无效。".into());
    }
    let count = readers.len();
    let mut opened = BTreeMap::new();
    let mut uuids = HashSet::new();
    let mut supplied = BTreeMap::new();
    let mut shared = None;
    for mut reader in readers {
        let sb = read_super_with_mirror_policy(&mut reader, recover_mirror, &mut checkpoint)?;
        if sb.device_count < count || (!allow_missing && sb.device_count != count) {
            return Err("Btrfs 已提供的成员数量与卷记录不符。".into());
        }
        if shared.is_some_and(|hash| sb.shared != hash) {
            return Err("Btrfs 成员的文件系统、代次或卷级元数据不一致。".into());
        }
        shared = Some(sb.shared);
        let device = sb.device.clone();
        if !uuids.insert(device.uuid) || supplied.insert(device.id, device.clone()).is_some() {
            return Err("Btrfs 成员 ID 或 UUID 重复。".into());
        }
        opened.insert(device.id, (reader, sb));
    }
    let (primary_id, (primary, sb)) = opened.pop_first().ok_or("Btrfs 缺少成员。")?;
    let mut views = vec![sb.identity];
    let mut others = BTreeMap::new();
    for (id, (reader, member)) in opened {
        views.push(member.identity);
        others.insert(id, reader);
    }
    let view = view_identity(&views);
    let mut reader = Members {
        primary,
        primary_id,
        others,
    };
    check_chunk_ranges(&sb.bootstrap, &supplied, count < sb.device_count)?;
    let mut mapping = BTreeMap::new();
    let mut devices = BTreeMap::new();
    let mut device_uuids = HashSet::new();
    let mut total = 0u64;
    scan_tree(
        &mut reader,
        &sb,
        &sb.bootstrap,
        sb.chunk_root,
        CHUNK_TREE,
        None,
        |key, item| {
            match key.kind {
                CHUNK_ITEM => {
                    let chunk = Chunk::parse(key, item, sb.sector_size)?;
                    if mapping.insert(key.offset, chunk).is_some() {
                        return Err("Btrfs 块组映射重复。".into());
                    }
                }
                DEVICE_ITEM => {
                    let device = Device::parse(item)?;
                    if key.object_id != 1
                        || key.offset != device.id
                        || device.id == 0
                        || device.uuid == [0; 16]
                        || !(SUPER_OFFSETS[0] + SUPER_BYTES as u64..=sb.total_size)
                            .contains(&device.size)
                        || device.allocated > device.size
                        || device.size % u64::from(sb.sector_size) != 0
                        || word::<16>(item, 82)? != sb.fsid
                        || supplied
                            .get(&device.id)
                            .is_some_and(|known| known != &device)
                        || !device_uuids.insert(device.uuid)
                        || devices.len() >= sb.device_count
                    {
                        return Err("Btrfs 块组树与设备身份不一致。".into());
                    }
                    total = total.checked_add(device.size).ok_or("Btrfs 总容量溢出。")?;
                    if devices.insert(device.id, device).is_some() {
                        return Err("Btrfs 块组树中的成员 ID 重复。".into());
                    }
                }
                _ => return Err("Btrfs 块组树含不支持的条目。".into()),
            }
            Ok(())
        },
        &mut checkpoint,
    )?;
    chunks_consistent(&mapping, &devices)?;
    if devices.len() != sb.device_count
        || total != sb.total_size
        || supplied.keys().any(|id| !devices.contains_key(id))
        || sb
            .bootstrap
            .iter()
            .any(|(start, chunk)| mapping.get(start) != Some(chunk))
    {
        return Err("Btrfs 系统块组与完整块组映射不一致。".into());
    }
    let current = read_roots(&mut reader, &sb, &mapping, sb.root, &mut checkpoint)?;
    let inventory = Inventory {
        sector_size: sb.sector_size,
        node_size: sb.node_size,
        generation: sb.generation,
        subvolumes: current.subvolumes.clone(),
    };
    let snapshots = recoverable_snapshots(&current.roots, &current.identities);
    let mut volume = Volume {
        reader,
        superblock: sb,
        recover_mirror,
        devices,
        view,
        mapping,
        current,
        retained: BTreeMap::new(),
        inventory,
        snapshots,
    };
    if volume.devices.len() > 1 || recover_mirror {
        volume.checked_device_extents(false, &mut checkpoint)?;
    }
    Ok(volume)
}

fn read_roots(
    reader: &mut Members<impl Read + Seek>,
    sb: &Super,
    mapping: &BTreeMap<u64, Chunk>,
    root: BlockRef,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Trees, String> {
    let mut subvolumes = Vec::new();
    let mut roots = BTreeMap::new();
    let mut root_identities = BTreeMap::new();
    let mut extent_root = None;
    let mut device_root = None;
    let mut checksum_root = None;
    scan_tree(
        reader,
        sb,
        mapping,
        root,
        ROOT_TREE,
        None,
        |key, item| {
            if key.kind == ROOT_ITEM && matches!(key.object_id, 2 | DEVICE_TREE | 7) {
                let reference = BlockRef {
                    logical: le64(item, 176)?,
                    generation: le64(item, 160)?,
                    level: word::<1>(item, 238)?[0],
                };
                if !(1..=root.generation).contains(&reference.generation)
                    || reference.logical == 0
                    || reference.level >= 8
                {
                    return Err("Btrfs 分配或文件校验树根无效。".into());
                }
                let destination = match key.object_id {
                    2 => &mut extent_root,
                    DEVICE_TREE => &mut device_root,
                    _ => &mut checksum_root,
                };
                if destination.replace(reference).is_some() {
                    return Err("Btrfs 分配或文件校验树根重复。".into());
                }
            }
            if key.kind != ROOT_ITEM || !file_tree(key.object_id) {
                return Ok(());
            }
            let generation = le64(item, 160)?;
            let root_directory = le64(item, 168)?;
            let refs = le32(item, 216)?;
            if refs == 0 {
                return Ok(());
            }
            let reference = BlockRef {
                logical: le64(item, 176)?,
                generation,
                level: word::<1>(item, 238)?[0],
            };
            if root_directory < 256
                || reference.logical == 0
                || reference.level >= 8
                || generation == 0
                || generation > root.generation
                || roots.insert(key.object_id, reference).is_some()
            {
                return Err("Btrfs 子卷元数据无效或标识重复。".into());
            }
            subvolumes.push(Subvolume {
                id: key.object_id,
                generation,
                root_directory,
            });
            if item.len() >= 311 && le64(item, 239)? == generation {
                let uuid = word::<16>(item, 247)?;
                let created = le64(item, 303)?;
                if uuid != [0; 16] && created <= generation {
                    root_identities.insert(
                        key.object_id,
                        RootIdentity {
                            uuid,
                            parent: word(item, 263)?,
                            received: word::<16>(item, 279)? != [0; 16],
                            read_only: le64(item, 208)? & 1 != 0,
                            created,
                        },
                    );
                }
            }
            Ok(())
        },
        checkpoint,
    )?;
    if subvolumes.first().is_none_or(|root| root.id != 5) {
        return Err("Btrfs 缺少顶层文件树。".into());
    }
    Ok(Trees {
        generation: root.generation,
        roots,
        identities: root_identities,
        subvolumes,
        extent: extent_root,
        device: device_root,
        checksum: checksum_root,
    })
}

pub fn inspect(
    reader: &mut (impl Read + Seek),
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Inventory, String> {
    Ok(open(reader, checkpoint)?.inventory)
}

impl<R: Read + Seek> Volume<R> {
    /// Cross-check physical allocations with the independent device tree.
    fn verify_device_extents(&mut self) -> Result<BTreeMap<u64, [u8; 48]>, String> {
        if self.devices.len() != 1 {
            return Err("Btrfs 调整大小要求单设备卷。".into());
        }
        self.checked_device_extents(true, || Ok(())).map(|extents| {
            extents
                .into_iter()
                .map(|((_, offset), item)| (offset, item))
                .collect()
        })
    }

    fn checked_device_extents(
        &mut self,
        require_clean_stats: bool,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<BTreeMap<(u64, u64), [u8; 48]>, String> {
        let sb = &self.superblock;
        let chunk_header = read_node(
            &mut self.reader,
            sb,
            &self.mapping,
            sb.chunk_root,
            CHUNK_TREE,
        )?;
        let chunk_tree_uuid = word::<16>(&chunk_header.bytes, 64)?;
        let mut expected = BTreeMap::new();
        let mut allocated = BTreeMap::<u64, u64>::new();
        let mut extents = BTreeMap::new();
        for (&logical, chunk) in &self.mapping {
            for stripe in &chunk.stripes {
                expected.insert((stripe.device, stripe.offset), (logical, chunk.span()));
                let used = allocated.entry(stripe.device).or_default();
                *used = used
                    .checked_add(chunk.span())
                    .ok_or("Btrfs 分配容量溢出。")?;
            }
        }
        if chunk_tree_uuid == [0; 16]
            || self
                .devices
                .values()
                .any(|device| allocated.get(&device.id).copied().unwrap_or(0) != device.allocated)
        {
            return Err("Btrfs 设备分配容量或块组树标识不符。".into());
        }
        scan_tree(
            &mut self.reader,
            sb,
            &self.mapping,
            self.current.device.ok_or("Btrfs 缺少设备树。")?,
            DEVICE_TREE,
            None,
            |key, item| {
                // Linux records device I/O statistics separately from the
                // chunk allocations. Nonzero counters need investigation.
                if key.object_id == 0
                    && key.kind == PERSISTENT_ITEM
                    && self.devices.contains_key(&key.offset)
                {
                    if item.len() == 40
                        && (!require_clean_stats || item.iter().all(|&byte| byte == 0))
                    {
                        return Ok(());
                    }
                    return Err("Btrfs 设备报告 I/O 错误或设备统计不完整。".into());
                }
                let physical = (key.object_id, key.offset);
                if key.kind != DEVICE_EXTENT
                    || item.len() != 48
                    || le64(item, 0)? != CHUNK_TREE
                    || le64(item, 8)? != 256
                    || ![[0; 16], chunk_tree_uuid].contains(&word::<16>(item, 32)?)
                    || expected.remove(&physical) != Some((le64(item, 16)?, le64(item, 24)?))
                {
                    return Err("Btrfs 设备范围与块组条带不一致。".into());
                }
                extents.insert(physical, word(item, 0)?);
                Ok(())
            },
            checkpoint,
        )?;
        if !expected.is_empty() {
            return Err("Btrfs 设备树缺少块组条带。".into());
        }
        Ok(extents)
    }
}

/// Confirm the fixed geometry and empty directory of the freshly formatted
/// single-device filesystem. Every mirror inside the extent must match.
pub fn verify_format(
    reader: &mut (impl Read + Seek),
    fsid: [u8; 16],
    label: &str,
) -> Result<(), String> {
    let length = reader.seek(SeekFrom::End(0)).map_err(err)?;
    let mut expected = None;
    for offset in SUPER_OFFSETS
        .into_iter()
        .filter(|at| at + SUPER_BYTES as u64 <= length)
    {
        let mut bytes = [0; SUPER_BYTES];
        read_at(reader, offset, &mut bytes)?;
        if &bytes[64..72] != b"_BHRfS_M" {
            return Err("Btrfs 格式化缺少超级块副本。".into());
        }
        let sb = Super::parse(&bytes, offset, length)?;
        let label_bytes = &bytes[0x12b..0x22b];
        let disk_label = label_bytes
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        if sb.fsid != fsid
            || sb.algorithm != ChecksumType::Crc32
            || sb.device.size != length
            || sb.sector_size != 4096
            || sb.node_size != 16384
            || sb.generation != 1
            || le64(&bytes, 0xbc)? != 0x340
            || le64(&bytes, 0x38)? != 1
            || disk_label != label.as_bytes()
            || expected.is_some_and(|identity| identity != sb.identity)
        {
            return Err("Btrfs 卷标、容量、几何或超级块副本核对失败。".into());
        }
        expected = Some(sb.identity);
    }
    let mut volume = open(reader, || Ok(()))?;
    let [root] = volume.inventory().subvolumes.as_slice() else {
        return Err("Btrfs 格式化后的子卷列表不符。".into());
    };
    let directory = root.root_directory;
    if root.id != 5 || !volume.read_directory(5, directory, || Ok(()))?.is_empty() {
        return Err("Btrfs 格式化后的根目录或元数据树不符。".into());
    }
    volume.verify_device_extents()?;
    let extent = volume.current.extent.ok_or("Btrfs 缺少分配树。")?;
    let checksum = volume.current.checksum.ok_or("Btrfs 缺少文件校验树。")?;
    scan_tree(
        &mut volume.reader,
        &volume.superblock,
        &volume.mapping,
        extent,
        2,
        None,
        |key, _| {
            if key.kind == 168 {
                Err("新 Btrfs 文件系统包含文件数据范围。".into())
            } else {
                Ok(())
            }
        },
        || Ok(()),
    )?;
    scan_tree(
        &mut volume.reader,
        &volume.superblock,
        &volume.mapping,
        checksum,
        7,
        None,
        |_, _| Err("新 Btrfs 文件系统的文件校验树应为空。".into()),
        || Ok(()),
    )?;
    volume.verify_view(|| Ok(()))
}

#[cfg(test)]
#[path = "../tests/fixtures/btrfs.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::fixture::{image, retained_image, set32, set64, sign, snapshot_image, MIB};
    use super::*;
    use std::io::Cursor;

    #[test]
    fn validates_superblock_mapping_and_root_tree() {
        assert_eq!(crc32c(b"123456789"), 0xe3069283);
        let expected = Inventory {
            sector_size: 4096,
            node_size: 4096,
            generation: 10,
            subvolumes: vec![
                Subvolume {
                    id: 5,
                    generation: 10,
                    root_directory: 256,
                },
                Subvolume {
                    id: 300,
                    generation: 10,
                    root_directory: 256,
                },
            ],
        };
        assert_eq!(
            inspect(&mut Cursor::new(image()), || Ok(())).unwrap(),
            expected
        );
        let mut damaged = image();
        damaged[MIB as usize + 500] ^= 1;
        assert_eq!(
            inspect(&mut Cursor::new(damaged), || Ok(())).unwrap(),
            expected
        );
    }

    #[test]
    fn refuses_valid_but_conflicting_duplicates_and_dirty_roots() {
        let mut conflict = image();
        let at = 2 * MIB as usize;
        conflict[at + 500] ^= 1;
        sign(&mut conflict[at..at + 4096]);
        assert!(inspect(&mut Cursor::new(conflict), || Ok(()))
            .unwrap_err()
            .contains("镜像内容冲突"));
        let mut dirty = image();
        let start = SUPER_OFFSETS[0] as usize;
        let sb = &mut dirty[start..start + SUPER_BYTES];
        set64(sb, 0x60, MIB);
        sign(sb);
        assert!(inspect(&mut Cursor::new(dirty), || Ok(()))
            .unwrap_err()
            .contains("待回放"));
    }

    #[test]
    fn rejected_leaf_geometry_and_cancellation_never_produce_inventory() {
        let mut corrupt = image();
        let at = 3 * MIB as usize;
        let root = &mut corrupt[at..at + 4096];
        set32(root, HEADER + 17, 1);
        sign(root);
        assert!(inspect(&mut Cursor::new(corrupt), || Ok(())).is_err());
        assert_eq!(
            inspect(&mut Cursor::new(image()), || Err("cancel".into())).unwrap_err(),
            "cancel"
        );
    }

    #[test]
    fn reads_directory_index_and_preserves_raw_names() {
        let mut volume = open(Cursor::new(image()), || Ok(())).unwrap();
        let entries = volume.read_directory(5, 256, || Ok(())).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            DirectoryEntry {
                index: 2,
                transaction: 0,
                name: b"test-\xff".to_vec(),
                target: Key {
                    object_id: 257,
                    kind: INODE_ITEM,
                    offset: 0
                },
                file_kind: 1,
            }
        );
        assert_eq!(entries[1].target.object_id, 300);
        assert_eq!(
            volume.read_directory_entry(5, 256, 2, || Ok(())).unwrap(),
            entries[0]
        );
        assert!(volume
            .read_directory(300, 256, || Ok(()))
            .unwrap()
            .is_empty());
        assert!(volume.read_directory(5, 257, || Ok(())).is_err());
        assert!(volume.read_directory(256, 256, || Ok(())).is_err());
    }

    #[test]
    fn recognizes_read_only_snapshot_lineage_and_distinguishes_inode_absence() {
        let mut volume = open(Cursor::new(snapshot_image(false)), || Ok(())).unwrap();
        assert_eq!(
            volume.snapshots(),
            [Snapshot {
                subvolume: 300,
                origin: 5,
                created: 8
            }]
        );
        assert_eq!(
            volume
                .find_inode(5, 257, || Ok(()))
                .unwrap()
                .unwrap()
                .links(),
            1
        );
        assert!(volume.find_inode(5, 258, || Ok(())).unwrap().is_none());
        assert_eq!(
            volume
                .find_inode(300, 258, || Ok(()))
                .unwrap()
                .unwrap()
                .generation(),
            6
        );
        assert!(volume.find_inode(301, 258, || Ok(())).is_err());
    }

    #[test]
    fn reads_verified_data_and_holes_but_refuses_changed_bytes() {
        let mut volume = open(Cursor::new(image()), || Ok(())).unwrap();
        let mut bytes = Vec::new();
        let result = volume
            .read_file(5, 257, u64::MAX, &mut bytes, || Ok(()))
            .unwrap();
        assert_eq!(
            result,
            FileInfo {
                size: 4103,
                generation: 10,
                mode: 0x81a4
            }
        );
        assert_eq!(bytes[..4096], [23; 4096]);
        assert_eq!(bytes[4096..], [0; 7]);
        bytes.clear();
        assert_eq!(
            volume.read_file(5, 257, 3, &mut bytes, || Ok(())).unwrap(),
            result
        );
        assert_eq!(bytes, [23; 3]);

        let mut changed = image();
        changed[4 * MIB as usize + 4095] ^= 1;
        let mut volume = open(Cursor::new(changed), || Ok(())).unwrap();
        bytes.clear();
        assert!(volume
            .read_file(5, 257, 1, &mut bytes, || Ok(()))
            .unwrap_err()
            .contains("CRC32C"));
        assert!(bytes.is_empty());
    }

    #[test]
    fn checks_all_digest_bytes_for_metadata_and_data() {
        use btrfs_disk::util::csum_tree_block;

        for algorithm in [
            ChecksumType::Xxhash,
            ChecksumType::Sha256,
            ChecksumType::Blake2,
        ] {
            let mut disk = image();
            let sb = SUPER_OFFSETS[0] as usize;
            let superblock = &mut disk[sb..sb + SUPER_BYTES];
            superblock[0xc4..0xc6].copy_from_slice(&algorithm.to_raw().to_le_bytes());
            csum_tree_block(superblock, algorithm);

            let data = 4 * MIB as usize;
            let digest = algorithm.compute(&disk[data..data + 4096]);
            let checksum = 3 * MIB as usize + 8192;
            let leaf = &mut disk[checksum..checksum + 4096];
            let start = leaf.len() - digest.len();
            set32(leaf, HEADER + 17, (start - HEADER) as u32);
            set32(leaf, HEADER + 21, digest.len() as u32);
            leaf[start..].copy_from_slice(&digest);
            for at in [MIB as usize, 2 * MIB as usize]
                .into_iter()
                .chain((0..5).map(|block| 3 * MIB as usize + block * 4096))
            {
                csum_tree_block(&mut disk[at..at + 4096], algorithm);
            }

            let mut output = Vec::new();
            open(Cursor::new(&disk), || Ok(()))
                .unwrap()
                .read_file(5, 257, u64::MAX, &mut output, || Ok(()))
                .unwrap();
            assert_eq!(&output[..4096], &[23; 4096]);
            assert_eq!(&output[4096..], &[0; 7]);

            output.clear();
            disk[checksum + 4095] ^= 1;
            csum_tree_block(&mut disk[checksum..checksum + 4096], algorithm);
            assert!(open(Cursor::new(&disk), || Ok(()))
                .unwrap()
                .read_file(5, 257, 1, &mut output, || Ok(()))
                .is_err());
            assert!(output.is_empty());

            disk[sb + algorithm.size() - 1] ^= 1;
            assert!(inspect(&mut Cursor::new(disk), || Ok(()))
                .unwrap_err()
                .contains("超级块校验值不一致"));
        }
    }

    #[test]
    fn reads_retained_file_with_its_own_checksums_and_rejects_expired_root() {
        let mut image = retained_image(false);
        let mut volume = open(Cursor::new(&image), || Ok(())).unwrap();
        assert!(volume.snapshots().is_empty());
        let [root] = volume
            .retained_roots(|| Ok(()))
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            root,
            RetainedRoot {
                subvolume: 5,
                transaction: 8,
                root_directory: 256,
            }
        );
        assert!(volume.find_inode(5, 258, || Ok(())).unwrap().is_none());
        let mut output = Vec::new();
        volume
            .read_file(root, 258, 16, &mut output, || Ok(()))
            .unwrap();
        assert_eq!(output, [23; 16]);

        image[(3 * MIB + 28672 + 104) as usize] ^= 1;
        let mut volume = open(Cursor::new(image), || Ok(())).unwrap();
        assert!(volume.retained_roots(|| Ok(())).unwrap().is_empty());
        assert!(volume
            .read_file(root, 258, 16, &mut Vec::new(), || Ok(()))
            .is_err());
    }

    #[test]
    fn verifies_compressed_allocation_before_exporting_the_referenced_slice() {
        let mut unpacked = [23u8; 8192];
        unpacked[4096..].fill(53);
        let packed = zstd::bulk::compress(&unpacked, 1).unwrap();
        let mut disk = image();
        let at = (3 * MIB + 4096) as usize;
        let leaf = &mut disk[at..at + 4096];
        let item_offset = u32::from_le_bytes(leaf[218..222].try_into().unwrap()) as usize;
        let item = &mut leaf[101 + item_offset..][..53];
        set64(item, 8, 8192);
        item[16] = 3;
        set64(item, 37, 4096);
        sign(leaf);

        let at = (4 * MIB) as usize;
        let data = &mut disk[at..at + 4096];
        data.fill(0);
        data[..packed.len()].copy_from_slice(&packed);
        let sum = crc32c(data);
        let at = (3 * MIB + 8192) as usize;
        let leaf = &mut disk[at..at + 4096];
        set32(leaf, 4092, sum);
        sign(leaf);

        let mut volume = open(Cursor::new(&disk), || Ok(())).unwrap();
        let mut bytes = Vec::new();
        volume.read_file(5, 257, 3, &mut bytes, || Ok(())).unwrap();
        assert_eq!(bytes, [53; 3]);
        bytes.clear();
        volume
            .read_file(5, 257, u64::MAX, &mut bytes, || Ok(()))
            .unwrap();
        assert_eq!(bytes[..4096], [53; 4096]);
        assert_eq!(bytes[4096..], [0; 7]);

        disk[(4 * MIB) as usize + 4095] = 1;
        let mut volume = open(Cursor::new(disk), || Ok(())).unwrap();
        bytes.clear();
        assert!(volume
            .read_file(5, 257, 3, &mut bytes, || Ok(()))
            .unwrap_err()
            .contains("CRC32C"));
        assert!(bytes.is_empty());
    }
}

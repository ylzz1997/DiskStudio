//! Recover freed Ext4 inode data using checksummed, committed journal images.
//! The current allocation maps are authoritative for access to data blocks.

use super::{journal::Journal, *};
use crate::used_space::Allocation;
use std::{collections::BTreeMap, ops::Range};

const MAX_RESULTS: usize = 4096;
const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_CONTENT: u64 = 1024 * 1024 * 1024;
const MAX_DIR_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DIRECTORIES: usize = 8192;
const MAX_DIRECTORY_ENTRIES: usize = 250_000;
const MAX_DIRECTORY_BLOCKS: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    inode: u32,
    filesystem: [u8; 32],
    journal: [u8; 32],
    before: [u8; 32],
    deleted: [u8; 32],
    name: Option<[u8; 32]>,
    content: [u8; 32],
}

struct Witness {
    inode: u32,
    generation: u32,
    old: Vec<u8>,
    incarnation: usize,
    before: usize,
    deleted: usize,
    size: u64,
    runs: Vec<Run>,
}

struct Run {
    logical: Range<u64>,
    physical: u64,
}

struct Named {
    bytes: Vec<u8>,
    parent: Arc<Ancestor>,
}

impl Named {
    fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update((self.bytes.len() as u32).to_le_bytes());
        digest.update(&self.bytes);
        digest.update(self.parent.proof.inode.to_le_bytes());
        digest.update(self.parent.proof.record);
        digest.update(&self.parent.proof.path);
        digest.finalize().into()
    }
}

struct History<R> {
    session: Session<R>,
    journal: Journal,
    transactions: Vec<journal::Transaction>,
    allocation: Allocation,
}

impl<R: Read + Seek + 'static> History<R> {
    fn read(
        reader: R,
        cancel: Arc<AtomicBool>,
        tick: &mut impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut session = Session::new(reader, cancel.clone())?;
        if session.geometry.journal_inode == 0
            || !session.geometry.metadata_csum
            || session.geometry.creator_os != 0
            || session.geometry.first_normal_inode < 11
        {
            return Err("Ext 删除恢复需要带内部日志和元数据校验的 Linux Ext4。".into());
        }
        let mut file = session
            .fs
            .open_internal_journal(session.geometry.journal_inode)
            .map_err(err)?;
        session.allocated(file.metadata())?;
        let bytes = file.metadata().len();
        if bytes == 0 || bytes > journal::MAX_JOURNAL_BYTES {
            return Err("Ext4 内部日志超出 128 MiB 检查上限。".into());
        }
        let mut data = vec![0; bytes as usize];
        for (i, mut chunk) in data.chunks_mut(1024 * 1024).enumerate() {
            tick((i * 1024 * 1024) as u64, bytes)?;
            while !chunk.is_empty() {
                let read = file.read_bytes(chunk).map_err(err)?;
                if read == 0 {
                    return Err("Ext4 内部日志提前结束。".into());
                }
                chunk = &mut chunk[read..];
            }
        }
        let journal = Journal::new(
            data,
            session.geometry.block as usize,
            session.geometry.size / session.geometry.block,
            session.geometry.uuid,
        )?;
        let transactions = journal.transactions(&cancel)?;
        let allocation =
            crate::ext_allocation::inspect(&mut session.source.borrow_mut().reader, &cancel)?;
        if allocation.filesystem != "Ext4" {
            return Err("当前来源不是受支持的 Ext4 卷。".into());
        }
        Ok(Self {
            session,
            journal,
            transactions,
            allocation,
        })
    }

    fn inode_tables(&self) -> Result<Vec<(Range<u64>, u32)>, String> {
        let geometry = &self.session.geometry;
        let blocks =
            (geometry.per_group as u64 * geometry.inode_size as u64).div_ceil(geometry.block);
        let mut tables = Vec::new();
        for (group, d) in geometry
            .descriptors
            .chunks_exact(geometry.descriptor_size)
            .enumerate()
        {
            let start = u64::from(u32le(d, 8))
                | if d.len() == 64 {
                    u64::from(u32le(d, 0x28)) << 32
                } else {
                    0
                };
            let end = start
                .checked_add(blocks)
                .filter(|&end| end <= geometry.size / geometry.block)
                .ok_or("Ext4 inode 表范围无效。")?;
            if u16le(d, 0x12) & 1 == 0 {
                tables.push((start..end, group as u32));
            }
        }
        tables.sort_unstable_by_key(|(range, _)| range.start);
        if tables
            .windows(2)
            .any(|pair| pair[0].0.end > pair[1].0.start)
        {
            return Err("Ext4 inode 表范围重叠。".into());
        }
        Ok(tables)
    }

    fn witnesses(&mut self) -> Result<Vec<Witness>, String> {
        let tables = self.inode_tables()?;
        let geometry = self.session.geometry.clone();
        let per_block = geometry.block as usize / geometry.inode_size;
        let mut targets = BTreeMap::new();
        for (transaction, log) in self.transactions.iter().enumerate() {
            for &address in log.snapshots.keys() {
                let table = tables.partition_point(|(range, _)| range.end <= address);
                if tables
                    .get(table)
                    .is_some_and(|(range, _)| range.contains(&address))
                {
                    targets
                        .entry((table, address))
                        .or_insert_with(Vec::new)
                        .push(transaction);
                }
            }
        }
        let mut results = Vec::new();
        let mut current = vec![0; geometry.block as usize];
        for ((table, address), ids) in targets {
            if ids.len() < 2 {
                continue;
            }
            let (range, group) = &tables[table];
            let first = u64::from(*group) * u64::from(geometry.per_group)
                + (address - range.start) * per_block as u64
                + 1;
            self.session
                .source
                .borrow_mut()
                .read_at(address * geometry.block, &mut current)?;
            // Materialize only this inode-table block's retained versions.
            let snapshots: Vec<_> = ids
                .iter()
                .map(|&id| {
                    (
                        id,
                        self.journal
                            .snapshot(self.transactions[id].snapshots[&address]),
                    )
                })
                .collect();
            for (slot, record) in current.chunks_exact(geometry.inode_size).enumerate() {
                let member = first + slot as u64;
                let inode = u32::try_from(member).map_err(err)?;
                if inode < geometry.first_normal_inode
                    || inode > geometry.inodes
                    || (inode - 1) / geometry.per_group != *group
                    || u16le(record, 0) & 0xf000 != 0x8000
                    || u32le(record, 0x14) == 0
                    || u16le(record, 0x1a) != 0
                    || !checked_inode(&geometry, inode, record)
                    || self.session.inode_allocated(inode)?
                {
                    continue;
                }
                let index = slot * geometry.inode_size;
                let history = snapshots
                    .iter()
                    .map(|(id, block)| (*id, &block[index..index + geometry.inode_size]));
                let mut history = history.rev().peekable();
                let mut deleted = None;
                while history.peek().is_some_and(|(_, old)| *old == record) {
                    deleted = history.next().map(|(id, _)| id);
                }
                let (Some(deleted), Some((before, old))) = (deleted, history.next()) else {
                    continue;
                };
                let generation = u32le(record, 0x64);
                if u16le(old, 0) & 0xf000 != 0x8000
                    || u16le(old, 0x1a) == 0
                    || u32le(old, 0x14) != 0
                    || u32le(old, 0x64) != generation
                    || !checked_inode(&geometry, inode, old)
                    || self.transactions[before + 1..=deleted]
                        .iter()
                        .any(|txn| txn.revoked.contains(&address))
                {
                    continue;
                }
                let size = u64::from(u32le(old, 4)) | (u64::from(u32le(old, 0x6c)) << 32);
                let Some(runs) = extents(old, size, &geometry, &self.allocation) else {
                    continue;
                };
                let mut incarnation = before;
                for (older, raw) in history {
                    if u16le(raw, 0) & 0xf000 != 0x8000
                        || u16le(raw, 0x1a) == 0
                        || u32le(raw, 0x14) != 0
                        || u32le(raw, 0x64) != generation
                        || !checked_inode(&geometry, inode, raw)
                        || self.transactions[older + 1..=incarnation]
                            .iter()
                            .any(|txn| txn.revoked.contains(&address))
                    {
                        break;
                    }
                    incarnation = older;
                }
                if results.len() == MAX_RESULTS {
                    return Err("Ext4 删除候选超过 4096 项检查上限。".into());
                }
                results.push(Witness {
                    inode,
                    generation,
                    old: old.to_vec(),
                    incarnation,
                    before,
                    deleted,
                    size,
                    runs,
                });
            }
        }
        results.sort_unstable_by_key(|proof| proof.inode);
        Ok(results)
    }

    fn names(&mut self, proofs: &[Witness]) -> Result<BTreeMap<u32, Named>, String> {
        let records: BTreeMap<_, _> = proofs.iter().map(|proof| (proof.inode, proof)).collect();
        let mut names = BTreeMap::new();
        let mut ambiguous = HashSet::new();
        if records.is_empty() {
            return Ok(names);
        }
        let mut targets = BTreeMap::<u64, Vec<usize>>::new();
        for (index, transaction) in self.transactions.iter().enumerate() {
            for &address in transaction.snapshots.keys() {
                targets.entry(address).or_default().push(index);
            }
        }
        let root = self
            .session
            .fs
            .symlink_metadata(b"/".as_slice())
            .map_err(err)?;
        let root = node(root, b"/".to_vec(), None);
        let mut dirs = vec![root];
        let mut seen = HashSet::from([2]);
        let mut entries = 0;
        let mut blocks = 0;
        for index in 0..MAX_DIRECTORIES {
            crate::used_space::stop(&self.session.source.borrow().cancel)?;
            let Some(parent) = dirs.get(index).cloned() else {
                break;
            };
            self.session.prove(&parent.proof)?;
            let path = parent.proof.path.as_slice();
            let metadata = self.session.fs.symlink_metadata(path).map_err(err)?;
            if !metadata.is_dir() || metadata.len() > MAX_DIR_BYTES {
                continue;
            }
            let seed = inode_seed(
                &self.session.geometry,
                metadata.inode_number(),
                metadata.inode_record(),
            );
            let block_size = self.session.geometry.block;
            for address in self
                .session
                .fs
                .directory_blocks(path, MAX_DIR_BYTES)
                .map_err(err)?
            {
                blocks += 1;
                if blocks > MAX_DIRECTORY_BLOCKS {
                    return Ok(names);
                }
                let Some(indices) = targets.get(&address) else {
                    continue;
                };
                let mut current = None;
                for &index in indices {
                    let snapshot = &self.transactions[index].snapshots[&address];
                    let old = self.journal.snapshot(*snapshot);
                    for (inode, name) in names_in_block(seed, &old).unwrap_or_default() {
                        let Some(proof) = records.get(&inode).filter(|proof| {
                            (proof.incarnation..=proof.before).contains(&index)
                                && !self.transactions[index + 1..proof.deleted]
                                    .iter()
                                    .any(|txn| txn.snapshots.contains_key(&address))
                        }) else {
                            continue;
                        };
                        let Some(tombstone) =
                            self.transactions[proof.deleted].snapshots.get(&address)
                        else {
                            continue;
                        };
                        if current.is_none() {
                            let mut data = vec![0; block_size as usize];
                            self.session
                                .source
                                .borrow_mut()
                                .read_at(address * block_size, &mut data)?;
                            current = Some(data);
                        }
                        let current = current.as_ref().unwrap();
                        if self.journal.snapshot(*tombstone) != *current
                            || names_in_block(seed, current).is_none_or(|live| {
                                live.iter().any(|(i, n)| inode == *i || name == *n)
                            })
                            || self.transactions[index + 1..=proof.deleted]
                                .iter()
                                .any(|txn| txn.revoked.contains(&address))
                        {
                            continue;
                        }
                        if ambiguous.contains(&inode) {
                            continue;
                        }
                        let hit = Named {
                            bytes: name,
                            parent: parent.clone(),
                        };
                        if names.insert(inode, hit).is_some() {
                            ambiguous.insert(inode);
                            names.remove(&inode);
                        }
                    }
                }
            }
            if parent.depth >= MAX_DEPTH {
                continue;
            }
            for entry in self.session.fs.read_dir(path).map_err(err)? {
                entries += 1;
                if entries > MAX_DIRECTORY_ENTRIES {
                    return Ok(names);
                }
                let entry = entry.map_err(err)?;
                let name = entry.file_name();
                if name.as_ref() == b"."
                    || name.as_ref() == b".."
                    || !entry.file_type().map_err(err)?.is_dir()
                {
                    continue;
                }
                let metadata = entry.metadata().map_err(err)?;
                self.session.allocated(&metadata)?;
                if dirs.len() < MAX_DIRECTORIES && seen.insert(metadata.inode_number()) {
                    let mut full = path.to_vec();
                    if full != b"/" {
                        full.push(b'/');
                    }
                    full.extend_from_slice(name.as_ref());
                    dirs.push(node(metadata, full, Some(parent.clone())));
                }
            }
        }
        Ok(names)
    }

    fn location(
        &self,
        proof: &Witness,
        journal: [u8; 32],
        name: Option<&Named>,
        content: [u8; 32],
    ) -> Location {
        Location {
            inode: proof.inode,
            filesystem: self.session.geometry.fingerprint,
            journal,
            before: self.transactions[proof.before].proof,
            deleted: self.transactions[proof.deleted].proof,
            name: name.map(Named::digest),
            content,
        }
    }

    fn content<W: Write>(
        &mut self,
        proof: &Witness,
        mut output: W,
        output_limit: u64,
        tick: &mut impl FnMut(u64) -> Result<(), String>,
    ) -> Result<([u8; 32], u64), String> {
        let mut position = 0;
        let mut buffer = vec![0; 1024 * 1024];
        let mut sum = Sha256::new();
        for run in &proof.runs {
            while position < run.logical.start {
                let len = (run.logical.start - position).min(buffer.len() as u64) as usize;
                buffer[..len].fill(0);
                write_chunk(
                    &buffer[..len],
                    &mut sum,
                    &mut output,
                    &mut position,
                    output_limit,
                )?;
                tick(position)?;
            }
            while position < run.logical.end {
                let len = (run.logical.end - position).min(buffer.len() as u64) as usize;
                let at = run.physical + position - run.logical.start;
                self.session
                    .source
                    .borrow_mut()
                    .read_at(at, &mut buffer[..len])?;
                write_chunk(
                    &buffer[..len],
                    &mut sum,
                    &mut output,
                    &mut position,
                    output_limit,
                )?;
                tick(position)?;
            }
        }
        while position < proof.size {
            let len = (proof.size - position).min(buffer.len() as u64) as usize;
            buffer[..len].fill(0);
            write_chunk(
                &buffer[..len],
                &mut sum,
                &mut output,
                &mut position,
                output_limit,
            )?;
            tick(position)?;
        }
        Ok((sum.finalize().into(), position.min(output_limit)))
    }
}

fn write_chunk(
    bytes: &[u8],
    sum: &mut Sha256,
    output: &mut impl Write,
    offset: &mut u64,
    limit: u64,
) -> Result<(), String> {
    sum.update(bytes);
    let write = (limit.saturating_sub(*offset)).min(bytes.len() as u64) as usize;
    output.write_all(&bytes[..write]).map_err(err)?;
    *offset += bytes.len() as u64;
    Ok(())
}

fn inode_seed(geometry: &Geometry, inode: u32, raw: &[u8]) -> u32 {
    crc(
        crc(geometry.checksum_seed, &inode.to_le_bytes()),
        &raw[0x64..0x68],
    )
}

fn checked_inode(geometry: &Geometry, inode: u32, raw: &[u8]) -> bool {
    raw.len() >= 256
        && u16le(raw, 0x80) >= 4
        && crc(
            crc(
                crc(
                    crc(crc(inode_seed(geometry, inode, raw), &raw[..0x7c]), &[0; 2]),
                    &raw[0x7e..0x82],
                ),
                &[0; 2],
            ),
            &raw[0x84..],
        ) == (u32::from(u16le(raw, 0x7c)) | u32::from(u16le(raw, 0x82)) << 16)
}

fn extents(
    raw: &[u8],
    size: u64,
    geometry: &Geometry,
    allocation: &Allocation,
) -> Option<Vec<Run>> {
    let tree = &raw[0x28..0x64];
    let count = usize::from(u16le(tree, 2));
    let flags = u32le(raw, 0x20);
    if size > MAX_FILE
        || flags & !0x80000 != 0
        || flags & 0x80000 == 0
        || u16le(tree, 0) != 0xf30a
        || count > 4
        || u16le(tree, 4) != 4
        || u16le(tree, 6) != 0
    {
        return None;
    }
    let block = geometry.block;
    let mut runs = Vec::new();
    let mut physical = Vec::new();
    let mut last = 0;
    for extent in tree[12..12 + count * 12].chunks_exact(12) {
        let start = u64::from(u32le(extent, 0)) * block;
        let length = u64::from(u16le(extent, 4)) * block;
        let disk =
            (u64::from(u32le(extent, 8)) | u64::from(u16le(extent, 6)) << 32).checked_mul(block)?;
        let end = disk.checked_add(length)?;
        let range = allocation.free.partition_point(|range| range.end <= disk);
        if length == 0
            || u16le(extent, 4) > 32768
            || start < last
            || start >= size
            || start + length > size.div_ceil(block) * block
            || end > geometry.size
            || allocation
                .free
                .get(range)
                .is_none_or(|range| disk < range.start || end > range.end)
        {
            return None;
        }
        physical.push(disk..end);
        last = start + length;
        runs.push(Run {
            logical: start..last.min(size),
            physical: disk,
        });
    }
    physical.sort_unstable_by_key(|range| range.start);
    (!physical.windows(2).any(|pair| pair[0].end > pair[1].start)).then_some(runs)
}

fn names_in_block(seed: u32, bytes: &[u8]) -> Option<Vec<(u32, Vec<u8>)>> {
    let end = bytes.len() - 12;
    if bytes[end..end + 8] != [0, 0, 0, 0, 12, 0, 0, 0xde]
        || crc(seed, &bytes[..end]) != u32le(bytes, end + 8)
    {
        return None;
    }
    let mut names = Vec::new();
    let mut at = 0;
    while at < end {
        if end - at < 8 {
            return None;
        }
        let inode = u32le(bytes, at);
        let size = usize::from(u16le(bytes, at + 4));
        let length = usize::from(bytes[at + 6]);
        if size < 8 || size % 4 != 0 || size > end - at || length > size - 8 {
            return None;
        }
        if inode != 0 && bytes[at + 7] == 1 {
            let raw = &bytes[at + 8..at + 8 + length];
            if raw.is_empty() || raw.contains(&0) || raw.contains(&b'/') {
                return None;
            }
            names.push((inode, raw.to_vec()));
        }
        at += size;
    }
    Some(names)
}

fn node(metadata: Metadata, path: Vec<u8>, parent: Option<Arc<Ancestor>>) -> Arc<Ancestor> {
    Arc::new(Ancestor {
        proof: Proof {
            path,
            inode: metadata.inode_number(),
            record: Sha256::digest(metadata.inode_record()).into(),
        },
        depth: parent.as_ref().map_or(0, |node| node.depth + 1),
        parent,
    })
}

fn directory_id(node: &Ancestor) -> String {
    if node.proof.inode == 2 {
        ROOT.to_owned()
    } else {
        format!("ext-journal-{}", node.proof.inode)
    }
}

fn witness_name(proof: &Witness, name: Option<&Named>) -> String {
    name.map_or_else(
        || format!("ext4-inode-{}-{:08x}", proof.inode, proof.generation),
        |name| String::from_utf8_lossy(&name.bytes).into_owned(),
    )
}

fn reject_overlaps(proofs: &mut Vec<Witness>) {
    let mut ranges = Vec::new();
    for proof in proofs.iter() {
        for run in &proof.runs {
            ranges.push((
                run.physical,
                run.physical + run.logical.end - run.logical.start,
                proof.inode,
            ));
        }
    }
    ranges.sort_unstable();
    let mut furthest = None;
    let mut ambiguous = HashSet::new();
    for (start, end, inode) in ranges {
        if let Some((previous_end, previous_inode)) = furthest {
            if start < previous_end && inode != previous_inode {
                ambiguous.insert(previous_inode);
                ambiguous.insert(inode);
            }
            if previous_end >= end {
                continue;
            }
        }
        furthest = Some((end, inode));
    }
    proofs.retain(|proof| !ambiguous.contains(&proof.inode));
}

pub(crate) fn scan<R: Read + Seek + 'static>(
    reader: R,
    cancel: Arc<AtomicBool>,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    let mut history = History::read(reader, cancel.clone(), &mut tick)?;
    let journal = history.journal.digest();
    let mut proofs = history.witnesses()?;
    reject_overlaps(&mut proofs);
    let mut names = history.names(&proofs)?;
    let mut emitted = HashSet::new();
    let mut budget = MAX_CONTENT;
    let mut count = 0;
    for proof in proofs {
        crate::used_space::stop(&cancel)?;
        budget = budget
            .checked_sub(proof.size)
            .ok_or("Ext4 候选内容达到 1 GiB 摘要检查上限。")?;
        let (content, _) = history.content(&proof, std::io::sink(), 0, &mut |_| {
            crate::used_space::stop(&cancel)
        })?;
        let named = names.remove(&proof.inode);
        let parent = named.as_ref().map(|witness| directory_id(&witness.parent));
        if let Some(witness) = &named {
            let mut chain = Vec::new();
            let mut next = Some(witness.parent.as_ref());
            while let Some(ancestor) = next {
                if ancestor.proof.inode == 2 || !emitted.insert(ancestor.proof.inode) {
                    break;
                }
                chain.push(ancestor);
                next = ancestor.parent.as_deref();
            }
            for node in chain.into_iter().rev() {
                let mut metadata = crate::file_metadata::Metadata::default();
                metadata.directory_proof =
                    Some(crate::directory_metadata::Proof::Ext(DirectoryLocation {
                        filesystem: history.session.geometry.fingerprint,
                        node: Arc::new(node.clone()),
                    }));
                directory(Directory {
                    id: directory_id(node),
                    parent: node.parent.as_ref().map(|parent| directory_id(parent)),
                    name: String::from_utf8_lossy(
                        node.proof.path.rsplit(|b| *b == b'/').next().unwrap(),
                    )
                    .into_owned(),
                    deleted: false,
                    metadata,
                })?;
            }
        }
        count += 1;
        found(Candidate {
            id: count,
            name: witness_name(&proof, named.as_ref()),
            size: proof.size,
            deleted: true,
            method: "Ext4 日志".into(),
            note: "已核对删除前的 inode、删除记录和当前空闲块。当前读取的字节仍可能受覆盖或 TRIM 影响。".into(),
            supported: true,
            directory: parent.clone(),
            directory_note: if parent.is_some() { "目录由已提交的日志记录确认" } else { "日志中缺少可核对的原始文件名" }.into(),
            metadata: crate::file_metadata::ext(&proof.old, true),
            locator: Locator::ExtDeleted(history.location(&proof, journal, named.as_ref(), content)),
        });
    }
    tick(history.journal.bytes(), history.journal.bytes())?;
    Ok(format!("Ext4 日志删除候选（{count}）。"))
}

pub(crate) fn extract<R: Read + Seek + 'static, W: Write>(
    reader: R,
    candidate: &Candidate,
    location: &Location,
    data: &mut W,
    limit: u64,
    cancel: Arc<AtomicBool>,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<crate::recovery::Extraction, String> {
    if !candidate.deleted || !candidate.supported {
        return Err("Ext4 日志候选不可提取。".into());
    }
    tick(0)?;
    let mut history = History::read(reader, cancel.clone(), &mut |_, _| {
        crate::used_space::stop(&cancel)
    })?;
    let digest = history.journal.digest();
    if digest != location.journal || history.session.geometry.fingerprint != location.filesystem {
        return Err("Ext4 文件系统或保留日志已改变，请重新扫描。".into());
    }
    let mut proofs = history.witnesses()?;
    reject_overlaps(&mut proofs);
    let proof = proofs
        .into_iter()
        .find(|proof| proof.inode == location.inode)
        .ok_or("Ext4 删除凭据已失效。")?;
    let named = history
        .names(std::slice::from_ref(&proof))?
        .remove(&proof.inode);
    let (content, _) = history.content(&proof, std::io::sink(), 0, &mut |_| {
        crate::used_space::stop(&cancel)
    })?;
    if candidate.size != proof.size
        || candidate.name != witness_name(&proof, named.as_ref())
        || candidate.directory != named.as_ref().map(|name| directory_id(&name.parent))
        || history.location(&proof, digest, named.as_ref(), content) != *location
    {
        return Err("Ext4 删除候选及数据摘要与扫描记录不一致。".into());
    }
    let (after, bytes) = history.content(&proof, data, limit, &mut |done| tick(done.min(limit)))?;
    if after != content {
        return Err("Ext4 候选内容在提取过程中改变。".into());
    }
    Ok(crate::recovery::Extraction {
        bytes,
        metadata: crate::file_metadata::ext(&proof.old, true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_tree::Catalog;
    use std::io::Cursor;

    fn fixture() -> Vec<u8> {
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(
            include_bytes!("../../tests/fixtures/ext-journal-linux.raw.gz").as_slice(),
        )
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .unwrap();
        assert_eq!(raw.len(), 32 * 1024 * 1024);
        raw
    }

    fn scan_fixture(raw: Arc<[u8]>) -> (Vec<Candidate>, Catalog) {
        let mut files = Vec::new();
        let mut catalog = Catalog::default();
        scan(
            Cursor::new(raw),
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |candidate| files.push(candidate),
            |directory| catalog.insert(directory),
        )
        .unwrap();
        (files, catalog)
    }

    #[test]
    fn linux_ext4_journal_rejects_changed_data_and_allocated_blocks() {
        let raw: Arc<[u8]> = fixture().into();
        let (files, _) = scan_fixture(raw.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut history =
            History::read(Cursor::new(raw.clone()), cancel.clone(), &mut |_, _| Ok(())).unwrap();
        let proof = history.witnesses().unwrap().remove(0);
        let offset = proof.runs[0].physical as usize;
        let mut changed = raw.to_vec();
        changed[offset] ^= 1;
        let mut output = Vec::new();
        let error = crate::recovery::extract_owned(
            Cursor::new(changed),
            &files[0],
            &mut output,
            u64::MAX,
            cancel.clone(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("摘要"));
        assert!(output.is_empty());

        // Mark the first deleted file's data block allocated and update the
        // owning group's bitmap, counters and checksums. This leaves the second
        // deleted file provably free on a fully consistent allocation map.
        let block = history.session.geometry.block as usize;
        let seed = history.session.geometry.checksum_seed;
        let mut reused = raw.to_vec();
        let bitmap = u32le(&reused, block) as usize * block;
        reused[bitmap + offset / block / 8] |= 1 << (offset / block % 8);
        let used_checksum = crc(seed, &reused[bitmap..bitmap + block]);
        let descriptor = &mut reused[block..block + 64];
        let free = u16le(descriptor, 0xc) - 1;
        descriptor[0xc..0xe].copy_from_slice(&free.to_le_bytes());
        descriptor[0x18..0x1a].copy_from_slice(&(used_checksum as u16).to_le_bytes());
        descriptor[0x38..0x3a].copy_from_slice(&((used_checksum >> 16) as u16).to_le_bytes());
        descriptor[0x1e..0x20].fill(0);
        let descriptor_sum = crc(crc(seed, &0u32.to_le_bytes()), descriptor) as u16;
        descriptor[0x1e..0x20].copy_from_slice(&descriptor_sum.to_le_bytes());
        let header = &mut reused[1024..2048];
        let free = u32le(header, 0xc) - 1;
        header[0xc..0x10].copy_from_slice(&free.to_le_bytes());
        let header_sum = crc(!0, &header[..0x3fc]);
        header[0x3fc..].copy_from_slice(&header_sum.to_le_bytes());
        let (remaining, _) = scan_fixture(reused.into());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "fragment.bin");
    }

    #[test]
    fn linux_ext4_journal_cannot_bridge_a_corrupted_commit() {
        let mut disk = fixture();
        // The Linux fixture's transaction 5 commit is logical log block 25,
        // the first block in the journal's third physical run at block 550.
        disk[550 * 4096 + 16] ^= 1;
        let (files, _) = scan_fixture(disk.into());
        assert!(files.is_empty());
    }

    #[test]
    fn linux_ext4_journal_deleted_files_keep_names_sparse_data_and_original_hashes() {
        let raw: Arc<[u8]> = fixture().into();
        let mut found = Vec::new();
        let mut tree = Catalog::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let message = scan(
            Cursor::new(raw.clone()),
            cancel.clone(),
            |_, _| Ok(()),
            |candidate| found.push(candidate),
            |directory| tree.insert(directory),
        )
        .unwrap();
        assert_eq!(
            found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["removed.txt", "fragment.bin"],
            "{message}"
        );
        assert_eq!(found[0].directory.as_deref(), Some(ROOT));
        assert_eq!(
            tree.snapshot()
                .iter()
                .find(|node| Some(node.id.as_str()) == found[1].directory.as_deref())
                .unwrap()
                .name,
            "nested"
        );
        for (candidate, (size, digest)) in found.iter().zip([
            (
                39,
                "e3db9e1888e849f3d5b3d5347ecbb7b2ce0bd2dd93f7f73dd117d2d93c53b48e",
            ),
            (
                15668,
                "e66791aecd54ff8a0d6cafdab9cf46d98c22a965211b615831c8065806ee2069",
            ),
        ]) {
            assert!(candidate.deleted && candidate.supported);
            let mut output = Vec::new();
            crate::recovery::extract_owned(
                Cursor::new(raw.clone()),
                candidate,
                &mut output,
                size,
                cancel.clone(),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(output.len() as u64, size);
            assert_eq!(format!("{:x}", Sha256::digest(output)), digest);
        }
    }
}

//! Bounded reader for the committed device-mapper thin metadata.
use super::*;
use btrfs_disk::util::btrfs_csum_data as crc32c;

const BLOCK_BYTES: usize = 4096;
const SUPERBLOCK_SALT: u32 = 160774;
const TREE_SALT: u32 = 121107;
const BITMAP_SALT: u32 = 240779;
const BITMAP_ENTRIES: u64 = ((BLOCK_BYTES - 16) * 4) as u64;
const MAX_DEPTH: u8 = 12;

pub(super) enum Error {
    Invalid,
    Limit,
    Stopped(String),
}

pub(super) struct Budget {
    blocks: usize,
    runs: usize,
}

impl Default for Budget {
    fn default() -> Self {
        // Bound work and retained mappings across every selected pool.
        Self {
            blocks: 65536,
            runs: 1_000_000,
        }
    }
}

impl Budget {
    fn block(&mut self) -> Result<(), Error> {
        self.blocks = self.blocks.checked_sub(1).ok_or(Error::Limit)?;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.runs = self.runs.checked_sub(1).ok_or(Error::Limit)?;
        Ok(())
    }
}

pub(super) struct Run {
    pub logical: Range<u64>,
    pub physical: u64,
}

impl Run {
    fn len(&self) -> u64 {
        self.logical.end - self.logical.start
    }
}

struct Superblock {
    mapping: u64,
    details: u64,
    allocation: u64,
    metadata_blocks: u64,
    data_blocks: u64,
    time: u32,
}

fn checksum(block: &[u8; BLOCK_BYTES], salt: u32) -> bool {
    u32le(block, 0) == (!crc32c(&block[4..]) ^ salt)
}

impl Superblock {
    fn parse(
        block: &[u8; BLOCK_BYTES],
        metadata: u64,
        data: u64,
        chunk: u64,
        tx: u64,
    ) -> Result<Self, Error> {
        let sb = Self {
            mapping: u64le(block, 320),
            details: u64le(block, 328),
            allocation: u64le(block, 80),
            metadata_blocks: u64le(block, 344),
            data_blocks: u64le(block, 64),
            time: u32le(block, 44),
        };
        let held = u64le(block, 56);
        if !checksum(block, SUPERBLOCK_SALT)
            || u32le(block, 4) != 0
            || u64le(block, 8) != 0
            || u64le(block, 32) != 27022010
            || !matches!(u32le(block, 40), 1 | 2)
            || u64le(block, 48) != tx
            || u64::from(u32le(block, 336)) != chunk / SECTOR
            || u64::from(u32le(block, 340)) != BLOCK_BYTES as u64 / SECTOR
            || u64le(block, 192) != sb.metadata_blocks
            || sb.metadata_blocks > metadata / BLOCK_BYTES as u64
            || sb.data_blocks == 0
            || sb.data_blocks != data / chunk
            || u64le(block, 72) > sb.data_blocks
            || u64le(block, 200) > sb.metadata_blocks
            || u32le(block, 352) != 0
            || u32le(block, 356) != 0
            || u32le(block, 360) != 0
            || [sb.mapping, sb.details, sb.allocation]
                .into_iter()
                .any(|n| !sb.address(n))
            || (held != 0 && !sb.address(held))
        {
            return Err(Error::Invalid);
        }
        Ok(sb)
    }

    fn address(&self, block: u64) -> bool {
        (1..self.metadata_blocks).contains(&block)
    }
}

struct Reader<'a, R, C> {
    metadata: &'a Volume,
    readers: &'a mut [R],
    checkpoint: &'a mut C,
    budget: &'a mut Budget,
    proof: &'a mut Sha256,
    scratch: Vec<u8>,
    blocks: u64,
}

impl<R: Read + Seek, C: FnMut() -> Result<(), String>> Reader<'_, R, C> {
    fn block(&mut self, index: u64) -> Result<[u8; BLOCK_BYTES], Error> {
        if index >= self.blocks {
            return Err(Error::Invalid);
        }
        (self.checkpoint)().map_err(Error::Stopped)?;
        self.budget.block()?;
        let mut buffer = [0; BLOCK_BYTES];
        self.metadata
            .read_exact_at(
                self.readers,
                index * BLOCK_BYTES as u64,
                &mut buffer,
                &mut self.scratch,
            )
            .map_err(|_| Error::Invalid)?;
        self.proof.update(index.to_le_bytes());
        self.proof.update(buffer);
        Ok(buffer)
    }

    fn walk(
        &mut self,
        root: u64,
        keys: Range<u64>,
        value_bytes: usize,
        mut visit: impl FnMut(u64, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut walk = Walk {
            value_bytes,
            visited: HashSet::new(),
            visit: &mut visit,
        };
        walk.node(self, root, keys, 0).map(|_| ())
    }
}

struct Walk<'a, V> {
    value_bytes: usize,
    visited: HashSet<u64>,
    visit: &'a mut V,
}

impl<V: FnMut(u64, &[u8]) -> Result<(), Error>> Walk<'_, V> {
    fn node(
        &mut self,
        reader: &mut Reader<'_, impl Read + Seek, impl FnMut() -> Result<(), String>>,
        index: u64,
        keys: Range<u64>,
        depth: u8,
    ) -> Result<u8, Error> {
        if index == 0 || depth >= MAX_DEPTH || !self.visited.insert(index) {
            return Err(Error::Invalid);
        }
        let block = reader.block(index)?;
        let kind = u32le(&block, 4);
        let entries = u32le(&block, 16) as usize;
        let slots = u32le(&block, 20) as usize;
        let value_bytes = if kind == 1 { 8 } else { self.value_bytes };
        if !matches!(kind, 1 | 2)
            || !checksum(&block, TREE_SALT)
            || u64le(&block, 8) != index
            || u32le(&block, 24) as usize != value_bytes
            || slots == 0
            || slots > (BLOCK_BYTES - 32) / (8 + value_bytes)
            || entries > slots
            || (entries == 0 && (kind == 1 || depth != 0))
        {
            return Err(Error::Invalid);
        }
        let key_at = |entry| u64le(&block, 32 + 8 * entry);
        let mut prev = None;
        for entry in 0..entries {
            let key = key_at(entry);
            if !keys.contains(&key) || prev.is_some_and(|previous| previous >= key) {
                return Err(Error::Invalid);
            }
            prev = Some(key);
        }
        let values = 32 + 8 * slots;
        let mut branch_depth = None;
        for entry in 0..entries {
            let key = key_at(entry);
            let start = values + entry * value_bytes;
            let value = &block[start..start + value_bytes];
            if kind == 1 {
                let end = if entry + 1 < entries {
                    key_at(entry + 1)
                } else {
                    keys.end
                };
                let descent = self.node(reader, u64le(value, 0), key..end, depth + 1)?;
                if branch_depth.is_some_and(|other| other != descent) {
                    return Err(Error::Invalid);
                }
                branch_depth = Some(descent);
            } else {
                (self.visit)(key, value)?;
            }
        }
        Ok(branch_depth.unwrap_or(depth))
    }
}

struct Device {
    mapped: u64,
    transaction: u64,
    created: u32,
    snapshot: u32,
}

pub(super) struct Pool {
    sb: Superblock,
    roots: BTreeMap<u64, u64>,
    details: BTreeMap<u64, Device>,
    index: BTreeMap<u64, u64>,
    bitmaps: BTreeMap<u64, Box<[u8; BLOCK_BYTES]>>,
}

impl Pool {
    pub(super) fn open<R: Read + Seek>(
        readers: &mut [R],
        metadata: &Volume,
        data: u64,
        chunk: u64,
        tx: u64,
        budget: &mut Budget,
        proof: &mut Sha256,
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Self, Error> {
        let mut reader = Reader {
            blocks: metadata.size / BLOCK_BYTES as u64,
            readers,
            metadata,
            checkpoint,
            budget,
            proof,
            scratch: Vec::new(),
        };
        let sb = Superblock::parse(&reader.block(0)?, metadata.size, data, chunk, tx)?;
        reader.blocks = sb.metadata_blocks;
        let dev_ids = 0..1 << 24;
        let mut roots = BTreeMap::new();
        reader.walk(sb.mapping, dev_ids.clone(), 8, |id, value| {
            let root = u64le(value, 0);
            if roots.len() == 256 || !sb.address(root) {
                return Err(Error::Invalid);
            }
            roots.insert(id, root);
            Ok(())
        })?;
        let mut details = BTreeMap::new();
        reader.walk(sb.details, dev_ids, 24, |id, value| {
            if !roots.contains_key(&id) {
                return Err(Error::Invalid);
            }
            let device = Device {
                mapped: u64le(value, 0),
                transaction: u64le(value, 8),
                created: u32le(value, 16),
                snapshot: u32le(value, 20),
            };
            if device.mapped > sb.data_blocks
                || device.transaction > tx
                || device.created > sb.time
                || device.snapshot > sb.time
            {
                return Err(Error::Invalid);
            }
            details.insert(id, device);
            Ok(())
        })?;
        if details.len() != roots.len() {
            return Err(Error::Invalid);
        }
        let mut index = BTreeMap::new();
        let bitmap_count = sb.data_blocks.div_ceil(BITMAP_ENTRIES);
        let mut seen = HashSet::new();
        reader.walk(sb.allocation, 0..bitmap_count, 16, |key, value| {
            let address = u64le(value, 0);
            if !sb.address(address)
                || !seen.insert(address)
                || u64::from(u32le(value, 8)) > BITMAP_ENTRIES
                || u64::from(u32le(value, 12)) > BITMAP_ENTRIES
            {
                return Err(Error::Invalid);
            }
            index.insert(key, address);
            Ok(())
        })?;
        if index.len() as u64 != bitmap_count {
            return Err(Error::Invalid);
        }
        Ok(Self {
            sb,
            roots,
            details,
            index,
            bitmaps: BTreeMap::new(),
        })
    }

    pub(super) fn mapping<R: Read + Seek>(
        &mut self,
        readers: &mut [R],
        metadata: &Volume,
        (id, transaction): (u64, u64),
        size: u64,
        budget: &mut Budget,
        proof: &mut Sha256,
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Vec<Run>, Error> {
        let &root = self.roots.get(&id).ok_or(Error::Invalid)?;
        let device = self.details.get(&id).ok_or(Error::Invalid)?;
        if device.transaction != transaction {
            return Err(Error::Invalid);
        }
        let mut runs: Vec<Run> = Vec::new();
        let mut mapped = 0;
        let run_limit = budget.runs;
        let mut reader = Reader {
            blocks: self.sb.metadata_blocks,
            readers,
            metadata,
            checkpoint,
            budget,
            proof,
            scratch: Vec::new(),
        };
        reader.walk(root, 0..u64::MAX, 8, |logical, value| {
            let data = u64le(value, 0) >> 24;
            if data >= self.sb.data_blocks {
                return Err(Error::Invalid);
            }
            mapped += 1;
            // A reduced virtual size can leave allocated mappings beyond the
            // exposed device. They are included in the on-disk mapped count.
            if logical >= size {
                return Ok(());
            }
            if let Some(last) = runs
                .last_mut()
                .filter(|last| last.logical.end == logical && last.physical + last.len() == data)
            {
                last.logical.end += 1;
            } else {
                if runs.len() == run_limit {
                    return Err(Error::Limit);
                }
                runs.push(Run {
                    logical: logical..logical + 1,
                    physical: data,
                });
            }
            Ok(())
        })?;
        if mapped != device.mapped {
            return Err(Error::Invalid);
        }
        for run in &runs {
            reader.budget.run()?;
            let end = run.physical + run.len();
            let mut at = run.physical;
            while at < end {
                let key = at / BITMAP_ENTRIES;
                let offset = at % BITMAP_ENTRIES;
                let limit = (end - at).min(BITMAP_ENTRIES - offset);
                let bitmap = match self.bitmaps.entry(key) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let address = *self.index.get(&key).ok_or(Error::Invalid)?;
                        let bitmap = reader.block(address)?;
                        if !checksum(&bitmap, BITMAP_SALT) || u64le(&bitmap, 8) != address {
                            return Err(Error::Invalid);
                        }
                        entry.insert(Box::new(bitmap))
                    }
                };
                if (offset..offset + limit).any(|entry| {
                    let byte = bitmap[16 + (entry / 4) as usize];
                    byte & (3 << (2 * (entry % 4))) == 0
                }) {
                    return Err(Error::Invalid);
                }
                at += limit;
            }
        }
        Ok(runs)
    }
}

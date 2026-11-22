//! Inspect retained JBD2 transactions from a clean, internal Ext4 journal.
//! The log is never replayed. Its snapshots are evidence for separate recovery.

use super::crc;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;
const MAGIC: u32 = 0xc03b3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE: u32 = 5;
const FEATURE_64BIT: u32 = 2;
const FEATURE_CSUM_V3: u32 = 16;
const TAG_ESCAPE: u32 = 1;
const TAG_SAME_UUID: u32 = 2;
const TAG_LAST: u32 = 8;

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Snapshot {
    block: u32,
    escaped: bool,
}

#[derive(Debug)]
pub(super) struct Transaction {
    pub(super) snapshots: BTreeMap<u64, Snapshot>,
    pub(super) revoked: BTreeSet<u64>,
    start: u32,
    len: u32,
    pub(super) proof: [u8; 32],
}

pub(super) struct Journal {
    bytes: Vec<u8>,
    block_size: usize,
    first: u32,
    limit: u32,
    sequence: u32,
    head: Option<u32>,
    uuid: [u8; 16],
    seed: u32,
    wide: bool,
    filesystem_blocks: u64,
}

impl Journal {
    pub(super) fn new(
        bytes: Vec<u8>,
        block_size: usize,
        filesystem_blocks: u64,
        uuid: [u8; 16],
    ) -> Result<Self, String> {
        if !matches!(block_size, 1024 | 2048 | 4096)
            || bytes.len() < block_size
            || bytes.len() as u64 > MAX_JOURNAL_BYTES
            || bytes.len() % block_size != 0
        {
            return Err("Ext4 内部日志大小或块大小不支持。".into());
        }
        let superblock = &bytes[..1024];
        let limit = be32(superblock, 0x10);
        let first = be32(superblock, 0x14);
        let head = be32(superblock, 0x58);
        let incompatible = be32(superblock, 0x28);
        if be32(superblock, 0) != MAGIC
            || be32(superblock, 4) != SUPERBLOCK_V2
            || be32(superblock, 0xc) as usize != block_size
            || limit as usize != bytes.len() / block_size
            || first == 0
            || limit.checked_sub(first).is_none_or(|size| size < 3)
            || be32(superblock, 0x1c) != 0
            || be32(superblock, 0x20) != 0
            || be32(superblock, 0x24) != 0
            || incompatible & !0x13 != 0
            || incompatible & FEATURE_CSUM_V3 == 0
            || be32(superblock, 0x2c) != 0
            || superblock[0x30..0x40] != uuid
            || be32(superblock, 0x40) != 1
            || be32(superblock, 0x44) != 0
            || superblock[0x50] != 4
            || be32(superblock, 0x54) != 0
            || (head != 0 && !(first..limit).contains(&head))
            || !checksum(!0, superblock, 0xfc)
        {
            return Err("Ext4 删除恢复需要干净且校验通过的内部 JBD2 v3 日志。".into());
        }
        Ok(Self {
            block_size,
            first,
            limit,
            sequence: be32(superblock, 0x18),
            head: (head != 0).then_some(head),
            uuid,
            seed: crc(!0, &uuid),
            wide: incompatible & FEATURE_64BIT != 0,
            filesystem_blocks,
            bytes,
        })
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }

    fn block(&self, index: u32) -> &[u8] {
        let at = index as usize * self.block_size;
        &self.bytes[at..at + self.block_size]
    }

    fn advance(&self, from: u32, distance: u32) -> u32 {
        self.first + (from - self.first + distance) % (self.limit - self.first)
    }

    fn distance(&self, from: u32, to: u32) -> u32 {
        if to >= from {
            to - from
        } else {
            (self.limit - from) + (to - self.first)
        }
    }

    fn control(&self, block: &[u8]) -> Option<(u32, u32)> {
        if be32(block, 0) != MAGIC {
            return None;
        }
        let kind = be32(block, 4);
        let sequence = be32(block, 8);
        let csum = match kind {
            DESCRIPTOR | REVOKE => self.block_size - 4,
            COMMIT if block[12..16] == [0; 4] && be32(block, 56) < 1_000_000_000 => 16,
            _ => return None,
        };
        checksum(self.seed, block, csum).then_some((kind, sequence))
    }

    pub(super) fn snapshot(&self, snapshot: Snapshot) -> Vec<u8> {
        let mut bytes = self.block(snapshot.block).to_vec();
        if snapshot.escaped {
            bytes[..4].copy_from_slice(&MAGIC.to_be_bytes());
        }
        bytes
    }

    fn descriptor(
        &self,
        at: u32,
        distance_to_commit: u32,
        sequence: u32,
        snapshots: &mut BTreeMap<u64, Snapshot>,
    ) -> Option<u32> {
        let descriptor = self.block(at);
        let data_seed = crc(self.seed, &sequence.to_be_bytes());
        let mut cursor = 12;
        let mut data = 0;
        let end = descriptor.len() - 4;
        while cursor + 16 <= end {
            let target =
                u64::from(be32(descriptor, cursor)) | u64::from(be32(descriptor, cursor + 8)) << 32;
            let flags = be32(descriptor, cursor + 4);
            let expected = be32(descriptor, cursor + 12);
            cursor += 16;
            // The first tag must establish the UUID. Linux does not emit
            // obsolete DELETED tags in checksummed transactions.
            if flags & !(TAG_ESCAPE | TAG_SAME_UUID | TAG_LAST) != 0
                || (data == 0 && flags & TAG_SAME_UUID != 0)
                || target >= self.filesystem_blocks
                || (!self.wide && target > u64::from(u32::MAX))
            {
                return None;
            }
            if flags & TAG_SAME_UUID == 0 {
                // Linux can leave the tag owner UUID zeroed for an internal,
                // single-user journal. The descriptor CRC still binds it to
                // the UUID verified in the journal superblock.
                if cursor + 16 > end
                    || (descriptor[cursor..cursor + 16] != self.uuid
                        && descriptor[cursor..cursor + 16] != [0; 16])
                {
                    return None;
                }
                cursor += 16;
            }
            data += 1;
            if data >= distance_to_commit {
                return None;
            }
            let block = self.advance(at, data);
            let content = self.block(block);
            let escaped = flags & TAG_ESCAPE != 0;
            if crc(data_seed, content) != expected
                || escaped && content[..4] != [0; 4]
                || snapshots
                    .insert(target, Snapshot { block, escaped })
                    .is_some()
            {
                return None;
            }
            if flags & TAG_LAST != 0 {
                return Some(data + 1);
            }
        }
        None
    }

    fn revocations(&self, block: &[u8], revoked: &mut BTreeSet<u64>) -> Option<()> {
        let end = be32(block, 12) as usize;
        let width = if self.wide { 8 } else { 4 };
        if !(16..=block.len() - 4).contains(&end) || (end - 16) % width != 0 {
            return None;
        }
        for record in block[16..end].chunks_exact(width) {
            let target = if self.wide {
                be64(record, 0)
            } else {
                u64::from(be32(record, 0))
            };
            if target >= self.filesystem_blocks || !revoked.insert(target) {
                return None;
            }
        }
        Some(())
    }

    fn transaction(&self, first: u32, commit: u32, sequence: u32) -> Option<Transaction> {
        let end = self.distance(first, commit);
        let mut offset = 0;
        let mut snapshots = BTreeMap::new();
        let mut revoked = BTreeSet::new();
        while offset < end {
            let at = self.advance(first, offset);
            let block = self.block(at);
            let (kind, seq) = self.control(block)?;
            if seq != sequence {
                return None;
            }
            offset += match kind {
                DESCRIPTOR => self.descriptor(at, end - offset, sequence, &mut snapshots)?,
                REVOKE => {
                    self.revocations(block, &mut revoked)?;
                    1
                }
                _ => return None,
            };
        }
        if offset != end || snapshots.keys().any(|target| revoked.contains(target)) {
            return None;
        }
        let len = end + 1;
        let mut digest = Sha256::new();
        for offset in 0..len {
            digest.update(self.block(self.advance(first, offset)));
        }
        Some(Transaction {
            snapshots,
            revoked,
            start: first,
            len,
            proof: digest.finalize().into(),
        })
    }

    /// Find the newest consecutive physical chain of checksummed transactions.
    /// The oldest transaction is usable only if its start follows a validated
    /// commit or the untouched ring prefix. A partially overwritten transaction
    /// cannot prove a prior inode state.
    pub(super) fn transactions(&self, cancel: &AtomicBool) -> Result<Vec<Transaction>, String> {
        #[derive(Default)]
        struct Records {
            starts: Vec<u32>,
            commits: Vec<u32>,
        }
        let capacity = self.limit - self.first;
        let mut controls = BTreeMap::<u32, Records>::new();
        for index in self.first..self.limit {
            if cancel.load(Ordering::Relaxed) {
                return Err("Ext4 日志检查已取消。".into());
            }
            let Some((kind, sequence)) = self.control(self.block(index)) else {
                continue;
            };
            let age = self.sequence.wrapping_sub(sequence);
            if age == 0 || age > capacity {
                continue;
            }
            let records = controls.entry(age).or_default();
            match kind {
                COMMIT => records.commits.push(index),
                _ => records.starts.push(index),
            }
        }
        let mut newest_first = Vec::new();
        let mut expected_age = controls
            .iter()
            .find(|(_, r)| !r.commits.is_empty())
            .map(|(&age, _)| age);
        let mut next_start = self.head;
        let mut count = 0;
        while let Some(age) = expected_age {
            if cancel.load(Ordering::Relaxed) {
                return Err("Ext4 日志检查已取消。".into());
            }
            let Some(records) = controls.get(&age) else {
                break;
            };
            let [commit] = records.commits.as_slice() else {
                break;
            };
            if next_start.is_some_and(|next| self.advance(*commit, 1) != next) {
                break;
            }
            // The previous commit is a stronger start anchor than a descriptor
            // in the current transaction, which may be only a surviving suffix.
            let predecessor = controls
                .get(&(age + 1))
                .and_then(|r| match r.commits.as_slice() {
                    [pos] => Some(*pos),
                    _ => None,
                });
            let first = if let Some(pos) = predecessor {
                self.advance(pos, 1)
            } else if records.starts.iter().copied().min() == Some(self.first)
                && self.block(self.limit - 1).iter().all(|b| *b == 0)
            {
                self.first
            } else {
                break;
            };
            let sequence = self.sequence.wrapping_sub(age);
            let Some(transaction) = self.transaction(first, *commit, sequence) else {
                break;
            };
            count += transaction.len;
            if count > capacity {
                break;
            }
            next_start = Some(transaction.start);
            newest_first.push(transaction);
            expected_age = Some(age + 1);
        }
        newest_first.reverse();
        Ok(newest_first)
    }
}

fn checksum(seed: u32, bytes: &[u8], field: usize) -> bool {
    let checksum = crc(
        crc(crc(seed, &bytes[..field]), &[0; 4]),
        &bytes[field + 4..],
    );
    checksum == be32(bytes, field)
}

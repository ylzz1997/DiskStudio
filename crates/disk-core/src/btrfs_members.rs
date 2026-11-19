//! Validated Btrfs member identity and logical stripe routing.
use super::{le16, le32, le64, read_at, word, Key, CHUNK_ITEM, DUP};
use crate::raid_field::products as field_products;
use btrfs_disk::raw::{
    BTRFS_BLOCK_GROUP_RAID0, BTRFS_BLOCK_GROUP_RAID1, BTRFS_BLOCK_GROUP_RAID10,
    BTRFS_BLOCK_GROUP_RAID1C3, BTRFS_BLOCK_GROUP_RAID1C4, BTRFS_BLOCK_GROUP_RAID5,
    BTRFS_BLOCK_GROUP_RAID56_MASK, BTRFS_BLOCK_GROUP_RAID6,
};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Seek},
};

pub const MAX_MEMBERS: usize = 64;
const MAX_STRIPES: usize = MAX_MEMBERS;
const STRIPE_UNIT: u64 = 64 * 1024;
const RAID0: u64 = BTRFS_BLOCK_GROUP_RAID0 as u64;
const RAID1: u64 = BTRFS_BLOCK_GROUP_RAID1 as u64;
const RAID10: u64 = BTRFS_BLOCK_GROUP_RAID10 as u64;
const RAID1C3: u64 = BTRFS_BLOCK_GROUP_RAID1C3 as u64;
const RAID1C4: u64 = BTRFS_BLOCK_GROUP_RAID1C4 as u64;
const RAID5: u64 = BTRFS_BLOCK_GROUP_RAID5 as u64;
const RAID6: u64 = BTRFS_BLOCK_GROUP_RAID6 as u64;
const RAID56: u64 = BTRFS_BLOCK_GROUP_RAID56_MASK as u64;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Device {
    pub(super) id: u64,
    pub(super) uuid: [u8; 16],
    pub(super) size: u64,
    pub(super) allocated: u64,
}

impl Device {
    pub(super) fn parse(item: &[u8]) -> Result<Self, String> {
        if item.len() != 98 {
            return Err("Btrfs 设备条目长度无效。".into());
        }
        Ok(Self {
            id: le64(item, 0)?,
            size: le64(item, 8)?,
            allocated: le64(item, 16)?,
            uuid: word(item, 66)?,
        })
    }
}

pub(super) struct Members<R> {
    pub(super) primary: R,
    pub(super) primary_id: u64,
    pub(super) others: BTreeMap<u64, R>,
}

impl<R: Read + Seek> Members<R> {
    pub(super) fn read(
        &mut self,
        stripe: Stripe,
        delta: u64,
        bytes: &mut [u8],
    ) -> Result<(), String> {
        let reader = if stripe.device == self.primary_id {
            &mut self.primary
        } else {
            self.others
                .get_mut(&stripe.device)
                .ok_or("Btrfs 缺少所需成员。")?
        };
        read_at(reader, stripe.offset + delta, bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stripe {
    pub(super) device: u64,
    pub(super) uuid: [u8; 16],
    pub(super) offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Chunk {
    pub(super) length: u64,
    pub(super) kind: u64,
    pub(super) stripes: Vec<Stripe>,
    pub(super) columns: usize,
    pub(super) copies: usize,
}

pub(super) struct Run<'a> {
    pub(super) stripes: &'a [Stripe],
    pub(super) delta: u64,
    pub(super) len: usize,
    pub(super) parity: Option<ParityRun<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct ParityRun<'a> {
    stripes: &'a [Stripe],
    columns: usize,
    rotation: usize,
    target: usize,
}

impl ParityRun<'_> {
    fn stripe(self, index: usize) -> Stripe {
        self.stripes[(self.rotation + index) % self.stripes.len()]
    }

    /// Rebuild a range inside one physical stripe. The caller must validate
    /// the result using the filesystem's independent data or node checksum.
    pub(super) fn rebuild(
        self,
        reader: &mut Members<impl Read + Seek>,
        delta: u64,
        len: usize,
        prefer_q: bool,
    ) -> Option<Vec<u8>> {
        let raid6 = self.stripes.len() - self.columns == 2;
        if prefer_q && !raid6 {
            return None;
        }
        let mut stripe = vec![0; len];
        let mut p = vec![0; len];
        let mut q = if raid6 { vec![0; len] } else { Vec::new() };
        let field = raid6.then(field_products);
        let mut coefficient = 1;
        let mut target_coefficient = 0;
        let mut other_missing = None;
        for index in 0..self.columns {
            if index == self.target {
                target_coefficient = coefficient;
            } else if reader.read(self.stripe(index), delta, &mut stripe).is_ok() {
                for (sum, byte) in p.iter_mut().zip(&stripe) {
                    *sum ^= byte;
                }
                if let Some(field) = field {
                    for (sum, &byte) in q.iter_mut().zip(&stripe) {
                        *sum ^= field[coefficient][byte as usize];
                    }
                }
            } else if !raid6 || other_missing.replace(coefficient).is_some() {
                return None;
            }
            if let Some(field) = field {
                coefficient = field[2][coefficient] as usize;
            }
        }
        // With one erasure, either syndrome can independently recover it.
        // Prefer P first so a failed checksum can retry using Q.
        if !prefer_q || other_missing.is_some() {
            let read_p = reader
                .read(self.stripe(self.columns), delta, &mut stripe)
                .is_ok();
            if read_p {
                for (sum, byte) in p.iter_mut().zip(&stripe) {
                    *sum ^= byte;
                }
                if other_missing.is_none() {
                    return Some(p);
                }
            } else if other_missing.is_some() {
                return None;
            }
        }
        let field = field?;
        reader
            .read(self.stripe(self.columns + 1), delta, &mut stripe)
            .ok()?;
        // When two data columns are missing:
        // P' = x ^ y, Q' = ax ^ by, so x = (Q' ^ bP') / (a ^ b).
        let b = other_missing.unwrap_or(0);
        let divisor = target_coefficient ^ b;
        let inverse = field[divisor].iter().position(|&value| value == 1)?;
        for ((q, p), parity) in q.iter_mut().zip(p).zip(stripe) {
            *q = field[inverse][(*q ^ parity ^ field[b][p as usize]) as usize];
        }
        Some(q)
    }
}

impl Chunk {
    pub(super) fn parse(key: Key, bytes: &[u8], sector_size: u32) -> Result<Self, String> {
        let length = le64(bytes, 0)?;
        let kind = le64(bytes, 24)?;
        let count = usize::from(le16(bytes, 44)?);
        let sub = usize::from(le16(bytes, 46)?);
        let alignment = u64::from(sector_size);
        let (columns, copies) = match (kind & !7, count) {
            (0, 1) => (1, 1),
            (DUP | RAID1, 2) => (1, 2),
            (RAID1C3, 3) => (1, 3),
            (RAID1C4, 4) => (1, 4),
            (RAID0, n) if n > 0 => (n, 1),
            (RAID10, n) if n >= 2 && sub == 2 && n % sub == 0 => (n / sub, sub),
            (RAID5, n) if n >= 2 => (n - 1, 1),
            (RAID6, n) if n >= 3 => (n - 2, 1),
            _ => return Err("Btrfs 块组配置不支持或条带数量不符。".into()),
        };
        if key.object_id != 256
            || key.kind != CHUNK_ITEM
            || le64(bytes, 8)? != 2
            || le64(bytes, 16)? != STRIPE_UNIT
            || count > MAX_STRIPES
            || length == 0
            || length % alignment != 0
            || ((columns > 1 || kind & RAID56 != 0) && length % (STRIPE_UNIT * columns as u64) != 0)
            || key.offset % alignment != 0
            || key.offset.checked_add(length).is_none()
            || !matches!(kind & 7, 1 | 2 | 4 | 5)
            || le32(bytes, 40)? != sector_size
            || bytes.len() != 48 + count * 32
        {
            return Err("Btrfs 块组几何无效。".into());
        }
        let mut stripes = Vec::with_capacity(count);
        for index in 0..count {
            let start = 48 + index * 32;
            let stripe = Stripe {
                device: le64(bytes, start)?,
                offset: le64(bytes, start + 8)?,
                uuid: word(bytes, start + 16)?,
            };
            if stripe.device == 0 || stripe.uuid == [0; 16] || stripe.offset % alignment != 0 {
                return Err("Btrfs 物理条带未对齐。".into());
            }
            stripes.push(stripe);
        }
        let device_count = stripes
            .iter()
            .map(|s| s.device)
            .collect::<HashSet<_>>()
            .len();
        if device_count != if kind & DUP != 0 { 1 } else { count } {
            return Err("Btrfs 条带的设备组合与块组配置不符。".into());
        }
        Ok(Self {
            length,
            kind,
            stripes,
            columns,
            copies,
        })
    }

    pub(super) fn span(&self) -> u64 {
        self.length / self.columns as u64
    }

    pub(super) fn copies(&self) -> usize {
        self.copies
    }

    pub(super) fn run(&self, within: u64, limit: usize) -> Run<'_> {
        let parity = self.kind & RAID56 != 0;
        if self.columns == 1 && !parity {
            return Run {
                stripes: &self.stripes,
                delta: within,
                len: (self.length - within).min(limit as u64) as usize,
                parity: None,
            };
        }
        let stripe_number = within / STRIPE_UNIT;
        let columns = self.columns as u64;
        let row = stripe_number / columns;
        let target = (stripe_number % columns) as usize;
        let rotation = (row % self.stripes.len() as u64) as usize;
        // Linux orders a full row as data columns, P, then Q. The first
        // data column rotates by one physical device for each row.
        let start = if parity {
            (rotation + target) % self.stripes.len()
        } else {
            target * self.copies
        };
        let intra = within % STRIPE_UNIT;
        Run {
            stripes: &self.stripes[start..start + self.copies],
            delta: row * STRIPE_UNIT + intra,
            len: (STRIPE_UNIT - intra).min(limit as u64) as usize,
            parity: parity.then_some(ParityRun {
                stripes: &self.stripes,
                columns: self.columns,
                rotation,
                target,
            }),
        }
    }
}

pub(super) fn chunks_consistent(
    chunks: &BTreeMap<u64, Chunk>,
    devices: &BTreeMap<u64, Device>,
) -> Result<(), String> {
    check_chunk_ranges(chunks, devices, false)
}

pub(super) fn check_chunk_ranges(
    chunks: &BTreeMap<u64, Chunk>,
    devices: &BTreeMap<u64, Device>,
    allow_unopened: bool,
) -> Result<(), String> {
    let mut end = 0;
    let mut physical = Vec::new();
    for (&start, chunk) in chunks {
        if start < end {
            return Err("Btrfs 逻辑块组重叠。".into());
        }
        end = start + chunk.length;
        for stripe in &chunk.stripes {
            let end = stripe
                .offset
                .checked_add(chunk.span())
                .ok_or("Btrfs 条带范围溢出。")?;
            match devices.get(&stripe.device) {
                Some(device) if stripe.uuid == device.uuid && end <= device.size => {}
                None if allow_unopened => {}
                _ => return Err("Btrfs 条带设备标识或容量不符。".into()),
            }
            physical.push((stripe.device, stripe.offset, end));
        }
    }
    physical.sort_unstable();
    if physical
        .windows(2)
        .any(|w| w[0].0 == w[1].0 && w[0].2 > w[1].1)
    {
        return Err("Btrfs 同一成员的物理条带重叠。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::fixture::{set32, set64};
    use std::io::Cursor;

    #[test]
    fn raid6_rotates_and_rebuilds_with_any_two_missing_members() {
        let bytes = [0x83, 0x46, 0x22, 0xe7, 0x83 ^ 0x8c ^ 0x88];
        let chunk = Chunk {
            length: 6 * STRIPE_UNIT,
            kind: RAID6 | 1,
            stripes: (1..=5)
                .map(|device| Stripe {
                    device,
                    uuid: [device as u8; 16],
                    offset: 0,
                })
                .collect(),
            columns: 3,
            copies: 1,
        };
        for data in 0..3 {
            let run = chunk.run((3 + data) * STRIPE_UNIT, 1);
            assert_eq!(run.stripes[0].device, data + 2);
            for other in 0..5 {
                let mut readers = (0..5)
                    .filter(|&column| column != data as usize && column != other)
                    .map(|column| {
                        let mut storage = vec![0; STRIPE_UNIT as usize + 1];
                        storage[STRIPE_UNIT as usize] = bytes[column];
                        (((column + 1) % 5) as u64 + 1, Cursor::new(storage))
                    })
                    .collect::<BTreeMap<_, _>>();
                let (primary_id, primary) = readers.pop_first().unwrap();
                let mut members = Members {
                    primary_id,
                    primary,
                    others: readers,
                };
                assert_eq!(
                    run.parity
                        .unwrap()
                        .rebuild(&mut members, run.delta, 1, false),
                    Some(vec![bytes[data as usize]]),
                    "data column {data}, other missing column {other}",
                );
                if other != 4 {
                    assert_eq!(
                        run.parity
                            .unwrap()
                            .rebuild(&mut members, run.delta, 1, true),
                        Some(vec![bytes[data as usize]]),
                    );
                }
            }
        }
    }

    #[test]
    fn btrfs_raid10_routes_rows_and_rejects_wrong_member_geometry() {
        let key = Key {
            object_id: 256,
            kind: CHUNK_ITEM,
            offset: 8 * STRIPE_UNIT,
        };
        let mut raw = [0; 48 + 4 * 32];
        set64(&mut raw, 0, 4 * STRIPE_UNIT);
        set64(&mut raw, 8, 2);
        set64(&mut raw, 16, STRIPE_UNIT);
        set64(&mut raw, 24, RAID10 | 1);
        set32(&mut raw, 40, 4096);
        raw[44..48].copy_from_slice(&[4, 0, 2, 0]);
        let devices = (1..=4)
            .map(|id| {
                let device = Device {
                    id,
                    uuid: [id as u8; 16],
                    size: 8 * STRIPE_UNIT,
                    allocated: 2 * STRIPE_UNIT,
                };
                let stripe = 48 + (id as usize - 1) * 32;
                set64(&mut raw, stripe, id);
                set64(&mut raw, stripe + 8, 4 * STRIPE_UNIT);
                raw[stripe + 16..stripe + 32].copy_from_slice(&device.uuid);
                (device.id, device)
            })
            .collect::<BTreeMap<_, _>>();
        let chunk = Chunk::parse(key, &raw, 4096).unwrap();
        assert_eq!(chunk.span(), 2 * STRIPE_UNIT);
        let tail = chunk.run(STRIPE_UNIT - 4096, 8192);
        assert_eq!(
            (
                tail.stripes[0].device,
                tail.stripes[1].device,
                tail.delta,
                tail.len
            ),
            (1, 2, STRIPE_UNIT - 4096, 4096)
        );
        let next = chunk.run(STRIPE_UNIT, 4096);
        assert_eq!(
            (next.stripes[0].device, next.stripes[1].device, next.delta),
            (3, 4, 0)
        );
        let next_row = chunk.run(2 * STRIPE_UNIT, 4096);
        assert_eq!(
            (next_row.stripes[0].device, next_row.delta),
            (1, STRIPE_UNIT)
        );
        let mut mapping = BTreeMap::from([(key.offset, chunk)]);
        chunks_consistent(&mapping, &devices).unwrap();
        mapping.get_mut(&key.offset).unwrap().stripes[3].offset += 3 * STRIPE_UNIT;
        assert!(chunks_consistent(&mapping, &devices).is_err());
        set64(&mut raw, 48 + 3 * 32, 1);
        assert!(Chunk::parse(key, &raw, 4096).is_err());
    }
}

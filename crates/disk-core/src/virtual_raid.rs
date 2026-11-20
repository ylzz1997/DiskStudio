//! Read-only reconstruction from explicitly ordered member ranges.
pub use crate::raid10::{
    Format as Raid10Format, Geometry as Raid10Geometry, Groups as Raid10Groups,
    Layout as Raid10Layout,
};
use crate::raid_parity::Row;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, SeekFrom};

pub const MAX_MEMBERS: usize = 64;
const MAX_STRIPE: u64 = 1024 * 1024;
const MAX_IO: u64 = 64 * 1024;
const INVALID: &str = "RAID 成员、范围或条带参数无效。";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Layout {
    Jbod,
    Raid0,
    Raid0Original,
    Raid0Alternate,
    Raid1,
    Raid10,
    Raid10Md(Raid10Layout),
    Raid10MdAdvanced(Raid10Geometry),
    Raid4,
    Raid4ParityLast,
    Raid5FixedFirst,
    Raid5LeftAsymmetric,
    Raid5RightAsymmetric,
    Raid5LeftSymmetric,
    Raid5RightSymmetric,
    Raid5FixedLast,
    Raid6ZeroRestart,
    Raid6NRestart,
    Raid6NContinue,
    Raid6LeftAsymmetric,
    Raid6RightAsymmetric,
    Raid6LeftSymmetric,
    Raid6RightSymmetric,
    Raid6FixedFirst,
    Raid6FixedLast,
    Raid6LeftAsymmetricQLast,
    Raid6RightAsymmetricQLast,
    Raid6LeftSymmetricQLast,
    Raid6RightSymmetricQLast,
    Raid6PFirstQLast,
}

impl Layout {
    fn parity(self) -> Option<(u32, u32)> {
        Some(match self {
            Self::Raid4 => (5, 4),
            Self::Raid4ParityLast => (5, 5),
            Self::Raid5FixedFirst => (5, 4),
            Self::Raid5LeftAsymmetric => (5, 0),
            Self::Raid5RightAsymmetric => (5, 1),
            Self::Raid5LeftSymmetric => (5, 2),
            Self::Raid5RightSymmetric => (5, 3),
            Self::Raid5FixedLast => (5, 5),
            Self::Raid6ZeroRestart => (6, 8),
            Self::Raid6NRestart => (6, 9),
            Self::Raid6NContinue => (6, 10),
            Self::Raid6LeftAsymmetric => (6, 0),
            Self::Raid6RightAsymmetric => (6, 1),
            Self::Raid6LeftSymmetric => (6, 2),
            Self::Raid6RightSymmetric => (6, 3),
            Self::Raid6FixedFirst => (6, 4),
            Self::Raid6FixedLast => (6, 5),
            Self::Raid6LeftAsymmetricQLast => (6, 16),
            Self::Raid6RightAsymmetricQLast => (6, 17),
            Self::Raid6LeftSymmetricQLast => (6, 18),
            Self::Raid6RightSymmetricQLast => (6, 19),
            Self::Raid6PFirstQLast => (6, 20),
            _ => return None,
        })
    }

    pub(crate) fn minimum_members(self) -> usize {
        self.parity().map_or(2, |(level, _)| level as usize - 2)
    }

    pub(crate) fn zoned_raid0(self) -> bool {
        matches!(self, Self::Raid0Original | Self::Raid0Alternate)
    }

    fn raid10(self, count: usize) -> Result<Option<Raid10Geometry>, &'static str> {
        let geometry = match self {
            Self::Raid10 if count >= 4 && count % 2 == 0 => Raid10Layout::NEAR_PAIR.geometry(),
            Self::Raid10 => return Err(INVALID),
            Self::Raid10Md(layout) if layout.format == Raid10Format::Near && layout.far_sets => {
                return Err(INVALID)
            }
            Self::Raid10Md(layout) => layout.geometry(),
            Self::Raid10MdAdvanced(geometry) => geometry,
            _ => return Ok(None),
        };
        geometry
            .valid(count)
            .then_some(Some(geometry))
            .ok_or(INVALID)
    }

    pub(crate) fn readable(
        self,
        present: &[bool],
        member_chunks: u64,
    ) -> Result<bool, &'static str> {
        let count = present.len();
        if let Some(layout) = self.raid10(count)? {
            let chunks = layout.logical_chunks(member_chunks, count).ok_or(INVALID)?;
            if !layout.non_overlapping(count, member_chunks, chunks) {
                return Err("RAID10 布局的副本范围越界或指向重复的物理块。");
            }
            // The same sets of member slots repeat after at most `count` chunks.
            return Ok((0..chunks.min(count as u64)).all(|chunk| {
                (0..layout.copies()).any(|copy| present[layout.replica(count, 0, chunk, copy).0])
            }));
        }
        Ok(match self {
            Self::Raid1 => present.contains(&true),
            _ => {
                let missing = present.iter().filter(|&&member| !member).count();
                missing <= self.parity().map_or(0, |(level, _)| level as usize - 4)
            }
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Jbod => "JBOD",
            Self::Raid0 => "RAID 0",
            Self::Raid0Original => "RAID 0 · Linux md original",
            Self::Raid0Alternate => "RAID 0 · Linux md alternate",
            Self::Raid1 => "RAID 1",
            Self::Raid10 => "RAID 10 (near)",
            Self::Raid10Md(layout) => match layout.format {
                Raid10Format::Near => "RAID 10 · Linux md near",
                Raid10Format::Far => "RAID 10 · Linux md far",
                Raid10Format::Offset => "RAID 10 · Linux md offset",
            },
            Self::Raid10MdAdvanced(_) => "RAID 10 · Linux md advanced",
            Self::Raid4 => "RAID 4",
            Self::Raid4ParityLast => "RAID 4 · parity last",
            Self::Raid5FixedFirst => "RAID 5 · parity first",
            Self::Raid5LeftAsymmetric => "RAID 5 · left asymmetric",
            Self::Raid5RightAsymmetric => "RAID 5 · right asymmetric",
            Self::Raid5LeftSymmetric => "RAID 5 · left symmetric",
            Self::Raid5RightSymmetric => "RAID 5 · right symmetric",
            Self::Raid5FixedLast => "RAID 5 · parity last",
            Self::Raid6ZeroRestart => "RAID 6 · DDF zero restart",
            Self::Raid6NRestart => "RAID 6 · DDF N restart",
            Self::Raid6NContinue => "RAID 6 · DDF N continue",
            Self::Raid6LeftAsymmetric => "RAID 6 · left asymmetric",
            Self::Raid6RightAsymmetric => "RAID 6 · right asymmetric",
            Self::Raid6LeftSymmetric => "RAID 6 · left symmetric",
            Self::Raid6RightSymmetric => "RAID 6 · right symmetric",
            Self::Raid6FixedFirst => "RAID 6 · P/Q first",
            Self::Raid6FixedLast => "RAID 6 · P/Q last",
            Self::Raid6LeftAsymmetricQLast => "RAID 6 · left asymmetric, Q last",
            Self::Raid6RightAsymmetricQLast => "RAID 6 · right asymmetric, Q last",
            Self::Raid6LeftSymmetricQLast => "RAID 6 · left symmetric, Q last",
            Self::Raid6RightSymmetricQLast => "RAID 6 · right symmetric, Q last",
            Self::Raid6PFirstQLast => "RAID 6 · P first, Q last",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub layout: Layout,
    pub sector: u64,
    pub stripe: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Range {
    pub start: u64,
    pub size: u64,
}

pub struct Member<R> {
    pub reader: Option<R>,
    pub range: Range,
}

pub struct Reader<R> {
    config: Config,
    members: Vec<Member<R>>,
    len: u64,
    data_lanes: usize,
    raid10: Option<Raid10Geometry>,
    raid0_zones: Vec<StripeZone>,
    pos: u64,
    scratch: Vec<u8>,
}

impl<R: Read + Seek> Reader<R> {
    pub fn open(config: Config, mut members: Vec<Member<R>>) -> Result<Self, String> {
        let count = members.len();
        let parity = config
            .layout
            .parity()
            .map_or(0, |(level, _)| level as usize - 4);
        let striped = !matches!(config.layout, Layout::Jbod | Layout::Raid1);
        if !(config.layout.minimum_members()..=MAX_MEMBERS).contains(&count)
            || !matches!(config.sector, 512 | 4096)
            || (striped
                && (!(config.sector..=MAX_STRIPE).contains(&config.stripe)
                    || !config.stripe.is_power_of_two()))
            || (!striped && config.stripe != 0)
        {
            return Err(INVALID.into());
        }
        let first_size = members[0].range.size;
        for member in &mut members {
            let Range { start, size } = member.range;
            let end = start.checked_add(size).ok_or(INVALID)?;
            if size == 0
                || start % config.sector != 0
                || size % config.sector != 0
                || (config.layout != Layout::Jbod
                    && !config.layout.zoned_raid0()
                    && size != first_size)
                || (striped && size % config.stripe != 0)
            {
                return Err(INVALID.into());
            }
            if let Some(reader) = &mut member.reader {
                if end > reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? {
                    return Err(INVALID.into());
                }
            } else if start != 0 {
                return Err(INVALID.into());
            }
        }
        let present: Vec<_> = members
            .iter()
            .map(|member| member.reader.is_some())
            .collect();
        let raid10 = config.layout.raid10(count)?;
        let member_chunks = first_size / config.stripe.max(1);
        if !config.layout.readable(&present, member_chunks)? {
            return Err("RAID 缺少的成员超出可重建范围。".into());
        }
        let data_lanes = if config.layout == Layout::Raid1 {
            1
        } else {
            count - parity
        };
        let raid0_zones = if config.layout.zoned_raid0() {
            let sizes: Vec<_> = members.iter().map(|member| member.range.size).collect();
            StripeZone::from_sizes(&sizes, config.stripe)?
        } else {
            Vec::new()
        };
        let len = if let Some(last) = raid0_zones.last() {
            Some(last.end)
        } else if let Some(layout) = raid10 {
            layout
                .logical_chunks(member_chunks, count)
                .and_then(|chunks| chunks.checked_mul(config.stripe))
        } else if config.layout == Layout::Jbod {
            members
                .iter()
                .try_fold(0u64, |sum, member| sum.checked_add(member.range.size))
        } else {
            first_size.checked_mul(data_lanes as u64)
        }
        .filter(|size| *size <= i64::MAX as u64)
        .ok_or(INVALID)?;
        Ok(Self {
            config,
            members,
            len,
            data_lanes,
            raid10,
            raid0_zones,
            pos: 0,
            scratch: Vec::new(),
        })
    }

    /// Check up to sixteen stripes across the member address space. A pass
    /// neither proves every stripe nor identifies the original parameters.
    pub fn check_parity_samples(&mut self) -> io::Result<usize> {
        let Some((level, layout)) = self.config.layout.parity() else {
            return Ok(0);
        };
        if self.members.iter().any(|member| member.reader.is_none()) {
            return Ok(0);
        }
        let rows = self.members[0].range.size / self.config.stripe;
        let samples = rows.min(16);
        let count = self.members.len();
        for index in 0..samples {
            let row = if samples == 1 {
                0
            } else {
                index * (rows - 1) / (samples - 1)
            };
            let offset = row * self.config.stripe;
            Row::new(row, count, level, layout)
                .verify_sample(self.config.stripe.min(MAX_IO) as usize, |lane, out| {
                    read_member(&mut self.members[lane], offset, out)
                })?;
        }
        Ok(samples as usize)
    }

    fn read_chunk(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.raid0_zones.is_empty() {
            let index = self
                .raid0_zones
                .partition_point(|zone| zone.end <= self.pos);
            let zone = &self.raid0_zones[index];
            let relative = self.pos - zone.start;
            let stripe = self.config.stripe;
            let within = relative % stripe;
            let width = zone.members.len() as u64;
            // The original layout selects the slot from the full logical
            // address. The alternate layout starts again at slot zero in each zone.
            let lane_address = if self.config.layout == Layout::Raid0Original {
                self.pos
            } else {
                relative
            };
            let member = zone.members[(lane_address / stripe % width) as usize];
            let offset = zone.member_start + relative / stripe / width * stripe + within;
            let size = out.len().min((stripe - within).min(MAX_IO) as usize);
            read_member(&mut self.members[member], offset, &mut out[..size])?;
            return Ok(size);
        }
        if self.config.layout == Layout::Jbod {
            let mut pos = self.pos;
            for member in &mut self.members {
                if pos < member.range.size {
                    let size = (out.len() as u64).min(member.range.size - pos) as usize;
                    read_member(member, pos, &mut out[..size])?;
                    return Ok(size);
                }
                pos -= member.range.size;
            }
            return Err(io::Error::other(INVALID));
        }
        let count = self.members.len();
        if let Some(layout) = self.raid10 {
            let stripe = self.config.stripe;
            let chunk = self.pos / stripe;
            let within = self.pos % stripe;
            let size = out.len().min((stripe - within).min(MAX_IO) as usize);
            let far_rows = layout.section_rows(self.members[0].range.size / stripe);
            let replicas = (0..layout.copies()).map(|copy| {
                let (lane, row) = layout.replica(count, far_rows, chunk, copy);
                (lane, row * stripe + within)
            });
            self.read_replicas(replicas, &mut out[..size])?;
            return Ok(size);
        }
        let (column, row, within, limit) = if self.config.layout == Layout::Raid1 {
            (0, 0, self.pos, self.len - self.pos)
        } else {
            let stripe = self.config.stripe;
            let chunk = self.pos / stripe;
            let within = self.pos % stripe;
            (
                (chunk % self.data_lanes as u64) as usize,
                chunk / self.data_lanes as u64,
                within,
                stripe - within,
            )
        };
        let offset = row * self.config.stripe + within;
        let size = out.len().min(limit.min(MAX_IO) as usize);
        let out = &mut out[..size];
        if let Some((level, layout)) = self.config.layout.parity() {
            let parity = Row::new(row, count, level, layout);
            let target = parity.data_lane(column);
            if self.members[target].reader.is_some() {
                read_member(&mut self.members[target], offset, out)?;
            } else {
                let available: Vec<_> = self
                    .members
                    .iter()
                    .enumerate()
                    .map(|(lane, member)| member.reader.as_ref().map(|_| lane))
                    .collect();
                parity.rebuild(
                    &mut self.members,
                    |members, &lane, bytes| read_member(&mut members[lane], offset, bytes),
                    &available,
                    target,
                    out,
                    &mut self.scratch,
                )?;
            }
            return Ok(size);
        }
        let lanes = if self.config.layout == Layout::Raid1 {
            0..count
        } else {
            column..column + 1
        };
        self.read_replicas(lanes.map(|lane| (lane, offset)), out)?;
        Ok(size)
    }

    fn read_replicas(
        &mut self,
        replicas: impl Iterator<Item = (usize, u64)>,
        out: &mut [u8],
    ) -> io::Result<()> {
        let mut read = false;
        for (lane, offset) in replicas {
            let member = &mut self.members[lane];
            if member.reader.is_none() {
                continue;
            }
            if read {
                self.scratch.resize(out.len(), 0);
                read_member(member, offset, &mut self.scratch)?;
                if self.scratch != out {
                    return Err(io::Error::other("RAID 镜像副本内容不一致。"));
                }
            } else {
                read_member(member, offset, out)?;
                read = true;
            }
        }
        read.then_some(()).ok_or_else(|| io::Error::other(INVALID))
    }
}

pub(crate) struct StripeZone {
    start: u64,
    end: u64,
    member_start: u64,
    members: Vec<usize>,
}

impl StripeZone {
    pub(crate) fn from_sizes(sizes: &[u64], stripe: u64) -> Result<Vec<Self>, &'static str> {
        if stripe == 0 || sizes.iter().any(|size| *size == 0 || size % stripe != 0) {
            return Err(INVALID);
        }
        let mut boundaries = sizes.to_vec();
        boundaries.sort_unstable();
        boundaries.dedup();
        let mut zones = Vec::with_capacity(boundaries.len());
        let mut member_start = 0;
        let mut start = 0u64;
        for boundary in boundaries {
            let members: Vec<_> = sizes
                .iter()
                .enumerate()
                .filter_map(|(slot, &size)| (size >= boundary).then_some(slot))
                .collect();
            let bytes = (boundary - member_start)
                .checked_mul(members.len() as u64)
                .ok_or(INVALID)?;
            let end = start
                .checked_add(bytes)
                .filter(|&end| end <= i64::MAX as u64)
                .ok_or(INVALID)?;
            zones.push(Self {
                start,
                end,
                member_start,
                members,
            });
            member_start = boundary;
            start = end;
        }
        Ok(zones)
    }

    pub(crate) fn layouts_differ(&self, stripe: u64) -> bool {
        (self.start / stripe) % self.members.len() as u64 != 0
    }
}

fn read_member<R: Read + Seek>(
    member: &mut Member<R>,
    offset: u64,
    out: &mut [u8],
) -> io::Result<()> {
    if offset
        .checked_add(out.len() as u64)
        .is_none_or(|end| end > member.range.size)
    {
        return Err(io::Error::other(INVALID));
    }
    let reader = member
        .reader
        .as_mut()
        .ok_or_else(|| io::Error::other(INVALID))?;
    reader.seek(SeekFrom::Start(member.range.start + offset))?;
    reader.read_exact(out)
}

impl<R: Read + Seek> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.len || out.is_empty() {
            return Ok(0);
        }
        let len = self.read_chunk(out)?;
        self.pos += len as u64;
        Ok(len)
    }
}

impl<R: Read + Seek> Seek for Reader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
        };
        if !(0..=i128::from(self.len)).contains(&next) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, INVALID));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn member(data: Option<Vec<u8>>, offset: u64, size: u64) -> Member<Cursor<Vec<u8>>> {
        Member {
            reader: data.map(Cursor::new),
            range: Range {
                start: offset,
                size,
            },
        }
    }

    #[test]
    fn unequal_raid0_maps_all_zones_and_bounds_unaligned_reads() {
        let config = |layout| Config {
            layout,
            sector: 512,
            stripe: 512,
        };
        let data = |values: &[u8], prefix: usize| {
            let mut bytes = vec![0xee; prefix];
            for &value in values {
                bytes.extend_from_slice(&[value; 512]);
            }
            // Trailing data after the member range must stay out of the array.
            bytes.extend_from_slice(&[0xff; 512]);
            member(Some(bytes), prefix as u64, values.len() as u64 * 512)
        };
        let sources = || {
            vec![
                data(&[10, 11, 12, 13, 14], 512),
                data(&[20], 1024),
                data(&[30, 31, 32], 1536),
            ]
        };
        for (layout, stripes) in [
            (Layout::Raid0Original, [10, 20, 30, 31, 11, 32, 12, 13, 14]),
            (Layout::Raid0Alternate, [10, 20, 30, 11, 31, 12, 32, 13, 14]),
        ] {
            let mut reader = Reader::open(config(layout), sources()).unwrap();
            let expected = stripes
                .into_iter()
                .flat_map(|byte| [byte; 512])
                .collect::<Vec<_>>();
            let mut actual = Vec::new();
            reader.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, expected);
            let start = 3 * 512 - 13;
            let end = 7 * 512 + 7;
            reader.seek(SeekFrom::Start(start as u64)).unwrap();
            let mut across = vec![0; end - start];
            reader.read_exact(&mut across).unwrap();
            assert_eq!(across, expected[start..end]);
            assert!(reader.seek(SeekFrom::End(1)).is_err());
            let mut incomplete = sources();
            incomplete[1] = member(None, 0, 512);
            assert!(Reader::open(config(layout), incomplete).is_err());
        }
        assert!(Reader::open(config(Layout::Raid0), sources()).is_err());
    }

    #[test]
    fn raid5_fixed_parity_rebuilds_missing_data_and_rejects_invalid_samples() {
        let a = [vec![1; 512], vec![2; 512]].concat();
        let b = [vec![3; 512], vec![8; 512]].concat();
        let p = [vec![2; 512], vec![10; 512]].concat();
        let start = 512;
        let size = 1024;
        let present = |bytes: &[u8]| {
            member(
                Some([vec![0xff; 512], bytes.to_vec()].concat()),
                start,
                size,
            )
        };
        let config = Config {
            layout: Layout::Raid5FixedLast,
            sector: 512,
            stripe: 512,
        };
        let mut ready = Reader::open(config, vec![present(&a), present(&b), present(&p)]).unwrap();
        assert_eq!(ready.check_parity_samples().unwrap(), 2);
        let mut wrong = a.clone();
        wrong[512] ^= 1;
        let mut wrong =
            Reader::open(config, vec![present(&wrong), present(&b), present(&p)]).unwrap();
        assert!(wrong.check_parity_samples().is_err());
        let mut missing = Reader::open(
            config,
            vec![member(None, 0, size), present(&b), present(&p)],
        )
        .unwrap();
        let mut result = Vec::new();
        missing.seek(SeekFrom::Start(511)).unwrap();
        missing.read_to_end(&mut result).unwrap();
        let logical = [vec![1; 512], vec![3; 512], vec![2; 512], vec![8; 512]].concat();
        assert_eq!(result, logical[511..]);
        assert!(missing.seek(SeekFrom::End(1)).is_err());
    }

    #[test]
    fn array_requires_a_complete_mapping_and_rejects_disagreeing_mirror() {
        let config = Config {
            layout: Layout::Raid10,
            sector: 512,
            stripe: 512,
        };
        let absent = || member(None, 0, 512);
        let disk = |byte| member(Some(vec![byte; 512]), 0, 512);
        assert!(Reader::open(config, vec![absent(), absent(), disk(1), disk(1)]).is_err());
        let mut ready = Reader::open(config, vec![disk(1), absent(), disk(2), disk(3)]).unwrap();
        ready.read_exact(&mut [0; 512]).unwrap();
        assert!(ready.read_exact(&mut [0; 512]).is_err());
        assert_eq!(ready.stream_position().unwrap(), 512);
        assert!(Reader::open(
            config,
            vec![
                member(Some(vec![0; 511]), 0, 512),
                absent(),
                disk(1),
                disk(1)
            ]
        )
        .is_err());
    }

    #[test]
    fn grouped_offset_raid10_uses_the_last_wider_group_and_ignores_unused_rows() {
        let config = Config {
            layout: Layout::Raid10Md(Raid10Layout {
                format: Raid10Format::Offset,
                copies: 2,
                far_sets: true,
            }),
            sector: 512,
            stripe: 512,
        };
        let source = |missing: &[usize], corrupt| {
            [
                [0, 1, 2, 3, 4],
                [1, 0, 4, 2, 3],
                [5, 6, 7, 8, 9],
                [6, 5, 9, 7, 8],
                [255; 5],
            ]
            .iter()
            .fold(vec![Vec::new(); 5], |mut buffers, row| {
                for (buffer, &byte) in buffers.iter_mut().zip(row) {
                    buffer.extend_from_slice(&[byte; 512]);
                }
                buffers
            })
            .into_iter()
            .enumerate()
            .map(|(lane, mut bytes)| {
                if corrupt && lane == 1 {
                    bytes[512] ^= 1;
                }
                member((!missing.contains(&lane)).then_some(bytes), 0, 2560)
            })
            .collect()
        };
        let expected: Vec<_> = (0..10).flat_map(|value| [value; 512]).collect();
        for missing in [&[][..], &[1, 4]] {
            let mut array = Reader::open(config, source(missing, false)).unwrap();
            let mut bytes = Vec::new();
            array.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
        }
        assert!(Reader::open(config, source(&[1, 3, 4], false)).is_err());
        let mut array = Reader::open(config, source(&[], true)).unwrap();
        assert!(array.read_exact(&mut [0; 512]).is_err());
        assert_eq!(array.stream_position().unwrap(), 0);
    }
}

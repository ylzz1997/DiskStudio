//! Read-only reconstruction from explicitly ordered member ranges.
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
    Raid1,
    Raid10,
    Raid4,
    Raid5LeftAsymmetric,
    Raid5RightAsymmetric,
    Raid5LeftSymmetric,
    Raid5RightSymmetric,
    Raid5FixedLast,
    Raid6ZeroRestart,
    Raid6NRestart,
    Raid6NContinue,
}

impl Layout {
    fn parity(self) -> Option<(u32, u32)> {
        Some(match self {
            Self::Raid4 => (5, 4),
            Self::Raid5LeftAsymmetric => (5, 0),
            Self::Raid5RightAsymmetric => (5, 1),
            Self::Raid5LeftSymmetric => (5, 2),
            Self::Raid5RightSymmetric => (5, 3),
            Self::Raid5FixedLast => (5, 5),
            Self::Raid6ZeroRestart => (6, 8),
            Self::Raid6NRestart => (6, 9),
            Self::Raid6NContinue => (6, 10),
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Jbod => "JBOD",
            Self::Raid0 => "RAID 0",
            Self::Raid1 => "RAID 1",
            Self::Raid10 => "RAID 10 (near)",
            Self::Raid4 => "RAID 4",
            Self::Raid5LeftAsymmetric => "RAID 5 · left asymmetric",
            Self::Raid5RightAsymmetric => "RAID 5 · right asymmetric",
            Self::Raid5LeftSymmetric => "RAID 5 · left symmetric",
            Self::Raid5RightSymmetric => "RAID 5 · right symmetric",
            Self::Raid5FixedLast => "RAID 5 · parity last",
            Self::Raid6ZeroRestart => "RAID 6 · zero restart",
            Self::Raid6NRestart => "RAID 6 · N restart",
            Self::Raid6NContinue => "RAID 6 · N continue",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub layout: Layout,
    pub sector: u64,
    pub stripe: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
        let minimum = if parity > 0 { parity + 2 } else { 2 };
        let striped = !matches!(config.layout, Layout::Jbod | Layout::Raid1);
        if !(minimum..=MAX_MEMBERS).contains(&count)
            || !matches!(config.sector, 512 | 4096)
            || (striped
                && (!(config.sector..=MAX_STRIPE).contains(&config.stripe)
                    || !config.stripe.is_power_of_two()))
            || (!striped && config.stripe != 0)
            || (config.layout == Layout::Raid10 && (count < 4 || count % 2 != 0))
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
                || (config.layout != Layout::Jbod && size != first_size)
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
        let missing = members
            .iter()
            .filter(|member| member.reader.is_none())
            .count();
        let readable = match config.layout {
            Layout::Raid1 => missing < count,
            Layout::Raid10 => members
                .chunks_exact(2)
                .all(|pair| pair.iter().any(|member| member.reader.is_some())),
            _ => missing <= parity,
        };
        if !readable {
            return Err("RAID 缺少的成员超出可重建范围。".into());
        }
        let data_lanes = match config.layout {
            Layout::Raid1 => 1,
            Layout::Raid10 => count / 2,
            _ => count - parity,
        };
        let len = if config.layout == Layout::Jbod {
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
        let lanes = match self.config.layout {
            Layout::Raid1 => 0..count,
            Layout::Raid10 => 2 * column..2 * column + 2,
            _ => column..column + 1,
        };
        let mut replicas = self.members[lanes]
            .iter_mut()
            .filter(|member| member.reader.is_some());
        read_member(
            replicas.next().ok_or_else(|| io::Error::other(INVALID))?,
            offset,
            out,
        )?;
        for replica in replicas {
            self.scratch.resize(size, 0);
            read_member(replica, offset, &mut self.scratch)?;
            if self.scratch != out {
                return Err(io::Error::other("RAID 镜像副本内容不一致。"));
            }
        }
        Ok(size)
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
}

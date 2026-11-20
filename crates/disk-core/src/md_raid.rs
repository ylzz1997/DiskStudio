//! Native Linux md version-1 member metadata used for read-only assembly.
use crate::virtual_raid::{
    Config, Layout, Raid10Geometry, Raid10Layout, Range, StripeZone, MAX_MEMBERS,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
};

const SECTOR: u64 = 512;
const BLOCK: usize = 4096;
const MAGIC: u32 = 0xa92b4efc;
const RAID0_LAYOUT: u32 = 4096;
const INVALID: &str = "Linux md 成员元数据损坏或数据范围无效。";
const UNSUPPORTED: &str = "此 Linux md 元数据含有暂不支持的布局或特性。";
const MISMATCH: &str = "所选 Linux md 成员的阵列、元数据版本或事件代次不一致。";
const MISSING: &str = "所选 Linux md 成员不足以读取此布局。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembly {
    pub config: Config,
    /// In logical slot order, indexed into the original source list.
    pub members: Vec<(Option<usize>, Range)>,
    pub version: &'static str,
    pub array_uuid: String,
    pub events: u64,
    pub fingerprint: String,
}

#[derive(PartialEq, Eq)]
struct Common {
    version: &'static str,
    uuid: [u8; 16],
    creation: u64,
    level: i32,
    layout: u32,
    component: u64,
    chunk: u64,
    members: usize,
    features: u32,
    events: u64,
    sector: u64,
    roles: Vec<u16>,
}

struct Superblock {
    common: Common,
    device_uuid: [u8; 16],
    slot: usize,
    available: Range,
    digest: [u8; 32],
}

/// Reads only the three documented metadata positions on each already opened
/// member. They must be complete RAID member extents, such as a partition image.
pub fn inspect<R: Read + Seek>(sources: &mut [R], sector: u64) -> Result<Assembly, String> {
    if sources.is_empty() || sources.len() > MAX_MEMBERS || !matches!(sector, 512 | 4096) {
        return Err(INVALID.into());
    }
    let superblocks = sources
        .iter_mut()
        .map(probe)
        .collect::<Result<Vec<_>, _>>()?;
    assemble(&superblocks, sector).map_err(str::to_owned)
}

fn probe(reader: &mut (impl Read + Seek)) -> Result<Superblock, String> {
    let len = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let end = (len / SECTOR)
        .checked_sub(16)
        .map(|sectors| (sectors & !7) * SECTOR);
    let mut found = None;
    let mut bytes = [0; BLOCK];
    for (version, start) in [("1.1", Some(0)), ("1.2", Some(4096)), ("1.0", end)] {
        let Some(start) =
            start.filter(|&start| len.checked_sub(start).is_some_and(|n| n >= BLOCK as u64))
        else {
            continue;
        };
        if version == "1.0" && start < 8192 {
            continue;
        }
        reader
            .seek(SeekFrom::Start(start))
            .map_err(|e| e.to_string())?;
        reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        if le32(&bytes, 0) != MAGIC {
            continue;
        }
        if found.is_some() {
            return Err("一个成员包含多个 Linux md 超级块，无法确定当前阵列。".into());
        }
        found = Some(decode(&bytes, len, start, version)?);
    }
    found.ok_or_else(|| "所选范围缺少 Linux md 1.0、1.1 或 1.2 超级块。".into())
}

fn decode(
    bytes: &[u8; BLOCK],
    len: u64,
    start: u64,
    version: &'static str,
) -> Result<Superblock, &'static str> {
    let max_dev = le32(bytes, 220) as usize;
    if le32(bytes, 4) != 1
        || le64(bytes, 144) != start / SECTOR
        || le32(bytes, 12) != 0
        || bytes[228..256].iter().any(|&byte| byte != 0)
        || !(1..=(BLOCK - 256) / 2).contains(&max_dev)
    {
        return Err(INVALID);
    }
    let used = 256 + max_dev * 2;
    if checksum(&bytes[..used]) != le32(bytes, 216) {
        return Err(INVALID);
    }
    let level = le32(bytes, 72) as i32;
    let features = le32(bytes, 8);
    if (features != 0 && !(level == 0 && features == RAID0_LAYOUT))
        || (level >= 1 && le64(bytes, 208) != u64::MAX)
    {
        return Err("Linux md 成员未处于干净同步状态，或使用了未支持的元数据特性。");
    }
    let uuid = bytes[16..32].try_into().unwrap();
    let device_uuid = bytes[168..184].try_into().unwrap();
    let members = le32(bytes, 92) as usize;
    let device = le32(bytes, 160) as usize;
    if uuid == [0; 16]
        || device_uuid == [0; 16]
        || !(2..=MAX_MEMBERS).contains(&members)
        || device >= max_dev
    {
        return Err(INVALID);
    }
    let roles: Vec<_> = bytes[256..used]
        .chunks_exact(2)
        .map(|raw| u16::from_le_bytes(raw.try_into().unwrap()))
        .collect();
    let mut claimed = HashSet::new();
    for &role in &roles {
        if role < members as u16 {
            if !claimed.insert(role) {
                return Err(INVALID);
            }
        } else if !matches!(role, 0xfffe | 0xffff) {
            return Err(UNSUPPORTED);
        }
    }
    let slot = usize::from(roles[device]);
    if slot >= members {
        return Err("所选 Linux md 成员是备用盘或已标记故障。");
    }
    let data = le64(bytes, 128).checked_mul(SECTOR).ok_or(INVALID)?;
    let size = le64(bytes, 136).checked_mul(SECTOR).ok_or(INVALID)?;
    let data_end = data
        .checked_add(size)
        .filter(|&end| end <= len)
        .ok_or(INVALID)?;
    let sector = u64::from(le32(bytes, 224));
    if size < 10 * SECTOR || !matches!(sector, 0 | 512 | 4096) {
        return Err(INVALID);
    }
    if version == "1.0" {
        if data_end > start {
            return Err(INVALID);
        }
    } else if data < start + (used as u64).next_multiple_of(sector.max(SECTOR)) {
        return Err(INVALID);
    }
    Ok(Superblock {
        common: Common {
            version,
            uuid,
            creation: le64(bytes, 64),
            level,
            layout: le32(bytes, 76),
            component: le64(bytes, 80).checked_mul(SECTOR).ok_or(INVALID)?,
            chunk: u64::from(le32(bytes, 88)) * SECTOR,
            members,
            features,
            events: le64(bytes, 200),
            sector,
            roles,
        },
        device_uuid,
        slot,
        available: Range { start: data, size },
        digest: Sha256::digest(&bytes[..used]).into(),
    })
}

fn assemble(superblocks: &[Superblock], sector: u64) -> Result<Assembly, &'static str> {
    let common = &superblocks[0].common;
    let mut layout = layout(common.level, common.layout, common.features)?;
    if layout == Layout::Raid10Md(Raid10Layout::NEAR_PAIR)
        && common.members >= 4
        && common.members % 2 == 0
    {
        layout = Layout::Raid10;
    }
    let stripe = if matches!(layout, Layout::Raid1 | Layout::Jbod) {
        0
    } else {
        common.chunk
    };
    if (common.sector != 0 && common.sector != sector)
        || common.members < layout.minimum_members()
        || (stripe != 0 && (stripe < sector || stripe > 1024 * 1024 || !stripe.is_power_of_two()))
        || (stripe == 0 && !matches!(layout, Layout::Raid1 | Layout::Jbod))
        || (layout == Layout::Jbod && common.chunk != 0)
    {
        return Err(UNSUPPORTED);
    }
    let missing = Range {
        start: 0,
        size: common.component,
    };
    let mut members = vec![(None, missing); common.members];
    let mut devices = HashSet::new();
    let mut fingerprint = Sha256::new();
    fingerprint.update(sector.to_le_bytes());
    for (index, item) in superblocks.iter().enumerate() {
        if &item.common != common {
            return Err(MISMATCH);
        }
        let member = &mut members[item.slot];
        if member.0.is_some() || !devices.insert(item.device_uuid) {
            return Err("所选 Linux md 成员存在重复角色或设备编号。");
        }
        let start = item.available.start;
        let size = match layout {
            Layout::Jbod => item.available.size,
            Layout::Raid0 | Layout::Raid0Original | Layout::Raid0Alternate => {
                item.available.size / stripe * stripe
            }
            _ => common.component,
        };
        if size == 0
            || size > item.available.size
            || size % sector.max(stripe) != 0
            || start % sector != 0
        {
            return Err(INVALID);
        }
        *member = (Some(index), Range { start, size });
        fingerprint.update(item.digest);
    }
    let present: Vec<_> = members.iter().map(|(index, _)| index.is_some()).collect();
    if !layout.readable(&present, common.component / stripe.max(1))? {
        return Err(MISSING);
    }
    if common.level == 0 {
        let sizes: Vec<_> = members.iter().map(|member| member.1.size).collect();
        let zones = StripeZone::from_sizes(&sizes, stripe)?;
        if zones.len() == 1 {
            layout = Layout::Raid0;
        } else if common.features == 0 {
            if zones.iter().any(|zone| zone.layouts_differ(stripe)) {
                return Err("Linux md RAID0 缺少布局标记，两种条带映射会得到不同结果。");
            }
            layout = Layout::Raid0Original;
        }
    }
    Ok(Assembly {
        config: Config {
            layout,
            sector,
            stripe,
        },
        members,
        version: common.version,
        array_uuid: common
            .uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        events: common.events,
        fingerprint: format!("{:x}", fingerprint.finalize()),
    })
}

fn layout(level: i32, code: u32, features: u32) -> Result<Layout, &'static str> {
    use Layout::*;
    Ok(match (level, code) {
        (-1, _) => Jbod,
        (0, _) if features == 0 => Raid0,
        (0, 1) => Raid0Original,
        (0, 2) => Raid0Alternate,
        (1, _) => Raid1,
        (4, _) => Raid4ParityLast,
        (5, 0) => Raid5LeftAsymmetric,
        (5, 1) => Raid5RightAsymmetric,
        (5, 2) => Raid5LeftSymmetric,
        (5, 3) => Raid5RightSymmetric,
        (5, 4) => Raid5FixedFirst,
        (5, 5) => Raid5FixedLast,
        (6, 0) => Raid6LeftAsymmetric,
        (6, 1) => Raid6RightAsymmetric,
        (6, 2) => Raid6LeftSymmetric,
        (6, 3) => Raid6RightSymmetric,
        (6, 4) => Raid6FixedFirst,
        (6, 5) => Raid6FixedLast,
        (6, 8) => Raid6ZeroRestart,
        (6, 9) => Raid6NRestart,
        (6, 10) => Raid6NContinue,
        (6, 16) => Raid6LeftAsymmetricQLast,
        (6, 17) => Raid6RightAsymmetricQLast,
        (6, 18) => Raid6LeftSymmetricQLast,
        (6, 19) => Raid6RightSymmetricQLast,
        (6, 20) => Raid6PFirstQLast,
        (10, _) => {
            let geometry = Raid10Geometry::from_md(code).ok_or(UNSUPPORTED)?;
            geometry
                .conventional()
                .map_or(Raid10MdAdvanced(geometry), Raid10Md)
        }
        _ => return Err(UNSUPPORTED),
    })
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut words = bytes.chunks_exact(4);
    let sum: u64 = words
        .by_ref()
        .enumerate()
        .filter(|(index, _)| *index != 216 / 4)
        .map(|(_, raw)| u64::from(u32::from_le_bytes(raw.try_into().unwrap())))
        .sum();
    let sum = match words.remainder() {
        &[low, high] => sum + u64::from(u16::from_le_bytes([low, high])),
        _ => sum,
    };
    (sum as u32).wrapping_add((sum >> 32) as u32)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const LEN: usize = 128 * 1024;
    const DATA: u64 = 32 * 1024;

    fn update(bytes: &mut [u8], start: usize, edit: impl FnOnce(&mut [u8])) {
        let superblock = &mut bytes[start..start + BLOCK];
        edit(superblock);
        let size = 256 + 2 * le32(superblock, 220) as usize;
        let check = checksum(&superblock[..size]);
        superblock[216..220].copy_from_slice(&check.to_le_bytes());
    }

    fn member(version: &str, slot: u16) -> Cursor<Vec<u8>> {
        let (start, offset) = match version {
            "1.0" => (LEN - 8192, 0),
            "1.1" => (0, DATA),
            "1.2" => (4096, DATA),
            _ => unreachable!(),
        };
        let mut bytes = vec![0; LEN];
        update(&mut bytes, start, |sb| {
            for (at, value) in [
                (0, MAGIC),
                (4, 1),
                (72, 5),
                (76, 4),
                (88, 8),
                (92, 3),
                (160, u32::from(slot)),
                (220, 3),
                (224, 4096),
            ] {
                sb[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            for (at, value) in [
                (64, 37),
                (80, 32),
                (128, offset / SECTOR),
                (136, 64),
                (144, start as u64 / SECTOR),
                (200, 7),
                (208, u64::MAX),
            ] {
                sb[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            sb[16..32].fill(0xAC);
            sb[168..184].fill(slot as u8 + 1);
            sb[256..262].copy_from_slice(&[0, 0, 1, 0, 2, 0]);
        });
        Cursor::new(bytes)
    }

    #[test]
    fn reads_all_three_versions_and_places_missing_roles_in_the_array() {
        for version in ["1.0", "1.1", "1.2"] {
            let mut files = [member(version, 2), member(version, 0)];
            let plan = inspect(&mut files, 4096).unwrap();
            assert_eq!(plan.version, version);
            assert_eq!(
                plan.config,
                Config {
                    layout: Layout::Raid5FixedFirst,
                    sector: 4096,
                    stripe: 4096
                }
            );
            let range = Range {
                start: if version == "1.0" { 0 } else { DATA },
                size: 16 * 1024,
            };
            assert_eq!(
                plan.members,
                vec![
                    (Some(1), range),
                    (None, Range { start: 0, ..range }),
                    (Some(0), range)
                ]
            );
            assert_eq!(
                plan.fingerprint,
                inspect(&mut files, 4096).unwrap().fingerprint
            );
            files.reverse();
            assert_ne!(
                plan.fingerprint,
                inspect(&mut files, 4096).unwrap().fingerprint
            );
            assert!(inspect(&mut files, 512).is_err());
        }
    }

    #[test]
    fn zoned_raid0_uses_the_layout_flag_and_rejects_unmarked_ambiguity() {
        let sources = |chunks: [u64; 3], code: Option<u32>| {
            [2, 0, 1].map(|slot| {
                let mut source = member("1.2", slot);
                update(source.get_mut(), 4096, |sb| {
                    sb[72..76].copy_from_slice(&0u32.to_le_bytes());
                    let features = if code.is_some() { RAID0_LAYOUT } else { 0 };
                    sb[8..12].copy_from_slice(&features.to_le_bytes());
                    sb[76..80].copy_from_slice(&code.unwrap_or(0).to_le_bytes());
                    sb[136..144].copy_from_slice(&(chunks[slot as usize] * 8).to_le_bytes());
                });
                source
            })
        };
        for (code, layout) in [(1, Layout::Raid0Original), (2, Layout::Raid0Alternate)] {
            let mut files = sources([7, 3, 5], Some(code));
            let plan = inspect(&mut files, 4096).unwrap();
            assert_eq!(plan.config.layout, layout);
            assert_eq!(
                plan.members
                    .iter()
                    .map(|member| member.0)
                    .collect::<Vec<_>>(),
                [Some(1), Some(2), Some(0)]
            );
            assert_eq!(
                plan.members
                    .iter()
                    .map(|member| member.1.size / 4096)
                    .collect::<Vec<_>>(),
                [7, 3, 5]
            );
            assert!(inspect(&mut files[..2], 4096).is_err());
        }
        assert!(inspect(&mut sources([7, 3, 5], None), 4096).is_err());
        assert!(inspect(&mut sources([7, 3, 5], Some(3)), 4096).is_err());
        assert_eq!(
            inspect(&mut sources([5, 2, 4], None), 4096)
                .unwrap()
                .config
                .layout,
            Layout::Raid0Original
        );
        assert_eq!(
            inspect(&mut sources([5; 3], Some(2)), 4096)
                .unwrap()
                .config
                .layout,
            Layout::Raid0
        );
    }

    #[test]
    fn rejects_stale_dirty_corrupt_and_ambiguous_superblocks() {
        let raid = || [member("1.2", 1), member("1.2", 0)];
        assert!(inspect(&mut [member("1.2", 0)], 4096).is_err());
        assert!(inspect(&mut [member("1.2", 0), member("1.2", 0)], 4096).is_err());
        assert!(inspect(&mut [member("1.2", 0), member("1.0", 1)], 4096).is_err());
        assert!(inspect(&mut [Cursor::new(vec![0; 1024])], 512).is_err());
        for (at, value) in [
            (8, 4),
            (128, 8),
            (136, u32::MAX),
            (144, 0),
            (200, 8),
            (208, 0),
            (258, 0),
        ] {
            let mut files = raid();
            update(files[0].get_mut(), 4096, |sb| {
                sb[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
            });
            assert!(
                inspect(&mut files, 4096).is_err(),
                "accepted change of field at byte {at}"
            );
        }
        let mut corrupt = raid();
        corrupt[0].get_mut()[4096 + 216] ^= 1;
        assert!(inspect(&mut corrupt, 4096).is_err());
        let mut ambiguous = raid();
        let end = member("1.0", 1);
        ambiguous[0].get_mut()[LEN - 8192..].copy_from_slice(&end.get_ref()[LEN - 8192..]);
        assert!(inspect(&mut ambiguous, 4096).is_err());
    }
}

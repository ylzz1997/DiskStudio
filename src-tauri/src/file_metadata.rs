//! Metadata from already-read, validated filesystem records. Serializable fields
//! are display-only; fresh internal values can be used by the output adapter.
//! No paths, devices, clock, timezone database, or writes. Unknown stays unknown.
use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime, Time};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Stamp {
    pub text: String,
    pub zone: String,
    pub precision: String,
    /// Only fresh filesystem parsing populates this. Imported scan snapshots
    /// carry display text, never authority to set output timestamps.
    #[serde(skip)]
    pub(crate) instant_ns: Option<i128>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub source: Option<String>,
    pub created: Option<Stamp>,
    pub modified: Option<Stamp>,
    pub accessed: Option<Stamp>,
    pub changed: Option<Stamp>,
    pub attributes: Vec<String>,
    pub raw_flags: Option<String>,
    pub permissions: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub links: Option<u32>,
    #[serde(skip)]
    pub(crate) windows_flags: Option<u32>,
    #[serde(skip)]
    pub(crate) directory_proof: Option<crate::directory_metadata::Proof>,
}
impl Metadata {
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.created.is_none()
            && self.modified.is_none()
            && self.accessed.is_none()
            && self.changed.is_none()
            && self.attributes.is_empty()
            && self.raw_flags.is_none()
            && self.permissions.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
            && self.links.is_none()
    }
    /// Saved values are historical display text, never paths or write attributes.
    pub fn validate_snapshot(&self) -> Result<(), String> {
        let short = |s: &str, n: usize| s.len() <= n && !s.chars().any(char::is_control);
        if self.source.as_ref().is_some_and(|s| !short(s, 80))
            || self.attributes.len() > 32
            || self.attributes.iter().any(|s| !short(s, 80))
            || [&self.raw_flags, &self.permissions]
                .iter()
                .any(|s| s.as_ref().is_some_and(|s| !short(s, 128)))
            || [&self.created, &self.modified, &self.accessed, &self.changed]
                .iter()
                .any(|s| {
                    s.as_ref().is_some_and(|s| {
                        !short(&s.text, 80) || !short(&s.zone, 80) || !short(&s.precision, 32)
                    })
                })
        {
            return Err("保存的文件元数据过长或含控制字符".into());
        }
        Ok(())
    }
}
fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn display(date: Date, clock: Option<Time>, zone: String, precision: &'static str) -> Stamp {
    let offset = if zone == "UTC" {
        Some(0)
    } else {
        zone.strip_prefix("UTC").and_then(|s| {
            let sign = match s.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = s.get(1..)?.split_once(':')?;
            let h = hours.parse::<i32>().ok()?;
            let m = minutes.parse::<i32>().ok()?;
            (h < 24 && m < 60).then_some(sign * (h * 3600 + m * 60))
        })
    };
    let instant_ns = clock.zip(offset).and_then(|(t, o)| {
        time::UtcOffset::from_whole_seconds(o).ok().map(|offset| {
            date.with_time(t)
                .assume_offset(offset)
                .unix_timestamp_nanos()
        })
    });
    let mut text = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    );
    if let Some(t) = clock {
        text.push_str(&format!(
            " {:02}:{:02}:{:02}",
            t.hour(),
            t.minute(),
            t.second()
        ));
        if t.nanosecond() > 0 {
            text.push_str(format!(".{:09}", t.nanosecond()).trim_end_matches('0'));
        }
    }
    Stamp {
        text,
        zone,
        precision: precision.into(),
        instant_ns,
    }
}
fn unix(seconds: i64, nanos: u32, precision: &'static str) -> Option<Stamp> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    let value = OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .replace_nanosecond(nanos)
        .ok()?;
    if !(1..=9999).contains(&value.year()) {
        return None;
    }
    Some(display(
        value.date(),
        Some(value.time()),
        "UTC".into(),
        precision,
    ))
}
fn nt_time(raw: u64) -> Option<Stamp> {
    if raw == 0 {
        return None;
    }
    unix(
        (raw / 10_000_000) as i64 - 11_644_473_600,
        ((raw % 10_000_000) * 100) as u32,
        "100 ns",
    )
}
fn dos_date(value: u16) -> Option<Date> {
    Date::from_calendar_date(
        1980 + i32::from(value >> 9),
        Month::try_from(((value >> 5) & 15) as u8).ok()?,
        (value & 31) as u8,
    )
    .ok()
}
fn dos_time(date: u16, clock: u16, increment: Option<u8>, offset: Option<u8>) -> Option<Stamp> {
    let increment = increment.map(u32::from);
    if increment.is_some_and(|i| i > 199) {
        return None;
    }
    let sec = (clock & 31) * 2;
    let t = Time::from_hms_nano(
        (clock >> 11) as u8,
        ((clock >> 5) & 63) as u8,
        (sec + increment.unwrap_or(0) as u16 / 100) as u8,
        increment.unwrap_or(0) % 100 * 10_000_000,
    )
    .ok()?;
    // A missing/invalid UTC offset is not the host's current timezone.
    let zone = if let Some(raw) = offset.filter(|o| o & 0x80 != 0) {
        let q = if raw & 0x40 != 0 {
            i16::from(raw & 127) - 128
        } else {
            i16::from(raw & 127)
        };
        let minutes = q * 15;
        if minutes == 0 {
            "UTC".into()
        } else {
            format!(
                "UTC{}{:02}:{:02}",
                if minutes < 0 { "-" } else { "+" },
                minutes.abs() / 60,
                minutes.abs() % 60
            )
        }
    } else {
        "本地时间（时区未知）".into()
    };
    Some(display(
        dos_date(date)?,
        Some(t),
        zone,
        if increment.is_some() { "10 ms" } else { "2 s" },
    ))
}
fn flags(raw: u32, list: &[(u32, &'static str)]) -> Vec<String> {
    list.iter()
        .filter_map(|(bit, label)| (raw & bit != 0).then(|| (*label).into()))
        .collect()
}
const DOS_FLAGS: &[(u32, &str)] = &[
    (1, "只读"),
    (2, "隐藏"),
    (4, "系统"),
    (16, "目录"),
    (32, "归档"),
];
const NT_FLAGS: &[(u32, &str)] = &[
    (1, "只读"),
    (2, "隐藏"),
    (4, "系统"),
    (16, "目录"),
    (32, "归档"),
    (0x80, "普通"),
    (0x100, "临时"),
    (0x200, "稀疏"),
    (0x400, "重解析点"),
    (0x800, "压缩"),
    (0x1000, "脱机"),
    (0x2000, "不索引内容"),
    (0x4000, "加密"),
];

pub fn ntfs(file: &ntfs::NtfsFile<'_>) -> Metadata {
    let mut value = None;
    for (index, attr) in file.attributes_raw().enumerate() {
        if index >= 4096 {
            return Metadata::default();
        }
        let Ok(attr) = attr else {
            return Metadata::default();
        };
        if attr.ty().ok() != Some(ntfs::NtfsAttributeType::StandardInformation) {
            continue;
        }
        if value.is_some() {
            return Metadata::default();
        }
        let b = attr.record_bytes();
        if b.len() < 24 || b[8] != 0 || b[9] != 0 {
            return Metadata::default();
        }
        let at = usize::from(u16le(b, 20));
        let len = u32le(b, 16) as usize;
        if at < 24 || len < 48 || at.checked_add(len).is_none_or(|end| end > b.len()) {
            return Metadata::default();
        }
        value = Some(ntfs_value(&b[at..at + len]));
    }
    let mut result = value.unwrap_or_default();
    result.links = Some(file.hard_link_count().into());
    result
}
fn ntfs_value(b: &[u8]) -> Metadata {
    if b.len() < 48 {
        return Metadata::default();
    }
    let raw = u32le(b, 32);
    Metadata {
        source: Some("NTFS 标准信息".into()),
        created: nt_time(u64le(b, 0)),
        modified: nt_time(u64le(b, 8)),
        changed: nt_time(u64le(b, 16)),
        accessed: nt_time(u64le(b, 24)),
        attributes: flags(raw, NT_FLAGS),
        raw_flags: Some(format!("0x{raw:08X}")),
        windows_flags: Some(raw),
        ..Metadata::default()
    }
}
pub fn fat(b: &[u8], exfat: bool) -> Metadata {
    if b.len() < 32 {
        return Metadata::default();
    }
    let raw = if exfat {
        u16le(b, 4) as u32
    } else {
        b[11] as u32
    };
    let mut result = Metadata {
        source: Some(if exfat {
            "exFAT 目录项".into()
        } else {
            "FAT 目录项".into()
        }),
        attributes: flags(raw, DOS_FLAGS),
        raw_flags: Some(format!("0x{raw:04X}")),
        windows_flags: Some(raw),
        ..Metadata::default()
    };
    if exfat {
        result.created = dos_time(u16le(b, 10), u16le(b, 8), Some(b[20]), Some(b[22]));
        result.modified = dos_time(u16le(b, 14), u16le(b, 12), Some(b[21]), Some(b[23]));
        result.accessed = dos_time(u16le(b, 18), u16le(b, 16), None, Some(b[24]));
    } else {
        result.created = dos_time(u16le(b, 16), u16le(b, 14), Some(b[13]), None);
        result.modified = dos_time(u16le(b, 24), u16le(b, 22), None, None);
        result.accessed =
            dos_date(u16le(b, 18)).map(|d| display(d, None, "本地时间（时区未知）".into(), "1 天"));
    }
    result
}
fn ext_stamp(b: &[u8], base: usize, extra: usize, end: usize) -> Option<Stamp> {
    if base + 4 > end {
        return None;
    }
    let low = i64::from(u32le(b, base) as i32);
    if extra + 4 <= end {
        let raw = u32le(b, extra);
        unix(low + i64::from(raw & 3) * (1i64 << 32), raw >> 2, "1 ns")
    } else {
        unix(low, 0, "1 s")
    }
}
pub(crate) fn btrfs(b: &[u8; 160]) -> Metadata {
    let stamp = |at| {
        let seconds = u64le(b, at) as i64;
        let nanos = u32le(b, at + 8);
        (seconds != 0 || nanos != 0)
            .then(|| unix(seconds, nanos, "1 ns"))
            .flatten()
    };
    Metadata {
        source: Some("Btrfs inode".into()),
        created: stamp(148),
        modified: stamp(136),
        accessed: stamp(112),
        changed: stamp(124),
        raw_flags: Some(format!("0x{:016X}", u64le(b, 64))),
        permissions: Some(format!("{:04o}", u32le(b, 52) & 0xfff)),
        uid: Some(u32le(b, 44)),
        gid: Some(u32le(b, 48)),
        links: Some(u32le(b, 40)),
        ..Metadata::default()
    }
}

pub fn ext(b: &[u8], linux_ids: bool) -> Metadata {
    if b.len() < 128 {
        return Metadata::default();
    }
    let raw = u32le(b, 32);
    let mut result = Metadata {
        source: Some("Ext inode".into()),
        attributes: flags(
            raw,
            &[
                (4, "压缩标志"),
                (8, "同步写入"),
                (16, "不可变"),
                (32, "仅追加"),
                (64, "不备份"),
                (128, "不更新访问时间"),
                (0x800, "加密"),
                (0x80000, "Extent 存储"),
                (0x10000000, "内联数据"),
            ],
        ),
        raw_flags: Some(format!("0x{raw:08X}")),
        permissions: Some(format!("{:04o}", u16le(b, 0) & 0xfff)),
        links: Some(u16le(b, 26).into()),
        ..Metadata::default()
    };
    if linux_ids {
        result.uid = Some(u32::from(u16le(b, 2)) | u32::from(u16le(b, 120)) << 16);
        result.gid = Some(u32::from(u16le(b, 24)) | u32::from(u16le(b, 122)) << 16);
    }
    // EA_INODE repurposes timestamps; do not display reference counts as dates.
    if raw & 0x200000 != 0 {
        return result;
    }
    let end = if b.len() >= 130 {
        let extra = usize::from(u16le(b, 128));
        if extra == 0 {
            128
        } else if extra < 4 || extra % 4 != 0 || 128 + extra > b.len() {
            return result;
        } else {
            128 + extra
        }
    } else {
        128
    };
    result.accessed = ext_stamp(b, 8, 140, end);
    result.changed = ext_stamp(b, 12, 132, end);
    result.modified = ext_stamp(b, 16, 136, end);
    result.created = ext_stamp(b, 144, 148, end);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn date(y: u16, m: u16, d: u16) -> u16 {
        ((y - 1980) << 9) | (m << 5) | d
    }
    fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    #[test]
    fn metadata_ntfs_epoch_precision_bounds_and_flags() {
        let epoch = 116444736000000000;
        assert_eq!(nt_time(epoch).unwrap().text, "1970-01-01 00:00:00");
        assert_eq!(
            nt_time(epoch - 1).unwrap().text,
            "1969-12-31 23:59:59.9999999"
        );
        let t = nt_time(epoch + 1).unwrap();
        assert_eq!(t.text, "1970-01-01 00:00:00.0000001");
        assert_eq!(t.precision, "100 ns");
        assert_eq!(t.zone, "UTC");
        assert!(nt_time(0).is_none() && nt_time(u64::MAX).is_none());
        let mut b = [0; 48];
        b[..8].copy_from_slice(&epoch.to_le_bytes());
        put32(&mut b, 32, 0x80004003);
        let m = ntfs_value(&b);
        assert_eq!(m.attributes, ["只读", "隐藏", "加密"]);
        assert_eq!(m.raw_flags.as_deref(), Some("0x80004003"));
        assert!(m.modified.is_none());
    }
    #[test]
    fn metadata_fat_validates_calendar_and_distinguishes_date_precision() {
        let mut b = [0; 32];
        b[11] = 0x27;
        b[13] = 199;
        put16(&mut b, 14, (23 << 11) | (59 << 5) | 29);
        put16(&mut b, 16, date(2000, 2, 29));
        put16(&mut b, 18, date(2000, 2, 29));
        put16(&mut b, 22, 29);
        put16(&mut b, 24, date(2107, 12, 31));
        let m = fat(&b, false);
        let created = m.created.unwrap();
        assert_eq!(created.text, "2000-02-29 23:59:59.99");
        assert_eq!(created.precision, "10 ms");
        assert!(created.zone.contains("未知"));
        let accessed = m.accessed.unwrap();
        assert_eq!(accessed.text, "2000-02-29");
        assert_eq!(accessed.precision, "1 天");
        assert_eq!(m.modified.unwrap().text, "2107-12-31 00:00:58");
        assert_eq!(m.attributes, ["只读", "隐藏", "系统", "归档"]);
        put16(&mut b, 16, date(2100, 2, 29));
        assert!(fat(&b, false).created.is_none());
        put16(&mut b, 16, date(2000, 2, 29));
        b[13] = 200;
        assert!(fat(&b, false).created.is_none());
        assert!(dos_time(date(2000, 1, 1), 30, None, None).is_none());
        assert!(dos_date(0).is_none());
    }
    #[test]
    fn metadata_exfat_preserves_signed_offsets_and_unknown_zone() {
        let mut b = [0; 32];
        b[20] = 123;
        for at in [10, 14, 18] {
            put16(&mut b, at, date(2024, 2, 29));
        }
        for (raw, zone) in [
            (0x80, "UTC"),
            (0x97, "UTC+05:45"),
            (0xf2, "UTC-03:30"),
            (0xc0, "UTC-16:00"),
            (0xbf, "UTC+15:45"),
            (0x17, "本地时间（时区未知）"),
        ] {
            b[22] = raw;
            let m = fat(&b, true);
            let t = m.created.unwrap();
            assert_eq!(t.zone, zone);
            assert_eq!(t.text, "2024-02-29 00:00:01.23");
            assert_eq!(m.accessed.unwrap().precision, "2 s");
        }
        b[21] = 255;
        assert!(fat(&b, true).modified.is_none());
    }
    #[test]
    fn metadata_ext_legacy_and_extra_epoch_fields_are_bounded() {
        let mut b = [0; 256];
        put16(&mut b, 0, 0o100640);
        put32(&mut b, 16, u32::MAX);
        put16(&mut b, 2, 123);
        put16(&mut b, 120, 2);
        put16(&mut b, 24, 456);
        put16(&mut b, 122, 3);
        put16(&mut b, 26, 2);
        put32(&mut b, 32, 0x30);
        b[132..152].fill(255); // Undefined padding must not become timestamps.
        let m = ext(&b, true);
        assert_eq!(m.modified.unwrap().text, "1969-12-31 23:59:59");
        assert!(m.created.is_none());
        assert_eq!(m.uid, Some(131195));
        assert_eq!(m.gid, Some(197064));
        assert_eq!(m.permissions.as_deref(), Some("0640"));
        assert_eq!(m.links, Some(2));
        assert_eq!(m.attributes, ["不可变", "仅追加"]);
        assert!(ext(&b, false).uid.is_none());
        b[132..152].fill(0);
        put16(&mut b, 128, 24);
        put32(&mut b, 136, (123456789 << 2) | 1);
        let m = ext(&b, true);
        assert_eq!(m.modified.unwrap().text, "2106-02-07 06:28:15.123456789");
        assert_eq!(m.created.unwrap().text, "1970-01-01 00:00:00");
        put32(&mut b, 136, 1_000_000_000u32 << 2);
        assert!(ext(&b, true).modified.is_none());
        put16(&mut b, 128, 20);
        let m = ext(&b, true);
        assert_eq!(m.created.unwrap().precision, "1 s");
        for extra in [2, 6, 2000] {
            put16(&mut b, 128, extra);
            let m = ext(&b, true);
            assert!(m.modified.is_none() && m.created.is_none());
        }
        put16(&mut b, 128, 0);
        put32(&mut b, 32, 0x200000);
        assert!(ext(&b, true).modified.is_none());
    }
    #[test]
    fn metadata_truncated_records_stay_unknown_without_panics() {
        for len in 0..256 {
            let b = vec![0xff; len];
            let _ = fat(&b, false);
            let _ = fat(&b, true);
            let _ = ntfs_value(&b);
            let _ = ext(&b, true);
            if len < 32 {
                assert!(fat(&b, true).source.is_none());
            }
            if len < 48 {
                assert!(ntfs_value(&b).source.is_none());
            }
            if len < 128 {
                assert!(ext(&b, true).source.is_none());
            }
        }
    }
}

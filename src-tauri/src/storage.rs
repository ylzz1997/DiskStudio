//! Read-only Windows storage metadata. No device handles, sectors or WMI methods.
//! This module deliberately exposes a single, argument-free snapshot operation.
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
#[cfg(windows)]
const DISK_QUERY: &str = "SELECT Number,SerialNumber,UniqueId,FriendlyName,Size,LogicalSectorSize,PartitionStyle,BusType,HealthStatus,IsBoot,IsSystem,IsOffline,IsReadOnly,NumberOfPartitions FROM MSFT_Disk";
#[cfg(windows)]
const PARTITION_QUERY: &str = "SELECT __PATH,DiskNumber,PartitionNumber,DriveLetter,Offset,Size,Guid,GptType,MbrType,IsSystem,IsBoot,IsHidden FROM MSFT_Partition";
#[cfg(windows)]
const VOLUME_QUERY: &str = "SELECT __PATH,DriveLetter,FileSystemLabel,FileSystem,Size,SizeRemaining,HealthStatus FROM MSFT_Volume";
#[cfg(windows)]
const ASSOCIATION_QUERY: &str = "SELECT Partition,Volume FROM MSFT_PartitionToVolume";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDisk {
    number: u32,
    serial_number: Option<String>,
    unique_id: Option<String>,
    friendly_name: Option<String>,
    size: u64,
    logical_sector_size: Option<u64>,
    partition_style: Option<u16>,
    bus_type: Option<u16>,
    health_status: Option<u16>,
    is_boot: Option<bool>,
    is_system: Option<bool>,
    is_offline: Option<bool>,
    is_read_only: Option<bool>,
    number_of_partitions: Option<u32>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawPartition {
    #[serde(rename = "__PATH")]
    object_path: Option<String>,
    disk_number: u32,
    partition_number: u32,
    drive_letter: Option<String>,
    offset: u64,
    size: u64,
    guid: Option<String>,
    gpt_type: Option<String>,
    mbr_type: Option<u16>,
    is_system: Option<bool>,
    is_boot: Option<bool>,
    is_hidden: Option<bool>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawVolume {
    #[serde(rename = "__PATH")]
    object_path: Option<String>,
    drive_letter: Option<String>,
    file_system_label: Option<String>,
    file_system: Option<String>,
    size: Option<u64>,
    size_remaining: Option<u64>,
    health_status: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawAssociation {
    partition: String,
    volume: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub disks: Vec<LocalDisk>,
    pub observed_at: u64,
    pub warnings: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDisk {
    pub id: String,
    pub name: String,
    pub filename: String,
    pub size: u64,
    pub sector_size: u64,
    pub scheme: String,
    pub kind: &'static str,
    pub partitions: Vec<LocalPartition>,
    pub hardware: Hardware,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hardware {
    pub identity: Option<String>,
    pub layout_key: Option<String>,
    pub number: u32,
    pub model: String,
    pub bus: &'static str,
    pub health: &'static str,
    pub is_system: bool,
    pub offline: Option<bool>,
    pub device_read_only: Option<bool>,
    pub layout_available: bool,
    pub layout_note: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPartition {
    #[serde(flatten)]
    pub base: disk_core::Partition,
    pub number: u32,
    pub drive_letter: Option<String>,
    pub role: &'static str,
    pub platform: Option<&'static str>,
    pub volume_size: Option<u64>,
    pub volume_label: Option<String>,
    pub available: Option<u64>,
    pub volume_status: &'static str,
    pub hidden: bool,
}

#[cfg(windows)]
pub fn read_snapshot() -> Result<Snapshot, String> {
    // The WMI connection stays on this worker thread; the UI cannot supply queries.
    let con = wmi::WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
        .map_err(|e| format!("无法读取 Windows 存储信息（{e}）。未请求管理员权限。"))?;
    let disks: Vec<RawDisk> = con
        .raw_query(DISK_QUERY)
        .map_err(|e| format!("无法读取本机磁盘列表（{e}）。请稍后重新读取。"))?;
    let mut warnings = Vec::new();
    let partitions: Option<Vec<RawPartition>> = match con.raw_query(PARTITION_QUERY) {
        Ok(items) => Some(items),
        Err(_) => {
            warnings.push("Windows 未提供分区布局；磁盘基本信息仍可查看。".into());
            None
        }
    };
    let volumes: Vec<RawVolume> = match con.raw_query(VOLUME_QUERY) {
        Ok(items) => items,
        Err(_) => {
            warnings.push("Windows 未提供卷容量信息，文件系统和占用显示为未知。".into());
            Vec::new()
        }
    };
    let associations = match con.raw_query::<RawAssociation>(ASSOCIATION_QUERY) {
        Ok(rows) if rows.len() <= 32768 => Some(rows),
        _ => {
            warnings.push(
                "Windows 未提供完整的分区与卷关联；无盘符分区的文件系统和容量保持未知。".into(),
            );
            None
        }
    };
    normalize_associated(disks, partitions, volumes, warnings, associations)
}
#[cfg(not(windows))]
pub fn read_snapshot() -> Result<Snapshot, String> {
    Err("本机磁盘信息当前仅支持 Windows 桌面版。".into())
}

fn letter(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .filter(|s| s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic())
        .map(str::to_ascii_uppercase)
}
fn clean(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect::<String>()
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
fn role(p: &RawPartition) -> &'static str {
    let guid = p
        .gpt_type
        .as_deref()
        .unwrap_or("")
        .trim_matches(['{', '}'])
        .to_ascii_lowercase();
    match guid.as_str() {
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" => "EFI 系统",
        "e3c9e316-0b5c-4db8-817d-f92df00215ae" => "MSR 保留",
        "de94bba4-06d1-4d40-a16a-bfd50179d6ac" => "恢复分区",
        "0fc63daf-8483-4772-8e79-3d69d8477de4" => "Linux 数据",
        "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f" => "Linux 交换",
        "af9b60a0-1431-4f62-bc68-3311714a69ad" => "动态磁盘数据",
        "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3" => "动态磁盘元数据",
        _ if p.is_boot == Some(true) => "Windows 系统",
        _ if p.is_system == Some(true) => "系统分区",
        _ if p.mbr_type == Some(0x27) => "恢复分区",
        _ => "数据分区",
    }
}
// Only Windows' current OS partition gets an OS badge. IsSystem also covers
// boot-loader partitions, and a disk-level flag must never label all its volumes.
// Partition types alone cannot prove an installed Windows or Linux system.
fn platform(p: &RawPartition) -> Option<&'static str> {
    if p.is_boot != Some(true) {
        return None;
    }
    let guid = p
        .gpt_type
        .as_deref()
        .unwrap_or("")
        .trim_matches(['{', '}'])
        .to_ascii_lowercase();
    match guid.as_str() {
        "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7" => Some("windows"),
        "" => match p.mbr_type {
            Some(0x01 | 0x04 | 0x06 | 0x07 | 0x0b | 0x0c | 0x0e) => Some("windows"),
            _ => None,
        },
        _ => None,
    }
}
fn bus(value: Option<u16>) -> &'static str {
    match value {
        Some(1) => "SCSI",
        Some(3) => "ATA",
        Some(7) => "USB",
        Some(8) => "RAID",
        Some(9) => "iSCSI",
        Some(10) => "SAS",
        Some(11) => "SATA",
        Some(12) => "SD",
        Some(13) => "MMC",
        Some(14 | 15) => "虚拟磁盘",
        Some(16) => "存储空间",
        Some(17) => "NVMe",
        _ => "未知接口",
    }
}
#[cfg(test)]
fn normalize(
    disks: Vec<RawDisk>,
    partitions: Option<Vec<RawPartition>>,
    volumes: Vec<RawVolume>,
    warnings: Vec<String>,
) -> Result<Snapshot, String> {
    normalize_associated(disks, partitions, volumes, warnings, None)
}

fn same_reference(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    // Local association references use \\.\ while __PATH uses the local
    // computer name. Only that server alias differs; namespace, class and
    // the entire provider key must still match. Never strip arbitrary hosts.
    fn split(s: &str) -> Option<(&str, &str)> {
        s.strip_prefix(r"\\").and_then(|s| s.split_once('\\'))
    }
    match (split(a), split(b)) {
        (Some((ah, ap)), Some((bh, bp))) => {
            (ah == "." || bh == ".")
                && !ah.is_empty()
                && !bh.is_empty()
                && ap.eq_ignore_ascii_case(bp)
        }
        _ => false,
    }
}

// Metadata only. Association references come from fixed SELECTs, and are never
// turned into device paths or method targets. Missing/ambiguous/conflicting
// associations must not fall back to a coincidentally matching drive letter.
fn partition_volume<'a>(
    p: &RawPartition,
    parts: &[RawPartition],
    volumes: &'a [RawVolume],
    associations: Option<&[RawAssociation]>,
) -> Option<&'a RawVolume> {
    if let Some(links) = associations {
        let path = p.object_path.as_deref()?;
        if parts
            .iter()
            .filter(|q| {
                q.object_path
                    .as_deref()
                    .is_some_and(|s| same_reference(s, path))
            })
            .count()
            != 1
        {
            return None;
        }
        let forward = links
            .iter()
            .filter(|a| same_reference(&a.partition, path))
            .collect::<Vec<_>>();
        if forward.len() != 1 {
            return None;
        }
        let volume_path = &forward[0].volume;
        if links
            .iter()
            .filter(|a| same_reference(&a.volume, volume_path))
            .count()
            != 1
        {
            return None;
        }
        let found = volumes
            .iter()
            .filter(|v| {
                v.object_path
                    .as_deref()
                    .is_some_and(|s| same_reference(s, volume_path))
            })
            .collect::<Vec<_>>();
        if found.len() != 1 || letter(&p.drive_letter) != letter(&found[0].drive_letter) {
            return None;
        }
        return Some(found[0]);
    }
    // A failed association query can preserve the previous unique-letter
    // display. A letterless volume is never matched by absence of a letter.
    let drive_letter = letter(&p.drive_letter)?;
    if parts
        .iter()
        .filter(|q| letter(&q.drive_letter).as_deref() == Some(&drive_letter))
        .count()
        != 1
    {
        return None;
    }
    let found = volumes
        .iter()
        .filter(|v| letter(&v.drive_letter).as_deref() == Some(&drive_letter))
        .collect::<Vec<_>>();
    (found.len() == 1).then(|| found[0])
}

fn normalize_associated(
    mut disks: Vec<RawDisk>,
    partitions: Option<Vec<RawPartition>>,
    volumes: Vec<RawVolume>,
    mut warnings: Vec<String>,
    associations: Option<Vec<RawAssociation>>,
) -> Result<Snapshot, String> {
    if disks.len() > 256 || partitions.as_ref().is_some_and(|p| p.len() > 4096) {
        return Err("Windows 返回的存储对象数量超出显示范围。".into());
    }
    disks.sort_by_key(|d| d.number);
    let mut output = Vec::new();
    for d in disks {
        if d.size == 0 || d.size > MAX_SAFE_INTEGER {
            warnings.push(format!("磁盘 {} 的容量不可用，暂未显示。", d.number));
            continue;
        }
        let mut raw: Vec<_> = partitions
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|p| p.disk_number == d.number)
            .collect();
        raw.sort_by_key(|p| p.offset);
        let mut end = 0;
        let mut numbers = std::collections::HashSet::new();
        let valid = raw.iter().all(|p| {
            let next = p.offset.checked_add(p.size);
            let ok = p.size > 0
                && p.offset >= end
                && next.is_some_and(|n| n <= d.size)
                && numbers.insert(p.partition_number);
            end = next.unwrap_or(u64::MAX);
            ok
        });
        let count_matches = d
            .number_of_partitions
            .is_none_or(|n| n as usize == raw.len());
        let layout_available = partitions.is_some() && valid && count_matches;
        let layout_note = if !layout_available {
            Some(
                "分区信息暂不完整，可能是设备状态发生变化。请重新读取；这里不会推测空闲空间。"
                    .into(),
            )
        } else {
            None
        };
        let mut normalized = Vec::new();
        if layout_available {
            for (index, p) in raw.into_iter().enumerate() {
                let drive_letter = letter(&p.drive_letter);
                let volume = partition_volume(
                    p,
                    partitions.as_deref().unwrap_or(&[]),
                    &volumes,
                    associations.as_deref(),
                );
                let space = volume
                    .and_then(|v| v.size.zip(v.size_remaining))
                    .filter(|(size, remaining)| *size > 0 && *size <= p.size && remaining <= size);
                let role = role(p);
                let name = volume
                    .and_then(|v| clean(&v.file_system_label))
                    .unwrap_or_else(|| {
                        if drive_letter.is_some() {
                            "本地磁盘".into()
                        } else {
                            role.into()
                        }
                    });
                let fs = volume
                    .and_then(|v| clean(&v.file_system))
                    .unwrap_or_else(|| "未知".into());
                let color = if p.is_system == Some(true) {
                    "blue"
                } else if role == "MSR 保留" {
                    "mint"
                } else {
                    ["coral", "violet", "coral", "violet"][index % 4]
                };
                normalized.push(LocalPartition {
                    base: disk_core::Partition {
                        id: format!("local-{}-partition-{}", d.number, p.partition_number),
                        name,
                        fs,
                        start: p.offset,
                        size: p.size,
                        used: space.map(|(size, free)| size - free),
                        color: color.into(),
                        system: p.is_system == Some(true)
                            || p.is_boot == Some(true)
                            || role == "MSR 保留",
                    },
                    number: p.partition_number,
                    drive_letter,
                    role,
                    platform: platform(p),
                    volume_size: space.map(|(size, _)| size),
                    volume_label: volume.and_then(|v| v.file_system_label.clone()),
                    available: space.map(|(_, remaining)| remaining),
                    volume_status: match volume.and_then(|v| v.health_status) {
                        Some(0) => "正常",
                        Some(1) => "需检查",
                        Some(2 | 3) => "需修复",
                        _ => "未提供",
                    },
                    hidden: p.is_hidden.unwrap_or(false),
                });
            }
        }
        output.push(LocalDisk {
            id: format!("local-{}", d.number),
            name: format!("磁盘 {}", d.number),
            filename: String::new(),
            size: d.size,
            sector_size: d.logical_sector_size.unwrap_or(0),
            scheme: match d.partition_style {
                Some(1) => "MBR",
                Some(2) => "GPT",
                Some(0) => "RAW",
                _ => "未知",
            }
            .into(),
            kind: "physical",
            partitions: normalized,
            hardware: Hardware {
                identity: crate::disk_identity::key(
                    d.serial_number.as_deref(),
                    d.unique_id.as_deref(),
                    d.friendly_name.as_deref(),
                    d.size,
                    d.logical_sector_size,
                    d.bus_type,
                ),
                layout_key: if layout_available {
                    partitions.as_ref().map(|items| {
                        crate::disk_identity::layout_key(
                            d.partition_style,
                            items
                                .iter()
                                .filter(|p| p.disk_number == d.number)
                                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                                .collect(),
                        )
                    })
                } else {
                    None
                },
                number: d.number,
                model: clean(&d.friendly_name).unwrap_or_else(|| "未提供型号".into()),
                bus: bus(d.bus_type),
                health: match d.health_status {
                    Some(0) => "正常",
                    Some(1) => "警告",
                    Some(2) => "异常",
                    _ => "未知",
                },
                is_system: d.is_system.unwrap_or(false) || d.is_boot.unwrap_or(false),
                offline: d.is_offline,
                device_read_only: d.is_read_only,
                layout_available,
                layout_note,
            },
        });
    }
    Ok(Snapshot {
        disks: output,
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: u64 = 1024 * 1024;
    #[test]
    fn local_wmi_reference_alias_preserves_namespace_class_and_key() {
        assert!(same_reference(
            r"\\.\ROOT\Microsoft\Windows\Storage:MSFT_Partition.ObjectId=one",
            r"\\LAB\root\Microsoft\Windows\Storage:MSFT_Partition.ObjectId=one"
        ));
        for different in [
            r"\\LAB\root\Other:MSFT_Partition.ObjectId=one",
            r"\\LAB\root\Microsoft\Windows\Storage:MSFT_Volume.ObjectId=one",
            r"\\LAB\root\Microsoft\Windows\Storage:MSFT_Partition.ObjectId=two",
            r"\\OTHER\root\Microsoft\Windows\Storage:MSFT_Partition.ObjectId=one",
        ] {
            assert!(!same_reference(
                r"\\LAB\root\Microsoft\Windows\Storage:MSFT_Partition.ObjectId=one",
                different
            ));
        }
    }
    #[test]
    fn letterless_volume_requires_unique_bidirectional_association() {
        let mut p = partition();
        p.object_path = Some("provider:partition-1".into());
        let v = RawVolume {
            object_path: Some("provider:volume-1".into()),
            file_system: Some("FAT32".into()),
            file_system_label: Some("DATA".into()),
            size: Some(89 * M),
            size_remaining: Some(40 * M),
            ..Default::default()
        };
        let link = RawAssociation {
            partition: "PROVIDER:PARTITION-1".into(),
            volume: "PROVIDER:VOLUME-1".into(),
        };
        assert!(partition_volume(&p, &[partition()], &[RawVolume::default()], None).is_none());
        let s = normalize_associated(
            vec![disk()],
            Some(vec![p]),
            vec![v],
            vec![],
            Some(vec![link]),
        )
        .unwrap();
        let p = &s.disks[0].partitions[0];
        assert_eq!(p.base.fs, "FAT32");
        assert_eq!(p.base.name, "DATA");
        assert_eq!(p.available, Some(40 * M));
        assert_eq!(p.drive_letter, None);
    }
    #[test]
    fn ambiguous_or_changed_associations_never_fall_back_to_letters() {
        let p = RawPartition {
            object_path: Some("p1".into()),
            drive_letter: Some("T".into()),
            ..partition()
        };
        let v = RawVolume {
            object_path: Some("v1".into()),
            drive_letter: Some("T".into()),
            file_system: Some("NTFS".into()),
            ..Default::default()
        };
        let link = RawAssociation {
            partition: "p1".into(),
            volume: "v1".into(),
        };
        for links in [
            vec![],
            vec![link.clone(), link.clone()],
            vec![
                link.clone(),
                RawAssociation {
                    partition: "p2".into(),
                    volume: "v1".into(),
                },
            ],
            vec![RawAssociation {
                partition: "p1".into(),
                volume: "missing".into(),
            }],
        ] {
            assert!(partition_volume(
                &p,
                std::slice::from_ref(&p),
                std::slice::from_ref(&v),
                Some(&links)
            )
            .is_none());
        }
        let mismatch = RawVolume {
            drive_letter: Some("U".into()),
            ..v
        };
        assert!(
            partition_volume(&p, std::slice::from_ref(&p), &[mismatch], Some(&[link])).is_none()
        );
    }
    fn disk() -> RawDisk {
        RawDisk {
            number: 2,
            size: 100 * M,
            ..Default::default()
        }
    }
    fn partition() -> RawPartition {
        RawPartition {
            disk_number: 2,
            partition_number: 1,
            offset: M,
            size: 90 * M,
            ..Default::default()
        }
    }
    #[test]
    fn unknown_filesystem_and_nul_letter_are_never_inferred() {
        let p = RawPartition {
            drive_letter: Some("\0".into()),
            gpt_type: Some("{0fc63daf-8483-4772-8e79-3d69d8477de4}".into()),
            ..partition()
        };
        let snapshot = normalize(vec![disk()], Some(vec![p]), vec![], vec![]).unwrap();
        let p = &snapshot.disks[0].partitions[0];
        assert_eq!(p.role, "Linux 数据");
        assert_eq!(p.platform, None);
        assert_eq!(p.base.fs, "未知");
        assert_eq!(p.base.used, None);
        assert_eq!(p.drive_letter, None);
        assert_eq!(snapshot.disks[0].kind, "physical");
        assert_eq!(snapshot.disks[0].partitions.len(), 1); // No invented free extents.
    }
    #[test]
    fn os_badge_does_not_spread_to_data_or_utility_partitions() {
        // All partitions are on the same system disk; only the running OS
        // partition qualifies, even when a data volume is labelled "Windows".
        let kinds = [
            (
                "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
                Some(true),
                false,
                Some("windows"),
            ),
            (
                "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
                Some(false),
                false,
                None,
            ),
            ("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7", None, false, None),
            (
                "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
                Some(false),
                true,
                None,
            ),
            (
                "e3c9e316-0b5c-4db8-817d-f92df00215ae",
                Some(false),
                false,
                None,
            ),
            (
                "de94bba4-06d1-4d40-a16a-bfd50179d6ac",
                Some(false),
                false,
                None,
            ),
            (
                "0fc63daf-8483-4772-8e79-3d69d8477de4",
                Some(false),
                false,
                None,
            ),
            (
                "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f",
                Some(false),
                false,
                None,
            ),
        ];
        let parts = kinds
            .iter()
            .enumerate()
            .map(|(index, (kind, boot, system, _))| RawPartition {
                partition_number: index as u32 + 1,
                offset: (index as u64 + 1) * M,
                size: M,
                drive_letter: (index == 1).then(|| "D".into()),
                gpt_type: Some((*kind).into()),
                is_boot: *boot,
                is_system: Some(*system),
                ..partition()
            })
            .collect();
        let s = normalize(
            vec![RawDisk {
                is_boot: Some(true),
                is_system: Some(true),
                ..disk()
            }],
            Some(parts),
            vec![RawVolume {
                drive_letter: Some("D".into()),
                file_system_label: Some("Windows".into()),
                file_system: Some("NTFS".into()),
                ..Default::default()
            }],
            vec![],
        )
        .unwrap();
        assert!(s.disks[0].hardware.is_system);
        assert_eq!(s.disks[0].partitions.len(), kinds.len());
        for (p, (_, _, _, expected)) in s.disks[0].partitions.iter().zip(kinds) {
            assert_eq!(p.platform, expected, "partition {}", p.number);
        }
    }
    #[test]
    fn usage_comes_from_volume_size_not_partition_size() {
        let p = RawPartition {
            drive_letter: Some("T".into()),
            ..partition()
        };
        let v = RawVolume {
            drive_letter: Some("T".into()),
            size: Some(90 * M - 4096),
            size_remaining: Some(30 * M),
            file_system: Some("NTFS".into()),
            ..Default::default()
        };
        let s = normalize(vec![disk()], Some(vec![p]), vec![v], vec![]).unwrap();
        let p = &s.disks[0].partitions[0];
        assert_eq!(p.available, Some(30 * M));
        assert_eq!(p.base.used, Some(60 * M - 4096));
    }
    #[test]
    fn partial_or_conflicting_layouts_are_not_rendered_as_complete() {
        let d = RawDisk {
            number_of_partitions: Some(2),
            ..disk()
        };
        let s = normalize(vec![d], Some(vec![partition()]), vec![], vec![]).unwrap();
        assert!(!s.disks[0].hardware.layout_available);
        assert!(s.disks[0].partitions.is_empty());
        let bad = RawPartition {
            partition_number: 2,
            offset: 2 * M,
            ..partition()
        };
        let s = normalize(vec![disk()], Some(vec![partition(), bad]), vec![], vec![]).unwrap();
        assert!(!s.disks[0].hardware.layout_available);
    }
    #[test]
    fn failed_partition_query_preserves_disk_metadata_only() {
        let s = normalize(vec![disk()], None, vec![], vec![]).unwrap();
        assert_eq!(s.disks.len(), 1);
        assert!(!s.disks[0].hardware.layout_available);
        assert!(s.disks[0].partitions.is_empty());
    }
    #[test]
    fn inconsistent_volume_usage_stays_unknown() {
        let p = RawPartition {
            drive_letter: Some("T".into()),
            ..partition()
        };
        let v = RawVolume {
            drive_letter: Some("T".into()),
            size: Some(90 * M),
            size_remaining: Some(100 * M),
            ..Default::default()
        };
        let s = normalize(vec![disk()], Some(vec![p]), vec![v], vec![]).unwrap();
        assert_eq!(s.disks[0].partitions[0].base.used, None);
    }
}

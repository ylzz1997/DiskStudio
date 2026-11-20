//! Pure validation; unit tests never connect to storage or open device handles.
use super::{Action, Extent, Target};
pub use crate::disk_identity::clean;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;
pub(crate) const MIN_RESIZE_SIZE: u64 = 64 * MIB;
pub const BASIC_DATA: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
pub const LINUX_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
pub const LINUX_SWAP: &str = "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LegacyIdentity {
    pub index: u32,
    pub serial_number: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "MSFT_Disk", rename_all = "PascalCase")]
pub struct Disk {
    #[serde(rename = "__PATH")]
    pub path: String,
    pub number: u32,
    pub serial_number: Option<String>,
    pub friendly_name: Option<String>,
    pub unique_id: Option<String>,
    pub size: u64,
    pub logical_sector_size: Option<u64>,
    pub bus_type: Option<u16>,
    pub partition_style: Option<u16>,
    pub number_of_partitions: Option<u32>,
    pub is_boot: Option<bool>,
    pub is_system: Option<bool>,
    pub is_offline: Option<bool>,
    pub is_read_only: Option<bool>,
    pub is_clustered: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "MSFT_Partition", rename_all = "PascalCase")]
pub struct Partition {
    #[serde(rename = "__PATH")]
    pub path: String,
    pub disk_number: u32,
    pub partition_number: u32,
    pub offset: u64,
    pub size: u64,
    pub guid: Option<String>,
    pub gpt_type: Option<String>,
    pub drive_letter: Option<String>,
    pub is_boot: Option<bool>,
    pub is_system: Option<bool>,
    pub is_read_only: Option<bool>,
    pub is_offline: Option<bool>,
    pub is_hidden: Option<bool>,
    pub is_shadow_copy: Option<bool>,
}
pub fn guid(s: &str) -> String {
    clean(s).trim_matches(['{', '}']).to_ascii_lowercase()
}
pub fn identity(d: &Disk) -> Option<Target> {
    crate::disk_identity::key(
        d.serial_number.as_deref(),
        d.unique_id.as_deref(),
        d.friendly_name.as_deref(),
        d.size,
        d.logical_sector_size,
        d.bus_type,
    )
    .map(Target)
}
pub fn inventory(disks: &[Disk]) -> Result<(), String> {
    if disks.len() > 256
        || disks.iter().map(|d| d.number).collect::<HashSet<_>>().len() != disks.len()
    {
        return Err("Windows 返回的磁盘数量或编号异常，请刷新信息。".into());
    }
    Ok(())
}
pub fn target_disk<'a>(disks: &'a [Disk], target: &Target) -> Result<&'a Disk, String> {
    let matching: Vec<_> = disks
        .iter()
        .filter(|d| identity(d).as_ref() == Some(target))
        .collect();
    if matching.len() != 1 {
        return Err("磁盘身份已变化、缺失或重复，请刷新后重新选择。".into());
    }
    let d = matching[0];
    let serial = d.serial_number.as_deref().map(clean);
    if disks
        .iter()
        .filter(|other| other.serial_number.as_deref().map(clean) == serial)
        .count()
        != 1
    {
        return Err("磁盘序列号重复，无法唯一确认目标。".into());
    }
    Ok(d)
}
pub fn manageable(d: &Disk) -> Result<(), String> {
    if d.is_read_only == Some(true) || d.is_offline == Some(true) {
        return Err("磁盘处于只读或脱机状态，当前不可操作。".into());
    }
    if d.size < 66 * MIB
        || d.size > (1u64 << 53) - 1
        || !matches!(d.logical_sector_size, Some(512 | 4096))
        || d.is_boot.is_none()
        || d.is_system.is_none()
        || d.is_offline != Some(false)
        || d.is_read_only != Some(false)
        || d.is_clustered != Some(false)
        || d.path.is_empty()
    {
        return Err("磁盘状态不完整或类型暂不支持，当前不可操作。".into());
    }
    if !matches!(d.partition_style, Some(0 | 2)) {
        return Err("当前支持 GPT 数据磁盘及空白 RAW 磁盘；MBR 管理尚未开放。".into());
    }
    Ok(())
}
pub fn corroborate(d: &Disk, legacy: &[LegacyIdentity]) -> Result<(), String> {
    let matches: Vec<_> = legacy.iter().filter(|l| l.index == d.number).collect();
    let serial = d
        .serial_number
        .as_deref()
        .map(clean)
        .filter(|s| !s.is_empty());
    if matches.len() != 1
        || serial.is_none()
        || matches[0].serial_number.as_deref().map(clean) != serial
        || legacy
            .iter()
            .filter(|l| l.serial_number.as_deref().map(clean) == serial)
            .count()
            != 1
    {
        return Err("两套 Windows 存储信息无法唯一对应此磁盘，当前不可操作。".into());
    }
    Ok(())
}
pub fn layout<'a>(disk: &Disk, partitions: &'a [Partition]) -> Result<Vec<&'a Partition>, String> {
    let mut parts: Vec<_> = partitions
        .iter()
        .filter(|p| p.disk_number == disk.number)
        .collect();
    parts.sort_by_key(|p| p.offset);
    if disk.number_of_partitions != Some(parts.len() as u32) || parts.len() > 128 {
        return Err("分区列表不完整，请刷新后重试。".into());
    }
    let mut end = 0;
    let mut numbers = HashSet::new();
    let mut ids = HashSet::new();
    let mut guids = HashSet::new();
    for p in &parts {
        if p.size == 0
            || p.offset < end
            || p.offset.checked_add(p.size).is_none_or(|e| e > disk.size)
            || !numbers.insert(p.partition_number)
            || p.path.is_empty()
            || !ids.insert(p.path.as_str())
        {
            return Err("分区布局冲突或越界，已拒绝操作。".into());
        }
        end = p.offset + p.size;
        if disk.partition_style == Some(2)
            && p.guid
                .as_deref()
                .map(guid)
                .is_none_or(|g| g.is_empty() || !guids.insert(g))
        {
            return Err("GPT 分区标识缺失或重复，已拒绝操作。".into());
        }
    }
    if disk.partition_style == Some(0) && !parts.is_empty() {
        return Err("RAW 磁盘的分区状态不一致。".into());
    }
    Ok(parts)
}
fn writable_partition(p: &Partition) -> bool {
    p.is_boot.is_some()
        && p.is_system == Some(false)
        && p.is_read_only == Some(false)
        && p.is_offline == Some(false)
        && p.is_hidden == Some(false)
        && p.is_shadow_copy == Some(false)
        && p.guid.as_deref().is_some_and(|s| !clean(s).is_empty())
}
pub fn linux(p: &Partition) -> bool {
    matches!(
        p.gpt_type.as_deref().map(guid).as_deref(),
        Some(LINUX_DATA | LINUX_SWAP)
    )
}
// A Linux GPT type is only eligibility for explicit inspection, not proof of an Ext filesystem.
pub fn ext_candidate(d: &Disk, all: &[&Partition], p: &Partition) -> bool {
    linux_data_candidate(d, all, p) && online_linux_partition(p)
}

// Eligibility only. The clone must corroborate disk offline state and GPT
// protection metadata on the opened device before using the partition.
pub(crate) fn linux_clone_candidate(d: &Disk, all: &[&Partition], p: &Partition) -> bool {
    d.partition_style == Some(2)
        && linux_data_candidate(d, all, p)
        && match d.is_offline {
            Some(true) => true,
            Some(false) => online_linux_partition(p),
            None => false,
        }
}

fn linux_data_candidate(d: &Disk, all: &[&Partition], p: &Partition) -> bool {
    d.is_boot == Some(false)
        && d.is_system == Some(false)
        && all
            .iter()
            .all(|p| p.is_boot == Some(false) && p.is_system == Some(false))
        && p.gpt_type.as_deref().map(guid).as_deref() == Some(LINUX_DATA)
        && unprotected_linux_partition(p)
        && p.drive_letter
            .as_deref()
            .is_none_or(|s| clean(s).is_empty())
        && p.offset % MIB == 0
        && p.size % MIB == 0
}
pub fn resizable(p: &Partition) -> bool {
    writable_partition(p) && p.gpt_type.as_deref().map(guid).as_deref() == Some(BASIC_DATA)
}
pub fn formattable(p: &Partition) -> bool {
    resizable(p) && p.is_boot == Some(false)
}
pub fn sector_writable(d: &Disk, parts: &[&Partition], selected: &Partition) -> Result<(), String> {
    manageable(d)?;
    if d.partition_style != Some(2)
        || d.is_boot != Some(false)
        || d.is_system != Some(false)
        || parts
            .iter()
            .any(|p| p.is_boot != Some(false) || p.is_system != Some(false))
        || !formattable(selected)
        || !parts
            .iter()
            .any(|p| p.partition_number == selected.partition_number && p.guid == selected.guid)
    {
        return Err("扇区写入需要可独占的非系统 GPT 磁盘，并选择无保护属性的普通 Windows 数据分区；整盘、Linux、系统及未知类型暂不支持写回。".into());
    }
    Ok(())
}
pub fn deletable(p: &Partition) -> bool {
    if linux(p) {
        return unprotected_linux_partition(p) && online_linux_partition(p);
    }
    formattable(p)
}

fn online_linux_partition(p: &Partition) -> bool {
    p.is_hidden.is_some() && p.is_offline == Some(false)
}

fn unprotected_linux_partition(p: &Partition) -> bool {
    // Windows can return NULL for the Windows-volume-only flags on a Linux
    // GPT entry. Any explicit protection flag still rejects the operation.
    p.is_boot == Some(false)
        && p.is_system == Some(false)
        && p.is_read_only != Some(true)
        && p.is_shadow_copy != Some(true)
        && p.guid.as_deref().is_some_and(|s| !clean(s).is_empty())
}
pub fn free_ranges(disk: &Disk, parts: &[&Partition]) -> Vec<Extent> {
    if disk.partition_style != Some(2) || disk.size > (1u64 << 53) - 1 {
        return vec![];
    }
    // Reserve at least 1 MiB on both ends for GPT, align inward. Never derive
    // free ranges from incomplete metadata.
    let mut result = vec![];
    let mut start = MIB;
    for p in parts {
        let stop = p.offset / MIB * MIB;
        if stop > start {
            result.push(Extent {
                offset: start,
                size: stop - start,
            });
        }
        start = (p.offset + p.size).div_ceil(MIB) * MIB;
    }
    let stop = disk.size.saturating_sub(MIB) / MIB * MIB;
    if stop > start {
        result.push(Extent {
            offset: start,
            size: stop - start,
        });
    }
    result
}
pub fn validate(disk: &Disk, partitions: &[Partition], action: &Action) -> Result<(), String> {
    manageable(disk)?;
    let parts = layout(disk, partitions)?;
    if matches!(action, Action::Initialize {}) {
        return if disk.partition_style == Some(0)
            && parts.is_empty()
            && disk.is_boot == Some(false)
            && disk.is_system == Some(false)
        {
            Ok(())
        } else {
            Err("只允许初始化尚无分区的 RAW 磁盘。".into())
        };
    }
    if disk.partition_style != Some(2) {
        return Err("请先将空白磁盘初始化为 GPT。".into());
    }
    if let Action::Create { offset, size, .. } = action {
        if *size < MIB
            || size % MIB != 0
            || offset % MIB != 0
            || !free_ranges(disk, &parts).iter().any(|e| {
                *offset >= e.offset
                    && offset
                        .checked_add(*size)
                        .is_some_and(|end| end <= e.offset + e.size)
            })
        {
            return Err("新分区须至少 1 MiB、按 MiB 对齐，并完全位于同一段未分配空间中。".into());
        }
    }
    if let Some(number) = action.partition() {
        let p = parts
            .iter()
            .find(|p| p.partition_number == number)
            .ok_or("分区已发生变化，请刷新后重试。")?;
        let allowed = match action {
            Action::Resize { .. } => resizable(p),
            Action::FatResize { .. } => sector_writable(disk, &parts, p).is_ok(),
            Action::Format { .. } | Action::SetLabel { .. } | Action::SetDriveLetter { .. } => {
                formattable(p)
            }
            Action::Delete { .. } => deletable(p),
            Action::ExtFormat { .. }
            | Action::ExtResize { .. }
            | Action::BtrfsFormat { .. }
            | Action::BtrfsResize { .. } => ext_candidate(disk, &parts, p),
            _ => false,
        };
        if !allowed {
            return Err("此分区不支持所选操作；运行中的 Windows 卷不能格式化或删除，EFI、保留、隐藏及状态未知的分区受保护。".into());
        }
        if let Action::BtrfsFormat { label, .. } = action {
            if label.len() > 255
                || label
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\'))
            {
                return Err(
                    "Btrfs 卷标最多 255 个 UTF-8 字节，不能包含斜杠、反斜杠或控制字符。".into(),
                );
            }
            if p.size < 128 * MIB || p.offset % 4096 != 0 || p.size % 4096 != 0 {
                return Err("Btrfs 格式化要求分区至少 128 MiB，起点和容量按 4 KiB 对齐。".into());
            }
        }
        if let Action::Resize { size, .. }
        | Action::ExtResize { size, .. }
        | Action::BtrfsResize { size, .. }
        | Action::FatResize { size, .. } = action
        {
            let minimum = match action {
                Action::FatResize { .. } => MIB,
                Action::BtrfsResize { .. } => 128 * MIB,
                _ => MIN_RESIZE_SIZE,
            };
            if *size < minimum
                || size % MIB != 0
                || *size == p.size
                || p.offset
                    .checked_add(*size)
                    .is_none_or(|end| end > disk.size - MIB)
                || parts
                    .iter()
                    .any(|other| other.offset > p.offset && other.offset < p.offset + size)
            {
                return Err("目标容量无效或会跨越相邻分区，请重新选择。".into());
            }
        }
    }
    if let Action::Format {
        partition,
        label,
        file_system,
        cluster_size,
        options,
    } = action
    {
        let p = parts
            .iter()
            .find(|p| p.partition_number == *partition)
            .ok_or("分区不存在")?;
        options.validate(
            *file_system,
            p.size,
            disk.logical_sector_size.unwrap_or(0),
            label,
            *cluster_size,
        )?;
    }
    if let Action::ExtFormat {
        label,
        block_size,
        file_system,
        partition,
    } = action
    {
        if label.trim().is_empty() || label.len() > 16 || label.chars().any(char::is_control) {
            return Err("Ext 卷标须为 1–16 个 UTF-8 字节，不能包含控制字符。".into());
        }
        if ![1024, 2048, 4096].contains(block_size)
            || u64::from(*block_size) < disk.logical_sector_size.unwrap_or(u64::MAX)
        {
            return Err("Ext 块大小须为 1/2/4 KiB，且不能小于磁盘逻辑扇区。".into());
        }
        let p = parts
            .iter()
            .find(|p| p.partition_number == *partition)
            .ok_or("分区不存在")?;
        if p.size < 64 * MIB {
            return Err("Ext 格式化至少需要 64 MiB。".into());
        }
        if *file_system != super::ExtFileSystem::Ext4
            && p.size / u64::from(*block_size) > u64::from(u32::MAX)
        {
            return Err("分区容量超过 Ext2/3 的 32 位块计数范围。".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btrfs_clone_accepts_a_verified_offline_linux_data_partition() {
        let mut disk = disks()[1].clone();
        disk.partition_style = Some(2);
        let mut partition = part();
        partition.gpt_type = Some(LINUX_DATA.into());
        partition.is_hidden = Some(true);
        partition.is_read_only = None;
        partition.is_shadow_copy = None;
        partition.is_offline = Some(true);
        let eligible = |disk: &Disk, part: &Partition| linux_clone_candidate(disk, &[part], part);
        assert!(!eligible(&disk, &partition));
        disk.is_offline = Some(true);
        assert!(eligible(&disk, &partition));
        assert!(!ext_candidate(&disk, &[&partition], &partition));
        partition.is_offline = None;
        partition.is_hidden = None;
        assert!(eligible(&disk, &partition));
        assert!(!ext_candidate(&disk, &[&partition], &partition));
        assert!(!deletable(&partition));
        disk.is_offline = Some(false);
        assert!(!eligible(&disk, &partition));
        disk.is_offline = None;
        assert!(!eligible(&disk, &partition));
        disk.is_offline = Some(true);
        partition.is_boot = None;
        assert!(!eligible(&disk, &partition));
        partition.is_boot = Some(false);
        partition.is_read_only = Some(true);
        assert!(!eligible(&disk, &partition));
        partition.is_read_only = None;
        partition.is_shadow_copy = Some(true);
        assert!(!eligible(&disk, &partition));
        partition.is_shadow_copy = None;
        partition.gpt_type = Some(LINUX_SWAP.into());
        assert!(!eligible(&disk, &partition));
    }

    #[test]
    fn ext_format_validates_each_generation_and_sector_geometry() {
        use super::super::ExtFileSystem;
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        let mut p = part();
        p.gpt_type = Some(LINUX_DATA.into());
        p.is_hidden = Some(true);
        p.is_read_only = None;
        p.is_shadow_copy = None;
        for fs in [
            ExtFileSystem::Ext2,
            ExtFileSystem::Ext3,
            ExtFileSystem::Ext4,
        ] {
            for sector in [512, 4096] {
                d.logical_sector_size = Some(sector);
                for block in [0, 512, 1024, 2048, 4096, 8192] {
                    let action = Action::ExtFormat {
                        partition: 1,
                        label: "Linux资料".into(),
                        file_system: fs,
                        block_size: block,
                    };
                    assert_eq!(
                        validate(&d, &[p.clone()], &action).is_ok(),
                        [1024, 2048, 4096].contains(&block) && u64::from(block) >= sector
                    );
                    let mut protected = d.clone();
                    protected.is_system = Some(true);
                    assert!(validate(&protected, &[p.clone()], &action).is_err());
                }
            }
        }
    }
    #[test]
    fn ext4_only_accepts_non_system_linux_data_and_valid_utf8_label_lengths() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        let mut p = part();
        p.gpt_type = Some(LINUX_DATA.into());
        p.is_hidden = Some(true);
        p.is_read_only = None;
        p.is_shadow_copy = None;
        let action = Action::ExtResize {
            partition: 1,
            size: GIB,
        };
        assert!(validate(&d, &[p.clone()], &action).is_ok());
        for label in ["", "12345678901234567", "六个中文字节太长", "bad\0label"] {
            assert!(validate(
                &d,
                &[p.clone()],
                &Action::ExtFormat {
                    partition: 1,
                    label: label.into(),
                    file_system: super::super::ExtFileSystem::Ext4,
                    block_size: 4096,
                }
            )
            .is_err());
        }
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::ExtFormat {
                partition: 1,
                label: "Linux资料".into(),
                file_system: super::super::ExtFileSystem::Ext4,
                block_size: 4096,
            }
        )
        .is_ok());
        d.is_boot = Some(true);
        assert!(validate(&d, &[p.clone()], &action).is_err());
        d.is_boot = Some(false);
        p.gpt_type = Some(LINUX_SWAP.into());
        assert!(validate(&d, &[p.clone()], &action).is_err());
        p.gpt_type = Some(LINUX_DATA.into());
        p.is_read_only = Some(true);
        assert!(validate(&d, &[p], &action).is_err());
    }
    fn disks() -> Vec<Disk> {
        ["DS-OS-0001", "DS-TEST-A-0001", "DS-TEST-B-0001"]
            .iter()
            .enumerate()
            .map(|(i, s)| Disk {
                path: format!("disk-{i}"),
                number: i as u32,
                serial_number: Some(s.to_string()),
                friendly_name: Some("Example disk".into()),
                unique_id: Some(format!("vendor-{i}")),
                size: if i == 0 { 80 * GIB } else { 16 * GIB },
                logical_sector_size: Some(512),
                bus_type: Some(if i == 0 { 11 } else { 17 }),
                partition_style: Some(if i == 0 { 1 } else { 0 }),
                number_of_partitions: Some(0),
                is_boot: Some(i == 0),
                is_system: Some(i == 0),
                is_offline: Some(false),
                is_read_only: Some(false),
                is_clustered: Some(false),
            })
            .collect()
    }
    #[test]
    fn sector_write_rejects_missing_protections_system_and_non_data_targets() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        let p = part();
        assert!(sector_writable(&d, &[&p], &p).is_ok());
        for field in 0..6 {
            for value in [None, Some(true)] {
                let mut bad = p.clone();
                match field {
                    0 => bad.is_boot = value,
                    1 => bad.is_system = value,
                    2 => bad.is_read_only = value,
                    3 => bad.is_offline = value,
                    4 => bad.is_hidden = value,
                    _ => bad.is_shadow_copy = value,
                };
                assert!(sector_writable(&d, &[&bad], &bad).is_err());
            }
        }
        for kind in [
            LINUX_DATA,
            LINUX_SWAP,
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
            "",
        ] {
            let mut bad = p.clone();
            bad.gpt_type = Some(kind.into());
            assert!(sector_writable(&d, &[&bad], &bad).is_err());
        }
        let mut other = p.clone();
        other.partition_number = 2;
        other.is_system = Some(true);
        assert!(sector_writable(&d, &[&p, &other], &p).is_err());
        d.is_read_only = None;
        assert!(sector_writable(&d, &[&p], &p).is_err());
    }
    fn part() -> Partition {
        Partition {
            path: "partition-1".into(),
            disk_number: 1,
            partition_number: 1,
            offset: MIB,
            size: 2 * GIB,
            guid: Some("unique-guid".into()),
            gpt_type: Some(BASIC_DATA.into()),
            is_boot: Some(false),
            is_system: Some(false),
            is_read_only: Some(false),
            is_offline: Some(false),
            is_hidden: Some(false),
            is_shadow_copy: Some(false),
            ..Default::default()
        }
    }
    #[test]
    fn all_format_filesystems_preserve_partition_protections() {
        use super::super::FileSystem;
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        for fs in [
            FileSystem::Ntfs,
            FileSystem::Fat12,
            FileSystem::Fat16,
            FileSystem::Fat32,
            FileSystem::Exfat,
        ] {
            let mut base = part();
            if fs == FileSystem::Fat12 {
                base.size = 2 * MIB;
            }
            let action = Action::Format {
                options: Default::default(),
                partition: 1,
                label: "DATA".into(),
                file_system: fs,
                cluster_size: 0,
            };
            assert!(validate(&d, &[base.clone()], &action).is_ok());
            for variant in 0..7 {
                let mut p = base.clone();
                match variant {
                    0 => p.is_boot = Some(true),
                    1 => p.is_system = Some(true),
                    2 => p.is_read_only = Some(true),
                    3 => p.is_hidden = Some(true),
                    4 => p.is_offline = Some(true),
                    5 => p.is_boot = None,
                    _ => p.gpt_type = Some("c12a7328-f81f-11d2-ba4b-00a0c93ec93b".into()),
                }
                assert!(
                    validate(&d, &[p], &action).is_err(),
                    "{fs:?} protection {variant}"
                );
            }
        }
    }
    #[test]
    fn metadata_changes_preserve_system_and_unknown_state_protections() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        // Ordinary data partitions on a disk containing Windows are eligible.
        // Protection applies to the selected partition, not the whole disk.
        d.is_boot = Some(true);
        d.is_system = Some(true);
        for action in [
            Action::SetLabel {
                partition: 1,
                label: "资料".into(),
            },
            Action::SetDriveLetter {
                partition: 1,
                letter: None,
            },
        ] {
            let base = part();
            assert!(validate(&d, &[base.clone()], &action).is_ok());
            for variant in 0..10 {
                let mut p = base.clone();
                match variant {
                    0 => p.is_boot = Some(true),
                    1 => p.is_system = Some(true),
                    2 => p.is_read_only = Some(true),
                    3 => p.is_hidden = Some(true),
                    4 => p.is_offline = Some(true),
                    5 => p.is_shadow_copy = Some(true),
                    6 => p.is_boot = None,
                    7 => p.is_read_only = None,
                    8 => p.gpt_type = Some(LINUX_DATA.into()),
                    _ => p.gpt_type = Some("c12a7328-f81f-11d2-ba4b-00a0c93ec93b".into()),
                }
                assert!(validate(&d, &[p], &action).is_err(), "protection {variant}");
            }
        }
    }
    #[test]
    fn generic_disks_are_supported_without_vm_identity_or_fixed_capacity() {
        let mut ds = disks();
        ds.truncate(2);
        ds[1].serial_number = Some("ordinary-manufacturer-serial".into());
        ds[1].size = 2 * 1024 * GIB;
        ds[1].bus_type = Some(11);
        ds[1].logical_sector_size = Some(4096);
        ds[1].number = 7;
        let id = identity(&ds[1]).unwrap();
        assert!(inventory(&ds).is_ok());
        assert!(manageable(target_disk(&ds, &id).unwrap()).is_ok());
        ds[1].number = 9;
        assert_eq!(target_disk(&ds, &id).unwrap().number, 9);
        ds[1].serial_number = Some("replacement".into());
        assert!(target_disk(&ds, &id).is_err());
    }
    #[test]
    fn ambiguous_identity_and_cross_provider_mismatch_are_rejected() {
        let mut ds = disks();
        let id = identity(&ds[1]).unwrap();
        let mut legacy: Vec<_> = ds
            .iter()
            .map(|d| LegacyIdentity {
                index: d.number,
                serial_number: d.serial_number.clone(),
            })
            .collect();
        assert!(corroborate(&ds[1], &legacy).is_ok());
        legacy[1].serial_number = Some("different".into());
        assert!(corroborate(&ds[1], &legacy).is_err());
        ds[2].serial_number = ds[1].serial_number.clone();
        assert!(target_disk(&ds, &id).is_err());
        ds[1].serial_number = Some(" ".into());
        assert!(identity(&ds[1]).is_none());
    }
    #[test]
    fn system_and_unknown_protection_states_never_allow_operations() {
        let base = disks()[1].clone();
        for variant in 0..7 {
            let mut d = base.clone();
            match variant {
                0 => d.is_boot = Some(true),
                1 => d.is_system = Some(true),
                2 => d.is_system = None,
                3 => d.is_read_only = Some(true),
                4 => d.is_offline = Some(true),
                5 => d.is_clustered = Some(true),
                _ => d.partition_style = Some(1),
            }
            assert!(validate(&d, &[], &Action::Initialize {}).is_err());
        }
    }
    #[test]
    fn unsafe_parameters_and_protected_partitions_are_rejected() {
        let mut d = disks()[1].clone();
        assert!(validate(&d, &[], &Action::Initialize {}).is_ok());
        assert!(validate(
            &d,
            &[],
            &Action::Create {
                offset: MIB,
                size: GIB,
                kind: super::super::PartitionKind::Windows
            }
        )
        .is_err());
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        let p = part();
        assert!(validate(&d, &[p.clone()], &Action::Initialize {}).is_err());
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Create {
                offset: MIB,
                size: GIB,
                kind: super::super::PartitionKind::Windows
            }
        )
        .is_err());
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Create {
                offset: u64::MAX,
                size: GIB,
                kind: super::super::PartitionKind::Windows
            }
        )
        .is_err());
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Format {
                options: Default::default(),
                partition: 1,
                label: "bad:label".into(),
                file_system: super::super::FileSystem::Ntfs,
                cluster_size: 4096,
            }
        )
        .is_err());
        for variant in 0..5 {
            let mut q = p.clone();
            match variant {
                0 => q.is_system = Some(true),
                1 => q.is_boot = None,
                2 => q.gpt_type = Some("reserved".into()),
                3 => q.is_read_only = Some(true),
                _ => q.is_hidden = Some(true),
            };
            assert!(validate(&d, &[q], &Action::Delete { partition: 1 }).is_err());
        }
    }
    #[test]
    fn gaps_and_resize_never_cross_other_partitions() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(2);
        let p = part();
        let mut q = part();
        q.offset = 4 * GIB;
        q.partition_number = 2;
        q.path = "partition-2".into();
        q.guid = Some("another-guid".into());
        let ps = vec![p, q];
        assert!(validate(
            &d,
            &ps,
            &Action::Resize {
                partition: 1,
                size: 5 * GIB
            }
        )
        .is_err());
        assert!(validate(
            &d,
            &ps,
            &Action::Resize {
                partition: 1,
                size: GIB
            }
        )
        .is_ok());
        assert!(validate(
            &d,
            &ps,
            &Action::Create {
                offset: 3 * GIB,
                size: 512 * MIB,
                kind: super::super::PartitionKind::Windows
            }
        )
        .is_ok());
        assert!(validate(&d, &ps[..1], &Action::Delete { partition: 1 }).is_err());
    }
    #[test]
    fn system_disk_data_partitions_are_managed_but_running_windows_only_resizes() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        d.is_system = Some(true);
        d.is_boot = Some(true);
        let mut p = part();
        assert!(validate(&d, &[p.clone()], &Action::Delete { partition: 1 }).is_ok());
        p.is_boot = Some(true);
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Resize {
                partition: 1,
                size: GIB
            }
        )
        .is_ok());
        assert!(validate(&d, &[p.clone()], &Action::Delete { partition: 1 }).is_err());
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Format {
                options: Default::default(),
                partition: 1,
                label: "Windows".into(),
                file_system: super::super::FileSystem::Ntfs,
                cluster_size: 4096,
            }
        )
        .is_err());
        p.is_system = Some(true);
        assert!(validate(
            &d,
            &[p.clone()],
            &Action::Resize {
                partition: 1,
                size: GIB
            }
        )
        .is_err());
        p.is_system = None;
        assert!(validate(
            &d,
            &[p],
            &Action::Resize {
                partition: 1,
                size: GIB
            }
        )
        .is_err());
    }

    #[test]
    fn fat_resize_needs_a_non_system_disk_and_cannot_overlap_another_partition() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(2);
        let p = part();
        let mut other = part();
        other.partition_number = 2;
        other.path = "partition-2".into();
        other.guid = Some("second-guid".into());
        other.offset = 4 * GIB;
        let parts = [p, other];
        let request = Action::FatResize {
            partition: 1,
            size: MIB,
        };
        assert!(validate(&d, &parts, &request).is_ok());
        assert!(validate(
            &d,
            &parts,
            &Action::FatResize {
                partition: 1,
                size: 4 * GIB
            },
        )
        .is_err());
        d.is_system = Some(true);
        assert!(validate(&d, &parts, &request).is_err());
        d.is_system = Some(false);
        let mut parts = parts;
        parts[0].is_shadow_copy = None;
        assert!(validate(&d, &parts, &request).is_err());
    }
    #[test]
    fn linux_partition_deletion_does_not_imply_filesystem_support() {
        let mut d = disks()[1].clone();
        d.partition_style = Some(2);
        d.number_of_partitions = Some(1);
        for kind in [LINUX_DATA, LINUX_SWAP] {
            let mut p = part();
            p.gpt_type = Some(format!("{{{}}}", kind.to_uppercase()));
            assert!(validate(&d, &[p.clone()], &Action::Delete { partition: 1 }).is_ok());
            assert!(validate(
                &d,
                &[p.clone()],
                &Action::Resize {
                    partition: 1,
                    size: GIB
                }
            )
            .is_err());
            assert!(validate(
                &d,
                &[p.clone()],
                &Action::Format {
                    options: Default::default(),
                    partition: 1,
                    label: "new".into(),
                    file_system: super::super::FileSystem::Ntfs,
                    cluster_size: 4096,
                }
            )
            .is_err());
            p.is_hidden = Some(true);
            p.is_read_only = None;
            p.is_shadow_copy = None;
            assert!(validate(&d, &[p.clone()], &Action::Delete { partition: 1 }).is_ok());
            p.is_read_only = Some(true);
            assert!(validate(&d, &[p], &Action::Delete { partition: 1 }).is_err());
        }
        assert!(serde_json::from_str::<Action>(
            r#"{"type":"create","offset":1048576,"size":1073741824,"kind":"arbitrary-guid"}"#
        )
        .is_err());
    }
    #[test]
    fn frontend_cannot_supply_paths_methods_or_unknown_targets() {
        assert!(serde_json::from_str::<super::super::Request>(
            r#"{"target":"os","revision":"x","action":{"type":"initialize"}}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<Action>(r#"{"type":"initialize","path":"PhysicalDrive0"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Action>(r#"{"type":"resize","partition":1,"size":-1}"#).is_err()
        );
    }
}

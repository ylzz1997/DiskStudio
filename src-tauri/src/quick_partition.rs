//! Typed batch planning. No provider calls or device access in this module.
use super::{format_options::FormatOptions, FileSystem, Plan, Target};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Windows,
    Linux,
    Efi,
    Reserved,
}
impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "Windows 数据",
            Self::Linux => "Linux 数据",
            Self::Efi => "ESP",
            Self::Reserved => "MSR",
        }
    }
    pub fn gpt_type(self) -> &'static str {
        match self {
            Self::Windows => "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
            Self::Linux => "0fc63daf-8483-4772-8e79-3d69d8477de4",
            Self::Efi => "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
            Self::Reserved => "e3c9e316-0b5c-4db8-817d-f92df00215ae",
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Format {
    pub file_system: FileSystem,
    pub label: String,
    pub cluster_size: u32,
    pub options: FormatOptions,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub offset: u64,
    pub size: u64,
    pub role: Role,
    pub format: Option<Format>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub target: Target,
    pub revision: String,
    pub alignment: u32,
    pub delete_partitions: Vec<u32>,
    pub entries: Vec<Entry>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Step {
    Delete {
        partition: u32,
        offset: u64,
        size: u64,
        guid: String,
    },
    Create {
        entry: Entry,
    },
    Format {
        entry: Entry,
    },
}
impl Step {
    pub fn title(&self) -> String {
        match self {
            Self::Delete { partition, .. } => format!("删除分区 {partition}"),
            Self::Create { entry } => format!(
                "新建 {} 分区（起点 {} MiB，容量 {} MiB）",
                entry.role.name(),
                entry.offset / 1048576,
                entry.size / 1048576
            ),
            Self::Format { entry } => {
                let f = entry.format.as_ref().expect("validated format step");
                format!(
                    "{} · {}（起点 {} MiB）",
                    f.file_system.title(),
                    f.label,
                    entry.offset / 1048576
                )
            }
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    #[serde(flatten)]
    pub plan: Plan,
    pub steps: Vec<Step>,
    pub retained_partitions: Vec<u32>,
}

/// Advisory, invocation-scoped progress. Only Outcome is the final result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: Phase,
    pub completed: usize,
    pub total: usize,
    pub current: Option<usize>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Checking,
    Running,
    Verified,
    Stopped,
}

#[cfg(any(test, windows))]
pub(crate) fn validate(
    disk: &super::policy::Disk,
    partitions: &[super::policy::Partition],
    request: &Request,
) -> Result<Vec<Step>, String> {
    use super::{policy as p, Action};
    use std::collections::HashSet;
    p::manageable(disk)?;
    let parts = p::layout(disk, partitions)?;
    if disk.partition_style != Some(2) {
        return Err(
            "快速分区当前要求已初始化的 GPT 磁盘；RAW 请先单独初始化，MBR 尚不支持。".into(),
        );
    }
    if request.entries.is_empty()
        || request.entries.len() > 32
        || request.delete_partitions.len() > 128
    {
        return Err("每批须新建 1–32 个分区。".into());
    }
    let alignment = u64::from(request.alignment);
    if !(p::MIB..=16 * p::MIB).contains(&alignment) || !alignment.is_power_of_two() {
        return Err("对齐须为 1、2、4、8 或 16 MiB。".into());
    }
    let deleted: HashSet<_> = request.delete_partitions.iter().copied().collect();
    if deleted.len() != request.delete_partitions.len() {
        return Err("删除列表包含重复分区。".into());
    }
    let mut steps = Vec::new();
    for number in &request.delete_partitions {
        p::validate(disk, partitions, &Action::Delete { partition: *number })?;
        let old = parts
            .iter()
            .find(|p| p.partition_number == *number)
            .ok_or("分区已不存在")?;
        steps.push(Step::Delete {
            partition: *number,
            offset: old.offset,
            size: old.size,
            guid: old.guid.clone().ok_or("缺少分区 GUID")?,
        });
    }
    if parts.len() - deleted.len() + request.entries.len() > 128 {
        return Err("最终 GPT 布局超过 128 个分区。".into());
    }
    let mut occupied: Vec<(u64, u64)> = parts
        .iter()
        .filter(|p| !deleted.contains(&p.partition_number))
        .map(|p| (p.offset, p.offset + p.size))
        .collect();
    let mut roles: Vec<_> = parts
        .iter()
        .filter(|p| !deleted.contains(&p.partition_number))
        .filter_map(|p| p.gpt_type.as_deref())
        .map(p::guid)
        .collect();
    for entry in &request.entries {
        let end = entry.offset.checked_add(entry.size).ok_or("布局容量溢出")?;
        if entry.offset < p::MIB
            || entry.size < p::MIB
            || entry.size % p::MIB != 0
            || entry.offset % alignment != 0
            || end > disk.size - p::MIB
            || occupied
                .iter()
                .any(|(start, stop)| entry.offset < *stop && end > *start)
        {
            return Err(
                "新分区必须按所选边界对齐、容量为整 MiB，并完全位于删除所选分区后的未分配空间中。"
                    .into(),
            );
        }
        occupied.push((entry.offset, end));
        if matches!(entry.role, Role::Efi | Role::Reserved) {
            if disk.is_boot != Some(false)
                || disk.is_system != Some(false)
                || parts
                    .iter()
                    .any(|p| p.is_boot != Some(false) || p.is_system != Some(false))
            {
                return Err("仅可在非启动、非系统磁盘上新建 ESP/MSR 条目。".into());
            }
            if roles.iter().any(|r| r == entry.role.gpt_type()) {
                return Err("磁盘上已存在或本批已安排同类型 ESP/MSR 条目。".into());
            }
            if (entry.role == Role::Efi && entry.size < 260 * p::MIB)
                || (entry.role == Role::Reserved && entry.size != 16 * p::MIB)
            {
                return Err("ESP 条目至少 260 MiB；MSR 条目固定为 16 MiB。".into());
            }
        }
        roles.push(entry.role.gpt_type().into());
        if let Some(f) = &entry.format {
            if entry.role != Role::Windows {
                return Err("批量格式化仅限新建 Windows 数据分区。Linux、ESP/MSR 仅建立 GPT 条目，不创建文件系统或引导文件。".into());
            }
            f.options.validate(
                f.file_system,
                entry.size,
                disk.logical_sector_size.ok_or("缺少扇区大小")?,
                &f.label,
                f.cluster_size,
            )?;
        }
        steps.push(Step::Create {
            entry: entry.clone(),
        });
        if entry.format.is_some() {
            steps.push(Step::Format {
                entry: entry.clone(),
            });
        }
    }
    Ok(steps)
}

pub fn prepare(request: Request) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        super::platform::quick::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("快速分区仅支持 Windows 桌面版。".into())
    }
}
#[allow(dead_code)] // The guarded guest driver also exercises this no-observer API.
pub fn execute(token: String) -> Result<super::Outcome, String> {
    execute_with_progress(token, |_| {})
}
pub fn execute_with_progress(
    token: String,
    progress: impl FnMut(Progress),
) -> Result<super::Outcome, String> {
    #[cfg(windows)]
    {
        super::platform::quick::execute(token, progress)
    }
    #[cfg(not(windows))]
    {
        let _ = (token, progress);
        Err("快速分区仅支持 Windows 桌面版。".into())
    }
}
pub fn discard(token: String) {
    #[cfg(windows)]
    super::platform::quick::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partitions::policy::{self as p, Disk, Partition};
    fn fixture() -> (Disk, Vec<Partition>, Request) {
        let disk = Disk {
            path: "synthetic-provider".into(),
            number: 2,
            size: 4 * p::GIB,
            logical_sector_size: Some(512),
            partition_style: Some(2),
            number_of_partitions: Some(1),
            is_boot: Some(false),
            is_system: Some(false),
            is_offline: Some(false),
            is_read_only: Some(false),
            is_clustered: Some(false),
            ..Default::default()
        };
        let parts = vec![Partition {
            path: "synthetic-partition".into(),
            disk_number: 2,
            partition_number: 1,
            offset: p::MIB,
            size: 128 * p::MIB,
            guid: Some("synthetic-guid".into()),
            gpt_type: Some(p::BASIC_DATA.into()),
            is_boot: Some(false),
            is_system: Some(false),
            is_offline: Some(false),
            is_read_only: Some(false),
            is_hidden: Some(false),
            is_shadow_copy: Some(false),
            ..Default::default()
        }];
        let request = Request {
            target: Target("a".repeat(64)),
            revision: "synthetic".into(),
            alignment: p::MIB as u32,
            delete_partitions: vec![],
            entries: vec![Entry {
                offset: 256 * p::MIB,
                size: 128 * p::MIB,
                role: Role::Windows,
                format: Some(Format {
                    file_system: FileSystem::Ntfs,
                    label: "DATA".into(),
                    cluster_size: 4096,
                    options: FormatOptions::default(),
                }),
            }],
        };
        (disk, parts, request)
    }
    #[test]
    fn supports_preserved_layout_or_explicit_replacement_and_orders_steps() {
        let (d, p, mut r) = fixture();
        let s = validate(&d, &p, &r).unwrap();
        assert!(matches!(
            s.as_slice(),
            [Step::Create { .. }, Step::Format { .. }]
        ));
        r.delete_partitions.push(1);
        r.entries[0].offset = p[0].offset;
        let s = validate(&d, &p, &r).unwrap();
        assert!(matches!(
            s.as_slice(),
            [
                Step::Delete { .. },
                Step::Create { .. },
                Step::Format { .. }
            ]
        ));
    }
    #[test]
    fn rejects_overlap_with_retained_or_new_entries_and_checked_overflow() {
        let (d, p, base) = fixture();
        let mut r = base.clone();
        r.entries[0].offset = p[0].offset;
        assert!(validate(&d, &p, &r).is_err());
        let mut r = base.clone();
        r.entries.push(r.entries[0].clone());
        assert!(validate(&d, &p, &r).is_err());
        let mut r = base.clone();
        r.entries[0].size = u64::MAX;
        assert!(validate(&d, &p, &r).is_err());
        let mut r = base;
        r.entries[0].offset = d.size - p::MIB;
        assert!(validate(&d, &p, &r).is_err());
    }
    #[test]
    fn deletion_never_inherits_permission_from_another_entry() {
        let (d, parts, mut r) = fixture();
        r.delete_partitions = vec![1];
        for protected in [
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
            "e3c9e316-0b5c-4db8-817d-f92df00215ae",
            "unknown",
        ] {
            let mut p = parts.clone();
            p[0].gpt_type = Some(protected.into());
            assert!(validate(&d, &p, &r).is_err());
        }
        let mut p = parts.clone();
        p[0].is_boot = Some(true);
        assert!(validate(&d, &p, &r).is_err());
        let mut p = parts.clone();
        p[0].is_read_only = None;
        assert!(validate(&d, &p, &r).is_err());
        let mut p = parts;
        p[0].is_hidden = Some(true);
        assert!(validate(&d, &p, &r).is_err());
        r.delete_partitions = vec![1, 1];
        assert!(validate(&d, &p, &r).is_err());
        r.delete_partitions = vec![99];
        assert!(validate(&d, &p, &r).is_err());
    }
    #[test]
    fn esp_msr_only_allow_explicit_unformatted_unique_non_system_entries() {
        let (mut d, p, mut r) = fixture();
        r.entries[0].role = Role::Efi;
        r.entries[0].size = 260 * p::MIB;
        assert!(validate(&d, &p, &r).is_err());
        r.entries[0].format = None;
        validate(&d, &p, &r).unwrap();
        let mut duplicate = r.entries[0].clone();
        duplicate.offset = p::GIB;
        r.entries.push(duplicate);
        assert!(validate(&d, &p, &r).is_err());
        r.entries.pop();
        d.is_boot = Some(true);
        assert!(validate(&d, &p, &r).is_err());
        d.is_boot = Some(false);
        r.entries[0].role = Role::Reserved;
        assert!(validate(&d, &p, &r).is_err());
        r.entries[0].size = 16 * p::MIB;
        validate(&d, &p, &r).unwrap();
        let mut p = p;
        p[0].gpt_type = Some(Role::Reserved.gpt_type().into());
        assert!(validate(&d, &p, &r).is_err());
    }
    #[test]
    fn alignment_capacity_and_unsupported_format_parameters_fail_closed() {
        let (mut d, p, mut r) = fixture();
        for alignment in [0, 512, 3 * p::MIB as u32, 32 * p::MIB as u32] {
            r.alignment = alignment;
            assert!(validate(&d, &p, &r).is_err());
        }
        r.alignment = 16 * p::MIB as u32;
        validate(&d, &p, &r).unwrap();
        r.entries[0].offset += p::MIB;
        assert!(validate(&d, &p, &r).is_err());
        r.entries[0].offset -= p::MIB;
        r.entries[0].role = Role::Linux;
        assert!(validate(&d, &p, &r).is_err());
        r.entries[0].role = Role::Windows;
        let f = r.entries[0].format.as_mut().unwrap();
        f.options.ntfs_compression = true;
        f.cluster_size = 65536;
        assert!(validate(&d, &p, &r).is_err());
        r.entries[0].format = None;
        d.logical_sector_size = Some(4096);
        validate(&d, &p, &r).unwrap();
        d.partition_style = Some(1);
        assert!(validate(&d, &p, &r).is_err());
        d.partition_style = Some(0);
        assert!(validate(&d, &[], &r).is_err());
    }
    #[test]
    fn wire_protocol_rejects_paths_unknown_roles_and_unknown_options() {
        let (_, _, r) = fixture();
        let base = serde_json::to_value(&r).unwrap();
        for key in ["path", "wql", "method"] {
            let mut bad = base.clone();
            bad[key] = "arbitrary".into();
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
        let mut bad = base.clone();
        bad["entries"][0]["role"] = "swap".into();
        assert!(serde_json::from_value::<Request>(bad).is_err());
        let mut bad = base.clone();
        bad["entries"][0]["format"]["options"]["force"] = true.into();
        assert!(serde_json::from_value::<Request>(bad).is_err());
        let mut bad = base;
        bad["target"] = "PhysicalDrive2".into();
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }
}

//! Explicit whole-device state changes; inventory never invokes this module.
use crate::{
    data_tasks::Selection,
    partitions::policy::{self, Disk, Partition},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Online,
    Offline,
    ReadOnly,
    ReadWrite,
}
impl Action {
    pub fn title(self) -> &'static str {
        match self {
            Self::Online => "将磁盘联机",
            Self::Offline => "将磁盘脱机",
            Self::ReadOnly => "设为只读",
            Self::ReadWrite => "解除只读",
        }
    }
    pub fn desired(self, before: State) -> State {
        match self {
            Self::Online => State {
                offline: false,
                ..before
            },
            Self::Offline => State {
                offline: true,
                ..before
            },
            Self::ReadOnly => State {
                read_only: true,
                ..before
            },
            Self::ReadWrite => State {
                read_only: false,
                ..before
            },
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub offline: bool,
    pub read_only: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub action: Action,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub title: String,
    pub target: String,
    pub size: u64,
    pub partitions: usize,
    pub before: State,
    pub after: State,
    pub warning: String,
}
#[derive(Serialize)]
pub struct Outcome {
    pub message: String,
}

pub fn eligible(d: &Disk, parts: &[Partition], action: Action) -> Result<State, String> {
    if d.path.is_empty()
        || d.is_boot != Some(false)
        || d.is_system != Some(false)
        || d.is_clustered != Some(false)
        || !matches!(d.partition_style, Some(0..=2))
        || !matches!(d.logical_sector_size, Some(512 | 4096))
        || d.size == 0
        || d.size > (1u64 << 53) - 1
        || matches!(d.bus_type, None | Some(0 | 8 | 14 | 16))
        || parts
            .iter()
            .any(|p| p.is_boot != Some(false) || p.is_system != Some(false))
    {
        return Err("设备状态变更仅适用于身份、布局和保护信息完整的非系统基本磁盘；系统、群集、动态/存储池及未知状态不支持。".into());
    }
    let before = State {
        offline: d.is_offline.ok_or("脱机状态未知")?,
        read_only: d.is_read_only.ok_or("只读状态未知")?,
    };
    if action.desired(before) == before {
        return Err("磁盘已经处于所选状态，请刷新信息。".into());
    }
    if matches!(action, Action::ReadOnly | Action::ReadWrite)
        && (before.offline || d.partition_style == Some(0))
    {
        return Err(
            "Windows 的只读属性方法需要已初始化的联机磁盘；请先单独完成联机，不会自动联机。".into(),
        );
    }
    Ok(before)
}

/// A clone must not silently acquire access through duplicate GPT identities.
pub fn unique_partition_ids(
    disks: &[Disk],
    parts: &[Partition],
    target: u32,
) -> Result<(), String> {
    let mut ids = std::collections::HashMap::<String, Vec<u32>>::new();
    for disk in disks.iter().filter(|d| d.partition_style == Some(2)) {
        for p in policy::layout(disk, parts)? {
            let id = p
                .guid
                .as_deref()
                .map(policy::guid)
                .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                .filter(|id| !id.is_nil())
                .ok_or("GPT 分区标识清单不完整，不能解除访问限制。")?
                .to_string();
            ids.entry(id).or_default().push(disk.number);
        }
    }
    if ids
        .values()
        .any(|owners| owners.contains(&target) && owners.len() != 1)
    {
        return Err(
            "目标存在与其他磁盘重复的 GPT 分区 GUID，不能联机或解除只读；不会自动更改标识。".into(),
        );
    }
    Ok(())
}

pub fn prepare(request: Request) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::device_state::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("设备状态变更当前仅支持 Windows。".into())
    }
}
pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::device_state::execute(token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("设备状态变更当前仅支持 Windows。".into())
    }
}
pub fn discard(token: String) {
    #[cfg(windows)]
    crate::partitions::platform::device_state::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

#[cfg(test)]
#[path = "device_state_tests.rs"]
mod tests;

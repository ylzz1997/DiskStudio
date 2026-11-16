//! Typed GPT basic-data attribute edits. Pure planning never opens devices.
use crate::{
    data_tasks::Selection,
    table_archive::{Archive, Kind},
    table_restore,
};
use serde::{Deserialize, Serialize};
pub const READ_ONLY: u64 = 1 << 60;
pub const HIDDEN: u64 = 1 << 62;
pub const NO_LETTER: u64 = 1 << 63;
pub const ALLOWED: u64 = READ_ONLY | HIDDEN | NO_LETTER;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Hide,
    Show,
    ReadOnly,
    ReadWrite,
    NoDefaultLetter,
    AutomaticLetter,
}
impl Action {
    pub fn title(self) -> &'static str {
        match self {
            Self::Hide => "隐藏分区",
            Self::Show => "显示分区",
            Self::ReadOnly => "分区设为只读",
            Self::ReadWrite => "解除分区只读",
            Self::NoDefaultLetter => "禁止自动分配盘符",
            Self::AutomaticLetter => "允许自动分配盘符",
        }
    }
    pub fn desired(self, before: u64) -> u64 {
        match self {
            Self::Hide => before | HIDDEN,
            Self::Show => before & !HIDDEN,
            Self::ReadOnly => before | READ_ONLY,
            Self::ReadWrite => before & !READ_ONLY,
            Self::NoDefaultLetter => before | NO_LETTER,
            Self::AutomaticLetter => before & !NO_LETTER,
        }
    }
    pub fn opens_access(self) -> bool {
        matches!(self, Self::Show | Self::ReadWrite | Self::AutomaticLetter)
    }
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attributes {
    pub hidden: bool,
    pub read_only: bool,
    pub no_default_letter: bool,
}
impl From<u64> for Attributes {
    fn from(bits: u64) -> Self {
        Self {
            hidden: bits & HIDDEN != 0,
            read_only: bits & READ_ONLY != 0,
            no_default_letter: bits & NO_LETTER != 0,
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub title: String,
    pub target: String,
    pub partition: u32,
    pub offset: u64,
    pub size: u64,
    pub before: Attributes,
    pub after: Attributes,
    pub warning: String,
}
#[derive(Serialize)]
pub struct Outcome {
    pub message: String,
    pub sha256: String,
}

/// Omitted WMI parameters must remain unspecified, never rewrite other flags.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Params {
    #[serde(skip_serializing_if = "Option::is_none")]
    is_hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_default_drive_letter: Option<bool>,
}
pub fn params(action: Action) -> Params {
    let mut p = Params {
        is_hidden: None,
        is_read_only: None,
        no_default_drive_letter: None,
    };
    match action {
        Action::Hide => p.is_hidden = Some(true),
        Action::Show => p.is_hidden = Some(false),
        Action::ReadOnly => p.is_read_only = Some(true),
        Action::ReadWrite => p.is_read_only = Some(false),
        Action::NoDefaultLetter => p.no_default_drive_letter = Some(true),
        Action::AutomaticLetter => p.no_default_drive_letter = Some(false),
    }
    p
}

/// Construct expected bytes only. Windows performs the actual attribute change.
pub fn expected(current: &Archive, guid: &str, action: Action) -> Result<Archive, String> {
    if current.includes_boot {
        return Err("属性预览不能包含文件系统引导扇区。".into());
    }
    let before = table_restore::archive_layout(current)?;
    let selected: Vec<_> = before.entries.iter().filter(|e| e.guid == guid).collect();
    if selected.len() != 1 {
        return Err("分区标识不唯一。".into());
    }
    let e = selected[0];
    if e.kind != crate::partitions::policy::BASIC_DATA || e.attributes & !ALLOWED != 0 {
        return Err("仅支持普通 GPT Windows 数据分区；必需、快照及未知属性不允许更改。".into());
    }
    let bits = action.desired(e.attributes);
    if bits == e.attributes {
        return Err("分区已经处于所选状态。".into());
    }
    let mut after = current.clone();
    let mut crc = None;
    for extent in &mut after.extents {
        if matches!(extent.kind, Kind::PrimaryEntries | Kind::BackupEntries) {
            let at = (e.slot as usize - 1) * 128 + 48;
            extent.bytes[at..at + 8].copy_from_slice(&bits.to_le_bytes());
            let hash = crc32fast::hash(&extent.bytes[..before.count as usize * 128]);
            if crc.is_some_and(|c| c != hash) {
                return Err("GPT 条目副本不一致。".into());
            }
            crc = Some(hash);
        }
    }
    let crc = crc.ok_or("缺少 GPT 条目表")?;
    for extent in &mut after.extents {
        match extent.kind {
            Kind::PrimaryHeader | Kind::BackupHeader => {
                extent.bytes[88..92].copy_from_slice(&crc.to_le_bytes());
                extent.bytes[16..20].fill(0);
                let len = u32::from_le_bytes(extent.bytes[12..16].try_into().unwrap()) as usize;
                let crc = crc32fast::hash(&extent.bytes[..len]);
                extent.bytes[16..20].copy_from_slice(&crc.to_le_bytes());
            }
            Kind::Mbr | Kind::PrimaryEntries | Kind::BackupEntries => {}
            _ => return Err("属性预览含非 GPT 表范围。".into()),
        }
    }
    let mut desired = before;
    desired
        .entries
        .iter_mut()
        .find(|p| p.guid == guid)
        .unwrap()
        .attributes = bits;
    if table_restore::archive_layout(&after)? != desired {
        return Err("属性预览改变了其他布局字段。".into());
    }
    Ok(after)
}

#[cfg(any(test, windows))]
pub fn eligible(
    d: &crate::partitions::policy::Disk,
    p: &crate::partitions::policy::Partition,
) -> Result<(), String> {
    use crate::partitions::policy;
    if d.partition_style != Some(2)
        || d.is_offline != Some(false)
        || d.is_read_only != Some(false)
        || d.is_boot != Some(false)
        || d.is_system != Some(false)
        || d.is_clustered != Some(false)
        || matches!(d.bus_type, None | Some(0 | 8 | 14 | 16))
        || p.disk_number != d.number
        || p.path.is_empty()
        || p.gpt_type.as_deref().map(policy::guid).as_deref() != Some(policy::BASIC_DATA)
        || p.is_boot != Some(false)
        || p.is_system != Some(false)
        || p.is_shadow_copy != Some(false)
        || p.is_offline != Some(false)
        || p.is_hidden.is_none()
        || p.is_read_only.is_none()
    {
        return Err(
            "属性编辑需要联机、可写的非系统 GPT 基本磁盘和保护信息完整的普通 Windows 数据分区。"
                .into(),
        );
    }
    Ok(())
}
pub fn prepare(request: Request) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::partition_attributes::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区属性当前仅支持 Windows。".into())
    }
}
pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::partition_attributes::execute(token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("分区属性当前仅支持 Windows。".into())
    }
}
pub fn discard(token: String) {
    #[cfg(windows)]
    crate::partitions::platform::partition_attributes::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

#[cfg(test)]
#[path = "partition_attributes_tests.rs"]
mod tests;

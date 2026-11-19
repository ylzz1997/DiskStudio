//! The same partition-management API ships in every desktop build.
//! VM isolation belongs to development tools, not production capabilities.
use serde::{Deserialize, Serialize};
#[path = "format_options.rs"]
pub mod format_options;
pub use format_options::FileSystem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtFileSystem {
    Ext2,
    Ext3,
    Ext4,
}
impl ExtFileSystem {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ext2 => "ext2",
            Self::Ext3 => "ext3",
            Self::Ext4 => "ext4",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Ext2 => "ext2 格式化",
            Self::Ext3 => "ext3 格式化",
            Self::Ext4 => "ext4 格式化",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct Target(pub String);
impl TryFrom<String> for Target {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value))
        } else {
            Err("磁盘标识无效，请刷新后重新选择。")
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    Initialize {},
    Create {
        offset: u64,
        size: u64,
        kind: PartitionKind,
    },
    Format {
        partition: u32,
        label: String,
        #[serde(rename = "fileSystem")]
        file_system: FileSystem,
        #[serde(rename = "clusterSize")]
        cluster_size: u32,
        #[serde(default)]
        options: format_options::FormatOptions,
    },
    Resize {
        partition: u32,
        size: u64,
    },
    FatResize {
        partition: u32,
        size: u64,
    },
    ExtFormat {
        partition: u32,
        label: String,
        #[serde(rename = "fileSystem")]
        file_system: ExtFileSystem,
        #[serde(rename = "blockSize")]
        block_size: u32,
    },
    ExtResize {
        partition: u32,
        size: u64,
    },
    BtrfsFormat {
        partition: u32,
        label: String,
    },
    BtrfsResize {
        partition: u32,
        size: u64,
    },
    Delete {
        partition: u32,
    },
    SetLabel {
        partition: u32,
        label: String,
    },
    SetDriveLetter {
        partition: u32,
        letter: Option<DriveLetter>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct DriveLetter(pub String);
impl TryFrom<String> for DriveLetter {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 1 && value.as_bytes()[0].is_ascii_uppercase() {
            Ok(Self(value))
        } else {
            Err("盘符必须为 A–Z 中的一个大写字母。")
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PartitionKind {
    Windows,
    Linux,
}
impl Action {
    pub fn partition(&self) -> Option<u32> {
        match self {
            Self::Format { partition, .. }
            | Self::Resize { partition, .. }
            | Self::FatResize { partition, .. }
            | Self::ExtFormat { partition, .. }
            | Self::ExtResize { partition, .. }
            | Self::BtrfsFormat { partition, .. }
            | Self::BtrfsResize { partition, .. }
            | Self::SetLabel { partition, .. }
            | Self::SetDriveLetter { partition, .. }
            | Self::Delete { partition } => Some(*partition),
            _ => None,
        }
    }
    pub fn title(&self) -> &'static str {
        match self {
            Self::Initialize {} => "初始化为 GPT",
            Self::Create { .. } => "新建分区",
            Self::Format { file_system, .. } => file_system.title(),
            Self::ExtFormat { file_system, .. } => file_system.title(),
            Self::ExtResize { .. } => "调整 Ext 文件系统大小",
            Self::BtrfsFormat { .. } => "Btrfs 格式化",
            Self::BtrfsResize { .. } => "调整 Btrfs 文件系统大小",
            Self::FatResize { .. } => "调整 FAT / exFAT 分区大小",
            Self::Resize { .. } => "调整分区大小",
            Self::Delete { .. } => "删除分区",
            Self::SetLabel { .. } => "修改卷标",
            Self::SetDriveLetter { .. } => "管理盘符",
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub target: Target,
    pub revision: String,
    pub action: Action,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Extent {
    pub offset: u64,
    pub size: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target: Target,
    pub disk_id: String,
    pub name: String,
    pub serial: String,
    pub revision: String,
    pub free_ranges: Vec<Extent>,
    pub layout_key: String,
    pub format_partitions: Vec<u32>,
    pub delete_partitions: Vec<u32>,
    pub resize_partitions: Vec<u32>,
    pub fat_resize_partitions: Vec<u32>,
    pub ext_partitions: Vec<u32>,
    pub metadata_partitions: Vec<u32>,
    pub available_drive_letters: Vec<String>,
    pub available: bool,
    pub reason: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub reason: Option<String>,
    pub targets: Vec<TargetInfo>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub token: String,
    pub title: String,
    pub target_name: String,
    pub serial: String,
    pub partition: Option<u32>,
    pub before_size: Option<u64>,
    pub after_size: Option<u64>,
    pub warning: String,
    pub expires_at: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub message: String,
    pub verified: bool,
}

#[cfg(any(test, windows))]
#[path = "partition_merge.rs"]
pub(crate) mod merge;
#[cfg(windows)]
#[path = "windows_storage.rs"]
pub(crate) mod platform;
#[cfg(any(test, windows))]
#[path = "partition_policy.rs"]
pub(crate) mod policy;
#[path = "quick_partition.rs"]
pub mod quick;
#[path = "partition_reallocation.rs"]
pub(crate) mod reallocation;

pub fn status() -> Status {
    #[cfg(windows)]
    {
        match platform::status() {
            Ok(s) => s,
            Err(reason) => Status {
                enabled: false,
                reason: Some(reason),
                targets: vec![],
            },
        }
    }
    #[cfg(not(windows))]
    {
        Status {
            enabled: false,
            reason: Some("分区管理当前仅支持 Windows 桌面版。".into()),
            targets: vec![],
        }
    }
}
pub fn prepare(request: Request) -> Result<Plan, String> {
    #[cfg(windows)]
    {
        platform::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区管理当前仅支持 Windows 桌面版。".into())
    }
}
pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(windows)]
    {
        platform::execute(token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("分区管理当前仅支持 Windows 桌面版。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_format_requests_stay_quick_and_unknown_settings_are_rejected() {
        let base = serde_json::json!({"type":"format","partition":3,"label":"DATA","fileSystem":"NTFS","clusterSize":0});
        let Action::Format { options, .. } =
            serde_json::from_value::<Action>(base.clone()).unwrap()
        else {
            panic!()
        };
        assert_eq!(options, format_options::FormatOptions::default());
        let mut full = base.clone();
        full["options"] =
            serde_json::json!({"mode":"full","ntfsCompression":true,"ntfsShortNames":true});
        let Action::Format { options, .. } = serde_json::from_value::<Action>(full).unwrap() else {
            panic!()
        };
        assert_eq!(options.mode, format_options::FormatMode::Full);
        assert!(options.ntfs_compression && options.ntfs_short_names);
        for extra in [
            serde_json::json!({"mode":"full","force":true}),
            serde_json::json!({"method":"Format"}),
            serde_json::json!({"mode":"full","path":"C:\\"}),
        ] {
            let mut bad = base.clone();
            bad["options"] = extra;
            assert!(serde_json::from_value::<Action>(bad).is_err());
        }
    }
    #[test]
    fn target_is_an_opaque_identifier_never_a_path_or_number() {
        for invalid in [
            "os",
            "0",
            "\\\\.\\PhysicalDrive0",
            "SELECT * FROM MSFT_Disk",
        ] {
            assert!(Target::try_from(invalid.to_string()).is_err());
        }
        let target = Target::try_from("a".repeat(64)).unwrap();
        assert_eq!(
            serde_json::to_string(&target).unwrap(),
            format!("\"{}\"", "a".repeat(64))
        );
    }
}

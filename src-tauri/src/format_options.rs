//! Pure format settings. Never opens a volume or invokes a storage provider.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FormatMode {
    #[default]
    Quick,
    Full,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormatOptions {
    #[serde(default)]
    pub mode: FormatMode,
    #[serde(default)]
    pub ntfs_compression: bool,
    #[serde(default)]
    pub ntfs_short_names: bool,
}
impl FormatOptions {
    pub fn effective_cluster(self, fs: FileSystem, cluster: u32) -> u32 {
        // Never let a large-volume provider default silently exceed the NTFS
        // compression limit. The effective 4 KiB choice appears in preview.
        if fs == FileSystem::Ntfs && self.ntfs_compression && cluster == 0 {
            4096
        } else {
            cluster
        }
    }
    pub fn validate(
        self,
        fs: FileSystem,
        size: u64,
        sector: u64,
        label: &str,
        cluster: u32,
    ) -> Result<(), String> {
        if fs != FileSystem::Ntfs && (self.ntfs_compression || self.ntfs_short_names) {
            return Err("压缩和 8.3 短文件名选项仅适用于 NTFS。".into());
        }
        let effective = self.effective_cluster(fs, cluster);
        if self.ntfs_compression && effective > 4096 {
            return Err("NTFS 压缩需要不大于 4 KiB 的簇；默认选项会明确使用 4 KiB。".into());
        }
        fs.validate(size, sector, label, effective)
    }
    pub fn warning(self, fs: FileSystem, cluster: u32) -> String {
        let mut message = match self.mode {
            FormatMode::Quick => "将快速建立新的文件系统；不会逐扇区覆盖原有数据。".to_owned(),
            FormatMode::Full if fs.fat_type().is_some() => "将覆盖所选卷的全部逻辑扇区，建立 FAT 文件系统并完整回读 SHA-256；所需时间与卷容量有关。".to_owned(),
            FormatMode::Full => "将由 Windows 执行完整格式化并写入卷内全部扇区，所需时间与卷容量有关；提供者调用期间不能取消。".to_owned(),
        };
        message.push_str(" 所选分区的现有文件将被清除。格式化不等同于 SSD 硬件安全擦除。执行期间请勿关闭应用或计算机。");
        if fs == FileSystem::Ntfs {
            message.push_str(&format!(
                " NTFS 新文件默认压缩：{}；卷的 8.3 短文件名设置：{}。",
                if self.ntfs_compression {
                    "开启"
                } else {
                    "关闭"
                },
                if self.ntfs_short_names {
                    "开启"
                } else {
                    "关闭"
                }
            ));
            if self.ntfs_compression && cluster == 0 {
                message.push_str(" 为支持压缩，本次实际采用 4096 字节簇。");
            }
            message.push_str(" 短文件名的实际创建还受 Windows 全局策略影响；不会修改全局策略。");
        }
        message
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FormatInput<'a> {
    file_system: &'static str,
    file_system_label: &'a str,
    // The provider rejects an explicit zero; leave its default unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    allocation_unit_size: Option<u32>,
    full: bool,
    force: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    compress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_file_name_support: Option<bool>,
}
impl<'a> FormatInput<'a> {
    pub fn new(fs: FileSystem, label: &'a str, cluster: u32, options: FormatOptions) -> Self {
        let cluster = options.effective_cluster(fs, cluster);
        Self {
            file_system: fs.name(),
            file_system_label: label,
            allocation_unit_size: (cluster != 0).then_some(cluster),
            full: options.mode == FormatMode::Full,
            force: false,
            // NTFS-specific options are not valid FAT/exFAT arguments,
            // including explicitly setting them to false.
            compress: (fs == FileSystem::Ntfs).then_some(options.ntfs_compression),
            short_file_name_support: (fs == FileSystem::Ntfs).then_some(options.ntfs_short_names),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FileSystem {
    #[serde(rename = "NTFS")]
    Ntfs,
    #[serde(rename = "FAT12")]
    Fat12,
    #[serde(rename = "FAT16")]
    Fat16,
    #[serde(rename = "FAT32")]
    Fat32,
    #[serde(rename = "exFAT")]
    Exfat,
}
impl FileSystem {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ntfs => "NTFS",
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::Exfat => "exFAT",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Ntfs => "格式化为 NTFS",
            Self::Fat12 => "格式化为 FAT12",
            Self::Fat16 => "格式化为 FAT16",
            Self::Fat32 => "格式化为 FAT32",
            Self::Exfat => "格式化为 exFAT",
        }
    }
    pub fn clusters(self) -> &'static [u32] {
        match self {
            Self::Ntfs => &[512, 1024, 2048, 4096, 8192, 16384, 32768, 65536],
            Self::Fat12 | Self::Fat16 | Self::Fat32 => &[512, 1024, 2048, 4096, 8192, 16384, 32768],
            Self::Exfat => &[
                4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152, 4194304,
                8388608, 16777216, 33554432,
            ],
        }
    }
    pub fn fat_type(self) -> Option<disk_core::fat_format::FatType> {
        use disk_core::fat_format::FatType;
        match self {
            Self::Fat12 => Some(FatType::Fat12),
            Self::Fat16 => Some(FatType::Fat16),
            Self::Fat32 => Some(FatType::Fat32),
            _ => None,
        }
    }
    pub fn windows_name(self) -> &'static str {
        if matches!(self, Self::Fat12 | Self::Fat16) {
            "FAT"
        } else {
            self.name()
        }
    }
    pub fn provider_clusters_allow(self, sizes: Option<&[u32]>, cluster: u32) -> bool {
        let Some(sizes) = sizes else {
            return false;
        };
        if sizes.is_empty() && self == Self::Exfat {
            // Server 2025 returns success with an empty list for exFAT (all
            // case variants). The documented Format method still supports it.
            // Use the bounded exFAT options already checked by validate();
            // the Format provider remains the final authority at execution.
            return cluster == 0 || self.clusters().contains(&cluster);
        }
        if cluster == 0 {
            sizes.iter().any(|size| self.clusters().contains(size))
        } else {
            sizes.contains(&cluster)
        }
    }
    pub fn validate(self, size: u64, sector: u64, label: &str, cluster: u32) -> Result<(), String> {
        if let Some(kind) = self.fat_type() {
            return disk_core::fat_format::Plan::new(
                kind,
                size,
                sector.try_into().map_err(|_| "无效扇区大小")?,
                cluster,
                label,
                1,
                0,
            )
            .map(|_| ())
            .map_err(|e| e.to_string());
        }
        let max_label = if self == Self::Ntfs { 32 } else { 11 };
        if label.trim().is_empty()
            || label != label.trim()
            || label.encode_utf16().count() > max_label
            || label
                .chars()
                .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        {
            return Err(format!("{} 卷标须为 1–{max_label} 个 UTF-16 字符，不能包含首尾空格、控制字符或 \\ / : * ? \" < > |。", self.name()));
        }
        if !matches!(sector, 512 | 4096) || size < 64 * 1024 * 1024 {
            return Err("文件系统容量或扇区信息不支持格式化。".into());
        }
        if cluster != 0
            && (!self.clusters().contains(&cluster)
                || u64::from(cluster) < sector
                || u64::from(cluster) > size)
        {
            return Err("簇大小无效，必须使用支持的选项且不小于逻辑扇区。".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const G: u64 = 1024 * 1024 * 1024;
    #[test]
    fn full_format_and_ntfs_options_have_strict_typed_boundaries() {
        let options = FormatOptions {
            mode: FormatMode::Full,
            ntfs_compression: true,
            ntfs_short_names: true,
        };
        let input =
            serde_json::to_value(FormatInput::new(FileSystem::Ntfs, "DATA", 0, options)).unwrap();
        assert_eq!(input["Full"], true);
        assert_eq!(input["Force"], false);
        assert_eq!(input["Compress"], true);
        assert_eq!(input["ShortFileNameSupport"], true);
        assert_eq!(input["AllocationUnitSize"], 4096);
        assert!(options
            .validate(FileSystem::Ntfs, G, 4096, "DATA", 0)
            .is_ok());
        for cluster in [8192, 65536] {
            assert!(options
                .validate(FileSystem::Ntfs, G, 512, "DATA", cluster)
                .is_err());
        }
        assert!(options
            .validate(FileSystem::Ntfs, G, 4096, "DATA", 512)
            .is_err());
        for fs in [
            FileSystem::Fat12,
            FileSystem::Fat16,
            FileSystem::Fat32,
            FileSystem::Exfat,
        ] {
            for o in [
                options,
                FormatOptions {
                    ntfs_short_names: true,
                    ..Default::default()
                },
                FormatOptions {
                    ntfs_compression: true,
                    ..Default::default()
                },
            ] {
                assert!(o.validate(fs, G, 512, "DATA", 0).is_err());
            }
            let input = serde_json::to_value(FormatInput::new(
                fs,
                "DATA",
                0,
                FormatOptions {
                    mode: FormatMode::Full,
                    ..Default::default()
                },
            ))
            .unwrap();
            assert_eq!(input["Full"], true);
            assert!(input.get("Compress").is_none());
            assert!(input.get("ShortFileNameSupport").is_none());
        }
        for json in [
            r#"{"mode":"unsafe"}"#,
            r#"{"mode":true}"#,
            r#"{"force":true}"#,
            r#"{"ntfsCompression":null}"#,
        ] {
            assert!(serde_json::from_str::<FormatOptions>(json).is_err());
        }
        assert_eq!(
            serde_json::from_str::<FormatOptions>("{}").unwrap(),
            FormatOptions::default()
        );
    }
    #[test]
    fn provider_default_cluster_is_omitted_instead_of_zero() {
        let default = serde_json::to_value(FormatInput::new(
            FileSystem::Fat32,
            "DATA",
            0,
            FormatOptions::default(),
        ))
        .unwrap();
        assert!(default.get("AllocationUnitSize").is_none());
        assert_eq!(default["FileSystem"], "FAT32");
        assert_eq!(default["Force"], false);
        assert!(default.get("Compress").is_none());
        assert!(default.get("ShortFileNameSupport").is_none());
        let explicit = serde_json::to_value(FormatInput::new(
            FileSystem::Exfat,
            "资料",
            131072,
            FormatOptions::default(),
        ))
        .unwrap();
        assert_eq!(explicit["AllocationUnitSize"], 131072);
        assert_eq!(explicit["FileSystemLabel"], "资料");
        assert!(explicit.get("Compress").is_none());
        let ntfs = serde_json::to_value(FormatInput::new(
            FileSystem::Ntfs,
            "DATA",
            4096,
            FormatOptions::default(),
        ))
        .unwrap();
        assert_eq!(ntfs["Compress"], false);
    }
    #[test]
    fn exfat_empty_provider_list_does_not_enable_unknown_or_invalid_options() {
        assert!(FileSystem::Exfat.provider_clusters_allow(Some(&[]), 0));
        assert!(FileSystem::Exfat.provider_clusters_allow(Some(&[]), 131072));
        assert!(!FileSystem::Exfat.provider_clusters_allow(Some(&[]), 1234));
        assert!(!FileSystem::Exfat.provider_clusters_allow(None, 0));
        assert!(!FileSystem::Ntfs.provider_clusters_allow(Some(&[]), 0));
        assert!(!FileSystem::Fat32.provider_clusters_allow(Some(&[]), 4096));
        assert!(!FileSystem::Exfat.provider_clusters_allow(Some(&[4096]), 131072));
        assert!(FileSystem::Fat32.provider_clusters_allow(Some(&[512, 4096]), 4096));
    }
    #[test]
    fn format_settings_reject_lossy_labels_and_unsafe_sizes() {
        assert!(FileSystem::Fat32
            .validate(2 * G, 512, "DS_FAT32", 4096)
            .is_ok());
        assert!(FileSystem::Fat32
            .validate(33 * G, 512, "DS_FAT32", 0)
            .is_ok());
        assert!(FileSystem::Fat32.validate(2 * G, 512, "中文", 0).is_err());
        assert!(FileSystem::Exfat
            .validate(64 * G, 4096, "资料", 131072)
            .is_ok());
        assert!(FileSystem::Exfat
            .validate(G, 512, "😀".repeat(6).as_str(), 0)
            .is_err());
        assert!(FileSystem::Ntfs.validate(G, 4096, "DATA", 512).is_err());
        assert!(FileSystem::Ntfs.validate(G, 512, "DATA", 1234).is_err());
        for fs in [FileSystem::Ntfs, FileSystem::Fat32, FileSystem::Exfat] {
            for label in ["", " DATA", "DATA ", "DATA:ADS", "DATA\0", "DATA/NAME"] {
                assert!(fs.validate(G, 512, label, 0).is_err());
            }
        }
        assert!(serde_json::from_str::<FileSystem>("\"arbitrary-shell-command\"").is_err());
    }
}

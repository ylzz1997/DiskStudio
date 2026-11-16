//! Output-only metadata, derived from fresh extraction receipts. No source or
//! path access, no ACL/owner/reparse/compression/encryption changes.
use crate::file_metadata::{Metadata, Stamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    #[serde(default = "yes")]
    pub timestamps: bool,
    #[serde(default = "yes")]
    pub attributes: bool,
    #[serde(default = "two")]
    pub workers: usize,
}
fn yes() -> bool {
    true
}
fn two() -> usize {
    2
}
impl Default for Options {
    fn default() -> Self {
        Self {
            timestamps: true,
            attributes: true,
            workers: 2,
        }
    }
}
impl Options {
    pub fn validate(self) -> Result<Self, String> {
        if ![1, 2, 4].contains(&self.workers) {
            return Err("提取并发只能选择 1、2 或 4。".into());
        }
        Ok(self)
    }
}
pub const ATTRIBUTE_MASK: u32 = 1 | 2 | 4 | 0x20 | 0x2000;
const EPOCH_NS: i128 = 11_644_473_600_000_000_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub name: &'static str,
    pub status: &'static str,
    pub requested: Option<String>,
    pub observed: Option<String>,
    pub note: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub source: Metadata,
    pub fields: Vec<Field>,
    pub error: Option<String>,
}
impl Report {
    pub fn warnings(&self) -> usize {
        usize::from(self.error.is_some())
            + self
                .fields
                .iter()
                .filter(|f| matches!(f.status, "unavailable" | "rounded" | "mismatch" | "failed"))
                .count()
    }
}
fn filetime(stamp: &Stamp) -> Option<i64> {
    let ns = stamp.instant_ns?.checked_add(EPOCH_NS)?;
    let ticks = i64::try_from(ns.checked_div(100)?).ok()?;
    // Zero / negative values have special native semantics; never pass them as
    // an inferred timestamp. Sub-100ns precision is reported explicitly.
    (ns >= 100 && ticks > 0).then_some(ticks)
}
fn requests(metadata: &Metadata, options: Options) -> (Report, [Option<i64>; 3], Option<u32>) {
    let stamps = [
        ("created", &metadata.created),
        ("modified", &metadata.modified),
        ("accessed", &metadata.accessed),
    ];
    let mut fields = vec![];
    let mut times = [None; 3];
    for (i, (name, stamp)) in stamps.into_iter().enumerate() {
        times[i] = options
            .timestamps
            .then(|| stamp.as_ref().and_then(filetime))
            .flatten();
        fields.push(Field {
            name,
            status: if !options.timestamps {
                "disabled"
            } else if times[i].is_some() {
                "pending"
            } else {
                "unavailable"
            },
            requested: times[i].map(|t| t.to_string()),
            observed: None,
            note: if options.timestamps && times[i].is_none() {
                "源记录没有可写入的绝对时间；未推断时区、日期或创建时间。".into()
            } else {
                String::new()
            },
        });
    }
    let attributes = options
        .attributes
        .then_some(metadata.windows_flags)
        .flatten()
        .map(|f| f & ATTRIBUTE_MASK);
    fields.push(Field {
        name: "attributes",
        status: if !options.attributes {
            "disabled"
        } else if attributes.is_some() {
            "pending"
        } else {
            "unavailable"
        },
        requested: attributes.map(|f| format!("0x{f:08X}")),
        observed: None,
        note: if options.attributes && attributes.is_none() {
            "源记录没有受支持的 Windows 文件属性；不把 Linux 权限映射为 Windows 属性。".into()
        } else {
            String::new()
        },
    });
    (
        Report {
            source: metadata.clone(),
            fields,
            error: None,
        },
        times,
        attributes,
    )
}

#[cfg(windows)]
pub fn apply(file: &std::fs::File, metadata: &Metadata, options: Options) -> Report {
    apply_kind(file, metadata, options, false)
}
#[cfg(windows)]
pub(crate) fn apply_directory(
    file: &std::fs::File,
    metadata: &Metadata,
    options: Options,
) -> Report {
    apply_kind(file, metadata, options, true)
}
pub(crate) fn unmodified(metadata: &Metadata, options: Options, error: Option<String>) -> Report {
    let (mut report, _, _) = requests(metadata, options);
    report.error = error;
    for field in &mut report.fields {
        if field.status == "pending" {
            field.status = "unavailable";
        }
    }
    report
}
#[cfg(windows)]
fn apply_kind(
    file: &std::fs::File,
    metadata: &Metadata,
    options: Options,
    directory: bool,
) -> Report {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::*;
    let (mut report, times, attributes) = requests(metadata, options);
    let result = (|| -> Result<FILE_BASIC_INFO, String> {
        let get = || -> Result<FILE_BASIC_INFO, String> {
            let mut info = FILE_BASIC_INFO::default();
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&mut info as *mut FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(info)
        };
        let before = get()?;
        if before.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (before.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        {
            return Err("元数据目标不是已验证的输出文件或目录类型。".into());
        }
        let mut attrs = attributes.map_or(0, |desired| {
            (before.FileAttributes & !ATTRIBUTE_MASK & !FILE_ATTRIBUTE_NORMAL) | desired
        });
        if attributes.is_some() && attrs == 0 {
            attrs = FILE_ATTRIBUTE_NORMAL;
        }
        let info = FILE_BASIC_INFO {
            CreationTime: times[0].unwrap_or(0),
            LastWriteTime: times[1].unwrap_or(0),
            LastAccessTime: times[2].unwrap_or(0),
            ChangeTime: 0,
            FileAttributes: attrs,
        };
        if times.iter().any(Option::is_some) || attributes.is_some() {
            if unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&info as *const FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        get()
    })();
    match result {
        Err(e) => {
            report.error = Some(e);
            for f in &mut report.fields {
                if f.status == "pending" {
                    f.status = "failed";
                }
            }
        }
        Ok(observed) => {
            let observed_times = [
                observed.CreationTime,
                observed.LastWriteTime,
                observed.LastAccessTime,
            ];
            let stamps = [&metadata.created, &metadata.modified, &metadata.accessed];
            for i in 0..3 {
                if let Some(t) = times[i] {
                    report.fields[i].observed = Some(observed_times[i].to_string());
                    let lost_precision = stamps[i]
                        .as_ref()
                        .and_then(|s| s.instant_ns)
                        .is_some_and(|n| n % 100 != 0);
                    report.fields[i].status = if observed_times[i] == t && !lost_precision {
                        "preserved"
                    } else {
                        "rounded"
                    };
                    if report.fields[i].status == "rounded" {
                        report.fields[i].note =
                            "输出文件系统或 Windows 时间精度与源记录不同，实际值已记录。".into();
                    }
                }
            }
            if let Some(attrs) = attributes {
                report.fields[3].observed = Some(format!(
                    "0x{:08X}",
                    observed.FileAttributes & ATTRIBUTE_MASK
                ));
                report.fields[3].status = if observed.FileAttributes & ATTRIBUTE_MASK == attrs {
                    "preserved"
                } else {
                    "mismatch"
                };
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imported_metadata_cannot_supply_writable_values_and_local_times_stay_unknown() {
        let metadata:Metadata=serde_json::from_value(serde_json::json!({"source":"NTFS 标准信息","created":{"text":"2024-01-01 00:00:00","zone":"UTC","precision":"100 ns"},"modified":null,"accessed":null,"changed":null,"attributes":["只读"],"rawFlags":"0x00000001","permissions":null,"uid":null,"gid":null,"links":null})).unwrap();
        let (report, times, attrs) = requests(&metadata, Options::default());
        assert!(times.iter().all(Option::is_none));
        assert!(attrs.is_none());
        assert_eq!(report.warnings(), 4);
        let stamp = Stamp {
            text: String::new(),
            zone: "UTC".into(),
            precision: String::new(),
            instant_ns: Some(0),
        };
        assert_eq!(filetime(&stamp), Some(116444736000000000));
        for ns in [-EPOCH_NS, -EPOCH_NS - 1, i128::MAX] {
            let mut invalid = stamp.clone();
            invalid.instant_ns = Some(ns);
            assert!(filetime(&invalid).is_none());
        }
        for workers in [0, 3, 5, usize::MAX] {
            assert!(Options {
                workers,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for field in ["windowsFlags", "instantNs"] {
            let mut value = serde_json::to_value(&metadata).unwrap();
            if field == "windowsFlags" {
                value[field] = serde_json::json!(1);
            } else {
                value["created"][field] = serde_json::json!(0);
            }
            assert!(serde_json::from_value::<Metadata>(value).is_err());
        }
        for workers in [1, 2, 4] {
            let options: Options = serde_json::from_value(serde_json::json!({
                "workers":workers,"timestamps":false,"attributes":false
            }))
            .unwrap();
            assert!(options.validate().is_ok());
            let (report, times, attrs) = requests(&metadata, options);
            assert!(times.iter().all(Option::is_none));
            assert!(attrs.is_none());
            assert!(report.fields.iter().all(|f| f.status == "disabled"));
            assert_eq!(report.warnings(), 0);
        }
        assert!(serde_json::from_value::<Options>(serde_json::json!({"path":"anything"})).is_err());
    }
    #[test]
    fn native_ordinary_output_preserves_times_and_safe_attributes_after_close() {
        use std::{fs::OpenOptions, io::Write, os::windows::fs::MetadataExt};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join(format!("metadata-output-{}.bin", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"synthetic metadata output").unwrap();
        file.sync_all().unwrap();
        let instant = 1_700_000_000_123_456_700i128;
        let stamp = Stamp {
            text: "2023 synthetic".into(),
            zone: "UTC".into(),
            precision: "100 ns".into(),
            instant_ns: Some(instant),
        };
        let metadata = Metadata {
            created: Some(stamp.clone()),
            modified: Some(stamp.clone()),
            accessed: Some(stamp.clone()),
            windows_flags: Some(0x23),
            ..Default::default()
        };
        let report = apply(&file, &metadata, Options::default());
        assert!(report.error.is_none(), "{:?}", report.error);
        assert!(
            report.fields.iter().all(|f| f.status == "preserved"),
            "{report:?}"
        );
        drop(file);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.last_write_time(), filetime(&stamp).unwrap() as u64);
        assert_eq!(after.creation_time(), filetime(&stamp).unwrap() as u64);
        assert_eq!(after.file_attributes() & ATTRIBUTE_MASK, 0x23);
        // Clear only this fixture's readonly/hidden attributes before removing
        // its exact ordinary file. No filesystem or hardware discovery.
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .access_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES
                    | windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES,
            )
            .open(&path)
            .unwrap();
        let clean = apply(
            &file,
            &Metadata {
                windows_flags: Some(0),
                ..Default::default()
            },
            Options {
                timestamps: false,
                ..Default::default()
            },
        );
        assert!(clean.error.is_none());
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic metadata output");
        std::fs::remove_file(path).unwrap();
    }
}

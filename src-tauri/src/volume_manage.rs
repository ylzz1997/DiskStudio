//! Drive-letter and label changes use provider-owned objects only. This module
//! never opens devices, shells out, or accepts a path from the frontend.
use super::*;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub(super) struct Proof {
    fingerprint: String,
    letter: Option<String>,
    label: String,
    free_letters: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Access {
    #[serde(rename = "__PATH")]
    path: String,
    disk_number: u32,
    partition_number: u32,
    offset: u64,
    size: u64,
    guid: Option<String>,
    access_paths: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyVolume {
    #[serde(rename = "DeviceID")]
    device_id: String,
    drive_letter: Option<String>,
    file_system: Option<String>,
    label: Option<String>,
    boot_volume: Option<bool>,
    system_volume: Option<bool>,
    page_file_present: Option<bool>,
}

fn letter(value: Option<&str>, colon: bool) -> Result<Option<String>, String> {
    let value = value.map(policy::clean).unwrap_or("");
    if value.is_empty() {
        return Ok(None);
    }
    let value = if colon {
        value.strip_suffix(':').ok_or("盘符元数据无效")?
    } else {
        value
    };
    if value.len() != 1 || !value.as_bytes()[0].is_ascii_alphabetic() {
        return Err("盘符元数据无效".into());
    }
    Ok(Some(value.to_ascii_uppercase()))
}
pub(super) fn free_letters(s: &Session) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct LogicalDisk {
        #[serde(rename = "DeviceID")]
        id: String,
    }
    let rows: Vec<LogicalDisk> = WMIConnection::new()
        .map_err(err)?
        .raw_query("SELECT DeviceID FROM Win32_LogicalDisk")
        .map_err(err)?;
    if rows.len() > 26 {
        return Err("逻辑盘符列表异常".into());
    }
    let mut used = BTreeSet::new();
    for row in rows {
        used.insert(letter(Some(&row.id), true)?.ok_or("缺少逻辑盘符")?);
    }
    for p in &s.partitions {
        if let Some(l) = letter(p.drive_letter.as_deref(), false)? {
            used.insert(l);
        }
    }
    Ok((b'A'..=b'Z')
        .map(|c| (c as char).to_string())
        .filter(|l| !used.contains(l))
        .collect())
}
pub(super) fn normalize_label(fs: &str, input: &str) -> Result<String, String> {
    let fs = fs.to_ascii_uppercase();
    let fat = matches!(fs.as_str(), "FAT" | "FAT12" | "FAT16" | "FAT32");
    let max = match fs.as_str() {
        "NTFS" | "REFS" => 32,
        "EXFAT" | "FAT" | "FAT12" | "FAT16" | "FAT32" => 11,
        _ => return Err("该文件系统暂不支持修改卷标。".into()),
    };
    let value = if fat {
        input.to_ascii_uppercase()
    } else {
        input.to_string()
    };
    if value != value.trim()
        || value.encode_utf16().count() > max
        || value
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        || (fat
            && !value
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b" _-".contains(&b)))
    {
        return Err(if fat {
            "FAT 卷标最多 11 个英文字母、数字、空格、下划线或连字符；可留空清除。".into()
        } else {
            format!("卷标最多 {max} 个 UTF-16 字符，不能有首尾空格、控制字符或 \\ / : * ? \" < > |；可留空清除。")
        });
    }
    Ok(value)
}
pub(super) fn volume_key(value: &str) -> Result<String, String> {
    let s = value.trim_end_matches('\\').to_ascii_lowercase();
    if s.len() != 48
        || !s.starts_with("\\\\?\\volume{")
        || !s.ends_with('}')
        || uuid::Uuid::parse_str(&s[11..47]).is_err()
    {
        return Err("卷 GUID 路径无法确认。".into());
    }
    Ok(s)
}
fn legacy_matches(
    legacy: &LegacyVolume,
    v: &Volume,
    current: &Option<String>,
) -> Result<bool, String> {
    Ok(legacy.boot_volume == Some(false)
        && legacy.system_volume == Some(false)
        && legacy.page_file_present == Some(false)
        && letter(legacy.drive_letter.as_deref(), true)? == *current
        && legacy
            .file_system
            .as_deref()
            .unwrap_or("")
            .eq_ignore_ascii_case(v.file_system.as_deref().unwrap_or(""))
        && legacy.label.as_deref().unwrap_or("") == v.file_system_label.as_deref().unwrap_or(""))
}
fn inspect(s: &Session, r: &Request, v: &Volume) -> Result<Proof, String> {
    let d = s.disk(&r.target)?;
    policy::validate(d, &s.partitions, &r.action)?;
    let p = s.partition(&r.target, r.action.partition().ok_or("未选择分区")?)?;
    let current = letter(p.drive_letter.as_deref(), false)?;
    if current != letter(v.drive_letter.as_deref(), false)? {
        return Err("分区与卷的盘符不一致。".into());
    }
    let key = volume_key(v.volume_path.as_deref().ok_or("缺少卷 GUID")?)?;
    let rows: Vec<LegacyVolume> = WMIConnection::new().map_err(err)?
        .raw_query("SELECT DeviceID,DriveLetter,FileSystem,Label,BootVolume,SystemVolume,PageFilePresent FROM Win32_Volume").map_err(err)?;
    let rows: Vec<_> = rows
        .iter()
        .filter(|l| volume_key(&l.device_id).as_deref() == Ok(key.as_str()))
        .collect();
    if rows.len() != 1 {
        return Err("两套卷元数据不能唯一对应目标。".into());
    }
    let legacy = rows[0];
    if !legacy_matches(legacy, v, &current)? {
        return Err("系统、启动、分页文件卷或状态不一致的卷不能修改盘符/卷标。".into());
    }
    if let Some(l) = &current {
        if s.pagefiles
            .iter()
            .any(|f| f.name.to_ascii_uppercase().starts_with(&format!("{l}:")))
        {
            return Err("目标卷正在承载分页文件。".into());
        }
    }
    let access: Vec<Access> = s.con.raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,AccessPaths FROM MSFT_Partition").map_err(err)?;
    let matches: Vec<_> = access
        .iter()
        .filter(|a| a.path.eq_ignore_ascii_case(&p.path))
        .collect();
    if matches.len() != 1 {
        return Err("访问路径与分区关联不唯一。".into());
    }
    let a = matches[0];
    if a.disk_number != p.disk_number
        || a.partition_number != p.partition_number
        || a.offset != p.offset
        || a.size != p.size
        || a.guid != p.guid
    {
        return Err("读取访问路径时分区身份已变化。".into());
    }
    let mut paths = BTreeSet::new();
    let mut path_letters = BTreeSet::new();
    for path in a.access_paths.as_ref().ok_or("访问路径元数据缺失")? {
        let path = path.trim_end_matches('\\').to_ascii_lowercase();
        if path.len() == 2 && path.ends_with(':') {
            path_letters.insert(letter(Some(&path), true)?.ok_or("无效盘符路径")?);
        } else {
            paths.insert(path);
        }
    }
    if path_letters != current.iter().cloned().collect() || !paths.contains(&key) {
        return Err("盘符与卷访问路径不一致。".into());
    }
    // Normalize only the selected drive letter. Every disk/partition identity,
    // protection flag, boundary and other partition's drive letter stays bound.
    let mut parts = s.partitions.clone();
    for part in &mut parts {
        part.drive_letter =
            if part.disk_number == p.disk_number && part.partition_number == p.partition_number {
                None
            } else {
                letter(part.drive_letter.as_deref(), false)?
            };
    }
    parts.sort_by_key(|p| (p.disk_number, p.partition_number));
    let mut disks = s.disks.clone();
    disks.sort_by_key(|d| d.number);
    Ok(Proof {
        fingerprint: digest(&(
            disks,
            parts,
            &v.object_id,
            key,
            &v.file_system,
            v.size,
            paths,
        ))?,
        letter: current,
        label: v.file_system_label.clone().unwrap_or_default(),
        free_letters: free_letters(s)?,
    })
}
pub(super) fn preview(s: &Session, r: &Request, v: &Volume) -> Result<Proof, String> {
    let proof = inspect(s, r, v)?;
    match &r.action {
        Action::SetLabel { label, .. } => {
            let normalized =
                normalize_label(v.file_system.as_deref().ok_or("文件系统未知")?, label)?;
            if *label != normalized {
                return Err("卷标未规范化，请重新预览。".into());
            }
            if *label == proof.label {
                return Err("卷标没有变化。".into());
            }
        }
        Action::SetDriveLetter { letter, .. } => {
            let desired = letter.as_ref().map(|l| l.0.clone());
            if let Some(l) = &desired {
                DriveLetter::try_from(l.clone()).map_err(String::from)?;
            }
            if desired == proof.letter {
                return Err("盘符没有变化。".into());
            }
            if desired
                .as_ref()
                .is_some_and(|l| !proof.free_letters.contains(l))
            {
                return Err("目标盘符已被占用，请选择其他盘符。".into());
            }
        }
        _ => return Err("不支持的卷管理操作".into()),
    }
    Ok(proof)
}
fn expected(proof: &Proof, before: &Proof, letter: &Option<String>, label: &str) -> bool {
    proof.fingerprint == before.fingerprint && proof.letter == *letter && proof.label == label
}
fn postcheck(
    r: &Request,
    before: &Proof,
    letter: &Option<String>,
    label: &str,
) -> Result<Session, String> {
    let mut last = String::new();
    for _ in 0..8 {
        let result = (|| {
            let fresh = Session::open()?;
            let v = fresh.volume(fresh.partition(&r.target, r.action.partition().unwrap())?)?;
            if !expected(&inspect(&fresh, r, &v)?, before, letter, label) {
                return Err("卷标、盘符、访问路径或分区布局未达到预期。".into());
            }
            Ok(fresh)
        })();
        match result {
            Ok(fresh) => return Ok(fresh),
            Err(error) => last = error,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "操作后的卷身份、保护状态、盘符或卷标未核对完整：{last}"
    ))
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct AccessInput {
    access_path: String,
}
impl AccessInput {
    fn for_letter(value: &str) -> Result<Self, String> {
        let letter = DriveLetter::try_from(value.to_owned()).map_err(String::from)?;
        // MSFT_Partition.AddAccessPath treats AccessPath and AssignDriveLetter
        // as mutually exclusive even when the latter is explicitly false.
        // Fixed-letter operations supply only AccessPath; never auto-assign.
        Ok(Self {
            access_path: format!("{}:\\", letter.0),
        })
    }
}
pub(super) fn execute(s: &Session, r: &Request, v: &Volume) -> Result<Outcome, String> {
    let before = preview(s, r, v)?;
    let number = r.action.partition().ok_or("未选择分区")?;
    let result = (|| {
        match &r.action {
            Action::SetLabel { label, .. } => {
                #[derive(Serialize)]
                #[serde(rename_all = "PascalCase")]
                struct LabelInput<'a> {
                    file_system_label: &'a str,
                }
                call::<Volume>(
                    s,
                    &v.path,
                    "SetFileSystemLabel",
                    LabelInput {
                        file_system_label: label,
                    },
                )?;
                postcheck(r, &before, &before.letter, label)?;
            }
            Action::SetDriveLetter { letter, .. } => {
                let desired = letter.as_ref().map(|l| l.0.clone());
                let mut fresh = None;
                if let Some(old) = &before.letter {
                    call::<Partition>(
                        s,
                        &s.partition(&r.target, number)?.path,
                        "RemoveAccessPath",
                        AccessInput::for_letter(old)?,
                    )?;
                    fresh = Some(postcheck(r, &before, &None, &before.label)?);
                }
                if let Some(new) = &desired {
                    let s = fresh.as_ref().unwrap_or(s);
                    // Recheck the destination after removing the old letter.
                    // Never steal an occupied letter or retry a failed mutation.
                    if !free_letters(s)?.contains(new) {
                        return Err("目标盘符刚被占用，已停止。".into());
                    }
                    call::<Partition>(
                        s,
                        &s.partition(&r.target, number)?.path,
                        "AddAccessPath",
                        AccessInput::for_letter(new)?,
                    )?;
                }
                postcheck(r, &before, &desired, &before.label)?;
            }
            _ => return Err("不支持的卷管理操作".into()),
        }
        Ok(())
    })();
    result.map_err(|e: String| {
        crate::native_locale::diagnostic(
            "{error} 操作可能部分完成，请刷新核对实际盘符和卷标；不会自动重试或回滚。",
            &e,
        )
    })?;
    let locale = crate::native_locale::current();
    Ok(Outcome {
        verified: true,
        message: locale.format(
            "{action}已完成，已核对卷身份、分区布局及其他访问路径。",
            &[("action", locale.text(r.action.title()))],
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_letter_method_inputs_omit_mutually_exclusive_auto_assignment() {
        for value in b'A'..=b'Z' {
            let letter = (value as char).to_string();
            let input = serde_json::to_value(AccessInput::for_letter(&letter).unwrap()).unwrap();
            assert_eq!(
                input,
                serde_json::json!({"AccessPath": format!("{letter}:\\")})
            );
            assert!(input.get("AssignDriveLetter").is_none());
        }
        for invalid in [
            "",
            "a",
            "Z:",
            "Z:\\",
            "AA",
            "1",
            "\\\\server\\share",
            "C:\\folder",
        ] {
            assert!(AccessInput::for_letter(invalid).is_err());
        }
    }
    #[test]
    fn legacy_volume_protection_includes_unlettered_pagefile_volumes() {
        let v = Volume {
            path: "provider-reference".into(),
            object_id: "id".into(),
            volume_path: None,
            file_system: Some("NTFS".into()),
            file_system_label: Some("Data".into()),
            drive_letter: None,
            size: Some(1024),
        };
        for boot in [None, Some(false), Some(true)] {
            for system in [None, Some(false), Some(true)] {
                for pagefile in [None, Some(false), Some(true)] {
                    let mut l = LegacyVolume {
                        device_id: "id".into(),
                        drive_letter: None,
                        file_system: Some("ntfs".into()),
                        label: Some("Data".into()),
                        boot_volume: boot,
                        system_volume: system,
                        page_file_present: pagefile,
                    };
                    assert_eq!(
                        legacy_matches(&l, &v, &None).unwrap(),
                        boot == Some(false) && system == Some(false) && pagefile == Some(false)
                    );
                    l.label = Some("Replaced".into());
                    assert!(!legacy_matches(&l, &v, &None).unwrap());
                }
            }
        }
    }
    #[test]
    fn labels_preserve_unicode_lengths_and_clear_explicitly() {
        assert_eq!(normalize_label("NTFS", "资料😀").unwrap(), "资料😀");
        assert_eq!(normalize_label("FAT", "data-1").unwrap(), "DATA-1");
        for fs in ["NTFS", "FAT", "FAT32", "exFAT", "ReFS"] {
            assert_eq!(normalize_label(fs, "").unwrap(), "");
        }
        for (fs, s) in [
            ("exFAT", "😀😀😀😀😀😀"),
            ("NTFS", " bad"),
            ("FAT", "中文"),
            ("FAT", "A.B"),
            ("RAW", ""),
            ("NTFS", "a\0b"),
        ] {
            assert!(normalize_label(fs, s).is_err());
        }
        assert!(normalize_label("NTFS", &"😀".repeat(16)).is_ok());
        assert!(normalize_label("NTFS", &"😀".repeat(17)).is_err());
    }
    #[test]
    fn drive_letter_inputs_never_become_arbitrary_paths() {
        for bad in ["c", "C:", "C:\\", "\\\\?\\Volume{abc}", "AA", "", "1", "é"] {
            assert!(serde_json::from_value::<DriveLetter>(serde_json::json!(bad)).is_err());
        }
        assert_eq!(letter(Some("f:"), true).unwrap(), Some("F".into()));
        assert!(letter(Some("F:\\"), true).is_err());
        assert!(volume_key(r"\\.\PhysicalDrive1").is_err());
        assert!(volume_key(r"\\?\Volume{110ba738-0cdb-426d-a6bf-e7709f06e498}\").is_ok());
        assert!(serde_json::from_value::<Action>(
            serde_json::json!({"type":"setDriveLetter","partition":1,"letter":"F","path":"C:\\"})
        )
        .is_err());
    }
    #[test]
    fn postconditions_reject_replacement_partial_changes_and_other_layout_changes() {
        let base = Proof {
            fingerprint: "bound".into(),
            letter: Some("F".into()),
            label: "Data".into(),
            free_letters: vec![],
        };
        let mut after = Proof {
            fingerprint: "bound".into(),
            letter: None,
            label: "Data".into(),
            free_letters: vec!["F".into()],
        };
        assert!(expected(&after, &base, &None, "Data"));
        assert!(!expected(&after, &base, &Some("G".into()), "Data"));
        after.letter = Some("G".into());
        assert!(expected(&after, &base, &Some("G".into()), "Data"));
        assert!(!expected(&after, &base, &Some("G".into()), "New"));
        after.fingerprint = "replacement or changed layout".into();
        assert!(!expected(&after, &base, &Some("G".into()), "Data"));
    }
}

use crate::{
    data_tasks::Selection,
    table_archive::{self, Info},
};
use serde::{Deserialize, Serialize};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupRequest {
    pub source: Selection,
    pub includes_boot: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub name: String,
    pub info: Info,
    pub file_sha256: String,
}
#[cfg(windows)]
fn run<T>(
    title: &str,
    action: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "另一项磁盘操作正在运行。")?;
    let (id, cancel) = crate::data_tasks::start(title)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(&cancel)))
        .unwrap_or_else(|_| {
            Err("分区表备份任务异常结束；未修改源磁盘，未完成输出可能保留。".into())
        });
    crate::data_tasks::finish(
        &id,
        result
            .as_ref()
            .map(|_| ("分区表备份任务完成".into(), None))
            .map_err(Clone::clone),
    );
    result
}
pub fn backup(request: BackupRequest) -> Result<Option<Report>, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区表备份当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        run("备份分区表", |cancel| windows::backup(request, cancel))
    }
}
pub fn verify() -> Result<Option<Report>, String> {
    #[cfg(not(windows))]
    {
        Err("原生备份文件选择当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        run("验证分区表备份", |cancel| windows::verify(cancel))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backup_ipc_accepts_only_typed_identity_layout_and_boot_choice() {
        let good = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"layout","partition":null},"includesBoot":true});
        assert!(serde_json::from_value::<BackupRequest>(good.clone()).is_ok());
        for key in ["path", "device", "method"] {
            let mut value = good.clone();
            value[key] = serde_json::json!("x");
            assert!(serde_json::from_value::<BackupRequest>(value).is_err());
        }
        let mut value = good;
        value["source"]["target"] = serde_json::json!("device");
        assert!(serde_json::from_value::<BackupRequest>(value).is_err());
    }
}
#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{data_windows as platform, partitions::platform as partitions};
    use cap_std::fs::{Dir, OpenOptions as CapOptions, OpenOptionsExt as CapOptionsExt};
    use sha2::{Digest, Sha256};
    use std::{
        fs::OpenOptions,
        io::{Read, Seek, SeekFrom, Write},
        os::windows::fs::{MetadataExt, OpenOptionsExt},
        sync::atomic::{AtomicBool, Ordering},
    };
    use windows_sys::Win32::Storage::FileSystem::*;
    fn check(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            Err("已取消分区表备份任务。".into())
        } else {
            Ok(())
        }
    }
    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    pub fn backup(request: BackupRequest, cancel: &AtomicBool) -> Result<Option<Report>, String> {
        if request.source.partition.is_some() {
            return Err("请选择整块磁盘。".into());
        }
        let initial = partitions::table_read_context(&request.source, None)?;
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("备份分区表到另一块磁盘"))
            .add_filter(
                crate::native_locale::text("DiskStudio 分区表备份"),
                &["dspt"],
            )
            .set_file_name("DiskStudio-partition-table.dspt")
            .save_file()
        else {
            return Ok(None);
        };
        check(cancel)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("文件名无效")?;
        if name.len() > 180
            || name.contains([':', '/', '\\'])
            || !name.to_ascii_lowercase().ends_with(".dspt")
        {
            return Err("请选择新的 .dspt 文件名。".into());
        }
        let folder = platform::destination_folder(
            path.parent().ok_or("目录无效")?,
            &request.source.target,
            table_archive::MAX_FILE as u64,
        )?;
        let dir =
            Dir::open_ambient_dir(&folder, cap_std::ambient_authority()).map_err(platform::err)?;
        if dir.exists(name) {
            return Err("备份文件已存在，请选择新文件名。".into());
        }
        let ctx = partitions::table_read_context(&request.source, None)?;
        if ctx.fingerprint != initial.fingerprint {
            return Err("选择文件期间磁盘状态已变化，请重试。".into());
        }
        let locks = platform::lock(&ctx.resolved)?;
        let fresh = partitions::table_read_context(&request.source, Some((&ctx, &locks)))?;
        let mut reader = platform::device(&fresh.resolved, false)?;
        let d = &fresh.resolved.disk;
        // Exclude table-dependent UniqueId so a later damaged-table recovery
        // can still bind the same physical medium. The live selection remains
        // the full opaque identity and is independently revalidated each time.
        let source = crate::disk_identity::key(
            d.serial_number.as_deref(),
            None,
            d.friendly_name.as_deref(),
            d.size,
            d.logical_sector_size,
            d.bus_type,
        )
        .ok_or("磁盘身份缺失")?;
        let archive = table_archive::capture(
            &mut reader,
            d.size,
            d.logical_sector_size.ok_or("扇区信息缺失")?,
            source,
            request.includes_boot,
            || check(cancel),
        )?;
        let info = archive.validate()?;
        if info.layout.len() != fresh.parts.len()
            || info.layout.iter().any(|p| {
                !fresh
                    .parts
                    .iter()
                    .any(|m| m.offset == p.offset && m.size == p.size)
            })
        {
            return Err("原始分区表与 Windows 分区范围不一致，未保存备份。".into());
        }
        partitions::table_read_context(&request.source, Some((&ctx, &locks)))?;
        archive.matches(&mut reader, || check(cancel))?;
        let bytes = archive.encode()?;
        drop(reader);
        drop(locks);
        check(cancel)?;
        platform::destination_folder(&folder, &request.source.target, bytes.len() as u64)?;
        let partial = format!("DiskStudio-table-{}.partial", uuid::Uuid::new_v4());
        let mut options = CapOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
            .share_mode(0);
        let mut file = dir
            .open_with(&partial, &options)
            .map_err(platform::err)?
            .into_std();
        let saved = (|| -> Result<(), String> {
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(platform::err)?;
            file.seek(SeekFrom::Start(0)).map_err(platform::err)?;
            let mut verify = vec![0; bytes.len()];
            file.read_exact(&mut verify).map_err(platform::err)?;
            if hash(&verify) != hash(&bytes) {
                return Err("输出文件 SHA-256 回读不一致。".into());
            }
            table_archive::Archive::decode(&verify)?;
            check(cancel)?;
            crate::recovery_output::finish(&file, &dir, name)
        })();
        saved.map_err(|e| format!("{e}；未完成文件保留为 {}", folder.join(partial).display()))?;
        Ok(Some(Report {
            name: name.into(),
            info,
            file_sha256: hash(&bytes),
        }))
    }
    pub fn verify(cancel: &AtomicBool) -> Result<Option<Report>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(crate::native_locale::text("验证分区表备份"))
            .add_filter(
                crate::native_locale::text("DiskStudio 分区表备份"),
                &["dspt"],
            )
            .pick_file()
        else {
            return Ok(None);
        };
        check(cancel)?;
        let text = path.to_string_lossy();
        let p = text.as_bytes();
        if p.len() < 3
            || !p[0].is_ascii_alphabetic()
            || &p[1..3] != b":\\"
            || text[2..].contains(':')
        {
            return Err("请选择本地普通备份文件。".into());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(platform::err)?;
        let meta = file.metadata().map_err(platform::err)?;
        if !meta.is_file()
            || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || meta.len() > table_archive::MAX_FILE as u64
        {
            return Err("备份必须为大小受限的普通文件。".into());
        }
        let mut bytes = vec![0; meta.len() as usize];
        file.read_exact(&mut bytes).map_err(platform::err)?;
        let archive = table_archive::Archive::decode(&bytes)?;
        check(cancel)?;
        Ok(Some(Report {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            info: archive.validate()?,
            file_sha256: hash(&bytes),
        }))
    }
}

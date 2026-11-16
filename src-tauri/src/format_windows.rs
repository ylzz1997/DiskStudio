//! Post-format queries only; never invoked by inventory or preview.
//! Provider-derived volume GUIDs and retained handles are rechecked before
//! interpreting NTFS compression and per-volume short-name state.
use super::{
    format_options::FormatOptions, policy, volume_manage, Partition, Request, Session, Volume,
};
use crate::data_windows as device;
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
    System::Ioctl::{
        FSCTL_GET_COMPRESSION, FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
        PERSISTENT_VOLUME_STATE_SHORT_NAME_CREATION_DISABLED,
    },
};

fn compression(bytes: &[u8], attributes: u32, expected: bool) -> Result<(), String> {
    if bytes.len() != 2
        || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("NTFS 根目录压缩状态缺失或目录身份异常。".into());
    }
    let state = u16::from_le_bytes(bytes.try_into().unwrap());
    if !matches!(state, 0 | 2) || (attributes & FILE_ATTRIBUTE_COMPRESSED != 0) != (state == 2) {
        return Err("NTFS 根目录压缩算法或属性不一致。".into());
    }
    if (state == 2) != expected {
        return Err("格式化后的 NTFS 根目录压缩设置与确认选项不一致。".into());
    }
    Ok(())
}
fn short_names(bytes: &[u8], expected: bool) -> Result<(), String> {
    if bytes.len() != 16 {
        return Err("NTFS 卷的短文件名状态长度无效。".into());
    }
    let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mask = PERSISTENT_VOLUME_STATE_SHORT_NAME_CREATION_DISABLED;
    // Version and FlagMask are request fields, not a required response echo.
    // Server 2025 returns VolumeFlags with the remaining three words zero.
    // Accept that observed response and an explicit echo; reject other forms.
    let no_echo = word(4) == 0 && word(8) == 0;
    let echo = word(4) == mask && word(8) == 1;
    if word(12) != 0 || !(no_echo || echo) {
        return Err("NTFS 卷没有返回可确认的短文件名状态。".into());
    }
    if (word(0) & mask == 0) != expected {
        return Err("格式化后的 NTFS 卷短文件名设置与确认选项不一致。".into());
    }
    Ok(())
}
pub(super) fn verify(
    session: &Session,
    request: &Request,
    p: &Partition,
    v: &Volume,
    options: FormatOptions,
) -> Result<(), String> {
    let path = v.volume_path.as_deref().ok_or("格式化后的卷 GUID 缺失")?;
    let id = volume_manage::volume_key(path)?;
    let volume_path = format!("\\\\?\\Volume{{{}}}", &id[11..47]);
    let root = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(format!("{volume_path}\\"))
        .map_err(|e| format!("无法打开格式化后的根目录进行核对：{e}"))?;
    let d = session.disk(&request.target)?;
    let layout = crate::disk_identity::layout_key(
        d.partition_style,
        policy::layout(d, &session.partitions)?
            .iter()
            .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
            .collect(),
    );
    let location =
        device::file_location(&root).map_err(|e| format!("NTFS 根目录身份核对失败：{e}"))?;
    location.confirm_partition(
        &request.target,
        &layout,
        &volume_path,
        (d.number, p.offset, p.size),
    )?;
    let volume = OpenOptions::new()
        // A zero-access device handle can answer disk IOCTLs but does not
        // dispatch this NTFS filesystem query (ERROR_INVALID_FUNCTION).
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&volume_path)
        .map_err(|e| format!("无法打开 NTFS 只读核验卷：{e}"))?;
    crate::fat_volume::verify_extent(&volume, d, p)
        .map_err(|e| format!("NTFS 卷范围核对失败：{e}"))?;
    verify_handles(&root, &volume, options)?;
    crate::fat_volume::verify_extent(&volume, d, p)
        .map_err(|e| format!("NTFS 核验后卷范围检查失败：{e}"))?;
    if device::file_location(&root)? != location {
        return Err("核对格式化选项期间目录所在卷发生变化。".into());
    }
    Ok(())
}

/// Read-only settings queries on retained handles. The caller must verify the
/// root/volume identity and extent before and after this function.
pub(crate) fn verify_handles(
    root: &File,
    volume: &File,
    options: FormatOptions,
) -> Result<(), String> {
    let mut compressed = [0u8; 2];
    let n = device::control(root, FSCTL_GET_COMPRESSION, &[], &mut compressed)
        .map_err(|e| format!("NTFS 根目录压缩状态查询失败：{e}"))?;
    if n > compressed.len() {
        return Err("NTFS 压缩响应越界。".into());
    }
    compression(
        &compressed[..n],
        root.metadata().map_err(device::err)?.file_attributes(),
        options.ntfs_compression,
    )?;
    let mut input = [0u8; 16];
    input[4..8]
        .copy_from_slice(&PERSISTENT_VOLUME_STATE_SHORT_NAME_CREATION_DISABLED.to_le_bytes());
    input[8..12].copy_from_slice(&1u32.to_le_bytes());
    let mut result = [0u8; 16];
    let n = device::control(
        volume,
        FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
        &input,
        &mut result,
    )
    .map_err(|e| format!("NTFS 卷短文件名状态查询失败：{e}"))?;
    if n > result.len() {
        return Err("NTFS 短文件名响应越界。".into());
    }
    short_names(&result[..n], options.ntfs_short_names)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ntfs_settings_require_complete_consistent_native_responses() {
        assert!(compression(
            &[2, 0],
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_COMPRESSED,
            true
        )
        .is_ok());
        assert!(compression(&[0, 0], FILE_ATTRIBUTE_DIRECTORY, false).is_ok());
        for bytes in [vec![], vec![2], vec![1, 0], vec![3, 0], vec![2, 0, 0]] {
            assert!(compression(
                &bytes,
                FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_COMPRESSED,
                true
            )
            .is_err());
        }
        assert!(compression(&[2, 0], FILE_ATTRIBUTE_DIRECTORY, true).is_err());
        assert!(compression(&[0, 0], FILE_ATTRIBUTE_DIRECTORY, true).is_err());
        assert!(compression(
            &[2, 0],
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_REPARSE_POINT,
            true
        )
        .is_err());
        let mut state = [0u8; 16];
        state[4] = 1;
        state[8] = 1;
        assert!(short_names(&state, true).is_ok());
        assert!(short_names(&state, false).is_err());
        state[0] = 1;
        assert!(short_names(&state, false).is_ok());
        assert!(short_names(&state[..15], false).is_err());
        for at in [4, 8, 12] {
            let mut bad = state;
            bad[at] ^= 1;
            assert!(short_names(&bad, false).is_err());
        }
    }
    #[test]
    fn accepts_observed_ntfs_reply_without_request_field_echo() {
        // Captured by read-only VM diagnosis on the exact failed B fixture.
        let mut state = [0u8; 16];
        state[0] = 1;
        assert!(short_names(&state, false).is_ok());
        assert!(short_names(&state, true).is_err());
        state[0] = 0;
        assert!(short_names(&state, true).is_ok());
        assert!(short_names(&state, false).is_err());
        for len in 0..16 {
            assert!(short_names(&state[..len], true).is_err());
        }
        for at in [4, 8, 12] {
            let mut unknown = state;
            unknown[at] = 2;
            assert!(short_names(&unknown, true).is_err());
        }
    }
}

//! Read-only BitLocker identification and Windows provider metadata.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const CIPHERTEXT_NOTICE: &str =
    "检测到 BitLocker 加密源。请在 Windows 解锁已登记的卷后，从物理分区读取文件。";

pub(crate) fn boot_marker(boot: &[u8]) -> bool {
    boot.get(3..11) == Some(b"-FVE-FS-")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum State {
    Encrypted,
    Encrypting,
    Decrypting,
    EncryptionPaused,
    DecryptionPaused,
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Win32_EncryptableVolume", rename_all = "PascalCase")]
pub(crate) struct Volume {
    #[serde(rename = "__PATH")]
    pub(crate) path: Option<String>,
    #[serde(rename = "DeviceID")]
    device_id: Option<String>,
    #[serde(rename = "PersistentVolumeID")]
    persistent_volume_id: Option<String>,
    conversion_status: Option<u32>,
}

impl Volume {
    pub(crate) fn state(&self) -> Option<State> {
        match self.conversion_status {
            Some(0) => None,
            Some(1) => Some(State::Encrypted),
            Some(2) => Some(State::Encrypting),
            Some(3) => Some(State::Decrypting),
            Some(4) => Some(State::EncryptionPaused),
            Some(5) => Some(State::DecryptionPaused),
            _ if self
                .persistent_volume_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty()) => Some(State::Unknown),
            _ => None,
        }
    }
}

// Only canonical volume identities are compared. Never compare by drive letter
// or use identifiers returned by this provider as raw-device paths.
pub(crate) fn volume_id(value: &str) -> Option<Uuid> {
    let value = value.strip_prefix(r"\\?\")?;
    let value = value.strip_suffix('\\').unwrap_or(value);
    if value.len() != 44 || !value.get(..7)?.eq_ignore_ascii_case("Volume{") {
        return None;
    }
    let id = Uuid::parse_str(value.strip_suffix('}')?.get(7..)?).ok()?;
    (!id.is_nil()).then_some(id)
}

pub(crate) fn associated<'a>(all: &'a [Volume], device: &str) -> Option<&'a Volume> {
    let want = volume_id(device)?;
    let mut rows = all
        .iter()
        .filter(|row| row.device_id.as_deref().and_then(volume_id) == Some(want));
    let found = rows.next()?;
    rows.next().is_none().then_some(found)
}

#[cfg(windows)]
pub(crate) fn query() -> Result<(wmi::WMIConnection, Vec<Volume>), String> {
    const NAMESPACE: &str = r"ROOT\CIMV2\Security\MicrosoftVolumeEncryption";
    const QUERY: &str = "SELECT __PATH,DeviceID,PersistentVolumeID,ConversionStatus FROM Win32_EncryptableVolume";
    let connect = wmi::WMIConnection::with_namespace_path(NAMESPACE)
        .map_err(|_| "Windows BitLocker 状态接口不可用。")?;
    let rows: Vec<Volume> = connect
        .raw_query(QUERY)
        .map_err(|_| "Windows BitLocker 状态无法读取。")?;
    if rows.len() > 4096 {
        return Err("Windows 返回的 BitLocker 卷数量超出范围。".into());
    }
    Ok((connect, rows))
}

#[cfg(windows)]
pub(crate) fn require_unlocked(device: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Access {
        return_value: u32,
        lock_status: Option<u32>,
    }
    let (connect, rows) = query()?;
    let found = associated(&rows, device).ok_or("Windows 无法唯一核对该 BitLocker 卷。")?;
    let path = found.path.as_deref().ok_or("BitLocker 卷缺少 Windows 对象路径。")?;
    if found.state().is_none()
        || rows.iter()
            .filter(|row| row.path.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(path)))
            .count() != 1
    {
        return Err("Windows 无法唯一核对该 BitLocker 卷。".into());
    }
    let result: Access = connect
        .exec_instance_method::<Volume, _>(path, "GetLockStatus", ())
        .map_err(|_| "Windows 无法验证 BitLocker 解锁状态。")?;
    match (result.return_value, result.lock_status) {
        (0, Some(0)) if found.state() == Some(State::Encrypted) => Ok(()),
        (0, Some(0)) => Err("当前只读取已完成加密的 BitLocker 卷。请等待加密或解密完成。".into()),
        (0, Some(1)) => Err("BitLocker 卷已锁定。请在 Windows 解锁后重新读取。".into()),
        _ => Err("Windows 无法验证 BitLocker 解锁状态。".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_requires_a_unique_exact_volume_id_without_inferring_lock_status() {
        let name = r"\\?\Volume{bb824775-90ae-4db9-9754-59243b252bc5}";
        let row = |device: &str, conversion_status, persistent_volume_id: Option<&str>| Volume {
            path: None,
            device_id: Some(device.into()),
            conversion_status,
            persistent_volume_id: persistent_volume_id.map(str::to_owned),
        };
        for (code, expected) in [
            (0, None),
            (1, Some(State::Encrypted)),
            (2, Some(State::Encrypting)),
            (3, Some(State::Decrypting)),
            (4, Some(State::EncryptionPaused)),
            (5, Some(State::DecryptionPaused)),
        ] {
            let key = format!("{}\\", name.to_uppercase());
            let all = [row(&key, Some(code), Some("identifier"))];
            assert_eq!(associated(&all, name).and_then(Volume::state), expected);
        }
        assert!(associated(&[row(name, Some(1), None), row(name, Some(0), None)], name).is_none());
        assert!(associated(&[row("D:", Some(1), Some("fake"))], name).is_none());
        assert_eq!(row(name, None, Some("identifier")).state(), Some(State::Unknown));
        assert_eq!(row(name, None, None).state(), None);
    }
}

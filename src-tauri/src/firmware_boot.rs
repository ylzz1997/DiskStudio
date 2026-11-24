use serde::{Deserialize, Serialize};

#[cfg(any(windows, test))]
mod archive;
#[cfg(any(windows, test))]
mod catalog;
#[cfg(windows)]
mod loader;
pub(crate) mod loader_security;
#[cfg(any(windows, test))]
mod model;
pub(crate) mod secure_boot;
#[cfg(windows)]
mod windows;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    pub revision: String,
    pub order: Vec<u16>,
    pub current: Option<u16>,
    pub next: Option<u16>,
    pub setup_next: Option<bool>,
    pub all_entries: bool,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub id: u16,
    pub status: EntryStatus,
    pub description: Option<String>,
    pub active: Option<bool>,
    pub boot_category: bool,
    pub file_paths: Vec<String>,
    pub gpt_partitions: Vec<uuid::Uuid>,
    pub parameters_bytes: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum EntryStatus {
    Ready,
    Missing,
    Invalid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub revision: String,
    pub change: Change,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Change {
    Order {
        entries: Vec<u16>,
    },
    Once {
        entry: u16,
    },
    ClearOnce,
    Active {
        entry: u16,
        active: bool,
    },
    Rename {
        entry: u16,
        description: String,
    },
    Create {
        candidate: uuid::Uuid,
        description: String,
    },
    Remove {
        entry: u16,
    },
    Attach {
        entry: u16,
    },
    Retarget {
        entry: u16,
        candidate: uuid::Uuid,
        #[serde(rename = "preserveParameters")]
        preserve_parameters: bool,
    },
    Restore,
    Setup {
        next: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub token: String,
    pub before: Report,
    pub after: Report,
    pub change: Change,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Loader {
    pub id: uuid::Uuid,
    pub path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoaderVerification {
    pub sha256: String,
    pub partition: uuid::Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupRequest {
    pub revision: String,
    pub entry: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupResult {
    pub name: String,
    pub sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WinPeRequest {
    pub revision: String,
    pub source: crate::data_tasks::Selection,
    #[cfg(windows)]
    pub task: Option<crate::winpe_handoff::Request>,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WinPeMedia {
    pub partition: uuid::Uuid,
    pub wim_bytes: u64,
    pub wim_image: u32,
    pub wim_sha256: String,
    pub app_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WinPePreview {
    pub token: String,
    pub entry: u16,
    pub creates_entry: bool,
    pub media: WinPeMedia,
    #[cfg(windows)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<crate::winpe_handoff::Preview>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WinPeRestart {
    pub report: Report,
    pub error: Option<String>,
}

pub(crate) fn winpe_preview(request: WinPeRequest) -> Result<Option<WinPePreview>, String> {
    #[cfg(windows)]
    return windows::winpe_preview(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("重启到 WinPE 需要 Windows UEFI。".into())
    }
}

#[cfg(windows)]
pub(crate) fn claim_winpe_task() -> Result<Option<crate::winpe_handoff::Task>, String> {
    windows::claim_winpe_task()
}

pub(crate) fn winpe_restart(token: String) -> Result<WinPeRestart, String> {
    #[cfg(windows)]
    return windows::winpe_restart(token);
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("重启到 WinPE 需要 Windows UEFI。".into())
    }
}

pub(crate) fn inspect() -> Result<Report, String> {
    #[cfg(windows)]
    return windows::inspect();
    #[cfg(not(windows))]
    Err("固件启动项管理需要 Windows UEFI。".into())
}

pub(crate) fn inspect_secure_boot() -> Result<secure_boot::Report, String> {
    #[cfg(windows)]
    return windows::inspect_secure_boot();
    #[cfg(not(windows))]
    Err("固件启动项管理需要 Windows UEFI。".into())
}

pub(crate) fn preview(request: Request) -> Result<Option<Preview>, String> {
    #[cfg(windows)]
    return windows::preview(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("固件启动项管理需要 Windows UEFI。".into())
    }
}

pub(crate) fn list_loaders(source: crate::data_tasks::Selection) -> Result<Vec<Loader>, String> {
    #[cfg(windows)]
    return loader::list(source);
    #[cfg(not(windows))]
    {
        let _ = source;
        Err("固件启动项管理需要 Windows UEFI。".into())
    }
}

pub(crate) fn verify_loader(id: uuid::Uuid) -> Result<LoaderVerification, String> {
    #[cfg(windows)]
    return loader::candidate(id)?.verification();
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("EFI 文件核对需要 Windows。".into())
    }
}

pub(crate) fn inspect_loader_security(id: uuid::Uuid) -> Result<loader_security::Report, String> {
    #[cfg(windows)]
    {
        let (bytes, partition) = loader::candidate(id)?.security_source()?;
        loader_security::inspect(&bytes, partition, windows::inspect_secure_boot)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("EFI 文件核对需要 Windows。".into())
    }
}

pub(crate) fn backup(request: BackupRequest) -> Result<Option<BackupResult>, String> {
    #[cfg(windows)]
    return windows::backup(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("固件启动项管理需要 Windows UEFI。".into())
    }
}

pub(crate) fn apply(token: String) -> Result<Report, String> {
    #[cfg(windows)]
    return windows::apply(token);
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("固件启动项管理需要 Windows UEFI。".into())
    }
}

pub(crate) fn discard(token: String) {
    #[cfg(windows)]
    windows::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

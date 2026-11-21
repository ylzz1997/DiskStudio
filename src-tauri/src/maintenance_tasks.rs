//! Explicit maintenance requests; polling only reads retained memory.
use crate::{
    data_tasks::Selection,
    maintenance_io::{BenchmarkOptions, BenchmarkProgress, WipeOptions, WipeProgress},
    surface_scan::Options,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Operation {
    Wipe { options: WipeOptions },
    WipeFree { options: WipeOptions },
    Benchmark { options: BenchmarkOptions },
    Trim,
}
impl Operation {
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::Wipe { .. }
                | Self::WipeFree { .. }
                | Self::Benchmark {
                    options: BenchmarkOptions { write: true, .. }
                }
        )
    }
    pub fn title(&self) -> &'static str {
        match self {
            Self::Wipe { .. } => "清除扇区",
            Self::WipeFree { .. } => "清除空闲空间",
            Self::Benchmark { options } => {
                if options.read_percent.is_some() {
                    "混合读写测速"
                } else if options.write {
                    "写入测速"
                } else {
                    "读取测速"
                }
            }
            Self::Trim => "TRIM 空闲空间",
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub range: Options,
    pub operation: Operation,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub title: String,
    pub target: String,
    pub offset: u64,
    pub size: u64,
    pub sector_size: u64,
    pub destructive: bool,
    pub warning: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub id: String,
    pub source: Selection,
    pub title: String,
    pub target: String,
    pub operation: Operation,
    pub offset: u64,
    pub size: u64,
    pub state: String,
    pub phase: String,
    pub message: String,
    pub started_at: u64,
    pub wipe: Option<WipeProgress>,
    pub free_space: Option<crate::free_space_wipe::Summary>,
    pub benchmark: Option<BenchmarkProgress>,
    pub disk_left_offline: bool,
}
pub(crate) static REPORT: Mutex<Option<Report>> = Mutex::new(None);
pub fn status() -> Option<Report> {
    REPORT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
pub(crate) fn publish(report: &Report) {
    *REPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(report.clone());
}

pub(crate) struct Ticket<T> {
    pub token: String,
    pub expires: Instant,
    pub data: T,
}
impl<T> Ticket<T> {
    pub fn new(token: String, data: T) -> Self {
        Self {
            token,
            expires: Instant::now() + Duration::from_secs(300),
            data,
        }
    }
}
pub(crate) fn consume<T>(
    state: &mut Option<Ticket<T>>,
    token: &str,
    now: Instant,
) -> Result<T, String> {
    let item = state.take().ok_or("确认已失效，请重新预览。")?;
    if item.token != token || now >= item.expires {
        return Err("确认不匹配或已过期。".into());
    }
    Ok(item.data)
}
pub fn prepare(request: Request) -> Result<Preview, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::maintenance::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("磁盘维护目前仅支持 Windows。".into())
    }
}
pub fn execute(token: String) -> Result<Report, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::maintenance::execute(token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("磁盘维护目前仅支持 Windows。".into())
    }
}
pub fn discard(token: String) {
    #[cfg(windows)]
    {
        crate::partitions::platform::maintenance::discard(&token);
    }
    #[cfg(not(windows))]
    {
        let _ = token;
    }
}
pub fn export(id: String) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        crate::partitions::platform::maintenance::export(&id)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("报告导出目前仅支持 Windows。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tickets_consume_once_on_success_mismatch_and_expiry() {
        for (token, expired, ok) in [
            ("once", false, true),
            ("wrong", false, false),
            ("once", true, false),
        ] {
            let ticket = Ticket::new("once".into(), 42);
            let now = if expired {
                ticket.expires
            } else {
                Instant::now()
            };
            let mut state = Some(ticket);
            assert_eq!(consume(&mut state, token, now).is_ok(), ok);
            assert!(state.is_none());
            assert!(consume(&mut state, "once", now).is_err());
        }
    }
    #[test]
    fn ipc_is_typed_and_rejects_device_paths_methods_and_extra_options() {
        let good = serde_json::json!({"source":{"target":"a".repeat(64),"layoutKey":"fixture","partition":2},"range":{"startSector":0,"sectorCount":8},"operation":{"kind":"wipe","options":{"passes":1,"pattern":{"kind":"bytes","hex":"00"}}}});
        assert!(serde_json::from_value::<Request>(good.clone()).is_ok());
        for key in ["path", "method", "wql", "device"] {
            let mut bad = good.clone();
            bad[key] = serde_json::json!("arbitrary");
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
        let mut bad = good;
        bad["operation"]["options"]["verify"] = serde_json::json!(false);
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }
    #[test]
    fn free_wipe_is_destructive_and_rejects_frontend_free_ranges_or_disabled_verification() {
        let good = serde_json::json!({"kind":"wipeFree","options":{"passes":1,"pattern":{"kind":"bytes","hex":"00"}}});
        let op = serde_json::from_value::<Operation>(good.clone()).unwrap();
        assert!(op.destructive());
        for field in ["ranges", "free", "allocation", "path", "verify"] {
            let mut bad = good.clone();
            bad[field] = serde_json::json!(false);
            assert!(serde_json::from_value::<Operation>(bad).is_err());
        }
    }
}

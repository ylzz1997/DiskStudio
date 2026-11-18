//! Typed entry points for explicit partition move and resume jobs.
use crate::data_tasks::Selection;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub source: Selection,
    pub destination: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub target: String,
    pub source_offset: u64,
    pub destination_offset: u64,
    pub size: u64,
    pub overlap: bool,
    pub journal_name: String,
    pub resuming: bool,
    pub copied_bytes: u64,
    pub completed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub message: String,
    pub sha256: String,
}

#[cfg(windows)]
#[path = "partition_move_windows.rs"]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{move_for_batch, JournalLocation};

pub fn prepare(request: Request) -> Result<Option<Preview>, String> {
    #[cfg(windows)]
    {
        windows::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("分区移动当前仅支持 Windows。".into())
    }
}

pub fn resume(source: Selection) -> Result<Option<Preview>, String> {
    #[cfg(windows)]
    {
        windows::resume(source)
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        Err("分区移动当前仅支持 Windows。".into())
    }
}

pub fn execute(token: String) -> Result<Outcome, String> {
    #[cfg(windows)]
    {
        windows::execute(token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Err("分区移动当前仅支持 Windows。".into())
    }
}

pub fn discard(token: String) {
    #[cfg(windows)]
    windows::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

#[cfg(test)]
#[test]
fn move_ipc_accepts_only_the_bound_selection_and_destination() {
    let valid = serde_json::json!({"source": {"target": "a".repeat(64), "layoutKey": "known", "partition": 3}, "destination": 2097152});
    assert!(serde_json::from_value::<Request>(valid.clone()).is_ok());
    for key in ["path", "journal", "wql", "method", "device"] {
        let mut rejected = valid.clone();
        rejected[key] = "unexpected".into();
        assert!(serde_json::from_value::<Request>(rejected).is_err());
    }
}

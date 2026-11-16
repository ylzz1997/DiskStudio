use crate::{
    data_tasks::{self, Selection},
    health_data::Health,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub target: crate::partitions::Target,
    pub layout_key: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub observed_at: u64,
    pub health: Health,
}

pub fn read(request: Request) -> Result<Report, String> {
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("健康信息读取当前仅支持 Windows。".into())
    }
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行。")?;
        let (id, _) = data_tasks::start("读取磁盘健康信息")?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let health = crate::health_windows::read(&Selection {
                target: request.target,
                layout_key: request.layout_key,
                partition: None,
            })?;
            Ok(Report {
                observed_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_secs(),
                health,
            })
        }))
        .unwrap_or_else(|_| Err("健康信息读取异常结束。".into()));
        data_tasks::finish(
            &id,
            result
                .as_ref()
                .map(|_| ("健康信息读取完成".into(), None))
                .map_err(Clone::clone),
        );
        result
    }
}

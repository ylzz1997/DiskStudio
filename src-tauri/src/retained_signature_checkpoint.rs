//! Portable signature boundaries for a retained image or a verified backup point.
use super::Kind;
use crate::{
    recovery::Candidate,
    signature_checkpoint::{hash, hex, restore_records, Binding, MAX_FILE},
    signature_scan::{Checkpoint, Options, StoredCandidate},
};
use serde::{Deserialize, Serialize};

const FORMAT: &str = "DiskStudio retained signature progress";
const VERSION: u32 = 6;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Progress {
    pub kind: Kind,
    pub partition: Option<u32>,
    pub source_digest: String,
    pub binding: Binding,
    pub options: Options,
    pub cursor: Checkpoint,
    pub records: Vec<StoredCandidate>,
}

impl Progress {
    pub fn validate(&self) -> Result<Vec<Candidate>, String> {
        if !hex(&self.source_digest) || (self.options.free_only && self.partition.is_none()) {
            return Err("镜像或备份的恢复进度来源无效。".into());
        }
        restore_records(&self.binding, &self.options, &self.cursor, &self.records)
    }

    pub fn verify_source(
        &self,
        kind: Kind,
        partition: Option<u32>,
        digest: &str,
    ) -> Result<(), String> {
        if self.kind != kind || self.partition != partition || self.source_digest != digest {
            return Err("请选择进度对应的原镜像、备份还原点和扫描范围。".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    version: u32,
    sha256: String,
    progress: Progress,
}

pub(super) fn encode(progress: Progress) -> Result<Vec<u8>, String> {
    progress.validate()?;
    let bytes = serde_json::to_vec(&Envelope {
        format: FORMAT.into(),
        version: VERSION,
        sha256: hash(&serde_json::to_vec(&progress).map_err(|error| error.to_string())?),
        progress,
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("恢复进度超过 64 MiB 保存上限。".into());
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<(Progress, Vec<Candidate>), String> {
    if bytes.len() as u64 > MAX_FILE {
        return Err("恢复进度文件超过 64 MiB 上限。".into());
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| "恢复进度文件格式无效")?;
    if envelope.format != FORMAT
        || envelope.version != VERSION
        || envelope.sha256
            != hash(&serde_json::to_vec(&envelope.progress).map_err(|e| e.to_string())?)
    {
        return Err("恢复进度版本不支持或摘要不一致。".into());
    }
    let files = envelope.progress.validate()?;
    Ok((envelope.progress, files))
}

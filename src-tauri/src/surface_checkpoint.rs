//! Portable progress, never executable input. The digest detects accidental
//! corruption; imported results remain historical observations, not attestation.
use crate::{
    data_tasks::Selection, partitions::Target, surface_scan::Options, surface_tasks::Snapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub const MAX_FILE: u64 = 2 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Progress {
    pub source: Selection,
    pub options: Options,
    pub snapshot: Snapshot,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    format: String,
    version: u32,
    sha256: String,
    progress: Progress,
}
impl Progress {
    pub fn validate(&self, selected: &Target) -> Result<(), String> {
        let s = &self.snapshot;
        if &self.source.target != selected
            || s.target != selected.0
            || self.source.layout_key.len() != 64
            || !self
                .source
                .layout_key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.source.partition == Some(0)
            || s.partition != self.source.partition
            || s.options != self.options
            || s.source.len() > 1024
            || s.source.chars().any(char::is_control)
            || uuid::Uuid::parse_str(&s.id).is_err()
            || s.resume_count > 1024
            || s.started_at > 32_503_680_000
            || s.report.state == "running"
        {
            return Err("进度文件与所选磁盘不匹配，或记录内容无效。".into());
        }
        s.report.validate()?;
        if self.options.sector_count.checked_mul(s.report.sector_size) != Some(s.report.size) {
            return Err("进度中的范围容量不一致。".into());
        }
        let relative = self
            .options
            .start_sector
            .checked_mul(s.report.sector_size)
            .ok_or("进度起点溢出")?;
        if relative > s.report.offset
            || (self.source.partition.is_none() && relative != s.report.offset)
        {
            return Err("进度的相对与绝对位置不一致。".into());
        }
        Ok(())
    }
}
pub fn encode(progress: Progress) -> Result<Vec<u8>, String> {
    progress.validate(&progress.source.target)?;
    let canonical = serde_json::to_vec(&progress).map_err(|e| e.to_string())?;
    let envelope = Envelope {
        format: "DiskStudio surface progress".into(),
        version: 1,
        sha256: format!("{:x}", Sha256::digest(&canonical)),
        progress,
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("检测进度超过文件大小上限。".into());
    }
    Ok(bytes)
}
pub fn decode(bytes: &[u8], target: &Target) -> Result<Progress, String> {
    if bytes.len() as u64 > MAX_FILE {
        return Err("检测进度文件超过 2 MiB 上限。".into());
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| "检测进度格式无效。")?;
    if envelope.version != 1 || envelope.format != "DiskStudio surface progress" {
        return Err("不支持此检测进度版本。".into());
    }
    let canonical = serde_json::to_vec(&envelope.progress).map_err(|e| e.to_string())?;
    if envelope.sha256 != format!("{:x}", Sha256::digest(&canonical)) {
        return Err("检测进度校验失败，文件可能已损坏。".into());
    }
    envelope.progress.validate(target)?;
    Ok(envelope.progress)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface_scan::{Range, Refinement, Report, TimedOutRead, BLOCK};
    fn sample() -> Progress {
        let target = Target("a".repeat(64));
        let options = Options {
            start_sector: 0,
            sector_count: BLOCK * 3 / 512,
        };
        let report = Report::new(
            Range {
                offset: 0,
                size: BLOCK * 3,
                sector: 512,
            },
            false,
        );
        let mut p = Progress {
            source: Selection {
                target: target.clone(),
                layout_key: "b".repeat(64),
                partition: None,
            },
            options,
            snapshot: Snapshot {
                id: uuid::Uuid::new_v4().to_string(),
                target: target.0,
                source: "Synthetic disk".into(),
                started_at: 1,
                report,
                partition: None,
                options,
                loaded: false,
                resume_count: 0,
            },
        };
        p.snapshot.report.state = "paused".into();
        p
    }
    #[test]
    fn roundtrip_is_bound_to_target_and_detects_corruption() {
        let p = sample();
        let bytes = encode(p.clone()).unwrap();
        assert_eq!(
            decode(&bytes, &p.source.target)
                .unwrap()
                .snapshot
                .report
                .state,
            "paused"
        );
        assert!(decode(&bytes, &Target("c".repeat(64))).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["progress"]["snapshot"]["report"]["checkedBytes"] = 512.into();
        assert!(decode(&serde_json::to_vec(&value).unwrap(), &p.source.target).is_err());
    }
    #[test]
    fn legacy_progress_keeps_its_shape_and_refined_progress_retains_its_mode() {
        let mut p = sample();
        let legacy = encode(p.clone()).unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
        assert!(envelope["progress"]["snapshot"]["report"]
            .get("refinement")
            .is_none());
        assert!(decode(&legacy, &p.source.target)
            .unwrap()
            .snapshot
            .report
            .refinement
            .is_none());

        p.snapshot.report.refinement = Some(Refinement {
            reads: 1,
            ..Refinement::default()
        });
        let bytes = encode(p.clone()).unwrap();
        let restored = decode(&bytes, &p.source.target).unwrap();
        assert_eq!(restored.snapshot.report.refinement.unwrap().reads, 1);
        p.snapshot
            .report
            .refinement
            .as_mut()
            .unwrap()
            .failed_sectors = 1;
        assert!(encode(p).is_err());
    }
    #[test]
    fn valid_digest_does_not_bypass_structural_validation() {
        let mut p = sample();
        p.snapshot.report.checked_bytes = 512;
        assert!(encode(p).is_err());
        let mut p = sample();
        p.snapshot.partition = Some(9);
        assert!(encode(p).is_err());
        let mut p = sample();
        p.options.start_sector = 1;
        assert!(encode(p).is_err());
        let mut p = sample();
        p.source.layout_key = "invalid".into();
        assert!(encode(p).is_err());
    }
    #[test]
    fn huge_unknown_and_newer_formats_are_rejected() {
        let p = sample();
        let bytes = encode(p.clone()).unwrap();
        assert!(decode(&vec![b' '; MAX_FILE as usize + 1], &p.source.target).is_err());
        let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["devicePath"] = "untrusted".into();
        assert!(decode(&serde_json::to_vec(&v).unwrap(), &p.source.target).is_err());
        v.as_object_mut().unwrap().remove("devicePath");
        v["version"] = 2.into();
        assert!(decode(&serde_json::to_vec(&v).unwrap(), &p.source.target).is_err());
    }

    #[test]
    fn timeout_checkpoint_requires_a_valid_limit_and_an_unfinished_transfer() {
        let mut p = sample();
        p.snapshot.report.state = "timedOut".into();
        p.snapshot.report.read_timeout_seconds = Some(1);
        p.snapshot.report.timed_out_read = Some(TimedOutRead {
            start_sector: 7,
            sector_count: 8,
        });
        let decoded = decode(&encode(p.clone()).unwrap(), &p.source.target).unwrap();
        assert_eq!(decoded.snapshot.report.read_timeout_seconds, Some(1));
        assert_eq!(
            decoded.snapshot.report.timed_out_read.unwrap().start_sector,
            7
        );
        for illegal in [None, Some(0), Some(121)] {
            let mut invalid = p.clone();
            invalid.snapshot.report.read_timeout_seconds = illegal;
            assert!(encode(invalid).is_err());
        }
        p.snapshot
            .report
            .timed_out_read
            .as_mut()
            .unwrap()
            .start_sector = BLOCK / 512;
        assert!(encode(p).is_err());
    }
}

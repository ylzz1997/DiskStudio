//! Bounded, portable carving progress. No raw path, filename, hardware serial
//! or executable input is persisted. Digests detect corruption, not forgery.
use crate::{
    data_tasks::Selection,
    recovery::Candidate,
    signature_scan::{Checkpoint, Options, StoredCandidate},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
pub const MAX_FILE: u64 = 64 * 1024 * 1024;
const ANCHOR: u64 = 64 * 1024;
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub size: u64,
    pub offset: u64,
    pub sector: u64,
    hashes: Vec<String>,
}
fn anchors(size: u64) -> Result<Vec<(u64, usize)>, String> {
    if !(512..=1 << 43).contains(&size) {
        return Err("扫描源容量不受支持。".into());
    }
    let n = size.min(ANCHOR);
    let mut positions = vec![0, ((size - n) / 2 / 512) * 512, size - n];
    positions.sort_unstable();
    positions.dedup();
    Ok(positions.into_iter().map(|at| (at, n as usize)).collect())
}
fn sample(reader: &mut (impl Read + Seek), size: u64) -> Result<Vec<String>, String> {
    anchors(size)?
        .into_iter()
        .map(|(at, n)| {
            reader
                .seek(SeekFrom::Start(at))
                .map_err(|e| e.to_string())?;
            let mut bytes = vec![0; n];
            reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            Ok(hash(&bytes))
        })
        .collect()
}
impl Binding {
    pub fn capture(
        reader: &mut (impl Read + Seek),
        size: u64,
        offset: u64,
        sector: u64,
    ) -> Result<Self, String> {
        let result = Self {
            size,
            offset,
            sector,
            hashes: sample(reader, size)?,
        };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !matches!(self.sector, 512 | 4096)
            || self.size % self.sector != 0
            || self.offset % self.sector != 0
            || self
                .offset
                .checked_add(self.size)
                .is_none_or(|end| end > 1 << 43)
            || self.hashes.len() != anchors(self.size)?.len()
            || self.hashes.iter().any(|h| !hex(h))
        {
            return Err("进度源几何或内容特征无效。".into());
        }
        Ok(())
    }
    pub fn verify(
        &self,
        reader: &mut (impl Read + Seek),
        size: u64,
        offset: u64,
        sector: u64,
    ) -> Result<(), String> {
        self.validate()?;
        if (self.size, self.offset, self.sector) != (size, offset, sector)
            || sample(reader, size)? != self.hashes
        {
            return Err("源范围或内容特征与保存时不同，请重新扫描。".into());
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Progress {
    pub source: Selection,
    pub options: Options,
    pub binding: Binding,
    pub cursor: Checkpoint,
    pub records: Vec<StoredCandidate>,
}
impl Progress {
    pub fn validate(&self, selected: &Selection) -> Result<Vec<Candidate>, String> {
        if self.source.target != selected.target
            || self.source.layout_key != selected.layout_key
            || self.source.partition != selected.partition
            || !hex(&self.source.target.0)
            || !hex(&self.source.layout_key)
            || self.source.partition == Some(0)
            || self.records.len() > crate::recovery::MAX_RESULTS
            || self.records.len() != self.cursor.count as usize
        {
            return Err("恢复进度与所选磁盘、分区或布局不匹配。".into());
        }
        self.binding.validate()?;
        let range = self.options.range(self.binding.size)?;
        self.cursor
            .validate(range.end - range.start, &self.options)?;
        self.records
            .iter()
            .enumerate()
            .map(|(id, r)| r.candidate(id as u32, range.clone(), &self.options))
            .collect()
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
#[cfg(test)]
pub fn encode(progress: Progress) -> Result<Vec<u8>, String> {
    progress.validate(&progress.source)?;
    let canonical = serde_json::to_vec(&progress).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&Envelope {
        format: "DiskStudio signature progress".into(),
        version: 1,
        sha256: hash(&canonical),
        progress,
    })
    .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("恢复进度超过 64 MiB 保存上限。".into());
    }
    Ok(bytes)
}
pub fn decode(bytes: &[u8], selected: &Selection) -> Result<Progress, String> {
    if bytes.len() as u64 > MAX_FILE {
        return Err("恢复进度文件超过 64 MiB 上限。".into());
    }
    let e: Envelope = serde_json::from_slice(bytes).map_err(|_| "恢复进度文件格式无效")?;
    if e.format != "DiskStudio signature progress"
        || e.version != 1
        || e.sha256 != hash(&serde_json::to_vec(&e.progress).map_err(|e| e.to_string())?)
    {
        return Err("恢复进度版本不支持或摘要不一致。".into());
    }
    e.progress.validate(selected)?;
    Ok(e.progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn fixture() -> (Vec<u8>, Progress) {
        let mut data = vec![0; 3 * 1024 * 1024];
        data[37..42].copy_from_slice(b"%PDF-");
        data[100..105].copy_from_slice(b"%%EOF");
        let source = Selection {
            target: crate::partitions::Target("a".repeat(64)),
            layout_key: "b".repeat(64),
            partition: Some(9),
        };
        let options = Options::default();
        let mut cursor = Checkpoint::default();
        let mut records = vec![];
        let _ = crate::signature_scan::continue_scan(
            &mut Cursor::new(&data),
            data.len() as u64,
            &options,
            &mut cursor,
            |d, _| {
                if d >= 1024 * 1024 {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |c| records.push(StoredCandidate::from_candidate(&c).unwrap()),
        );
        let binding =
            Binding::capture(&mut Cursor::new(&data), data.len() as u64, 4096, 512).unwrap();
        (
            data,
            Progress {
                source,
                options,
                binding,
                cursor,
                records,
            },
        )
    }
    #[test]
    fn progress_roundtrip_restores_candidates_and_resumes_without_duplicates() {
        let (data, p) = fixture();
        let bytes = encode(p.clone()).unwrap();
        let mut loaded = decode(&bytes, &p.source).unwrap();
        loaded
            .binding
            .verify(&mut Cursor::new(&data), data.len() as u64, 4096, 512)
            .unwrap();
        loaded
            .cursor
            .verify_cursor(&mut Cursor::new(&data), &loaded.options)
            .unwrap();
        let mut results = loaded.validate(&p.source).unwrap();
        assert_eq!(results.len(), 1);
        crate::signature_scan::continue_scan(
            &mut Cursor::new(&data),
            data.len() as u64,
            &loaded.options,
            &mut loaded.cursor,
            |_, _| Ok(()),
            |c| results.push(c),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(loaded.cursor.sealed);
        assert!(!String::from_utf8(bytes).unwrap().contains("PhysicalDrive"));
    }
    #[test]
    fn changed_scope_corrupt_digest_unknown_fields_and_forged_bounds_fail_closed() {
        let (_, p) = fixture();
        let bytes = encode(p.clone()).unwrap();
        let mut other = p.source.clone();
        other.target.0 = "c".repeat(64);
        assert!(decode(&bytes, &other).is_err());
        other = p.source.clone();
        other.layout_key = "d".repeat(64);
        assert!(decode(&bytes, &other).is_err());
        other = p.source.clone();
        other.partition = None;
        assert!(decode(&bytes, &other).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["sha256"] = "0".repeat(64).into();
        assert!(decode(&serde_json::to_vec(&value).unwrap(), &p.source).is_err());
        value = serde_json::from_slice(&bytes).unwrap();
        value["progress"]["path"] = "C:\\arbitrary".into();
        assert!(decode(&serde_json::to_vec(&value).unwrap(), &p.source).is_err());
        for field in ["consumed", "searched", "count", "archiveRemaining"] {
            let mut value = serde_json::to_value(&p).unwrap();
            value["cursor"][field] = u64::MAX.into();
            let bad: Progress = match serde_json::from_value(value) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let e = Envelope {
                format: "DiskStudio signature progress".into(),
                version: 1,
                sha256: hash(&serde_json::to_vec(&bad).unwrap()),
                progress: bad,
            };
            assert!(decode(&serde_json::to_vec(&e).unwrap(), &p.source).is_err());
        }
        let mut value = serde_json::to_value(&p).unwrap();
        value["records"][0]["offset"] = p.binding.size.into();
        let bad: Progress = serde_json::from_value(value).unwrap();
        assert!(encode(bad).is_err());
        let mut value = serde_json::to_value(&p).unwrap();
        value["cursor"]["sealed"] = true.into();
        value["cursor"]["consumed"] = 0.into();
        let bad: Progress = serde_json::from_value(value).unwrap();
        assert!(bad.validate(&p.source).is_err());
    }
    #[test]
    fn binding_checks_geometry_and_each_sample_but_does_not_claim_full_content_proof() {
        let (data, p) = fixture();
        for (at, _) in anchors(data.len() as u64).unwrap() {
            let mut changed = data.clone();
            changed[at as usize] ^= 1;
            assert!(p
                .binding
                .verify(&mut Cursor::new(&changed), changed.len() as u64, 4096, 512)
                .is_err());
        }
        assert!(p
            .binding
            .verify(&mut Cursor::new(&data), data.len() as u64, 8192, 512)
            .is_err());
        assert!(p
            .binding
            .verify(&mut Cursor::new(&data), data.len() as u64, 4096, 4096)
            .is_err());
        let mut changed = data.clone();
        changed[128 * 1024] ^= 1;
        p.binding
            .verify(&mut Cursor::new(&changed), changed.len() as u64, 4096, 512)
            .unwrap();
        assert!(anchors(511).is_err());
        assert!(anchors((1 << 43) + 1).is_err());
    }
}

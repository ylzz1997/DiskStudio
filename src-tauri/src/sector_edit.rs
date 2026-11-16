//! Staged byte edits and single-attempt sector writes. All tests are synthetic.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Edit {
    pub offset: u32,
    pub value: u8,
}
pub fn apply(before: &[u8], edits: &[Edit]) -> Result<Vec<u8>, String> {
    if !matches!(before.len(), 512 | 4096) || edits.is_empty() || edits.len() > before.len() {
        return Err("请选择一个完整扇区内的字节修改。".into());
    }
    let mut after = before.to_vec();
    let mut seen = vec![false; before.len()];
    for edit in edits {
        let at = edit.offset as usize;
        if at >= before.len() || seen[at] {
            return Err("修改偏移越界或重复。".into());
        }
        seen[at] = true;
        after[at] = edit.value;
    }
    if after == before {
        return Err("没有需要保存的字节改动。".into());
    }
    Ok(after)
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn write_once<R: Read + Write + Seek>(
    file: &mut R,
    offset: u64,
    before: &[u8],
    after: &[u8],
    sync: impl FnOnce(&mut R) -> std::io::Result<()>,
) -> Result<String, String> {
    if !matches!(before.len(), 512 | 4096)
        || after.len() != before.len()
        || offset % before.len() as u64 != 0
        || offset
            .checked_add(before.len() as u64)
            .is_none_or(|end| end > i64::MAX as u64)
        || before == after
    {
        return Err("扇区写入范围或数据无效。".into());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut current = vec![0; before.len()];
    file.read_exact(&mut current).map_err(|e| e.to_string())?;
    if current != before {
        return Err("源扇区与预览时不同，未写入。请刷新后重新编辑。".into());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    // One OS write request. A short write or error is partial failure, never retry.
    let n = file
        .write(after)
        .map_err(|e| format!("写入请求失败，可能已部分写入：{e}"))?;
    if n != after.len() {
        return Err(format!("只写入 {n}/{} 字节，未重试。", after.len()));
    }
    sync(file).map_err(|e| format!("写入后刷新失败：{e}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut current)
        .map_err(|e| format!("写入后回读失败：{e}"))?;
    let expected = hash(after);
    if hash(&current) != expected {
        return Err("写入后 SHA-256 回读不一致。".into());
    }
    Ok(expected)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};
    #[test]
    fn staged_edits_are_bounded_unique_and_do_not_modify_original() {
        let original = vec![0; 512];
        let edits = vec![Edit {
            offset: 511,
            value: 255,
        }];
        let result = apply(&original, &edits).unwrap();
        assert_eq!(result[511], 255);
        assert_eq!(original[511], 0);
        for bad in [
            vec![],
            vec![Edit {
                offset: 512,
                value: 1,
            }],
            vec![
                Edit {
                    offset: 0,
                    value: 1,
                },
                Edit {
                    offset: 0,
                    value: 2,
                },
            ],
            vec![Edit {
                offset: 0,
                value: 0,
            }],
        ] {
            assert!(apply(&original, &bad).is_err())
        }
        assert!(apply(
            &vec![0; 4096],
            &[Edit {
                offset: 4095,
                value: 1
            }]
        )
        .is_ok());
        assert!(serde_json::from_str::<Edit>(r#"{"offset":0,"value":256}"#).is_err());
        assert!(serde_json::from_str::<Edit>(r#"{"offset":0,"value":1,"path":"x"}"#).is_err());
    }
    struct Device {
        bytes: Cursor<Vec<u8>>,
        writes: usize,
        short: bool,
        corrupt: bool,
    }
    impl Read for Device {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.bytes.read(b)
        }
    }
    impl Seek for Device {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(p)
        }
    }
    impl Write for Device {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            let len = if self.short { 17 } else { b.len() };
            self.bytes.write_all(&b[..len])?;
            if self.corrupt {
                let at = self.bytes.position() as usize - 1;
                self.bytes.get_mut()[at] ^= 1;
            }
            Ok(len)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn device() -> Device {
        Device {
            bytes: Cursor::new(vec![7; 1536]),
            writes: 0,
            short: false,
            corrupt: false,
        }
    }
    #[test]
    fn one_sector_write_preserves_neighbors_and_verifies_hash() {
        let mut d = device();
        let before = vec![7; 512];
        let mut after = before.clone();
        after[123] = 42;
        assert_eq!(
            write_once(&mut d, 512, &before, &after, |_| Ok(())).unwrap(),
            hash(&after)
        );
        assert_eq!(d.writes, 1);
        assert_eq!(&d.bytes.get_ref()[..512], &before);
        assert_eq!(&d.bytes.get_ref()[1024..], &before);
        assert_eq!(&d.bytes.get_ref()[512..1024], &after);
    }
    #[test]
    fn stale_bytes_and_invalid_ranges_never_write() {
        let mut d = device();
        let before = vec![0; 512];
        let after = vec![1; 512];
        assert!(write_once(&mut d, 512, &before, &after, |_| Ok(()))
            .unwrap_err()
            .contains("未写入"));
        assert!(write_once(&mut d, 1, &before, &after, |_| Ok(())).is_err());
        assert!(write_once(&mut d, u64::MAX - 511, &before, &after, |_| Ok(())).is_err());
        assert_eq!(d.writes, 0);
    }
    #[test]
    fn partial_sync_and_readback_failures_do_not_retry_or_rollback() {
        for (short, corrupt, sync_error) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let mut d = device();
            d.short = short;
            d.corrupt = corrupt;
            assert!(write_once(&mut d, 512, &vec![7; 512], &vec![8; 512], |_| {
                if sync_error {
                    Err(io::Error::other("flush"))
                } else {
                    Ok(())
                }
            })
            .is_err());
            assert_eq!(d.writes, 1);
            assert_eq!(d.bytes.get_ref()[512], 8);
        }
    }
}

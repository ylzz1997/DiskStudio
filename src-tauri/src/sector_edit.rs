//! Staged byte edits and single-attempt sector writes. All tests are synthetic.
use crate::surface_scan::Range;
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
pub fn physical_block(
    page: Range,
    scope: Range,
    physical: u64,
    alignment_offset: u64,
) -> Result<Range, String> {
    let logical = page.sector;
    if !matches!(logical, 512 | 4096)
        || scope.sector != logical
        || page.size != logical
        || !matches!(physical, 512 | 4096)
        || physical < logical
        || alignment_offset >= physical
        || alignment_offset % logical != 0
        || page.offset % logical != 0
    {
        return Err("无法确认编辑所需的物理扇区边界。".into());
    }
    let padding = (page.offset % physical + alignment_offset) % physical;
    let offset = page
        .offset
        .checked_sub(padding)
        .ok_or("物理扇区跨出所选范围。")?;
    let scope_end = scope
        .offset
        .checked_add(scope.size)
        .ok_or("物理扇区跨出所选范围。")?;
    if offset < scope.offset
        || offset
            .checked_add(physical)
            .is_none_or(|end| end > scope_end)
    {
        return Err("物理扇区跨出所选范围。".into());
    }
    Ok(Range {
        offset,
        size: physical,
        sector: logical,
    })
}

pub fn check_unallocated(
    block: Range,
    first_usable: u64,
    last_usable: u64,
    entries: impl IntoIterator<Item = (u64, u64)>,
) -> Result<(), String> {
    let end = block.offset.checked_add(block.size).ok_or("扇区位置溢出")?;
    if !matches!(block.sector, 512 | 4096)
        || block.size < block.sector
        || block.offset / block.sector < first_usable
        || (end - 1) / block.sector > last_usable
        || entries.into_iter().any(|(offset, size)| {
            offset < end
                && offset
                    .checked_add(size)
                    .is_none_or(|next| block.offset < next)
        })
    {
        return Err("整盘视图只能写回 GPT 可用范围内的未分配物理扇区。".into());
    }
    Ok(())
}

pub fn write_once<R: Read + Write + Seek>(
    file: &mut R,
    range: Range,
    before: &[u8],
    after: &[u8],
    sync: impl FnOnce(&mut R) -> std::io::Result<()>,
) -> Result<String, String> {
    let offset = range.offset;
    if !matches!(before.len(), 512 | 4096)
        || after.len() != before.len()
        || range.size != before.len() as u64
        || !matches!(range.sector, 512 | 4096)
        || range.sector > range.size
        || offset % range.sector != 0
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
    fn range(offset: u64, size: u64, sector: u64) -> Range {
        Range {
            offset,
            size,
            sector,
        }
    }
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
    fn physical_block_respects_alignment_scope_and_gpt_gaps() {
        let disk = range(0, 16384, 512);
        let block = physical_block(range(4608, 512, 512), disk, 4096, 0).unwrap();
        assert_eq!(block, range(4096, 4096, 512));
        assert!(check_unallocated(block, 8, 15, [(0, 4096), (8192, 512)]).is_ok());
        for (first, last, parts) in [(9, 15, vec![]), (8, 14, vec![]), (8, 15, vec![(7680, 512)])] {
            assert!(check_unallocated(block, first, last, parts).is_err());
        }
        assert_eq!(
            physical_block(range(4096, 512, 512), disk, 4096, 512).unwrap(),
            range(3584, 4096, 512)
        );
        assert!(physical_block(range(0, 512, 512), disk, 4096, 512).is_err());
        assert!(physical_block(range(4608, 512, 512), range(4608, 8192, 512), 4096, 0).is_err());
        assert_eq!(
            physical_block(range(8192, 4096, 4096), range(0, 16384, 4096), 4096, 0).unwrap(),
            range(8192, 4096, 4096)
        );
    }
    #[test]
    fn edits_in_512e_require_unchanged_neighbors_and_write_one_physical_block() {
        let mut d = device();
        d.bytes = Cursor::new(vec![7; 12288]);
        let block = range(4096, 4096, 512);
        let before = vec![7; 4096];
        let mut after = before.clone();
        after[512] = 42;
        d.bytes.get_mut()[4096 + 511] = 9;
        assert!(write_once(&mut d, block, &before, &after, |_| Ok(())).is_err());
        assert_eq!(d.writes, 0);
        d.bytes.get_mut()[4096 + 511] = 7;
        assert_eq!(
            write_once(&mut d, block, &before, &after, |_| Ok(())).unwrap(),
            hash(&after)
        );
        assert_eq!(d.writes, 1);
        assert_eq!(
            d.bytes
                .get_ref()
                .iter()
                .enumerate()
                .filter(|(_, value)| **value != 7)
                .collect::<Vec<_>>(),
            vec![(4608, &42)]
        );
    }
    #[test]
    fn one_sector_write_preserves_neighbors_and_verifies_hash() {
        let mut d = device();
        let before = vec![7; 512];
        let mut after = before.clone();
        after[123] = 42;
        assert_eq!(
            write_once(&mut d, range(512, 512, 512), &before, &after, |_| Ok(())).unwrap(),
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
        assert!(
            write_once(&mut d, range(512, 512, 512), &before, &after, |_| Ok(()))
                .unwrap_err()
                .contains("未写入")
        );
        assert!(write_once(&mut d, range(1, 512, 512), &before, &after, |_| Ok(())).is_err());
        assert!(write_once(
            &mut d,
            range(u64::MAX - 511, 512, 512),
            &before,
            &after,
            |_| Ok(())
        )
        .is_err());
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
            assert!(write_once(
                &mut d,
                range(512, 512, 512),
                &vec![7; 512],
                &vec![8; 512],
                |_| {
                    if sync_error {
                        Err(io::Error::other("flush"))
                    } else {
                        Ok(())
                    }
                }
            )
            .is_err());
            assert_eq!(d.writes, 1);
            assert_eq!(d.bytes.get_ref()[512], 8);
        }
    }
}

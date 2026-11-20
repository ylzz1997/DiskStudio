//! Exact-sector rewrite plans. The Windows adapter owns device identity,
//! volume locks, the exclusive writable handle, and disk state.
use crate::surface_scan::{self, FailureResolution, Range, Report, ScanSource, TransferError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(windows)]
#[path = "surface_repair_windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::{discard, execute, prepare};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub logical: u64,
    pub physical: u64,
    pub offset: u64,
}
impl Geometry {
    fn validate(self, scan: Range) -> Result<(), String> {
        if self.logical != scan.sector
            || !matches!(self.logical, 512 | 4096)
            || !matches!(self.physical, 512 | 4096)
            || self.physical < self.logical
            || self.offset >= self.physical
            || self.offset % self.logical != 0
        {
            return Err("无法确认修复所需的物理扇区边界。".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Group {
    pub range: Range,
    candidates: Vec<u64>,
}
impl Group {
    pub fn candidate_count(&self) -> u64 {
        self.candidates.len() as u64
    }
}

pub(crate) fn groups(report: &Report, geometry: Geometry) -> Result<(Vec<Group>, u64), String> {
    report.validate()?;
    let scan = report.range();
    geometry.validate(scan)?;
    let refinement = report.refinement.as_ref().ok_or("请先细查读取失败的块。")?;
    let mut list: Vec<Group> = Vec::new();
    let mut excluded = refinement.failed_sectors;
    for failure in &refinement.ranges {
        if failure.resolution != FailureResolution::SectorReadFailed {
            continue;
        }
        let at = failure.start_sector * geometry.logical;
        let padding = (at + geometry.offset) % geometry.physical;
        let Some(offset) = at.checked_sub(padding) else {
            continue;
        };
        if offset < scan.offset || offset + geometry.physical > scan.offset + scan.size {
            continue;
        }
        excluded -= 1;
        if let Some(group) = list.last_mut().filter(|g| g.range.offset == offset) {
            group.candidates.push(failure.start_sector);
        } else {
            list.push(Group {
                range: Range {
                    offset,
                    size: geometry.physical,
                    sector: geometry.logical,
                },
                candidates: vec![failure.start_sector],
            });
        }
    }
    Ok((list, excluded))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedSector {
    lba: u64,
    code: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockProof {
    start_lba: u64,
    sector_count: u64,
    failed: Vec<FailedSector>,
    sha256: String,
}
impl BlockProof {
    pub(crate) fn failure_count(&self) -> u64 {
        self.failed.len() as u64
    }
}

#[repr(align(4096))]
pub(crate) struct Aligned([u8; 4096]);

pub(crate) fn canceled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("修复任务已取消。".into())
    } else {
        Ok(())
    }
}

/// Reconstruct the entire physical block from exact logical reads. An
/// unexpected unreadable neighbor blocks the whole write.
pub(crate) fn observe(
    reader: &mut impl ScanSource,
    group: &Group,
    cancel: &AtomicBool,
) -> Result<Option<(BlockProof, Aligned)>, String> {
    let range = group.range;
    let mut scratch = Aligned([0; 4096]);
    let mut image = Aligned([0; 4096]);
    let mut failed = Vec::new();
    let logical = range.sector as usize;
    for (index, bytes) in image.0[..range.size as usize]
        .chunks_mut(logical)
        .enumerate()
    {
        canceled(cancel)?;
        let offset = range.offset + index as u64 * range.sector;
        let lba = offset / range.sector;
        match reader.read_at(offset, &mut scratch.0[..logical], cancel) {
            Ok(n) if n == logical => bytes.copy_from_slice(&scratch.0[..logical]),
            Err(TransferError::Io(err))
                if err.raw_os_error().is_some_and(surface_scan::media_error) =>
            {
                if group.candidates.binary_search(&lba).is_err() {
                    return Ok(None);
                }
                failed.push(FailedSector {
                    lba,
                    code: err.raw_os_error().unwrap(),
                });
            }
            other => return Err(read_error(lba, other)),
        }
    }
    Ok(Some((
        BlockProof {
            start_lba: range.offset / range.sector,
            sector_count: range.size / range.sector,
            failed,
            sha256: hash(&image.0[..range.size as usize]),
        },
        image,
    )))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_error(lba: u64, error: Result<usize, TransferError>) -> String {
    let reason = match error {
        Ok(n) => format!("读取长度异常（{n} 字节）。"),
        Err(TransferError::Io(err)) => err.to_string(),
        #[cfg(test)]
        Err(TransferError::Position(err)) => err.to_string(),
        Err(TransferError::Canceled) => "修复任务已取消。".into(),
        Err(TransferError::TimedOut) => "读取超时。".into(),
    };
    format!("LBA {lba}：{reason}")
}

pub(crate) fn write_verified<W: Seek + Write>(
    writer: &mut W,
    reader: &mut impl ScanSource,
    group: &Group,
    expected: &BlockProof,
    image: &Aligned,
    sync: impl FnOnce(&mut W) -> std::io::Result<()>,
    cancel: &AtomicBool,
    attempted: &mut usize,
) -> Result<(), String> {
    let offset = group.range.offset;
    let size = group.range.size as usize;
    writer
        .seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    canceled(cancel)?;
    *attempted += 1;
    // One request per physical block. A short write is an uncertain change.
    match writer.write(&image.0[..size]) {
        Ok(n) if n == size => {}
        Ok(n) => {
            return Err(format!(
                "LBA {} 写入长度异常（{n}/{size} 字节）。",
                expected.start_lba
            ))
        }
        Err(err) => return Err(format!("LBA {} 写入失败：{err}", expected.start_lba)),
    }
    sync(writer).map_err(|e| format!("LBA {} 写后刷新失败：{e}", expected.start_lba))?;
    let mut readback = Aligned([0; 4096]);
    // User cancellation stops before the next block. Finish the readback for
    // the block that has already been written, using the native read deadline.
    match reader.read_at(offset, &mut readback.0[..size], &AtomicBool::new(false)) {
        Ok(n) if n == size => {}
        err => return Err(read_error(expected.start_lba, err)),
    }
    if hash(&readback.0[..size]) != expected.sha256 {
        return Err(format!("LBA {} 写后 SHA-256 不一致。", expected.start_lba));
    }
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub target: String,
    pub logical_sector_size: u64,
    pub physical_sector_size: u64,
    pub excluded_sectors: u64,
    pub now_readable_sectors: u64,
    pub blocks: Vec<BlockProof>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub state: &'static str,
    pub planned_blocks: usize,
    pub attempted_blocks: usize,
    pub verified_blocks: Vec<BlockProof>,
    pub disk_left_offline: Option<bool>,
    pub message: String,
}

#[cfg(not(windows))]
pub fn prepare(_: String) -> Result<Preview, String> {
    Err("当前仅 Windows 支持坏道写入修复。".into())
}
#[cfg(not(windows))]
pub fn execute(_: String) -> Result<Outcome, String> {
    Err("当前仅 Windows 支持坏道写入修复。".into())
}
#[cfg(not(windows))]
pub fn discard(_: String) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        io::{self, Cursor},
        rc::Rc,
    };

    #[derive(Clone)]
    struct Media(Rc<RefCell<Memory>>);
    struct Memory {
        bytes: Cursor<Vec<u8>>,
        faults: Vec<u64>,
        writes: usize,
        short: bool,
    }
    impl Media {
        fn new(faults: Vec<u64>) -> Self {
            Self(Rc::new(RefCell::new(Memory {
                bytes: Cursor::new(vec![37; 8192]),
                faults,
                writes: 0,
                short: false,
            })))
        }
    }
    impl ScanSource for Media {
        fn read_at(
            &mut self,
            offset: u64,
            buffer: &mut [u8],
            _: &AtomicBool,
        ) -> Result<usize, TransferError> {
            let memory = self.0.borrow();
            if memory
                .faults
                .iter()
                .any(|&at| (offset..offset + buffer.len() as u64).contains(&at))
            {
                return Err(TransferError::Io(io::Error::from_raw_os_error(1117)));
            }
            let start = offset as usize;
            buffer.copy_from_slice(&memory.bytes.get_ref()[start..start + buffer.len()]);
            Ok(buffer.len())
        }
    }
    impl Seek for Media {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.borrow_mut().bytes.seek(pos)
        }
    }
    impl Write for Media {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut memory = self.0.borrow_mut();
            let start = memory.bytes.position();
            memory.writes += 1;
            let count = if memory.short {
                bytes.len() / 2
            } else {
                bytes.len()
            };
            let n = memory.bytes.write(&bytes[..count])?;
            memory
                .faults
                .retain(|&at| !(start..start + n as u64).contains(&at));
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn physical() -> Group {
        Group {
            range: Range {
                offset: 0,
                size: 4096,
                sector: 512,
            },
            candidates: vec![3, 4],
        }
    }

    #[test]
    fn repair_reads_each_logical_sector_and_preserves_readable_neighbors() {
        let mut media = Media::new(vec![1536]);
        let cancel = AtomicBool::new(false);
        let group = physical();
        let (proof, image) = observe(&mut media, &group, &cancel).unwrap().unwrap();
        assert_eq!(proof.failed, vec![FailedSector { lba: 3, code: 1117 }]);
        let mut attempted = 0;
        write_verified(
            &mut media.clone(),
            &mut media,
            &group,
            &proof,
            &image,
            |writer| writer.flush(),
            &cancel,
            &mut attempted,
        )
        .unwrap();
        let memory = media.0.borrow();
        let mut expected = vec![37; 8192];
        expected[1536..2048].fill(0);
        assert_eq!(memory.bytes.get_ref(), &expected);
        assert_eq!(memory.writes, 1);
        assert_eq!(attempted, 1);
    }

    #[test]
    fn repair_stops_on_unlisted_failure_or_short_write_without_retry() {
        let mut media = Media::new(vec![1536, 2560]);
        let cancel = AtomicBool::new(false);
        let group = physical();
        assert!(observe(&mut media, &group, &cancel).unwrap().is_none());
        assert_eq!(media.0.borrow().writes, 0);
        media.0.borrow_mut().faults.pop();
        media.0.borrow_mut().short = true;
        let (proof, image) = observe(&mut media, &group, &cancel).unwrap().unwrap();
        let mut attempted = 0;
        assert!(write_verified(
            &mut media.clone(),
            &mut media,
            &group,
            &proof,
            &image,
            |writer| writer.flush(),
            &cancel,
            &mut attempted
        )
        .is_err());
        assert_eq!(media.0.borrow().writes, 1);
        assert_eq!(attempted, 1);
    }

    #[test]
    fn repair_uses_reported_alignment_and_excludes_a_partial_physical_block() {
        let range = Range {
            offset: 0,
            size: 8192,
            sector: 512,
        };
        let mut media = Media::new(vec![1024, 3072, 4096]);
        let cancel = AtomicBool::new(false);
        let report = surface_scan::continue_scan(
            &mut media,
            Report::new(range, true),
            surface_scan::TransferLimit::from_adapter(1048576, 256, 0, 4096).unwrap(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let (groups, excluded) = groups(
            &report,
            Geometry {
                logical: 512,
                physical: 4096,
                offset: 1536,
            },
        )
        .unwrap();
        assert_eq!(excluded, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].range.offset, 2560);
        assert_eq!(groups[0].candidates, vec![6, 8]);
    }
}

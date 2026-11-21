//! Locate a missing Ext4 extent, then open a corroborated read-only view.
use super::{canceled, io, Candidate, Range, Read, Region, Seek, SeekFrom, Validation};
use crate::{ext_recovery::reconstruct as ext, recovery::view};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{atomic::AtomicBool, Arc},
};

pub(super) const BUDGET: u64 = 128 * 1024 * 1024;
const BUDGET_ERROR: &str = "Ext4 备用超级块证据超过搜索预算，请缩小范围。";

#[derive(Clone, Copy)]
pub(super) struct Anchor {
    pub(super) start: u64,
    pub(super) size: u64,
}

pub(super) fn hint(bytes: &[u8], offset: u64, range: Range) -> Option<Anchor> {
    let (distance, size) = ext::backup_extent(bytes, range.size)?;
    let start = offset.checked_sub(distance)?;
    (start >= range.offset
        && start % range.sector == 0
        && size % range.sector == 0
        && start.checked_add(size)? <= range.offset + range.size)
        .then_some(Anchor { start, size })
}

struct EvidenceReader<R> {
    reader: R,
    remaining: Rc<Cell<u64>>,
    fault: Rc<RefCell<Option<String>>>,
    cancel: Arc<AtomicBool>,
}

impl<R: Read> Read for EvidenceReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        canceled(&self.cancel).map_err(io::Error::other)?;
        let result = match self.remaining.get().checked_sub(bytes.len() as u64) {
            Some(remaining) => {
                self.remaining.set(remaining);
                self.reader.read_exact(bytes).map(|()| bytes.len())
            }
            None => Err(io::Error::other(BUDGET_ERROR)),
        };
        result.inspect_err(|err| *self.fault.borrow_mut() = Some(err.to_string()))
    }
}

impl<R: Seek> Seek for EvidenceReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.reader
            .seek(from)
            .inspect_err(|err| *self.fault.borrow_mut() = Some(err.to_string()))
    }
}

pub(super) fn probe<R: Read + Seek + 'static>(
    reader: R,
    sector: u64,
    anchor: Anchor,
    cancel: &Arc<AtomicBool>,
    remaining: &Rc<Cell<u64>>,
) -> Result<Option<Candidate>, String> {
    let fault = Rc::new(RefCell::new(None));
    let reader = EvidenceReader {
        reader,
        remaining: remaining.clone(),
        fault: fault.clone(),
        cancel: cancel.clone(),
    };
    let mut region =
        Region::for_metadata(reader, anchor.start, anchor.size).map_err(|e| e.to_string())?;
    let checked = (|| {
        let Some(evidence) = ext::inspect(&mut region, sector, cancel)? else {
            return Ok(None);
        };
        let (mut view, proof) = evidence.open(region, sector, cancel)?;
        if view.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != anchor.size {
            return Ok(None);
        }
        Ok::<_, String>(Some(proof))
    })();
    if let Some(error) = fault.borrow_mut().take() {
        return Err(error);
    }
    canceled(cancel)?;
    let Ok(Some(proof)) = checked else {
        return Ok(None);
    };
    Ok(Some(Candidate {
        id: uuid::Uuid::new_v4().to_string(),
        filesystem: "Ext4".into(),
        offset: anchor.start,
        size: anchor.size,
        evidence: "Ext4 备用超级块、当前块组、根 inode 与内部日志已核对。".into(),
        warning: "使用备用元数据重建文件视图。容量来自文件系统，可预览和提取已核对的文件。".into(),
        can_restore: false,
        can_browse: true,
        btrfs_member_count: None,
        validation: Validation::BackupMetadata(view::Proof::Ext(proof)),
        btrfs_member: None,
    }))
}

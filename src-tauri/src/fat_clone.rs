//! Read the resized FAT filesystem as a logical stream backed by the unchanged source.
use super::{at, bitmap_test, e, prepare_classic, prepare_exfat, u16at, ClusterCopies, Plan};
use std::io::{self, Read, Seek, SeekFrom};

pub(crate) fn prepare<R: Read + Seek>(
    reader: &mut R,
    sector: u64,
    size: u64,
    destination_offset: u64,
) -> Result<Plan, String> {
    if !matches!(sector, 512 | 4096) || destination_offset % sector != 0 {
        return Err("目标分区与逻辑扇区未对齐。".into());
    }
    let boot = at(reader, 0, 512)?;
    let boot: &[u8; 512] = boot.as_slice().try_into().unwrap();
    if &boot[3..11] == b"NTFS    " || !crate::recovery::fat_boot(boot) {
        return Err("不同容量克隆目前支持可核对的 FAT12/16/32 和 exFAT 数据分区。".into());
    }
    let lba = destination_offset / sector;
    let mut plan = if &boot[3..11] == b"EXFAT   " {
        let mut plan = prepare_exfat(reader, sector, size)?;
        for edit in &mut plan.edits {
            if [0, 12 * sector].contains(&edit.offset) {
                edit.after[64..72].copy_from_slice(&lba.to_le_bytes());
                super::exfat::write_boot_checksum(&mut edit.after, sector as usize);
            }
        }
        plan
    } else {
        let hidden = u32::try_from(lba).map_err(|_| "目标起始 LBA 超出 FAT 引导字段范围。")?;
        let mut plan = prepare_classic(reader, sector, size)?;
        let backup = (plan.filesystem == "FAT32").then(|| u64::from(u16at(boot, 50)) * sector);
        for edit in &mut plan.edits {
            if edit.offset == 0 || backup == Some(edit.offset) {
                edit.after[28..32].copy_from_slice(&hidden.to_le_bytes());
            }
        }
        plan
    };
    plan.edits.sort_unstable_by_key(|edit| edit.offset);
    let mut end = 0;
    for edit in &plan.edits {
        if edit.offset < end || edit.after.is_empty() {
            return Err("克隆文件系统元数据范围重叠或无效。".into());
        }
        end = edit
            .offset
            .checked_add(edit.after.len() as u64)
            .filter(|&end| end <= size)
            .ok_or("克隆文件系统元数据范围越界。")?;
    }
    Ok(plan)
}

/// Read-only composition of volume metadata, relocated data and the unchanged
/// source bytes. Rewrites override mapped clusters just as Plan::apply does.
pub(crate) struct Reader<'a, R> {
    plan: &'a Plan,
    source: R,
    position: u64,
}

impl<'a, R: Read + Seek> Reader<'a, R> {
    pub(crate) fn new(plan: &'a Plan, mut source: R) -> Result<Self, String> {
        if source.seek(SeekFrom::End(0)).map_err(e)? != plan.old_size {
            return Err("读取范围与调整容量前的分区不一致。".into());
        }
        plan.compare(&mut source, false)?;
        Ok(Self {
            plan,
            source,
            position: 0,
        })
    }

    fn source_span(&self, limit: u64) -> (u64, u64) {
        let pos = self.position;
        let cluster = self.plan.cluster_size as u64;
        match &self.plan.copies {
            ClusterCopies::Relocated(copies) => {
                // The planner allocates destination clusters in ascending order.
                let next = copies.partition_point(|copy| copy.to + cluster <= pos);
                match copies.get(next) {
                    Some(copy) if copy.to <= pos => {
                        (copy.from + pos - copy.to, limit.min(copy.to + cluster))
                    }
                    Some(copy) => (pos, limit.min(copy.to)),
                    None => (pos, limit),
                }
            }
            ClusterCopies::Shifted {
                heap,
                delta,
                allocated,
                ..
            } => {
                let first = heap + delta;
                let count = u64::from(self.plan.old_clusters);
                let end = first + count * cluster;
                if pos < first {
                    return (pos, limit.min(first));
                }
                if pos >= end {
                    return (pos, limit);
                }
                let index = (pos - first) / cluster;
                let moved = bitmap_test(allocated, index as usize);
                let mut next = index + 1;
                while next < count
                    && first + next * cluster < limit
                    && bitmap_test(allocated, next as usize) == moved
                {
                    next += 1;
                }
                (
                    if moved { pos - delta } else { pos },
                    limit.min(first + next * cluster),
                )
            }
        }
    }
}

impl<R: Read + Seek> Read for Reader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let pos = self.position;
        if pos == self.plan.new_size || out.is_empty() {
            return Ok(0);
        }
        let limit = pos + (self.plan.new_size - pos).min(out.len() as u64);
        let index = self
            .plan
            .edits
            .partition_point(|edit| edit.offset + edit.after.len() as u64 <= pos);
        let edit = self.plan.edits.get(index);
        let count = if let Some(edit) = edit.filter(|edit| edit.offset <= pos) {
            let bytes = &edit.after[(pos - edit.offset) as usize..];
            let count = bytes.len().min((limit - pos) as usize);
            out[..count].copy_from_slice(&bytes[..count]);
            count
        } else {
            let (from, end) = self.source_span(edit.map_or(limit, |edit| limit.min(edit.offset)));
            let count = (end - pos) as usize;
            if from >= self.plan.old_size {
                out[..count].fill(0);
                count
            } else {
                let count = count.min((self.plan.old_size - from).min(usize::MAX as u64) as usize);
                self.source.seek(SeekFrom::Start(from))?;
                self.source.read_exact(&mut out[..count])?;
                count
            }
        };
        self.position += count as u64;
        Ok(count)
    }
}

impl<R> Seek for Reader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let size = i128::from(self.plan.new_size);
        let pos = match from {
            SeekFrom::Start(pos) => i128::from(pos),
            SeekFrom::End(delta) => size + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if !(0..=size).contains(&pos) {
            return Err(io::Error::other("定位超出克隆分区范围。"));
        }
        self.position = pos as u64;
        Ok(self.position)
    }
}

#[cfg(test)]
pub(super) fn assert_materializes(source: &[u8], sector: u64, size: u64) {
    use std::io::Cursor;
    let mut base = Cursor::new(source);
    let target_offset = 3 * 1024 * 1024;
    let plan = prepare(&mut base, sector, size, target_offset).unwrap();
    let mut view = Reader::new(&plan, base).unwrap();
    let mut clone = Vec::new();
    view.read_to_end(&mut clone).unwrap();
    assert_eq!(clone.len() as u64, size);
    let mut expected = Cursor::new(source.to_vec());
    expected
        .get_mut()
        .resize(size.max(source.len() as u64) as usize, 0);
    plan.apply(&mut expected).unwrap();
    assert_eq!(clone, expected.get_ref()[..size as usize]);
    let lba = target_offset / sector;
    if plan.filesystem == "exFAT" {
        assert_eq!(clone[64..72], lba.to_le_bytes());
        assert_eq!(
            clone[12 * sector as usize + 64..12 * sector as usize + 72],
            lba.to_le_bytes()
        );
    } else {
        assert_eq!(clone[28..32], (lba as u32).to_le_bytes());
    }
    plan.verify(&mut Cursor::new(&clone)).unwrap();
    use std::sync::atomic::AtomicBool;
    let cancel = AtomicBool::new(false);
    let mut view = Reader::new(&plan, Cursor::new(source)).unwrap();
    let allocation = crate::used_space::inspect(&mut view, size, sector, None, &cancel).unwrap();
    assert_eq!(allocation.volumes[0].filesystem, plan.filesystem);
    plan.verify(&mut allocation.masked(view)).unwrap();
}

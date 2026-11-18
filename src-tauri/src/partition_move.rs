//! GPT changes for same-disk partition moves. A caller must secure the device,
//! bind the plan to a durable journal and verify the destination before commit.
use crate::{
    partitions::policy::BASIC_DATA,
    table_archive::{read_at, Archive, Extent, Kind},
    table_restore::{archive_layout, edit_gpt_entry, Layout},
};
use disk_core::partition_move::{Journal, Range, Stage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

#[path = "partition_move_boot.rs"]
mod boot;

pub(crate) use boot::supports_filesystem;

const COMMIT_ORDER: [Kind; 4] = [
    Kind::BackupEntries,
    Kind::BackupHeader,
    Kind::PrimaryEntries,
    Kind::PrimaryHeader,
];
const MAX_SEAL_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordedPlan {
    table: Archive,
    partition_guid: String,
    boot: boot::BootRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedPlan {
    plan: String,
    seal: Vec<u8>,
}

/// Both fresh and resumed moves use the same data and metadata execution
/// sequence. The platform verifies disk identity, locks and offline state.
pub(crate) struct Plan {
    pub layout: LayoutPlan,
    pub partition_guid: String,
    boot: boot::BootPlan,
    record: Vec<u8>,
}

impl Plan {
    pub fn prepare(
        table: Archive,
        partition_guid: &str,
        destination: u64,
        volume: &mut (impl Read + Seek),
        reported_fs: &str,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let layout = LayoutPlan::prepare(table, partition_guid, destination)?;
        let boot = boot::BootRecord::read(volume, layout.range, reported_fs, cancel)?;
        let after_boot = boot.plan(layout.range)?;
        let record = serde_json::to_vec(&RecordedPlan {
            table: layout.before.clone(),
            partition_guid: partition_guid.to_owned(),
            boot,
        })
        .map_err(|e| e.to_string())?;
        Ok(Self {
            layout,
            partition_guid: partition_guid.to_owned(),
            boot: after_boot,
            record,
        })
    }

    pub fn seal(
        &mut self,
        source_hash: [u8; 32],
        protect: impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>,
    ) -> Result<(), String> {
        let digest = journal_binding(self.layout.range, source_hash, &self.record);
        let seal = protect(&digest)?;
        if !(1..=MAX_SEAL_BYTES).contains(&seal.len()) {
            return Err("移动记录的来源凭据大小无效。".into());
        }
        let plan =
            String::from_utf8(std::mem::take(&mut self.record)).map_err(|e| e.to_string())?;
        self.record = serde_json::to_vec(&SealedPlan { plan, seal }).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn resume<I: Read + Write + Seek>(
        journal: &Journal<I>,
        unprotect: impl FnOnce(&[u8]) -> Result<[u8; 32], String>,
    ) -> Result<Self, String> {
        let sealed: SealedPlan =
            serde_json::from_slice(journal.metadata()).map_err(|_| "断点记录缺少来源凭据。")?;
        if !(1..=MAX_SEAL_BYTES).contains(&sealed.seal.len())
            || unprotect(&sealed.seal)?
                != journal_binding(
                    journal.range(),
                    journal.source_hash(),
                    sealed.plan.as_bytes(),
                )
        {
            return Err("断点记录的来源凭据或确认内容无法核对。".into());
        }
        let data: RecordedPlan = serde_json::from_str(&sealed.plan).map_err(|e| e.to_string())?;
        let layout = LayoutPlan::prepare(
            data.table,
            &data.partition_guid,
            journal.range().destination(),
        )?;
        if layout.range != journal.range() {
            return Err("移动记录的范围与 GPT 条目不一致。".into());
        }
        let boot = data.boot.plan(layout.range)?;
        Ok(Self {
            layout,
            partition_guid: data.partition_guid,
            boot,
            record: journal.metadata().to_vec(),
        })
    }

    pub fn metadata(&self) -> &[u8] {
        &self.record
    }

    pub fn transfer<D: Read + Write + Seek, I: Read + Write + Seek>(
        &self,
        disk: &mut D,
        journal: &mut Journal<I>,
        mut sync: impl FnMut(&mut D) -> io::Result<()>,
        mut progress: impl FnMut(Stage, u64) -> Result<(), String>,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if journal.range() != self.layout.range || journal.metadata() != self.record {
            return Err("移动计划与断点记录不匹配。".into());
        }
        let current = self.layout.committed_ranges(disk)?;
        if !journal.copy_finished() {
            if current != 0 {
                return Err("数据还未复制完成，原 GPT 已变化。".into());
            }
            if journal.copied_bytes() == 0 {
                self.boot.verify_source(disk)?;
            }
            journal.copy(disk, &mut sync, &mut progress)?;
        } else if (current > 0 || journal.completed()) && !self.boot.verified(disk)? {
            return Err("GPT 已写入，但目标卷引导更新尚未通过核对。".into());
        }
        journal.verify_destination(
            disk,
            |offset, bytes| self.boot.normalize(offset, bytes),
            &mut progress,
        )?;
        if journal.completed() {
            return self.layout.after.matches(disk, checkpoint);
        }
        checkpoint()?;
        self.boot.apply(disk, &mut sync)?;
        self.layout.commit(disk, &mut sync, checkpoint)?;
        sync(disk).map_err(error)
    }
}

fn journal_binding(range: Range, source_hash: [u8; 32], record: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"DiskStudio partition move journal v1");
    for value in [
        range.source(),
        range.destination(),
        range.length(),
        range.capacity(),
        range.sector(),
    ] {
        digest.update(value.to_le_bytes());
    }
    digest.update(source_hash);
    digest.update(record);
    digest.finalize().into()
}

pub(crate) struct LayoutPlan {
    pub range: Range,
    pub before: Archive,
    pub old_layout: Layout,
    pub new_layout: Layout,
    after: Archive,
}

impl LayoutPlan {
    pub(crate) fn result_archive(&self) -> &Archive {
        &self.after
    }

    pub fn prepare(
        before: Archive,
        partition_guid: &str,
        destination: u64,
    ) -> Result<Self, String> {
        let old_layout = archive_layout(&before)?;
        let selected = old_layout
            .entries
            .iter()
            .find(|entry| entry.guid == partition_guid)
            .ok_or("未找到所选 GPT 条目。")?;
        if selected.kind != BASIC_DATA || selected.attributes & !(1 << 63) != 0 {
            return Err("移动要求保护信息完整的普通 GPT 数据分区。".into());
        }
        let range = Range::new(
            selected.offset,
            destination,
            selected.size,
            before.size,
            before.sector,
        )?;
        let end = destination + range.length();
        let start_lba = destination / before.sector;
        let last_lba = end / before.sector - 1;
        if start_lba < old_layout.first
            || last_lba > old_layout.last
            || old_layout.entries.iter().any(|other| {
                other.guid != selected.guid
                    && other.offset < end
                    && destination < other.offset + other.size
            })
        {
            return Err("新范围越过 GPT 可用空间或与另一个分区重叠。".into());
        }
        let after = edit_gpt_entry(&before, &old_layout, selected.slot, |entry| {
            entry[32..40].copy_from_slice(&start_lba.to_le_bytes());
            entry[40..48].copy_from_slice(&last_lba.to_le_bytes());
        })?;
        let mut new_layout = old_layout.clone();
        new_layout
            .entries
            .iter_mut()
            .find(|entry| entry.guid == partition_guid)
            .unwrap()
            .offset = destination;
        if archive_layout(&after)? != new_layout {
            return Err("移动 GPT 计划改变了其他持久布局字段。".into());
        }
        Ok(Self {
            range,
            before,
            old_layout,
            new_layout,
            after,
        })
    }

    // The only resumable layouts are a prefix of completed backup/primary
    // writes followed by untouched ranges. Unknown or torn sectors fail closed.
    pub fn committed_ranges(&self, disk: &mut (impl Read + Seek)) -> Result<usize, String> {
        let mbr = self.extent(&self.before, Kind::Mbr)?;
        if read_at(disk, mbr.offset, mbr.bytes.len(), self.range.capacity())? != mbr.bytes {
            return Err("GPT 保护性 MBR 与原记录不一致。".into());
        }
        let mut committed = 0;
        let mut untouched = false;
        for kind in COMMIT_ORDER {
            let before = self.extent(&self.before, kind.clone())?;
            let after = self.extent(&self.after, kind)?;
            let actual = read_at(
                disk,
                before.offset,
                before.bytes.len(),
                self.range.capacity(),
            )?;
            if actual == before.bytes {
                untouched |= before.bytes != after.bytes;
            } else if !untouched && actual == after.bytes {
                committed += 1;
            } else {
                return Err("当前 GPT 包含无法匹配移动记录的字节或写入顺序。".into());
            }
        }
        Ok(committed)
    }

    pub fn commit<D: Read + Write + Seek>(
        &self,
        disk: &mut D,
        mut sync: impl FnMut(&mut D) -> io::Result<()>,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut committed = self.committed_ranges(disk)?;
        let sector = self.range.sector() as usize;
        for kind in COMMIT_ORDER {
            let before = self.extent(&self.before, kind.clone())?;
            let after = self.extent(&self.after, kind)?;
            if before.bytes == after.bytes {
                continue;
            }
            if committed > 0 {
                committed -= 1;
                continue;
            }
            checkpoint()?;
            let mut changed = before
                .bytes
                .chunks_exact(sector)
                .zip(after.bytes.chunks_exact(sector))
                .enumerate()
                .filter(|(_, (a, b))| a != b);
            let Some((i, (_, bytes))) = changed.next() else {
                return Err("找不到待更新的 GPT 扇区。".into());
            };
            if changed.next().is_some() {
                return Err("移动单个分区时 GPT 改动超过一个目标扇区。".into());
            }
            let offset = after.offset + i as u64 * sector as u64;
            write_verified(disk, offset, bytes, self.range.capacity(), &mut sync)?;
        }
        self.after.matches(disk, checkpoint)
    }

    fn extent<'a>(&self, archive: &'a Archive, kind: Kind) -> Result<&'a Extent, String> {
        archive
            .extents
            .iter()
            .find(|extent| extent.kind == kind)
            .ok_or_else(|| "GPT 记录不完整。".into())
    }
}

fn write_verified<D: Read + Write + Seek>(
    disk: &mut D,
    offset: u64,
    bytes: &[u8],
    capacity: u64,
    sync: impl FnOnce(&mut D) -> io::Result<()>,
) -> Result<(), String> {
    disk.seek(SeekFrom::Start(offset)).map_err(error)?;
    if disk.write(bytes).map_err(error)? != bytes.len() {
        return Err("移动元数据时短写，操作可能部分完成。".into());
    }
    sync(disk).map_err(error)?;
    let actual = read_at(disk, offset, bytes.len(), capacity)?;
    if Sha256::digest(&actual) != Sha256::digest(bytes) {
        return Err("移动元数据的 SHA-256 回读不匹配。".into());
    }
    Ok(())
}

fn error(e: io::Error) -> String {
    e.to_string()
}

#[cfg(test)]
#[path = "partition_move_tests.rs"]
mod tests;

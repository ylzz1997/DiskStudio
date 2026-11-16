//! Bounded GPT restore planning and single-attempt writes. No device paths or APIs.
use crate::table_archive::{Archive, Kind};
use disk_core::{
    table_check::{self, CheckState},
    MIB,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub slot: u32,
    pub guid: String,
    pub kind: String,
    pub offset: u64,
    pub size: u64,
    pub attributes: u64,
    pub name: Vec<u16>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub guid: String,
    pub first: u64,
    pub last: u64,
    pub count: u32,
    pub entries: Vec<Entry>,
}
impl Layout {
    /// Windows partition numbers are runtime identifiers, not GPT array slots.
    /// Cross-check every persistent field by unique GUID, retaining raw slots
    /// separately for protected-entry preservation and the restore preview.
    pub fn agrees(&self, other: &Self) -> bool {
        let normalized = |layout: &Self| {
            let mut entries = layout.entries.clone();
            for e in &mut entries {
                e.slot = 0;
            }
            entries.sort_by(|a, b| a.guid.cmp(&b.guid));
            entries
        };
        self.guid == other.guid
            && self.first == other.first
            && self.last == other.last
            && self.count == other.count
            && normalized(self) == normalized(other)
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub guid: String,
    pub before: Option<Entry>,
    pub after: Option<Entry>,
}
#[derive(Clone)]
struct Snapshot {
    offset: u64,
    bytes: Vec<u8>,
}
#[derive(Clone)]
struct WriteRange {
    kind: Kind,
    offset: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}
pub struct Plan {
    pub layout: Layout,
    pub changes: Vec<Change>,
    pub changed_bytes: u64,
    pub write_bytes: u64,
    pub warnings: Vec<String>,
    size: u64,
    before: Vec<Snapshot>,
    writes: Vec<WriteRange>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub written_bytes: u64,
    pub ranges: usize,
    pub sha256: String,
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn guid(b: &[u8]) -> String {
    uuid::Uuid::from_bytes_le(b.try_into().unwrap()).to_string()
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn ordinary(e: &Entry) -> bool {
    matches!(
        e.kind.as_str(),
        crate::partitions::policy::BASIC_DATA
            | crate::partitions::policy::LINUX_DATA
            | crate::partitions::policy::LINUX_SWAP
    ) && e.attributes == 0
}
fn known(e: &Entry) -> bool {
    ordinary(&Entry {
        attributes: 0,
        ..e.clone()
    }) || matches!(
        e.kind.as_str(),
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
            | "e3c9e316-0b5c-4db8-817d-f92df00215ae"
            | "de94bba4-06d1-4d40-a16a-bfd50179d6ac"
    )
}
// Call only with independently CRC/layout-validated header and array bytes.
fn decode_layout(header: &[u8], table: &[u8], sector: u64) -> Result<Layout, String> {
    if header.len() < 92 || &header[..8] != b"EFI PART" || u32le(header, 84) != 128 {
        return Err("恢复当前要求常规 128 字节 GPT 条目。".into());
    }
    let count = u32le(header, 80);
    if count == 0 || count > 128 || table.len() < count as usize * 128 {
        return Err("GPT 条目表超过恢复范围或不完整。".into());
    }
    let mut entries = vec![];
    for (i, e) in table[..count as usize * 128].chunks_exact(128).enumerate() {
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let first = u64le(e, 32);
        let last = u64le(e, 40);
        entries.push(Entry {
            slot: i as u32 + 1,
            guid: guid(&e[16..32]),
            kind: guid(&e[..16]),
            offset: first.checked_mul(sector).ok_or("GPT 偏移溢出")?,
            size: last
                .checked_sub(first)
                .and_then(|n| n.checked_add(1))
                .and_then(|n| n.checked_mul(sector))
                .ok_or("GPT 容量溢出")?,
            attributes: u64le(e, 48),
            name: e[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
        });
    }
    Ok(Layout {
        guid: guid(&header[56..72]),
        first: u64le(header, 40),
        last: u64le(header, 48),
        count,
        entries,
    })
}
pub fn archive_layout(archive: &Archive) -> Result<Layout, String> {
    if archive.validate()?.scheme != "GPT" {
        return Err("MBR/EBR 分区表恢复尚未实现。".into());
    }
    let header = archive
        .extents
        .iter()
        .find(|e| e.kind == Kind::PrimaryHeader)
        .ok_or("主 GPT 缺失")?;
    let table = archive
        .extents
        .iter()
        .find(|e| e.kind == Kind::PrimaryEntries)
        .ok_or("GPT 条目表缺失")?;
    decode_layout(&header.bytes, &table.bytes, archive.sector)
}
fn read(
    reader: &mut (impl Read + Seek),
    offset: u64,
    len: usize,
    size: u64,
) -> Result<Vec<u8>, String> {
    if len == 0 || len > MIB as usize || offset.checked_add(len as u64).is_none_or(|end| end > size)
    {
        return Err("恢复检查范围越界。".into());
    }
    let mut b = vec![0; len];
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(&mut b))
        .map_err(|e| e.to_string())?;
    Ok(b)
}
pub(crate) fn current_layout(
    head: &[u8],
    tail: &[u8],
    size: u64,
    sector: u64,
) -> Result<(Layout, Vec<String>), String> {
    let report = table_check::inspect_image(head, tail, size, "restore-target.img")?.report;
    if report.sector_size != Some(sector)
        || report.scheme != "GPT"
        || report.items.iter().any(|i| {
            i.code == "sector-ambiguity"
                || i.code == "gpt-agreement" && i.state == CheckState::Failed
        })
    {
        return Err("当前 GPT 结构不唯一或主备表冲突，不能恢复。".into());
    }
    let good = |code: &str| {
        report
            .items
            .iter()
            .any(|i| i.code == code && matches!(i.state, CheckState::Passed | CheckState::Warning))
    };
    let primary = good("primary-gpt") && good("primary-layout");
    let backup = good("backup-gpt") && good("backup-layout");
    if !primary && !backup {
        return Err(
            "当前至少需要一份完整有效的 GPT，用于核对已有分区和保护区域；双表损坏恢复尚未实现。"
                .into(),
        );
    }
    // A hybrid MBR must never be silently replaced by a protective one.
    if (0..4).any(|i| {
        let e = &head[446 + i * 16..462 + i * 16];
        e.iter().any(|&b| b != 0) && e[4] != 0xee
    }) {
        return Err("当前包含非保护性 MBR 条目，不能自动替换混合布局。".into());
    }
    let header = if primary {
        &head[sector as usize..sector as usize * 2]
    } else {
        &tail[tail.len() - sector as usize..]
    };
    let at = u64le(header, 72)
        .checked_mul(sector)
        .ok_or("GPT 条目位置溢出")?;
    let len = u64::from(u32le(header, 80))
        .checked_mul(u64::from(u32le(header, 84)))
        .ok_or("GPT 条目长度溢出")?;
    let table = if at
        .checked_add(len)
        .is_some_and(|end| end <= head.len() as u64)
    {
        &head[at as usize..(at + len) as usize]
    } else if at >= size - tail.len() as u64 && at.checked_add(len).is_some_and(|end| end <= size) {
        let start = (at - (size - tail.len() as u64)) as usize;
        &tail[start..start + len as usize]
    } else {
        return Err("GPT 条目表超出检查窗口。".into());
    };
    let warnings = report
        .items
        .iter()
        .filter(|i| i.state != CheckState::Passed)
        .map(|i| format!("{}：{}", i.title, i.detail))
        .collect();
    Ok((decode_layout(header, table, sector)?, warnings))
}
/// `native` must already agree with complete Windows metadata. `protected` is
/// derived from mandatory target flags plus native service/attribute checks.
pub fn prepare(
    reader: &mut (impl Read + Seek),
    archive: &Archive,
    stable_identity: &str,
    size: u64,
    sector: u64,
    native: &Layout,
    protected: &[String],
    include_boot: bool,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Plan, String> {
    let desired = archive_layout(archive)?;
    if archive.source != stable_identity || archive.size != size || archive.sector != sector {
        return Err("备份来源、容量或逻辑扇区与目标不一致。".into());
    }
    if include_boot && !archive.includes_boot {
        return Err("该备份没有保存分区首扇区。".into());
    }
    checkpoint()?;
    let head = read(reader, 0, MIB as usize, size)?;
    checkpoint()?;
    let tail = read(reader, size - MIB, MIB as usize, size)?;
    let (current, warnings) = current_layout(&head, &tail, size, sector)?;
    if !current.agrees(native) {
        return Err("当前原始 GPT 与 Windows 原生布局不一致，不能恢复。".into());
    }
    if desired.guid != current.guid
        || desired.first != current.first
        || desired.last != current.last
        || desired.count != current.count
    {
        return Err(
            "恢复要求同一磁盘 GUID、可用区域和 GPT 表容量；不改变磁盘身份或表几何。".into(),
        );
    }
    if current
        .entries
        .iter()
        .chain(&desired.entries)
        .any(|e| !known(e))
    {
        return Err("布局包含未知或复杂存储分区类型，尚不支持恢复。".into());
    }
    let is_protected = |e: &Entry| !ordinary(e) || protected.contains(&e.guid);
    for e in &current.entries {
        if is_protected(e) && !desired.entries.contains(e) {
            return Err("恢复会改变已有的受保护分区，已拒绝。".into());
        }
    }
    for e in &desired.entries {
        if !ordinary(e) && !current.entries.contains(e) {
            return Err("恢复不能新增或改变系统、保留或带保护属性的分区。".into());
        }
    }
    let mut changes = vec![];
    for before in &current.entries {
        let after = desired.entries.iter().find(|e| e.guid == before.guid);
        if after != Some(before) {
            changes.push(Change {
                guid: before.guid.clone(),
                before: Some(before.clone()),
                after: after.cloned(),
            });
        }
    }
    for after in &desired.entries {
        if !current.entries.iter().any(|e| e.guid == after.guid) {
            changes.push(Change {
                guid: after.guid.clone(),
                before: None,
                after: Some(after.clone()),
            });
        }
    }
    let mut plan = Plan {
        layout: desired.clone(),
        changes,
        changed_bytes: 0,
        write_bytes: 0,
        warnings,
        size,
        before: vec![
            Snapshot {
                offset: 0,
                bytes: head,
            },
            Snapshot {
                offset: size - MIB,
                bytes: tail,
            },
        ],
        writes: vec![],
    };
    for e in &archive.extents {
        if e.kind == Kind::Boot && !include_boot {
            continue;
        }
        checkpoint()?;
        let before = read(reader, e.offset, e.bytes.len(), size)?;
        if e.kind == Kind::Boot {
            let target = desired
                .entries
                .iter()
                .find(|p| p.offset == e.offset)
                .ok_or("首扇区没有分区关联")?;
            if before != e.bytes && is_protected(target) {
                return Err("恢复会改写受保护分区的首扇区，已拒绝。".into());
            }
            plan.before.push(Snapshot {
                offset: e.offset,
                bytes: before.clone(),
            });
        }
        if before == e.bytes {
            continue;
        }
        plan.changed_bytes += before.iter().zip(&e.bytes).filter(|(a, b)| a != b).count() as u64;
        plan.write_bytes += e.bytes.len() as u64;
        plan.writes.push(WriteRange {
            kind: e.kind.clone(),
            offset: e.offset,
            before,
            after: e.bytes.clone(),
        });
    }
    // GPT backup array/header must become durable before primary array/header.
    plan.writes.sort_by_key(|w| {
        (
            match w.kind {
                Kind::Boot => 0,
                Kind::BackupEntries => 1,
                Kind::BackupHeader => 2,
                Kind::PrimaryEntries => 3,
                Kind::PrimaryHeader => 4,
                Kind::Mbr => 5,
                Kind::Ebr => 6,
            },
            w.offset,
        )
    });
    plan.matches_before(reader, checkpoint)?;
    Ok(plan)
}

#[cfg(test)]
#[path = "table_restore_tests.rs"]
mod tests;
impl Plan {
    pub fn ranges(&self) -> usize {
        self.writes.len()
    }
    pub fn matches_before(
        &self,
        reader: &mut (impl Read + Seek),
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for s in &self.before {
            checkpoint()?;
            if read(reader, s.offset, s.bytes.len(), self.size)? != s.bytes {
                return Err("预览后目标内容已变化，未写入分区表。".into());
            }
        }
        for s in &self.writes {
            checkpoint()?;
            if read(reader, s.offset, s.before.len(), self.size)? != s.before {
                return Err("恢复范围与预览不同，未写入分区表。".into());
            }
        }
        Ok(())
    }
    pub fn execute<R: Read + Write + Seek>(
        &self,
        file: &mut R,
        mut sync: impl FnMut(&mut R) -> Result<(), String>,
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<Outcome, String> {
        self.matches_before(file, &mut checkpoint)?;
        let mut digest = Sha256::new();
        let mut written = 0;
        for (index, w) in self.writes.iter().enumerate() {
            let result = (|| -> Result<(), String> {
                checkpoint()?;
                file.seek(SeekFrom::Start(w.offset))
                    .map_err(|e| e.to_string())?;
                let n = file
                    .write(&w.after)
                    .map_err(|e| format!("写入请求失败，可能部分写入：{e}"))?;
                if n != w.after.len() {
                    return Err(format!("短写 {n}/{} 字节", w.after.len()));
                }
                sync(file)?;
                let bytes = read(file, w.offset, w.after.len(), self.size)?;
                if hash(&bytes) != hash(&w.after) {
                    return Err("SHA-256 回读不一致".into());
                }
                Ok(())
            })();
            result.map_err(|e|format!("恢复停止于 {:?}、偏移 {}；此前已校验 {index} 个范围、{written} 字节：{e}。未重试或回滚。",w.kind,w.offset))?;
            digest.update(w.offset.to_le_bytes());
            digest.update((w.after.len() as u64).to_le_bytes());
            digest.update(&w.after);
            written += w.after.len() as u64;
        }
        self.verify_after(file)?;
        Ok(Outcome {
            written_bytes: written,
            ranges: self.writes.len(),
            sha256: format!("{:x}", digest.finalize()),
        })
    }
    pub fn verify_after(&self, file: &mut (impl Read + Seek)) -> Result<(), String> {
        // Include untouched bytes of both guard windows in the postcondition.
        for s in &self.before {
            let mut expected = s.bytes.clone();
            for w in &self.writes {
                let start = s.offset.max(w.offset);
                let end = (s.offset + s.bytes.len() as u64).min(w.offset + w.after.len() as u64);
                if start < end {
                    expected[(start - s.offset) as usize..(end - s.offset) as usize]
                        .copy_from_slice(
                            &w.after[(start - w.offset) as usize..(end - w.offset) as usize],
                        );
                }
            }
            if read(file, s.offset, expected.len(), self.size)? != expected {
                return Err("写后完整范围校验不一致；可能已有部分恢复，未回滚。".into());
            }
        }
        Ok(())
    }
}

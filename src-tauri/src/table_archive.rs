//! Partition-table archives over bounded readers, with no paths or device access.
//! Archive extents are derived again from validated tables before being trusted.
use disk_core::{
    mbr::EbrSector,
    table_check::{self, CheckState, TableReport},
    Disk, MIB,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

pub const MAX_FILE: usize = 24 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"DSPT0001";
const PREFIX: usize = 44;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Mbr,
    PrimaryHeader,
    PrimaryEntries,
    BackupEntries,
    BackupHeader,
    Ebr,
    Boot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Extent {
    pub kind: Kind,
    pub offset: u64,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Archive {
    pub version: u32,
    pub size: u64,
    pub sector: u64,
    pub source: String,
    pub includes_boot: bool,
    pub extents: Vec<Extent>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub scheme: String,
    pub size: u64,
    pub sector: u64,
    pub partitions: usize,
    pub table_bytes: u64,
    pub boot_bytes: u64,
    pub sha256: String,
    pub report: TableReport,
    pub layout: Vec<PartitionRange>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionRange {
    pub name: String,
    pub offset: u64,
    pub size: u64,
}
struct Layout {
    disk: Disk,
    report: TableReport,
    ranges: Vec<(Kind, u64, usize)>,
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn geometry(size: u64, sector: u64) -> Result<(), String> {
    if !(4 * MIB..=1 << 43).contains(&size) || !matches!(sector, 512 | 4096) || size % sector != 0 {
        return Err("分区表备份当前支持 4 MiB–8 TiB、512/4096 字节逻辑扇区。".into());
    }
    Ok(())
}
pub(crate) fn read_at(
    reader: &mut (impl Read + Seek),
    at: u64,
    len: usize,
    size: u64,
) -> Result<Vec<u8>, String> {
    if len == 0 || len > MIB as usize || at.checked_add(len as u64).is_none_or(|n| n > size) {
        return Err("分区表读取范围越界。".into());
    }
    let mut bytes = vec![0; len];
    reader
        .seek(SeekFrom::Start(at))
        .and_then(|_| reader.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn layout(
    head: &[u8],
    tail: &[u8],
    size: u64,
    sector: u64,
    ebrs: &[EbrSector],
    boot: bool,
) -> Result<Layout, String> {
    let inspected =
        table_check::inspect_image_with_ebrs(head, tail, size, "partition-table.img", ebrs)?;
    if matches!(
        inspected.report.state,
        CheckState::Failed | CheckState::Unchecked
    ) {
        return Err(format!(
            "分区表未通过完整检查，不能生成可恢复备份：{}",
            inspected
                .report
                .items
                .iter()
                .filter(|i| matches!(i.state, CheckState::Failed | CheckState::Unchecked))
                .map(|i| i.detail.as_str())
                .collect::<Vec<_>>()
                .join("；")
        ));
    }
    let disk = inspected
        .disk
        .ok_or_else(|| inspected.parse_error.unwrap_or("分区表不可解析".into()))?;
    if disk.sector_size != sector {
        return Err("分区表与设备报告的逻辑扇区大小不一致。".into());
    }
    let mut ranges = vec![(Kind::Mbr, 0, sector as usize)];
    if disk.scheme == "GPT" {
        if !ebrs.is_empty() {
            return Err("GPT 备份不能包含 EBR 数据。".into());
        }
        for (header, at, hk, ek) in [
            (
                &head[sector as usize..sector as usize * 2],
                sector,
                Kind::PrimaryHeader,
                Kind::PrimaryEntries,
            ),
            (
                &tail[tail.len() - sector as usize..],
                size - sector,
                Kind::BackupHeader,
                Kind::BackupEntries,
            ),
        ] {
            let bytes = u64::from(u32le(header, 80)) * u64::from(u32le(header, 84));
            let table_at = u64le(header, 72)
                .checked_mul(sector)
                .ok_or("GPT 位置溢出")?;
            let len = bytes
                .div_ceil(sector)
                .checked_mul(sector)
                .ok_or("GPT 长度溢出")?;
            if len > MIB {
                return Err("GPT 条目表超过备份上限。".into());
            }
            ranges.push((hk, at, sector as usize));
            ranges.push((ek, table_at, len as usize));
        }
    } else {
        // The existing MBR/EBR validator deliberately supports 512-byte LBAs.
        // A known 4Kn device must never be silently interpreted as 512 B.
        if sector != 512 {
            return Err("4Kn MBR 备份尚未实现。".into());
        }
        ranges.extend(ebrs.iter().map(|e| (Kind::Ebr, e.offset, 512)));
    }
    if boot {
        ranges.extend(
            disk.partitions
                .iter()
                .filter(|p| p.fs != "—")
                .map(|p| (Kind::Boot, p.start, sector as usize)),
        );
    }
    ranges.sort_by_key(|r| r.1);
    let mut end = 0;
    for (_, at, len) in &ranges {
        if *at < end {
            return Err("分区表与引导扇区范围重叠。".into());
        }
        end = at.checked_add(*len as u64).ok_or("分区表范围溢出")?;
        if end > size {
            return Err("分区表范围超出磁盘。".into());
        }
    }
    Ok(Layout {
        disk,
        report: inspected.report,
        ranges,
    })
}
pub fn capture(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    source: String,
    includes_boot: bool,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<Archive, String> {
    geometry(size, sector)?;
    checkpoint()?;
    let head = read_at(reader, 0, MIB as usize, size)?;
    checkpoint()?;
    let tail = read_at(reader, size - MIB, MIB as usize, size)?;
    let mut ebrs = vec![];
    if sector == 512
        && ![512, 4096]
            .iter()
            .any(|&n| head.get(n..n + 8) == Some(b"EFI PART"))
    {
        while let Some(at) = disk_core::mbr::next_ebr(&head[..512], size, &ebrs)? {
            checkpoint()?;
            let bytes = read_at(reader, at, 512, size)?;
            ebrs.push(EbrSector { offset: at, bytes });
        }
    }
    let parsed = layout(&head, &tail, size, sector, &ebrs, includes_boot)?;
    let mut extents = vec![];
    for (kind, offset, len) in parsed.ranges {
        checkpoint()?;
        let bytes = read_at(reader, offset, len, size)?;
        if kind != Kind::Boot {
            let original = if offset + len as u64 <= MIB {
                Some(&head[offset as usize..offset as usize + len])
            } else if offset >= size - MIB {
                Some(
                    &tail[(offset - (size - MIB)) as usize..(offset - (size - MIB)) as usize + len],
                )
            } else {
                ebrs.iter()
                    .find(|e| e.offset == offset)
                    .map(|e| e.bytes.as_slice())
            };
            if original != Some(bytes.as_slice()) {
                return Err("读取期间分区表发生变化，备份未完成。".into());
            }
        }
        extents.push(Extent {
            kind,
            offset,
            bytes,
        });
    }
    let archive = Archive {
        version: 1,
        size,
        sector,
        source,
        includes_boot,
        extents,
    };
    archive.validate()?;
    // Re-read every retained extent, including boot sectors, before publishing.
    archive.matches(reader, checkpoint)?;
    Ok(archive)
}
impl Archive {
    pub fn validate(&self) -> Result<Info, String> {
        geometry(self.size, self.sector)?;
        if self.version != 1
            || self.source.len() != 64
            || !self
                .source
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.extents.is_empty()
            || self.extents.len() > 256
        {
            return Err("分区表备份版本、来源标识或片段数量无效。".into());
        }
        let mut head = vec![0; MIB as usize];
        let mut tail = vec![0; MIB as usize];
        let mut ebrs = vec![];
        let mut end = 0;
        let mut total = 0usize;
        for e in &self.extents {
            if e.bytes.is_empty()
                || e.bytes.len() > MIB as usize
                || e.offset % self.sector != 0
                || e.bytes.len() as u64 % self.sector != 0
                || e.offset < end
            {
                return Err("备份片段未排序、重叠或未对齐。".into());
            }
            end = e
                .offset
                .checked_add(e.bytes.len() as u64)
                .ok_or("备份片段溢出")?;
            total = total.checked_add(e.bytes.len()).ok_or("备份长度溢出")?;
            if end > self.size || total > 3 * MIB as usize {
                return Err("备份片段范围或总量越界。".into());
            }
            if e.offset < MIB {
                let len = e.bytes.len().min((MIB - e.offset) as usize);
                head[e.offset as usize..e.offset as usize + len].copy_from_slice(&e.bytes[..len]);
            }
            if end > self.size - MIB {
                let start = e.offset.max(self.size - MIB);
                tail[(start - (self.size - MIB)) as usize..(end - (self.size - MIB)) as usize]
                    .copy_from_slice(&e.bytes[(start - e.offset) as usize..]);
            }
            if e.kind == Kind::Ebr {
                ebrs.push(EbrSector {
                    offset: e.offset,
                    bytes: e.bytes.clone(),
                });
            }
        }
        let parsed = layout(
            &head,
            &tail,
            self.size,
            self.sector,
            &ebrs,
            self.includes_boot,
        )?;
        if parsed.ranges.len() != self.extents.len()
            || parsed
                .ranges
                .iter()
                .zip(&self.extents)
                .any(|((kind, at, len), e)| {
                    *kind != e.kind || *at != e.offset || *len != e.bytes.len()
                })
        {
            return Err("备份片段与分区表推导的完整范围不一致。".into());
        }
        let payload = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        Ok(Info {
            scheme: parsed.disk.scheme,
            size: self.size,
            sector: self.sector,
            partitions: parsed
                .disk
                .partitions
                .iter()
                .filter(|p| p.fs != "—")
                .count(),
            table_bytes: self
                .extents
                .iter()
                .filter(|e| e.kind != Kind::Boot)
                .map(|e| e.bytes.len() as u64)
                .sum(),
            boot_bytes: self
                .extents
                .iter()
                .filter(|e| e.kind == Kind::Boot)
                .map(|e| e.bytes.len() as u64)
                .sum(),
            sha256: hash(&payload),
            layout: parsed
                .disk
                .partitions
                .iter()
                .filter(|p| p.fs != "—")
                .map(|p| PartitionRange {
                    name: p.name.clone(),
                    offset: p.start,
                    size: p.size,
                })
                .collect(),
            report: parsed.report,
        })
    }
    pub fn matches(
        &self,
        reader: &mut (impl Read + Seek),
        mut checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for e in &self.extents {
            checkpoint()?;
            if read_at(reader, e.offset, e.bytes.len(), self.size)? != e.bytes {
                return Err("备份范围的源内容已变化，请重新读取。".into());
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let payload = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if payload.len() > MAX_FILE - PREFIX {
            return Err("分区表备份超过文件大小上限。".into());
        }
        let mut file = Vec::with_capacity(PREFIX + payload.len());
        file.extend_from_slice(MAGIC);
        file.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        file.extend_from_slice(&Sha256::digest(&payload));
        file.extend_from_slice(&payload);
        Ok(file)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < PREFIX
            || bytes.len() > MAX_FILE
            || &bytes[..8] != MAGIC
            || u32le(bytes, 8) as usize != bytes.len() - PREFIX
            || Sha256::digest(&bytes[PREFIX..]).as_slice() != &bytes[12..PREFIX]
        {
            return Err("分区表备份长度、版本或 SHA-256 校验失败。".into());
        }
        let value: Self = serde_json::from_slice(&bytes[PREFIX..])
            .map_err(|e| format!("分区表备份结构无效：{e}"))?;
        value.validate()?;
        Ok(value)
    }
}

#[cfg(test)]
#[path = "table_archive_tests.rs"]
pub(crate) mod tests;

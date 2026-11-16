//! File-level access to verified backup content. Accepts retained readers only;
//! never opens a device, mounts a volume, or expands an archive to disk.
use crate::{
    backup_archive::{Chain, SharedReader, Source},
    data_io::Region,
};
use serde::Serialize;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    pub id: u32,
    pub name: String,
    pub start: u64,
    pub size: u64,
}

#[cfg(test)]
#[path = "backup_browser_tests.rs"]
mod tests;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub id: String,
    pub label: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub kind: String,
    pub size: u64,
    pub sector: u64,
    pub layers: usize,
    pub points: Vec<Point>,
    pub selected_point: String,
    pub partitions: Vec<Part>,
    pub warning: String,
}
pub struct Browser {
    content: Content,
    pub info: Info,
}
enum Content {
    V2 {
        chain: Arc<Mutex<Chain>>,
        layer: usize,
    },
    V1 {
        source: Arc<Mutex<Box<dyn Source>>>,
        header: crate::data_tasks::BackupHeader,
    },
}
struct LegacyReader {
    source: Arc<Mutex<Box<dyn Source>>>,
    pos: u64,
}
impl Read for LegacyReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut file = self
            .source
            .lock()
            .map_err(|_| io::Error::other("备份读取状态异常"))?;
        file.seek(SeekFrom::Start(self.pos))?;
        let n = file.read(bytes)?;
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for LegacyReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut file = self
            .source
            .lock()
            .map_err(|_| io::Error::other("备份读取状态异常"))?;
        file.seek(SeekFrom::Start(self.pos))?;
        self.pos = file.seek(from)?;
        Ok(self.pos)
    }
}
impl Browser {
    pub fn provenance(
        &self,
        partition: u32,
    ) -> Result<crate::recovery_export::BackupSource, String> {
        let point = self
            .info
            .points
            .iter()
            .find(|p| p.id == self.info.selected_point)
            .ok_or("备份还原点不存在")?;
        let part = self
            .info
            .partitions
            .iter()
            .find(|p| p.id == partition)
            .ok_or("备份分区不存在")?;
        Ok(crate::recovery_export::BackupSource {
            archive_name: self.info.name.clone(),
            point_id: point.id.clone(),
            point_label: point.label.clone(),
            partition: part.name.clone(),
        })
    }
    pub fn source_label(&self, partition: u32) -> Result<String, String> {
        let source = self.provenance(partition)?;
        Ok(format!(
            "{} / {} / {}",
            source.archive_name, source.point_label, source.partition
        ))
    }
    pub fn from_chain(
        chain: Chain,
        name: String,
        tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let h = chain.header().clone();
        let points = chain
            .headers()
            .iter()
            .enumerate()
            .map(|(i, h)| Point {
                id: h.id.clone(),
                label: if i == 0 {
                    if h.parent.is_some() {
                        "最新增量备份"
                    } else {
                        "完整备份"
                    }
                    .into()
                } else if h.parent.is_none() {
                    "最早完整备份".into()
                } else {
                    format!("此前第 {i} 层增量备份")
                },
            })
            .collect();
        let info = Info {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            version: h.version,
            kind: h.kind,
            size: h.size,
            sector: h.sector,
            layers: chain.depth(),
            points,
            selected_point: h.id,
            partitions: vec![],
            warning: String::new(),
        };
        Self::prepare(
            Content::V2 {
                chain: Arc::new(Mutex::new(chain)),
                layer: 0,
            },
            info,
            tick,
        )
    }
    pub fn from_legacy(
        mut source: impl Source + 'static,
        name: String,
        tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let h = crate::data_tasks::read_header(&mut source)?;
        let info = Info {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            version: 1,
            kind: h.kind.clone(),
            size: h.size,
            sector: h.sector,
            layers: 1,
            points: vec![Point {
                id: "legacy".into(),
                label: "完整备份".into(),
            }],
            selected_point: "legacy".into(),
            partitions: vec![],
            warning: String::new(),
        };
        Self::prepare(
            Content::V1 {
                source: Arc::new(Mutex::new(Box::new(source))),
                header: h,
            },
            info,
            tick,
        )
    }
    fn prepare(
        content: Content,
        info: Info,
        mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut browser = Self { content, info };
        let size = browser.info.size;
        tick(0, size)?;
        match &browser.content {
            Content::V2 { chain, layer } => chain
                .lock()
                .map_err(|_| "备份读取状态异常")?
                .verify_at(*layer, |n| tick(n, size))?,
            Content::V1 { header, .. } => {
                let digest =
                    crate::data_io::hash_exact(&mut browser.content_reader()?, size, |n| {
                        tick(n, size)
                    })?;
                if digest != header.sha256 {
                    return Err("完整备份 SHA-256 校验失败".into());
                }
            }
        }
        if let Content::V2 { chain, layer } = &browser.content {
            let locked = chain.lock().map_err(|_| "备份读取状态异常")?;
            let headers = locked.headers();
            if let Some(policy) = &headers[*layer].allocation {
                browser.info.warning = format!(
                    "按已用空间备份：{} B 已确认空闲的区域按零保存，不保留这些区域的已删除内容。",
                    policy.skipped_bytes
                );
            }
        }
        if browser.info.kind == "partition" {
            browser.info.partitions.push(Part {
                id: 0,
                name: "备份分区".into(),
                start: 0,
                size,
            });
        } else {
            let mut reader = browser.content_reader()?;
            let count = size.min(disk_core::MIB) as usize;
            let mut head = vec![0; count];
            let mut tail = vec![0; count];
            reader.read_exact(&mut head).map_err(|e| e.to_string())?;
            reader
                .seek(SeekFrom::Start(size - count as u64))
                .map_err(|e| e.to_string())?;
            reader.read_exact(&mut tail).map_err(|e| e.to_string())?;
            let mut ebrs = vec![];
            if head.get(510..512) == Some(&[0x55, 0xaa])
                && ![512, 4096]
                    .iter()
                    .any(|n| head.get(*n..*n + 8) == Some(b"EFI PART"))
            {
                while let Some(offset) = disk_core::mbr::next_ebr(&head[..512], size, &ebrs)? {
                    tick(size, size)?;
                    let mut bytes = vec![0; 512];
                    reader
                        .seek(SeekFrom::Start(offset))
                        .map_err(|e| e.to_string())?;
                    reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                    ebrs.push(disk_core::mbr::EbrSector { offset, bytes });
                }
            }
            let inspected = disk_core::table_check::inspect_image_with_ebrs(
                &head,
                &tail,
                size,
                "backup-content.img",
                &ebrs,
            )?;
            if matches!(
                inspected.report.state,
                disk_core::table_check::CheckState::Failed
                    | disk_core::table_check::CheckState::Unchecked
            ) {
                return Err(format!(
                    "备份内容校验通过，但分区表不完整或存在错误：{}",
                    inspected
                        .report
                        .items
                        .iter()
                        .filter(|i| matches!(
                            i.state,
                            disk_core::table_check::CheckState::Failed
                                | disk_core::table_check::CheckState::Unchecked
                        ))
                        .map(|i| i.detail.as_str())
                        .collect::<Vec<_>>()
                        .join("；")
                ));
            }
            let disk = inspected
                .disk
                .ok_or_else(|| inspected.parse_error.unwrap_or("无法读取备份分区表".into()))?;
            if disk.sector_size != browser.info.sector {
                return Err("备份记录的扇区大小与分区表不一致".into());
            }
            browser.info.warning.push_str(
                &inspected
                    .report
                    .items
                    .iter()
                    .filter(|i| i.state == disk_core::table_check::CheckState::Warning)
                    .map(|i| i.detail.as_str())
                    .collect::<Vec<_>>()
                    .join("；"),
            );
            browser.info.partitions = disk
                .partitions
                .into_iter()
                .filter(|p| p.color != "free")
                .enumerate()
                .map(|(id, p)| Part {
                    id: id as u32,
                    name: p.name,
                    start: p.start,
                    size: p.size,
                })
                .collect();
        }
        tick(size, size)?;
        Ok(browser)
    }
    fn content_reader(&self) -> Result<Box<dyn Source>, String> {
        match &self.content {
            Content::V2 { chain, layer } => Ok(Box::new(SharedReader::at(chain.clone(), *layer)?)),
            Content::V1 { source, header } => Ok(Box::new(
                Region::new(
                    LegacyReader {
                        source: source.clone(),
                        pos: 0,
                    },
                    4096,
                    header.size,
                )
                .map_err(|e| e.to_string())?,
            )),
        }
    }
    /// New immutable view over the retained chain. Readers already handed out
    /// keep their original layer; there is no global mutable current point.
    pub fn select_point(
        &self,
        point: &str,
        tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let Content::V2 { chain, .. } = &self.content else {
            return Err("第一版完整备份没有可切换的还原点".into());
        };
        let headers = chain.lock().map_err(|_| "备份读取状态异常")?.headers();
        let layer = headers
            .iter()
            .position(|h| h.id == point)
            .ok_or("备份还原点不存在")?;
        let h = &headers[layer];
        let mut info = self.info.clone();
        info.id = uuid::Uuid::new_v4().to_string();
        info.selected_point = h.id.clone();
        info.size = h.size;
        info.sector = h.sector;
        info.kind = h.kind.clone();
        info.version = h.version;
        info.partitions.clear();
        info.warning.clear();
        Self::prepare(
            Content::V2 {
                chain: chain.clone(),
                layer,
            },
            info,
            tick,
        )
    }
    pub fn reader(&self, part: u32) -> Result<Region<Box<dyn Source>>, String> {
        let part = self
            .info
            .partitions
            .iter()
            .find(|p| p.id == part)
            .ok_or("备份分区选择无效")?;
        if part
            .start
            .checked_add(part.size)
            .is_none_or(|n| n > self.info.size)
        {
            return Err("备份分区越界".into());
        }
        Region::new(self.content_reader()?, part.start, part.size).map_err(|e| e.to_string())
    }
}

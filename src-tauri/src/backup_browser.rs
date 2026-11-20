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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_format: Option<RestoreFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreFormat {
    Fat,
    Exfat,
    Ntfs,
    Btrfs,
    Ext,
}

#[cfg(test)]
#[path = "backup_browser_tests.rs"]
mod tests;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<usize>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_issue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation_skipped_bytes: Option<u64>,
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
                ordinal: (i != 0 && h.parent.is_some()).then_some(i),
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
            table_issue: None,
            allocation_skipped_bytes: None,
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
                ordinal: None,
            }],
            selected_point: "legacy".into(),
            partitions: vec![],
            warning: String::new(),
            table_issue: None,
            allocation_skipped_bytes: None,
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
        browser.verify_content(&mut tick)?;
        if let Content::V2 { chain, layer } = &browser.content {
            let locked = chain.lock().map_err(|_| "备份读取状态异常")?;
            let headers = locked.headers();
            browser.info.allocation_skipped_bytes = headers[*layer]
                .allocation
                .as_ref()
                .map(|policy| policy.skipped_bytes);
        }
        if browser.info.kind == "partition" {
            browser.info.partitions.push(Part {
                id: 0,
                name: "备份分区".into(),
                start: 0,
                size,
                restore_format: None,
            });
        } else {
            browser.index_disk(&mut tick)?;
        }
        let mut content = browser.content_reader()?;
        for part in &mut browser.info.partitions {
            tick(size, size)?;
            let mut extent =
                Region::new(&mut content, part.start, part.size).map_err(|e| e.to_string())?;
            part.restore_format = restore_format(&mut extent, part.size)?;
        }
        tick(size, size)?;
        Ok(browser)
    }

    // A bad partition table blocks all partition access. The independently
    // verified disk byte stream remains available for an explicit disk restore.
    fn index_disk(
        &mut self,
        mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let size = self.info.size;
        let mut reader = self.content_reader()?;
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
            loop {
                let offset = match disk_core::mbr::next_ebr(&head[..512], size, &ebrs) {
                    Ok(Some(offset)) => offset,
                    Ok(None) => break,
                    Err(issue) => {
                        self.info.table_issue = Some(issue);
                        return Ok(());
                    }
                };
                tick(size, size)?;
                let mut bytes = vec![0; 512];
                reader
                    .seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                ebrs.push(disk_core::mbr::EbrSector { offset, bytes });
            }
        }
        let inspected = match disk_core::table_check::inspect_image_with_ebrs(
            &head,
            &tail,
            size,
            "backup-content.img",
            &ebrs,
        ) {
            Ok(inspected) => inspected,
            Err(issue) => {
                self.info.table_issue = Some(issue);
                return Ok(());
            }
        };
        if matches!(
            inspected.report.state,
            disk_core::table_check::CheckState::Failed
                | disk_core::table_check::CheckState::Unchecked
        ) {
            self.info.table_issue = Some(
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
                    .join("；"),
            );
            return Ok(());
        }
        let Some(disk) = inspected.disk else {
            self.info.table_issue = Some(
                inspected.parse_error.unwrap_or_else(|| "无法读取备份分区表".into()),
            );
            return Ok(());
        };
        if disk.sector_size != self.info.sector {
            self.info.table_issue = Some("备份记录的扇区大小与分区表不一致".into());
            return Ok(());
        }
        self.info.warning =
            inspected
                .report
                .items
                .iter()
                .filter(|i| i.state == disk_core::table_check::CheckState::Warning)
                .map(|i| i.detail.as_str())
                .collect::<Vec<_>>()
                .join("；");
        self.info.partitions = disk
            .partitions
            .into_iter()
            .filter(|p| p.color != "free")
            .enumerate()
            .map(|(id, p)| Part {
                id: id as u32,
                name: p.name,
                start: p.start,
                size: p.size,
                restore_format: None,
            })
            .collect();
        Ok(())
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
    /// Verify the retained point and all of its dependencies again before a
    /// destructive restore. An older point has its own immutable chain view.
    pub(crate) fn verify_content(
        &self,
        mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    ) -> Result<String, String> {
        let size = self.info.size;
        tick(0, size)?;
        match &self.content {
            Content::V2 { chain, layer } => {
                let mut chain = chain.lock().map_err(|_| "备份读取状态异常")?;
                chain.verify_at(*layer, |n| tick(n, size))?;
                Ok(chain.headers()[*layer].sha256.clone())
            }
            Content::V1 { header, .. } => {
                let digest =
                    crate::data_io::hash_exact(&mut self.content_reader()?, size, |n| tick(n, size))?;
                if digest != header.sha256 {
                    return Err("完整备份 SHA-256 校验失败".into());
                }
                Ok(digest)
            }
        }
    }

    pub(crate) fn restore_size(&self, part: Option<u32>) -> Result<u64, String> {
        match part {
            Some(id) => self
                .info
                .partitions
                .iter()
                .find(|part| part.id == id)
                .map(|part| part.size)
                .ok_or("备份分区选择无效".into()),
            None if self.info.kind == "disk" => Ok(self.info.size),
            None => Err("分区备份需要选择分区还原。".into()),
        }
    }

    pub(crate) fn restore_reader(&self, part: Option<u32>) -> Result<Box<dyn Source>, String> {
        self.restore_size(part)?;
        match part {
            Some(id) => Ok(Box::new(self.reader(id)?)),
            None => self.content_reader(),
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
        info.table_issue = None;
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

// Route only on bytes from a verified backup point. A Windows filesystem
// can retain an older signature well past its current boot sector.
fn restore_format(
    reader: &mut (impl Read + Seek),
    size: u64,
) -> Result<Option<RestoreFormat>, String> {
    let mut boot = [0; 512];
    if size < boot.len() as u64 {
        return Ok(None);
    }
    reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    reader.read_exact(&mut boot).map_err(|e| e.to_string())?;
    if crate::bitlocker::boot_marker(&boot) {
        Ok(None)
    } else if &boot[3..11] == b"NTFS    " {
        Ok(Some(RestoreFormat::Ntfs))
    } else if crate::recovery::fat_boot(&boot) {
        Ok(Some(if &boot[3..11] == b"EXFAT   " {
            RestoreFormat::Exfat
        } else {
            RestoreFormat::Fat
        }))
    } else if disk_core::btrfs::has_magic(reader, size)? {
        Ok(Some(RestoreFormat::Btrfs))
    } else {
        Ok(crate::ext_allocation::has_magic(reader, size)?.then_some(RestoreFormat::Ext))
    }
}

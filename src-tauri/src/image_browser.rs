//! Read-only logical image views. The caller retains an immutable ordinary file
//! or decoder factory; this module never opens a path or a device.
use crate::{backup_archive::Source, data_io::Region};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Disk,
    Partition,
    Search,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub kind: Kind,
    pub sector: u64,
}
impl Options {
    pub fn validate(self, size: u64) -> Result<(), String> {
        if !matches!(self.sector, 512 | 4096)
            || !(1024 * 1024..=1u64 << 43).contains(&size)
            || size % self.sector != 0
        {
            return Err("镜像逻辑容量须为 1 MiB–8 TiB，且按 512 B 或 4096 B 扇区对齐。".into());
        }
        Ok(())
    }
}
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    pub id: u32,
    pub name: String,
    pub start: u64,
    pub size: u64,
}
#[derive(Clone, Serialize)]
pub(crate) struct ParallelsPoint {
    pub id: uuid::Uuid,
    pub name: String,
    pub current: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub options: Options,
    pub partitions: Vec<Part>,
    pub warning: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verification_notes: Vec<String>,
    pub format: String,
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parallels_snapshot: Option<ParallelsPoint>,
}
pub trait ReaderFactory: Send + Sync {
    fn reader(&self) -> Result<Box<dyn Source>, String>;
}
pub struct Browser {
    source: Option<Arc<Mutex<Box<dyn Source>>>>,
    factory: Option<Arc<dyn ReaderFactory>>,
    pub info: Info,
}
struct SharedReader {
    source: Arc<Mutex<Box<dyn Source>>>,
    pos: u64,
    size: u64,
}
impl Read for SharedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min((self.size - self.pos) as usize);
        if n == 0 {
            return Ok(0);
        }
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("镜像读取状态异常"))?;
        source.seek(SeekFrom::Start(self.pos))?;
        let n = source.read(&mut out[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for SharedReader {
    fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
        let n = match at {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(n) => self.size as i128 + n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if n < 0 || n > self.size as i128 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "镜像读取越界"));
        }
        self.pos = n as u64;
        Ok(self.pos)
    }
}

pub(crate) struct CheckedLayout {
    pub disk: disk_core::Disk,
    pub warning: String,
    pub head: Vec<u8>,
    pub gpt_identity: Option<disk_core::table_check::GptIdentity>,
}

pub(crate) fn checked_layout(
    source: &mut impl Source,
    name: &str,
    size: u64,
    sector: u64,
    tick: &mut impl FnMut() -> Result<(), String>,
) -> Result<CheckedLayout, String> {
    let count = size.min(disk_core::MIB) as usize;
    let mut head = vec![0; count];
    let mut tail = vec![0; count];
    source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    source.read_exact(&mut head).map_err(|e| e.to_string())?;
    source.seek(SeekFrom::Start(size - count as u64)).map_err(|e| e.to_string())?;
    source.read_exact(&mut tail).map_err(|e| e.to_string())?;
    let ebrs = crate::table_archive::read_ebrs(source, &head, size, sector, tick)?;
    let inspected = disk_core::table_check::inspect_image_with_geometry(
        &head, &tail, size, name, Some(sector), &ebrs,
    )?;
    use disk_core::table_check::CheckState;
    if matches!(inspected.report.state, CheckState::Failed | CheckState::Unchecked) {
        return Err(format!(
            "镜像分区表未通过检查：{}",
            inspected.report.items.iter()
                .filter(|i| matches!(i.state, CheckState::Failed | CheckState::Unchecked))
                .map(|i| i.detail.as_str()).collect::<Vec<_>>().join("；")
        ));
    }
    let disk = inspected.disk.ok_or_else(|| {
        inspected.parse_error.unwrap_or("镜像没有可读取的分区表；分区文件请选择“分区镜像”。".into())
    })?;
    if disk.sector_size != sector {
        return Err("所选扇区大小与 GPT 分区表不一致，请重新选择。".into());
    }
    let warning = inspected.report.items.iter().filter(|i| i.state == CheckState::Warning)
        .map(|i| i.detail.as_str()).collect::<Vec<_>>().join("；");
    Ok(CheckedLayout { disk, warning, head, gpt_identity: inspected.gpt_identity })
}

impl Browser {
    pub fn open(
        mut source: impl Source + 'static,
        name: String,
        options: Options,
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let size = source.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        options.validate(size)?;
        tick()?;
        let mut warning = String::new();
        let partitions = match options.kind {
            Kind::Search => {
                warning = "已打开整盘镜像以搜索丢失分区。".into();
                Vec::new()
            }
            Kind::Partition => vec![Part {
                id: 0,
                name: "镜像分区".into(),
                start: 0,
                size,
            }],
            Kind::Disk => {
                let layout = checked_layout(&mut source, &name, size, options.sector, &mut tick)?;
                warning = layout.warning;
                layout.disk.partitions
                    .into_iter()
                    .filter(|p| p.color != "free")
                    .enumerate()
                    .map(|(id, p)| Part {
                        id: id as u32,
                        name: p.name,
                        start: p.start,
                        size: p.size,
                    })
                    .collect()
            }
        };
        for part in &partitions {
            if part.size == 0 || part.start.checked_add(part.size).is_none_or(|n| n > size) {
                return Err("镜像分区范围无效。".into());
            }
        }
        tick()?;
        Ok(Self {
            source: Some(Arc::new(Mutex::new(Box::new(source)))),
            factory: None,
            info: Info {
                id: uuid::Uuid::new_v4().to_string(),
                files: vec![name.clone()],
                name,
                format: "RAW".into(),
                size,
                options,
                partitions,
                warning,
                verification_notes: Vec::new(),
                parallels_snapshot: None,
            },
        })
    }
    pub fn from_factory(
        factory: Arc<dyn ReaderFactory>,
        name: String,
        options: Options,
        format: String,
        files: Vec<String>,
        notes: String,
        tick: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut browser = Self::open(factory.reader()?, name, options, tick)?;
        // Release the initial bridge reader after table inspection. Every later
        // filesystem operation gets an independent parser, cache and budget.
        browser.source = None;
        browser.factory = Some(factory);
        browser.info.format = format;
        browser.info.files = files;
        if !notes.is_empty() {
            if !browser.info.warning.is_empty() {
                browser.info.warning.push('；');
            }
            browser.info.warning.push_str(&notes);
        }
        Ok(browser)
    }
    pub fn part(&self, id: u32) -> Result<&Part, String> {
        self.info
            .partitions
            .iter()
            .find(|p| p.id == id)
            .ok_or("镜像分区选择已失效。".into())
    }
    pub(crate) fn logical_reader(&self) -> Result<Box<dyn Source>, String> {
        if let Some(factory) = &self.factory {
            return factory.reader();
        }
        Ok(Box::new(SharedReader {
            source: self.source.as_ref().ok_or("镜像读取来源已失效")?.clone(),
            pos: 0,
            size: self.info.size,
        }))
    }
    pub fn reader(&self, id: u32) -> Result<Region<Box<dyn Source>>, String> {
        let part = self.part(id)?;
        Region::for_file_view(self.logical_reader()?, part.start, part.size).map_err(|e| e.to_string())
    }
    pub fn readers(&self, ids: &[u32]) -> Result<Vec<Region<Box<dyn Source>>>, String> {
        let parts = ids
            .iter()
            .map(|id| self.part(*id))
            .collect::<Result<Vec<_>, _>>()?;
        let source = if let Some(factory) = &self.factory {
            Arc::new(Mutex::new(factory.reader()?))
        } else {
            self.source.as_ref().ok_or("镜像读取来源已失效")?.clone()
        };
        parts
            .into_iter()
            .map(|part| {
                let reader = Box::new(SharedReader {
                    source: source.clone(),
                    pos: 0,
                    size: self.info.size,
                }) as Box<dyn Source>;
                Region::new(reader, part.start, part.size).map_err(|e| e.to_string())
            })
            .collect()
    }
    pub fn label(&self, id: u32) -> Result<String, String> {
        Ok(format!("{} / {}", self.info.name, self.part(id)?.name))
    }
    pub fn scope_label(&self, id: Option<u32>) -> Result<String, String> {
        match id {
            Some(id) => self.label(id),
            None => Ok(format!("{} / {}", self.info.name, crate::native_locale::current().text("整盘镜像"))),
        }
    }
    pub fn scope_provenance(
        &self,
        id: Option<u32>,
    ) -> Result<crate::recovery_export::ImageSource, String> {
        let (name, start, size) = match id {
            Some(id) => {
                let part = self.part(id)?;
                (part.name.clone(), part.start, part.size)
            }
            None => ("整盘镜像".into(), 0, self.info.size),
        };
        Ok(crate::recovery_export::ImageSource {
            file_name: self.info.name.clone(),
            format: self.info.format.clone(),
            partition: name,
            offset: start.to_string(),
            size: size.to_string(),
            sector: self.info.options.sector,
            parallels_snapshot: self.info.parallels_snapshot.as_ref().map(|point| point.id),
        })
    }
}

#[cfg(test)]
#[path = "image_browser_tests.rs"]
mod tests;

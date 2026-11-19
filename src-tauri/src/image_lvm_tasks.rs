//! Read-only LVM2 views over retained image partitions or individual PV images.
use super::*;
use crate::{
    backup_archive::Source as ReadSource,
    recovery_export::{ImageLvmMember, ImageLvmSource},
};
use disk_core::lvm::{Group, Inventory, VolumeInfo, MAX_MEMBERS};
use std::collections::HashSet;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectRequest {
    id: String,
    partitions: Vec<u32>,
    #[serde(default)]
    allow_degraded_raid: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    id: String,
    volume: String,
    mode: ScanMode,
}

#[derive(Clone)]
enum Members {
    Partitions {
        opened: Opened,
        ids: Vec<u32>,
    },
    #[cfg(windows)]
    Raw(Arc<crate::image_files::Group>),
}

impl Members {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Partitions { opened, .. } => opened.validate(),
            #[cfg(windows)]
            Self::Raw(group) => group.validate(),
        }
    }

    fn open(
        &self,
        allow_degraded_raid: bool,
        checkpoint: impl FnMut() -> Result<(), String>,
    ) -> Result<Group<Box<dyn ReadSource>>, String> {
        self.validate()?;
        let readers = match self {
            Self::Partitions { opened, ids } => opened
                .browser
                .readers(ids)?
                .into_iter()
                .map(|reader| Box::new(reader) as Box<dyn ReadSource>)
                .collect(),
            #[cfg(windows)]
            Self::Raw(group) => group
                .raw_readers()?
                .into_iter()
                .map(|reader| Box::new(reader) as Box<dyn ReadSource>)
                .collect(),
        };
        let group =
            disk_core::lvm::open_with_raid_policy(readers, allow_degraded_raid, checkpoint)?;
        self.validate()?;
        Ok(group)
    }

    fn image_id(&self) -> Option<&str> {
        match self {
            Self::Partitions { opened, .. } => Some(&opened.browser.info.id),
            #[cfg(windows)]
            Self::Raw(_) => None,
        }
    }

    fn disks(&self) -> Vec<Target> {
        match self {
            Self::Partitions { opened, .. } => opened.source_disks.clone(),
            #[cfg(windows)]
            Self::Raw(group) => group.disks(),
        }
    }

    fn provenance(&self) -> Result<Vec<ImageLvmMember>, String> {
        match self {
            Self::Partitions { opened, ids } => ids
                .iter()
                .map(|id| {
                    let info = &opened.browser.info;
                    let part = opened.browser.part(*id)?;
                    Ok(ImageLvmMember {
                        files: info.files.clone(),
                        format: info.format.clone(),
                        offset: part.start.to_string(),
                        size: part.size.to_string(),
                    })
                })
                .collect(),
            #[cfg(windows)]
            Self::Raw(group) => Ok(group
                .info()
                .files
                .into_iter()
                .map(|file| ImageLvmMember {
                    files: vec![file.name],
                    format: "RAW".into(),
                    offset: "0".into(),
                    size: file.size.to_string(),
                })
                .collect()),
        }
    }
}

pub(crate) struct Saved {
    id: String,
    members: Members,
    inventory: Inventory,
    fingerprint: [u8; 32],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    id: String,
    #[serde(flatten)]
    inventory: Inventory,
    scan_id: Option<String>,
    scan_mode: Option<ScanMode>,
}

fn status_in(s: &State) -> Option<Status> {
    let saved = s.image_lvm.as_ref()?;
    let scan = s.scan.as_ref().and_then(|(id, source)| match source {
        ScanSource::ImageLvm(source) if Arc::ptr_eq(saved, &source.saved) => Some(id),
        _ => None,
    });
    Some(Status {
        id: saved.id.clone(),
        inventory: saved.inventory.clone(),
        scan_id: scan.cloned(),
        scan_mode: scan.map(|_| s.scan_mode),
    })
}

pub fn status() -> Option<Status> {
    status_in(&state())
}

fn clear(s: &mut State) {
    if matches!(s.scan, Some((_, ScanSource::ImageLvm(_)))) {
        s.scan = None;
        s.candidates.clear();
        s.directories = Catalog::default();
        s.scan_name.clear();
        s.recovery_progress = None;
        s.pending = None;
    }
    s.image_lvm = None;
}

pub(super) fn clear_for_image(s: &mut State) {
    if s.image_lvm
        .as_ref()
        .is_some_and(|saved| saved.members.image_id().is_some())
    {
        clear(s);
    }
}

pub fn close(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let mut s = state();
    idle(&s)?;
    if s.image_lvm.as_ref().is_none_or(|saved| saved.id != id) {
        return Err("镜像 LVM2 卷组检查已失效，请重新读取。".into());
    }
    clear(&mut s);
    Ok(())
}

fn save(members: Members, allow_degraded_raid: bool) -> Result<Status, String> {
    let group = members.open(allow_degraded_raid, || Ok(()))?;
    let saved = Saved {
        id: uuid::Uuid::new_v4().to_string(),
        members,
        inventory: group.inventory().clone(),
        fingerprint: group.fingerprint(),
    };
    let mut s = state();
    clear(&mut s);
    s.image_lvm = Some(Arc::new(saved));
    Ok(status_in(&s).unwrap())
}

fn check_partitions(ids: &[u32]) -> Result<(), String> {
    if !(1..=MAX_MEMBERS).contains(&ids.len()) {
        return Err("请选择 1–64 个 LVM2 物理卷来源。".into());
    }
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err("LVM2 物理卷来源重复。".into());
    }
    Ok(())
}

pub fn inspect(request: InspectRequest) -> Result<Status, String> {
    check_partitions(&request.partitions)?;
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let opened = {
        let s = state();
        idle(&s)?;
        s.opened_image
            .as_ref()
            .filter(|opened| opened.browser.info.id == request.id)
            .ok_or("镜像浏览会话已失效。")?
            .clone()
    };
    save(
        Members::Partitions {
            opened,
            ids: request.partitions,
        },
        request.allow_degraded_raid,
    )
}

pub fn inspect_raw(allow_degraded_raid: bool) -> Result<Option<Status>, String> {
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 原生文件选择。".into());
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        crate::image_files::choose_lvm_members()?
            .map(|group| save(Members::Raw(group), allow_degraded_raid))
            .transpose()
    }
}

#[derive(Clone)]
pub(crate) struct Source {
    saved: Arc<Saved>,
    volume: VolumeInfo,
}

impl Source {
    pub(in crate::data_tasks) fn degraded_raid(&self) -> bool {
        self.volume.degraded_raid
    }

    pub(in crate::data_tasks) fn disks(&self) -> Vec<Target> {
        self.saved.members.disks()
    }

    pub(in crate::data_tasks) fn validate(&self) -> Result<(), String> {
        self.saved.members.validate()
    }

    pub(in crate::data_tasks) fn label(&self) -> String {
        format!(
            "镜像 LVM2 · {}/{}",
            self.saved.inventory.name, self.volume.name
        )
    }

    pub(in crate::data_tasks) fn provenance(&self) -> Result<ImageLvmSource, String> {
        Ok(ImageLvmSource {
            volume_group: self.saved.inventory.name.clone(),
            sequence: self.saved.inventory.sequence,
            logical_volume: self.volume.name.clone(),
            logical_volume_id: self.volume.id.clone(),
            members: self.saved.members.provenance()?,
        })
    }

    pub(in crate::data_tasks) fn reader(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Box<dyn ReadSource>, String> {
        let group = self
            .saved
            .members
            .open(self.saved.inventory.allow_degraded_raid, || {
                if cancel.load(Ordering::Relaxed) {
                    Err("LVM2 读取已取消。".into())
                } else {
                    Ok(())
                }
            })?;
        if self.saved.fingerprint != group.fingerprint() {
            return Err("LVM2 物理卷或卷组元数据已变化，请重新读取。".into());
        }
        Ok(Box::new(group.reader(&self.volume.id)?))
    }
}

pub fn scan(request: BrowseRequest) -> Result<String, String> {
    if request.mode == ScanMode::Signatures {
        return Err("镜像目录读取请选择现存文件、全部文件或删除候选。".into());
    }
    let guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    let source = {
        let s = state();
        idle(&s)?;
        let saved = s
            .image_lvm
            .as_ref()
            .filter(|saved| saved.id == request.id)
            .ok_or("镜像 LVM2 卷组检查已失效，请重新读取。")?;
        let volume = saved
            .inventory
            .volumes
            .iter()
            .find(|volume| volume.id == request.volume && volume.readable)
            .ok_or("LVM2 逻辑卷不可读取。")?;
        Source {
            saved: saved.clone(),
            volume: volume.clone(),
        }
    };
    source.validate()?;
    let label = source.label();
    scan_source_mode(
        ScanSource::ImageLvm(source),
        None,
        label,
        "读取 LVM2 逻辑卷",
        request.mode,
        guard,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_bounded_opaque_partition_selections() {
        assert!(check_partitions(&[]).is_err());
        assert!(check_partitions(&[3, 3]).is_err());
        assert!(check_partitions(&[3, 4]).is_ok());
        assert!(check_partitions(&(0..65).collect::<Vec<_>>()).is_err());
        let json = serde_json::json!({"id":"image","partitions":[1],"path":"injected.img"});
        assert!(serde_json::from_value::<InspectRequest>(json).is_err());
    }
}

//! Retained sources for explicit, read-only Windows dynamic volume recovery.
use crate::partitions::Target;
#[cfg(windows)]
use disk_core::ldm::Availability;
use disk_core::ldm::Inventory;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(windows)]
#[path = "ldm_devices_windows.rs"]
mod devices;

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum InspectRequest {
    Images { sector_size: u64 },
    Disks { members: Vec<Target> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    pub(crate) session_id: String,
    pub(crate) volume: String,
    pub(crate) deleted: bool,
    pub(crate) allow_degraded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    session_id: String,
    #[serde(flatten)]
    inventory: Inventory,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCandidate {
    target: Target,
    number: u32,
    size: u64,
    model: String,
    known_offline: bool,
}

pub fn device_candidates() -> Result<Vec<DeviceCandidate>, String> {
    #[cfg(windows)]
    return Ok(devices::list()?
        .into_iter()
        .map(|member| DeviceCandidate {
            target: member.target,
            number: member.number,
            size: member.size,
            model: member.model,
            known_offline: member.known_offline,
        })
        .collect());
    #[cfg(not(windows))]
    Err("动态磁盘恢复当前仅支持 Windows。".into())
}

#[cfg(windows)]
#[derive(Clone)]
enum Members {
    Images {
        files: Arc<crate::image_files::Group>,
        sector_size: u64,
    },
    Disks(Vec<devices::Member>),
}

#[cfg(windows)]
struct Saved {
    session_id: String,
    members: Members,
    fingerprint: [u8; 32],
    inventory: Inventory,
}

#[derive(Clone)]
pub(crate) struct Source {
    #[cfg(windows)]
    saved: Arc<Saved>,
    #[cfg(windows)]
    volume: String,
    #[cfg(windows)]
    label: String,
    #[cfg(windows)]
    pub(crate) degraded: bool,
}

impl Source {
    pub(crate) fn disks(&self) -> Vec<Target> {
        #[cfg(windows)]
        return match &self.saved.members {
            Members::Images { files, .. } => files.disks(),
            Members::Disks(members) => members.iter().map(|source| source.target.clone()).collect(),
        };
        #[cfg(not(windows))]
        Vec::new()
    }

    #[cfg(windows)]
    pub(crate) fn disk_numbers(&self) -> Vec<u32> {
        match &self.saved.members {
            Members::Images { .. } => Vec::new(),
            Members::Disks(members) => members.iter().map(|member| member.number).collect(),
        }
    }
}

pub fn inspect(request: InspectRequest) -> Result<Option<View>, String> {
    #[cfg(windows)]
    return platform::inspect(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("动态磁盘恢复当前仅支持 Windows。".into())
    }
}

#[cfg(windows)]
pub(crate) use platform::selected;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dynamic_disk_requests_are_typed_and_do_not_accept_paths() {
        assert!(serde_json::from_value::<InspectRequest>(
            json!({"kind": "images", "sectorSize": 512})
        )
        .is_ok());
        assert!(serde_json::from_value::<InspectRequest>(
            json!({"kind": "images", "sectorSize": 512, "paths": []})
        )
        .is_err());
        assert!(serde_json::from_value::<BrowseRequest>(
            json!({"sessionId": "s", "volume": "v", "deleted": false, "allowDegraded": true})
        )
        .is_ok());
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::backup_archive::Source as ReadSource;
    use disk_core::ldm::{self, Group, Member};
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicBool, Ordering},
    };

    fn state() -> std::sync::MutexGuard<'static, Option<Arc<Saved>>> {
        static SAVED: OnceLock<Mutex<Option<Arc<Saved>>>> = OnceLock::new();
        SAVED
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    impl Members {
        fn validate(&self) -> Result<(), String> {
            match self {
                Self::Images { files, .. } => files.validate(),
                Self::Disks(members) => {
                    let current = devices::list()?;
                    if members.iter().any(|member| !current.contains(member)) {
                        return Err("动态磁盘身份或提供者信息已变化，请重新选择。".into());
                    }
                    Ok(())
                }
            }
        }

        fn open(
            &self,
            checkpoint: impl FnMut() -> Result<(), String>,
        ) -> Result<Group<Box<dyn ReadSource>>, String> {
            self.validate()?;
            let readers = match self {
                Self::Images { files, sector_size } => files
                    .raw_readers()?
                    .into_iter()
                    .map(|reader| Member {
                        reader: Box::new(reader) as Box<dyn ReadSource>,
                        sector_size: *sector_size,
                    })
                    .collect(),
                Self::Disks(members) => members
                    .iter()
                    .map(|member| {
                        Ok(Member {
                            reader: Box::new(member.reader()?) as Box<dyn ReadSource>,
                            sector_size: member.sector,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            };
            let group = ldm::open(readers, checkpoint)?;
            self.validate()?;
            Ok(group)
        }
    }

    pub(super) fn inspect(request: InspectRequest) -> Result<Option<View>, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行")?;
        let members = match request {
            InspectRequest::Images { sector_size } => {
                if !matches!(sector_size, 512 | 4096) {
                    return Err("请选择动态磁盘镜像的逻辑扇区大小。".into());
                }
                let Some(files) =
                    crate::image_files::choose_raw_members("选择动态磁盘成员的 RAW/IMG 文件")?
                else {
                    return Ok(None);
                };
                Members::Images { files, sector_size }
            }
            InspectRequest::Disks { members } => {
                let mut ids = HashSet::new();
                if !(1..=ldm::MAX_MEMBERS).contains(&members.len())
                    || members.iter().any(|target| !ids.insert(&target.0))
                {
                    return Err("请选择 1–64 块不同的完整动态磁盘。".into());
                }
                let candidates = devices::list()?;
                let selected = members
                    .iter()
                    .map(|target| {
                        let mut matched = candidates
                            .iter()
                            .filter(|candidate| &candidate.target == target);
                        let member = matched.next().ok_or("动态磁盘身份已变化，请重新选择。")?;
                        if matched.next().is_some() {
                            return Err("动态磁盘身份重复，请重新选择。");
                        }
                        Ok(member.clone())
                    })
                    .collect::<Result<_, _>>()?;
                Members::Disks(selected)
            }
        };
        let group = members.open(|| Ok(()))?;
        let inventory = group.inventory().clone();
        let session_id = uuid::Uuid::new_v4().to_string();
        *state() = Some(Arc::new(Saved {
            session_id: session_id.clone(),
            members,
            inventory: inventory.clone(),
            fingerprint: group.fingerprint(),
        }));
        Ok(Some(View {
            session_id,
            inventory,
        }))
    }

    pub(crate) fn selected(request: BrowseRequest) -> Result<Source, String> {
        let saved = state()
            .as_ref()
            .filter(|saved| saved.session_id == request.session_id)
            .cloned()
            .ok_or("动态磁盘检查已失效，请重新读取。")?;
        let volume = saved
            .inventory
            .volumes
            .iter()
            .find(|volume| volume.id == request.volume)
            .ok_or("动态卷不存在。")?;
        let degraded = volume.availability == Availability::Degraded;
        if volume.availability != Availability::Complete && !(degraded && request.allow_degraded) {
            return Err("动态卷无法读取，或尚未确认降级读取。".into());
        }
        Ok(Source {
            label: format!(
                "LDM · {} / {} · {}",
                saved.inventory.name, volume.name, volume.id
            ),
            volume: volume.id.clone(),
            degraded,
            saved,
        })
    }

    impl Source {
        pub(crate) fn label(&self) -> &str {
            &self.label
        }

        pub(crate) fn reader(
            &self,
            cancel: &AtomicBool,
        ) -> Result<Box<dyn crate::backup_archive::Source>, String> {
            let group = self.saved.members.open(|| {
                if cancel.load(Ordering::Relaxed) {
                    return Err("动态卷读取已取消。".into());
                }
                Ok(())
            })?;
            if group.fingerprint() != self.saved.fingerprint {
                return Err("动态磁盘成员或数据库已变化，请重新检查。".into());
            }
            Ok(Box::new(group.into_volume(&self.volume)?))
        }
    }
}

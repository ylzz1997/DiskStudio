//! Binds explicitly selected physical volumes to one checked LVM2 view.
use crate::data_tasks::Selection;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectRequest {
    pub members: Vec<Selection>,
    #[serde(default)]
    pub allow_degraded_raid: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowseRequest {
    pub id: String,
    pub volume: String,
    pub deleted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: String,
    #[serde(flatten)]
    inventory: disk_core::lvm::Inventory,
}

struct Saved {
    id: String,
    members: Vec<Selection>,
    inventory: disk_core::lvm::Inventory,
    fingerprint: [u8; 32],
}

static SAVED: OnceLock<Mutex<Option<Saved>>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, Option<Saved>> {
    SAVED
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn validate(members: &[Selection]) -> Result<(), String> {
    if !(1..=disk_core::lvm::MAX_MEMBERS).contains(&members.len()) {
        return Err("请选择 1–64 个 LVM2 物理卷来源。".into());
    }
    let mut disks = HashSet::new();
    for source in members {
        if !disks.insert((&source.target.0, source.partition))
            || (source.partition.is_some()
                && members
                    .iter()
                    .any(|other| other.target == source.target && other.partition.is_none()))
        {
            return Err("LVM2 来源重复，或整盘与分区选择重叠。".into());
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Source {
    pub members: Vec<Selection>,
    pub label: String,
    pub degraded_raid: bool,
    allow_degraded_raid: bool,
    volume: String,
    volume_group: String,
    sequence: u64,
    volume_name: String,
    fingerprint: [u8; 32],
}

#[cfg(windows)]
use crate::{data_io::Region, data_windows::StableRead};
#[cfg(windows)]
use std::{
    fs::File,
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(windows)]
fn open(
    locks: &[StableRead],
    allow_degraded_raid: bool,
    checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<disk_core::lvm::Group<Region<File>>, String> {
    let readers = locks
        .iter()
        .map(StableRead::reader)
        .collect::<Result<Vec<_>, _>>()?;
    disk_core::lvm::open_with_raid_policy(readers, allow_degraded_raid, checkpoint)
}

pub fn inspect(request: InspectRequest) -> Result<View, String> {
    validate(&request.members)?;
    #[cfg(not(windows))]
    return Err("当前仅支持 Windows 设备读取".into());
    #[cfg(windows)]
    {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "另一项磁盘操作正在运行")?;
        *state() = None;
        let locks = request
            .members
            .iter()
            .map(StableRead::lock)
            .collect::<Result<Vec<_>, _>>()?;
        let group = open(&locks, request.allow_degraded_raid, || Ok(()))?;
        for lock in &locks {
            lock.verify()?;
        }
        let inventory = group.inventory().clone();
        let id = uuid::Uuid::new_v4().to_string();
        *state() = Some(Saved {
            id: id.clone(),
            members: request.members,
            fingerprint: group.fingerprint(),
            inventory: inventory.clone(),
        });
        Ok(View { id, inventory })
    }
}

pub(crate) fn selected(request: BrowseRequest) -> Result<(Source, bool), String> {
    let saved = state();
    let group = saved
        .as_ref()
        .filter(|group| group.id == request.id)
        .ok_or("LVM2 卷组检查已失效，请重新读取。")?;
    let volume = group
        .inventory
        .volumes
        .iter()
        .find(|volume| volume.id == request.volume && volume.readable)
        .ok_or("LVM2 逻辑卷不可读取。")?;
    Ok((
        Source {
            members: group.members.clone(),
            volume: volume.id.clone(),
            volume_group: group.inventory.name.clone(),
            sequence: group.inventory.sequence,
            volume_name: volume.name.clone(),
            label: format!("LVM2 · {}/{}", group.inventory.name, volume.name),
            degraded_raid: volume.degraded_raid,
            allow_degraded_raid: group.inventory.allow_degraded_raid,
            fingerprint: group.fingerprint,
        },
        request.deleted,
    ))
}

#[cfg(windows)]
impl Source {
    pub(crate) fn provenance(
        &self,
        locks: Option<&[StableRead]>,
    ) -> Result<crate::recovery_export::PhysicalLvmSource, String> {
        use crate::recovery_export::{PhysicalLvmMember, PhysicalLvmSource};
        if locks.is_some_and(|locks| locks.len() != self.members.len()) {
            return Err("LVM2 物理卷读取凭据不完整。".into());
        }
        let members = self
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let resolved = match locks {
                    Some(locks) => locks[index].resolved()?,
                    None => crate::data_windows::resolve(member, false, false)?,
                };
                Ok(PhysicalLvmMember {
                    disk_number: resolved.disk.number,
                    partition: member.partition,
                    offset: resolved.offset.to_string(),
                    size: resolved.size.to_string(),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(PhysicalLvmSource {
            volume_group: self.volume_group.clone(),
            sequence: self.sequence,
            logical_volume: self.volume_name.clone(),
            logical_volume_id: self.volume.clone(),
            members,
        })
    }

    pub(crate) fn reader(
        &self,
        locks: &[StableRead],
        cancel: &AtomicBool,
    ) -> Result<disk_core::lvm::LogicalReader<Region<File>>, String> {
        if locks.len() != self.members.len() {
            return Err("LVM2 物理卷读取凭据不完整。".into());
        }
        let group = open(locks, self.allow_degraded_raid, || {
            if cancel.load(Ordering::Relaxed) {
                Err("LVM2 读取已取消。".into())
            } else {
                Ok(())
            }
        })?;
        if group.fingerprint() != self.fingerprint {
            return Err("LVM2 物理卷或卷组元数据已变化，请重新读取。".into());
        }
        group.reader(&self.volume)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partitions::Target;

    #[test]
    fn ipc_forbids_raw_paths_and_overlapping_sources() {
        let source = |partition| Selection {
            target: Target("disk".into()),
            layout_key: "layout".into(),
            partition,
        };
        assert!(validate(&[source(None), source(Some(2))]).is_err());
        assert!(validate(&[source(Some(1)), source(Some(1))]).is_err());
        assert!(validate(&[source(Some(1)), source(Some(2))]).is_ok());
        let input =
            serde_json::json!({"id":"token","volume":"lv","deleted":false,"path":"injected"});
        assert!(serde_json::from_value::<BrowseRequest>(input).is_err());
    }
}

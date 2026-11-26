use super::*;
use crate::used_space::Span;
use std::collections::BTreeSet;
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

const RECOVERY: &str = "de94bba4-06d1-4d40-a16a-bfd50179d6ac";
const UNSUPPORTED: &str = "热迁移需要正在运行的 Windows 和 EFI 启动分区位于同一普通 GPT 磁盘。";

pub(crate) enum HotPartitionKind {
    Snapshot(String),
    CheckedPhysical,
    Unavailable,
}

pub(crate) struct HotPartition {
    pub(crate) number: u32,
    pub(crate) span: Span,
    pub(crate) kind: HotPartitionKind,
}

pub(crate) struct HotMigrationSource {
    pub(crate) resolved: Resolved,
    pub(crate) windows: Span,
    pub(crate) efi: uuid::Uuid,
    pub(crate) partitions: Vec<HotPartition>,
}

fn windows_volume() -> Result<String, String> {
    let mut directory = [0u16; 260];
    let count = unsafe { GetWindowsDirectoryW(directory.as_mut_ptr(), directory.len() as u32) };
    if count < 4
        || count as usize >= directory.len()
        || !matches!(directory[0], 65..=90 | 97..=122)
        || directory[1..3] != [58, 92]
    {
        return Err("无法核对当前 Windows 所在卷。".into());
    }
    let root = [directory[0], 58, 92, 0];
    let mut volume = [0u16; 64];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    } == 0
    {
        return Err("无法核对当前 Windows 所在卷。".into());
    }
    let end = volume
        .iter()
        .position(|&c| c == 0)
        .ok_or("卷标识格式无效")?;
    volume_root(&String::from_utf16(&volume[..end]).map_err(err)?)
}

impl HotMigrationSource {
    pub(crate) fn is_windows_partition(selection: &Selection, observed: &Resolved) -> Result<bool, String> {
        let Some(number) = selection.partition else { return Ok(false) };
        if observed.disk.is_boot != Some(true) {
            return Ok(false);
        }
        let inventory = Inventory::read()?;
        let disk = policy::target_disk(&inventory.disks, &selection.target)?;
        policy::corroborate(disk, &inventory.legacy)?;
        let parts = policy::layout(disk, &inventory.parts)?;
        verify_handoff_metadata(selection, observed, disk, &parts)?;
        parts.iter().find(|part| part.partition_number == number)
            .and_then(|part| part.is_boot)
            .ok_or_else(|| "分区启动状态未知。".to_owned())
    }

    pub(crate) fn bind(selection: &Selection) -> Result<Self, String> {
        if selection.partition.is_some() {
            return Err(UNSUPPORTED.into());
        }
        let mut resolved = resolve(selection, false, false)?;
        let inv = Inventory::read()?;
        let disk = policy::target_disk(&inv.disks, &selection.target)?;
        policy::corroborate(disk, &inv.legacy)?;
        crate::storage_spaces::PoolProtection::read(&inv.con)?.check_disk(disk)?;
        let parts = policy::layout(disk, &inv.parts)?;
        verify_handoff_metadata(selection, &resolved, disk, &parts)?;
        if *disk != resolved.disk
            || disk.partition_style != Some(2)
            || !matches!(disk.bus_type, Some(1 | 3 | 7 | 11 | 17))
            || disk.is_boot != Some(true)
            || disk.is_system != Some(true)
            || disk.is_offline != Some(false)
            || disk.is_read_only != Some(false)
            || disk.is_clustered != Some(false)
            || !(4 * disk_core::MIB..=1 << 43).contains(&disk.size)
        {
            return Err(UNSUPPORTED.into());
        }
        let sector = disk.logical_sector_size.ok_or(UNSUPPORTED)?;
        let live = windows_volume()?;
        let mut windows = None;
        let mut efi = None;
        let mut associations = Vec::new();
        let mut partitions = Vec::with_capacity(parts.len());
        let mut volumes = BTreeSet::new();
        for part in &parts {
            let partition_kind = part
                .gpt_type
                .as_deref()
                .map(policy::guid)
                .ok_or(UNSUPPORTED)?;
            let guid = part
                .guid
                .as_deref()
                .and_then(|value| uuid::Uuid::parse_str(&policy::guid(value)).ok())
                .filter(|id| !id.is_nil())
                .ok_or(UNSUPPORTED)?;
            let start = part.offset;
            let end = start
                .checked_add(part.size)
                .filter(|&end| end <= disk.size)
                .ok_or(UNSUPPORTED)?;
            let span = Span { start, end };
            if start == 0
                || start % sector != 0
                || end <= start
                || end % sector != 0
                || part.is_boot.is_none()
                || part.is_system.is_none()
                || part.is_offline == Some(true)
                || part.is_shadow_copy == Some(true)
            {
                return Err(UNSUPPORTED.into());
            }
            let association = retained_association(partition_volumes(&inv, part)?, None)?;
            let mut root = None;
            if let Some(volume) = &association {
                let id = volume_root(volume.unique_id.as_deref().ok_or("卷标识缺失")?)?;
                if !volumes.insert(id.clone()) {
                    return Err("多个分区返回了同一个卷标识。".into());
                }
                root = Some(id);
            }
            let is_ntfs = association
                .as_ref()
                .and_then(|volume| volume.file_system.as_deref())
                .is_some_and(|fs| fs.eq_ignore_ascii_case("NTFS"));
            let ordinary = partition_kind == policy::BASIC_DATA
                && part.is_offline == Some(false)
                && part.is_hidden == Some(false)
                && part.is_read_only == Some(false)
                && part.is_shadow_copy == Some(false);
            let kind = if part.is_system == Some(true) {
                if part.is_boot != Some(false)
                    || uuid::Uuid::parse_str(&partition_kind).ok() != Some(EFI_SYSTEM_TYPE)
                    || efi.replace(guid).is_some()
                {
                    return Err(UNSUPPORTED.into());
                }
                if association
                    .as_ref()
                    .and_then(|volume| volume.file_system.as_deref())
                    .is_some_and(|fs| !fs.eq_ignore_ascii_case("FAT32"))
                {
                    return Err(UNSUPPORTED.into());
                }
                HotPartitionKind::CheckedPhysical
            } else if part.is_boot == Some(true) {
                if !ordinary
                    || !is_ntfs
                    || root.as_ref() != Some(&live)
                    || windows.replace(span).is_some()
                {
                    return Err(UNSUPPORTED.into());
                }
                HotPartitionKind::Snapshot(live.clone())
            } else if ordinary && is_ntfs {
                HotPartitionKind::Snapshot(root.clone().ok_or("卷标识缺失")?)
            } else if (partition_kind == crate::ldm_basic::MICROSOFT_RESERVED
                && association.is_none())
                || (partition_kind == RECOVERY
                    && part.drive_letter.as_deref().is_none_or(str::is_empty)
                    && part.is_hidden == Some(true))
            {
                HotPartitionKind::CheckedPhysical
            } else {
                HotPartitionKind::Unavailable
            };
            if let Some(volume) = association {
                associations.push((part.partition_number, volume));
            }
            partitions.push(HotPartition {
                number: part.partition_number,
                span,
                kind,
            });
        }
        let windows = windows.ok_or(UNSUPPORTED)?;
        let efi = efi.ok_or(UNSUPPORTED)?;
        let fingerprint = lock_fingerprint(disk, &parts, &associations, selection, false)?;
        resolved.data_lock_proof = Some(DataLockProof {
            source: selection.clone(),
            writing: false,
            fingerprint,
            associations,
            ranges: Vec::new(),
        });
        partitions.sort_by_key(|part| part.span.start);
        Ok(Self {
            resolved,
            windows,
            efi,
            partitions,
        })
    }

    pub(crate) fn verify_snapshot_volume(
        &self,
        selection: &Selection,
        root: &str,
        span: Span,
    ) -> Result<(), String> {
        volume_location(root, &Inventory::read()?)?.confirm_partition(
            &selection.target,
            &selection.layout_key,
            root,
            (self.resolved.disk.number, span.start, span.end - span.start),
        )
    }

    pub(crate) fn revalidate(&self, selection: &Selection, file: &File) -> Result<(), String> {
        let fresh = Self::bind(selection)?;
        if !self.resolved.same_data_binding(&fresh.resolved) {
            return Err("热迁移来源的磁盘、布局或卷关联已变化。".into());
        }
        verify_device_identity(
            self.resolved.disk.number,
            self.resolved.size,
            self.resolved.disk.logical_sector_size,
            self.resolved.disk.serial_number.as_deref(),
            |code, input, output| control(file, code, input, output),
        )?;
        verify_read_state(&self.resolved, file)
    }
}

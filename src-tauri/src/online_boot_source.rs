//! Read-only binding for an EFI partition on an online Windows disk.
use super::{
    control, lock_fingerprint, open_bound_physical, partition_volumes, policy,
    retained_association, verify_boot_gpt, verify_device_identity, volume_location, volume_root,
    Disk, Inventory, Partition, Selection, Volume, IOCTL_DISK_GET_DISK_ATTRIBUTES,
};
use disk_core::table_check::{GptIdentity, GptPartitionIdentity};
use std::fs::File;
use uuid::Uuid;

pub(crate) const EFI_SYSTEM_TYPE: Uuid = Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b);

pub(crate) struct OnlineBootSource {
    disk: Disk,
    parts: Vec<Partition>,
    partition: Uuid,
    kind: Uuid,
    offset: u64,
    bytes: u64,
    sector: u64,
    fingerprint: String,
}

fn check_disk(disk: &Disk) -> Result<u64, String> {
    match disk.logical_sector_size {
        Some(sector @ (512 | 4096))
            if disk.partition_style == Some(2)
                && disk.is_offline == Some(false)
                && disk.is_read_only.is_some()
                && disk.is_clustered == Some(false)
                && disk.is_boot.is_some()
                && disk.is_system.is_some()
                && disk.bus_type != Some(16)
                && disk.size >= 4 * 1024 * 1024
                && disk.size % sector == 0 =>
        {
            Ok(sector)
        }
        _ => Err("在线 EFI 来源需要状态明确的普通 GPT 磁盘。".into()),
    }
}

fn check_partition(part: &Partition, sector: u64, winpe: bool) -> Result<(Uuid, Uuid), String> {
    let kind = part
        .gpt_type
        .as_deref()
        .map(policy::guid)
        .and_then(|kind| Uuid::parse_str(&kind).ok())
        .ok_or("EFI 分区类型缺失。")?;
    let winpe_data = winpe
        && part.gpt_type.as_deref().map(policy::guid).as_deref() == Some(policy::BASIC_DATA)
        && part.is_system == Some(false)
        && part.is_boot == Some(false)
        && part.is_offline == Some(false);
    if (kind != EFI_SYSTEM_TYPE && !winpe_data)
        || part.is_offline == Some(true)
        || part.is_shadow_copy == Some(true)
        || part.offset == 0
        || part.offset % sector != 0
        || part.size == 0
        || part.size % sector != 0
    {
        return Err("所选在线分区与普通 EFI 系统分区不一致。".into());
    }
    let id = part
        .guid
        .as_deref()
        .map(policy::guid)
        .and_then(|id| Uuid::parse_str(&id).ok())
        .filter(|id| !id.is_nil())
        .ok_or("EFI 分区身份缺失。")?;
    Ok((id, kind))
}

fn check_volume(volume: &Volume) -> Result<(), String> {
    if volume
        .file_system
        .as_deref()
        .is_some_and(|fs| !fs.eq_ignore_ascii_case("FAT32"))
    {
        return Err("Windows 报告的 EFI 卷文件系统与 FAT32 不一致。".into());
    }
    Ok(())
}

impl OnlineBootSource {
    pub(crate) fn resolve(source: &Selection) -> Result<Self, String> {
        Self::resolve_selected(source, false)
    }

    pub(crate) fn winpe(source: &Selection) -> Result<Self, String> {
        Self::resolve_selected(source, true)
    }

    fn resolve_selected(source: &Selection, winpe: bool) -> Result<Self, String> {
        let number = source.partition.ok_or("请选择一个 EFI 系统分区。")?;
        let inv = Inventory::read()?;
        let disk = policy::target_disk(&inv.disks, &source.target)?;
        policy::corroborate(disk, &inv.legacy)?;
        crate::storage_spaces::PoolProtection::read(&inv.con)?.check_disk(disk)?;
        let sector = check_disk(disk)?;
        let parts = policy::layout(disk, &inv.parts)?;
        let layout_key = crate::disk_identity::layout_key(
            disk.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        );
        if layout_key != source.layout_key {
            return Err("磁盘布局已变化，请刷新并重新选择。".into());
        }
        let part = parts
            .iter()
            .find(|p| p.partition_number == number)
            .ok_or("所选分区已不存在。")?;
        let (partition, kind) = check_partition(part, sector, winpe)?;
        let partition_id = partition.to_string();
        if inv
            .parts
            .iter()
            .filter(|p| {
                p.guid
                    .as_deref()
                    .is_some_and(|guid| policy::guid(guid) == partition_id)
            })
            .count()
            != 1
        {
            return Err("EFI 分区 GUID 缺失或在多个 Windows 磁盘上重复。".into());
        }
        let association = retained_association(partition_volumes(&inv, part)?, None)?;
        if kind != EFI_SYSTEM_TYPE
            && !association.as_ref().is_some_and(|volume| {
                volume.unique_id.is_some()
                    && volume
                        .file_system
                        .as_deref()
                        .is_some_and(|fs| fs.eq_ignore_ascii_case("FAT32"))
            })
        {
            return Err("WinPE 数据分区需要可核对的联机 FAT32 卷。".into());
        }
        if let Some(volume) = &association {
            check_volume(volume)?;
            if let Some(volume) = volume.unique_id.as_deref() {
                let root = volume_root(volume)?;
                volume_location(&root, &inv)?.confirm_partition(
                    &source.target,
                    &source.layout_key,
                    &root,
                    (disk.number, part.offset, part.size),
                )?;
            }
        }
        let associations: Vec<_> = association
            .into_iter()
            .map(|volume| (number, volume))
            .collect();
        let fingerprint = lock_fingerprint(disk, &parts, &associations, source, false)?;
        Ok(Self {
            disk: disk.clone(),
            offset: part.offset,
            bytes: part.size,
            parts: parts.into_iter().cloned().collect(),
            partition,
            kind,
            sector,
            fingerprint,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.disk.size
    }

    pub(crate) fn sector(&self) -> u64 {
        self.sector
    }

    pub(crate) fn open(&self) -> Result<File, String> {
        let serial = self
            .disk
            .serial_number
            .as_deref()
            .ok_or("缺少 EFI 来源磁盘序列号。")?;
        let file =
            open_bound_physical(self.disk.number, self.disk.size, self.sector, serial, false)?;
        self.verify_attributes(&file)?;
        Ok(file)
    }

    pub(crate) fn check_layout<'a>(
        &self,
        file: &File,
        layout: &'a GptIdentity,
    ) -> Result<&'a GptPartitionIdentity, String> {
        let entry = layout
            .partitions
            .iter()
            .find(|entry| {
                entry.guid == self.partition.to_bytes_le()
                    && entry.kind == self.kind.to_bytes_le()
                    && entry.range.start == self.offset
                    && entry.range.end - entry.range.start == self.bytes
            })
            .ok_or("在线 EFI 分区的 GPT 标识或范围已变化。")?;
        verify_boot_gpt(
            file,
            layout,
            &self.parts.iter().collect::<Vec<_>>(),
            self.sector,
        )?;
        Ok(entry)
    }

    pub(crate) fn revalidate(&self, source: &Selection, file: &File) -> Result<(), String> {
        if Self::resolve_selected(source, self.kind != EFI_SYSTEM_TYPE)?.fingerprint
            != self.fingerprint
        {
            return Err("在线 EFI 来源的磁盘、分区或卷关联已变化。".into());
        }
        verify_device_identity(
            self.disk.number,
            self.disk.size,
            self.disk.logical_sector_size,
            self.disk.serial_number.as_deref(),
            |code, input, output| control(file, code, input, output),
        )?;
        self.verify_attributes(file)
    }

    fn verify_attributes(&self, file: &File) -> Result<(), String> {
        let mut bytes = [0; 16];
        let count = control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut bytes)?;
        check_attributes(&self.disk, &bytes[..count.min(bytes.len())])
    }
}

fn check_attributes(disk: &Disk, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() != 16 || u32::from_le_bytes(bytes[..4].try_into().unwrap()) < 16 {
        return Err("原生设备状态返回不完整。".into());
    }
    let flags = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if flags & 1 != 0 || disk.is_read_only != Some(flags & 2 != 0) {
        return Err("设备句柄的在线或只读状态与选择不一致。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_system_gpt_is_a_read_source_and_rejects_state_mismatch() {
        let mut disk = Disk {
            size: 16 * 1024 * 1024 * 1024,
            partition_style: Some(2),
            logical_sector_size: Some(4096),
            is_boot: Some(true),
            is_system: Some(true),
            is_offline: Some(false),
            is_read_only: Some(false),
            is_clustered: Some(false),
            ..Disk::default()
        };
        assert_eq!(check_disk(&disk), Ok(4096));
        let mut attributes = [0; 16];
        attributes[..4].copy_from_slice(&16u32.to_le_bytes());
        assert!(check_attributes(&disk, &attributes).is_ok());
        attributes[8] = 1;
        assert!(check_attributes(&disk, &attributes).is_err());
        attributes[8] = 2;
        assert!(check_attributes(&disk, &attributes).is_err());
        disk.is_read_only = Some(true);
        assert!(check_attributes(&disk, &attributes).is_ok());
        disk.is_offline = Some(true);
        assert!(check_disk(&disk).is_err());
        disk.is_offline = Some(false);
        disk.is_clustered = Some(true);
        assert!(check_disk(&disk).is_err());
    }

    #[test]
    fn refuses_other_gpt_types_unaligned_ranges_and_shadow_copies() {
        let mut part = Partition {
            is_system: Some(true),
            is_hidden: Some(true),
            gpt_type: Some(EFI_SYSTEM_TYPE.to_string()),
            guid: Some("11111111-2222-4333-8444-555555555555".into()),
            offset: 16 * 1024 * 1024,
            size: 272 * 1024 * 1024,
            ..Partition::default()
        };
        assert!(check_partition(&part, 4096, false).is_ok());
        part.is_shadow_copy = Some(true);
        assert!(check_partition(&part, 4096, false).is_err());
        part.is_shadow_copy = Some(false);
        part.size += 512;
        assert!(check_partition(&part, 4096, false).is_err());
        part.size -= 512;
        part.gpt_type = Some(policy::BASIC_DATA.into());
        assert!(check_partition(&part, 4096, false).is_err());
    }
}

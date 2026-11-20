//! Pure disk models, bounded image-header parsing and FAT metadata generation.
//! Intentionally no filesystem, raw-device, process, mount or elevation APIs.
pub mod btrfs;
pub mod fat_format;
mod image;
pub mod lvm;
pub mod mbr;
pub mod md_raid;
pub mod partition_move;
mod raid10;
mod raid_field;
mod raid_parity;
pub mod table_check;
pub mod virtual_raid;
pub use image::parse_image;
use serde::{Deserialize, Serialize};

pub const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    pub id: String,
    pub name: String,
    pub fs: String,
    pub start: u64,
    pub size: u64,
    pub used: Option<u64>,
    pub color: String,
    pub system: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub id: String,
    pub name: String,
    pub filename: String,
    pub size: u64,
    pub sector_size: u64,
    pub scheme: String,
    pub kind: String,
    pub partitions: Vec<Partition>,
}
pub fn validate(disk: &Disk) -> Result<(), String> {
    if !(4 * MIB..=1 << 43).contains(&disk.size)
        || ![512, 4096].contains(&disk.sector_size)
        || disk.partitions.len() > 256
    {
        return Err("磁盘容量、扇区大小或分区数量超出支持范围。".into());
    }
    let mut end = 0;
    let mut ids = std::collections::HashSet::new();
    for p in &disk.partitions {
        let next = p.start.checked_add(p.size).ok_or("分区范围溢出。")?;
        if p.size == 0
            || p.start < end
            || next > disk.size
            || p.start % disk.sector_size != 0
            || p.size % disk.sector_size != 0
            || !ids.insert(&p.id)
            || p.used.is_some_and(|n| n > p.size)
        {
            return Err("分区布局重叠、越界、标识重复或未对齐。".into());
        }
        end = next;
    }
    Ok(())
}
pub(crate) fn free(start: u64, size: u64, id: String) -> Partition {
    Partition {
        id,
        name: "未分配".into(),
        fs: "—".into(),
        start,
        size,
        used: Some(0),
        color: "free".into(),
        system: false,
    }
}
#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::{apply_operations, sample_disks, Operation};

#[cfg(test)]
mod tests {
    use super::*;
    fn op(t: &str, p: &str, size: Option<u64>) -> Operation {
        Operation {
            id: "test-op".into(),
            r#type: t.into(),
            partition_id: p.into(),
            name: Some("New".into()),
            fs: Some("FAT32".into()),
            size,
        }
    }
    #[test]
    fn samples_are_valid() {
        for d in sample_disks().unwrap() {
            validate(&d).unwrap();
        }
    }
    #[test]
    fn refuses_imported_image_mutation() {
        let mut d = sample_disks().unwrap().remove(0);
        d.kind = "image".into();
        assert!(apply_operations(d, vec![]).is_err());
    }
    #[test]
    fn refuses_physical_disk_operations() {
        let mut d = sample_disks().unwrap().remove(0);
        d.kind = "physical".into();
        for kind in ["create", "resize", "format", "delete", "rename"] {
            assert!(apply_operations(d.clone(), vec![op(kind, "workspace", Some(MIB))]).is_err());
        }
    }
    #[test]
    fn refuses_overlap_and_system_change() {
        let d = sample_disks().unwrap().remove(0);
        assert!(apply_operations(
            d.clone(),
            vec![op("resize", "workspace", Some(50 * 1024 * MIB))]
        )
        .is_err());
        assert!(apply_operations(d.clone(), vec![op("delete", "efi", None)]).is_err());
        assert!(apply_operations(d, vec![op("resize", "workspace", Some(2 * MIB))]).is_err());
    }
    #[test]
    fn shrink_merge_then_grow_conserves_extent() {
        let d = sample_disks().unwrap().remove(1);
        let shrunk = apply_operations(
            d.clone(),
            vec![op("resize", "backup-data", Some(16 * 1024 * MIB))],
        )
        .unwrap();
        assert_eq!(shrunk.partitions.len(), 2);
        let grown = apply_operations(
            shrunk,
            vec![op("resize", "backup-data", Some(24 * 1024 * MIB))],
        )
        .unwrap();
        assert_eq!(grown.partitions[0].size, d.partitions[0].size);
        assert_eq!(grown.partitions[1].size, d.partitions[1].size);
    }
    #[test]
    fn failed_batch_does_not_modify_source() {
        let d = sample_disks().unwrap().remove(0);
        let original = d.clone();
        assert!(apply_operations(
            d.clone(),
            vec![
                op("delete", "archive", None),
                op("resize", "missing", Some(MIB))
            ]
        )
        .is_err());
        assert_eq!(d, original);
    }
}

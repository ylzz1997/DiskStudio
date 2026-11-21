//! Metadata-only candidates and read-only handles for offline LDM members.
use crate::{data_io::Region, data_windows, disk_identity, partitions::Target};
use serde::Deserialize;
use wmi::WMIConnection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Member {
    pub target: Target,
    pub number: u32,
    serial: String,
    pub size: u64,
    pub sector: u64,
    pub model: String,
    pub known_offline: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Legacy {
    index: u32,
    serial_number: Option<String>,
    model: Option<String>,
    bytes_per_sector: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Physical {
    serial_number: Option<String>,
    unique_id: Option<String>,
    size: u64,
    logical_sector_size: Option<u64>,
    bus_type: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StorageDisk {
    number: u32,
    serial_number: Option<String>,
    size: u64,
    logical_sector_size: Option<u64>,
    is_boot: Option<bool>,
    is_system: Option<bool>,
    is_offline: Option<bool>,
    is_clustered: Option<bool>,
}

fn serial(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(disk_identity::clean)
        .filter(|s| !s.is_empty())
}

fn bind(legacy: &[Legacy], physical: &[Physical], storage: &[StorageDisk]) -> Vec<Member> {
    legacy
        .iter()
        .filter_map(|row| {
            let value = serial(&row.serial_number)?;
            if legacy
                .iter()
                .filter(|other| {
                    other.index == row.index || serial(&other.serial_number) == Some(value)
                })
                .count()
                != 1
            {
                return None;
            }
            let mut matching = physical
                .iter()
                .filter(|disk| serial(&disk.serial_number) == Some(value));
            let disk = matching.next()?;
            let sector = disk.logical_sector_size?;
            if matching.next().is_some()
                || !matches!(sector, 512 | 4096)
                || row.bytes_per_sector != Some(sector)
                || !(4 * 1024 * 1024..=1 << 44).contains(&disk.size)
                || disk.size % sector != 0
                || !matches!(disk.bus_type, Some(1 | 2 | 3 | 7 | 10 | 11 | 17))
            {
                return None;
            }
            let mut modern = storage.iter().filter(|candidate| {
                candidate.number == row.index || serial(&candidate.serial_number) == Some(value)
            });
            let candidate = modern.next();
            if modern.next().is_some()
                || candidate.is_some_and(|candidate| {
                    candidate.number != row.index
                        || serial(&candidate.serial_number) != Some(value)
                        || candidate.size != disk.size
                        || candidate.logical_sector_size != Some(sector)
                        || candidate.is_offline != Some(true)
                        || candidate.is_boot != Some(false)
                        || candidate.is_system != Some(false)
                        || candidate.is_clustered != Some(false)
                })
            {
                return None;
            }
            let id = disk_identity::key(
                Some(value),
                disk.unique_id.as_deref(),
                row.model.as_deref(),
                disk.size,
                Some(sector),
                disk.bus_type,
            )?;
            let model = row
                .model
                .as_deref()
                .unwrap_or_default()
                .chars()
                .filter(|character| !character.is_control())
                .take(160)
                .collect();
            Some(Member {
                target: Target(id),
                number: row.index,
                serial: value.into(),
                size: disk.size,
                sector,
                model,
                known_offline: candidate.is_some(),
            })
        })
        .collect()
}

pub(super) fn list() -> Result<Vec<Member>, String> {
    let legacy = WMIConnection::new().map_err(data_windows::err)?;
    let legacy: Vec<Legacy> = legacy
        .raw_query("SELECT Index,SerialNumber,Model,BytesPerSector FROM Win32_DiskDrive")
        .map_err(data_windows::err)?;
    let con = WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
        .map_err(data_windows::err)?;
    let physical: Vec<Physical> = con
        .raw_query(
            "SELECT SerialNumber,UniqueId,Size,LogicalSectorSize,BusType FROM MSFT_PhysicalDisk",
        )
        .map_err(data_windows::err)?;
    let storage: Vec<StorageDisk> = con.raw_query(
        "SELECT Number,SerialNumber,Size,LogicalSectorSize,IsBoot,IsSystem,IsOffline,IsClustered FROM MSFT_Disk",
    ).map_err(data_windows::err)?;
    if legacy.len() > 256 || physical.len() > 256 || storage.len() > 256 {
        return Err("Windows 返回的磁盘数量超出核验范围。".into());
    }
    Ok(bind(&legacy, &physical, &storage))
}

impl Member {
    pub(super) fn reader(&self) -> Result<Region<data_windows::OfflinePhysical>, String> {
        data_windows::offline_physical_reader(self.number, self.size, self.sector, &self.serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_candidates_corroborate_identity_and_fail_closed_on_provider_conflicts() {
        let legacy = Legacy {
            index: 1,
            serial_number: Some("disk".into()),
            model: None,
            bytes_per_sector: Some(4096),
        };
        let disk = Physical {
            serial_number: Some("disk ".into()),
            unique_id: None,
            size: 16 * 1024 * 1024,
            logical_sector_size: Some(4096),
            bus_type: Some(17),
        };
        let mut modern = StorageDisk {
            number: 1,
            serial_number: Some("disk".into()),
            size: disk.size,
            logical_sector_size: Some(4096),
            is_boot: Some(false),
            is_system: Some(false),
            is_offline: Some(false),
            is_clustered: Some(false),
        };
        let legacy = [legacy];
        let physical = [disk];
        assert!(!bind(&legacy, &physical, &[])[0].known_offline);
        assert!(bind(&legacy, &physical, std::slice::from_ref(&modern)).is_empty());
        modern.is_offline = Some(true);
        assert!(bind(&legacy, &physical, std::slice::from_ref(&modern))[0].known_offline);
        modern.number = 2;
        assert!(bind(&legacy, &physical, &[modern]).is_empty());
    }
}

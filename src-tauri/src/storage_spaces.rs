//! Windows-reported Storage Spaces topology. All inputs come from fixed
//! metadata queries. Association references never leave the process.
use crate::{disk_identity, storage::same_reference};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_OBJECTS: usize = 4096;
const MAX_LINKS: usize = 32768;
const POOL_FIELDS: &str = "SELECT __PATH,UniqueId,FriendlyName,IsPrimordial,Size,AllocatedSize,HealthStatus,OperationalStatus,IsReadOnly,IsClustered FROM MSFT_StoragePool";
const MEMBER_FIELDS: &str = "SELECT __PATH,UniqueId,SerialNumber,FriendlyName,Size,HealthStatus,OperationalStatus,Usage FROM MSFT_PhysicalDisk";
const POOL_MEMBERS: &str = "SELECT StoragePool,PhysicalDisk FROM MSFT_StoragePoolToPhysicalDisk";
const INCOMPLETE: &str = "Windows 存储池关联不完整或冲突，请重新读取。";
const MEMBER: &str = "此磁盘是存储池成员，不能作为独立磁盘操作。";

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PoolRow {
    #[serde(rename = "__PATH")]
    path: String,
    unique_id: Option<String>,
    friendly_name: Option<String>,
    is_primordial: Option<bool>,
    size: Option<u64>,
    allocated_size: Option<u64>,
    health_status: Option<u16>,
    operational_status: Option<Vec<u16>>,
    is_read_only: Option<bool>,
    is_clustered: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MemberRow {
    #[serde(rename = "__PATH")]
    path: String,
    unique_id: Option<String>,
    serial_number: Option<String>,
    friendly_name: Option<String>,
    size: Option<u64>,
    health_status: Option<u16>,
    operational_status: Option<Vec<u16>>,
    usage: Option<u16>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SpaceRow {
    #[serde(rename = "__PATH")]
    path: String,
    unique_id: Option<String>,
    friendly_name: Option<String>,
    size: Option<u64>,
    allocated_size: Option<u64>,
    footprint_on_pool: Option<u64>,
    health_status: Option<u16>,
    operational_status: Option<Vec<u16>>,
    resiliency_setting_name: Option<String>,
    number_of_columns: Option<u16>,
    number_of_data_copies: Option<u16>,
    number_of_available_copies: Option<u16>,
    physical_disk_redundancy: Option<u16>,
    provisioning_type: Option<u16>,
    interleave: Option<u64>,
    logical_sector_size: Option<u64>,
    physical_sector_size: Option<u64>,
    detached_reason: Option<u16>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DiskRow {
    #[serde(rename = "__PATH")]
    path: String,
    number: u32,
    serial_number: Option<String>,
    unique_id: Option<String>,
    friendly_name: Option<String>,
    size: u64,
    logical_sector_size: Option<u64>,
    bus_type: Option<u16>,
    is_offline: Option<bool>,
    is_read_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PoolMember {
    storage_pool: String,
    physical_disk: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PoolSpace {
    storage_pool: String,
    virtual_disk: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SpaceMember {
    virtual_disk: String,
    physical_disk: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SpaceDisk {
    virtual_disk: String,
    disk: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    id: String,
    name: Option<String>,
    unique_id: Option<String>,
    size: Option<u64>,
    allocated: Option<u64>,
    health: Option<u16>,
    operational: Vec<u16>,
    read_only: Option<bool>,
    clustered: Option<bool>,
    complete: bool,
    members: Vec<Member>,
    spaces: Vec<Space>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    id: String,
    name: Option<String>,
    serial: Option<String>,
    size: Option<u64>,
    health: Option<u16>,
    operational: Vec<u16>,
    usage: Option<u16>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Space {
    id: String,
    name: Option<String>,
    unique_id: Option<String>,
    size: Option<u64>,
    allocated: Option<u64>,
    footprint: Option<u64>,
    health: Option<u16>,
    operational: Vec<u16>,
    resiliency: Option<String>,
    columns: Option<u16>,
    copies: Option<u16>,
    available_copies: Option<u16>,
    redundancy: Option<u16>,
    provisioning: Option<u16>,
    interleave: Option<u64>,
    logical_sector: Option<u64>,
    physical_sector: Option<u64>,
    detached_reason: Option<u16>,
    disk: Option<Disk>,
    member_ids: Vec<String>,
    complete: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Disk {
    number: u32,
    target: Option<String>,
    offline: Option<bool>,
    read_only: Option<bool>,
}

#[derive(Default)]
struct Inventory {
    pools: Vec<PoolRow>,
    members: Vec<MemberRow>,
    spaces: Vec<SpaceRow>,
    disks: Vec<DiskRow>,
    pool_members: Vec<PoolMember>,
    pool_spaces: Vec<PoolSpace>,
    space_members: Vec<SpaceMember>,
    space_disks: Vec<SpaceDisk>,
}

fn bounded(objects: &[usize], links: &[usize]) -> Result<(), String> {
    if objects.iter().any(|n| *n > MAX_OBJECTS) || links.iter().any(|n| *n > MAX_LINKS) {
        return Err("Windows 返回的存储池对象或关联过多。".into());
    }
    Ok(())
}

fn label(text: &Option<String>) -> Option<String> {
    let text: String = text
        .as_deref()?
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn bytes(value: Option<u64>) -> Option<u64> {
    value.filter(|n| *n < 1 << 53)
}

fn id(path: &str) -> String {
    format!("{:x}", Sha256::digest(path.as_bytes()))
}

fn resolve_links<'a, T>(
    references: impl Iterator<Item = &'a str>,
    rows: &'a [T],
    path: impl Fn(&T) -> &str,
) -> (Vec<&'a T>, bool) {
    let mut found = Vec::new();
    let mut indices = HashSet::new();
    let mut complete = true;
    for reference in references {
        let mut matches = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| same_reference(reference, path(row)));
        match (matches.next(), matches.next()) {
            (Some((index, row)), None) if indices.insert(index) => found.push(row),
            _ => complete = false,
        }
    }
    (found, complete)
}

impl Inventory {
    fn pools(&self) -> Result<Vec<Pool>, String> {
        bounded(
            &[
                self.pools.len(),
                self.members.len(),
                self.spaces.len(),
                self.disks.len(),
            ],
            &[
                self.pool_members.len(),
                self.pool_spaces.len(),
                self.space_disks.len(),
                self.space_members.len(),
            ],
        )?;
        if self.pools.iter().any(|p| p.is_primordial.is_none()) {
            return Err(INCOMPLETE.into());
        }
        let mut result = Vec::new();
        for row in self.pools.iter().filter(|p| p.is_primordial == Some(false)) {
            if row.path.is_empty()
                || self
                    .pools
                    .iter()
                    .filter(|p| same_reference(&row.path, &p.path))
                    .count()
                    != 1
            {
                return Err(INCOMPLETE.into());
            }
            let (members, member_links) = resolve_links(
                self.pool_members
                    .iter()
                    .filter(|link| same_reference(&link.storage_pool, &row.path))
                    .map(|link| link.physical_disk.as_str()),
                &self.members,
                |member| &member.path,
            );
            let (spaces, space_links) = resolve_links(
                self.pool_spaces
                    .iter()
                    .filter(|link| same_reference(&link.storage_pool, &row.path))
                    .map(|link| link.virtual_disk.as_str()),
                &self.spaces,
                |space| &space.path,
            );
            let complete = !members.is_empty() && member_links && space_links;
            result.push(Pool {
                id: id(&row.path),
                name: label(&row.friendly_name),
                unique_id: label(&row.unique_id),
                size: bytes(row.size),
                allocated: bytes(row.allocated_size),
                health: row.health_status,
                operational: row.operational_status.clone().unwrap_or_default(),
                read_only: row.is_read_only,
                clustered: row.is_clustered,
                complete,
                spaces: spaces
                    .into_iter()
                    .map(|space| self.space(space, &members, complete))
                    .collect(),
                members: members
                    .into_iter()
                    .map(|member| Member {
                        id: id(&member.path),
                        name: label(&member.friendly_name),
                        serial: label(&member.serial_number),
                        size: bytes(member.size),
                        health: member.health_status,
                        operational: member.operational_status.clone().unwrap_or_default(),
                        usage: member.usage,
                    })
                    .collect(),
            });
        }
        Ok(result)
    }

    fn space(&self, space: &SpaceRow, pool_members: &[&MemberRow], pool_complete: bool) -> Space {
        let (members, member_links) = resolve_links(
            self.space_members
                .iter()
                .filter(|link| same_reference(&link.virtual_disk, &space.path))
                .map(|link| link.physical_disk.as_str()),
            pool_members,
            |member| &member.path,
        );
        let (disks, disk_links) = resolve_links(
            self.space_disks
                .iter()
                .filter(|link| same_reference(&link.virtual_disk, &space.path))
                .map(|link| link.disk.as_str()),
            &self.disks,
            |disk| &disk.path,
        );
        let exclusive = self
            .pool_spaces
            .iter()
            .filter(|link| same_reference(&link.virtual_disk, &space.path))
            .count()
            == 1;
        let detached = space
            .operational_status
            .as_deref()
            .is_some_and(|states| states.contains(&0xd002));
        let (disk, disk_complete) = match disks.as_slice() {
            [disk]
                if disk_links
                    && !detached
                    && disk.bus_type == Some(16)
                    && Some(disk.size) == space.size
                    && self
                        .disks
                        .iter()
                        .filter(|other| other.number == disk.number)
                        .count()
                        == 1 =>
            {
                (
                    Some(Disk {
                        number: disk.number,
                        target: disk_identity::key(
                            disk.serial_number.as_deref(),
                            disk.unique_id.as_deref(),
                            disk.friendly_name.as_deref(),
                            disk.size,
                            disk.logical_sector_size,
                            disk.bus_type,
                        ),
                        offline: disk.is_offline,
                        read_only: disk.is_read_only,
                    }),
                    true,
                )
            }
            [] if disk_links => (None, true),
            _ => (None, false),
        };
        Space {
            id: id(&space.path),
            name: label(&space.friendly_name),
            unique_id: label(&space.unique_id),
            size: bytes(space.size),
            allocated: bytes(space.allocated_size),
            footprint: bytes(space.footprint_on_pool),
            health: space.health_status,
            operational: space.operational_status.clone().unwrap_or_default(),
            resiliency: label(&space.resiliency_setting_name),
            columns: space.number_of_columns.filter(|n| *n > 0),
            copies: space.number_of_data_copies,
            available_copies: space.number_of_available_copies,
            redundancy: space.physical_disk_redundancy,
            provisioning: space.provisioning_type,
            interleave: bytes(space.interleave),
            logical_sector: space.logical_sector_size,
            physical_sector: space.physical_sector_size,
            detached_reason: space.detached_reason,
            disk,
            member_ids: members.into_iter().map(|member| id(&member.path)).collect(),
            complete: pool_complete && member_links && disk_complete && exclusive,
        }
    }
}

pub(crate) struct PoolProtection {
    members: Vec<MemberRow>,
    links: Vec<PoolMember>,
    pools: Vec<PoolRow>,
}

impl PoolProtection {
    #[cfg(windows)]
    pub(crate) fn read(con: &wmi::WMIConnection) -> Result<Self, String> {
        let pools: Vec<PoolRow> = query(con, POOL_FIELDS)?;
        if pools.iter().any(|p| p.is_primordial.is_none()) {
            return Err(INCOMPLETE.into());
        }
        let mut protection = Self {
            pools,
            members: Vec::new(),
            links: Vec::new(),
        };
        if protection
            .pools
            .iter()
            .any(|p| p.is_primordial == Some(false))
        {
            protection.members = query(con, MEMBER_FIELDS)?;
            protection.links = query(con, POOL_MEMBERS)?;
        }
        bounded(
            &[protection.pools.len(), protection.members.len()],
            &[protection.links.len()],
        )?;
        Ok(protection)
    }

    pub(crate) fn require_standalone(
        &self,
        serial: Option<&str>,
        unique_id: Option<&str>,
    ) -> Result<(), String> {
        fn identity(value: Option<&str>) -> Option<&str> {
            value.map(disk_identity::clean).filter(|s| !s.is_empty())
        }
        for pool in self.pools.iter().filter(|p| p.is_primordial == Some(false)) {
            let (members, complete) = resolve_links(
                self.links
                    .iter()
                    .filter(|link| same_reference(&link.storage_pool, &pool.path))
                    .map(|link| link.physical_disk.as_str()),
                &self.members,
                |member| &member.path,
            );
            if !complete || members.is_empty() {
                return Err(INCOMPLETE.into());
            }
            for member in members {
                let comparable = [
                    identity(serial).zip(identity(member.serial_number.as_deref())),
                    identity(unique_id).zip(identity(member.unique_id.as_deref())),
                ];
                if comparable
                    .iter()
                    .flatten()
                    .any(|(given, reported)| given.eq_ignore_ascii_case(reported))
                {
                    return Err(MEMBER.into());
                }
                if comparable.iter().all(Option::is_none) {
                    return Err(INCOMPLETE.into());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn check_disk(&self, disk: &crate::partitions::policy::Disk) -> Result<(), String> {
        if disk.bus_type == Some(16) {
            return Err("存储空间不能作为独立物理盘操作。".into());
        }
        self.require_standalone(disk.serial_number.as_deref(), disk.unique_id.as_deref())
    }
}

#[cfg(windows)]
fn query<T: serde::de::DeserializeOwned>(
    con: &wmi::WMIConnection,
    query: &str,
) -> Result<Vec<T>, String> {
    con.raw_query(query)
        .map_err(|error| format!("无法读取 Windows 存储空间信息：{error}"))
}

pub fn read() -> Result<Vec<Pool>, String> {
    #[cfg(windows)]
    {
        let con = wmi::WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
            .map_err(|error| format!("无法读取 Windows 存储空间信息：{error}"))?;
        let protection = PoolProtection::read(&con)?;
        if protection
            .pools
            .iter()
            .all(|p| p.is_primordial == Some(true))
        {
            return Ok(Vec::new());
        }
        Inventory {
            pools: protection.pools,
            members: protection.members,
            pool_members: protection.links,
            spaces: query(&con, "SELECT __PATH,UniqueId,FriendlyName,Size,AllocatedSize,FootprintOnPool,HealthStatus,OperationalStatus,ResiliencySettingName,NumberOfColumns,NumberOfDataCopies,NumberOfAvailableCopies,PhysicalDiskRedundancy,ProvisioningType,Interleave,LogicalSectorSize,PhysicalSectorSize,DetachedReason FROM MSFT_VirtualDisk")?,
            disks: query(&con, "SELECT __PATH,Number,SerialNumber,UniqueId,FriendlyName,Size,LogicalSectorSize,BusType,IsOffline,IsReadOnly FROM MSFT_Disk")?,
            pool_spaces: query(&con, "SELECT StoragePool,VirtualDisk FROM MSFT_StoragePoolToVirtualDisk")?,
            space_members: query(&con, "SELECT VirtualDisk,PhysicalDisk FROM MSFT_VirtualDiskToPhysicalDisk")?,
            space_disks: query(&con, "SELECT VirtualDisk,Disk FROM MSFT_VirtualDiskToDisk")?,
        }.pools()
    }
    #[cfg(not(windows))]
    Err("存储空间信息当前仅支持 Windows。".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(class: &str, name: &str) -> String {
        format!(r#"\\guest\ROOT\Microsoft\Windows\Storage:{class}.ObjectId="{name}""#)
    }

    #[test]
    fn topology_keeps_explicit_pool_members_and_marks_dangling_or_conflicting_links() {
        let pool = local("MSFT_StoragePool", "Pool");
        let primordial = local("MSFT_StoragePool", "Available");
        let mirror = local("MSFT_VirtualDisk", "Mirror");
        let detached = local("MSFT_VirtualDisk", "Detached");
        let member = local("MSFT_PhysicalDisk", "Member");
        let outside = local("MSFT_PhysicalDisk", "SameNameDifferentIdentity");
        let disk = local("MSFT_Disk", "Disk");
        let mut input = Inventory {
            pools: vec![
                PoolRow {
                    path: primordial.clone(),
                    is_primordial: Some(true),
                    ..Default::default()
                },
                PoolRow {
                    path: pool.clone(),
                    is_primordial: Some(false),
                    ..Default::default()
                },
            ],
            members: vec![
                MemberRow {
                    path: member.clone(),
                    friendly_name: Some("NVMe".into()),
                    ..Default::default()
                },
                MemberRow {
                    path: outside.clone(),
                    friendly_name: Some("NVMe".into()),
                    ..Default::default()
                },
            ],
            spaces: vec![
                SpaceRow {
                    path: mirror.clone(),
                    size: Some(512),
                    number_of_data_copies: Some(2),
                    ..Default::default()
                },
                SpaceRow {
                    path: detached.clone(),
                    operational_status: Some(vec![0xd002]),
                    ..Default::default()
                },
            ],
            disks: vec![DiskRow {
                path: disk.clone(),
                number: 3,
                size: 512,
                bus_type: Some(16),
                ..Default::default()
            }],
            pool_members: vec![
                PoolMember {
                    storage_pool: primordial,
                    physical_disk: outside.clone(),
                },
                PoolMember {
                    storage_pool: pool.clone(),
                    physical_disk: member.replace(r"\\guest\", r"\\.\"),
                },
            ],
            pool_spaces: vec![
                PoolSpace {
                    storage_pool: pool.clone(),
                    virtual_disk: mirror.clone(),
                },
                PoolSpace {
                    storage_pool: pool,
                    virtual_disk: detached,
                },
            ],
            space_members: vec![SpaceMember {
                virtual_disk: mirror.clone(),
                physical_disk: member,
            }],
            space_disks: vec![SpaceDisk {
                virtual_disk: mirror.clone(),
                disk: disk.clone(),
            }],
        };
        let topology = input.pools().unwrap();
        let pool = &topology[0];
        assert_eq!(topology.len(), 1);
        assert!(pool.complete);
        assert_eq!(pool.members.len(), 1);
        assert!(pool.spaces[0].complete);
        assert_eq!(pool.spaces[0].disk.as_ref().unwrap().number, 3);
        assert_eq!(pool.spaces[0].member_ids, [pool.members[0].id.clone()]);
        assert!(pool.spaces[1].disk.is_none());
        assert!(pool.spaces[1].complete);
        assert!(!serde_json::to_string(&topology)
            .unwrap()
            .contains("ObjectId"));
        input.space_members.push(SpaceMember {
            virtual_disk: mirror.clone(),
            physical_disk: outside,
        });
        input.space_disks.push(SpaceDisk {
            virtual_disk: mirror,
            disk,
        });
        let topology = input.pools().unwrap();
        assert!(topology[0].spaces[0].disk.is_none());
        assert!(!topology[0].spaces[0].complete);
        assert_eq!(topology[0].spaces[0].member_ids.len(), 1);
    }

    #[test]
    fn operation_protection_checks_serial_and_id_and_rejects_unprovable_membership() {
        let pool = local("MSFT_StoragePool", "Pool");
        let disk = local("MSFT_PhysicalDisk", "Member");
        let mut protection = PoolProtection {
            pools: vec![PoolRow {
                path: pool.clone(),
                is_primordial: Some(false),
                ..Default::default()
            }],
            members: vec![MemberRow {
                path: disk.clone(),
                serial_number: Some(" member-a ".into()),
                unique_id: Some("pool-a".into()),
                ..Default::default()
            }],
            links: vec![PoolMember {
                storage_pool: pool,
                physical_disk: disk,
            }],
        };
        assert_eq!(
            protection
                .require_standalone(Some("MEMBER-A"), None)
                .unwrap_err(),
            MEMBER
        );
        assert_eq!(
            protection
                .require_standalone(None, Some("POOL-A"))
                .unwrap_err(),
            MEMBER
        );
        assert!(protection
            .require_standalone(Some("Ordinary-disk"), None)
            .is_ok());
        protection.members[0].serial_number = None;
        assert_eq!(
            protection
                .require_standalone(Some("Ordinary-disk"), None)
                .unwrap_err(),
            INCOMPLETE
        );
    }
}

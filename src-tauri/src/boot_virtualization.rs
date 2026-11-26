//! Verify the Windows boot chain and plan any EFI fallback in the copy only.
use super::{boot_path, inspect, open_extent, pe_machine, BindingState, Extent, Mode, EFI_MANAGER};
use crate::{
    backup_archive::Source,
    fat_recovery::boot_files::rebuild,
    image_table_conversion::{Overlay, Patch},
    system_virtualization::{Controller, Firmware},
    table_archive,
    used_space::Span,
};
use regf::{DataType, RegistryHive};
use sha2::{Digest, Sha256};
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    io::{self, Read, Seek},
};
use uuid::Uuid;

const X64: u16 = 0x8664;
const MAX_SYSTEM: usize = 64 * 1024 * 1024;
const MAX_PATCH: usize = 32 * 1024 * 1024;
const BOOTX64: &[&str] = &["EFI", "Boot", "bootx64.efi"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemHive {
    Clean,
    SnapshotLogs,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub firmware: Firmware,
    pub controller: Controller,
    pub driver: &'static str,
    size: u64,
    table_digest: String,
    windows: WindowsBoot,
    hive_digest: String,
    driver_digest: String,
    patches: Vec<Patch>,
}
impl Plan {
    pub(crate) fn adds_efi_fallback(&self) -> bool {
        !self.patches.is_empty()
    }
    pub(crate) fn reader<R: Read + Seek>(&self, reader: R) -> io::Result<Overlay<'_, R>> {
        Overlay::new(reader, &self.patches, self.size)
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn system_hive(bytes: Vec<u8>) -> Result<RegistryHive, String> {
    if !(4096..=MAX_SYSTEM).contains(&bytes.len())
        || &bytes[..4] != b"regf"
        || bytes[4..8] != bytes[8..12]
    {
        return Err("Windows SYSTEM 配置单元无效或需要回放日志。".into());
    }
    let hive = RegistryHive::from_bytes(bytes).map_err(|e| e.to_string())?;
    if !hive.base_block().checksum_valid {
        return Err("Windows SYSTEM 配置单元的校验和无效。".into());
    }
    Ok(hive)
}
fn enabled_storage(
    hive: &RegistryHive,
    firmware: Firmware,
) -> Result<Vec<(Controller, &'static str)>, String> {
    let default = hive
        .open_key("Select")
        .and_then(|select| select.value("Default"))
        .and_then(|value| value.dword_data())
        .map_err(|e| e.to_string())?;
    if !(1..=999).contains(&default) {
        return Err("无法核对 Windows 默认控制集。".into());
    }
    let services = hive
        .open_key(&format!("ControlSet{default:03}\\Services"))
        .map_err(|e| e.to_string())?;
    let mut choices = Vec::new();
    for (controller, driver) in [
        (Controller::Sata, "storahci"),
        (Controller::Sata, "msahci"),
        (Controller::Nvme, "stornvme"),
    ] {
        if controller == Controller::Nvme && firmware != Firmware::Uefi {
            continue;
        }
        let service = match services.open_subkey(driver) {
            Ok(service) => service,
            Err(regf::Error::KeyNotFound(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let start = service
            .value("Start")
            .and_then(|value| value.dword_data())
            .map_err(|e| e.to_string())?;
        let override_start = match service.open_subkey("StartOverride") {
            Ok(key) => match key.value("0") {
                Ok(value) if value.data_type() == DataType::Dword => {
                    Some(value.dword_data().map_err(|e| e.to_string())?)
                }
                Ok(_) => return Err("存储驱动的启动覆盖值类型无效。".into()),
                Err(regf::Error::ValueNotFound(_)) => None,
                Err(error) => return Err(error.to_string()),
            },
            Err(regf::Error::KeyNotFound(_)) => None,
            Err(error) => return Err(error.to_string()),
        };
        if start == 0 && override_start.is_none_or(|v| v == 0) {
            choices.push((controller, driver));
        }
    }
    Ok(choices)
}

pub(crate) fn fallback(
    reader: &mut impl Source,
    size: u64,
    sector: u64,
    windows: &WindowsBoot,
) -> Result<Vec<Patch>, String> {
    if windows.firmware != Firmware::Uefi {
        return Ok(Vec::new());
    }
    fallback_extent(reader, size, sector, windows.boot, &windows.manager_digest)
}

fn fallback_extent(
    reader: &mut impl Source,
    size: u64,
    sector: u64,
    esp: Extent,
    expected: &str,
) -> Result<Vec<Patch>, String> {
    let edits = {
        let (mut view, fs) = open_extent(reader, esp, sector)?;
        let manager = fs
            .read_file(&mut view, EFI_MANAGER, super::MAX_EXECUTABLE)?
            .ok_or("EFI 启动管理器已失效。")?;
        if hash(&manager) != expected || pe_machine(&manager, &[10]) != Ok(X64) {
            return Err("EFI 启动管理器与检查结果不一致。".into());
        }
        if let Some(existing) = fs.read_file(&mut view, BOOTX64, super::MAX_EXECUTABLE)? {
            return if existing == manager {
                Ok(Vec::new())
            } else {
                Err("EFI 回退入口与 Windows 启动管理器不同，无法核对新虚拟机的启动路径。".into())
            };
        }
        rebuild::plan(
            &mut view,
            sector,
            &[(rebuild::Target::FallbackX64, &manager)],
            rebuild::BcdUpdate::Keep,
        )?
    };
    let mut sectors = BTreeMap::new();
    for (_, edits) in edits.phases {
        for edit in edits {
            if edit
                .offset
                .checked_add(edit.bytes.len() as u64)
                .is_none_or(|end| end > esp.size)
            {
                return Err("EFI 回退更新越出已核对的系统分区。".into());
            }
            let mut at = edit.offset;
            let mut bytes = edit.bytes.as_slice();
            while !bytes.is_empty() {
                let used_bytes = sectors.len() * sector as usize;
                let offset = esp.start + at / sector * sector;
                let patch = match sectors.entry(offset) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        if used_bytes >= MAX_PATCH {
                            return Err("EFI 回退更新超出内存规划范围。".into());
                        }
                        let before = table_archive::read_at(reader, offset, sector as usize, size)?;
                        entry.insert(Patch {
                            offset,
                            after: before.clone(),
                            before,
                        })
                    }
                };
                let start = (at % sector) as usize;
                let used = bytes.len().min(sector as usize - start);
                patch.after[start..start + used].copy_from_slice(&bytes[..used]);
                at += used as u64;
                bytes = &bytes[used..];
            }
        }
    }
    let patches: Vec<_> = sectors
        .into_values()
        .filter(|p| p.before != p.after)
        .collect();
    let actual = {
        let mut view = Overlay::new(reader, &patches, size).map_err(|e| e.to_string())?;
        let (mut partition, fs) = open_extent(&mut view, esp, sector)?;
        fs.read_file(&mut partition, BOOTX64, super::MAX_EXECUTABLE)?
            .ok_or("无法在输出布局中读取 EFI 回退入口。")?
    };
    if hash(&actual) != expected {
        return Err("EFI 回退入口的预期摘要不一致。".into());
    }
    Ok(patches)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowsBoot {
    pub firmware: Firmware,
    boot: Extent,
    os: Extent,
    root: String,
    loader: Uuid,
    manager_digest: String,
    bcd_digest: String,
    loader_digest: String,
}

impl WindowsBoot {
    pub(crate) fn with_system_extent(&self, extent: Option<(u64, u64)>) -> Self {
        let mut expected = self.clone();
        if let Some((start, size)) = extent {
            expected.os = Extent { start, size };
        }
        expected
    }
    pub(crate) fn boot_offset(&self) -> u64 {
        self.boot.start
    }
    pub(crate) fn windows_offset(&self) -> u64 {
        self.os.start
    }

    pub(crate) fn gpt_dependencies(
        &self,
        reader: &mut impl Source,
        sector: u64,
        disk: Uuid,
    ) -> Result<BTreeSet<Uuid>, String> {
        let (mut view, fs) = open_extent(reader, self.boot, sector)?;
        let bytes = fs
            .read_file(&mut view, super::EFI_BCD, crate::boot_configuration::MAX_STORE)?
            .ok_or("未找到可核对的 Windows BCD。")?;
        if hash(&bytes) != self.bcd_digest {
            return Err("预览后 Windows BCD 已变化。".into());
        }
        crate::boot_configuration::gpt_dependencies(bytes, disk)
    }

    pub(crate) fn system_digest(
        &self, reader: &mut impl Source, sector: u64, resized: bool, policy: SystemHive,
    ) -> Result<String, String> {
        let (mut view, fs) = open_extent(reader, self.os, sector)?;
        let super::Filesystem::Ntfs(ntfs) = &fs else {
            return Err("需要可读取的 NTFS Windows 系统分区。".into());
        };
        if resized {
            crate::ntfs_allocation::require_resize_result(ntfs, &mut view)?;
        } else {
            crate::ntfs_allocation::require_clean(ntfs, &mut view)?;
        }
        let mut path = boot_path(&self.root)?;
        path.extend(["System32", "config", "SYSTEM"]);
        let bytes = fs
            .read_file(&mut view, &path, MAX_SYSTEM)?
            .ok_or("缺少 Windows SYSTEM 配置单元。")?;
        if policy == SystemHive::SnapshotLogs && bytes.len() >= 4096 && &bytes[..4] == b"regf"
            && bytes[4..8] != bytes[8..12]
        {
            let mut logs = Vec::new();
            let mut left = MAX_SYSTEM;
            let file = path.len() - 1;
            for name in ["SYSTEM.LOG", "SYSTEM.LOG1", "SYSTEM.LOG2"] {
                path[file] = name;
                if let Some(log) = fs.read_file(&mut view, &path, left)? {
                    left = left.checked_sub(log.len()).ok_or("Windows SYSTEM 快照日志超过上限。")?;
                    logs.push((name, log));
                }
            }
            let original_logs: Vec<_> = logs.iter().map(|(name, bytes)| (*name, bytes.as_slice())).collect();
            use crate::boot_configuration::recovery::{replay_hive, ReplayScope};
            let (recovered, _) = replay_hive(bytes, &original_logs, MAX_SYSTEM, ReplayScope::LiveSnapshot)
                .map_err(|error| format!("Windows SYSTEM 快照日志无法可靠回放。{error}"))?;
            let digest = hash(&recovered);
            let hive = system_hive(recovered)?;
            let default = hive.open_key("Select")
                .and_then(|select| select.value("Default"))
                .and_then(|value| value.dword_data())
                .map_err(|error| error.to_string())?;
            if !(1..=999).contains(&default)
                || hive.open_key(&format!("ControlSet{default:03}\\Services")).is_err()
            {
                return Err("无法核对 Windows 默认控制集。".into());
            }
            Ok(digest)
        } else {
            let digest = hash(&bytes);
            system_hive(bytes)?;
            Ok(digest)
        }
    }
}

pub(crate) fn check_windows(
    source: &mut impl Source,
    size: u64,
    sector: u64,
    expected: Option<&[Span]>,
) -> Result<(WindowsBoot, table_archive::Archive), String> {
    let report = inspect(
        &mut *source,
        "offline-windows".into(),
        size,
        sector,
        false,
        expected,
    )?;
    let [boot] = report.volumes.as_slice() else {
        return Err("需要唯一可核对的 EFI 系统分区或活动 MBR 主分区。".into());
    };
    if !boot.boot_signature
        || boot.error.is_some()
        || boot.mode == Mode::Bios && report.mbr_code_present != Some(true)
    {
        return Err("启动分区或 BIOS 启动代码未通过检查。".into());
    }
    let firmware = if boot.mode == Mode::Uefi {
        Firmware::Uefi
    } else {
        Firmware::Bios
    };
    let artifact = |path: &[&str]| {
        boot.artifacts.iter().find(|file| {
            file.path
                .eq_ignore_ascii_case(&format!("\\{}", path.join("\\")))
                && file.state == "present"
        })
    };
    let (manager_path, bcd_path) = if firmware == Firmware::Uefi {
        (EFI_MANAGER, super::EFI_BCD)
    } else {
        (super::BIOS_MANAGER, super::BIOS_BCD)
    };
    let manager_digest = artifact(manager_path)
        .filter(|file| firmware == Firmware::Bios || file.machine == Some(X64))
        .and_then(|file| file.sha256.clone())
        .ok_or("未找到适合的 Windows 启动管理器。")?;
    let bcd_digest = artifact(bcd_path)
        .and_then(|file| file.sha256.clone())
        .ok_or("未找到可核对的 Windows BCD。")?;
    let loader = boot
        .store
        .as_ref()
        .and_then(|store| store.next_loader())
        .ok_or("Windows BCD 中没有可核对的下一次启动项。")?;
    let reference = boot
        .references
        .iter()
        .find(|reference| reference.id == loader.id)
        .ok_or("无法核对 Windows 启动项的磁盘引用。")?;
    let local =
        |b: &super::Binding| matches!(b.state, BindingState::Matched | BindingState::Relative);
    if !reference.application.as_ref().is_some_and(local)
        || !reference.system.as_ref().is_some_and(local)
        || !reference
            .system_root
            .as_ref()
            .is_some_and(|a| a.state == "present")
    {
        return Err("Windows 启动项引用同一镜像外的设备，或系统目录无法读取。".into());
    }
    let loader_digest = reference
        .loader
        .as_ref()
        .filter(|a| a.state == "present" && a.machine == Some(X64))
        .and_then(|a| a.sha256.clone())
        .ok_or("下一次启动项没有可核对的 x64 Windows 加载器。")?;
    let archive = table_archive::capture(source, size, sector, "0".repeat(64), false, || Ok(()))?;
    let info = archive.validate()?;
    let os = reference
        .system
        .as_ref()
        .and_then(|binding| binding.offset)
        .and_then(|offset| {
            info.layout
                .iter()
                .find(|partition| partition.offset == offset)
        })
        .ok_or("无法核对 Windows 系统分区范围。")?;
    let root = loader
        .system_root
        .as_ref()
        .ok_or("BCD 缺少 Windows 系统目录。")?;
    boot_path(root)?;
    let os = Extent {
        start: os.offset,
        size: os.size,
    };
    {
        let (_, fs) = open_extent(source, os, sector)?;
        if fs.name() != "NTFS" {
            return Err("需要可读取的 NTFS Windows 系统分区。".into());
        }
    }
    Ok((
        WindowsBoot {
            firmware,
            boot: Extent {
                start: boot.offset,
                size: boot.size,
            },
            os,
            root: root.clone(),
            loader: loader.id,
            manager_digest,
            bcd_digest,
            loader_digest,
        },
        archive,
    ))
}

pub(crate) fn prepare(
    source: &mut impl Source,
    size: u64,
    sector: u64,
    expected: Option<&[Span]>,
) -> Result<Plan, String> {
    if sector != 512 {
        return Err("VMware 和 VirtualBox 系统导出需要 512 B 逻辑扇区。".into());
    }
    let (windows, archive) = check_windows(source, size, sector, expected)?;
    let firmware = windows.firmware;
    let root = boot_path(&windows.root)?;
    let (controller, driver, hive_digest, driver_digest) = {
        let (mut view, fs) = open_extent(source, windows.os, sector)?;
        let mut registry = root.clone();
        registry.extend(["System32", "config", "SYSTEM"]);
        let bytes = fs
            .read_file(&mut view, &registry, MAX_SYSTEM)?
            .ok_or("缺少 Windows SYSTEM 配置单元。")?;
        let hive_digest = hash(&bytes);
        let hive = system_hive(bytes)?;
        let mut detected = None;
        for (controller, driver) in enabled_storage(&hive, firmware)? {
            let file = format!("{driver}.sys");
            let mut path = root.clone();
            path.extend(["System32", "drivers", &file]);
            if let Some(bytes) = fs.read_file(&mut view, &path, super::MAX_EXECUTABLE)? {
                if pe_machine(&bytes, &[1]) == Ok(X64) {
                    detected = Some((controller, driver, hive_digest.clone(), hash(&bytes)));
                    break;
                }
            }
        }
        detected.ok_or("找不到已启用且可核对的 x64 SATA 或 NVMe 启动驱动。")?
    };
    let patches = fallback(source, size, sector, &windows)?;
    Ok(Plan {
        firmware,
        controller,
        driver,
        size,
        table_digest: archive.validate()?.sha256,
        windows,
        hive_digest,
        driver_digest,
        patches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::{
        fat_format::{FatType, Plan as Format},
        MIB,
    };
    use regf::HiveBuilder;
    use std::io::Cursor;

    fn storage_hive(override_sata: Option<u32>) -> Vec<u8> {
        let mut hive = HiveBuilder::new().unwrap();
        hive.create_key("Select").unwrap();
        hive.add_value_dword("Select", Some("Default"), 2).unwrap();
        for (driver, start) in [("storahci", 0), ("msahci", 3), ("stornvme", 0)] {
            let path = format!("ControlSet002\\Services\\{driver}");
            hive.create_key(&path).unwrap();
            hive.add_value_dword(&path, Some("Start"), start).unwrap();
        }
        if let Some(value) = override_sata {
            let path = "ControlSet002\\Services\\storahci\\StartOverride";
            hive.create_key(path).unwrap();
            hive.add_value_dword(path, Some("0"), value).unwrap();
        }
        hive.build().unwrap()
    }

    #[test]
    fn checks_default_control_set_overrides_and_bios_controller_compatibility() {
        let hive = system_hive(storage_hive(Some(3))).unwrap();
        assert_eq!(
            enabled_storage(&hive, Firmware::Uefi).unwrap(),
            vec![(Controller::Nvme, "stornvme")]
        );
        assert!(enabled_storage(&hive, Firmware::Bios).unwrap().is_empty());
        let hive = system_hive(storage_hive(None)).unwrap();
        assert_eq!(
            enabled_storage(&hive, Firmware::Bios).unwrap(),
            vec![(Controller::Sata, "storahci")]
        );
        let mut dirty = storage_hive(None);
        dirty[8] ^= 1;
        assert!(system_hive(dirty).is_err());
    }

    fn loader() -> Vec<u8> {
        let mut pe = vec![0; 512];
        pe[0..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&64u32.to_le_bytes());
        let header = &mut pe[64..];
        header[..4].copy_from_slice(b"PE\0\0");
        for (offset, word) in [
            (4, 0x8664u16),
            (6, 1),
            (20, 112),
            (22, 2),
            (24, 0x20b),
            (92, 10),
        ] {
            header[offset..offset + 2].copy_from_slice(&word.to_le_bytes());
        }
        pe
    }

    #[test]
    fn adds_only_a_missing_fallback_to_the_verified_copy() {
        let esp = Extent {
            start: MIB,
            size: 40 * MIB,
        };
        let size = 44 * MIB;
        let mut image = vec![0; size as usize];
        let manager = loader();
        let digest = hash(&manager);
        {
            let mut partition = Cursor::new(&mut image[MIB as usize..(41 * MIB) as usize]);
            Format::new(FatType::Fat32, esp.size, 512, 512, "BOOT", 11, 2048)
                .unwrap()
                .format(&mut partition, |_, _| Ok(()))
                .unwrap();
            let plan = rebuild::plan(
                &mut partition,
                512,
                &[(rebuild::Target::Manager, &manager)],
                rebuild::BcdUpdate::Keep,
            )
            .unwrap();
            for (_, edits) in plan.phases {
                for edit in edits {
                    let at = edit.offset as usize;
                    partition.get_mut()[at..at + edit.bytes.len()].copy_from_slice(&edit.bytes);
                }
            }
        }
        let initial = hash(&image);
        let patches = fallback_extent(&mut Cursor::new(&image), size, 512, esp, &digest).unwrap();
        assert!(!patches.is_empty());
        assert!(patches.iter().all(|patch| patch.offset >= esp.start
            && patch.offset + patch.after.len() as u64 <= esp.start + esp.size));
        {
            let mut view = Overlay::new(Cursor::new(&image), &patches, size).unwrap();
            let (mut partition, fs) = open_extent(&mut view, esp, 512).unwrap();
            assert_eq!(
                fs.read_file(&mut partition, BOOTX64, 512).unwrap().unwrap(),
                manager
            );
            assert_eq!(
                fs.read_file(&mut partition, EFI_MANAGER, 512)
                    .unwrap()
                    .unwrap(),
                manager
            );
        }
        assert_eq!(hash(&image), initial);
        assert!(fallback_extent(&mut Cursor::new(&image), size, 512, esp, "0").is_err());
        for patch in &patches {
            image[patch.offset as usize..patch.offset as usize + patch.after.len()]
                .copy_from_slice(&patch.after);
        }
        assert!(
            fallback_extent(&mut Cursor::new(&image), size, 512, esp, &digest)
                .unwrap()
                .is_empty()
        );
    }
}

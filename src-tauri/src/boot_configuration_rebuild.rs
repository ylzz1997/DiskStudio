//! Generate a clean offline BCD for one installed Windows UEFI boot entry.
use super::{
    boolean, clean_hive, key, parse_hive, repair::gpt_bytes, seconds, text, Device, Setting,
    BOOT_MANAGER, MAX_OBJECTS,
};
use regf::{DataType, HiveBuilder, KeyTreeNode, KeyTreeValue, RegistryHive, BASE_BLOCK_SIZE};
use serde::Serialize;
use uuid::Uuid;

const MANAGER_TYPE: u32 = 0x1010_0002;
const LOADER_TYPE: u32 = 0x1020_0003;
const MANAGER_PATH: &str = r"\EFI\Microsoft\Boot\bootmgfw.efi";
const LOADER_PATH: &str = r"\Windows\System32\winload.efi";
const SYSTEM_ROOT: &str = r"\Windows";

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub loader: Uuid,
    pub locale: String,
    manager: Device,
    windows: Device,
    timeout_seconds: String,
}

fn error(e: regf::Error) -> String {
    e.to_string()
}

struct Template {
    loader: Uuid,
    locale: String,
    timeout: u32,
    stamp: u64,
    isolated_context: bool,
}

fn template(bytes: Vec<u8>) -> Result<Template, String> {
    let hive = clean_hive(bytes)?;
    let objects = hive.open_key("Objects").map_err(error)?;
    if objects.subkey_count() > MAX_OBJECTS {
        return Err("The Windows BCD template has too many objects".into());
    }
    let manager = objects
        .open_subkey(&format!("{{{BOOT_MANAGER}}}"))
        .map_err(error)?;
    let manager_type = manager
        .open_subkey("Description")
        .and_then(|description| description.value("Type")?.dword_data())
        .map_err(error)?;
    let settings = manager.open_subkey("Elements").map_err(error)?;
    if manager_type != MANAGER_TYPE
        || !text(Some(&settings), Setting::ApplicationPath.code())?
            .is_some_and(|path| path.eq_ignore_ascii_case(MANAGER_PATH))
    {
        return Err("The Windows BCD template has no standard UEFI boot manager".into());
    }
    let timeout = seconds(Some(&settings), Setting::Timeout.code())?
        .unwrap_or_else(|| "30".into())
        .parse::<u32>()
        .ok()
        .filter(|seconds| *seconds <= 300)
        .ok_or("The Windows BCD template has an unsupported boot timeout")?;

    let mut loader = None;
    let mut seen = std::collections::HashSet::new();
    for object in objects.subkeys().map_err(error)? {
        let id = Uuid::parse_str(&object.name()).map_err(|err| err.to_string())?;
        if id.is_nil() || !seen.insert(id) {
            return Err("The Windows BCD template has duplicate or empty identifiers".into());
        }
        let kind = object
            .open_subkey("Description")
            .and_then(|description| description.value("Type")?.dword_data())
            .map_err(error)?;
        if kind != LOADER_TYPE {
            continue;
        }
        let Some(elements) = key(&object, "Elements")? else {
            continue;
        };
        let fields = Some(&elements);
        // Setup and recovery templates describe RAM disks or enable WinPE.
        // A rebuild for an installed OS must never select one of those.
        if key(&elements, "42000003")?.is_some()
            || boolean(fields, "26000022")? == Some(true)
            || !text(fields, Setting::SystemRoot.code())?
                .is_some_and(|root| root.eq_ignore_ascii_case(SYSTEM_ROOT))
        {
            continue;
        }
        let relative = text(fields, "42000002")?;
        let absolute = text(fields, Setting::ApplicationPath.code())?;
        if (relative.is_none() && absolute.is_none())
            || relative.is_some_and(|path| !path.eq_ignore_ascii_case(r"\system32\winload.efi"))
            || absolute.is_some_and(|path| !path.eq_ignore_ascii_case(LOADER_PATH))
        {
            continue;
        }
        let locale = text(fields, Setting::Locale.code())?
            .or(text(Some(&settings), Setting::Locale.code())?)
            .unwrap_or_else(|| "en-US".into());
        if !(2..=35).contains(&locale.len())
            || !locale
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|ch| ch.is_ascii_alphanumeric()))
        {
            return Err("The Windows BCD template has an unsupported boot locale".into());
        }
        let candidate = Template {
            loader: id,
            locale,
            timeout,
            stamp: hive.base_block().last_written,
            isolated_context: boolean(fields, "16000060")? == Some(true),
        };
        if loader.replace(candidate).is_some() {
            return Err(
                "The Windows BCD template has several installed UEFI loader profiles".into(),
            );
        }
    }
    loader.ok_or_else(|| "The Windows BCD template has no installed UEFI loader profile".into())
}

fn add(node: &mut KeyTreeNode, name: &str, data_type: DataType, data: Vec<u8>) {
    node.values.push(KeyTreeValue {
        name: name.into(),
        data_type,
        data,
    });
}

fn utf16(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn object(id: Uuid, kind: u32) -> KeyTreeNode {
    let mut object = KeyTreeNode::new(&format!("{{{id}}}"));
    let description = object.get_or_create_path("Description");
    add(
        description,
        "Type",
        DataType::Dword,
        kind.to_le_bytes().to_vec(),
    );
    object
}

fn field(object: &mut KeyTreeNode, code: &str, ty: DataType, value: Vec<u8>) {
    let fields = object.get_or_create_path("Elements");
    add(fields.get_or_create_path(code), "Element", ty, value);
}

fn string(object: &mut KeyTreeNode, setting: Setting, value: &str) {
    field(object, setting.code(), DataType::String, utf16(value));
}

/// Self-relative SD with built-in Administrators as owner and group and a
/// DACL granting KEY_ALL_ACCESS to SYSTEM and Administrators.
pub(super) fn hive_security() -> Vec<u8> {
    const SYSTEM: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    const ADMIN: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0];
    let mut bytes = vec![1, 0, 4, 128];
    for offset in [72u32, 88, 0, 20] {
        bytes.extend(offset.to_le_bytes());
    }
    bytes.extend([2, 0, 52, 0, 2, 0, 0, 0]);
    for sid in [SYSTEM.as_slice(), &ADMIN] {
        bytes.extend([0, 0]);
        bytes.extend(((8 + sid.len()) as u16).to_le_bytes());
        bytes.extend(0x000f_003fu32.to_le_bytes());
        bytes.extend(sid);
    }
    bytes.extend(ADMIN);
    bytes.extend(ADMIN);
    bytes
}

/// Retain the template timestamp so that each independent validation of the
/// same source produces identical bytes, including the registry header.
pub(super) fn normalize_timestamps(mut bytes: Vec<u8>, stamp: u64) -> Result<Vec<u8>, String> {
    let hive = RegistryHive::from_bytes(bytes.clone()).map_err(error)?;
    let mut base = hive.base_block().clone();
    let mut pending = vec![hive.root_key().map_err(error)?];
    while let Some(key) = pending.pop() {
        // Offset is relative to the hive bins and includes the cell size word.
        let offset = BASE_BLOCK_SIZE + key.offset() as usize + 8;
        bytes[offset..offset + 8].copy_from_slice(&stamp.to_le_bytes());
        pending.extend(key.subkeys().map_err(error)?);
    }
    base.last_written = stamp;
    base.set_file_name(r"\EFI\Microsoft\Boot\BCD");
    base.write(&mut &mut bytes[..BASE_BLOCK_SIZE])
        .map_err(|err| err.to_string())?;
    let first_bin_time = BASE_BLOCK_SIZE + 20;
    bytes[first_bin_time..first_bin_time + 8].copy_from_slice(&stamp.to_le_bytes());
    Ok(bytes)
}

pub(crate) fn generate(
    source: Vec<u8>,
    disk: Uuid,
    boot: Uuid,
    windows: Uuid,
) -> Result<(Vec<u8>, Entry), String> {
    if disk.is_nil() || boot.is_nil() || windows.is_nil() || boot == windows {
        return Err("The disk and partition identities for the new BCD are invalid".into());
    }
    let template = template(source)?;
    let id = format!("{{{}}}", template.loader);
    let mut manager = object(BOOT_MANAGER, MANAGER_TYPE);
    field(
        &mut manager,
        Setting::ApplicationDevice.code(),
        DataType::Binary,
        gpt_bytes(disk, boot),
    );
    string(&mut manager, Setting::ApplicationPath, MANAGER_PATH);
    string(&mut manager, Setting::Description, "Windows Boot Manager");
    string(&mut manager, Setting::Locale, &template.locale);
    string(&mut manager, Setting::Default, &id);
    field(
        &mut manager,
        Setting::DisplayOrder.code(),
        DataType::MultiString,
        utf16(&format!("{id}\0")),
    );
    field(
        &mut manager,
        Setting::Timeout.code(),
        DataType::Binary,
        u64::from(template.timeout).to_le_bytes().to_vec(),
    );

    let mut loader = object(template.loader, LOADER_TYPE);
    for setting in [Setting::ApplicationDevice, Setting::SystemDevice] {
        field(
            &mut loader,
            setting.code(),
            DataType::Binary,
            gpt_bytes(disk, windows),
        );
    }
    string(&mut loader, Setting::ApplicationPath, LOADER_PATH);
    string(&mut loader, Setting::SystemRoot, SYSTEM_ROOT);
    string(&mut loader, Setting::Description, "Windows");
    string(&mut loader, Setting::Locale, &template.locale);
    if template.isolated_context {
        field(&mut loader, "16000060", DataType::Binary, vec![1]);
    }

    let mut root = KeyTreeNode::new("System");
    // Windows opens this hive as the system store after the loader starts.
    // BCDBoot marks its installed store here; the installation template need not.
    add(
        root.get_or_create_path("Description"),
        "System",
        DataType::Dword,
        1u32.to_le_bytes().to_vec(),
    );
    root.get_or_create_path("Objects")
        .children
        .extend([manager, loader]);
    let mut hive = HiveBuilder::from_tree_with_version_and_sd(root, 1, 3, Some(hive_security()))
        .map_err(error)?;
    let bytes = normalize_timestamps(hive.build().map_err(error)?, template.stamp)?;
    let reference = Entry {
        loader: template.loader,
        locale: template.locale,
        manager: Device::Gpt {
            disk,
            partition: boot,
        },
        windows: Device::Gpt {
            disk,
            partition: windows,
        },
        timeout_seconds: template.timeout.to_string(),
    };
    reference.verify(&bytes)?;
    Ok((bytes, reference))
}

impl Entry {
    pub(crate) fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        let hive = clean_hive(bytes.to_vec())?;
        let system = hive
            .open_key("Description")
            .and_then(|description| description.value("System"))
            .is_ok_and(|marker| {
                marker.data_type() == DataType::Dword && matches!(marker.dword_data(), Ok(1))
            });
        if !system {
            return Err("The new BCD is not marked as a Windows system store".into());
        }
        let store = parse_hive(&hive)?;
        let valid_manager = store.manager.is_some_and(|manager| {
            manager.object_type == MANAGER_TYPE
                && manager.device == Some(self.manager)
                && manager.path.as_deref() == Some(MANAGER_PATH)
                && manager.locale.as_deref() == Some(self.locale.as_str())
                && manager.setting_issues.is_empty()
        });
        let valid_loader = store.entries.len() == 1 && {
            let loader = &store.entries[0];
            loader.id == self.loader
                && loader.object_type == LOADER_TYPE
                && loader.device == Some(self.windows)
                && loader.os_device == Some(self.windows)
                && loader.path.as_deref() == Some(LOADER_PATH)
                && loader.system_root.as_deref() == Some(SYSTEM_ROOT)
                && loader.locale.as_deref() == Some(self.locale.as_str())
                && loader.setting_issues.is_empty()
        };
        if !valid_manager
            || !valid_loader
            || store.default != Some(self.loader)
            || store.display_order != [self.loader]
            || store.timeout_seconds.as_ref() != Some(&self.timeout_seconds)
            || !store.unresolved_objects.is_empty()
        {
            return Err(
                "The new BCD does not match the selected Windows and EFI partitions".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_template(profiles: &[(Uuid, bool)]) -> Vec<u8> {
        let mut root = KeyTreeNode::new("System");
        let objects = root.get_or_create_path("Objects");
        let mut manager = object(BOOT_MANAGER, MANAGER_TYPE);
        string(&mut manager, Setting::ApplicationPath, MANAGER_PATH);
        field(
            &mut manager,
            Setting::Timeout.code(),
            DataType::Binary,
            vec![7],
        );
        objects.children.push(manager);
        for &(id, winpe) in profiles {
            let mut loader = object(id, LOADER_TYPE);
            string(&mut loader, Setting::SystemRoot, r"\windows");
            string(&mut loader, Setting::Locale, "zh-CN");
            field(
                &mut loader,
                "42000002",
                DataType::String,
                utf16(r"\system32\winload.efi"),
            );
            if winpe {
                field(&mut loader, "26000022", DataType::Binary, vec![1]);
            }
            objects.children.push(loader);
        }
        HiveBuilder::from_tree(root).unwrap().build().unwrap()
    }

    #[test]
    fn new_store_is_reproducible_and_matches_both_partitions() {
        let id = Uuid::from_u128(11);
        let bytes = build_template(&[(id, false), (Uuid::from_u128(12), true)]);
        let disk = Uuid::from_u128(21);
        let esp = Uuid::from_u128(22);
        let os = Uuid::from_u128(23);
        let (first, entry) = generate(bytes.clone(), disk, esp, os).unwrap();
        let (second, _) = generate(bytes.clone(), disk, esp, os).unwrap();
        let (wrong_partition, _) = generate(bytes, disk, esp, Uuid::from_u128(24)).unwrap();
        assert_eq!(first, second);
        assert_eq!(entry.loader, id);
        assert_eq!(entry.locale, "zh-CN");
        assert_eq!(entry.timeout_seconds, "7");
        assert!(entry.verify(&wrong_partition).is_err());

        let hive = clean_hive(first.clone()).unwrap();
        let description = hive.open_key("Description").unwrap();
        let system = description.value("System").unwrap();
        assert_eq!(system.data_type(), DataType::Dword);
        assert_eq!(system.dword_data().unwrap(), 1);
        assert!(system.raw_value().is_data_resident());
        // The four-byte cell header precedes vk, whose inline value is at +8.
        let data = BASE_BLOCK_SIZE + system.offset() as usize + 4 + 8;
        let mut non_system = first;
        non_system[data..data + 4].fill(0);
        assert!(entry
            .verify(&non_system)
            .unwrap_err()
            .contains("system store"));
    }

    #[test]
    fn rejects_ambiguous_or_dirty_templates_and_winpe_only_sources() {
        let disk = Uuid::from_u128(21);
        let esp = Uuid::from_u128(22);
        let os = Uuid::from_u128(23);
        let first = Uuid::from_u128(11);
        let second = Uuid::from_u128(12);
        assert!(generate(
            build_template(&[(first, false), (second, false)]),
            disk,
            esp,
            os
        )
        .is_err());
        assert!(generate(build_template(&[(first, true)]), disk, esp, os).is_err());
        let mut bytes = build_template(&[(first, false)]);
        bytes[4] ^= 1;
        assert!(generate(bytes, disk, esp, os).is_err());
    }
}

//! Replace existing direct GPT references in a clean BCD hive, in memory.
use super::{decode_device, parse, Device, Setting, APPLICATION_TYPE, BOOT_MANAGER, OS_LOADER};
use regf::{hive::RegistryValueEntry, parser::HiveParser, DataType, RegistryHive, BASE_BLOCK_SIZE};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::Cursor,
    ops::Range,
};
use uuid::Uuid;

const MAX_KEYS: usize = 8192;
const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct Repaired {
    pub bytes: Vec<u8>,
    pub application_before: Device,
    pub system_before: Device,
    pub application_after: Device,
    pub default_before: Option<Uuid>,
    pub default_after: Option<Uuid>,
}

struct Patch {
    cell: u32,
    range: Range<usize>,
    bytes: Vec<u8>,
}

fn exact_patch(
    value: &RegistryValueEntry<'_>,
    original: &[u8],
    layout: &HiveParser<Cursor<&[u8]>>,
    ty: DataType,
    replacement: Vec<u8>,
) -> Result<Patch, String> {
    let existing = value.raw_data().map_err(|e| e.to_string())?;
    let record = value.raw_value();
    if value.data_type() != ty
        || record.is_data_resident()
        || existing.len() != replacement.len()
        || replacement.len() > 512
    {
        return Err(
            "BCD repair requires an existing direct value with the expected type and size".into(),
        );
    }
    let cell = layout
        .read_cell(record.data_offset)
        .map_err(|e| e.to_string())?;
    if !cell.is_allocated() || !cell.data.starts_with(&existing) {
        return Err("BCD value data does not belong to an allocated contiguous hive cell".into());
    }
    let start = BASE_BLOCK_SIZE + record.data_offset as usize + 4;
    let range = start..start + replacement.len();
    if original.get(range.clone()) != Some(&existing) {
        return Err("BCD hive cell offset and value bytes disagree".into());
    }
    Ok(Patch {
        cell: value.offset(),
        range,
        bytes: replacement,
    })
}

fn gpt_bytes(disk: Uuid, partition: Uuid) -> Vec<u8> {
    let mut bytes = vec![0; 88];
    bytes[16..20].copy_from_slice(&6u32.to_le_bytes());
    bytes[24..28].copy_from_slice(&72u32.to_le_bytes());
    bytes[32..48].copy_from_slice(&partition.to_bytes_le());
    bytes[56..72].copy_from_slice(&disk.to_bytes_le());
    bytes
}

#[derive(PartialEq, Eq)]
struct KeyContent {
    name: String,
    class: Option<String>,
    parent: Option<u32>,
    values: BTreeMap<u32, (String, u32, [u8; 32])>,
}

fn unrelated_content(
    hive: &RegistryHive,
    edited: &HashSet<u32>,
) -> Result<BTreeMap<u32, KeyContent>, String> {
    let err = |e: regf::Error| e.to_string();
    let mut remaining = vec![(hive.root_key().map_err(err)?, None)];
    let mut keys = BTreeMap::new();
    let mut seen_values = HashSet::new();
    let mut value_bytes = 0usize;
    while let Some((key, parent)) = remaining.pop() {
        if keys.len() + remaining.len() >= MAX_KEYS || keys.contains_key(&key.offset()) {
            return Err("BCD hive has shared keys, a cycle, or too many keys".into());
        }
        let mut values = BTreeMap::new();
        let mut names = HashSet::new();
        for value in key.values().map_err(err)? {
            let name = value.name();
            if !names.insert(name.to_uppercase()) || !seen_values.insert(value.offset()) {
                return Err("BCD registry value identifiers are not unique".into());
            }
            value_bytes = value_bytes
                .checked_add(value.data_size() as usize)
                .filter(|size| *size <= MAX_VALUE_BYTES)
                .ok_or("BCD value inspection budget exceeded")?;
            let raw = value.raw_data().map_err(err)?;
            let digest = if edited.contains(&value.offset()) {
                [0; 32]
            } else {
                Sha256::digest(raw).into()
            };
            values.insert(value.offset(), (name, value.raw_data_type(), digest));
        }
        keys.insert(
            key.offset(),
            KeyContent {
                name: key.name(),
                class: key.class_name().map_err(err)?,
                parent,
                values,
            },
        );
        let mut names = HashSet::new();
        for child in key.subkeys().map_err(err)? {
            if !names.insert(child.name().to_uppercase()) {
                return Err("BCD registry key names are not unique".into());
            }
            remaining.push((child, Some(key.offset())));
        }
    }
    Ok(keys)
}

pub(crate) fn rebind(
    original: &[u8],
    entry: Uuid,
    disk: Uuid,
    partition: Uuid,
    make_default: bool,
) -> Result<Repaired, String> {
    if disk.is_nil() || partition.is_nil() {
        return Err("BCD repair target GUID is missing".into());
    }
    let before = parse(original.to_vec())?;
    let old = before
        .entries
        .iter()
        .find(|old| old.id == entry && old.object_type & APPLICATION_TYPE == OS_LOADER)
        .ok_or("The selected Windows loader no longer exists")?;
    let err = |e: regf::Error| e.to_string();
    let hive = RegistryHive::from_bytes(original.to_vec()).map_err(err)?;
    let layout = HiveParser::new(Cursor::new(original)).map_err(err)?;
    let objects = hive.open_key("Objects").map_err(err)?;
    let elements = objects
        .open_subkey(&format!("{{{entry}}}"))
        .and_then(|object| object.open_subkey("Elements"))
        .map_err(err)?;
    let mut patches = Vec::new();
    let mut old_devices = Vec::new();
    for setting in [Setting::ApplicationDevice, Setting::SystemDevice] {
        let value = elements
            .open_subkey(setting.code())
            .and_then(|key| key.value("Element"))
            .map_err(err)?;
        let previous = decode_device(&value.raw_data().map_err(err)?);
        if !matches!(previous, Device::Gpt { .. }) {
            return Err(
                "BCD repair requires direct qualified GPT references for both devices".into(),
            );
        }
        old_devices.push(previous);
        patches.push(exact_patch(
            &value,
            original,
            &layout,
            DataType::Binary,
            gpt_bytes(disk, partition),
        )?);
    }
    if make_default && before.default != Some(entry) {
        if !before.display_order.contains(&entry) {
            return Err("The replacement default must already appear in the BCD menu".into());
        }
        let manager = objects
            .open_subkey(&format!("{{{BOOT_MANAGER}}}"))
            .and_then(|object| object.open_subkey("Elements"))
            .and_then(|elements| elements.open_subkey(Setting::Default.code()))
            .and_then(|key| key.value("Element"))
            .map_err(err)?;
        let replacement = format!("{{{entry}}}\0")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        patches.push(exact_patch(
            &manager,
            original,
            &layout,
            DataType::String,
            replacement,
        )?);
    }
    patches.sort_by_key(|patch| patch.range.start);
    if patches
        .windows(2)
        .any(|pair| pair[0].range.end > pair[1].range.start)
    {
        return Err("BCD repair fields share the same storage".into());
    }
    let cells = patches
        .iter()
        .map(|patch| patch.cell)
        .collect::<HashSet<_>>();
    let expected = unrelated_content(&hive, &cells)?;
    let mut bytes = original.to_vec();
    for patch in patches {
        bytes[patch.range].copy_from_slice(&patch.bytes);
    }
    if bytes == original {
        return Err("The specified BCD references already match the proposed repair".into());
    }
    let edited = RegistryHive::from_bytes(bytes.clone()).map_err(err)?;
    if unrelated_content(&edited, &cells)? != expected {
        return Err("BCD repair changed unrelated keys or values".into());
    }
    let after = parse(bytes.clone())?;
    let now = after
        .entries
        .iter()
        .find(|now| now.id == entry)
        .ok_or("BCD loader missing after repair")?;
    let target = Device::Gpt { disk, partition };
    let default_after = if make_default {
        Some(entry)
    } else {
        before.default
    };
    if now.object_type != old.object_type
        || now.device != Some(target)
        || now.os_device != Some(target)
        || now.path != old.path
        || now.system_root != old.system_root
        || after.default != default_after
    {
        return Err("The resulting BCD did not verify against the proposed repair".into());
    }
    Ok(Repaired {
        bytes,
        application_before: old_devices[0],
        system_before: old_devices[1],
        application_after: target,
        default_before: before.default,
        default_after,
    })
}

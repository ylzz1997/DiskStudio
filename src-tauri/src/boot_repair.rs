//! Plan bounded, in-place repair of existing Windows UEFI GPT references.
use super::{
    boot_configuration, check_path, image_browser, Device, Extent, Mode, EFI_BCD, EFI_MANAGER,
};
use crate::{
    backup_archive::Source, data_io::Region, fat_recovery, sector_edit, surface_scan::Range,
    used_space::Span,
};
use disk_core::table_check::{GptIdentity, GptPartitionIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
};
use uuid::Uuid;

const EFI: Uuid = Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b);
const BASIC: Uuid = Uuid::from_u128(0xebd0a0a2_b9e5_4433_87c0_68b6b72699c7);

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Change {
    pub boot_partition: Uuid,
    pub store_sha256: String,
    pub entry: Uuid,
    pub windows_partition: Uuid,
    pub make_default: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Target {
    id: Uuid,
    name: String,
    offset: u64,
    size: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub boot_offset: u64,
    pub windows_offset: u64,
    pub application_before: Device,
    pub system_before: Device,
    pub application_after: Device,
    pub default_before: Option<Uuid>,
    pub default_after: Option<Uuid>,
    pub before_sha256: String,
    pub after_sha256: String,
    pub loader_sha256: String,
    pub blocks: usize,
}

struct Block {
    range: Range,
    before: Vec<u8>,
    after: Vec<u8>,
}

pub(crate) struct Plan {
    pub summary: Summary,
    pub fingerprint: [u8; 32],
    pub original: Vec<u8>,
    pub layout: GptIdentity,
    boot: Extent,
    geometry: Geometry,
    ownership: [u8; 32],
    blocks: Vec<Block>,
}

#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub size: u64,
    pub sector: u64,
    pub physical: u64,
    pub alignment: u64,
}

fn eligible(partition: &GptPartitionIdentity, kind: Uuid) -> bool {
    // Reject required, explicitly read-only and shadow-copy GPT partitions.
    let protected = 1 | (1 << 60) | (1 << 61);
    partition.kind == kind.to_bytes_le()
        && partition.attributes & protected == 0
        && (kind == EFI || partition.attributes & (1 << 62) == 0)
}

pub(crate) fn targets(identity: &GptIdentity, parts: &[disk_core::Partition]) -> Vec<Target> {
    identity
        .partitions
        .iter()
        .filter(|part| eligible(part, BASIC))
        .filter_map(|part| {
            let line = parts.iter().find(|p| {
                p.start == part.range.start && p.size == part.range.end - part.range.start
            })?;
            Some(Target {
                id: Uuid::from_bytes_le(part.guid),
                name: line.name.clone(),
                offset: line.start,
                size: line.size,
            })
        })
        .collect()
}

pub(crate) fn boot_partition(identity: &GptIdentity, offset: u64) -> Option<Uuid> {
    identity
        .partitions
        .iter()
        .find(|part| part.range.start == offset && eligible(part, EFI))
        .map(|part| Uuid::from_bytes_le(part.guid))
}

fn filesystem(
    f: &mut impl Source,
    g: Geometry,
    extent: Extent,
) -> Result<impl Source + '_, String> {
    if extent
        .start
        .checked_add(extent.size)
        .is_none_or(|end| end > g.size)
    {
        return Err("EFI filesystem extent is outside the disk".into());
    }
    Region::for_file_view(f, extent.start, extent.size).map_err(|e| e.to_string())
}

fn gpt(source: &mut impl Source, geometry: Geometry) -> Result<GptIdentity, String> {
    let mut reader = Region::for_metadata(source, 0, geometry.size).map_err(|e| e.to_string())?;
    image_browser::checked_layout(
        &mut reader,
        "boot-repair.img",
        geometry.size,
        geometry.sector,
        &mut || Ok(()),
    )?
    .gpt_identity
    .ok_or_else(|| "Boot repair requires matching primary and backup GPT tables".into())
}

fn read_block(f: &mut (impl Read + Seek), range: Range) -> Result<Vec<u8>, String> {
    let mut data = vec![0; range.size as usize];
    f.seek(SeekFrom::Start(range.offset))
        .and_then(|_| f.read_exact(&mut data))
        .map_err(|e| e.to_string())?;
    Ok(data)
}

fn changed_blocks(
    source: &mut impl Source,
    geometry: Geometry,
    extent: Extent,
    mapped: &fat_recovery::boot_files::MappedFile,
    updated: &[u8],
) -> Result<Vec<Block>, String> {
    if mapped.bytes.len() != updated.len()
        || mapped.cluster_size == 0
        || mapped.bytes.len().div_ceil(mapped.cluster_size) != mapped.clusters.len()
    {
        return Err("BCD patch cannot resize the file".into());
    }
    let scope = Range {
        offset: extent.start,
        size: extent.size,
        sector: geometry.sector,
    };
    let mut blocks = BTreeMap::new();
    for (index, (&before, &after)) in mapped.bytes.iter().zip(updated).enumerate() {
        if before == after {
            continue;
        }
        let cluster = mapped.clusters[index / mapped.cluster_size];
        let offset = extent.start + cluster + (index % mapped.cluster_size) as u64;
        let page = Range {
            offset: offset / geometry.sector * geometry.sector,
            size: geometry.sector,
            sector: geometry.sector,
        };
        let range =
            sector_edit::physical_block(page, scope, geometry.physical, geometry.alignment)?;
        let block = match blocks.entry(range.offset) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let bytes = read_block(source, range)?;
                entry.insert(Block {
                    range,
                    before: bytes.clone(),
                    after: bytes,
                })
            }
        };
        let at = (offset - range.offset) as usize;
        if block.before[at] != before {
            return Err("BCD bytes changed while the repair was being planned".into());
        }
        block.after[at] = after;
    }
    if blocks.is_empty() {
        return Err("No BCD values need to change".into());
    }
    Ok(blocks.into_values().collect())
}

#[cfg(test)]
#[path = "boot_repair_tests.rs"]
mod tests;

pub(crate) fn plan(
    mut source: impl Source,
    geometry: Geometry,
    expected_ranges: &[Span],
    change: &Change,
) -> Result<Plan, String> {
    if !matches!(geometry.sector, 512 | 4096)
        || geometry.physical < geometry.sector
        || change.boot_partition == change.windows_partition
    {
        return Err(
            "Repair requires separate EFI and Windows partitions with checked sector geometry"
                .into(),
        );
    }
    let layout = gpt(&mut source, geometry)?;
    let mut ordered = layout
        .partitions
        .iter()
        .map(|p| (p.range.start, p.range.end))
        .collect::<Vec<_>>();
    ordered.sort_unstable();
    if ordered.len() != expected_ranges.len()
        || !ordered
            .iter()
            .zip(expected_ranges)
            .all(|(&(start, end), span)| start == span.start && end == span.end)
    {
        return Err("EFI repair layout does not match the verified native partitions".into());
    }
    let extent = |id: Uuid, kind: Uuid| -> Result<Extent, String> {
        let part = layout
            .partitions
            .iter()
            .find(|part| part.guid == id.to_bytes_le() && eligible(part, kind))
            .ok_or("The selected GPT partition is absent or protected")?;
        Ok(Extent {
            start: part.range.start,
            size: part.range.end - part.range.start,
        })
    };
    let boot = extent(change.boot_partition, EFI)?;
    let windows = extent(change.windows_partition, BASIC)?;
    let (ownership, mapped) = {
        let mut view = filesystem(&mut source, geometry, boot)?;
        let ownership = fat_recovery::resize::check_boot_fat32(&mut view, geometry.sector)?;
        let volume = fat_recovery::boot_files::Volume::open(&mut view)?;
        (
            ownership,
            volume.mapped_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)?,
        )
    };
    let before_sha256 = sector_edit::hash(&mapped.bytes);
    if before_sha256 != change.store_sha256 {
        return Err(
            "BCD changed since inspection. Inspect the disk again before preparing a repair".into(),
        );
    }
    let store = boot_configuration::parse(mapped.bytes.clone())?;
    let entry = store
        .entries
        .iter()
        .find(|entry| entry.id == change.entry)
        .ok_or("Selected BCD entry is missing")?;
    let (Some(path), Some(root)) = (entry.path.as_deref(), entry.system_root.as_deref()) else {
        return Err("The selected BCD entry lacks a verified loader path or system root".into());
    };
    if !path.eq_ignore_ascii_case(&format!("{root}\\System32\\winload.efi")) {
        return Err("The selected entry does not use a standard Windows UEFI loader path".into());
    }
    let manager = check_path(
        &mut source,
        boot,
        geometry.sector,
        &format!("\\{}", EFI_MANAGER.join("\\")),
        Some(Mode::Uefi),
        false,
    );
    let loader = check_path(
        &mut source,
        windows,
        geometry.sector,
        path,
        Some(Mode::Uefi),
        false,
    );
    let directory = check_path(&mut source, windows, geometry.sector, root, None, true);
    if directory.state != "present"
        || loader.state != "present"
        || manager.machine.is_none()
        || manager.machine != loader.machine
    {
        return Err(
            "Windows loader, system directory, or matching EFI boot manager could not be verified"
                .into(),
        );
    }
    let loader_sha256 = loader
        .sha256
        .ok_or("Missing verified Windows loader digest")?;
    let corrected = boot_configuration::repair::rebind(
        &mapped.bytes,
        change.entry,
        Uuid::from_bytes_le(layout.disk_guid),
        change.windows_partition,
        change.make_default,
    )?;
    let after_sha256 = sector_edit::hash(&corrected.bytes);
    let blocks = changed_blocks(&mut source, geometry, boot, &mapped, &corrected.bytes)?;
    let mut witness = Sha256::new();
    witness.update(layout.disk_guid);
    for part in &layout.partitions {
        witness.update(part.guid);
        witness.update(part.kind);
        witness.update(part.attributes.to_le_bytes());
        witness.update(part.range.start.to_le_bytes());
        witness.update(part.range.end.to_le_bytes());
    }
    witness.update(ownership);
    witness.update(before_sha256.as_bytes());
    witness.update(after_sha256.as_bytes());
    witness.update(loader_sha256.as_bytes());
    witness.update(
        manager
            .sha256
            .ok_or("Missing EFI boot manager digest")?
            .as_bytes(),
    );
    for block in &blocks {
        witness.update(block.range.offset.to_le_bytes());
        witness.update((block.before.len() as u64).to_le_bytes());
        witness.update(&block.before);
        witness.update(&block.after);
    }
    Ok(Plan {
        summary: Summary {
            boot_offset: boot.start,
            windows_offset: windows.start,
            application_before: corrected.application_before,
            system_before: corrected.system_before,
            application_after: corrected.application_after,
            default_before: corrected.default_before,
            default_after: corrected.default_after,
            before_sha256,
            after_sha256,
            loader_sha256,
            blocks: blocks.len(),
        },
        fingerprint: witness.finalize().into(),
        original: mapped.bytes,
        layout,
        boot,
        geometry,
        ownership,
        blocks,
    })
}

impl Plan {
    pub(crate) fn write<R: Source + Write>(
        &self,
        writer: &mut R,
        before_write: impl FnOnce() -> Result<(), String>,
        mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
    ) -> Result<(), String> {
        for block in &self.blocks {
            if read_block(writer, block.range)? != block.before {
                return Err(
                    "BCD blocks changed since the confirmed preview. Nothing was written".into(),
                );
            }
        }
        before_write()?;
        for (i, block) in self.blocks.iter().enumerate() {
            sector_edit::write_once(writer, block.range, &block.before, &block.after, &mut sync)
                .map_err(|e| format!("BCD repair stopped after {i} verified blocks. The next block may be partially written. {e}"))?;
        }
        let mut view = filesystem(writer, self.geometry, self.boot)?;
        let ownership = fat_recovery::resize::check_boot_fat32(&mut view, self.geometry.sector)?;
        let volume = fat_recovery::boot_files::Volume::open(&mut view)?;
        let result = volume.mapped_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)?;
        if ownership != self.ownership
            || sector_edit::hash(&result.bytes) != self.summary.after_sha256
        {
            return Err(
                "BCD was written but full-file or FAT ownership readback did not match".into(),
            );
        }
        boot_configuration::parse(result.bytes)?;
        drop(view);
        if gpt(writer, self.geometry)? != self.layout {
            return Err("The GPT identity or layout changed during BCD repair".into());
        }
        Ok(())
    }
}

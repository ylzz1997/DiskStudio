//! Rebuild the Windows EFI manager and optional architecture fallback from
//! the selected offline disk's Windows installation.
use super::{
    filesystem, gpt, read_block, repair_extents, witness_block, witness_layout, Block, Geometry,
};
use crate::{
    backup_archive::Source,
    boot_inspection::{
        boot_configuration, open_extent, pe_machine, Extent, Filesystem, EFI_BCD, MAX_EXECUTABLE,
    },
    fat_recovery::boot_files::{rebuild, Volume},
    sector_edit,
    surface_scan::Range,
    used_space::Span,
};
use disk_core::table_check::GptIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};
use uuid::Uuid;

const INSTALLED_MANAGER: &[&str] = &["Windows", "Boot", "EFI", "bootmgfw.efi"];
const INSTALLED_LOADER: &[&str] = &["Windows", "System32", "winload.efi"];
const INSTALLED_TEMPLATE: &[&str] = &["Windows", "System32", "Config", "BCD-Template"];

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Change {
    pub boot_partition: Uuid,
    pub windows_partition: Uuid,
    pub rebuild_fallback: bool,
    #[serde(default)]
    pub rebuild_missing_bcd: bool,
    pub damaged_bcd: Option<DamagedBcd>,
}

#[derive(Clone, Deserialize)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum DamagedBcd {
    Recover { original_sha256: String },
    Replace { original_sha256: String },
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveredBcd {
    #[serde(flatten)]
    replay: boot_configuration::recovery::Replay,
    retired_logs: Vec<RetiredLog>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileSummary {
    pub path: String,
    pub bytes: usize,
    pub original_sha256: Option<String>,
    pub installed_sha256: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewBcd {
    #[serde(flatten)]
    entry: boot_configuration::rebuild::Entry,
    template_sha256: String,
    retired_logs: Vec<RetiredLog>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetiredLog {
    path: String,
    bytes: usize,
    sha256: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub boot_offset: u64,
    pub windows_offset: u64,
    pub machine: u16,
    pub loader_sha256: String,
    pub files: Vec<FileSummary>,
    pub physical_writes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_bcd: Option<NewBcd>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_bcd: Option<RecoveredBcd>,
}

pub(crate) struct Plan {
    pub summary: Summary,
    pub fingerprint: [u8; 32],
    pub layout: GptIdentity,
    pub originals: Vec<rebuild::File>,
    pub retired: Vec<rebuild::Retired>,
    phases: Vec<(rebuild::Phase, Vec<Block>)>,
    bcd_sha256: Option<String>,
    manager_sha256: String,
    boot: Extent,
    geometry: Geometry,
}

fn file_path(target: rebuild::Target) -> String {
    format!("\\{}", target.path().join("\\"))
}

/// Coalesce neighboring changes within each phase and carry the expected
/// sector contents forward. The first edit to a block fixes its write order.
fn physical_blocks(
    source: &mut impl Source,
    geometry: Geometry,
    boot: Extent,
    relative: Vec<(rebuild::Phase, Vec<rebuild::Edit>)>,
) -> Result<Vec<(rebuild::Phase, Vec<Block>)>, String> {
    let scope = Range {
        offset: boot.start,
        size: boot.size,
        sector: geometry.sector,
    };
    let mut latest = BTreeMap::<u64, Vec<u8>>::new();
    let mut result = Vec::with_capacity(relative.len());
    for (phase, edits) in relative {
        let mut indexes = BTreeMap::new();
        let mut blocks: Vec<Block> = Vec::new();
        for edit in edits {
            let mut offset = boot
                .start
                .checked_add(edit.offset)
                .ok_or("EFI write offset overflow")?;
            let mut data = edit.bytes.as_slice();
            while !data.is_empty() {
                let logical = Range {
                    offset: offset / geometry.sector * geometry.sector,
                    size: geometry.sector,
                    sector: geometry.sector,
                };
                let block = sector_edit::physical_block(
                    logical,
                    scope,
                    geometry.physical,
                    geometry.alignment,
                )?;
                let index = match indexes.entry(block.offset) {
                    std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let before = match latest.get(&block.offset) {
                            Some(bytes) => bytes.clone(),
                            None => read_block(source, block)?,
                        };
                        let index = blocks.len();
                        entry.insert(index);
                        blocks.push(Block {
                            range: block,
                            after: before.clone(),
                            before,
                        });
                        index
                    }
                };
                let start = (offset - block.offset) as usize;
                let used = data.len().min(block.size as usize - start);
                blocks[index].after[start..start + used].copy_from_slice(&data[..used]);
                offset += used as u64;
                data = &data[used..];
            }
        }
        blocks.retain(|block| block.before != block.after);
        for block in &blocks {
            latest.insert(block.range.offset, block.after.clone());
        }
        if !blocks.is_empty() {
            result.push((phase, blocks));
        }
    }
    Ok(result)
}

#[cfg(test)]
#[path = "boot_file_rebuild_tests.rs"]
mod tests;

pub(crate) fn plan(
    mut source: impl Source,
    geometry: Geometry,
    expected_ranges: &[Span],
    change: &Change,
) -> Result<Plan, String> {
    if change.rebuild_missing_bcd && change.damaged_bcd.is_some() {
        return Err("Choose one BCD repair action".into());
    }
    let (layout, boot, windows) = repair_extents(
        &mut source,
        geometry,
        expected_ranges,
        change.boot_partition,
        change.windows_partition,
    )?;
    let (manager, loader_sha256, machine, template) = {
        let (mut view, fs) = open_extent(&mut source, windows, geometry.sector)?;
        if !matches!(fs, Filesystem::Ntfs(_)) {
            return Err(
                "Select the NTFS partition containing the offline Windows installation".into(),
            );
        }
        let manager = fs
            .read_file(&mut view, INSTALLED_MANAGER, MAX_EXECUTABLE)?
            .ok_or("The Windows installation has no EFI boot manager")?;
        let loader = fs
            .read_file(&mut view, INSTALLED_LOADER, MAX_EXECUTABLE)?
            .ok_or("The Windows installation has no UEFI OS loader")?;
        let machine = pe_machine(&manager, &[10])?;
        if pe_machine(&loader, &[10, 16])? != machine {
            return Err("The Windows boot manager and loader architectures disagree".into());
        }
        let template = if change.rebuild_missing_bcd
            || matches!(change.damaged_bcd, Some(DamagedBcd::Replace { .. }))
        {
            Some(
                fs.read_file(&mut view, INSTALLED_TEMPLATE, boot_configuration::MAX_STORE)?
                    .ok_or("The Windows installation has no BCD-Template file")?,
            )
        } else {
            None
        };
        (manager, sector_edit::hash(&loader), machine, template)
    };
    let disk = Uuid::from_bytes_le(layout.disk_guid);
    let manager_sha256 = sector_edit::hash(&manager);
    let generated = template
        .map(|template| {
            let template_sha256 = sector_edit::hash(&template);
            let (bytes, entry) = boot_configuration::rebuild::generate(
                template,
                disk,
                change.boot_partition,
                change.windows_partition,
            )?;
            Ok::<_, String>((bytes, entry, template_sha256))
        })
        .transpose()?;
    let mut requested = vec![(rebuild::Target::Manager, manager.as_slice())];
    if change.rebuild_fallback {
        let fallback = match machine {
            0x8664 => rebuild::Target::FallbackX64,
            0x014c => rebuild::Target::FallbackIa32,
            0xaa64 => rebuild::Target::FallbackAa64,
            _ => {
                return Err(
                    "No supported EFI fallback filename for this Windows architecture".into(),
                )
            }
        };
        requested.push((fallback, manager.as_slice()));
    }
    let (relative, original_bcd_sha256, recovered_bcd) = {
        let mut view = filesystem(&mut source, geometry, boot)?;
        let volume = Volume::open(&mut view)?;
        let existing = volume.read_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)?;
        let original_sha256 = existing.as_deref().map(sector_edit::hash);
        let mut recovered = None;
        let bcd = match &change.damaged_bcd {
            Some(choice) => {
                let bcd = existing
                    .as_ref()
                    .ok_or("The approved damaged BCD is missing")?;
                let expected = match choice {
                    DamagedBcd::Recover { original_sha256 }
                    | DamagedBcd::Replace { original_sha256 } => original_sha256,
                };
                if original_sha256.as_deref() != Some(expected.as_str()) {
                    return Err("The damaged BCD digest changed. Inspect the disk again".into());
                }
                if boot_configuration::parse(bcd.clone()).is_ok() {
                    return Err("The existing BCD is readable and must be preserved".into());
                }
                if matches!(choice, DamagedBcd::Recover { .. }) {
                    let mut logs = Vec::new();
                    for target in rebuild::BCD_LOGS {
                        let path = target.path();
                        if let Some(bytes) = volume.read_file(&mut view, &path, MAX_EXECUTABLE)? {
                            logs.push((*path.last().unwrap(), bytes));
                        }
                    }
                    let inputs = logs
                        .iter()
                        .map(|(name, bytes)| (*name, bytes.as_slice()))
                        .collect::<Vec<_>>();
                    recovered = Some(boot_configuration::recovery::recover(
                        bcd.clone(),
                        &inputs,
                        disk,
                        change.boot_partition,
                        change.windows_partition,
                    )?);
                }
                rebuild::BcdUpdate::Replace(bcd)
            }
            None if change.rebuild_missing_bcd => {
                if existing.is_some() {
                    return Err(
                        "An existing BCD must be repaired through the existing-store action".into(),
                    );
                }
                rebuild::BcdUpdate::Create
            }
            None => rebuild::BcdUpdate::Keep,
        };
        if let Some(bytes) = generated
            .as_ref()
            .map(|(bytes, _, _)| bytes)
            .or(recovered.as_ref().map(|(bytes, _)| bytes))
        {
            requested.push((rebuild::Target::Bcd, bytes));
        }
        (
            rebuild::plan(&mut view, geometry.sector, &requested, bcd)?,
            original_sha256,
            recovered.map(|(_, replay)| replay),
        )
    };
    let phases = physical_blocks(&mut source, geometry, boot, relative.phases)?;
    let files: Vec<_> = relative
        .files
        .iter()
        .map(|file| FileSummary {
            path: file_path(file.target),
            bytes: file.bytes,
            original_sha256: file.original.as_deref().map(sector_edit::hash),
            installed_sha256: file.sha256.clone(),
        })
        .collect();
    let bcd_sha256 = relative
        .files
        .iter()
        .find(|file| file.target == rebuild::Target::Bcd)
        .map(|file| file.sha256.clone())
        .or_else(|| original_bcd_sha256.clone());
    let retired_logs = relative
        .retired
        .iter()
        .map(|file| RetiredLog {
            path: file_path(file.target),
            bytes: file.original.len(),
            sha256: sector_edit::hash(&file.original),
        })
        .collect::<Vec<_>>();
    let new_bcd = generated.map(|(_, entry, template_sha256)| NewBcd {
        entry,
        template_sha256,
        retired_logs: retired_logs.clone(),
    });
    let recovered_bcd = recovered_bcd.map(|replay| RecoveredBcd {
        replay,
        retired_logs: retired_logs.clone(),
    });
    let mut witness = Sha256::new();
    witness_layout(&mut witness, &layout);
    witness.update(relative.ownership_before);
    witness.update(loader_sha256.as_bytes());
    witness.update(manager_sha256.as_bytes());
    witness.update(original_bcd_sha256.as_deref().unwrap_or("").as_bytes());
    for file in &files {
        witness.update(file.path.as_bytes());
        witness.update(file.original_sha256.as_deref().unwrap_or("").as_bytes());
        witness.update(file.installed_sha256.as_bytes());
    }
    if let Some(bcd) = &new_bcd {
        witness.update(bcd.template_sha256.as_bytes());
    }
    for log in &retired_logs {
        witness.update(log.path.as_bytes());
        witness.update(log.sha256.as_bytes());
    }
    for (phase, blocks) in &phases {
        witness.update([*phase as u8]);
        for block in blocks {
            witness_block(&mut witness, block);
        }
    }
    Ok(Plan {
        summary: Summary {
            boot_offset: boot.start,
            windows_offset: windows.start,
            machine,
            loader_sha256,
            files,
            physical_writes: phases.iter().map(|(_, blocks)| blocks.len()).sum(),
            new_bcd,
            recovered_bcd,
        },
        fingerprint: witness.finalize().into(),
        layout,
        originals: relative.files,
        retired: relative.retired,
        phases,
        bcd_sha256,
        manager_sha256,
        boot,
        geometry,
    })
}

impl Plan {
    pub(crate) fn write<R: Source + Write>(
        &self,
        writer: &mut R,
        before_write: impl FnOnce() -> Result<(), String>,
        mut sync: impl FnMut(&mut R) -> std::io::Result<()>,
    ) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for (_, blocks) in &self.phases {
            for block in blocks {
                if seen.insert(block.range.offset)
                    && read_block(writer, block.range)? != block.before
                {
                    return Err("EFI blocks changed since the confirmed preview".into());
                }
            }
        }
        before_write()?;
        for (phase, blocks) in &self.phases {
            for block in blocks {
                sector_edit::write_once(writer, block.range, &block.before, &block.after, &mut sync)
                    .map_err(|e| format!("EFI file repair stopped in phase {phase:?}. The repair may be partial. {e}"))?;
            }
        }
        let mut view = filesystem(writer, self.geometry, self.boot)?;
        crate::fat_recovery::resize::check_boot_fat32(&mut view, self.geometry.sector)?;
        let volume = Volume::open(&mut view)?;
        let bcd = volume.read_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)?;
        if bcd.as_deref().map(sector_edit::hash) != self.bcd_sha256 {
            return Err("The BCD did not pass SHA-256 verification".into());
        }
        if let Some(rebuilt) = &self.summary.new_bcd {
            rebuilt
                .entry
                .verify(bcd.as_deref().ok_or("The new BCD is missing")?)?;
        }
        if let Some(recovered) = &self.summary.recovered_bcd {
            let boot = super::boot_partition(&self.layout, self.boot.start)
                .ok_or("EFI partition missing from the approved BCD layout")?;
            let windows = self
                .layout
                .partitions
                .iter()
                .find(|part| part.range.start == self.summary.windows_offset)
                .map(|part| Uuid::from_bytes_le(part.guid))
                .ok_or("Windows partition missing from the approved BCD layout")?;
            let (entry, count) = boot_configuration::recovery::verify_store(
                bcd.as_deref().ok_or("The restored BCD is missing")?,
                Uuid::from_bytes_le(self.layout.disk_guid),
                boot,
                windows,
            )?;
            if entry != recovered.replay.default_entry || count != recovered.replay.windows_entries
            {
                return Err("The restored BCD entries differ from the approved recovery".into());
            }
        }
        if self.summary.new_bcd.is_some() || self.summary.recovered_bcd.is_some() {
            for target in rebuild::BCD_LOGS {
                if volume
                    .read_file(&mut view, &target.path(), MAX_EXECUTABLE)?
                    .is_some()
                {
                    return Err("A stale BCD log remains after rebuilding the BCD".into());
                }
            }
            let manager = volume
                .read_file(&mut view, &rebuild::Target::Manager.path(), MAX_EXECUTABLE)?
                .ok_or("The EFI boot manager is missing after rebuilding the BCD")?;
            if sector_edit::hash(&manager) != self.manager_sha256 {
                return Err("The EFI boot manager differs from the Windows installation".into());
            }
        }
        for file in &self.originals {
            let data = volume
                .read_file(&mut view, &file.target.path(), MAX_EXECUTABLE)?
                .ok_or("A rebuilt EFI file is missing during readback")?;
            if data.len() != file.bytes || sector_edit::hash(&data) != file.sha256 {
                return Err("A rebuilt EFI file did not pass SHA-256 readback".into());
            }
        }
        drop(view);
        if gpt(writer, self.geometry)? != self.layout {
            return Err("The GPT identity or layout changed while copying EFI files".into());
        }
        Ok(())
    }
}

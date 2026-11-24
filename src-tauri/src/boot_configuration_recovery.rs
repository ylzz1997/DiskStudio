//! Recover a dirty BCD in memory from strictly checked, retained registry logs.
use super::{clean_hive, parse, repair, Device, MAX_STORE};
use regf::{
    transaction_log::{LogEntry, OldFormatLog},
    BaseBlock, BASE_BLOCK_SIZE,
};
use serde::Serialize;
use std::{collections::BTreeMap, io::Cursor};
use uuid::Uuid;

const PAGE: usize = 512;
const BIN: usize = 4096;
const MAX_ENTRIES: usize = 1024;
const MAX_PAGES: usize = MAX_STORE / PAGE;
const HALF_SEQUENCE_RANGE: u32 = 1 << 31;

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Format {
    Incremental,
    Legacy,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Replay {
    format: Format,
    first_sequence: u32,
    last_sequence: u32,
    transactions: usize,
    logs: Vec<&'static str>,
    pub default_entry: Uuid,
    pub windows_entries: usize,
}

fn bin_size(size: u32) -> Result<usize, String> {
    let size = size as usize;
    if size < BIN || size > MAX_STORE - BASE_BLOCK_SIZE || !size.is_multiple_of(BIN) {
        return Err("BCD log declares an invalid or oversized hive extent".into());
    }
    Ok(size)
}

fn log_header(raw: &[u8], primary: &BaseBlock) -> Result<BaseBlock, String> {
    let mut bytes = [0; BASE_BLOCK_SIZE];
    bytes[..PAGE].copy_from_slice(raw.get(..PAGE).ok_or("Truncated BCD log header")?);
    let log = BaseBlock::parse(&bytes).map_err(|error| error.to_string())?;
    let filename = primary.get_file_name();
    if !log.checksum_valid
        || log.primary_sequence != log.secondary_sequence
        || log.file_format != primary.file_format
        || log.clustering_factor != primary.clustering_factor
        || log.major_version != primary.major_version
        || log.minor_version != primary.minor_version
        || log.root_cell_offset != primary.root_cell_offset
        || !log.get_file_name().eq_ignore_ascii_case(&filename)
        || log.rm_id != primary.rm_id
        || log.tm_id != primary.tm_id
        || (filename.is_empty() && primary.rm_id == [0; 16])
    {
        return Err("BCD log header does not match the dirty store".into());
    }
    bin_size(log.hive_bins_data_size)?;
    Ok(log)
}

struct Image {
    bytes: Vec<u8>,
    present: Vec<bool>,
}

impl Image {
    fn new(mut bytes: Vec<u8>, bins: usize) -> Self {
        let original_pages = ((bytes.len() - BASE_BLOCK_SIZE) / PAGE).min(bins / PAGE);
        bytes.resize(BASE_BLOCK_SIZE + bins, 0);
        let mut present = vec![false; bins / PAGE];
        present[..original_pages].fill(true);
        Self { bytes, present }
    }

    fn resize(&mut self, bins: usize) {
        self.present.resize(bins / PAGE, false);
        self.bytes.resize(BASE_BLOCK_SIZE + bins, 0);
    }

    fn apply(&mut self, offset: usize, data: &[u8], written: &mut [bool]) -> Result<(), String> {
        if data.is_empty() || !offset.is_multiple_of(PAGE) || !data.len().is_multiple_of(PAGE) {
            return Err("BCD log contains a partial or unaligned dirty page".into());
        }
        let end = offset
            .checked_add(data.len())
            .ok_or("Invalid BCD log page range")?;
        let pages = offset / PAGE..end / PAGE;
        let transaction = written
            .get_mut(pages.clone())
            .ok_or("BCD log page extends beyond the declared hive size")?;
        if transaction.iter().any(|&written| written) {
            return Err("BCD log transaction contains overlapping dirty pages".into());
        }
        transaction.fill(true);
        self.present[pages].fill(true);
        self.bytes[BASE_BLOCK_SIZE + offset..BASE_BLOCK_SIZE + end].copy_from_slice(data);
        Ok(())
    }

    fn finish(mut self, mut header: BaseBlock, next: u32) -> Result<Vec<u8>, String> {
        if self.present.contains(&false) {
            return Err("BCD log chain does not restore all missing hive pages".into());
        }
        header.primary_sequence = next;
        header.secondary_sequence = next;
        header.hive_bins_data_size = (self.bytes.len() - BASE_BLOCK_SIZE) as u32;
        header
            .write(&mut &mut self.bytes[..BASE_BLOCK_SIZE])
            .map_err(|error| error.to_string())?;
        let hive = clean_hive(self.bytes.clone())?;
        repair::validate_tree(&hive)?;
        Ok(self.bytes)
    }
}

struct Incremental {
    name: &'static str,
    header: BaseBlock,
    entries: Vec<LogEntry>,
}

fn incremental(name: &'static str, raw: &[u8], header: BaseBlock) -> Result<Incremental, String> {
    let mut entries = Vec::new();
    let mut expected = header.primary_sequence;
    let mut pending = &raw[PAGE..];
    while !pending.is_empty() {
        if pending.iter().all(|&byte| byte == 0) {
            break;
        }
        if pending.get(..4) != Some(LogEntry::SIGNATURE.as_slice())
            || pending.len() < LogEntry::HEADER_SIZE
        {
            return Err("BCD log contains a damaged or truncated transaction".into());
        }
        let sequence = u32::from_le_bytes(pending[12..16].try_into().unwrap());
        // A log can retain already superseded entries after its live prefix.
        if !entries.is_empty() && sequence.wrapping_sub(expected) >= HALF_SEQUENCE_RANGE {
            break;
        }
        if sequence != expected || entries.len() >= MAX_ENTRIES {
            return Err(
                "BCD log transaction sequence is discontinuous or exceeds the limit".into(),
            );
        }
        let size = u32::from_le_bytes(pending[4..8].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(pending[20..24].try_into().unwrap()) as usize;
        if size < LogEntry::HEADER_SIZE || count == 0 || count > MAX_PAGES {
            return Err("BCD log transaction size or dirty page count is invalid".into());
        }
        let raw = pending
            .get(..size)
            .ok_or("BCD log transaction is truncated")?;
        let entry = LogEntry::parse(raw).map_err(|error| error.to_string())?;
        bin_size(entry.hive_bins_data_size)?;
        entries.push(entry);
        expected = expected.wrapping_add(1);
        pending = &pending[size..];
    }
    Ok(Incremental {
        name,
        header,
        entries,
    })
}

fn modern_replay(
    mut image: Image,
    mut primary: BaseBlock,
    logs: &[Incremental],
) -> Result<(Vec<u8>, Replay), String> {
    let start = primary.secondary_sequence;
    let distance = primary.primary_sequence.wrapping_sub(start);
    if distance >= HALF_SEQUENCE_RANGE || logs.len() > 2 {
        return Err("BCD log sequence or rotation is ambiguous".into());
    }
    let mut transactions = BTreeMap::new();
    for log in logs {
        for entry in &log.entries {
            let index = entry.sequence_number.wrapping_sub(start);
            if index < HALF_SEQUENCE_RANGE {
                if let Some((_, previous)) = transactions.insert(index, (log, entry)) {
                    if (previous.hash1, previous.hash2) != (entry.hash1, entry.hash2) {
                        return Err("BCD logs disagree about a transaction sequence".into());
                    }
                }
            }
        }
    }
    if transactions.is_empty()
        || transactions.len() < distance as usize
        || transactions.len() > MAX_ENTRIES
        || !transactions
            .keys()
            .copied()
            .eq(0..transactions.len() as u32)
    {
        return Err("BCD logs do not contain a complete sequence for this dirty store".into());
    }
    let mut used = Vec::new();
    let mut sequence = start;
    for (log, entry) in transactions.values() {
        image.resize(bin_size(entry.hive_bins_data_size)?);
        let mut written = vec![false; image.present.len()];
        for (page, data) in entry.page_references.iter().zip(&entry.dirty_pages) {
            image.apply(page.offset as usize, data, &mut written)?;
        }
        if !used.contains(&log.name) {
            used.push(log.name);
        }
        primary.flags = (log.header.flags & !1) | entry.flags;
        primary.last_written = log.header.last_written;
        primary.last_reorganized = log.header.last_reorganized;
        sequence = entry.sequence_number;
    }
    let bytes = image.finish(primary, sequence.wrapping_add(1))?;
    Ok((
        bytes,
        replay(
            Format::Incremental,
            start,
            sequence,
            transactions.len(),
            used,
        ),
    ))
}

fn legacy_replay(
    mut image: Image,
    mut primary: BaseBlock,
    name: &'static str,
    log: OldFormatLog,
) -> Result<(Vec<u8>, Replay), String> {
    let sequence = primary.primary_sequence;
    if !log.can_apply_to(primary.last_written)
        || log.base_block.primary_sequence != sequence
        || log.dirty_pages.is_empty()
    {
        return Err("The legacy BCD log does not match the interrupted write".into());
    }
    image.resize(bin_size(log.base_block.hive_bins_data_size)?);
    let mut written = vec![false; image.present.len()];
    for page in log.dirty_pages {
        image.apply(page.offset as usize, &page.data, &mut written)?;
    }
    primary.flags = log.base_block.flags;
    let bytes = image.finish(primary, sequence)?;
    Ok((
        bytes,
        replay(Format::Legacy, sequence, sequence, 1, vec![name]),
    ))
}

fn replay(
    format: Format,
    first_sequence: u32,
    last_sequence: u32,
    transactions: usize,
    logs: Vec<&'static str>,
) -> Replay {
    Replay {
        format,
        first_sequence,
        last_sequence,
        transactions,
        logs,
        default_entry: Uuid::nil(),
        windows_entries: 0,
    }
}

pub(crate) fn recover(
    original: Vec<u8>,
    logs: &[(&'static str, &[u8])],
    disk: Uuid,
    boot: Uuid,
    windows: Uuid,
) -> Result<(Vec<u8>, Replay), String> {
    let primary = BaseBlock::parse(&original).map_err(|error| error.to_string())?;
    if original.len() > MAX_STORE
        || !primary.checksum_valid
        || primary.file_type != 0
        || primary.file_format != 1
        || primary.major_version != 1
        || !(3..=6).contains(&primary.minor_version)
        || primary.primary_sequence == primary.secondary_sequence
    {
        return Err(
            "BCD log recovery requires a dirty store with a valid recoverable header".into(),
        );
    }
    let bins = bin_size(primary.hive_bins_data_size)?;
    let mut modern = Vec::new();
    let mut legacy = None;
    for &(name, raw) in logs {
        if raw.iter().all(|&byte| byte == 0) {
            continue;
        }
        let header = log_header(raw, &primary)?;
        match header.file_type {
            6 => modern.push(incremental(name, raw, header)?),
            1 | 2 => {
                let log =
                    OldFormatLog::parse(Cursor::new(raw)).map_err(|error| error.to_string())?;
                if log.can_apply_to(primary.last_written)
                    && header.primary_sequence == primary.primary_sequence
                {
                    if legacy.replace((name, log)).is_some() {
                        return Err(
                            "More than one legacy log matches the interrupted BCD write".into()
                        );
                    }
                }
            }
            _ => return Err("Unsupported BCD transaction log format".into()),
        }
    }
    let image = Image::new(original, bins);
    let (bytes, mut report) = match (legacy, modern.is_empty()) {
        (None, false) => modern_replay(image, primary, &modern)?,
        (Some((name, log)), true) => legacy_replay(image, primary, name, log)?,
        _ => return Err("No unambiguous BCD transaction log chain is available".into()),
    };
    (report.default_entry, report.windows_entries) = verify_store(&bytes, disk, boot, windows)?;
    Ok((bytes, report))
}

pub(crate) fn verify_store(
    bytes: &[u8],
    disk: Uuid,
    boot: Uuid,
    windows: Uuid,
) -> Result<(Uuid, usize), String> {
    let store = parse(bytes.to_vec())?;
    let manager = store
        .manager
        .ok_or("Recovered BCD lacks a Windows boot manager")?;
    let loader = store
        .default
        .and_then(|id| store.entries.iter().find(|entry| entry.id == id))
        .ok_or("Recovered BCD does not select a default Windows loader")?;
    let boot_gpt = Device::Gpt {
        disk,
        partition: boot,
    };
    let windows_gpt = Device::Gpt {
        disk,
        partition: windows,
    };
    if !matches!(manager.device, Some(Device::Boot)) && manager.device != Some(boot_gpt)
        || !manager
            .path
            .as_deref()
            .is_some_and(|path| path.eq_ignore_ascii_case(r"\EFI\Microsoft\Boot\bootmgfw.efi"))
        || loader.object_type != 0x1020_0003
        || loader.device != Some(windows_gpt)
        || loader.os_device != Some(windows_gpt)
        || !loader
            .path
            .as_deref()
            .is_some_and(|path| path.eq_ignore_ascii_case(r"\Windows\System32\winload.efi"))
        || !loader
            .system_root
            .as_deref()
            .is_some_and(|path| path.eq_ignore_ascii_case(r"\Windows"))
    {
        return Err("Recovered BCD does not match the selected EFI and Windows partitions".into());
    }
    let windows_entries = store
        .entries
        .iter()
        .filter(|entry| entry.object_type == 0x1020_0003)
        .count();
    Ok((loader.id, windows_entries))
}

#[cfg(test)]
#[path = "boot_configuration_recovery_tests.rs"]
mod tests;

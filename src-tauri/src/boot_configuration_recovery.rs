//! Replay strictly checked registry log chains in memory. BCD recovery also verifies the boot graph.
use super::{clean_hive, parse, repair, Device, MAX_STORE};
use regf::{structures::marvin32_hash, transaction_log::OldFormatLog, BaseBlock, BASE_BLOCK_SIZE};
use serde::Serialize;
use std::{collections::BTreeMap, io::Cursor};
use uuid::Uuid;

const PAGE: usize = 512;
const BIN: usize = 4096;
const MAX_ENTRIES: usize = 1024;
const HALF_SEQUENCE_RANGE: u32 = 1 << 31;
const LOG_SIGNATURE: &[u8; 4] = b"HvLE";
const LOG_HEADER: usize = 40;
// The registry log's native 64-bit seed, interpreted in little-endian order.
const LOG_SEED: u64 = 0x82ef_4d88_7a4e_55c5;

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

fn bin_size(size: u32, limit: usize) -> Result<usize, String> {
    let size = size as usize;
    if size < BIN || size > limit - BASE_BLOCK_SIZE || !size.is_multiple_of(BIN) {
        return Err("BCD log declares an invalid or oversized hive extent".into());
    }
    Ok(size)
}

fn log_header(raw: &[u8], primary: &BaseBlock, limit: usize) -> Result<BaseBlock, String> {
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
    bin_size(log.hive_bins_data_size, limit)?;
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
        Ok(self.bytes)
    }
}

struct Incremental<'a> {
    name: &'static str,
    header: BaseBlock,
    entries: Vec<Transaction<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayScope {
    BootStore,
    LiveSnapshot,
}

struct Transaction<'a> {
    raw: &'a [u8],
    sequence: u32,
    flags: u32,
    bins: usize,
    pages: Vec<(usize, &'a [u8])>,
}

fn transaction(raw: &[u8], expected: u32, limit: usize) -> Result<Transaction<'_>, String> {
    if raw.len() < LOG_HEADER || raw.get(..4) != Some(LOG_SIGNATURE) {
        return Err("Registry log contains a damaged or truncated transaction".into());
    }
    let word = |offset: usize| u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap());
    let sequence = word(12);
    if sequence != expected {
        return Err("Registry log transaction sequence is discontinuous".into());
    }
    let size = word(4) as usize;
    let count = word(20) as usize;
    let flags = word(8);
    let bins = bin_size(word(16), limit)?;
    if size < PAGE || !size.is_multiple_of(PAGE) || count == 0 || count > limit / PAGE || flags > 1
    {
        return Err("Registry log transaction size or dirty page count is invalid".into());
    }
    let raw = raw
        .get(..size)
        .ok_or("Registry log transaction is truncated")?;
    let hash1 = u64::from_le_bytes(raw[24..32].try_into().unwrap());
    let hash2 = u64::from_le_bytes(raw[32..40].try_into().unwrap());
    if marvin32_hash(&raw[..32], LOG_SEED) != hash2
        || marvin32_hash(&raw[LOG_HEADER..], LOG_SEED) != hash1
    {
        return Err("Registry log transaction checksum is invalid".into());
    }
    let mut data = LOG_HEADER + count * 8;
    let references = raw
        .get(LOG_HEADER..data)
        .ok_or("Registry log page references are truncated")?;
    let mut pages = Vec::with_capacity(count);
    for fields in references.chunks_exact(8) {
        let offset = u32::from_le_bytes(fields[..4].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(fields[4..].try_into().unwrap()) as usize;
        if len == 0 || offset.checked_add(len).is_none_or(|end| end > bins) {
            return Err("Registry log page exceeds the declared hive extent".into());
        }
        let end = data
            .checked_add(len)
            .ok_or("Registry log page size overflow")?;
        pages.push((
            offset,
            raw.get(data..end)
                .ok_or("Registry log page data is truncated")?,
        ));
        data = end;
    }
    Ok(Transaction {
        raw,
        sequence,
        flags,
        bins,
        pages,
    })
}

fn incremental<'a>(
    name: &'static str,
    raw: &'a [u8],
    header: BaseBlock,
    primary: &BaseBlock,
    limit: usize,
    scope: ReplayScope,
) -> Result<Incremental<'a>, String> {
    let mut entries = Vec::new();
    let mut expected = header.primary_sequence;
    let mut pending = &raw[PAGE..];
    while !pending.is_empty() {
        if pending.iter().all(|&byte| byte == 0) {
            break;
        }
        // A log can retain already superseded entries after its live prefix.
        if !entries.is_empty()
            && pending.get(..4) == Some(LOG_SIGNATURE)
            && pending.get(12..16).is_some_and(|sequence| {
                u32::from_le_bytes(sequence.try_into().unwrap()).wrapping_sub(expected)
                    >= HALF_SEQUENCE_RANGE
            })
        {
            break;
        }
        if entries.len() >= MAX_ENTRIES {
            return Err("Registry log transaction count exceeds the limit".into());
        }
        let entry = match transaction(pending, expected, limit) {
            Ok(entry) => entry,
            // A rotating live log may end in recycled padding or an incomplete
            // write. Stop at its verified prefix. Replay still requires a
            // complete, non-conflicting chain across both log files.
            Err(_) if scope == ReplayScope::LiveSnapshot => break,
            Err(error) => {
                return Err(format!(
                    "{name}, transaction {expected} (hive {}–{}): {error}",
                    primary.secondary_sequence, primary.primary_sequence,
                ))
            }
        };
        expected = expected.wrapping_add(1);
        pending = &pending[entry.raw.len()..];
        entries.push(entry);
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
    logs: &[Incremental<'_>],
) -> Result<(Vec<u8>, Replay), String> {
    let start = primary.secondary_sequence;
    let distance = primary.primary_sequence.wrapping_sub(start);
    if distance >= HALF_SEQUENCE_RANGE || logs.len() > 2 {
        return Err("BCD log sequence or rotation is ambiguous".into());
    }
    let mut transactions = BTreeMap::new();
    for log in logs {
        for entry in &log.entries {
            let index = entry.sequence.wrapping_sub(start);
            if index < HALF_SEQUENCE_RANGE {
                if let Some((_, previous)) = transactions.insert(index, (log, entry)) {
                    if previous.raw != entry.raw {
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
        image.resize(entry.bins);
        let mut written = vec![false; image.present.len()];
        for &(offset, data) in &entry.pages {
            image.apply(offset, data, &mut written)?;
        }
        if !used.contains(&log.name) {
            used.push(log.name);
        }
        primary.flags = (log.header.flags & !1) | entry.flags;
        primary.last_written = log.header.last_written;
        primary.last_reorganized = log.header.last_reorganized;
        sequence = entry.sequence;
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
    limit: usize,
) -> Result<(Vec<u8>, Replay), String> {
    let sequence = primary.primary_sequence;
    if !log.can_apply_to(primary.last_written)
        || log.base_block.primary_sequence != sequence
        || log.dirty_pages.is_empty()
    {
        return Err("The legacy BCD log does not match the interrupted write".into());
    }
    image.resize(bin_size(log.base_block.hive_bins_data_size, limit)?);
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
    let (bytes, mut report) = replay_hive(original, logs, MAX_STORE, ReplayScope::BootStore)?;
    let hive = clean_hive(bytes.clone())?;
    repair::validate_tree(&hive)?;
    (report.default_entry, report.windows_entries) = verify_store(&bytes, disk, boot, windows)?;
    Ok((bytes, report))
}

pub(crate) fn replay_hive(
    original: Vec<u8>,
    logs: &[(&'static str, &[u8])],
    limit: usize,
    scope: ReplayScope,
) -> Result<(Vec<u8>, Replay), String> {
    let primary = BaseBlock::parse(&original).map_err(|error| error.to_string())?;
    if !(BASE_BLOCK_SIZE..=limit).contains(&original.len())
        || logs.len() > 3
        || logs.iter().any(|(_, bytes)| bytes.len() > limit)
        || !primary.checksum_valid
        || primary.file_type != 0
        || primary.file_format != 1
        || primary.major_version != 1
        || !(3..=6).contains(&primary.minor_version)
        || primary.primary_sequence == primary.secondary_sequence
    {
        return Err(
            "Registry log replay requires a dirty hive with a valid recoverable header".into(),
        );
    }
    let bins = bin_size(primary.hive_bins_data_size, limit)?;
    let mut modern = Vec::new();
    let mut legacy = None;
    for &(name, raw) in logs {
        if raw.iter().all(|&byte| byte == 0) {
            continue;
        }
        let header = log_header(raw, &primary, limit)?;
        match header.file_type {
            6 => modern.push(incremental(name, raw, header, &primary, limit, scope)?),
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
    match (legacy, modern.is_empty()) {
        (None, false) => modern_replay(image, primary, &modern),
        (Some((name, log)), true) => legacy_replay(image, primary, name, log, limit),
        _ => Err("No unambiguous registry transaction log chain is available".into()),
    }
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

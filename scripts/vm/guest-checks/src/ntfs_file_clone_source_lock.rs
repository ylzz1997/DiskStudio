//! Measures the raw effects of one source-volume lock cycle without cloning.
use super::{
    completed_fixture, contents, resolved, sample, verify_scope, CompletedFixture, A, ALIAS, B,
    MIB, SOURCE_AT, SOURCE_BYTES, TARGET_BYTES,
};
use crate::{data_windows, err, partitions, reallocation_checks, resize_clone_checks, Selection};
use cap_std::fs::Dir;
use ntfs::{
    attribute_value::NtfsAttributeValue,
    structured_values::{NtfsFileName, NtfsFileNamespace},
    Ntfs, NtfsAttribute, NtfsAttributeType, NtfsFile, NtfsFileFlags,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read, Seek, SeekFrom},
    ops::Range,
    sync::atomic::AtomicBool,
};

const TXF_LOG_DIRECTORY: &str = "$Extend/$RmMetadata/$TxfLog";
const TXF_LOG_INDEX: &str = "$TxfLog $I30 index allocation";

#[derive(Debug, PartialEq, Eq)]
struct Extent {
    bytes: Range<usize>,
    stream: String,
    logical: usize,
    record_size: Option<usize>,
    index: bool,
    journal: bool,
}

#[derive(PartialEq, Eq)]
struct TxfIndexEntry {
    offset: usize,
    file: u64,
    sequence: u16,
}

#[derive(Default, PartialEq, Eq)]
struct Metadata {
    sector: usize,
    extents: Vec<Extent>,
    names: BTreeMap<usize, String>,
    transaction_records: BTreeSet<usize>,
    txf_index_entries: BTreeMap<usize, Vec<TxfIndexEntry>>,
}

fn read_range(reader: &mut (impl Read + Seek), output: &mut [u8]) -> Result<(), String> {
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    reader.read_exact(output).map_err(err)
}

fn stream_extents(
    metadata: &mut Metadata,
    reader: &mut (impl Read + Seek),
    attr: &NtfsAttribute<'_, '_>,
    label: &str,
    record_size: Option<usize>,
    journal: bool,
    size: usize,
) -> Result<(), String> {
    let NtfsAttributeValue::NonResident(value) = attr.value(reader).map_err(err)? else {
        return Err(format!("Unsupported {label} metadata layout"));
    };
    let index = attr.ty().map_err(err)? == NtfsAttributeType::IndexAllocation;
    let mut logical = 0;
    for run in value.data_runs() {
        let run = run.map_err(err)?;
        let len = run.allocated_size() as usize;
        if len == 0 || len % metadata.sector != 0 {
            return Err(format!("Unaligned {label} extent"));
        }
        if let Some(position) = run.data_position().value() {
            let start = position.get() as usize;
            let end = start
                .checked_add(len)
                .filter(|&end| end <= size)
                .ok_or_else(|| format!("Out-of-range {label} extent"))?;
            if start % metadata.sector != 0 {
                return Err(format!("Unaligned {label} extent"));
            }
            metadata.extents.push(Extent {
                bytes: start..end,
                stream: label.into(),
                logical,
                record_size,
                index,
                journal,
            });
        }
        logical = logical
            .checked_add(len)
            .ok_or("NTFS metadata run offset overflows")?;
    }
    if logical == 0 && attr.value_length() != 0 {
        return Err(format!("Missing {label} extents"));
    }
    Ok(())
}

fn txf_index_time_offset(
    parent: &NtfsFile<'_>,
    reader: &mut (impl Read + Seek),
    reported_at: u64,
    entry_len: usize,
    key: &NtfsFileName,
    metadata: &Metadata,
) -> Result<Option<(usize, usize)>, String> {
    let missing = "Invalid resident $TxfLog index entry";
    let at = usize::try_from(reported_at).map_err(err)?;
    if let Some(extent) = metadata
        .extents
        .iter()
        .find(|extent| extent.stream == TXF_LOG_INDEX && extent.index && extent.bytes.contains(&at))
    {
        let size = extent
            .record_size
            .ok_or("Missing $TxfLog index record size")?;
        let in_record = (extent.logical + at - extent.bytes.start) % size;
        let record_at = at
            .checked_sub(in_record)
            .ok_or("Invalid $TxfLog index position")?;
        if record_at < extent.bytes.start
            || record_at + size > extent.bytes.end
            || in_record + entry_len > size
        {
            return Err("The $TxfLog index entry exceeds its mapped record".into());
        }
        let mut raw = vec![0; size];
        reader
            .seek(SeekFrom::Start(record_at as u64))
            .map_err(err)?;
        reader.read_exact(&mut raw).map_err(err)?;
        let bytes = record_with_applied_fixups(&raw, true)
            .ok_or("Invalid $TxfLog index record protection")?;
        let time_at = in_record + 24;
        verify_index_key_position(&bytes, time_at, key)?;
        return Ok(Some((record_at, time_at)));
    }
    let parent_at = parent.position().value().ok_or(missing)?.get();
    let mut attributes = parent.attributes();
    while let Some(item) = attributes.next(reader) {
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.ty().map_err(err)? != NtfsAttributeType::IndexRoot
            || attr.name().map_err(err)? != "$I30"
        {
            continue;
        }
        let attr_at = attr.position().value().ok_or(missing)?.get();
        let value_len = attr.value_length();
        let Some(entry) = reported_at
            .checked_sub(attr_at)
            .filter(|&at| at < value_len)
        else {
            continue;
        };
        if !attr.is_resident() || entry + entry_len as u64 > value_len {
            return Err(missing.into());
        }
        // The vendored reader anchors resident data positions at the attribute
        // header. Add its on-disk value offset to address the actual key bytes.
        let attr_bytes = attr.record_bytes();
        let value_at = u16::from_le_bytes(
            attr_bytes
                .get(20..22)
                .ok_or(missing)?
                .try_into()
                .map_err(err)?,
        ) as usize;
        let time_at = value_at + entry as usize + 24;
        verify_index_key_position(attr_bytes, time_at, key)?;
        let offset = (attr_at + time_at as u64)
            .checked_sub(parent_at)
            .ok_or(missing)?;
        if offset + 48 > u64::from(parent.ntfs().file_record_size()) {
            return Err(missing.into());
        }
        return Ok(Some((parent_at as usize, offset as usize)));
    }
    Ok(None)
}

fn verify_index_key_position(
    bytes: &[u8],
    offset: usize,
    key: &NtfsFileName,
) -> Result<(), String> {
    let times = [
        key.creation_time(),
        key.modification_time(),
        key.mft_record_modification_time(),
        key.access_time(),
    ];
    for (bytes, time) in bytes
        .get(offset..offset + 32)
        .ok_or("Missing index timestamps")?
        .chunks_exact(8)
        .zip(times)
    {
        if bytes != time.nt_timestamp().to_le_bytes() {
            return Err("Decoded and raw index timestamps disagree".into());
        }
    }
    let sizes = bytes
        .get(offset + 32..offset + 48)
        .ok_or("Missing index sizes")?;
    if sizes[..8] != key.allocated_size().to_le_bytes()
        || sizes[8..] != key.data_size().to_le_bytes()
    {
        return Err("Decoded and raw index sizes disagree".into());
    }
    Ok(())
}

fn extend_metadata(
    ntfs: &Ntfs,
    reader: &mut (impl Read + Seek),
    parent: &NtfsFile<'_>,
    path: &str,
    size: usize,
    metadata: &mut Metadata,
) -> Result<(), String> {
    if path.matches('/').count() >= 8 {
        return Err("NTFS $Extend nesting exceeds the fixture budget".into());
    }
    let index = parent.directory_index(reader).map_err(err)?;
    let mut entries = index.entries();
    while let Some(entry) = entries.next(reader) {
        let entry = entry.map_err(err)?;
        let key = entry
            .key()
            .ok_or("An $Extend index key is missing")?
            .map_err(err)?;
        let parent_ref = key.parent_directory_reference();
        if parent_ref.file_record_number() != parent.file_record_number()
            || parent_ref.sequence_number() != parent.sequence_number()
        {
            return Err("An $Extend entry refers to a different directory".into());
        }
        let file = entry.to_file(ntfs, reader).map_err(err)?;
        if entry.file_reference().sequence_number() != file.sequence_number()
            || !file.flags().contains(NtfsFileFlags::IN_USE)
        {
            return Err("Invalid $Extend record identity".into());
        }
        let name = key.name().to_string_lossy();
        let file_path = format!("{path}/{name}");
        let txf_log = path == TXF_LOG_DIRECTORY
            && (name == "$TxfLog.blf"
                || name.strip_prefix("$TxfLogContainer").is_some_and(|number| {
                    number.len() == 20 && number.bytes().all(|digit| digit.is_ascii_digit())
                }));
        let txf_alias = path == TXF_LOG_DIRECTORY && key.namespace() == NtfsFileNamespace::Dos;
        if txf_alias {
            let names = crate::ntfs_names::read_for_merge(reader, &file)?;
            let primary = partitions::merge::txf_log_primary_name(&names)
                .ok_or("An alternate $TxfLog name lacks a verified journal file")?;
            if names.len() != 2
                || primary.parent != (parent.file_record_number(), parent.sequence_number())
                || file.is_directory()
            {
                return Err(
                    "An alternate $TxfLog name refers to a different file or parent".into(),
                );
            }
        }
        if (txf_log || txf_alias) && !file.is_directory() {
            let entry_at = entry
                .position()
                .value()
                .ok_or("Missing $TxfLog entry position")?
                .get();
            if let Some((record, offset)) = txf_index_time_offset(
                parent,
                reader,
                entry_at,
                usize::from(entry.index_entry_length()),
                &key,
                metadata,
            )? {
                metadata
                    .txf_index_entries
                    .entry(record)
                    .or_default()
                    .push(TxfIndexEntry {
                        offset,
                        file: file.file_record_number(),
                        sequence: file.sequence_number(),
                    });
            }
        }
        if txf_alias {
            continue;
        }
        if txf_log
            || matches!(
                file_path.as_str(),
                "$Extend/$RmMetadata/$Tops" | "$Extend/$RmMetadata/$TxfLog/$Tops"
            )
        {
            metadata
                .transaction_records
                .insert(file.file_record_number() as usize);
        }
        if metadata.names.len() >= 64
            || metadata
                .names
                .insert(file.file_record_number() as usize, name.clone())
                .is_some()
        {
            return Err("Ambiguous or excessive $Extend metadata".into());
        }
        let mut attrs = file.attributes();
        while let Some(item) = attrs.next(reader) {
            let item = item.map_err(err)?;
            let attr = item.to_attribute().map_err(err)?;
            if attr.is_resident() {
                continue;
            }
            let kind = attr.ty().map_err(err)?;
            let record_size = if kind == NtfsAttributeType::IndexAllocation
                && file_path == TXF_LOG_DIRECTORY
                && attr.name().map_err(err)? == "$I30"
            {
                let size = file
                    .directory_index(reader)
                    .map_err(err)?
                    .index_record_size() as usize;
                if !(512..=65536).contains(&size) || !size.is_power_of_two() {
                    return Err("Invalid $TxfLog index record geometry".into());
                }
                Some(size)
            } else {
                None
            };
            let label = match kind {
                NtfsAttributeType::Data if attr.name_length() == 0 => name.clone(),
                NtfsAttributeType::Data => {
                    format!("{name}:{}", attr.name().map_err(err)?.to_string_lossy())
                }
                NtfsAttributeType::IndexAllocation if record_size.is_some() => TXF_LOG_INDEX.into(),
                NtfsAttributeType::IndexAllocation => format!("{name} index"),
                NtfsAttributeType::Bitmap => format!("{name} bitmap"),
                _ => continue,
            };
            stream_extents(
                metadata,
                reader,
                &attr,
                &label,
                record_size,
                txf_log && kind == NtfsAttributeType::Data && attr.name_length() == 0,
                size,
            )?;
        }
        if file.is_directory() {
            extend_metadata(ntfs, reader, &file, &file_path, size, metadata)?;
        }
    }
    Ok(())
}

fn metadata_map(
    bytes: &[u8],
    catalogue: &partitions::merge::Catalogue,
) -> Result<Metadata, String> {
    let mut reader = Cursor::new(bytes);
    let ntfs = Ntfs::new(&mut reader).map_err(err)?;
    let sector = usize::from(ntfs.sector_size());
    if !matches!(sector, 512 | 4096) || bytes.len() % sector != 0 {
        return Err("The source uses an unexpected logical sector size".into());
    }
    let record_size = ntfs.file_record_size() as usize;
    if !(512..=65536).contains(&record_size) || !record_size.is_power_of_two() {
        return Err("An NTFS file record has unsupported geometry".into());
    }
    let mut metadata = Metadata {
        sector,
        ..Metadata::default()
    };
    metadata.names.insert(3, String::from("$Volume"));
    for (number, stream, record_size) in [
        (0, "$MFT", Some(record_size)),
        (1, "$MFTMirr", Some(record_size)),
        (2, "$LogFile", None),
        (4, "$AttrDef", None),
        (6, "$Bitmap", None),
        (10, "$UpCase", None),
    ] {
        let file = ntfs.file(&mut reader, number).map_err(err)?;
        if !file.flags().contains(NtfsFileFlags::IN_USE) || file.is_directory() {
            return Err(format!("Invalid NTFS metadata file {stream}"));
        }
        let item = file
            .data(&mut reader, "")
            .ok_or_else(|| format!("Missing {stream} data"))?
            .map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if !attr.is_resident() || number < 4 {
            stream_extents(
                &mut metadata,
                &mut reader,
                &attr,
                stream,
                record_size,
                number == 2,
                bytes.len(),
            )?;
        }
    }
    let extend = ntfs.file(&mut reader, 11).map_err(err)?;
    extend_metadata(
        &ntfs,
        &mut reader,
        &extend,
        "$Extend",
        bytes.len(),
        &mut metadata,
    )?;
    for (&id, node) in &catalogue.nodes {
        if !node.directory {
            continue;
        }
        let directory = ntfs.file(&mut reader, id.0).map_err(err)?;
        if directory.sequence_number() != id.1 || !directory.is_directory() {
            return Err("NTFS directory identity changed measuring its index".into());
        }
        let index = directory.directory_index(&mut reader).map_err(err)?;
        let size = index.index_record_size() as usize;
        let mut attrs = directory.attributes();
        while let Some(item) = attrs.next(&mut reader) {
            let item = item.map_err(err)?;
            let attr = item.to_attribute().map_err(err)?;
            if attr.ty().map_err(err)? == NtfsAttributeType::IndexAllocation
                && attr.name().map_err(err)? == "$I30"
            {
                if !(512..=65536).contains(&size) || !size.is_power_of_two() {
                    return Err("NTFS directory index has invalid record geometry".into());
                }
                stream_extents(
                    &mut metadata,
                    &mut reader,
                    &attr,
                    &format!("directory {} $I30", id.0),
                    Some(size),
                    false,
                    bytes.len(),
                )?;
            }
        }
    }
    metadata.extents.sort_by_key(|extent| extent.bytes.start);
    for entries in metadata.txf_index_entries.values_mut() {
        entries.sort_unstable_by_key(|entry| entry.offset);
    }
    if metadata
        .extents
        .windows(2)
        .any(|pair| pair[0].bytes.end > pair[1].bytes.start)
    {
        return Err("NTFS metadata extents overlap".into());
    }
    Ok(metadata)
}

fn delta(before: &[u8], after: &[u8], metadata: &Metadata) -> String {
    let mut groups = BTreeMap::new();
    let mut first_unknown = None;
    for (index, (before, after)) in before
        .chunks_exact(metadata.sector)
        .zip(after.chunks_exact(metadata.sector))
        .enumerate()
    {
        if before == after {
            continue;
        }
        let at = index * metadata.sector;
        let label = match metadata
            .extents
            .iter()
            .find(|extent| extent.bytes.contains(&at))
        {
            Some(extent) => match extent.record_size {
                Some(size) => {
                    let record = (extent.logical + at - extent.bytes.start) / size;
                    match metadata.names.get(&record).filter(|_| !extent.index) {
                        Some(name) => format!("{} #{record} {name}", extent.stream),
                        None => format!("{} #{record}", extent.stream),
                    }
                }
                None => extent.stream.clone(),
            },
            None => {
                first_unknown.get_or_insert(index);
                "unmapped".into()
            }
        };
        *groups.entry(label).or_insert(0usize) += 1;
    }
    let total: usize = groups.values().sum();
    let mut list = groups
        .iter()
        .take(8)
        .map(|(label, sectors)| format!("{label} {sectors}"))
        .collect::<Vec<_>>();
    if groups.len() > 8 {
        list.push(format!("{} more groups", groups.len() - 8));
    }
    if let Some(index) = first_unknown {
        list.push(format!("first unmapped LBA {index}"));
    }
    if list.is_empty() {
        "0 sectors".into()
    } else {
        format!("{total} sectors\n{}", list.join(", "))
    }
}

pub(super) struct SourceDelta {
    pub(super) sectors: usize,
    pub(super) mft_records: usize,
    pub(super) index_records: usize,
    pub(super) txf_index_fields: [usize; 3],
    pub(super) root_cache_names: Vec<String>,
}

fn record_with_applied_fixups(record: &[u8], index: bool) -> Option<Vec<u8>> {
    const FIXUP_STRIDE: usize = 512;
    let signature = if index { b"INDX" } else { b"FILE" };
    if record.get(..4)? != signature || record.len() % FIXUP_STRIDE != 0 {
        return None;
    }
    let word = |offset| {
        record
            .get(offset..offset + 2)?
            .try_into()
            .ok()
            .map(|bytes| u16::from_le_bytes(bytes) as usize)
    };
    let usa = word(4)?;
    let count = word(6)?;
    let payload = if index {
        let offset = u32::from_le_bytes(record.get(24..28)?.try_into().ok()?);
        24usize.checked_add(offset as usize)?
    } else {
        word(20)?
    };
    let usa_end = usa + count * 2;
    if usa < 40
        || usa_end > FIXUP_STRIDE - 2
        || usa_end > payload
        || payload > record.len()
        || count != record.len() / FIXUP_STRIDE + 1
    {
        return None;
    }
    let sequence = &record[usa..usa + 2];
    let mut restored = record.to_vec();
    for index in 1..count {
        let end = index * FIXUP_STRIDE;
        let saved = usa + index * 2;
        if &record[end - 2..end] != sequence {
            return None;
        }
        restored[end - 2..end].copy_from_slice(&record[saved..saved + 2]);
    }
    // The journal LSN and torn-write protection number can change when
    // Windows rewrites an otherwise byte-identical NTFS record.
    restored[8..16].fill(0);
    restored[usa..usa + 2].fill(0);
    Some(restored)
}

fn normalized_differences(before: &[u8], after: &[u8]) -> String {
    let mut count = 0;
    let mut offsets = Vec::new();
    let mut groups = BTreeSet::new();
    for (offset, (&a, &b)) in before.iter().zip(after).enumerate() {
        if a != b {
            count += 1;
            groups.insert(record_area(before, offset).unwrap_or_default());
            if offsets.len() < 12 {
                offsets.push(format!("{offset}:{a:02x}>{b:02x}"));
            }
        }
    }
    let areas = groups
        .iter()
        .map(|&area| {
            if area == 0 {
                "header/padding".into()
            } else {
                format!("{area:#x}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{count} payload bytes, areas {areas} ({})",
        offsets.join(" ")
    )
}

fn record_area(record: &[u8], offset: usize) -> Option<u32> {
    if record.get(..4)? != b"FILE" {
        return None;
    }
    let mut start = u16::from_le_bytes(record.get(20..22)?.try_into().ok()?) as usize;
    while start <= offset {
        let ty = u32::from_le_bytes(record.get(start..start + 4)?.try_into().ok()?);
        if ty == u32::MAX {
            return None;
        }
        let len = u32::from_le_bytes(record.get(start + 4..start + 8)?.try_into().ok()?) as usize;
        let end = start.checked_add(len)?;
        if len < 24 || end > record.len() {
            return None;
        }
        if offset < end {
            return Some(ty);
        }
        start = end;
    }
    None
}

fn timestamp_changes<'a>(
    before: &'a [u8],
    after: &'a [u8],
    first: usize,
) -> impl Iterator<Item = (usize, u64, u64)> + 'a {
    (0..4).filter_map(move |field| {
        let at = first + field * 8;
        let a = u64::from_le_bytes(before.get(at..at + 8)?.try_into().ok()?);
        let b = u64::from_le_bytes(after.get(at..at + 8)?.try_into().ok()?);
        (a != b).then_some((field, a, b))
    })
}

fn root_index_caches(
    fs: &Ntfs,
    reader: &mut (impl Read + Seek),
    start: usize,
    before: &[u8],
    after: &[u8],
) -> Result<Vec<String>, String> {
    let root = fs.root_directory(reader).map_err(err)?;
    let index = root.directory_index(reader).map_err(err)?;
    if index.index_record_size() as usize != after.len() || before.len() != after.len() {
        return Err("The changed root index record has unexpected geometry".into());
    }
    let end = start + after.len();
    let mut normalized = after.to_vec();
    let mut changed = Vec::new();
    let mut entries = index.entries();
    while let Some(entry) = entries.next(reader) {
        let entry = entry.map_err(err)?;
        let position = entry
            .position()
            .value()
            .ok_or("The root index entry has no position")?
            .get() as usize;
        if !(start..end).contains(&position) {
            continue;
        }
        let at = position - start;
        let first = at + 24;
        if at + usize::from(entry.index_entry_length()) > after.len() {
            return Err("The root index entry crosses its record boundary".into());
        }
        let key = entry.key().ok_or("Missing root index key")?.map_err(err)?;
        if key.parent_directory_reference().file_record_number() != root.file_record_number()
            || key.parent_directory_reference().sequence_number() != root.sequence_number()
        {
            return Err("A root index key refers to a different parent".into());
        }
        verify_index_key_position(after, first, &key)?;
        let times = timestamp_changes(before, after, first).collect::<Vec<_>>();
        if times.is_empty() {
            continue;
        }
        let file = entry.to_file(fs, reader).map_err(err)?;
        if !file.flags().contains(NtfsFileFlags::IN_USE)
            || entry.file_reference().sequence_number() != file.sequence_number()
        {
            return Err("A changed root cache refers to an inactive file".into());
        }
        let info = file.info().map_err(err)?;
        let name = key.name().to_string_lossy();
        for (field, previous, current) in times {
            let actual = match field {
                1 => info.modification_time(),
                2 => info.mft_record_modification_time(),
                _ => return Err(format!("Unexpected root cache field {field} in {name}")),
            }
            .nt_timestamp();
            if current < previous || current != actual {
                return Err(format!(
                    "Root cache {name} #{} field {field}: {previous:x}>{current:x}, record {actual:x}",
                    file.file_record_number()
                ));
            }
            let field = first + field * 8;
            normalized[field..field + 8].copy_from_slice(&before[field..field + 8]);
        }
        changed.push(format!("{name} #{}", file.file_record_number()));
    }
    if changed.is_empty() || normalized != before {
        return Err("Other bytes differ in the changed root index record".into());
    }
    Ok(changed)
}

fn compare_txf_index_entries(
    before: &[u8],
    after: &[u8],
    entries: &[TxfIndexEntry],
    mut checked_length: impl FnMut(&TxfIndexEntry) -> Option<u64>,
) -> Option<[usize; 3]> {
    let mut end = 0;
    let mut advanced = [0; 3];
    for entry in entries {
        let first = entry.offset;
        // Keep the creation and access times exact.
        let start = first + 8;
        if before.get(end..start)? != after.get(end..start)? {
            return None;
        }
        for (field, a, b) in timestamp_changes(before, after, first) {
            if !(1..=2).contains(&field) || b < a {
                return None;
            }
            advanced[field - 1] += 1;
        }
        let length_at = first + 40;
        if before.get(first + 32..length_at)? != after.get(first + 32..length_at)? {
            return None;
        }
        end = length_at + 8;
        let prior = before.get(length_at..end)?;
        let current = after.get(length_at..end)?;
        if prior != current {
            let found = u64::from_le_bytes(current.try_into().ok()?);
            // A stale zero length may be filled in only with the current
            // unnamed $DATA size of this exact verified journal file.
            if prior != [0; 8] || found == 0 || checked_length(entry) != Some(found) {
                return None;
            }
            advanced[2] += 1;
        }
    }
    (before.get(end..)? == after.get(end..)?).then_some(advanced)
}

fn txf_data_length(
    ntfs: &Ntfs,
    reader: &mut (impl Read + Seek),
    metadata: &Metadata,
    entry: &TxfIndexEntry,
) -> Option<u64> {
    let file = ntfs.file(reader, entry.file).ok()?;
    if file.sequence_number() != entry.sequence
        || file.is_directory()
        || !file.flags().contains(NtfsFileFlags::IN_USE)
    {
        return None;
    }
    let label = metadata.names.get(&(entry.file as usize))?;
    let allocated = metadata
        .extents
        .iter()
        .filter(|extent| extent.journal && &extent.stream == label)
        .map(|extent| extent.bytes.len() as u64)
        .sum::<u64>();
    let item = file.data(reader, "")?.ok()?;
    let attr = item.to_attribute().ok()?;
    let record = attr.record_bytes();
    let size = attr.value_length();
    (!attr.is_resident()
        && attr.flags().is_empty()
        && record.get(16..24)? == [0; 8]
        && u64::from_le_bytes(record.get(40..48)?.try_into().ok()?) == allocated
        && size <= allocated)
        .then_some(size)
}

pub(super) fn verify_clone_source(
    before: &[u8],
    after: &[u8],
    cancel: &AtomicBool,
) -> Result<SourceDelta, String> {
    if before.len() != after.len() {
        return Err("Source snapshots have different sizes".into());
    }
    let original = partitions::merge::catalogue_file_clone(&mut Cursor::new(before), cancel)?;
    let current = partitions::merge::catalogue_file_clone(&mut Cursor::new(after), cancel)?;
    original.same_source(&current)?;
    let metadata = metadata_map(before, &original)?;
    if metadata != metadata_map(after, &current)? {
        return Err("NTFS metadata mappings changed during the clone".into());
    }
    let mut changed = 0;
    let mut verified_records = BTreeSet::new();
    let mut unexpected_records = BTreeSet::new();
    let mut unexpected_sectors = BTreeSet::new();
    let mut problems = Vec::new();
    let mut txf_index_fields = [0; 3];
    let mut root_cache_names = Vec::new();
    let mut reader = Cursor::new(after);
    let ntfs = Ntfs::new(&mut reader).map_err(err)?;
    for (i, (left, right)) in before
        .chunks_exact(metadata.sector)
        .zip(after.chunks_exact(metadata.sector))
        .enumerate()
    {
        if left == right {
            continue;
        }
        changed += 1;
        for (j, (&left, &right)) in left.iter().zip(right).enumerate() {
            if left == right {
                continue;
            }
            let at = i * metadata.sector + j;
            let Some(extent) = metadata
                .extents
                .iter()
                .find(|extent| extent.bytes.contains(&at))
            else {
                if unexpected_sectors.insert(i) && problems.len() < 5 {
                    problems.push(format!(
                        "Unmapped LBA {i}, first changed byte {at}, sector prefix {:02x?}",
                        &before[i * metadata.sector..i * metadata.sector + 4],
                    ));
                }
                continue;
            };
            if extent.journal {
                continue;
            }
            if let Some(size) = extent.record_size {
                let logical = extent.logical + at - extent.bytes.start;
                let record = logical / size;
                if !extent.index && metadata.transaction_records.contains(&record) {
                    continue;
                }
                let offset = logical % size;
                if let Some(start) = at.checked_sub(offset) {
                    let end = start + size;
                    if start >= extent.bytes.start && end <= extent.bytes.end {
                        let key = (extent.index, start);
                        if verified_records.contains(&key) || unexpected_records.contains(&key) {
                            continue;
                        }
                        let a = record_with_applied_fixups(&before[start..end], extent.index);
                        let b = record_with_applied_fixups(&after[start..end], extent.index);
                        let mut root_problem = None;
                        let comparison = a.as_ref().zip(b.as_ref()).and_then(|(a, b)| {
                            if a == b {
                                Some([0; 3])
                            } else if extent.index && extent.stream == "directory 5 $I30" {
                                match root_index_caches(&ntfs, &mut reader, start, a, b) {
                                    Ok(names) => {
                                        root_cache_names.extend(names);
                                        Some([0; 3])
                                    }
                                    Err(error) => {
                                        root_problem = Some(error);
                                        None
                                    }
                                }
                            } else {
                                let entries = metadata.txf_index_entries.get(&start)?;
                                compare_txf_index_entries(a, b, entries, |entry| {
                                    txf_data_length(&ntfs, &mut reader, &metadata, entry)
                                })
                            }
                        });
                        if let Some(advanced) = comparison {
                            verified_records.insert(key);
                            for (sum, count) in txf_index_fields.iter_mut().zip(advanced) {
                                *sum += count;
                            }
                        } else if unexpected_records.insert(key) && problems.len() < 5 {
                            let name = metadata
                                .names
                                .get(&record)
                                .filter(|_| !extent.index)
                                .map_or("", String::as_str);
                            let detail = match (a, b) {
                                (Some(a), Some(b)) => {
                                    let mut detail = normalized_differences(&a, &b);
                                    if let Some(entries) = metadata.txf_index_entries.get(&start) {
                                        let times = entries
                                            .iter()
                                            .flat_map(|entry| {
                                                let at = entry.offset;
                                                timestamp_changes(&a, &b, at).map(
                                                    move |(field, a, b)| {
                                                        let label =
                                                            ["born", "write", "change", "access"]
                                                                [field];
                                                        format!(
                                                            "{label}@{}:{a:x}>{b:x}",
                                                            at + field * 8
                                                        )
                                                    },
                                                )
                                            })
                                            .collect::<Vec<_>>();
                                        detail.push_str(&format!(
                                            "\nTxF timestamps: {}",
                                            times.join(" ")
                                        ));
                                    }
                                    if let Some(problem) = root_problem {
                                        detail.push_str(&format!("\n{problem}"));
                                    }
                                    detail
                                }
                                _ => "invalid record protection sequence".into(),
                            };
                            problems.push(format!(
                                "{} #{record} {name} at LBA {}: {detail}",
                                extent.stream,
                                start / metadata.sector
                            ));
                        }
                        continue;
                    }
                }
            }
            if unexpected_sectors.insert(i) && problems.len() < 5 {
                problems.push(format!(
                    "LBA {i} in {}, first changed byte {at}: {left:02x}>{right:02x}",
                    extent.stream
                ));
            }
        }
    }
    if !unexpected_records.is_empty() || !unexpected_sectors.is_empty() {
        return Err(format!(
            "{changed} changed sectors. Unexplained records {}, other LBAs {}.\n{}",
            unexpected_records.len(),
            unexpected_sectors.len(),
            problems.join("\n")
        ));
    }
    let index_records = verified_records.iter().filter(|(index, _)| *index).count();
    Ok(SourceDelta {
        sectors: changed,
        mft_records: verified_records.len() - index_records,
        index_records,
        txf_index_fields,
        root_cache_names,
    })
}

#[cfg(test)]
#[test]
fn requires_every_protected_payload_byte_to_match() {
    fn file_record(sequence: u16, lsn: u64) -> Vec<u8> {
        let mut record = vec![0u8; 4096];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&40u16.to_le_bytes());
        record[6..8].copy_from_slice(&9u16.to_le_bytes());
        record[8..16].copy_from_slice(&lsn.to_le_bytes());
        record[20..22].copy_from_slice(&64u16.to_le_bytes());
        record[40..42].copy_from_slice(&sequence.to_le_bytes());
        for index in 1..=8 {
            let saved = 40 + index * 2;
            record[saved..saved + 2].copy_from_slice(&(index as u16).to_le_bytes());
            let end = index * 512;
            record[end - 2..end].copy_from_slice(&sequence.to_le_bytes());
        }
        record
    }
    let first = file_record(1, 99);
    let mut second = file_record(2, 100);
    let expected = record_with_applied_fixups(&first, false).unwrap();
    assert_eq!(
        record_with_applied_fixups(&second, false),
        Some(expected.clone())
    );
    second[44] ^= 1;
    assert_ne!(record_with_applied_fixups(&second, false), Some(expected));
    second[1022] ^= 1;
    assert!(record_with_applied_fixups(&second, false).is_none());

    let mut first_index = file_record(10, 101);
    let mut second_index = file_record(11, 102);
    for record in [&mut first_index, &mut second_index] {
        record[..4].copy_from_slice(b"INDX");
        record[24..28].copy_from_slice(&40u32.to_le_bytes());
    }
    assert_eq!(
        record_with_applied_fixups(&first_index, true),
        record_with_applied_fixups(&second_index, true),
    );
    second_index[72] ^= 1;
    assert_ne!(
        record_with_applied_fixups(&first_index, true),
        record_with_applied_fixups(&second_index, true),
    );
}

#[cfg(test)]
#[test]
fn permits_only_forward_txf_index_timestamps_and_verified_length() {
    let entries = [
        TxfIndexEntry {
            offset: 24,
            file: 32,
            sequence: 8,
        },
        TxfIndexEntry {
            offset: 88,
            file: 33,
            sequence: 9,
        },
    ];
    let compare = |before: &[u8], after: &[u8]| {
        compare_txf_index_entries(before, after, &entries, |entry| {
            (entry.file == 32 && entry.sequence == 8).then_some(65536)
        })
    };
    let mut before = [0; 160];
    before[32..40].copy_from_slice(&10u64.to_le_bytes());
    let mut after = before;
    after[32..40].copy_from_slice(&11u64.to_le_bytes());
    after[104..112].copy_from_slice(&12u64.to_le_bytes());
    assert_eq!(compare(&before, &after), Some([1, 1, 0]));
    after[64..72].copy_from_slice(&65536u64.to_le_bytes());
    assert_eq!(compare(&before, &after), Some([1, 1, 1]));
    after[64] = 1;
    assert!(compare(&before, &after).is_none());
    after[64] = 0;
    after[128..136].copy_from_slice(&65536u64.to_le_bytes());
    assert!(compare(&before, &after).is_none());
    after[128..136].fill(0);
    before[64] = 1;
    assert!(compare(&before, &after).is_none());
    before[64] = 0;
    after[112] = 1;
    assert!(compare(&before, &after).is_none());
    after[112] = 0;
    after[24] = 1;
    assert!(compare(&before, &after).is_none());
    after[24] = 0;
    after[56] = 1;
    assert!(compare(&before, &after).is_none());
    after[56] = 0;
    after[32..40].copy_from_slice(&9u64.to_le_bytes());
    assert!(compare(&before, &after).is_none());
}

fn confirm_mounted(
    selected: &Selection,
    source: &data_windows::Resolved,
    number: u32,
) -> Result<(), String> {
    let part = reallocation_checks::metadata_partition(
        A,
        Some("DSNTCOPY"),
        SOURCE_AT / MIB,
        SOURCE_BYTES / MIB,
    )?;
    let [volume] = source.volumes.as_slice() else {
        return Err("Expected one A fixture volume".into());
    };
    if part.number != number || part.drive_letter.as_deref() != Some("A") {
        return Err("The A source mount point differs from the fixture".into());
    }
    let root = Dir::open_ambient_dir(r"A:\", cap_std::ambient_authority()).map_err(err)?;
    let mut file = root.open(ALIAS).map_err(err)?.into_std();
    data_windows::file_location(&file)?.confirm_partition(
        &selected.target,
        &selected.layout_key,
        volume,
        (source.disk.number, source.offset, source.size),
    )?;
    let expected = contents();
    let mut actual = Vec::new();
    (&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)
        .map_err(err)?;
    if actual != expected {
        return Err("The mounted fixture file does not match".into());
    }
    Ok(())
}

pub(in super::super) fn diagnose() -> Result<String, String> {
    let CompletedFixture {
        witness,
        source_selection,
        source,
        target,
    } = completed_fixture()?;
    let target_before = resize_clone_checks::hash(&target)?;
    confirm_mounted(&source_selection, &source, witness.source)?;
    let cancel = AtomicBool::new(false);
    let mut before = vec![0; SOURCE_BYTES as usize];
    let mut locked = vec![0; before.len()];
    let mut reader = data_windows::reader(&source)?;
    read_range(&mut reader, &mut before)?;
    read_range(&mut reader, &mut locked)?;
    if before != locked {
        return Err("Source bytes changed during the control reads before locking".into());
    }
    let catalogue = partitions::merge::catalogue_file_clone(&mut Cursor::new(&before), &cancel)?;
    sample(&catalogue)?;
    let metadata = metadata_map(&before, &catalogue)?;
    let locks = data_windows::state_locks(&source)?;
    let checked = data_windows::resolve_locked(&source_selection, false, &source, &locks)?;
    read_range(&mut data_windows::reader(&checked)?, &mut locked)?;
    drop(locks);
    let mut after = vec![0; before.len()];
    read_range(&mut reader, &mut after)?;
    for bytes in [&locked, &after] {
        let captured = partitions::merge::catalogue_file_clone(&mut Cursor::new(bytes), &cancel)?;
        catalogue.same_source(&captured)?;
        if metadata_map(bytes, &captured)? != metadata {
            return Err("NTFS metadata mappings changed during the lock cycle".into());
        }
    }
    verify_scope(&witness)?;
    let (_, target_after) = resolved(B, witness.target, witness.target_at, TARGET_BYTES, false)?;
    if resize_clone_checks::hash(&target_after)? != target_before {
        return Err("The offline B clone changed during the lock diagnosis".into());
    }
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let hashes = [hash(&before), hash(&locked), hash(&after)];
    let released = if after == locked {
        "same as locked".into()
    } else {
        delta(&before, &after, &metadata)
    };
    Ok(format!(
        "A/{} lock cycle. Control reads matched.\nBefore -> locked: {}\nLocked -> released: {}\nBefore -> released: {}\nSource SHA-256 prefixes: {} / {} / {}\nOriginal baseline prefix: {}\nSource catalogue, GPTs and edges match. B stayed offline; digest unchanged.\nB SHA-256 {target_before}",
        witness.source,
        delta(&before, &locked, &metadata),
        delta(&locked, &after, &metadata),
        released,
        &hashes[0][..12],
        &hashes[1][..12],
        &hashes[2][..12],
        witness.source_digest.get(..12).ok_or("Invalid witness digest")?,
    ))
}

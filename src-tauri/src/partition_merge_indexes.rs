//! Bind portable NTFS metadata to the volume's object and reparse indexes.
use super::{
    check, err, is_extend_metadata,
    metadata::{OBJECT_ID, REPARSE},
    IndexReader, Node, Record, RecordId, MAX_ATTRIBUTES, MAX_LINKS,
};
use ntfs::{
    indexes::{NtfsIndexEntryKey, NtfsIndexEntryType},
    types::NtfsPosition,
    NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags, NtfsIndex, NtfsReadSeek,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek},
    sync::atomic::AtomicBool,
};

const INVALID: &str = "NTFS 对象 ID 或重解析点索引与活动文件记录不一致。";

#[derive(Clone, Copy)]
enum Kind {
    Object,
    Reparse,
}

#[derive(Clone, Debug)]
struct MetadataIndex;

#[derive(Debug)]
enum Key {
    Object([u8; 16]),
    Reparse([u8; 12]),
}

impl NtfsIndexEntryType for MetadataIndex {
    type KeyType = Key;
}

impl NtfsIndexEntryKey for Key {
    fn key_from_slice(slice: &[u8], position: NtfsPosition) -> ntfs::Result<Self> {
        match slice.len() {
            16 => Ok(Self::Object(slice.try_into().unwrap())),
            12 => Ok(Self::Reparse(slice.try_into().unwrap())),
            _ => Err(NtfsError::InvalidIndexEntryDataRange {
                position,
                range: 0..16,
                size: slice.len() as u16,
            }),
        }
    }
}

fn reference(bytes: &[u8]) -> RecordId {
    let value = u64::from_le_bytes(bytes.try_into().unwrap());
    RecordId(value & 0x0000_ffff_ffff_ffff, (value >> 48) as u16)
}

pub(super) fn verify<R: Read + Seek>(
    reader: &mut R,
    extend: &NtfsFile<'_>,
    records: &BTreeMap<RecordId, Record>,
    nodes: &mut BTreeMap<RecordId, Node>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let extend_id = RecordId::of(extend);
    for (kind, file_name, index_name, attribute) in [
        (Kind::Object, "$ObjId", "$O", OBJECT_ID),
        (Kind::Reparse, "$Reparse", "$R", REPARSE),
    ] {
        let mut candidates = records.iter().filter(|(_, record)| {
            record.names.iter().any(|name| {
                name.parent == (extend_id.0, extend_id.1) && name.display() == file_name
            })
        });
        let expected = nodes
            .values()
            .filter(|node| node.metadata.contains_key(&attribute))
            .count();
        let Some((&index_id, index_record)) = candidates.next() else {
            if expected != 0 {
                return Err(INVALID.into());
            }
            continue;
        };
        if index_record.directory
            || candidates.next().is_some()
            || !is_extend_metadata(index_id, extend_id, records)
        {
            return Err(INVALID.into());
        }
        let fs = extend.ntfs();
        let file = fs.file(reader, index_id.0).map_err(err)?;
        if RecordId::of(&file) != index_id {
            return Err(INVALID.into());
        }
        let mut found = BTreeSet::new();
        let mut object_ids = BTreeSet::new();
        walk(reader, &file, index_name, cancel, |reader, entry| {
            let raw = entry.record_bytes();
            let child = 8 * usize::from(entry.subnode_vcn().is_some());
            let data_offset = u16::from_le_bytes(raw[..2].try_into().unwrap());
            let data_length = u16::from_le_bytes(raw[2..4].try_into().unwrap());
            let (id, key, extended) = match (kind, entry.key().transpose().map_err(err)?) {
                (Kind::Object, Some(Key::Object(key)))
                    if raw.len() == 88 + child && data_offset == 32 && data_length == 56 =>
                {
                    if key == [0; 16] || !object_ids.insert(key) {
                        return Err(INVALID.into());
                    }
                    (reference(&raw[32..40]), &raw[16..32], &raw[40..88])
                }
                (Kind::Reparse, Some(Key::Reparse(key)))
                    if raw.len() == 32 + child
                        && matches!(data_offset, 0 | 28)
                        && data_length == 0 =>
                {
                    (reference(&key[4..]), &raw[16..20], &[][..])
                }
                _ => return Err(INVALID.into()),
            };
            if !found.insert(id) {
                return Err(INVALID.into());
            }
            if let Some(node) = nodes.get_mut(&id) {
                let value = node.metadata.get_mut(&attribute).ok_or(INVALID)?;
                if !value.starts_with(key) {
                    return Err(INVALID.into());
                }
                if matches!(kind, Kind::Object) {
                    if value.len() == key.len() {
                        value.extend_from_slice(extended);
                    } else if &value[key.len()..] != extended {
                        return Err(INVALID.into());
                    }
                }
            } else if id.0 < 16 {
                let reserved = fs.file(reader, id.0).map_err(err)?;
                if RecordId::of(&reserved) != id
                    || !reserved.flags().contains(NtfsFileFlags::IN_USE)
                {
                    return Err(INVALID.into());
                }
            } else if !is_extend_metadata(id, extend_id, records) {
                return Err(INVALID.into());
            }
            Ok(())
        })?;
        if found.iter().filter(|id| nodes.contains_key(id)).count() != expected {
            return Err(INVALID.into());
        }
    }
    Ok(())
}

fn walk<R: Read + Seek>(
    reader: &mut R,
    file: &NtfsFile<'_>,
    name: &str,
    cancel: &AtomicBool,
    mut visit: impl FnMut(
        &mut IndexReader<'_, R>,
        ntfs::NtfsIndexEntry<'_, MetadataIndex>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = IndexReader {
        reader,
        remaining: 65536 + MAX_LINKS * 96,
        cancel,
    };
    let mut root = None;
    let mut allocation = None;
    let mut attributes = file.attributes();
    let mut count = 0;
    while let Some(item) = attributes.next(&mut reader) {
        count += 1;
        if count > MAX_ATTRIBUTES {
            return Err(INVALID.into());
        }
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.name().map_err(err)? != name {
            continue;
        }
        match attr.ty().map_err(err)? {
            NtfsAttributeType::IndexRoot if attr.is_resident() && attr.flags().is_empty() => {
                let mut header = [0; 8];
                attr.value(&mut reader)
                    .map_err(err)?
                    .read_exact(&mut reader, &mut header)
                    .map_err(err)?;
                if header != [0, 0, 0, 0, 19, 0, 0, 0] || root.replace(item).is_some() {
                    return Err(INVALID.into());
                }
            }
            NtfsAttributeType::IndexAllocation
                if !attr.is_resident() && attr.flags().is_empty() =>
            {
                if allocation.replace(item).is_some() {
                    return Err(INVALID.into());
                }
            }
            NtfsAttributeType::Bitmap if attr.flags().is_empty() => (),
            _ => return Err(INVALID.into()),
        }
    }
    let index = NtfsIndex::<MetadataIndex>::new(root.ok_or(INVALID)?, allocation).map_err(err)?;
    let size = index.index_record_size();
    if size < u32::from(file.ntfs().sector_size()) || size > 65536 || !size.is_power_of_two() {
        return Err(INVALID.into());
    }
    let mut entries = index.entries();
    let mut count = 0;
    while let Some(entry) = entries.next(&mut reader) {
        check(cancel)?;
        count += 1;
        if count > MAX_LINKS {
            return Err(INVALID.into());
        }
        visit(&mut reader, entry.map_err(err)?)?;
    }
    Ok(())
}

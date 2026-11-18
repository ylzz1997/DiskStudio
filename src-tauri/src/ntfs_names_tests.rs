use super::*;
use crate::{
    recovery::{self, Candidate, ScanMode},
    recovery_tree::Catalog,
};
use std::io::Cursor;

const ORIGINAL: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn record_named(name: &str) -> (u64, usize, usize, u16) {
    let mut reader = Cursor::new(ORIGINAL);
    let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
    for n in 16..1024 {
        let Ok(file) = ntfs.file(&mut reader, n) else {
            continue;
        };
        if file
            .name(&mut reader, None, None)
            .is_some_and(|n| n.is_ok_and(|n| n.name().to_string_lossy() == name))
        {
            return (
                n,
                file.position().value().unwrap().get() as usize,
                ntfs.file_record_size() as usize,
                file.sequence_number(),
            );
        }
    }
    panic!("missing fixture record {name}")
}
fn decoded(bytes: &[u8], at: usize, size: usize) -> Vec<u8> {
    let mut record = bytes[at..at + size].to_vec();
    let usa = u16_at(&record, 4) as usize;
    let sectors = u16_at(&record, 6) as usize - 1;
    let sector_size = u16_at(bytes, 11) as usize;
    for i in 1..=sectors {
        let original = [record[usa + 2 * i], record[usa + 2 * i + 1]];
        let tail = i * sector_size - 2;
        assert_eq!(&record[tail..tail + 2], &record[usa..usa + 2]);
        record[tail..tail + 2].copy_from_slice(&original);
    }
    record
}
fn attributes(record: &[u8]) -> Vec<Vec<u8>> {
    let mut attrs = vec![];
    let mut at = u16_at(record, 20) as usize;
    while u32_at(record, at) != u32::MAX {
        let len = u32_at(record, at + 4) as usize;
        assert!(len >= 24 && at + len <= record.len());
        attrs.push(record[at..at + len].to_vec());
        at += len;
    }
    attrs
}
fn write_record(bytes: &mut [u8], at: usize, mut header: Vec<u8>, attrs: Vec<Vec<u8>>) {
    let start = u16_at(&header, 20) as usize;
    header[start..].fill(0);
    let mut end = start;
    for attr in attrs {
        assert!(end + attr.len() + 8 <= header.len());
        header[end..end + attr.len()].copy_from_slice(&attr);
        end += attr.len();
    }
    header[end..end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    header[24..28].copy_from_slice(&((end + 8) as u32).to_le_bytes());
    let usa = u16_at(&header, 4) as usize;
    let sectors = u16_at(&header, 6) as usize - 1;
    let sector_size = u16_at(bytes, 11) as usize;
    for i in 1..=sectors {
        let tail = i * sector_size - 2;
        let original = [header[tail], header[tail + 1]];
        let marker = [header[usa], header[usa + 1]];
        header[usa + 2 * i..usa + 2 * i + 2].copy_from_slice(&original);
        header[tail..tail + 2].copy_from_slice(&marker);
    }
    bytes[at..at + header.len()].copy_from_slice(&header);
}
fn resident(ty: u32, instance: u16, value: &[u8]) -> Vec<u8> {
    let mut attr = vec![0; (24 + value.len() + 7) / 8 * 8];
    attr[..4].copy_from_slice(&ty.to_le_bytes());
    let len = attr.len() as u32;
    attr[4..8].copy_from_slice(&len.to_le_bytes());
    attr[14..16].copy_from_slice(&instance.to_le_bytes());
    attr[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    attr[20..22].copy_from_slice(&24u16.to_le_bytes());
    attr[24..24 + value.len()].copy_from_slice(value);
    attr
}
fn name_attr(name: &Name, instance: u16) -> Vec<u8> {
    let mut value = vec![0; 66];
    value[..8].copy_from_slice(&(name.parent.0 | (u64::from(name.parent.1) << 48)).to_le_bytes());
    value[64] = (name.raw.len() / 2) as u8;
    value[65] = name.namespace;
    value.extend_from_slice(&name.raw);
    resident(0x30, instance, &value)
}
fn add_names(bytes: &mut [u8], names: &[Name]) {
    let (_, at, size, _) = record_named("file-with-12345");
    let mut header = decoded(bytes, at, size);
    let mut attrs = attributes(&header);
    for (i, name) in names.iter().enumerate() {
        attrs.push(name_attr(name, 100 + i as u16));
    }
    attrs.sort_by_key(|a| u32_at(a, 0));
    header[18..20].copy_from_slice(&(1 + names.len() as u16).to_le_bytes());
    write_record(bytes, at, header, attrs);
}
fn scan(bytes: &[u8], mode: ScanMode) -> (Vec<Candidate>, Catalog, String) {
    let mut files = vec![];
    let mut catalog = Catalog::default();
    let records = std::cell::RefCell::new(&mut catalog);
    let summary = recovery::scan_filesystem_owned(
        Cursor::new(bytes.to_vec()),
        mode,
        Default::default(),
        |_, _| Ok(()),
        |c| {
            records.borrow_mut().add_file(&c);
            files.push(c);
        },
        |d| records.borrow_mut().insert(d),
    )
    .unwrap();
    (files, catalog, summary)
}
fn extract(bytes: &[u8], candidate: &Candidate) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let result = recovery::extract_owned(
        Cursor::new(bytes.to_vec()),
        candidate,
        &mut out,
        u64::MAX,
        Default::default(),
        |_| Ok(()),
    );
    if result.is_err() {
        assert!(out.is_empty(), "identity failure must precede output");
    }
    result.map(|_| out)
}

#[test]
fn aliases_in_different_parents_short_names_and_empty_directories_survive_scan_and_export() {
    let (directory, _, _, sequence) = record_named("1");
    let mut alias = Name::fixture("别名.txt");
    alias.parent = (directory, sequence);
    let mut dos = Name::fixture("ALIAS~1.TXT");
    dos.namespace = 2;
    let mut bytes = ORIGINAL.to_vec();
    add_names(&mut bytes, &[alias, dos]);
    let (files, catalog, summary) = scan(&bytes, ScanMode::AllFiles);
    let selected: Vec<_> = files
        .iter()
        .filter(|c| ["file-with-12345", "别名.txt", "ALIAS~1.TXT"].contains(&c.name.as_str()))
        .cloned()
        .collect();
    assert_eq!(selected.len(), 3);
    assert!(summary.contains("additional names"), "{summary}");
    for c in &selected {
        assert_eq!(extract(&bytes, c).unwrap(), b"12345");
    }
    // Persist real multi-name locators, not just display strings. Reloading must
    // keep independent parent/name identities and extraction evidence.
    let mut checkpoint = crate::ntfs_scan::Checkpoint::default();
    let mut persisted = vec![];
    let mut saved_tree = Catalog::default();
    crate::ntfs_scan::continue_scan(
        &mut Cursor::new(&bytes),
        ScanMode::AllFiles,
        &mut checkpoint,
        |_, _| Ok(()),
        |c| persisted.push(c),
        |d| saved_tree.insert(d),
    )
    .unwrap();
    let source = crate::data_tasks::Selection {
        target: crate::partitions::Target("a".repeat(64)),
        layout_key: "b".repeat(64),
        partition: Some(2),
    };
    let saved = crate::recovery_checkpoint::Progress::capture(
        source.clone(),
        crate::signature_checkpoint::Binding::capture(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            4096,
            512,
        )
        .unwrap(),
        crate::recovery_checkpoint::Cursor::Ntfs {
            mode: ScanMode::AllFiles,
            cursor: checkpoint,
        },
        &persisted,
        &saved_tree,
    )
    .unwrap();
    let loaded = crate::recovery_checkpoint::decode(
        &crate::recovery_checkpoint::encode(saved).unwrap(),
        &source,
    )
    .unwrap();
    let (restored, restored_tree) = loaded.validate(&source).unwrap();
    assert_eq!(restored_tree.snapshot(), saved_tree.snapshot());
    for original in &selected {
        let c = restored.iter().find(|c| c.name == original.name).unwrap();
        assert_eq!(c.directory, original.directory);
        assert_eq!(extract(&bytes, c).unwrap(), b"12345");
    }
    let alias = selected.iter().find(|c| c.name == "别名.txt").unwrap();
    assert_eq!(
        catalog
            .path(alias.directory.as_deref().unwrap())
            .unwrap()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["根目录", "many_subdirs", "1"]
    );
    assert!(selected
        .iter()
        .find(|c| c.name == "ALIAS~1.TXT")
        .unwrap()
        .directory_note
        .contains("DOS 短名称"));
    let plan =
        crate::recovery_export::plan(&selected, &catalog, crate::recovery_export::Layout::Tree)
            .unwrap();
    assert_eq!(plan.entries.len(), 3);
    assert_eq!(plan.total_bytes, 15); // independent copies, not deduplicated links
    let (_, empty_catalog, _) = scan(ORIGINAL, ScanMode::AllFiles);
    let id = format!("ntfs-{directory}-{sequence}");
    let parent = empty_catalog
        .path(&id)
        .unwrap()
        .iter()
        .rev()
        .nth(1)
        .unwrap()
        .id
        .clone();
    let page = serde_json::to_value(empty_catalog.folders(Some(&parent), 0).unwrap()).unwrap();
    assert!(page["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["id"] == id && n["files"] == 0));
}

#[test]
fn raw_name_namespace_and_parent_changes_reject_extraction_without_writing() {
    let mut bytes = ORIGINAL.to_vec();
    let mut alias = Name::fixture("变化.txt");
    add_names(&mut bytes, &[alias.clone()]);
    let (files, _, _) = scan(&bytes, ScanMode::AllFiles);
    let candidate = files.iter().find(|c| c.name == "变化.txt").unwrap();
    for variant in 0..4 {
        let mut changed = ORIGINAL.to_vec();
        let mut name = alias.clone();
        match variant {
            0 => name.raw[0] ^= 1,
            1 => name.namespace = 0,
            2 => name.parent.0 += 1,
            _ => name.parent.1 += 1,
        }
        add_names(&mut changed, &[name]);
        assert!(extract(&changed, candidate).is_err(), "variant {variant}");
    }
    assert!(extract(ORIGINAL, candidate).is_err());
    // Different unpaired UTF-16 surrogates have the same display string, but
    // must remain separate identities and cannot substitute for one another.
    alias.raw = 0xd800u16.to_le_bytes().to_vec();
    let mut other = alias.clone();
    other.raw = 0xd801u16.to_le_bytes().to_vec();
    let mut raw = ORIGINAL.to_vec();
    add_names(&mut raw, &[alias, other]);
    let (files, _, _) = scan(&raw, ScanMode::AllFiles);
    assert_eq!(files.iter().filter(|c| c.name == "�").count(), 2);
}

fn extension_fixture(variant: u8) -> Vec<u8> {
    let (record, at, size, sequence) = record_named("file-with-12345");
    let (linked, link_at, link_size, link_sequence) = record_named("empty-file");
    let mut bytes = ORIGINAL.to_vec();
    let mut header = decoded(&bytes, link_at, link_size);
    let base = record | (u64::from(sequence + u16::from(variant == 2)) << 48);
    header[32..40].copy_from_slice(&base.to_le_bytes());
    write_record(
        &mut bytes,
        link_at,
        header,
        vec![name_attr(&Name::fixture("扩展记录别名.txt"), 99)],
    );
    let mut entry = vec![0; 32];
    entry[..4].copy_from_slice(&0x30u32.to_le_bytes());
    entry[4..6].copy_from_slice(&32u16.to_le_bytes());
    entry[16..24].copy_from_slice(
        &(linked | (u64::from(link_sequence + u16::from(variant == 1)) << 48)).to_le_bytes(),
    );
    entry[24..26].copy_from_slice(&99u16.to_le_bytes());
    if variant == 3 {
        let second = entry.clone();
        entry.extend(second);
    }
    let header = decoded(&bytes, at, size);
    let mut attrs = attributes(&header);
    attrs.push(resident(0x20, 90, &entry));
    attrs.sort_by_key(|a| u32_at(a, 0));
    write_record(&mut bytes, at, header, attrs);
    bytes
}

#[test]
fn attribute_list_names_validate_extension_sequence_owner_and_duplicate_references() {
    let bytes = extension_fixture(0);
    let (files, _, _) = scan(&bytes, ScanMode::AllFiles);
    let aliases: Vec<_> = files
        .iter()
        .filter(|c| c.name == "扩展记录别名.txt")
        .collect();
    assert_eq!(
        aliases.len(),
        1,
        "extension record must not appear as independent file"
    );
    assert_eq!(extract(&bytes, aliases[0]).unwrap(), b"12345");
    for variant in 1..=3 {
        let changed = extension_fixture(variant);
        assert!(extract(&changed, aliases[0]).is_err());
        let (changed_files, _, _) = scan(&changed, ScanMode::AllFiles);
        assert!(!changed_files.iter().any(|c| c.name == "扩展记录别名.txt"));
    }
}

#[test]
fn deleted_aliases_remain_deleted_and_invalid_name_metadata_is_not_accepted() {
    let mut bytes = ORIGINAL.to_vec();
    add_names(&mut bytes, &[Name::fixture("已删除别名.txt")]);
    let (_, at, size, _) = record_named("file-with-12345");
    let mut header = decoded(&bytes, at, size);
    let attrs = attributes(&header);
    header[22] &= !1;
    write_record(&mut bytes, at, header, attrs);
    let (files, _, _) = scan(&bytes, ScanMode::Deleted);
    let aliases: Vec<_> = files
        .iter()
        .filter(|c| ["file-with-12345", "已删除别名.txt"].contains(&c.name.as_str()))
        .collect();
    assert_eq!(aliases.len(), 2);
    for c in aliases {
        assert!(c.deleted);
        assert_eq!(extract(&bytes, c).unwrap(), b"12345");
    }
    for variant in 0..3 {
        let mut changed = ORIGINAL.to_vec();
        let mut bad = Name::fixture("bad");
        match variant {
            0 => bad.namespace = 7,
            1 => bad.raw = vec![0, 0],
            _ => bad.raw.clear(),
        }
        add_names(&mut changed, &[bad]);
        let (files, _, _) = scan(&changed, ScanMode::AllFiles);
        assert!(!files.iter().any(|c| c.name == "file-with-12345"));
    }
}

#[test]
fn attribute_list_length_count_and_truncated_tail_are_bounded() {
    let mut entry = vec![0; 32];
    entry[..4].copy_from_slice(&0x30u32.to_le_bytes());
    entry[4..6].copy_from_slice(&32u16.to_le_bytes());
    entry[16..24].copy_from_slice(&(19u64 | (2u64 << 48)).to_le_bytes());
    entry[24..26].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(name_references(&entry).unwrap(), [(19, 2, 9)]);
    for n in 1..32 {
        assert!(name_references(&entry[..n]).is_err());
    }
    for variant in 0..6 {
        let mut changed = entry.clone();
        match variant {
            0 => changed[4] = 0,
            1 => changed[4] = 40,
            2 => changed[4] = 31,
            3 => changed[6] = 1,
            4 => changed[8] = 1,
            _ => changed.push(0),
        }
        assert!(name_references(&changed).is_err(), "variant {variant}");
    }
    assert!(name_references(&entry.repeat(2)).is_err());
    entry[..4].copy_from_slice(&0x80u32.to_le_bytes()); // other attribute, still bounded
    assert!(name_references(&entry.repeat(MAX_ATTRIBUTES))
        .unwrap()
        .is_empty());
    assert!(name_references(&entry.repeat(MAX_ATTRIBUTES + 1)).is_err());
    assert!(name_references(&vec![0; MAX_LIST_BYTES as usize + 1]).is_err());
}

#[test]
fn ambiguous_directory_names_are_unknown_but_dos_alias_does_not_replace_long_name() {
    let (record, at, size, _) = record_named("1");
    for ambiguous in [false, true] {
        let mut bytes = ORIGINAL.to_vec();
        let header = decoded(&bytes, at, size);
        let mut attrs = attributes(&header);
        let original = attrs.iter().find(|a| u32_at(a, 0) == 0x30).unwrap();
        let value_at = u16_at(original, 20) as usize;
        let parent = u64::from_le_bytes(original[value_at..value_at + 8].try_into().unwrap());
        let mut name = Name::fixture("ALIAS");
        name.parent = (parent & 0x0000_ffff_ffff_ffff, (parent >> 48) as u16);
        name.namespace = if ambiguous { 1 } else { 2 };
        attrs.push(name_attr(&name, 99));
        attrs.sort_by_key(|a| u32_at(a, 0));
        write_record(&mut bytes, at, header, attrs);
        let mut reader = Cursor::new(bytes);
        let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
        let file = ntfs.file(&mut reader, record).unwrap();
        let result = directory(&mut reader, &file);
        if ambiguous {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().display(), "1");
        }
    }
}

#[test]
fn cancellation_and_source_read_failure_do_not_publish_or_extract_false_success() {
    let mut files = vec![];
    let result = recovery::scan_filesystem_owned(
        Cursor::new(ORIGINAL.to_vec()),
        ScanMode::AllFiles,
        Default::default(),
        |_, _| Err("cancelled".into()),
        |c| files.push(c),
        |_| Ok(()),
    );
    assert_eq!(result.unwrap_err(), "cancelled");
    assert!(files.is_empty());
    let (files, _, _) = scan(ORIGINAL, ScanMode::AllFiles);
    let candidate = files.iter().find(|c| c.name == "file-with-12345").unwrap();
    struct Failure(Cursor<Vec<u8>>, u64);
    impl Read for Failure {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() <= self.1 && self.0.position() + output.len() as u64 > self.1 {
                return Err(std::io::Error::other("injected source read failure"));
            }
            self.0.read(output)
        }
    }
    impl Seek for Failure {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(from)
        }
    }
    let (_, at, _, _) = record_named("file-with-12345");
    let mut output = Vec::new();
    let result = recovery::extract_owned(
        Failure(Cursor::new(ORIGINAL.to_vec()), at as u64),
        candidate,
        &mut output,
        u64::MAX,
        Default::default(),
        |_| Ok(()),
    );
    assert!(result.is_err());
    assert!(output.is_empty());
}

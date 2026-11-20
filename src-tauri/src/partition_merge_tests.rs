use super::*;

fn path(path: &[&str]) -> RelativePath {
    path.iter()
        .map(|name| name.encode_utf16().collect())
        .collect()
}

fn name(parent: RecordId, name: &str, namespace: u8) -> ntfs_names::Name {
    ntfs_names::Name {
        parent: (parent.0, parent.1),
        namespace,
        raw: name.encode_utf16().flat_map(u16::to_le_bytes).collect(),
    }
}

const ROOT_ID: RecordId = RecordId(5, 5);
const FOLDER: RecordId = RecordId(20, 7);
const FILE: RecordId = RecordId(21, 9);

fn records() -> BTreeMap<RecordId, Record> {
    BTreeMap::from([
        (
            ROOT_ID,
            Record {
                directory: true,
                attributes: NtfsFileAttributeFlags::empty(),
                names: vec![],
            },
        ),
        (
            FOLDER,
            Record {
                directory: true,
                attributes: NtfsFileAttributeFlags::empty(),
                names: vec![name(ROOT_ID, "folder", 1)],
            },
        ),
        (
            FILE,
            Record {
                directory: false,
                attributes: NtfsFileAttributeFlags::empty(),
                names: vec![
                    name(FOLDER, "Long Name", 1),
                    name(FOLDER, "LONGNA~1", 2),
                    name(ROOT_ID, "other link", 3),
                ],
            },
        ),
    ])
}

#[test]
fn namespace_preserves_hard_links_and_checks_significant_short_name_bindings() {
    let namespace = build_namespace(&records(), ROOT_ID, true, None, || Ok(())).unwrap();
    assert_eq!(namespace.names[&path(&["folder", "Long Name"])], FILE);
    assert_eq!(namespace.names[&path(&["other link"])], FILE);
    assert_eq!(namespace.short_names[&path(&["folder", "LONGNA~1"])], FILE);
    assert_eq!(namespace.children[&FOLDER].len(), 2);
    assert!(build_namespace(&records(), FOLDER, false, None, || Ok(())).is_err());
}

#[test]
fn namespace_fails_on_unknown_parents_directory_cycles_and_device_names() {
    for bad in ["..", "bad:stream", "CON.txt", "lpt¹", "x/second", "x. "] {
        assert!(validate_component(&bad.encode_utf16().collect::<Vec<_>>()).is_err());
    }
    let mut entries = records();
    entries
        .get_mut(&FILE)
        .unwrap()
        .names
        .push(name(RecordId(20, 8), "changed parent", 3));
    assert!(build_namespace(&entries, ROOT_ID, true, None, || Ok(())).is_err());

    let mut entries = records();
    entries
        .get_mut(&FOLDER)
        .unwrap()
        .names
        .push(name(FOLDER, "cycle", 1));
    assert!(build_namespace(&entries, ROOT_ID, true, None, || Ok(())).is_err());
}

#[test]
fn metadata_is_excluded_only_beneath_the_bound_system_directory() {
    let extend = RecordId(EXTEND, 3);
    let usn = RecordId(31, 1);
    let mut entries = records();
    entries.insert(
        extend,
        Record {
            directory: true,
            attributes: NtfsFileAttributeFlags::SYSTEM,
            names: vec![name(ROOT_ID, "$Extend", 3)],
        },
    );
    entries.insert(
        usn,
        Record {
            directory: false,
            attributes: NtfsFileAttributeFlags::SYSTEM,
            names: vec![name(extend, "$UsnJrnl", 3)],
        },
    );
    let catalogue = build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).unwrap();
    assert!(!catalogue.names.values().any(|id| *id == usn));
    assert_eq!(catalogue.children[&extend].len(), 1);

    entries.get_mut(&usn).unwrap().attributes = NtfsFileAttributeFlags::empty();
    assert!(build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).is_err());
    let record = entries.get_mut(&usn).unwrap();
    record.attributes = NtfsFileAttributeFlags::SYSTEM;
    record.names[0].parent.1 += 1;
    assert!(build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).is_err());
}

#[test]
fn archive_only_transaction_logs_require_the_exact_reserved_path() {
    let extend = RecordId(EXTEND, 11);
    let manager = RecordId(27, 1);
    let log = RecordId(30, 1);
    let container = RecordId(34, 1);
    let mut entries = records();
    for (id, parent, leaf) in [
        (extend, ROOT_ID, "$Extend"),
        (manager, extend, "$RmMetadata"),
        (log, manager, "$TxfLog"),
    ] {
        entries.insert(
            id,
            Record {
                directory: true,
                attributes: NtfsFileAttributeFlags::SYSTEM,
                names: vec![name(parent, leaf, 0)],
            },
        );
    }
    let leaf = "$TxfLogContainer00000000000000000001";
    entries.insert(
        container,
        Record {
            directory: false,
            attributes: NtfsFileAttributeFlags::ARCHIVE,
            names: vec![name(log, leaf, 0)],
        },
    );
    assert!(is_extend_metadata(container, extend, &entries));
    let namespace = build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).unwrap();
    assert!(!namespace.names.values().any(|id| *id == container));
    assert_eq!(namespace.children[&log].len(), 1);

    entries.get_mut(&container).unwrap().names = vec![name(log, leaf, 1), name(log, "$TXFLO~1", 2)];
    let namespace = build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).unwrap();
    assert!(!namespace.names.values().any(|id| *id == container));
    assert_eq!(namespace.children[&log].len(), 2);
    entries.get_mut(&container).unwrap().names[1].parent.1 += 1;
    assert!(!is_extend_metadata(container, extend, &entries));
    entries.get_mut(&container).unwrap().names =
        vec![name(log, leaf, 1), name(log, "invalid short", 2)];
    assert!(!is_extend_metadata(container, extend, &entries));

    entries.get_mut(&container).unwrap().names = vec![name(log, "user.docx", 0)];
    assert!(build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).is_err());
    entries.get_mut(&container).unwrap().names =
        vec![name(log, "$TxfLog.blf", 0), name(ROOT_ID, "linked.bin", 0)];
    assert!(!is_extend_metadata(container, extend, &entries));
    entries.get_mut(&container).unwrap().names = vec![name(ROOT_ID, leaf, 0)];
    let namespace = build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).unwrap();
    assert_eq!(namespace.names[&path(&[leaf])], container);
    entries.get_mut(&container).unwrap().names = vec![name(log, leaf, 0)];
    entries.get_mut(&log).unwrap().names[0].parent.1 += 1;
    assert!(build_namespace(&entries, ROOT_ID, true, Some(extend), || Ok(())).is_err());
}

fn catalogue_fixture(shift: u64, security_id: u32) -> Catalogue {
    let names = vec![
        (path(&[]), ROOT_ID),
        (path(&["a"]), FILE),
        (path(&["b"]), FILE),
    ];
    let short_names = BTreeMap::from([(path(&["A~1"]), RecordId(FILE.0 + shift, FILE.1))]);
    let names = names
        .into_iter()
        .map(|(name, id)| (name, RecordId(id.0 + shift, id.1)))
        .collect();
    let nodes = [ROOT_ID, FILE]
        .into_iter()
        .map(|id| {
            let node = Node {
                index_compressed: false,
                link_version_flag: false,
                directory: id == ROOT_ID,
                attributes: 0x20,
                times: [8, 9, 10],
                checkpoint: (1024, Some(12)),
                root_txf_data: None,
                security: NodeSecurity::Indexed(security_id),
                metadata: BTreeMap::new(),
                streams: (id == FILE)
                    .then(|| Stream {
                        locator: ntfs_streams::Identity {
                            name: b"x\0".to_vec(),
                            signature: [shift as u8; 32],
                        },
                        size: 100,
                        sha256: [5; 32],
                        storage_flags: 0,
                        allocated_ranges: Vec::new(),
                    })
                    .into_iter()
                    .collect(),
            };
            (RecordId(id.0 + shift, id.1), node)
        })
        .collect();
    let mut descriptor = vec![0; 20];
    descriptor[0] = 1;
    descriptor[3] = 128;
    Catalogue {
        volume_serial: shift + 1,
        boot: [shift as u8; 32],
        names,
        short_names,
        nodes,
        security: BTreeMap::from([(security_id, security::Descriptor::read(descriptor).unwrap())]),
    }
}

#[test]
fn independent_copy_checks_bytes_metadata_and_hard_link_topology() {
    let source = catalogue_fixture(0, 256);
    let mut target = catalogue_fixture(100, 308);
    let copied_file = RecordId(FILE.0 + 100, FILE.1);
    assert!(source.verify_copy(&target).is_ok());
    assert!(source.same_source(&target).is_err());

    target.nodes.get_mut(&copied_file).unwrap().streams[0].sha256[0] ^= 1;
    assert!(source.verify_copy(&target).is_err());
    target.nodes.get_mut(&copied_file).unwrap().streams[0].sha256[0] ^= 1;
    target.nodes.get_mut(&copied_file).unwrap().streams[0].storage_flags = 0x8000;
    assert!(source.verify_copy(&target).is_err());
    target.nodes.get_mut(&copied_file).unwrap().streams[0].storage_flags = 0;
    target.names.insert(path(&["b"]), RecordId(891, 5));
    assert!(source.verify_copy(&target).is_err());
}

#[test]
fn separate_short_name_requires_one_long_link_in_its_parent() {
    let mut source = catalogue_fixture(0, 256);
    let cancel = AtomicBool::new(false);
    assert!(metadata::short_name_links(&source, &cancel).is_err());
    source.names.remove(&path(&["b"]));
    let aliases = metadata::short_name_links(&source, &cancel).unwrap();
    assert_eq!(aliases[&path(&["a"])], path(&["A~1"])[0]);
}

#[test]
fn same_source_checks_usn_and_stream_locator_even_if_contents_agree() {
    let before = catalogue_fixture(0, 256);
    let mut after = catalogue_fixture(0, 256);
    assert!(before.same_source(&after).is_ok());
    after.nodes.get_mut(&FILE).unwrap().checkpoint.1 = Some(40);
    assert!(before.verify_copy(&after).is_ok());
    assert!(before.same_source(&after).is_err());
    after.nodes.get_mut(&FILE).unwrap().checkpoint.1 = Some(12);
    after.nodes.get_mut(&FILE).unwrap().streams[0]
        .locator
        .signature[0] ^= 1;
    assert!(before.same_source(&after).is_err());
}

#[test]
fn root_txf_metadata_is_checked_on_the_source_and_stays_on_its_volume() {
    let mut before = catalogue_fixture(0, 256);
    let mut after = catalogue_fixture(0, 256);
    let root = before.names[&Vec::new()];
    before.nodes.get_mut(&root).unwrap().root_txf_data = Some([1; ROOT_TXF_DATA_SIZE]);
    assert!(before.verify_copy(&after).is_ok());
    assert!(before.same_source(&after).is_err());
    after.nodes.get_mut(&root).unwrap().root_txf_data = Some([1; ROOT_TXF_DATA_SIZE]);
    assert!(before.same_source(&after).is_ok());
}

#[test]
fn active_record_iteration_uses_only_set_bitmap_bits() {
    assert_eq!(
        active_records(&[0x81, 0x0a, 0]).collect::<Vec<_>>(),
        [0, 7, 9, 11]
    );
}

#[test]
fn finds_the_reserved_security_stream_without_case_folding() {
    let mut reader = std::io::Cursor::new(include_bytes!("../../tests/fixtures/ntfs-testfs1.img"));
    let ntfs = Ntfs::new(&mut reader).unwrap();
    let descriptors = read_security(
        &ntfs,
        &mut reader,
        &BTreeSet::from([0x100]),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!descriptors[&0x100].bytes().is_empty());
}

#[test]
fn reads_real_ntfs_structures_from_the_repository_image_in_memory() {
    let bytes = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
    let cancel = AtomicBool::new(false);
    let corrupt = catalogue(&mut std::io::Cursor::new(bytes), None, &cancel).unwrap_err();
    assert!(corrupt.contains("Update Sequence Number"), "{corrupt}");

    // Restore the known missing update sequence marker in the last active
    // record on a private in-memory copy of the repository's NTFS fixture.
    let mut repaired = bytes.to_vec();
    assert_eq!(&repaired[0x43c00..0x43c04], b"FILE");
    assert_eq!(&repaired[0x43c30..0x43c32], &[3, 0]);
    assert_eq!(&repaired[0x43ffe..0x44000], &[0, 0]);
    repaired[0x43ffe..0x44000].copy_from_slice(&[3, 0]);
    let mut reader = std::io::Cursor::new(&repaired);
    let tree = catalogue(&mut reader, None, &cancel).unwrap();
    let expected_name = path(&["file-with-12345"]);
    let file = *tree
        .names
        .iter()
        .find(|(name, _)| name.last() == expected_name.last())
        .unwrap()
        .1;
    let streams = &tree.nodes[&file].streams;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].size, 5);
    assert_eq!(&streams[0].sha256[..], &Sha256::digest(b"12345")[..]);
    assert!(!tree.descriptor(file).unwrap().is_empty());
    let mut transferred = vec![];
    assert_eq!(
        tree.transfer_stream(&mut reader, file, &[], &mut transferred, &cancel)
            .unwrap(),
        5
    );
    assert_eq!(transferred, b"12345");

    let (directory_path, &directory) = tree
        .names
        .iter()
        .find(|(path, id)| path.len() == 1 && tree.nodes[id].directory)
        .expect("fixture has a subdirectory in the root");
    let directory_name = &directory_path[0];
    let staged = catalogue(&mut reader, Some((directory, directory_name)), &cancel).unwrap();
    assert_eq!(staged.names[&Vec::new()], directory);
    let changed_name = "different".encode_utf16().collect::<Vec<_>>();
    assert!(catalogue(&mut reader, Some((directory, &changed_name)), &cancel).is_err());
    assert!(catalogue(
        &mut reader,
        Some((RecordId(directory.0, directory.1 + 1), directory_name)),
        &cancel
    )
    .is_err());
}

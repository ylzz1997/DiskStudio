use super::*;
use std::io::Cursor;
const IMAGE: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
fn live() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn directories() -> Vec<Directory> {
    let mut result = vec![];
    crate::recovery::scan_filesystem_owned(
        Cursor::new(IMAGE),
        crate::recovery::ScanMode::Existing,
        live(),
        |_, _| Ok(()),
        |_| {},
        |d| {
            result.push(d);
            Ok(())
        },
    )
    .unwrap();
    assert!(!result.is_empty());
    result
}
#[test]
fn ntfs_directory_metadata_comes_from_fresh_records_and_saved_display_is_not_authority() {
    for mut directory in directories() {
        let expected = read(Cursor::new(IMAGE), &directory, live()).unwrap();
        assert!(expected
            .created
            .as_ref()
            .and_then(|s| s.instant_ns)
            .is_some());
        let bytes = serde_json::to_vec(&directory).unwrap();
        let saved: Directory = serde_json::from_slice(&bytes).unwrap();
        assert!(saved.metadata.directory_proof.is_none());
        let actual = read(Cursor::new(IMAGE), &saved, live()).unwrap();
        assert_eq!(actual.created, expected.created);
        directory.metadata.windows_flags = Some(0xFFFF_FFFF);
        directory.metadata.created.as_mut().unwrap().instant_ns = Some(123);
        assert_eq!(
            read(Cursor::new(IMAGE), &directory, live())
                .unwrap()
                .created,
            expected.created
        );
        let mut forged = saved.clone();
        forged.parent = Some("ntfs-999-1".into());
        assert!(read(Cursor::new(IMAGE), &forged, live()).is_err());
        let mut forged = saved;
        forged.name.push('x');
        assert!(read(Cursor::new(IMAGE), &forged, live()).is_err());
    }
}
#[test]
fn changed_ntfs_volume_or_reused_directory_records_and_cancellation_are_rejected() {
    let directory = directories().remove(0);
    let Some(Proof::Ntfs { record, .. }) = directory.metadata.directory_proof.as_ref() else {
        panic!()
    };
    let mut reader = Cursor::new(IMAGE);
    let ntfs = ntfs::Ntfs::new(&mut reader).unwrap();
    let at = ntfs
        .file(&mut reader, *record)
        .unwrap()
        .position()
        .value()
        .unwrap()
        .get() as usize;
    for offset in [72, at + 16] {
        let mut bytes = IMAGE.to_vec();
        bytes[offset] ^= 1;
        assert!(read(Cursor::new(bytes), &directory, live()).is_err());
    }
    assert!(read(
        Cursor::new(IMAGE),
        &directory,
        Arc::new(AtomicBool::new(true))
    )
    .unwrap_err()
    .contains("取消"));
}
#[test]
fn old_directory_json_is_stable_and_internal_proofs_cannot_be_imported() {
    let value = serde_json::json!({"id":"root","parent":null,"name":"根目录","deleted":false});
    let d: Directory = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(d).unwrap(), value);
    let mut value = serde_json::to_value(&directories()[0]).unwrap();
    value["metadata"]["directoryProof"] = serde_json::json!({"record":1});
    assert!(serde_json::from_value::<Directory>(value).is_err());
}

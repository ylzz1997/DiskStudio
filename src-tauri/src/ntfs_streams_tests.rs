use super::*;
use crate::recovery::{self, Candidate, ScanMode};
use std::io::Cursor;
const ORIGINAL: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
const PAYLOAD: usize = 0x180000;
const CLUSTER: usize =
    (ORIGINAL[11] as usize + ((ORIGINAL[12] as usize) << 8)) * ORIGINAL[13] as usize;
fn raw(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}
fn record_info(bytes: &[u8], name: &str) -> (u64, usize, usize, u16) {
    let mut r = Cursor::new(bytes);
    let ntfs = ntfs::Ntfs::new(&mut r).unwrap();
    for n in 16..1024 {
        let Ok(f) = ntfs.file(&mut r, n) else {
            continue;
        };
        if f.name(&mut r, None, None)
            .is_some_and(|a| a.is_ok_and(|a| a.name().to_string_lossy() == name))
        {
            return (
                n,
                f.position().value().unwrap().get() as usize,
                ntfs.file_record_size() as usize,
                f.sequence_number(),
            );
        }
    }
    panic!("fixture absent")
}
fn decoded(bytes: &[u8], at: usize, size: usize) -> Vec<u8> {
    let mut b = bytes[at..at + size].to_vec();
    let usa = u16le(&b, 4) as usize;
    for i in 1..u16le(&b, 6) as usize {
        let x = [b[usa + i * 2], b[usa + i * 2 + 1]];
        b[i * 512 - 2..i * 512].copy_from_slice(&x);
    }
    b
}
fn attrs(b: &[u8]) -> Vec<Vec<u8>> {
    let mut at = u16le(b, 20) as usize;
    let mut a = vec![];
    while b[at..at + 4] != [255; 4] {
        let n = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        a.push(b[at..at + n].to_vec());
        at += n;
    }
    a
}
fn write_record(bytes: &mut [u8], at: usize, mut b: Vec<u8>, attributes: Vec<Vec<u8>>) {
    let mut p = u16le(&b, 20) as usize;
    b[p..].fill(0);
    for a in attributes {
        assert!(p + a.len() + 8 <= b.len());
        b[p..p + a.len()].copy_from_slice(&a);
        p += a.len();
    }
    b[p..p + 4].fill(255);
    b[24..28].copy_from_slice(&((p + 8) as u32).to_le_bytes());
    let usa = u16le(&b, 4) as usize;
    for i in 1..u16le(&b, 6) as usize {
        let x = [b[i * 512 - 2], b[i * 512 - 1]];
        let marker = [b[usa], b[usa + 1]];
        b[usa + 2 * i..usa + 2 * i + 2].copy_from_slice(&x);
        b[i * 512 - 2..i * 512].copy_from_slice(&marker);
    }
    bytes[at..at + b.len()].copy_from_slice(&b);
}
fn resident(name: &[u8], instance: u16, value: &[u8]) -> Vec<u8> {
    let start = (24 + name.len()).div_ceil(8) * 8;
    let mut b = vec![0; (start + value.len()).div_ceil(8) * 8];
    let len = b.len() as u32;
    b[..4].copy_from_slice(&0x80u32.to_le_bytes());
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[9] = (name.len() / 2) as u8;
    b[10..12].copy_from_slice(&24u16.to_le_bytes());
    b[14..16].copy_from_slice(&instance.to_le_bytes());
    b[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    b[20..22].copy_from_slice(&(start as u16).to_le_bytes());
    b[24..24 + name.len()].copy_from_slice(name);
    b[start..start + value.len()].copy_from_slice(value);
    b
}
fn insert(bytes: &mut [u8], new: Vec<Vec<u8>>, deleted: bool) {
    let (_, at, size, _) = record_info(bytes, "file-with-12345");
    let mut b = decoded(bytes, at, size);
    let mut a = attrs(&b);
    a.extend(new);
    if deleted {
        b[22..24].copy_from_slice(&0u16.to_le_bytes());
    }
    write_record(bytes, at, b, a);
}
fn scan(bytes: &[u8]) -> Vec<Candidate> {
    let mut result = vec![];
    recovery::scan_ntfs(
        &mut Cursor::new(bytes),
        ScanMode::AllFiles,
        |_, _| Ok(()),
        |c| result.push(c),
    )
    .unwrap();
    result
}
fn extract_bytes(bytes: &[u8], c: &Candidate) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    recovery::extract(&mut Cursor::new(bytes), c, &mut out, u64::MAX, |_| Ok(()))?;
    Ok(out)
}
fn named<'a>(files: &'a [Candidate], name: &str) -> &'a Candidate {
    files
        .iter()
        .find(|c| c.name == format!("file-with-12345:{name}"))
        .unwrap()
}
fn mark(bytes: &mut [u8], clusters: &[u64], used: bool) {
    for c in clusters {
        let at = {
            let mut r = Cursor::new(&*bytes);
            let n = ntfs::Ntfs::new(&mut r).unwrap();
            let f = n.file(&mut r, 6).unwrap();
            let a = f.data(&mut r, "").unwrap().unwrap();
            let a = a.to_attribute().unwrap();
            let mut v = a.value(&mut r).unwrap();
            v.seek(&mut r, SeekFrom::Start(c / 8)).unwrap();
            v.data_position().value().unwrap().get() as usize
        };
        if used {
            bytes[at] |= 1 << (c % 8);
        } else {
            bytes[at] &= !(1 << (c % 8));
        }
    }
}
fn nonresident(
    name: &[u8],
    flags: u16,
    logical: u64,
    initialized: u64,
    runs: &[u8],
    clusters: u64,
    stored: u64,
) -> Vec<u8> {
    let start = if flags == 0 { 64 } else { 72 };
    let mapping = (start + name.len()).div_ceil(8) * 8;
    let mut b = vec![0; (mapping + runs.len()).div_ceil(8) * 8];
    let len = b.len() as u32;
    b[..4].copy_from_slice(&0x80u32.to_le_bytes());
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[8] = 1;
    b[9] = (name.len() / 2) as u8;
    b[10..12].copy_from_slice(&(start as u16).to_le_bytes());
    b[12..14].copy_from_slice(&flags.to_le_bytes());
    b[14..16].copy_from_slice(&100u16.to_le_bytes());
    b[24..32].copy_from_slice(&(clusters - 1).to_le_bytes());
    b[32..34].copy_from_slice(&(mapping as u16).to_le_bytes());
    if flags & 1 != 0 {
        b[34] = 4;
    }
    b[40..48].copy_from_slice(&(clusters * CLUSTER as u64).to_le_bytes());
    b[48..56].copy_from_slice(&logical.to_le_bytes());
    b[56..64].copy_from_slice(&initialized.to_le_bytes());
    if flags != 0 {
        b[64..72].copy_from_slice(&stored.to_le_bytes());
    }
    b[start..start + name.len()].copy_from_slice(name);
    b[mapping..mapping + runs.len()].copy_from_slice(runs);
    b
}
#[test]
fn named_resident_streams_keep_raw_identity_preview_and_safe_output_mapping() {
    let mut bytes = ORIGINAL.to_vec();
    insert(
        &mut bytes,
        vec![
            resident(&raw("说明"), 100, b"sidecar"),
            resident(&raw("$DATA"), 101, b"literal-name"),
            resident(&raw("empty"), 102, b""),
        ],
        false,
    );
    let files = scan(&bytes);
    for (name, expected) in [
        ("说明", &b"sidecar"[..]),
        ("$DATA", &b"literal-name"[..]),
        ("empty", &b""[..]),
    ] {
        let c = named(&files, name);
        assert!(c.supported, "{}", c.note);
        assert_eq!(extract_bytes(&bytes, c).unwrap(), expected);
    }
    assert_eq!(
        extract_bytes(
            &bytes,
            files.iter().find(|c| c.name == "file-with-12345").unwrap()
        )
        .unwrap(),
        b"12345"
    );
    let c = named(&files, "说明");
    let mut out = vec![];
    assert_eq!(
        recovery::extract(&mut Cursor::new(&bytes), c, &mut out, 4, |_| Ok(())).unwrap(),
        4
    );
    assert_eq!(out, b"side");
    let tree = crate::recovery_tree::Catalog::default();
    let p = crate::recovery_export::plan(&[c.clone()], &tree, crate::recovery_export::Layout::Flat)
        .unwrap();
    assert!(!p.entries[0].name.contains(':'));
    assert_eq!(p.entries[0].original_name, c.name);
    let (_, at, size, _) = record_info(&bytes, "file-with-12345");
    let b = decoded(&bytes, at, size);
    let mut a = attrs(&b);
    let s = a.iter_mut().find(|a| u16le(a, 14) == 100).unwrap();
    let p = u16le(s, 20) as usize;
    s[p] ^= 1;
    write_record(&mut bytes, at, b, a);
    assert!(extract_bytes(&bytes, c).unwrap_err().contains("身份"));
}
#[test]
fn named_plain_sparse_initialized_and_deleted_allocation_are_checked() {
    for sparse in [false, true] {
        let mut bytes = ORIGINAL.to_vec();
        let lcn = (PAYLOAD / CLUSTER) as u64;
        let mut runs = vec![0x21, 1, (lcn & 255) as u8, (lcn >> 8) as u8];
        if sparse {
            runs.extend([1, 1]);
        } else {
            runs.extend([0x11, 1, 2]);
        }
        runs.push(0);
        insert(
            &mut bytes,
            vec![nonresident(
                &raw("data"),
                if sparse { 0x8000 } else { 0 },
                (CLUSTER * 2) as u64,
                (CLUSTER + 17) as u64,
                &runs,
                2,
                CLUSTER as u64,
            )],
            false,
        );
        bytes[PAYLOAD..PAYLOAD + CLUSTER].fill(7);
        bytes[PAYLOAD + 2 * CLUSTER..PAYLOAD + 3 * CLUSTER].fill(9);
        let clusters = if sparse {
            vec![lcn]
        } else {
            vec![lcn, lcn + 2]
        };
        mark(&mut bytes, &clusters, true);
        let c = named(&scan(&bytes), "data").clone();
        assert!(c.supported, "{}", c.note);
        let out = extract_bytes(&bytes, &c).unwrap();
        assert_eq!(&out[..CLUSTER], vec![7; CLUSTER]);
        assert_eq!(
            &out[CLUSTER..CLUSTER + 17],
            vec![if sparse { 0 } else { 9 }; 17]
        );
        assert_eq!(&out[CLUSTER + 17..], vec![0; CLUSTER - 17]);
        mark(&mut bytes, &clusters, false);
        assert!(extract_bytes(&bytes, &c).unwrap_err().contains("分配"));
        let (_, at, size, _) = record_info(&bytes, "file-with-12345");
        let mut b = decoded(&bytes, at, size);
        let a = attrs(&b);
        b[22..24].fill(0);
        write_record(&mut bytes, at, b, a);
        let deleted = named(&scan(&bytes), "data").clone();
        assert!(deleted.deleted);
        assert_eq!(extract_bytes(&bytes, &deleted).unwrap(), out);
        mark(&mut bytes, &clusters, true);
        assert!(extract_bytes(&bytes, &deleted).is_err());
    }
}
#[test]
fn windows_sparse_unit_padding_is_plain_data_and_bounded() {
    let lcn = (PAYLOAD / CLUSTER) as u64;
    let logical = (32 * CLUSTER + 17) as u64;
    let runs = [
        0x21,
        16,
        (lcn & 255) as u8,
        (lcn >> 8) as u8,
        1,
        16,
        0x11,
        16,
        32,
        0,
    ];
    let mut a = nonresident(
        &raw("sparse"),
        0x8000,
        logical,
        logical,
        &runs,
        48,
        (32 * CLUSTER) as u64,
    );
    a[34] = 4;
    let fixture = |attr: Vec<u8>| {
        let mut bytes = ORIGINAL.to_vec();
        insert(&mut bytes, vec![attr], false);
        bytes[PAYLOAD..PAYLOAD + 16 * CLUSTER].fill(31);
        bytes[PAYLOAD + 32 * CLUSTER..PAYLOAD + 48 * CLUSTER].fill(47);
        mark(
            &mut bytes,
            &(lcn..lcn + 16)
                .chain(lcn + 32..lcn + 48)
                .collect::<Vec<_>>(),
            true,
        );
        bytes
    };
    let bytes = fixture(a.clone());
    let c = named(&scan(&bytes), "sparse").clone();
    assert!(c.supported, "{}", c.note);
    let expected = [vec![31; 16 * CLUSTER], vec![0; 16 * CLUSTER], vec![47; 17]].concat();
    assert_eq!(extract_bytes(&bytes, &c).unwrap(), expected);
    for unit in [0, 1, 3, 5, 16, 255] {
        let mut bad = a.clone();
        bad[34] = unit;
        let bad = fixture(bad);
        assert!(!named(&scan(&bad), "sparse").supported, "unit {unit}");
    }
    for offset in [35, 39, 40, 64] {
        let mut bad = a.clone();
        bad[offset] ^= 1;
        let bad = fixture(bad);
        assert!(!named(&scan(&bad), "sparse").supported, "offset {offset}");
    }
    // A final allocation unit is permitted, an extra virtual unit is not.
    let mut extra = a.clone();
    extra[24..32].copy_from_slice(&63u64.to_le_bytes());
    extra[40..48].copy_from_slice(&(64 * CLUSTER as u64).to_le_bytes());
    let at = u16le(&extra, 32) as usize;
    extra[at + 8] = 32;
    extra[64..72].copy_from_slice(&(48 * CLUSTER as u64).to_le_bytes());
    assert!(!named(&scan(&fixture(extra)), "sparse").supported);
}
#[test]
fn named_lznt1_stream_decodes_and_rechecks_allocation() {
    let mut bytes = ORIGINAL.to_vec();
    let lcn = (PAYLOAD / CLUSTER) as u64;
    let runs = [0x21, 1, (lcn & 255) as u8, (lcn >> 8) as u8, 1, 15, 0];
    insert(
        &mut bytes,
        vec![nonresident(
            &raw("packed"),
            1,
            (CLUSTER * 16) as u64,
            (CLUSTER * 16) as u64,
            &runs,
            16,
            CLUSTER as u64,
        )],
        false,
    );
    bytes[PAYLOAD..PAYLOAD + CLUSTER].fill(0);
    let packed = (0..CLUSTER * 16 / 4096)
        .flat_map(|_| [3, 0xb0, 2, b'Q', 0xfc, 0x0f])
        .collect::<Vec<_>>();
    bytes[PAYLOAD..PAYLOAD + packed.len()].copy_from_slice(&packed);
    mark(&mut bytes, &[lcn], true);
    let files = scan(&bytes);
    let c = named(&files, "packed");
    assert!(c.supported, "{}", c.note);
    assert_eq!(extract_bytes(&bytes, c).unwrap(), vec![b'Q'; CLUSTER * 16]);
    bytes[PAYLOAD] = 255;
    assert!(extract_bytes(&bytes, c).is_err());
}
#[test]
fn stream_names_and_attribute_bounds_reject_malformed_without_panics() {
    for n in [vec![], vec![1], vec![0, 0], vec![1; 512]] {
        assert!(validate_name(&n, false).is_err());
    }
    assert!(validate_name(&[0, 0xd8], false).is_ok());
    for n in 0..80 {
        for byte in [0, 1, 128, 255] {
            let b = vec![byte; n];
            let _ = attribute_name(&b);
            let _ = references(&b);
        }
    }
    let mut bytes = ORIGINAL.to_vec();
    // Even an empty value cannot point beyond its record.
    let mut a = resident(&raw("bad"), 100, b"");
    a[20..22].copy_from_slice(&65535u16.to_le_bytes());
    insert(&mut bytes, vec![a], false);
    assert!(!named(&scan(&bytes), "bad").supported);
}
fn listed(record: u64, sequence: u16, instance: u16, low: u64, name: &[u8]) -> Vec<u8> {
    let mut b = vec![0; (26 + name.len()).div_ceil(8) * 8];
    let len = b.len() as u16;
    b[..4].copy_from_slice(&0x80u32.to_le_bytes());
    b[4..6].copy_from_slice(&len.to_le_bytes());
    b[6] = (name.len() / 2) as u8;
    b[7] = 26;
    b[8..16].copy_from_slice(&low.to_le_bytes());
    b[16..24].copy_from_slice(&(record | (u64::from(sequence) << 48)).to_le_bytes());
    b[24..26].copy_from_slice(&instance.to_le_bytes());
    b[26..26 + name.len()].copy_from_slice(name);
    b
}
#[test]
fn named_stream_extents_validate_extension_ownership_and_join_fragmented_data() {
    let mut bytes = ORIGINAL.to_vec();
    let (record, at, size, sequence) = record_info(&bytes, "file-with-12345");
    let (ext, ext_at, ext_size, ext_sequence) = record_info(&bytes, "empty-file");
    let name = raw("跨记录");
    let lcn = (PAYLOAD / CLUSTER) as u64;
    let mut first = nonresident(
        &name,
        0,
        (CLUSTER * 2) as u64,
        (CLUSTER * 2) as u64,
        &[0x21, 1, lcn as u8, (lcn >> 8) as u8, 0],
        1,
        0,
    );
    first[40..48].copy_from_slice(&((CLUSTER * 2) as u64).to_le_bytes());
    let mut second = nonresident(
        &name,
        0,
        0,
        0,
        &[0x21, 1, (lcn + 2) as u8, ((lcn + 2) >> 8) as u8, 0],
        2,
        0,
    );
    second[16..24].copy_from_slice(&1u64.to_le_bytes());
    second[14..16].copy_from_slice(&101u16.to_le_bytes());
    let mut list = listed(record, sequence, 100, 0, &name);
    list.extend(listed(ext, ext_sequence, 101, 1, &name));
    let mut list_attr = resident(&[], 102, &list);
    list_attr[..4].copy_from_slice(&0x20u32.to_le_bytes());
    let header = decoded(&bytes, at, size);
    let mut a = attrs(&header);
    a.push(first);
    a.push(list_attr);
    a.sort_by_key(|a| u32::from_le_bytes(a[..4].try_into().unwrap()));
    write_record(&mut bytes, at, header, a);
    let mut header = decoded(&bytes, ext_at, ext_size);
    header[32..40].copy_from_slice(&(record | (u64::from(sequence) << 48)).to_le_bytes());
    write_record(&mut bytes, ext_at, header.clone(), vec![second.clone()]);
    bytes[PAYLOAD..PAYLOAD + CLUSTER].fill(3);
    bytes[PAYLOAD + CLUSTER * 2..PAYLOAD + CLUSTER * 3].fill(5);
    mark(&mut bytes, &[lcn, lcn + 2], true);
    let files = scan(&bytes);
    let c = named(&files, "跨记录");
    assert!(c.supported, "{}", c.note);
    let out = extract_bytes(&bytes, c).unwrap();
    assert_eq!(out, [vec![3; CLUSTER], vec![5; CLUSTER]].concat());
    for field in [16, 32] {
        let mut broken = bytes.clone();
        let mut bad = header.clone();
        bad[field] ^= 1;
        write_record(&mut broken, ext_at, bad, vec![second.clone()]);
        assert!(extract_bytes(&broken, c).is_err());
    }
    let mut wrong = second;
    wrong[64] ^= 1;
    write_record(&mut bytes, ext_at, header, vec![wrong]);
    assert!(extract_bytes(&bytes, c).is_err());
}
#[test]
fn checkpoints_preserve_stream_identity_without_alias_collision_and_read_legacy_records() {
    let mut bytes = ORIGINAL.to_vec();
    insert(
        &mut bytes,
        vec![
            resident(&raw("a"), 100, b"one"),
            resident(&raw("A"), 101, b"two"),
        ],
        false,
    );
    let mut cursor = crate::ntfs_scan::Checkpoint::default();
    let mut files = vec![];
    let mut tree = crate::recovery_tree::Catalog::default();
    crate::ntfs_scan::continue_scan(
        &mut Cursor::new(&bytes),
        ScanMode::AllFiles,
        &mut cursor,
        |_, _| Ok(()),
        |c| files.push(c),
        |d| tree.insert(d),
    )
    .unwrap();
    let saved = files
        .iter()
        .map(crate::ntfs_checkpoint::Record::capture)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let json = serde_json::to_value(&saved).unwrap();
    assert!(json
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.get("stream").is_none()));
    let decoded: Vec<crate::ntfs_checkpoint::Record> =
        serde_json::from_value(json.clone()).unwrap();
    let (restored, _) =
        crate::ntfs_checkpoint::restore(&decoded, &tree.snapshot(), &cursor, ScanMode::AllFiles)
            .unwrap();
    assert_eq!(
        extract_bytes(&bytes, named(&restored, "a")).unwrap(),
        b"one"
    );
    assert_eq!(
        extract_bytes(&bytes, named(&restored, "A")).unwrap(),
        b"two"
    );
    let mut bad = json;
    let list = bad.as_array_mut().unwrap();
    let stream = list.iter_mut().find(|r| r.get("stream").is_some()).unwrap();
    stream["stream"]["name"] = serde_json::json!([0]);
    let decoded: Vec<crate::ntfs_checkpoint::Record> = serde_json::from_value(bad).unwrap();
    assert!(crate::ntfs_checkpoint::restore(
        &decoded,
        &tree.snapshot(),
        &cursor,
        ScanMode::AllFiles
    )
    .is_err());
}
#[test]
fn named_stream_cancellation_prevents_read_completion_and_unknown_flags_are_visible() {
    let mut bytes = ORIGINAL.to_vec();
    let mut unknown = resident(&raw("encrypted"), 101, b"secret");
    unknown[12..14].copy_from_slice(&0x4000u16.to_le_bytes());
    insert(
        &mut bytes,
        vec![resident(&raw("ok"), 100, b"plain"), unknown],
        false,
    );
    let files = scan(&bytes);
    let c = named(&files, "ok");
    let mut out = vec![];
    assert!(
        recovery::extract(&mut Cursor::new(&bytes), c, &mut out, u64::MAX, |_| Err(
            "cancel".into()
        ))
        .is_err()
    );
    assert!(out.is_empty());
    let c = named(&files, "encrypted");
    assert!(!c.supported);
    assert!(extract_bytes(&bytes, c).is_err());
}

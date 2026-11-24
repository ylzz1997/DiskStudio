use super::*;
use crate::boot_configuration::{rebuild, repair::gpt_bytes, BOOT_MANAGER};
use regf::{transaction_log::DirtyPageReference, DataType, HiveBuilder, KeyTreeNode, KeyTreeValue};

const DISK: Uuid = Uuid::from_bytes_le(*b"DiskStudioGPT001");
const EFI: Uuid = Uuid::from_bytes_le(*b"DiskStudioBoot01");
const OS: Uuid = Uuid::from_bytes_le(*b"DiskStudioData01");
const MAIN: Uuid = Uuid::from_u128(0x089eaa02_cd8c_440b_83e6_b007e0cbb175);
const SECOND: Uuid = Uuid::from_u128(0x41c60b9e_4322_4ecb_88ed_0e391760c789);
const MAIN_OLD: &str = "Windows Main-OLD";
const MAIN_NEW: &str = "Windows Main-NEW";
const SECOND_OLD: &str = "Windows Backup-OLD";
const SECOND_NEW: &str = "Windows Backup-NEW";

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn field(root: &mut KeyTreeNode, id: Uuid, code: &str, ty: DataType, bytes: &[u8]) {
    let path = format!("Objects\\{{{id}}}\\Elements\\{code}");
    root.get_or_create_path(&path).values.push(KeyTreeValue {
        name: "Element".into(),
        data_type: ty,
        data: bytes.to_vec(),
    });
}

fn baseline(start: u32) -> (Vec<u8>, BaseBlock) {
    let mut root = KeyTreeNode::new("System");
    root.get_or_create_path("Description");
    for (id, kind) in [
        (BOOT_MANAGER, 0x10100002u32),
        (MAIN, 0x10200003),
        (SECOND, 0x10200003),
    ] {
        let path = format!("Objects\\{{{id}}}\\Description");
        root.get_or_create_path(&path).values.push(KeyTreeValue {
            name: "Type".into(),
            data_type: DataType::Dword,
            data: kind.to_le_bytes().to_vec(),
        });
    }
    for (id, name) in [(MAIN, MAIN_OLD), (SECOND, SECOND_OLD)] {
        for (code, value) in [
            ("12000002", r"\Windows\System32\winload.efi"),
            ("22000002", r"\Windows"),
            ("12000004", name),
        ] {
            field(&mut root, id, code, DataType::String, &utf16(value));
        }
        for code in ["11000001", "21000001"] {
            field(&mut root, id, code, DataType::Binary, &gpt_bytes(DISK, OS));
        }
    }
    field(
        &mut root,
        BOOT_MANAGER,
        "11000001",
        DataType::Binary,
        &gpt_bytes(DISK, EFI),
    );
    field(
        &mut root,
        BOOT_MANAGER,
        "12000002",
        DataType::String,
        &utf16(r"\EFI\Microsoft\Boot\bootmgfw.efi"),
    );
    field(
        &mut root,
        BOOT_MANAGER,
        "23000003",
        DataType::String,
        &utf16(&format!("{{{MAIN}}}")),
    );
    field(
        &mut root,
        BOOT_MANAGER,
        "24000001",
        DataType::MultiString,
        &utf16(&format!("{{{MAIN}}}\0{{{SECOND}}}\0")),
    );
    let mut hive =
        HiveBuilder::from_tree_with_version_and_sd(root, 1, 3, Some(rebuild::hive_security()))
            .unwrap();
    let mut bytes =
        rebuild::normalize_timestamps(hive.build().unwrap(), 133_971_840_000_000_000).unwrap();
    let mut base = BaseBlock::parse(&bytes).unwrap();
    base.primary_sequence = start.wrapping_add(1);
    base.secondary_sequence = start;
    base.write(&mut &mut bytes[..BASE_BLOCK_SIZE]).unwrap();
    (bytes, base)
}

fn edit_label(bytes: &mut [u8], before: &str, after: &str) -> (usize, Vec<u8>) {
    let original = utf16(before);
    let replacement = utf16(after);
    let index = bytes
        .windows(original.len())
        .position(|window| window == original)
        .unwrap();
    let end = index + original.len();
    bytes[index..end].copy_from_slice(&replacement);
    let page = index / PAGE * PAGE;
    (
        page - BASE_BLOCK_SIZE,
        bytes[page..end.div_ceil(PAGE) * PAGE].to_vec(),
    )
}

fn modern_log(base: &BaseBlock, sequence: u32, pages: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut header = base.clone();
    header.file_type = 6;
    header.primary_sequence = sequence;
    header.secondary_sequence = sequence;
    let mut log = Vec::new();
    header.write(&mut log).unwrap();
    log.truncate(PAGE);
    let len = LogEntry::HEADER_SIZE
        + 8 * pages.len()
        + pages.iter().map(|(_, data)| data.len()).sum::<usize>();
    let entry = LogEntry {
        signature: *LogEntry::SIGNATURE,
        size: (len.div_ceil(PAGE) * PAGE) as u32,
        flags: 0,
        sequence_number: sequence,
        hive_bins_data_size: base.hive_bins_data_size,
        dirty_pages_count: pages.len() as u32,
        hash1: 0,
        hash2: 0,
        page_references: pages
            .iter()
            .map(|(offset, data)| DirtyPageReference {
                offset: *offset as u32,
                size: data.len() as u32,
            })
            .collect(),
        dirty_pages: pages.iter().map(|(_, data)| data.clone()).collect(),
    };
    entry.write(&mut log).unwrap();
    log
}

fn dual_logs(start: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (dirty, base) = baseline(start);
    let mut expected = dirty.clone();
    let first = edit_label(&mut expected, MAIN_OLD, MAIN_NEW);
    let second = edit_label(&mut expected, SECOND_OLD, SECOND_NEW);
    assert!(first.0 + first.1.len() <= second.0 || second.0 + second.1.len() <= first.0);
    let first = modern_log(&base, start, &[first]);
    let second = modern_log(&base, start.wrapping_add(1), &[second]);
    (dirty, first, second, expected)
}

#[test]
fn recovers_two_rotated_logs_without_losing_either_windows_entry() {
    for start in [42, u32::MAX] {
        let (dirty, first, second, expected) = dual_logs(start);
        let (clean, result) = recover(
            dirty,
            &[("BCD.LOG1", &second), ("BCD.LOG2", &first)],
            DISK,
            EFI,
            OS,
        )
        .unwrap();
        assert_eq!(result.default_entry, MAIN);
        assert_eq!(result.windows_entries, 2);
        assert_eq!(result.transactions, 2);
        assert_eq!(result.logs, ["BCD.LOG2", "BCD.LOG1"]);
        assert_eq!(&clean[BASE_BLOCK_SIZE..], &expected[BASE_BLOCK_SIZE..]);
        assert_eq!(
            BaseBlock::parse(&clean).unwrap().primary_sequence,
            start.wrapping_add(2)
        );
        assert!(verify_store(&clean, DISK, EFI, Uuid::from_u128(19)).is_err());
    }
}

#[test]
fn declines_missing_gapped_corrupt_and_conflicting_transactions() {
    let (dirty, first, mut second, _) = dual_logs(42);
    let base = BaseBlock::parse(&dirty).unwrap();
    let page = dirty[BASE_BLOCK_SIZE..BASE_BLOCK_SIZE + PAGE].to_vec();
    let gap = modern_log(&base, 44, &[(0, page.clone())]);
    let overlap = modern_log(&base, 42, &[(0, page)]);
    *second.last_mut().unwrap() ^= 1;
    for logs in [
        vec![("BCD.LOG1", second.as_slice())],
        vec![
            ("BCD.LOG2", first.as_slice()),
            ("BCD.LOG1", second.as_slice()),
        ],
        vec![("BCD.LOG2", first.as_slice()), ("BCD.LOG1", gap.as_slice())],
        vec![
            ("BCD.LOG2", first.as_slice()),
            ("BCD.LOG1", overlap.as_slice()),
        ],
    ] {
        assert!(recover(dirty.clone(), &logs, DISK, EFI, OS).is_err());
    }
}

#[test]
fn replays_a_legacy_dirty_vector_matching_the_primary_write() {
    let (dirty, mut base) = baseline(42);
    let mut clean = dirty.clone();
    edit_label(&mut clean, MAIN_OLD, MAIN_NEW);
    base.file_type = 1;
    base.secondary_sequence = base.primary_sequence;
    let mut log = Vec::new();
    base.write(&mut log).unwrap();
    log.truncate(PAGE);
    log.extend(b"DIRT");
    log.extend(vec![
        0xff;
        (base.hive_bins_data_size as usize / PAGE).div_ceil(8)
    ]);
    log.resize(2 * PAGE, 0);
    log.extend(&clean[BASE_BLOCK_SIZE..]);
    let (recovered, report) = recover(dirty, &[("BCD.LOG", &log)], DISK, EFI, OS).unwrap();
    assert_eq!(&recovered[BASE_BLOCK_SIZE..], &clean[BASE_BLOCK_SIZE..]);
    assert!(report.format == Format::Legacy);
    assert_eq!(report.default_entry, MAIN);
}

#[test]
#[ignore = "generates regular BCD and log files for the isolated VM fixture"]
fn stage_boot_log_fixture() {
    use std::io::Write;
    let dir = std::path::PathBuf::from(std::env::var_os("DISKSTUDIO_BCD_LOG_FIXTURE_DIR").unwrap());
    assert!(std::fs::metadata(&dir).unwrap().is_dir());
    let (dirty, first, second, _) = dual_logs(42);
    let (recovered, _) = recover(
        dirty.clone(),
        &[("BCD.LOG1", &second), ("BCD.LOG2", &first)],
        DISK,
        EFI,
        OS,
    )
    .unwrap();
    for (name, bytes) in [
        ("BCD", dirty),
        ("BCD.LOG1", second),
        ("BCD.LOG2", first),
        ("BCD-Recovered", recovered),
    ] {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        println!(
            "{name} {} {}",
            bytes.len(),
            crate::sector_edit::hash(&bytes)
        );
    }
}

use super::*;
use std::io::Cursor;
use uuid::Uuid;

fn gpt_source(sector: u64) -> (Archive, Vec<u8>) {
    let partition = table_restore::Entry {
        slot: 7,
        guid: "bb111111-2222-4333-8444-555555555551".into(),
        kind: crate::partitions::policy::BASIC_DATA.into(),
        offset: 2 * MIB,
        size: 2 * MIB,
        attributes: 1 << 63,
        name: "Windows".encode_utf16().collect(),
    };
    let mut table = table_restore::new_gpt(
        8 * MIB,
        sector,
        "0".repeat(64),
        Uuid::from_u128(1),
        &[partition],
    )
    .unwrap();
    table.extents[0].bytes[..5].copy_from_slice(b"OEM!!");
    let mut source = vec![0; table.size as usize];
    source[(2 * MIB) as usize..(4 * MIB) as usize].fill(0x53);
    for extent in &table.extents {
        let at = extent.offset as usize;
        source[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
    }
    (table, source)
}

#[test]
fn relocates_both_gpt_references_without_replacing_partition_data() {
    for sector in [512, 4096] {
        let (original, source) = gpt_source(sector);
        let expected_layout = table_restore::archive_layout(&original).unwrap();
        for size in [6 * MIB, original.size, 9 * MIB, original.size + sector] {
            let after = target_table(&original, size, Firmware::Uefi).unwrap();
            let patches =
                table_patches(&mut Cursor::new(&source), &original, &after, &[], &[]).unwrap();
            let mut output = Vec::new();
            let reader = Padded::new(Cursor::new(&source), original.size, size, &[]);
            Overlay::new(reader, &patches, size)
                .unwrap()
                .read_to_end(&mut output)
                .unwrap();
            let captured = table_archive::capture(
                &mut Cursor::new(&output),
                size,
                sector,
                original.source.clone(),
                false,
                || Ok(()),
            )
            .unwrap();
            assert_eq!(captured, after);
            let layout = table_restore::archive_layout(&captured).unwrap();
            assert_eq!(layout.entries, expected_layout.entries);
            assert_eq!(layout.guid, expected_layout.guid);
            assert_eq!(layout.first, expected_layout.first);
            assert_eq!(
                &source[(2 * MIB) as usize..(4 * MIB) as usize],
                &output[(2 * MIB) as usize..(4 * MIB) as usize]
            );
            assert_eq!(&output[..5], b"OEM!!");
            assert_eq!(&output[(size - sector) as usize..][..8], b"EFI PART");
            if size == 9 * MIB {
                let former_backup_start = original.extents[3].offset as usize;
                assert!(output[former_backup_start..original.size as usize]
                    .iter()
                    .all(|&b| b == 0));
            }
        }
        assert!(target_table(&original, 4 * MIB, Firmware::Uefi).is_err());
    }
}

#[test]
fn migration_streams_write_distinct_gpt_layouts_in_bounded_rounds() {
    use sha2::{Digest, Sha256};

    for sector in [512, 4096] {
        let (original, source) = gpt_source(sector);
        let tables: Vec<_> = [6 * MIB, 9 * MIB].into_iter().map(|size|
            target_table(&original, size, Firmware::Uefi).unwrap()
        ).collect();
        let patches: Vec<_> = tables.iter().map(|table|
            table_patches(&mut Cursor::new(&source), &original, table, &[], &[]).unwrap()
        ).collect();
        let mut copies: Vec<_> = tables.iter().zip(&patches).map(|(table, patches)| (
            Overlay::new(Padded::new(Cursor::new(&source), original.size, table.size, &[]), patches, table.size).unwrap(),
            Vec::new(),
            table.size,
        )).collect();
        let mut writes = Vec::new();
        let hashes = crate::data_io::copy_rounds(&mut copies, |index, done, writing| {
            if writing {
                writes.push((index, done / MIB));
            }
            Ok(())
        }).unwrap();
        assert_eq!(writes, [(0, 0), (1, 4), (0, 8), (1, 10), (1, 14)]);
        assert_ne!(hashes[0], hashes[1]);
        for (((_, bytes, size), table), hash) in copies.iter().zip(&tables).zip(hashes) {
            assert_eq!(hash, format!("{:x}", Sha256::digest(bytes)));
            assert_eq!(table_archive::capture(
                &mut Cursor::new(bytes), *size, sector, original.source.clone(), false, || Ok(()),
            ).unwrap(), *table);
            assert_eq!(&bytes[(2 * MIB) as usize..(4 * MIB) as usize], &source[(2 * MIB) as usize..(4 * MIB) as usize]);
        }
    }
}

#[test]
fn canceled_migration_leaves_later_targets_unwritten() {
    let bytes = [0x5a; 4096];
    let mut copies = [(Cursor::new(&bytes), Vec::new(), 4096),
        (Cursor::new(&bytes), Vec::new(), 4096), (Cursor::new(&bytes), Vec::new(), 4096)];
    let error = crate::data_io::copy_rounds(&mut copies, |index, _, writing| {
        if index == 1 && writing { Err("canceled".into()) } else { Ok(()) }
    }).unwrap_err();
    assert_eq!(error, (1, "canceled".into()));
    assert_eq!(copies[0].1, bytes);
    assert!(copies[1].1.is_empty());
    assert!(copies[2].1.is_empty());
}

#[test]
fn refuses_changed_source_bytes_in_relocated_table_metadata() {
    let (original, mut source) = gpt_source(512);
    let after = target_table(&original, 6 * MIB, Firmware::Uefi).unwrap();
    let patches = table_patches(&mut Cursor::new(&source), &original, &after, &[], &[]).unwrap();
    let sector = patches[0].offset;
    source[sector as usize] ^= 1;
    let reader = Padded::new(Cursor::new(source), original.size, after.size, &[]);
    let mut view = Overlay::new(reader, &patches, after.size).unwrap();
    view.seek(SeekFrom::Start(sector)).unwrap();
    assert!(view.read(&mut [0; 512]).is_err());
}

#[test]
fn mbr_identity_is_preserved_and_target_tail_reads_zero() {
    let size = 6 * MIB;
    let mut source = vec![0; size as usize];
    source[0] = 0xeb;
    source[440..444].copy_from_slice(b"MBR1");
    source[446] = 0x80;
    source[450] = 0x07;
    source[454..458].copy_from_slice(&2048u32.to_le_bytes());
    source[458..462].copy_from_slice(&2048u32.to_le_bytes());
    source[510..512].copy_from_slice(&[0x55, 0xaa]);
    source[size as usize - 1] = 0xcd;
    let before = table_archive::capture(
        &mut Cursor::new(&source),
        size,
        512,
        "0".repeat(64),
        false,
        || Ok(()),
    )
    .unwrap();
    let after = target_table(&before, size + MIB, Firmware::Bios).unwrap();
    assert_eq!(before.extents, after.extents);
    assert!(target_table(&before, size - MIB, Firmware::Bios).is_err());
    let mut reader = Padded::new(Cursor::new(&source), size, after.size, &[]);
    assert_eq!(reader.seek(SeekFrom::Start(size - 1)).unwrap(), size - 1);
    let mut pair = [0x30; 2];
    reader.read_exact(&mut pair).unwrap();
    assert_eq!(pair, [0xcd, 0]);
    assert!(reader.seek(SeekFrom::End(1)).is_err());
    assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), after.size);
    assert_eq!(reader.read(&mut pair).unwrap(), 0);
}

#[test]
fn copies_from_original_ranges_when_moved_volumes_overlap_former_positions() {
    let moving = |offset, new_offset, old_bytes, new_bytes| VolumeChange {
        partition: 1,
        slot: 1,
        offset,
        new_offset,
        old_bytes,
        new_bytes,
        serial: 0,
        cluster: 1,
        fs_bytes: 0,
        boot_move: None,
    };
    let changes = [moving(12, 2, 4, 5), moving(2, 9, 4, 2)];
    let masks = stream_mappings(&changes, 14, None);
    let source: Vec<u8> = (1..=16).collect();
    let mut reader = Padded::new(Cursor::new(source), 16, 15, &masks);
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, [1, 2, 13, 14, 15, 16, 0, 8, 9, 3, 4, 12, 0, 0, 15]);
    reader.seek(SeekFrom::Start(4)).unwrap();
    let mut slice = [0; 8];
    reader.read_exact(&mut slice).unwrap();
    assert_eq!(slice, output[4..12]);
}

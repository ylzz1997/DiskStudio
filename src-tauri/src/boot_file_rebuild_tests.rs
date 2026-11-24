use super::super::tests::SectorImage;
use super::*;
use crate::data_io::Region;
use std::io::Cursor;

#[test]
fn physical_phases_preserve_padding_and_carry_forward_updated_boot_metadata() {
    use rebuild::{Edit, Phase};
    let mut bytes = [0x59; 16384];
    let geometry = Geometry {
        size: bytes.len() as u64,
        sector: 512,
        physical: 4096,
        alignment: 0,
    };
    let boot = Extent {
        start: 4096,
        size: 8192,
    };
    let phases = physical_blocks(
        &mut Cursor::new(&mut bytes[..]),
        geometry,
        boot,
        vec![
            (
                Phase::Dirty,
                vec![Edit {
                    offset: 10,
                    bytes: vec![1, 2],
                }],
            ),
            (
                Phase::FreeSpace,
                vec![Edit {
                    offset: 4093,
                    bytes: vec![3, 4, 5, 6],
                }],
            ),
            (
                Phase::Clean,
                vec![Edit {
                    offset: 10,
                    bytes: vec![0x59; 2],
                }],
            ),
        ],
    )
    .unwrap();
    assert_eq!(phases.len(), 3);
    let (_, dirty) = &phases[0];
    let (_, free) = &phases[1];
    let (_, clean) = &phases[2];
    assert_eq!(dirty.len(), 1);
    assert_eq!(free.len(), 2);
    assert_eq!(dirty[0].after, free[0].before);
    assert_eq!(free[0].after, clean[0].before);
    assert_eq!(&clean[0].after[..4093], &[0x59; 4093]);
    assert_eq!(&clean[0].after[4093..], &[3, 4, 5]);
    assert_eq!(free[1].after[0], 6);
    assert_eq!(&free[1].after[1..], &[0x59; 4095]);
    assert!(physical_blocks(
        &mut Cursor::new(&mut bytes[..]),
        geometry,
        boot,
        vec![(
            Phase::Data,
            vec![Edit {
                offset: 8191,
                bytes: vec![0, 1]
            }]
        )],
    )
    .is_err());
}

fn fixture(var: &str) -> (SectorImage, Geometry, Vec<Span>, Change) {
    let path = std::path::PathBuf::from(std::env::var_os(var).unwrap());
    assert!(std::fs::metadata(&path).unwrap().is_file());
    let sector = std::env::var("DISKSTUDIO_BOOT_FILES_SECTOR")
        .unwrap()
        .parse()
        .unwrap();
    let mut disk = SectorImage {
        bytes: Cursor::new(std::fs::read(path).unwrap()),
        sector,
    };
    let geometry = Geometry {
        size: disk.bytes.get_ref().len() as u64,
        sector,
        physical: 4096,
        alignment: 0,
    };
    let layout = gpt(&mut disk, geometry).unwrap();
    let ranges = layout
        .partitions
        .iter()
        .map(|p| Span {
            start: p.range.start,
            end: p.range.end,
        })
        .collect::<Vec<_>>();
    let change = Change {
        boot_partition: Uuid::from_bytes_le(layout.partitions[0].guid),
        windows_partition: Uuid::from_bytes_le(layout.partitions[1].guid),
        rebuild_fallback: false,
        rebuild_missing_bcd: false,
        damaged_bcd: None,
    };
    (disk, geometry, ranges, change)
}

#[test]
#[ignore = "requires the generated GPT/FAT32/NTFS EFI file fixture"]
fn restores_the_original_windows_efi_manager_and_fallback_on_a_whole_disk() {
    let (mut disk, geometry, ranges, mut change) = fixture("DISKSTUDIO_BOOT_FILES_FIXTURE");
    change.rebuild_fallback = true;
    let sector = geometry.sector;
    let digest_before = sector_edit::hash(disk.bytes.get_ref());
    let patch = plan(&mut disk, geometry, &ranges, &change).unwrap();
    assert_eq!(patch.summary.files.len(), 2);
    assert_eq!(patch.summary.machine, 0x8664);
    assert_eq!(
        patch.summary.loader_sha256,
        "2829a8b2f97f018cb148f0235aedce5b49303155f01c2d6118c4748ce452162f"
    );
    assert_eq!(
        patch.originals[0].original.as_deref(),
        Some(&[0xbd; 719][..])
    );
    assert!(patch.originals[1].original.is_none());
    const MANAGER: &str = "d8ee3eb9725b14981aeca1cb2e9e984d39d6e8c6f6cec7f8a6d1cd4b15f7b45b";
    assert!(patch
        .summary
        .files
        .iter()
        .all(|file| file.installed_sha256 == MANAGER));
    let preserved_path = &["EFI", "other", "marker.txt"];
    let mut prior_blocks = BTreeMap::new();
    for (_, blocks) in &patch.phases {
        for block in blocks {
            assert!(block.range.offset >= ranges[0].start);
            assert!(block.range.offset + block.range.size <= ranges[0].end);
            prior_blocks
                .entry(block.range.offset as usize)
                .or_insert(&block.before);
        }
    }
    let (&first, _) = prior_blocks.first_key_value().unwrap();
    disk.bytes.get_mut()[first] ^= 1;
    assert!(patch
        .write(
            &mut disk,
            || panic!("stale patch must stop before mutation"),
            |_| Ok(())
        )
        .is_err());
    disk.bytes.get_mut()[first] ^= 1;
    assert!(patch
        .write(&mut disk, || Err("Canceled".into()), |_| Ok(()))
        .is_err());
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), digest_before);
    let mut flushes = 0;
    assert!(patch
        .write(
            &mut disk,
            || Ok(()),
            |_| {
                flushes += 1;
                if flushes == 3 {
                    Err(std::io::Error::other("injected sync failure"))
                } else {
                    Ok(())
                }
            }
        )
        .is_err());
    assert_eq!(flushes, 3);
    let mut view = filesystem(&mut disk, geometry, patch.boot).unwrap();
    assert!(crate::fat_recovery::resize::check_boot_fat32(&mut view, sector).is_err());
    drop(view);
    // Restore the synthetic bytes to exercise the successful commit separately.
    for (&offset, bytes) in &prior_blocks {
        disk.bytes.get_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
    if std::env::var_os("DISKSTUDIO_BOOT_FILES_UI_REPORT").is_some() {
        let report = crate::boot_inspection::inspect(
            Region::for_metadata(&mut disk, 0, geometry.size).unwrap(),
            "EFI file rebuild fixture".into(),
            geometry.size,
            sector,
            false,
            Some(&ranges),
        )
        .unwrap();
        println!(
            "BOOT_FILES_UI {}",
            serde_json::json!({"report":report,"preview":patch.summary})
        );
    }
    let mut flushes = 0;
    patch
        .write(
            &mut disk,
            || Ok(()),
            |_| {
                flushes += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(flushes, patch.summary.physical_writes);
    {
        let mut view = filesystem(&mut disk, geometry, patch.boot).unwrap();
        let volume = Volume::open(&mut view).unwrap();
        assert_eq!(
            volume
                .read_file(&mut view, preserved_path, 32)
                .unwrap()
                .as_deref(),
            Some(&b"independent EFI file"[..])
        );
        let bcd = volume
            .read_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)
            .unwrap();
        assert_eq!(bcd.as_deref().map(sector_edit::hash), patch.bcd_sha256);
    }
    assert!(plan(&mut disk, geometry, &ranges, &change)
        .err()
        .unwrap()
        .contains("already match"));
    for (offset, bytes) in prior_blocks {
        disk.bytes.get_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), digest_before);
}

#[test]
#[ignore = "requires a synthetic disk containing the unmodified installation BCD template"]
fn rebuilds_the_missing_bcd_from_an_installed_windows_template() {
    let (mut disk, geometry, ranges, mut change) = fixture("DISKSTUDIO_BOOT_STORE_FIXTURE");
    change.rebuild_missing_bcd = true;
    let hash_before = sector_edit::hash(disk.bytes.get_ref());
    let patch = plan(&mut disk, geometry, &ranges, &change).unwrap();
    let bcd = patch.summary.new_bcd.as_ref().unwrap();
    assert_eq!(patch.originals.len(), 1);
    assert_eq!(patch.originals[0].target, rebuild::Target::Bcd);
    assert!(patch.originals[0].original.is_none());
    assert_eq!(
        bcd.entry.loader.to_string(),
        "b012b84d-c47c-4ed5-b722-c0c42163e569"
    );
    assert_eq!(
        bcd.template_sha256,
        "e105605b19d922ec5f3783a20dcb5d0f0d655beaa65574e114c044ffa2a7962a"
    );
    assert_eq!(patch.retired.len(), 3);
    for (log, (target, byte, length)) in patch.retired.iter().zip([
        (rebuild::Target::BcdLog, 0x76, 19),
        (rebuild::Target::BcdLog1, 0x72, 777),
        (rebuild::Target::BcdLog2, 0x39, 4133),
    ]) {
        assert_eq!(log.target, target);
        assert_eq!(log.original, vec![byte; length]);
    }
    assert_eq!(
        plan(&mut disk, geometry, &ranges, &change)
            .unwrap()
            .fingerprint,
        patch.fingerprint
    );
    assert!(patch
        .write(&mut disk, || Err("Canceled".into()), |_| Ok(()))
        .is_err());
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), hash_before);
    let mut before = BTreeMap::new();
    for (_, blocks) in &patch.phases {
        for block in blocks {
            assert!(block.range.offset >= ranges[0].start);
            assert!(block.range.offset + block.range.size <= ranges[0].end);
            before
                .entry(block.range.offset as usize)
                .or_insert(&block.before);
        }
    }
    patch.write(&mut disk, || Ok(()), |_| Ok(())).unwrap();
    assert!(plan(&mut disk, geometry, &ranges, &change)
        .err()
        .unwrap()
        .contains("existing BCD"));
    let report = crate::boot_inspection::inspect(
        Region::for_metadata(&mut disk, 0, geometry.size).unwrap(),
        "Missing BCD fixture".into(),
        geometry.size,
        geometry.sector,
        false,
        Some(&ranges),
    )
    .unwrap();
    let report = serde_json::to_value(report).unwrap();
    let volume = &report["volumes"][0];
    let loader_id = bcd.entry.loader.to_string();
    assert_eq!(volume["store"]["default"], loader_id);
    let references = volume["references"].as_array().unwrap();
    let loader = references
        .iter()
        .find(|reference| reference["id"] == loader_id)
        .unwrap();
    assert_eq!(loader["application"]["state"], "matched");
    assert_eq!(loader["system"]["state"], "matched");
    assert_eq!(loader["loader"]["sha256"], patch.summary.loader_sha256);
    let mut view = filesystem(&mut disk, geometry, patch.boot).unwrap();
    let volume = Volume::open(&mut view).unwrap();
    assert_eq!(
        volume
            .read_file(&mut view, &["EFI", "other", "marker.txt"], 32)
            .unwrap()
            .as_deref(),
        Some(&b"independent EFI file"[..])
    );
    drop(view);
    for (offset, original) in before {
        disk.bytes.get_mut()[offset..offset + original.len()].copy_from_slice(original);
    }
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), hash_before);
    println!(
        "BCD_STORE_PREVIEW {}",
        serde_json::to_value(&patch.summary).unwrap()
    );
}

#[test]
#[ignore = "requires the generated GPT/FAT32/NTFS damaged BCD fixtures"]
fn handles_a_dirty_or_corrupt_existing_bcd_without_overwriting_a_readable_store() {
    let (mut disk, geometry, ranges, mut change) = fixture("DISKSTUDIO_BOOT_DAMAGED_FIXTURE");
    let replay = std::env::var("DISKSTUDIO_BOOT_DAMAGED_OPERATION").unwrap() == "recover";
    let boot = Extent {
        start: ranges[0].start,
        size: ranges[0].end - ranges[0].start,
    };
    let (original, logs) = {
        let mut view = filesystem(&mut disk, geometry, boot).unwrap();
        let volume = Volume::open(&mut view).unwrap();
        let bcd = volume
            .read_file(&mut view, EFI_BCD, boot_configuration::MAX_STORE)
            .unwrap()
            .unwrap();
        let logs = rebuild::BCD_LOGS
            .iter()
            .filter_map(|target| {
                volume
                    .read_file(&mut view, &target.path(), MAX_EXECUTABLE)
                    .unwrap()
                    .map(|bytes| (*target, bytes))
            })
            .collect::<Vec<_>>();
        (bcd, logs)
    };
    let hash = sector_edit::hash(&original);
    change.damaged_bcd = Some(DamagedBcd::Recover {
        original_sha256: "0".repeat(64),
    });
    assert!(plan(&mut disk, geometry, &ranges, &change)
        .err()
        .unwrap()
        .contains("digest changed"));
    change.damaged_bcd = Some(DamagedBcd::Recover {
        original_sha256: hash.clone(),
    });
    if !replay {
        assert!(plan(&mut disk, geometry, &ranges, &change).is_err());
        change.damaged_bcd = Some(DamagedBcd::Replace {
            original_sha256: hash,
        });
    }
    let before = sector_edit::hash(disk.bytes.get_ref());
    let patch = plan(&mut disk, geometry, &ranges, &change).unwrap();
    assert_eq!(patch.originals.len(), 1);
    assert_eq!(patch.originals[0].target, rebuild::Target::Bcd);
    assert_eq!(patch.originals[0].original.as_ref(), Some(&original));
    assert_eq!(patch.retired.len(), 2);
    for (record, (target, bytes)) in patch.retired.iter().zip(&logs) {
        assert_eq!((record.target, &record.original), (*target, bytes));
    }
    assert!(patch
        .write(&mut disk, || Err("Canceled".into()), |_| Ok(()))
        .is_err());
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), before);
    patch.write(&mut disk, || Ok(()), |_| Ok(())).unwrap();
    let installed = patch.bcd_sha256.clone().unwrap();
    change.damaged_bcd = Some(DamagedBcd::Replace {
        original_sha256: installed,
    });
    assert!(plan(&mut disk, geometry, &ranges, &change)
        .err()
        .unwrap()
        .contains("must be preserved"));
    let summary = serde_json::to_value(&patch.summary).unwrap();
    if replay {
        assert_eq!(summary["recoveredBcd"]["transactions"], 2);
        assert_eq!(summary["recoveredBcd"]["windowsEntries"], 2);
        assert_eq!(
            summary["recoveredBcd"]["logs"],
            serde_json::json!(["BCD.LOG2", "BCD.LOG1"])
        );
    } else {
        assert_eq!(
            summary["newBcd"]["loader"],
            "b012b84d-c47c-4ed5-b722-c0c42163e569"
        );
    }
    println!("DAMAGED_BCD_PREVIEW {summary}");
}

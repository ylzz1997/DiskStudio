use super::*;
use std::io::{self, Cursor};

struct SectorImage {
    bytes: Cursor<Vec<u8>>,
    sector: u64,
}

impl SectorImage {
    fn check(&self, len: usize) -> io::Result<()> {
        if self.bytes.position() % self.sector != 0 || len as u64 % self.sector != 0 {
            return Err(io::Error::other("Unaligned synthetic device request"));
        }
        Ok(())
    }
}

impl Read for SectorImage {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check(bytes.len())?;
        self.bytes.read(bytes)
    }
}

impl Write for SectorImage {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check(bytes.len())?;
        self.bytes.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for SectorImage {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.bytes.seek(from)
    }
}

#[test]
fn a_fragmented_bcd_patch_preserves_neighboring_bytes_and_checks_declared_scope() {
    let mut disk = vec![0x81; 16 * 1024];
    let extent = Extent {
        start: 4096,
        size: 8192,
    };
    let geometry = Geometry {
        size: disk.len() as u64,
        sector: 512,
        physical: 4096,
        alignment: 0,
    };
    let mapped = fat_recovery::boot_files::MappedFile {
        bytes: vec![0x81; 514],
        clusters: vec![512, 4608],
        cluster_size: 512,
    };
    let mut updated = mapped.bytes.clone();
    updated[511..514].copy_from_slice(&[0xa2, 0xb3, 0xc4]);
    let blocks = changed_blocks(
        &mut Cursor::new(&mut disk),
        geometry,
        extent,
        &mapped,
        &updated,
    )
    .unwrap();
    assert_eq!(blocks.len(), 2);
    for (block, offset, patch_start, patch) in [
        (&blocks[0], 4096, 1023, &[0xa2][..]),
        (&blocks[1], 8192, 512, &[0xb3, 0xc4][..]),
    ] {
        assert_eq!((block.range.offset, block.range.size), (offset, 4096));
        assert_eq!(block.before, vec![0x81; 4096]);
        let mut expected = block.before.clone();
        expected[patch_start..patch_start + patch.len()].copy_from_slice(patch);
        assert_eq!(block.after, expected);
    }
    let crossed_scope = Extent {
        start: extent.start,
        size: 4608 + 512,
    };
    assert!(changed_blocks(
        &mut Cursor::new(&mut disk),
        geometry,
        crossed_scope,
        &mapped,
        &updated
    )
    .is_err());
    disk[5119] = 0xa0;
    assert!(changed_blocks(
        &mut Cursor::new(&mut disk),
        geometry,
        extent,
        &mapped,
        &updated
    )
    .is_err());
}

/// Explicit fixture check with original Windows loaders and a synthetic disk
/// image. Read the regular file once and apply the patch in memory only.
#[test]
#[ignore = "requires the generated GPT/FAT32/NTFS boot repair fixture"]
fn applies_the_full_patch_with_real_windows_loaders() {
    let path =
        std::path::PathBuf::from(std::env::var_os("DISKSTUDIO_BOOT_REPAIR_FIXTURE").unwrap());
    assert!(std::fs::metadata(&path).unwrap().is_file());
    let sector: u64 = std::env::var("DISKSTUDIO_BOOT_REPAIR_SECTOR")
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
    assert_eq!(layout.partitions.len(), 2);
    let (esp, data) = (&layout.partitions[0], &layout.partitions[1]);
    let ranges = layout
        .partitions
        .iter()
        .map(|p| Span {
            start: p.range.start,
            end: p.range.end,
        })
        .collect::<Vec<_>>();
    let boot = Extent {
        start: esp.range.start,
        size: esp.range.end - esp.range.start,
    };
    let original = {
        let mut reader = filesystem(&mut disk, geometry, boot).unwrap();
        let fat = fat_recovery::boot_files::Volume::open(&mut reader).unwrap();
        fat.mapped_file(&mut reader, EFI_BCD, boot_configuration::MAX_STORE)
            .unwrap()
    };
    let entry = Uuid::from_u128(0xeaf9ef92_c25a_48f2_bc9a_13c6debf0001);
    let change = Change {
        boot_partition: Uuid::from_bytes_le(esp.guid),
        store_sha256: sector_edit::hash(&original.bytes),
        entry,
        windows_partition: Uuid::from_bytes_le(data.guid),
        make_default: true,
    };
    let patch = plan(&mut disk, geometry, &ranges, &change).unwrap();
    if std::env::var_os("DISKSTUDIO_BOOT_REPAIR_UI_REPORT").is_some() {
        let report = super::super::inspect(
            Region::for_metadata(&mut disk, 0, geometry.size).unwrap(),
            "Boot repair fixture".into(),
            geometry.size,
            sector,
            false,
            Some(&ranges),
        )
        .unwrap();
        println!(
            "BOOT_REPAIR_UI {}",
            serde_json::json!({"report":report,"preview":patch.summary})
        );
    }
    assert_eq!(patch.original, original.bytes);
    assert_ne!(
        patch.summary.application_before,
        patch.summary.application_after
    );
    assert_ne!(patch.summary.system_before, patch.summary.application_after);
    assert_ne!(patch.summary.default_before, Some(entry));
    assert_eq!(patch.summary.default_after, Some(entry));
    let digest_before = sector_edit::hash(disk.bytes.get_ref());
    let last = patch.blocks.last().unwrap().range.offset as usize;
    disk.bytes.get_mut()[last] ^= 1;
    assert!(patch
        .write(
            &mut disk,
            || panic!("all blocks must be checked first"),
            |_| Ok(())
        )
        .is_err());
    disk.bytes.get_mut()[last] ^= 1;
    assert!(patch
        .write(&mut disk, || Err("canceled by caller".into()), |_| Ok(()))
        .is_err());
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), digest_before);
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
    assert_eq!(flushes, patch.summary.blocks);
    let report = super::super::inspect(
        Region::for_metadata(&mut disk, 0, geometry.size).unwrap(),
        "Repaired fixture".into(),
        geometry.size,
        sector,
        false,
        Some(&ranges),
    )
    .unwrap();
    let json = serde_json::to_value(report).unwrap();
    let esp_report = &json["volumes"][0];
    let checked = esp_report["references"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == entry.to_string())
        .unwrap();
    assert_eq!(checked["application"]["state"], "matched");
    assert_eq!(checked["system"]["state"], "matched");
    assert_eq!(checked["loader"]["sha256"], patch.summary.loader_sha256);
    assert_eq!(esp_report["store"]["default"], entry.to_string());
    assert!(
        plan(&mut disk, geometry, &ranges, &change).is_err(),
        "the prior BCD digest is stale"
    );
    let mut reader = filesystem(&mut disk, geometry, boot).unwrap();
    let fat = fat_recovery::boot_files::Volume::open(&mut reader).unwrap();
    let repaired = fat
        .mapped_file(&mut reader, EFI_BCD, boot_configuration::MAX_STORE)
        .unwrap();
    assert_eq!(repaired.clusters, original.clusters);
    assert_eq!(repaired.cluster_size, original.cluster_size);
    drop(reader);
    // Revert only the two/three value cells in the BCD's existing chain. If any
    // other file, slack, allocation metadata or disk sector changed, this hash differs.
    for (index, bytes) in original.bytes.chunks(original.cluster_size).enumerate() {
        let offset = (boot.start + original.clusters[index]) as usize;
        disk.bytes.get_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
    assert_eq!(sector_edit::hash(disk.bytes.get_ref()), digest_before);
}

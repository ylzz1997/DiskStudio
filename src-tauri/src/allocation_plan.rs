use super::*;

/// `partitions` is absent for a selected partition, or is the complete fresh
/// provider layout for a whole disk. No frontend paths or offsets reach here.
pub fn inspect<R: Read + Seek>(
    source: &mut R,
    size: u64,
    sector: u64,
    partitions: Option<&[Span]>,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    let mut builder = Builder::new(size, sector)?;
    if source.seek(SeekFrom::End(0)).map_err(err)? != size {
        return Err("分配分析与已验证源容量不一致".into());
    }
    let ranges = if let Some(expected) = partitions {
        // Capture validates both GPT copies or the complete MBR/EBR chain.
        // Only exact table extents are retained as metadata proof, not the
        // discovery windows (which may contain ordinary filesystem bytes).
        let archive =
            crate::table_archive::capture(source, size, sector, "0".repeat(64), false, || {
                stop(cancel)
            })?;
        let mut actual = archive
            .validate()?
            .layout
            .into_iter()
            .map(|p| Span {
                start: p.offset,
                end: p.offset + p.size,
            })
            .collect::<Vec<_>>();
        actual.sort_by_key(|s| s.start);
        let mut expected = expected.to_vec();
        expected.sort_by_key(|s| s.start);
        if actual != expected {
            return Err("设备分区表与 Windows 提供的完整布局不一致，已拒绝按已用空间处理。".into());
        }
        for e in archive.extents {
            for (i, bytes) in e.bytes.chunks(CHUNK).enumerate() {
                builder
                    .proof
                    .record(e.offset + i as u64 * CHUNK as u64, bytes)
                    .map_err(err)?;
            }
        }
        actual
    } else {
        vec![Span {
            start: 0,
            end: size,
        }]
    };
    let mut end = 0;
    for range in ranges {
        stop(cancel)?;
        if range.start < end
            || range.start >= range.end
            || range.end > size
            || range.start % sector != 0
            || range.end % sector != 0
        {
            return Err("源分区范围重叠、越界或未对齐。".into());
        }
        end = range.end;
        let map = {
            let mut audit = Audit::aligned(
                source,
                range.start,
                range.size(),
                sector,
                &mut builder.proof,
                cancel,
            )?;
            let mut boot = [0; 512];
            audit.read_exact(&mut boot).map_err(err)?;
            if &boot[3..11] == b"NTFS    " {
                crate::ntfs_allocation::inspect(&mut audit, cancel)?
            } else if &boot[3..11] == b"EXFAT   "
                || &boot[54..62] == b"FAT12   "
                || &boot[54..62] == b"FAT16   "
                || &boot[82..90] == b"FAT32   "
                || (boot[510..] == [0x55, 0xaa]
                    && matches!(
                        u16::from_le_bytes([boot[11], boot[12]]),
                        512 | 1024 | 2048 | 4096
                    )
                    && boot[13].is_power_of_two()
                    && matches!(boot[16], 1 | 2))
            {
                crate::fat_recovery::allocation::inspect(&mut audit, cancel)?
            } else {
                let mut magic = [0; 2];
                if range.size() >= 2048 {
                    audit.seek(SeekFrom::Start(1024 + 0x38)).map_err(err)?;
                    audit.read_exact(&mut magic).map_err(err)?;
                }
                if magic == [0x53, 0xef] {
                    crate::ext_allocation::inspect(&mut audit, cancel)?
                } else if disk_core::btrfs::has_magic(&mut audit, range.size())? {
                    use disk_core::btrfs::{allocation_for_copy, CopyAllocation};
                    let mut map = Allocation::new("Btrfs", range.size());
                    match allocation_for_copy(&mut audit, || stop(cancel))? {
                        CopyAllocation::SingleDevice(free) => {
                            for span in free {
                                map.free(span.start, span.end - span.start)?;
                            }
                            map.note =
                                "已核对单设备 Btrfs 分配记录，完整保留元数据和系统块组。".into();
                        }
                        CopyAllocation::MultiDevice => {
                            map.note = "多设备 Btrfs 成员完整逐扇区保留。".into();
                        }
                    }
                    map
                } else {
                    let mut map = Allocation::new("未识别", range.size());
                    map.note = "未确认空闲范围，该分区完整逐扇区保留。".into();
                    map
                }
            }
        };
        map.validate()?;
        let before = builder.free.iter().map(|s| s.size()).sum::<u64>();
        for free in map.free {
            builder.free(range.start + free.start, free.size())?;
        }
        let after = builder.free.iter().map(|s| s.size()).sum::<u64>();
        builder.volumes.push(VolumeReport {
            offset: range.start,
            size: range.size(),
            filesystem: map.filesystem,
            skipped_bytes: after - before,
            note: map.note,
        });
    }
    builder.finish()
}

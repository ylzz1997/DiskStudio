//! Reconstruct a read-only Ext4 view from a verified backup superblock.
use super::{crc, err, superblock_layout, u16le, u32le, Session, SuperblockLayout};
use crate::recovery::{
    ntfs_reconstruct,
    view::{Patch, Reader},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    rc::Rc,
    sync::{atomic::AtomicBool, Arc},
};

const SUPERBLOCK: u64 = 1024;
const MAGIC: u16 = 0xef53;
const CHECKSUM: usize = 0x3fc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Proof {
    pub(crate) sector_size: u64,
    backup_group: u16,
    backup_offset: u64,
    bytes: u64,
    evidence_sha256: String,
}

pub(crate) struct Evidence {
    primary: [u8; 1024],
    backup: [u8; 1024],
    offset: u64,
    layout: SuperblockLayout,
}

fn read_at(reader: &mut (impl Read + Seek), at: u64, out: &mut [u8]) -> Result<(), String> {
    reader.seek(SeekFrom::Start(at)).map_err(err)?;
    reader.read_exact(out).map_err(err)
}

fn checksum(s: &[u8; 1024]) -> bool {
    s[0x175] == 1 && crc(!0, &s[..CHECKSUM]) == u32le(s, CHECKSUM)
}

fn clean_fields(s: &[u8; 1024]) -> bool {
    u32le(s, 0x60) & 4 == 0 && u32le(s, 0xe8) == 0 && u32le(s, 0x64) & 0x10000 == 0
}

fn backup_groups(s: &[u8; 1024], groups: u64) -> Vec<u64> {
    if u32le(s, 0x5c) & 0x200 != 0 {
        let mut backups = vec![u64::from(u32le(s, 0x24c)), u64::from(u32le(s, 0x250))];
        backups.sort_unstable();
        backups.dedup();
        return backups
            .into_iter()
            .filter(|g| (1..groups).contains(g))
            .collect();
    }
    if u32le(s, 0x64) & 1 != 0 {
        let mut backups = vec![1];
        for base in [3, 5, 7] {
            let mut group = base;
            while group < groups {
                backups.push(group);
                group *= base;
            }
        }
        backups.retain(|g| *g < groups);
        backups.sort_unstable();
        return backups;
    }
    (1..groups.min(33))
        .chain((groups > 33).then_some(groups - 1))
        .collect()
}

fn compatible(primary: &[u8; 1024], backup: &[u8; 1024]) -> bool {
    // Backup counters and mount times are historical. Compare identity,
    // interpretation fields and the locations of special metadata.
    [
        0x14..0x2c,
        0x48..0x50,
        0x54..0x5a,
        0x5c..0x78,
        0xd0..0xe8,
        0xec..0x100,
        0x24c..0x254,
        0x280..0x284,
    ]
    .into_iter()
    .all(|field| primary[field.clone()] == backup[field])
}

/// Derive a possible volume extent from a complete, checksummed backup.
/// The caller must still corroborate it against the current volume metadata.
pub(crate) fn backup_extent(bytes: &[u8], limit: u64) -> Option<(u64, u64)> {
    let s: &[u8; 1024] = bytes.get(..1024)?.try_into().ok()?;
    let group = u64::from(u16le(s, 0x5a));
    if group == 0
        || u16le(s, 0x38) != MAGIC
        || u16le(s, 0x3a) > 1
        || u32le(s, 0x64) & 0x400 == 0
        || !clean_fields(s)
        || !checksum(s)
    {
        return None;
    }
    let layout = superblock_layout(s, limit).ok()?;
    let listed = if u32le(s, 0x5c) & 0x200 != 0 || u32le(s, 0x64) & 1 != 0 {
        backup_groups(s, layout.groups).contains(&group)
    } else {
        group < layout.groups
    };
    let offset = (layout.first + group * layout.bpg) * layout.block;
    (listed && offset + SUPERBLOCK <= layout.size).then_some((offset, layout.size))
}

pub(crate) fn inspect(
    reader: &mut (impl Read + Seek),
    sector: u64,
    cancel: &AtomicBool,
) -> Result<Option<Evidence>, String> {
    crate::used_space::stop(cancel)?;
    let length = reader.seek(SeekFrom::End(0)).map_err(err)?;
    if !matches!(sector, 512 | 4096) || length < 4096 || length % sector != 0 {
        return Ok(None);
    }
    let mut primary = [0; 1024];
    read_at(reader, SUPERBLOCK, &mut primary)?;
    let signature = u16le(&primary, 0x38) == MAGIC;
    let checksummed = u32le(&primary, 0x64) & 0x400 != 0 && primary[0x175] == 1;
    let layout = superblock_layout(&primary, length);
    if !signature && (!checksummed || layout.is_err()) {
        return Ok(None);
    }
    let mut boot = [0; 512];
    read_at(reader, 0, &mut boot)?;
    if &boot[3..11] == b"NTFS    "
        || crate::recovery::fat_boot(&boot)
        || crate::bitlocker::boot_marker(&boot)
        || ntfs_reconstruct::has_tail_boot(reader, sector, length)?
        || disk_core::btrfs::has_magic(reader, length)?
        || crate::fat_recovery::reconstruct::inspect(reader, sector, cancel)?.is_some()
    {
        return Err("检测到其他文件系统或加密证据，不能采用 Ext4 备用超级块。".into());
    }
    if !checksummed || u16le(&primary, 0x5a) != 0 || primary[0x68..0x78] == [0; 16] {
        return Err("重建需要带元数据校验且保留卷身份的 Ext4 主超级块。".into());
    }
    if u16le(&primary, 0x3a) != 1 || !clean_fields(&primary) {
        return Err("Ext 文件系统未干净卸载或需要日志恢复；只读扫描不会重放日志。".into());
    }
    if signature && checksum(&primary) {
        return Err("Ext4 主超级块可读，请使用常规目录读取。".into());
    }
    let layout = layout?;
    if u32le(&primary, 0x5c) & 4 == 0
        || primary[0xd0..0xe0] != [0; 16]
        || u32le(&primary, 0xe0) != 8
        || u32le(&primary, 0xe4) != 0
    {
        return Err("Ext4 重建需要可核对的内部日志。".into());
    }
    let mut selected = None;
    for group in backup_groups(&primary, layout.groups) {
        crate::used_space::stop(cancel)?;
        let offset = (layout.first + group * layout.bpg) * layout.block;
        if offset + SUPERBLOCK > layout.size {
            continue;
        }
        let mut backup = [0; 1024];
        read_at(reader, offset, &mut backup)?;
        if u16le(&backup, 0x38) != MAGIC || !checksum(&backup) {
            continue;
        }
        if u64::from(u16le(&backup, 0x5a)) != group
            || u16le(&backup, 0x3a) > 1
            || !clean_fields(&backup)
            || superblock_layout(&backup, length).as_ref() != Ok(&layout)
            || !compatible(&primary, &backup)
        {
            return Err("Ext4 主备超级块的身份、状态或几何不一致。".into());
        }
        selected.get_or_insert((offset, backup));
    }
    let (offset, backup) = selected.ok_or("未找到与 Ext4 主超级块相符的有效备用副本。")?;
    Ok(Some(Evidence {
        primary,
        backup,
        offset,
        layout,
    }))
}

fn journal_header(
    mut bytes: [u8; 1024],
    uuid: &[u8],
    block_size: u64,
    inode_size: u64,
) -> Result<(), String> {
    let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let blocks = be(0x10);
    let first = be(0x14);
    let features = be(0x28);
    let expected_crc = be(0xfc);
    if be(0) != 0xc03b3998
        || be(4) != 4
        || u64::from(be(0xc)) != block_size
        || !(1..blocks).contains(&first)
        || u64::from(blocks) * block_size > inode_size
        || be(0x1c) != 0
        || be(0x20) != 0
        || be(0x24) & !1 != 0
        || features & !0x1b != 0
        || features & 0x18 == 0x18
        || be(0x2c) != 0
        || &bytes[0x30..0x40] != uuid
        || be(0x40) != 1
    {
        return Err("Ext4 内部日志状态、范围或卷身份核对失败。".into());
    }
    if features & 0x18 != 0 {
        bytes[0xfc..0x100].fill(0);
        if bytes[0x50] != 4 || crc(!0, &bytes) != expected_crc {
            return Err("Ext4 内部日志超级块校验失败。".into());
        }
    }
    Ok(())
}

impl Evidence {
    pub(crate) fn open<R: Read + Seek + 'static>(
        self,
        reader: R,
        sector: u64,
        cancel: &AtomicBool,
    ) -> Result<(Reader<R>, Proof), String> {
        crate::used_space::stop(cancel)?;
        let mut replacement = self.backup;
        replacement[0x5a..0x5c].fill(0);
        replacement[0x3a..0x3c].copy_from_slice(&1u16.to_le_bytes());
        let sum = crc(!0, &replacement[..CHECKSUM]);
        replacement[CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        let reader = Reader::new(
            reader,
            vec![Patch {
                at: SUPERBLOCK,
                bytes: replacement.to_vec(),
            }],
            self.layout.size,
        );
        let mut session = Session::new(reader, Arc::new(AtomicBool::new(false)))?;
        let root = session.fs.symlink_metadata(b"/".as_slice()).map_err(err)?;
        if root.inode_number() != 2 || !root.is_dir() {
            return Err("Ext4 根目录元数据无效。".into());
        }
        session.allocated(&root)?;
        let mut header = [0; 1024];
        let journal = session
            .fs
            .internal_journal_header(u32le(&replacement, 0xe0), &mut header)
            .map_err(err)?;
        session.allocated(&journal)?;
        journal_header(
            header,
            &replacement[0x68..0x78],
            self.layout.block,
            journal.len(),
        )?;
        crate::used_space::stop(cancel)?;

        let mut hash = Sha256::new();
        hash.update(self.primary);
        hash.update(self.backup);
        hash.update(session.geometry.fingerprint);
        hash.update(root.inode_record());
        hash.update(journal.inode_record());
        hash.update(header);
        let proof = Proof {
            sector_size: sector,
            backup_group: u16le(&self.backup, 0x5a),
            backup_offset: self.offset,
            bytes: self.layout.size,
            evidence_sha256: format!("{:x}", hash.finalize()),
        };
        // The checked Ext4 adapter shares the reader only for this validation.
        drop(session.fs);
        let source = Rc::try_unwrap(session.source)
            .map_err(|_| "Ext4 只读元数据检查仍持有源读取器。")?
            .into_inner();
        Ok((source.reader, proof))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;
    use crate::data_io::Region;
    use crate::recovery::{self, ScanMode};
    use std::io::{self, Cursor};

    const GROUP_ONE: usize = 8_389_632;
    const JOURNAL: usize = 16_778_240;

    fn damaged() -> Vec<u8> {
        let mut image = fixture(2).to_vec();
        image[1024 + 0x38..1024 + 0x3a].fill(0);
        image
    }

    struct BudgetedImage {
        inner: Cursor<Vec<u8>>,
        bytes_read: u64,
    }

    impl Read for BudgetedImage {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = self.inner.read(out)?;
            self.bytes_read += count as u64;
            if self.bytes_read > self.inner.get_ref().len() as u64 {
                return Err(io::Error::other(
                    "Exceeded the test image's transfer budget",
                ));
            }
            Ok(count)
        }
    }

    impl Seek for BudgetedImage {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.inner.seek(from)
        }
    }

    #[test]
    fn reconstructs_ext4_from_backup_and_exports_verified_file() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut image = damaged();
        for sector in [512, 4096] {
            let source = BudgetedImage {
                inner: Cursor::new(image.clone()),
                bytes_read: 0,
            };
            let region = Region::for_file_view(source, 0, image.len() as u64).unwrap();
            let (reader, proof) = recovery::view::Proof::open(region, sector, &cancel).unwrap();
            let recovery::view::Proof::Ext(details) = &proof else {
                panic!("Expected Ext4");
            };
            assert_eq!(details.backup_offset, GROUP_ONE as u64);
            let mut files = Vec::new();
            super::super::scan(
                reader,
                ScanMode::Existing,
                cancel.clone(),
                |_, _| Ok(()),
                |c| files.push(c),
                |_| Ok(()),
            )
            .unwrap();
            assert!(
                files.len() > 1_004,
                "Reached the fixture's large indexed directory"
            );
            let file = files.iter().find(|c| c.name == "small_file").unwrap();
            let mut bytes = Vec::new();
            let checked = proof.reopen(Cursor::new(image.clone()), &cancel).unwrap();
            recovery::extract_owned(checked, file, &mut bytes, u64::MAX, cancel.clone(), |_| {
                Ok(())
            })
            .unwrap();
            assert_eq!(bytes, b"hello, world!");
            image[1024 + CHECKSUM] ^= 1;
            assert!(proof.reopen(Cursor::new(image.clone()), &cancel).is_err());
        }
    }

    #[test]
    fn rejects_incompatible_backup_current_descriptors_and_dirty_journal() {
        let cancel = AtomicBool::new(false);
        let cases = [
            (GROUP_ONE + 0x68, "身份"),
            (2048 + 0x1e, "校验"),
            (JOURNAL + 0x1f, "日志"),
            (JOURNAL + 0xff, "校验"),
            (1024 + 0x3a, "未干净卸载"),
        ];
        for (at, reason) in cases {
            let mut image = damaged();
            image[at] ^= 1;
            if at == GROUP_ONE + 0x68 {
                let sum = crc(!0, &image[GROUP_ONE..GROUP_ONE + CHECKSUM]);
                image[GROUP_ONE + CHECKSUM..GROUP_ONE + 1024].copy_from_slice(&sum.to_le_bytes());
            }
            let error = recovery::view::Proof::open(Cursor::new(image), 512, &cancel)
                .err()
                .unwrap();
            assert!(error.contains(reason), "{error}");
        }
    }

    #[test]
    fn requires_surviving_primary_and_can_skip_a_bad_backup_checksum() {
        let cancel = AtomicBool::new(false);
        let mut image = damaged();
        image[GROUP_ONE + CHECKSUM] ^= 1;
        let (_, proof) =
            recovery::view::Proof::open(Cursor::new(image.clone()), 512, &cancel).unwrap();
        let recovery::view::Proof::Ext(proof) = proof else {
            panic!("Expected Ext4");
        };
        assert_eq!(proof.backup_group, 3);
        image[1024..2048].fill(0);
        assert!(inspect(&mut Cursor::new(image), 512, &cancel)
            .unwrap()
            .is_none());
        let healthy = inspect(&mut Cursor::new(fixture(2)), 512, &cancel)
            .err()
            .unwrap();
        assert!(healthy.contains("主超级块可读"));
    }
}

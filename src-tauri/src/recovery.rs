//! Actual read-only recovery engines. Candidates are not promises of intact data.
use crate::recovery_tree::Directory;
use ntfs::{Ntfs, NtfsAttributeFlags, NtfsFileFlags, NtfsReadSeek};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};

#[path = "ntfs_reconstruct.rs"]
pub(crate) mod ntfs_reconstruct;
#[path = "recovery_view.rs"]
pub(crate) mod view;

pub const MAX_RESULTS: usize = 100_000;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ScanMode {
    Deleted,
    AllFiles,
    Existing,
    Signatures,
}
impl ScanMode {
    pub fn includes(self, deleted: bool) -> bool {
        match self {
            Self::Deleted => deleted,
            Self::Existing => !deleted,
            Self::AllFiles | Self::Signatures => true,
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: u32,
    pub name: String,
    pub size: u64,
    pub deleted: bool,
    pub method: String,
    pub note: String,
    pub supported: bool,
    pub directory: Option<String>,
    pub directory_note: String,
    pub metadata: crate::file_metadata::Metadata,
    #[serde(skip)]
    pub locator: Locator,
}
/// Fresh metadata accompanies bytes extracted from a revalidated record. It is
/// never reconstructed from a client or an imported scan's display metadata.
pub struct Extraction {
    pub bytes: u64,
    pub metadata: crate::file_metadata::Metadata,
}
#[derive(Clone)]
pub enum Locator {
    Btrfs(crate::btrfs_recovery::Location),
    Ext(crate::ext_recovery::Location),
    Fat(crate::fat_recovery::Location),
    Ntfs {
        record: u64,
        sequence: u16,
        link: crate::ntfs_names::Name,
        compression: Option<[u8; 32]>,
        wof: Option<[u8; 32]>,
        stream: Option<crate::ntfs_streams::Identity>,
    },
    Signature {
        offset: u64,
        header: Vec<u8>,
        footer: Vec<u8>,
        content_hash: Option<[u8; 32]>,
        allocation_digest: Option<String>,
    },
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
pub fn scan_filesystem_owned<R: Read + Seek + 'static>(
    mut reader: R,
    mode: ScanMode,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    let size = reader.seek(SeekFrom::End(0)).map_err(err)?;
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    if crate::bitlocker::boot_marker(&boot) {
        return Err(crate::bitlocker::CIPHERTEXT_NOTICE.into());
    }
    // FAT/exFAT data or boot-code bytes can coincidentally contain EF53 at
    // offset 1080. Prefer their boot structures; each engine still fully
    // validates its own geometry/checksums before publishing candidates.
    let fat_boot = fat_boot(&boot);
    if &boot[3..11] != b"NTFS    " && !fat_boot && size >= 2048 {
        reader.seek(SeekFrom::Start(1080)).map_err(err)?;
        let mut magic = [0; 2];
        reader.read_exact(&mut magic).map_err(err)?;
        if magic == [0x53, 0xef] {
            return crate::ext_recovery::scan(reader, mode, cancel, tick, found, directory);
        }
        if disk_core::btrfs::has_magic(&mut reader, size)? {
            return crate::btrfs_recovery::scan(reader, mode, &cancel, tick, found, directory);
        }
    }
    scan_filesystem_with_directories(&mut reader, mode, tick, found, directory)
}
pub fn fat_boot(boot: &[u8; 512]) -> bool {
    let sector = u16::from_le_bytes([boot[11], boot[12]]);
    !crate::bitlocker::boot_marker(boot)
        && boot[510..] == [0x55, 0xaa]
        && (boot[3..11] == *b"EXFAT   "
            || (matches!(sector, 512 | 1024 | 2048 | 4096)
                && boot[13].is_power_of_two()
                && boot[13] <= 128
                && u16::from_le_bytes([boot[14], boot[15]]) != 0
                && matches!(boot[16], 1 | 2)))
}
pub fn extract_owned<R: Read + Seek + 'static, W: Write>(
    reader: R,
    candidate: &Candidate,
    out: &mut W,
    limit: u64,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    extract_owned_with_metadata(reader, candidate, out, limit, cancel, tick).map(|e| e.bytes)
}
pub fn extract_owned_with_metadata<R: Read + Seek + 'static, W: Write>(
    mut reader: R,
    candidate: &Candidate,
    out: &mut W,
    limit: u64,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Extraction, String> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("提取已取消。".into());
    }
    if let Locator::Ext(location) = &candidate.locator {
        return crate::ext_recovery::extract_with_metadata(
            reader, candidate, location, out, limit, cancel, tick,
        );
    }
    if let Locator::Btrfs(location) = &candidate.locator {
        if !candidate.supported {
            return Err(candidate.note.clone());
        }
        return crate::btrfs_recovery::extract(
            reader, candidate, location, out, limit, &cancel, tick,
        );
    }
    extract_with_metadata(&mut reader, candidate, out, limit, tick)
}
#[allow(dead_code)] // Compatibility entry point for the guest acceptance driver.
pub fn scan_filesystem<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    scan_filesystem_with_directories(reader, mode, tick, found, |_| Ok(()))
}
pub fn scan_filesystem_with_directories<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    if crate::bitlocker::boot_marker(&boot) {
        return Err(crate::bitlocker::CIPHERTEXT_NOTICE.into());
    }
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    if &boot[3..11] == b"NTFS    " {
        scan_ntfs_with_directories(reader, mode, tick, found, directory)
    } else {
        crate::fat_recovery::scan_with_directories(reader, mode, tick, found, directory)
    }
}

#[cfg(test)]
#[test]
fn bitlocker_marker_rejects_ciphertext_before_filesystem_fallbacks() {
    let mut bytes = vec![0; 4096];
    bytes[3..11].copy_from_slice(b"-FVE-FS-");
    bytes[11..17].copy_from_slice(&[0, 2, 8, 32, 0, 2]);
    bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
    bytes[1080..1082].copy_from_slice(&[0x53, 0xef]);
    assert!(!fat_boot(bytes[..512].try_into().unwrap()));
    let mut source = std::io::Cursor::new(bytes);
    let result = scan_filesystem(
        &mut source,
        ScanMode::Existing,
        |_, _| Ok(()),
        |_| panic!("encrypted bytes became a file candidate"),
    );
    assert_eq!(result.unwrap_err(), crate::bitlocker::CIPHERTEXT_NOTICE);
    let result = scan_filesystem_owned(
        source,
        ScanMode::AllFiles,
        Default::default(),
        |_, _| Ok(()),
        |_| panic!("encrypted bytes became a file candidate"),
        |_| panic!("encrypted bytes became a directory"),
    );
    assert_eq!(result.unwrap_err(), crate::bitlocker::CIPHERTEXT_NOTICE);
}

#[allow(dead_code)] // Compatibility entry point for the guest acceptance driver.
pub fn scan_ntfs<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    scan_ntfs_with_directories(reader, mode, tick, found, |_| Ok(()))
}
pub fn scan_ntfs_with_directories<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory_found: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    crate::ntfs_scan::continue_scan(
        reader,
        mode,
        &mut crate::ntfs_scan::Checkpoint::default(),
        tick,
        found,
        directory_found,
    )
}

#[allow(dead_code)] // Compatibility entry point for the guest acceptance driver.
pub fn scan_signatures<R: Read + Seek>(
    reader: &mut R,
    size: u64,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    crate::signature_scan::scan(
        reader,
        size,
        &crate::signature_scan::Options::default(),
        tick,
        found,
    )
}
pub fn extract<R: Read + Seek, W: Write>(
    reader: &mut R,
    c: &Candidate,
    out: &mut W,
    limit: u64,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    extract_with_metadata(reader, c, out, limit, tick).map(|e| e.bytes)
}
pub fn extract_with_metadata<R: Read + Seek, W: Write>(
    reader: &mut R,
    c: &Candidate,
    out: &mut W,
    limit: u64,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Extraction, String> {
    if !c.supported {
        return Err(c.note.clone());
    }
    let take = c.size.min(limit);
    let mut done = 0;
    let mut buffer = vec![0; 1024 * 1024];
    let mut metadata = crate::file_metadata::Metadata::default();
    match &c.locator {
        Locator::Btrfs(location) => {
            return crate::btrfs_recovery::extract(
                reader,
                c,
                location,
                out,
                limit,
                &std::sync::atomic::AtomicBool::new(false),
                tick,
            );
        }
        Locator::Ext(_) => return Err("Ext 提取需要使用拥有源读取流的入口。".into()),
        Locator::Fat(location) => {
            return crate::fat_recovery::extract_with_metadata(
                reader, c, location, out, limit, tick,
            )
        }
        Locator::Ntfs {
            record,
            sequence,
            link,
            compression,
            wof,
            stream,
        } => {
            let ntfs = Ntfs::new(reader).map_err(err)?;
            let file = ntfs.file(reader, *record).map_err(err)?;
            if file.sequence_number() != *sequence
                || file.is_directory()
                || !crate::ntfs_names::read(reader, &file)?.contains(link)
                || stream
                    .as_ref()
                    .map_or_else(|| link.display(), |s| s.display(&link.display()))
                    != c.name
                || (!file.flags().contains(NtfsFileFlags::IN_USE)) != c.deleted
            {
                return Err("文件记录已被复用或状态已变化，请重新扫描。".into());
            }
            if let Some(signature) = wof {
                if compression.is_some() || stream.is_some() {
                    return Err("NTFS WOF 候选身份冲突。".into());
                }
                let plan = crate::ntfs_wof::Plan::read(reader, &file)?;
                if plan.size() != c.size || &plan.signature != signature {
                    return Err("NTFS WOF 文件属性或卷身份已变化，请重新扫描。".into());
                }
                metadata = crate::file_metadata::ntfs(&file);
                return plan.extract(reader, &file, out, limit, tick)
                    .map(|bytes| Extraction { bytes, metadata });
            }
            if let Some(stream) = stream {
                if compression.is_some() {
                    return Err("数据流候选身份冲突".into());
                }
                let streams = crate::ntfs_streams::enumerate(reader, &file)?;
                let actual = streams
                    .iter()
                    .find(|s| s.identity == *stream)
                    .ok_or("数据流身份已变化")?;
                if actual.size != c.size || actual.reason.is_some() {
                    return Err("数据流容量或支持状态已变化".into());
                }
                metadata = crate::file_metadata::ntfs(&file);
                return crate::ntfs_streams::extract(reader, &file, stream, out, limit, tick)
                    .map(|bytes| Extraction { bytes, metadata });
            }
            let item = file
                .data(reader, "")
                .ok_or("文件数据已不可访问")?
                .map_err(err)?;
            let attr = item.to_attribute().map_err(err)?;
            if attr.value_length() != c.size
                || attr.flags().contains(NtfsAttributeFlags::ENCRYPTED)
                || attr.flags().contains(NtfsAttributeFlags::COMPRESSED) != compression.is_some()
                || attr.record_bytes()[12] > 1
                || crate::ntfs_compression::is_reparse(reader, &file)?
            {
                return Err("文件属性已改变或不支持。".into());
            }
            metadata = crate::file_metadata::ntfs(&file);
            if let Some(signature) = compression {
                let plan = crate::ntfs_compression::Plan::read(reader, &file)?;
                if &plan.signature != signature {
                    return Err("压缩文件布局或卷身份已变化，请重新扫描。".into());
                }
                return plan
                    .extract(reader, out, limit, tick)
                    .map(|bytes| Extraction { bytes, metadata });
            }
            let mut value = attr.value(reader).map_err(err)?;
            while done < take {
                tick(done)?;
                let n = (take - done).min(buffer.len() as u64) as usize;
                value.read_exact(reader, &mut buffer[..n]).map_err(err)?;
                out.write_all(&buffer[..n]).map_err(err)?;
                done += n as u64;
            }
        }
        Locator::Signature {
            offset,
            header,
            footer,
            content_hash,
            ..
        } => {
            if let Some(expected) = content_hash {
                let actual = crate::signature_zip::hash_range(reader, *offset, c.size, || tick(0))?;
                if &actual != expected {
                    return Err("候选内容已变化，请重新扫描。".into());
                }
            }
            let mut check = vec![0; header.len()];
            reader.seek(SeekFrom::Start(*offset)).map_err(err)?;
            reader.read_exact(&mut check).map_err(err)?;
            if check != *header {
                return Err("文件起始签名已变化，请重新扫描。".into());
            }
            check.resize(footer.len(), 0);
            reader
                .seek(SeekFrom::Start(offset + c.size - footer.len() as u64))
                .map_err(err)?;
            reader.read_exact(&mut check).map_err(err)?;
            if check != *footer {
                return Err("文件结束签名已变化，请重新扫描。".into());
            }
            reader.seek(SeekFrom::Start(*offset)).map_err(err)?;
            while done < take {
                tick(done)?;
                let n = (take - done).min(buffer.len() as u64) as usize;
                reader.read_exact(&mut buffer[..n]).map_err(err)?;
                out.write_all(&buffer[..n]).map_err(err)?;
                done += n as u64;
            }
        }
    }
    tick(done)?;
    Ok(Extraction {
        bytes: done,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_ntfs_scan_uses_standard_information_and_keeps_exact_content() {
        let original = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let mut r = Cursor::new(original.as_slice());
        let ntfs = Ntfs::new(&mut r).unwrap();
        let mut values = vec![];
        scan_ntfs(
            &mut r,
            ScanMode::Existing,
            |_, _| Ok(()),
            |c| values.push(c),
        )
        .unwrap();
        let c = values.iter().find(|c| c.name == "file-with-12345").unwrap();
        let Locator::Ntfs { record, .. } = c.locator else {
            panic!()
        };
        let f = ntfs.file(&mut r, record).unwrap();
        let attr = f
            .attributes_raw()
            .map(Result::unwrap)
            .find(|a| a.ty().unwrap() == ntfs::NtfsAttributeType::StandardInformation)
            .unwrap();
        let at = attr.position().value().unwrap().get() as usize
            + u16::from_le_bytes(attr.record_bytes()[20..22].try_into().unwrap()) as usize;
        assert!(at % 512 < 464);
        let mut bytes = original.to_vec();
        bytes[at..at + 8].copy_from_slice(&116444736015000000u64.to_le_bytes());
        bytes[at + 32..at + 36].copy_from_slice(&3u32.to_le_bytes());
        values.clear();
        scan_ntfs(
            &mut Cursor::new(&bytes),
            ScanMode::Existing,
            |_, _| Ok(()),
            |c| values.push(c),
        )
        .unwrap();
        let c = values.iter().find(|c| c.name == "file-with-12345").unwrap();
        assert_eq!(c.metadata.source.as_deref(), Some("NTFS 标准信息"));
        assert_eq!(
            c.metadata.created.as_ref().unwrap().text,
            "1970-01-01 00:00:01.5"
        );
        assert_eq!(c.metadata.attributes, ["只读", "隐藏"]);
        let mut c = c.clone();
        // Imported display metadata is not trusted for output attributes.
        c.metadata = crate::file_metadata::Metadata::default();
        let mut out = vec![];
        let receipt =
            extract_with_metadata(&mut Cursor::new(bytes), &c, &mut out, u64::MAX, |_| Ok(()))
                .unwrap();
        assert_eq!(receipt.bytes, 5);
        assert_eq!(receipt.metadata.windows_flags, Some(3));
        assert_eq!(
            receipt.metadata.created.unwrap().instant_ns,
            Some(1_500_000_000)
        );
        assert_eq!(out, b"12345");
    }
    #[test]
    fn browse_existing_ntfs_excludes_deleted_records_and_deleted_parents() {
        let original = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let collect = |bytes: Vec<u8>, mode| {
            let mut files = vec![];
            let mut dirs = vec![];
            scan_filesystem_owned(
                Cursor::new(bytes),
                mode,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                |_, _| Ok(()),
                |c| files.push(c),
                |d| {
                    dirs.push(d);
                    Ok(())
                },
            )
            .unwrap();
            (files, dirs)
        };
        let (files, dirs) = collect(original.to_vec(), ScanMode::Existing);
        assert!(!files.is_empty() && files.iter().all(|c| !c.deleted));
        assert!(dirs.iter().all(|d| !d.deleted));
        assert!(files.iter().any(|c| c.name == "$MFT"));
        let c = files.iter().find(|c| c.name == "file-with-12345").unwrap();
        let mut out = vec![];
        extract(
            &mut Cursor::new(original.as_slice()),
            c,
            &mut out,
            u64::MAX,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(out, b"12345");
        let Locator::Ntfs { record, .. } = c.locator else {
            panic!()
        };
        let mut r = Cursor::new(original.as_slice());
        let ntfs = Ntfs::new(&mut r).unwrap();
        let at = ntfs
            .file(&mut r, record)
            .unwrap()
            .position()
            .value()
            .unwrap()
            .get() as usize;
        let mut deleted = original.to_vec();
        deleted[at + 22] &= !1;
        assert!(!collect(deleted.clone(), ScanMode::Existing)
            .0
            .iter()
            .any(|c| c.name == "file-with-12345"));
        assert!(collect(deleted, ScanMode::AllFiles)
            .0
            .iter()
            .any(|c| c.name == "file-with-12345" && c.deleted));
        // Known fixture directory contains live descendants. Its deletion must
        // not publish a deleted directory in an existing-file browser.
        let dir = dirs.iter().find(|d| d.name == "many_subdirs").unwrap();
        let number: u64 = dir.id.split('-').nth(1).unwrap().parse().unwrap();
        let at = ntfs
            .file(&mut r, number)
            .unwrap()
            .position()
            .value()
            .unwrap()
            .get() as usize;
        let mut bytes = original.to_vec();
        bytes[at + 22] &= !1;
        let owner = ntfs.file(&mut r, number).unwrap();
        let reference = number | u64::from(owner.sequence_number()) << 48;
        let child = ntfs.file(&mut r, record).unwrap();
        let a = child
            .attributes_raw()
            .map(Result::unwrap)
            .find(|a| a.ty().unwrap() == ntfs::NtfsAttributeType::FileName)
            .unwrap();
        let value = usize::from(u16::from_le_bytes(
            a.record_bytes()[20..22].try_into().unwrap(),
        ));
        let at = a.position().value().unwrap().get() as usize + value;
        assert!(at % 512 < 504);
        bytes[at..at + 8].copy_from_slice(&reference.to_le_bytes());
        let (files, dirs) = collect(bytes, ScanMode::Existing);
        assert!(dirs.iter().all(|d| !d.deleted && d.id != dir.id));
        assert!(files.iter().any(|c| c.name == "file-with-12345"
            && c.directory.is_none()
            && !c.directory_note.is_empty()));
    }
    #[test]
    fn real_ntfs_metadata_keeps_nested_paths_and_rejects_reused_or_cyclic_parents() {
        use ntfs::NtfsAttributeType;
        use std::io::Cursor;
        let original = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
        let mut reader = Cursor::new(original.as_slice());
        let ntfs = Ntfs::new(&mut reader).unwrap();
        let mut file_record = None;
        let mut directory_record = None;
        for number in 16..1024 {
            let Ok(file) = ntfs.file(&mut reader, number) else {
                continue;
            };
            let Some(Ok(name)) = file.name(&mut reader, None, None) else {
                continue;
            };
            if name.name().to_string_lossy() == "file-with-12345" {
                file_record = Some(number);
            }
            if file.is_directory() && name.name().to_string_lossy() == "1" {
                directory_record = Some(number);
            }
        }
        let file_record = file_record.unwrap();
        let directory_record = directory_record.unwrap();
        let sequence = ntfs
            .file(&mut reader, directory_record)
            .unwrap()
            .sequence_number();
        let mut name_offset = |record| {
            let file = ntfs.file(&mut reader, record).unwrap();
            let mut attrs = file.attributes();
            while let Some(attr) = attrs.next(&mut reader) {
                let item = attr.unwrap();
                let attr = item.to_attribute().unwrap();
                if attr.ty().unwrap() == NtfsAttributeType::FileName {
                    assert!(attr.is_resident());
                    let at = attr.position().value().unwrap().get() as usize;
                    // ntfs 0.4.0 reports the attribute header position for a
                    // resident value. Read its on-disk value offset explicitly,
                    // so this fixture edit changes only the parent reference.
                    assert_eq!(&original[at..at + 4], &0x30u32.to_le_bytes());
                    let offset =
                        u16::from_le_bytes(original[at + 20..at + 22].try_into().unwrap()) as usize;
                    assert!(offset >= 24);
                    return at + offset;
                }
            }
            panic!("fixture lacks FILE_NAME");
        };
        let child_at = name_offset(file_record);
        let parent_at = name_offset(directory_record);
        assert!(child_at % 512 < 504 && parent_at % 512 < 504);
        for variant in 0..3 {
            let mut bytes = original.to_vec();
            let reference = directory_record | ((sequence as u64 + u64::from(variant == 1)) << 48);
            bytes[child_at..child_at + 8].copy_from_slice(&reference.to_le_bytes());
            if variant == 2 {
                bytes[parent_at..parent_at + 8].copy_from_slice(&reference.to_le_bytes());
            }
            let mut catalog = crate::recovery_tree::Catalog::default();
            let mut candidates = Vec::new();
            scan_filesystem_owned(
                Cursor::new(bytes.clone()),
                ScanMode::AllFiles,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                |_, _| Ok(()),
                |c| candidates.push(c),
                |d| catalog.insert(d),
            )
            .unwrap();
            let c = candidates
                .iter()
                .find(|c| c.name == "file-with-12345")
                .unwrap_or_else(|| {
                    panic!(
                        "variant {variant}; candidates: {:?}",
                        candidates.iter().map(|c| &c.name).collect::<Vec<_>>()
                    )
                });
            if variant == 0 {
                let names: Vec<_> = catalog
                    .path(c.directory.as_deref().unwrap())
                    .unwrap()
                    .into_iter()
                    .map(|d| d.name)
                    .collect();
                assert_eq!(names, ["根目录", "many_subdirs", "1"]);
            } else {
                assert!(c.directory.is_none());
                assert!(c
                    .directory_note
                    .contains(if variant == 1 { "复用" } else { "循环" }));
            }
            let mut extracted = Vec::new();
            extract_owned(
                Cursor::new(bytes),
                c,
                &mut extracted,
                99,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(extracted, b"12345");
        }
    }
    use std::io::Cursor;
    #[test]
    fn signature_spanning_chunks_recovers_exact_bytes() {
        let payload = b"%PDF-1.7\nactual fixture payload\n%%EOF";
        let mut bytes = vec![0; 1024 * 1024 - 3];
        let start = bytes.len();
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&[0; 32]);
        let mut results = vec![];
        let size = bytes.len() as u64;
        scan_signatures(
            &mut Cursor::new(&bytes),
            size,
            |_, _| Ok(()),
            |c| results.push(c),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].size, payload.len() as u64);
        let mut out = vec![];
        extract(
            &mut Cursor::new(&bytes),
            &results[0],
            &mut out,
            u64::MAX,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(out, payload);
        bytes[start] = 0;
        assert!(extract(
            &mut Cursor::new(bytes),
            &results[0],
            &mut vec![],
            100,
            |_| Ok(())
        )
        .is_err());
    }
    #[test]
    fn unclosed_candidate_not_reported_as_file() {
        let mut count = 0;
        scan_signatures(
            &mut Cursor::new(b"%PDF-truncated"),
            14,
            |_, _| Ok(()),
            |_| count += 1,
        )
        .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn scan_cancellation_stops_without_results() {
        assert!(scan_signatures(
            &mut Cursor::new(vec![0; 1024]),
            1024,
            |_, _| Err("cancel".into()),
            |_| panic!()
        )
        .is_err());
    }
}

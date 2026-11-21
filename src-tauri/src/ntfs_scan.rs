//! Resumable NTFS MFT enumeration. Commit one complete base record and all its aliases together.
use crate::recovery::{Candidate, Locator, ScanMode, MAX_RESULTS};
use crate::recovery_tree::{Directory, NtfsDirectory, NtfsResolver};
use ntfs::{Ntfs, NtfsAttributeFlags, NtfsFileFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
// Keep the MFT header and current record window together. The NTFS parser
// revisits record 0 for every file; a single-window device reader otherwise
// thrashes once the table exceeds that window. This cache lives for ONE scan
// invocation only; resume verification uses the original reader first.
struct Page {
    start: u64,
    valid: usize,
    bytes: Vec<u8>,
}
pub(crate) struct ScanRead<'a, R> {
    inner: &'a mut R,
    position: u64,
    size: u64,
    pages: [Page; 2],
}
impl<'a, R: Read + Seek> ScanRead<'a, R> {
    pub(crate) fn new(inner: &'a mut R) -> std::io::Result<Self> {
        let position = inner.stream_position()?;
        let size = inner.seek(SeekFrom::End(0))?;
        Ok(Self {
            inner,
            position,
            size,
            pages: std::array::from_fn(|_| Page {
                start: u64::MAX,
                valid: 0,
                bytes: vec![0; 1024 * 1024],
            }),
        })
    }
}
impl<R: Read + Seek> Read for ScanRead<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        let contains = |p: &Page| {
            p.valid > 0 && self.position >= p.start && self.position - p.start < p.valid as u64
        };
        if !contains(&self.pages[0]) {
            let cached = contains(&self.pages[1]);
            self.pages.swap(0, 1);
            if !cached {
                let p = &mut self.pages[0];
                p.valid = 0;
                p.start = u64::MAX;
                let start = self.position / p.bytes.len() as u64 * p.bytes.len() as u64;
                let n = (self.size - start).min(p.bytes.len() as u64) as usize;
                self.inner.seek(SeekFrom::Start(start))?;
                self.inner.read_exact(&mut p.bytes[..n])?;
                p.start = start;
                p.valid = n;
            }
        }
        let p = &self.pages[0];
        let offset = (self.position - p.start) as usize;
        let n = out.len().min(p.valid - offset);
        out[..n].copy_from_slice(&p.bytes[offset..offset + n]);
        self.position += n as u64;
        Ok(n)
    }
}
impl<R: Read + Seek> Seek for ScanRead<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let n = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => self.size as i128 + n as i128,
        };
        if n < 0 || n > self.size as i128 {
            return Err(std::io::Error::other("Seek outside NTFS scan source"));
        }
        self.position = n as u64;
        Ok(self.position)
    }
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub next: u64,
    pub total: u64,
    pub count: usize,
    skipped: usize,
    directory_errors: usize,
    aliases: usize,
    identity: Option<[u8; 32]>,
    last: Option<(u64, [u8; 32])>,
    pub sealed: bool,
}
fn record_digest(
    reader: &mut (impl Read + Seek),
    ntfs: &Ntfs,
    record: u64,
) -> Result<[u8; 32], String> {
    let file = ntfs.file(reader, record).map_err(err)?;
    let at = file.position().value().ok_or("MFT 记录位置不可用")?.get();
    let size = ntfs.file_record_size();
    if !(512..=65536).contains(&size) || !size.is_power_of_two() {
        return Err("不支持的 MFT 记录大小".into());
    }
    reader.seek(SeekFrom::Start(at)).map_err(err)?;
    let mut b = vec![0; size as usize];
    reader.read_exact(&mut b).map_err(err)?;
    Ok(Sha256::digest(&b).into())
}
fn identity(reader: &mut (impl Read + Seek), ntfs: &Ntfs) -> Result<([u8; 32], u64), String> {
    let mft = ntfs.file(reader, 0).map_err(err)?;
    let item = mft
        .data(reader, "")
        .ok_or("NTFS $MFT 数据缺失")?
        .map_err(err)?;
    let attr = item.to_attribute().map_err(err)?;
    let size = ntfs.file_record_size() as u64;
    if size == 0 || attr.value_length() % size != 0 {
        return Err("MFT 长度无效".into());
    }
    let records = attr.value_length() / size;
    if records < 16 || records > 16_000_000 {
        return Err("MFT 记录数超出 16 至 1600 万范围".into());
    }
    let mut hash = Sha256::new();
    reader.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    hash.update(boot);
    hash.update(attr.record_bytes());
    hash.update(record_digest(reader, ntfs, 0)?);
    Ok((hash.finalize().into(), records))
}
impl Checkpoint {
    pub fn validate(&self, mode: ScanMode) -> Result<(), String> {
        let first = if matches!(mode, ScanMode::Existing) {
            0
        } else {
            16
        };
        if matches!(mode, ScanMode::Signatures)
            || self.identity.is_none()
            || !(16..=16_000_000).contains(&self.total)
            || self.next < first
            || self.next > self.total
            || self.count > MAX_RESULTS
            || (!self.sealed && (self.next == self.total || self.count == MAX_RESULTS))
            || self.skipped as u64 > self.next
            || self.directory_errors as u64 > self.next
            || self.aliases > MAX_RESULTS + 2048
            || self
                .last
                .as_ref()
                .is_some_and(|(n, _)| n.checked_add(1) != Some(self.next))
        {
            return Err("NTFS 续扫位置或计数无效".into());
        }
        Ok(())
    }
    pub fn verify(&self, reader: &mut (impl Read + Seek), mode: ScanMode) -> Result<(), String> {
        self.validate(mode)?;
        let ntfs = Ntfs::new(reader).map_err(err)?;
        let (hash, records) = identity(reader, &ntfs)?;
        if self.identity != Some(hash) || self.total != records {
            return Err("NTFS 引导信息或 MFT 布局已改变，请重新扫描".into());
        }
        if let Some((record, expected)) = self.last {
            if record_digest(reader, &ntfs, record)? != expected {
                return Err("NTFS 续扫边界记录已变化，请重新扫描".into());
            }
        }
        Ok(())
    }
}
pub(crate) fn scan_reconstructed<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    let fs = Ntfs::new(reader).map_err(err)?;
    let (identity, total) = identity(reader, &fs)?;
    // Records 0–15 describe system metadata. The recovery view can contain
    // substituted bytes there, so these do not become export candidates.
    let mut checkpoint = Checkpoint { identity: Some(identity), next: 16, total, ..Checkpoint::default() };
    continue_scan(reader, mode, &mut checkpoint, tick, found, directory)
}

pub fn continue_scan<R: Read + Seek>(
    reader: &mut R,
    mode: ScanMode,
    cursor: &mut Checkpoint,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory_found: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    if matches!(mode, ScanMode::Signatures) {
        return Err("NTFS 扫描模式无效".into());
    }
    let ntfs = Ntfs::new(reader)
        .map_err(|e| format!("无法读取 NTFS 文件表：{e}。可尝试文件签名扫描。"))?;
    let (hash, records) = identity(reader, &ntfs)?;
    if cursor.identity.is_none() {
        *cursor = Checkpoint {
            identity: Some(hash),
            total: records,
            next: if matches!(mode, ScanMode::Existing) {
                0
            } else {
                16
            },
            ..Checkpoint::default()
        };
    }
    cursor.validate(mode)?;
    if cursor.sealed {
        return Err("NTFS 扫描已结束，不能继续".into());
    }
    if cursor.identity != Some(hash) || cursor.total != records {
        return Err("NTFS MFT 布局已改变".into());
    }
    if let Some((n, digest)) = cursor.last {
        if record_digest(reader, &ntfs, n)? != digest {
            return Err("NTFS 续扫边界记录已变化".into());
        }
    }
    let (mut count, mut skipped, mut directory_errors, mut aliases) = (
        cursor.count,
        cursor.skipped,
        cursor.directory_errors,
        cursor.aliases,
    );
    let mut directories = NtfsResolver::default();
    let mut cached = ScanRead::new(reader).map_err(err)?;
    let reader = &mut cached;
    while cursor.next < records {
        let record = cursor.next;
        tick(record, records)?;
        let boundary = record_digest(reader, &ntfs, record).ok();
        let mut pending_files = Vec::new();
        let mut pending_directories = Vec::new();
        'record: {
            let file = match ntfs.file(reader, record) {
                Ok(f) => f,
                Err(_) => {
                    skipped += 1;
                    break 'record;
                }
            };
            match crate::ntfs_names::base_reference(reader, &file) {
                Ok(0) => {}
                Ok(_) => break 'record,
                Err(_) => {
                    skipped += 1;
                    break 'record;
                }
            }
            let deleted = !file.flags().contains(NtfsFileFlags::IN_USE);
            if !mode.includes(deleted) {
                break 'record;
            }
            if file.is_directory() {
                // Register directories even if they contain no candidate files.
                if resolve_ntfs_directory(
                    &ntfs,
                    reader,
                    &mut directories,
                    (record, file.sequence_number()),
                    !matches!(mode, ScanMode::Existing),
                    &mut |d| {
                        pending_directories.push(d);
                        Ok(())
                    },
                )
                .is_err()
                {
                    directory_errors += 1;
                }
                break 'record;
            }
            let names = match crate::ntfs_names::read(reader, &file) {
                Ok(names) if !names.is_empty() => names,
                _ => {
                    skipped += 1;
                    break 'record;
                }
            };
            struct Content {
                size: u64,
                compression: Option<[u8; 32]>,
                wof: Option<[u8; 32]>,
                stream: Option<crate::ntfs_streams::Identity>,
                unsupported: Option<String>,
            }
            let mut contents = vec![];
            let mut wof = None;
            if let Some(data) = file.data(reader, "") {
                let data = match data {
                    Ok(d) => d,
                    Err(_) => {
                        skipped += 1;
                        break 'record;
                    }
                };
                let attr = match data.to_attribute() {
                    Ok(a) => a,
                    Err(_) => {
                        skipped += 1;
                        break 'record;
                    }
                };
                let mut compression = None;
                let unsupported = if crate::ntfs_compression::is_reparse(reader, &file)
                    .unwrap_or(true)
                {
                    match crate::ntfs_wof::Plan::read(reader, &file) {
                        Ok(plan) if plan.size() == attr.value_length() => {
                            wof = Some(plan.signature);
                            None
                        }
                        Ok(_) => Some("NTFS WOF 逻辑容量不一致。".into()),
                        Err(e) => Some(format!("无法读取 NTFS 重解析文件：{e}")),
                    }
                } else if attr.flags().contains(NtfsAttributeFlags::ENCRYPTED) {
                    Some("EFS 加密数据暂不支持".into())
                } else if attr.record_bytes()[12] > 1 {
                    Some("未知 NTFS 压缩方法暂不支持".into())
                } else if attr.flags().contains(NtfsAttributeFlags::COMPRESSED) {
                    match crate::ntfs_compression::Plan::read(reader, &file) {
                        Ok(plan) => {
                            compression = Some(plan.signature);
                            None
                        }
                        Err(e) => Some(format!("NTFS 压缩布局暂不支持或损坏：{e}")),
                    }
                } else {
                    None
                };
                contents.push(Content {
                    size: attr.value_length(),
                    compression,
                    wof,
                    stream: None,
                    unsupported,
                });
            }
            match crate::ntfs_streams::enumerate(reader, &file) {
                Ok(streams) => {
                    for s in streams {
                        if wof.is_some() && s.identity.name == crate::ntfs_wof::STREAM {
                            continue;
                        }
                        contents.push(Content {
                            size: s.size,
                            compression: None,
                            wof: None,
                            stream: Some(s.identity),
                            unsupported: s.reason,
                        });
                    }
                }
                Err(_) => {
                    skipped += 1;
                }
            }
            let metadata = crate::file_metadata::ntfs(&file);
            let multiple = names.len() > 1;
            aliases += names.len().saturating_sub(1);
            for link in names {
                let path = resolve_ntfs_directory(
                    &ntfs,
                    reader,
                    &mut directories,
                    link.parent,
                    !matches!(mode, ScanMode::Existing),
                    &mut |d| {
                        pending_directories.push(d);
                        Ok(())
                    },
                );
                let (directory, directory_note) = match path {
                    Ok(id) => (Some(id), String::new()),
                    Err(reason) => (None, reason),
                };
                let mut notes = Vec::new();
                if link.namespace == 2 {
                    notes.push("DOS 短名称".to_string());
                }
                if multiple {
                    notes.push("同一文件有多个名称；选中后分别保存副本".into());
                }
                if !directory_note.is_empty() {
                    notes.push(directory_note);
                }
                for Content { size, compression, wof, stream, unsupported } in &contents {
                    let supported = unsupported.is_none();
                    pending_files.push(Candidate {
                    id: count as u32,
                    metadata: metadata.clone(),
                    name: stream.as_ref().map_or_else(|| link.display(), |s| s.display(&link.display())),
                    size: *size,
                    deleted,
                    method: if stream.is_some() {
                        "NTFS 命名数据流"
                    } else if wof.is_some() {
                        "NTFS WOF 压缩文件"
                    } else {
                        "NTFS 文件表"
                    }.into(),
                    note: if let Some(reason) = unsupported {
                        reason.as_str()
                    } else if stream.is_some() {
                        "命名数据流；单独保存为普通文件，不附加到原文件。属性时间属于所属文件；内容可能变化，删除内容可能被覆盖"
                    } else if compression.is_some() {
                        "NTFS LZNT1 压缩文件；提取时解码，内容可能已变化或被覆盖"
                    } else if wof.is_some() {
                        "提取时解码卷内 WOF 压缩数据；内容可能已变化或被覆盖"
                    } else if deleted {
                        "可能已被覆盖；请先预览并核对恢复内容"
                    } else {
                        "现存文件；恢复时源文件可能变化"
                    }
                    .into(),
                    supported,
                    directory: directory.clone(),
                    directory_note: notes.join("；"),
                    locator: Locator::Ntfs {
                        record,
                        sequence: file.sequence_number(),
                        link: link.clone(),
                        compression: *compression,
                        wof: *wof,
                        stream: stream.clone(),
                    },
                });
                    count += 1;
                    if count >= MAX_RESULTS {
                        break 'record;
                    }
                }
            }
        }
        // Validate directory publication first. On failure no file or cursor is committed.
        for d in pending_directories {
            directory_found(d)?;
        }
        for c in pending_files {
            found(c);
        }
        *cursor = Checkpoint {
            next: record + 1,
            total: records,
            count,
            skipped,
            directory_errors,
            aliases,
            identity: Some(hash),
            last: boundary.map(|h| (record, h)),
            sealed: record + 1 == records || count >= MAX_RESULTS,
        };
        if count >= MAX_RESULTS {
            return Ok(crate::native_locale::current().format(
                "达到 {limit} 条结果上限，NTFS 扫描提前结束；未能解析 {skipped} 条记录。",
                &[
                    ("limit", &MAX_RESULTS.to_string()),
                    ("skipped", &skipped.to_string()),
                ],
            ));
        }
    }
    tick(records, records)?;
    Ok(crate::native_locale::current().format(
        "扫描完成，找到 {count} 项文件名称/命名数据流（含 {aliases} 个额外名称）；未能解析 {skipped} 条文件记录或数据流、{directoryErrors} 个目录链路。命名数据流单独导出，暂不枚举目录附带的数据流；名称可能指向同一文件，目录未必完整，内容需恢复后核对。",
        &[("count", &count.to_string()), ("aliases", &aliases.to_string()),
          ("skipped", &skipped.to_string()), ("directoryErrors", &directory_errors.to_string())],
    ))
}
fn resolve_ntfs_directory<R: Read + Seek>(
    ntfs: &Ntfs,
    reader: &mut R,
    resolver: &mut NtfsResolver,
    reference: (u64, u16),
    allow_deleted: bool,
    found: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    resolver.resolve(
        reference,
        |record| {
            let parent = ntfs.file(reader, record).map_err(err)?;
            if !allow_deleted && !parent.flags().contains(NtfsFileFlags::IN_USE) {
                return Err("现存文件的父目录记录已删除，不能确认目录关系".into());
            }
            let named = crate::ntfs_names::directory(reader, &parent)?;
            let mut metadata = crate::file_metadata::ntfs(&parent);
            metadata.directory_proof = Some(crate::directory_metadata::Proof::Ntfs {
                boot: crate::directory_metadata::boot(reader)?,
                record,
                sequence: parent.sequence_number(),
                name: named.clone(),
            });
            Ok(NtfsDirectory {
                sequence: parent.sequence_number(),
                directory: parent.is_directory(),
                name: named.display(),
                deleted: !parent.flags().contains(NtfsFileFlags::IN_USE),
                parent: named.parent,
                metadata,
            })
        },
        found,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_tree::Catalog;
    use std::io::Cursor;
    const IMAGE: &[u8] = include_bytes!("../../tests/fixtures/ntfs-testfs1.img");
    #[test]
    fn scan_cache_bounds_reads_and_reuses_header_without_surviving_a_new_scan() {
        struct Count {
            inner: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for Count {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(b)?;
                self.read += n;
                Ok(n)
            }
        }
        impl Seek for Count {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(p)
            }
        }
        let mut bytes = vec![0; 4 * 1024 * 1024];
        bytes[137] = 17;
        bytes[3 * 1024 * 1024 + 91] = 29;
        let mut source = Count {
            inner: Cursor::new(bytes),
            read: 0,
        };
        {
            let mut device = crate::data_io::Region::new(&mut source, 0, 4 * 1024 * 1024).unwrap();
            let mut cache = ScanRead::new(&mut device).unwrap();
            for _ in 0..100 {
                for (at, expected) in [(137, 17), (3 * 1024 * 1024 + 91, 29)] {
                    cache.seek(SeekFrom::Start(at)).unwrap();
                    let mut b = [0];
                    cache.read_exact(&mut b).unwrap();
                    assert_eq!(b[0], expected);
                }
            }
            assert!(cache.seek(SeekFrom::End(1)).is_err());
            assert!(cache.seek(SeekFrom::Start(0)).is_ok());
            assert!(cache.seek(SeekFrom::Current(-1)).is_err());
            cache.seek(SeekFrom::End(-1)).unwrap();
            let mut b = [1; 4];
            assert_eq!(cache.read(&mut b).unwrap(), 1);
            assert_eq!(cache.read(&mut b).unwrap(), 0);
        }
        assert_eq!(source.read, 2 * 1024 * 1024);
        source.inner.get_mut()[137] = 71;
        let mut cache = ScanRead::new(&mut source).unwrap();
        cache.seek(SeekFrom::Start(137)).unwrap();
        let mut b = [0];
        cache.read_exact(&mut b).unwrap();
        assert_eq!(b[0], 71);
    }
    fn snapshot(files: &[Candidate], dirs: &Catalog) -> (String, String) {
        (
            serde_json::to_string(files).unwrap(),
            serde_json::to_string(&dirs.snapshot()).unwrap(),
        )
    }
    #[test]
    fn mft_resume_roundtrips_record_boundaries_and_preserves_directory_catalogue() {
        for mode in [ScanMode::Existing, ScanMode::AllFiles, ScanMode::Deleted] {
            let mut complete = Checkpoint::default();
            let mut full = vec![];
            let mut tree = Catalog::default();
            continue_scan(
                &mut Cursor::new(IMAGE),
                mode,
                &mut complete,
                |_, _| Ok(()),
                |c| full.push(c),
                |d| tree.insert(d),
            )
            .unwrap();
            let expected = snapshot(&full, &tree);
            for stop in [16, 17, 24, complete.total / 2, complete.total - 1] {
                let mut cp = Checkpoint::default();
                let mut files = vec![];
                let mut dirs = Catalog::default();
                assert!(continue_scan(
                    &mut Cursor::new(IMAGE),
                    mode,
                    &mut cp,
                    |n, _| if n >= stop {
                        Err("pause".into())
                    } else {
                        Ok(())
                    },
                    |c| files.push(c),
                    |d| dirs.insert(d)
                )
                .is_err());
                assert_eq!(cp.next, stop);
                assert_eq!(cp.count, files.len());
                assert!(!cp.sealed);
                let mut cp: Checkpoint =
                    serde_json::from_slice(&serde_json::to_vec(&cp).unwrap()).unwrap();
                cp.verify(&mut Cursor::new(IMAGE), mode).unwrap();
                let first = std::cell::Cell::new(None);
                continue_scan(
                    &mut Cursor::new(IMAGE),
                    mode,
                    &mut cp,
                    |n, _| {
                        if first.get().is_none() {
                            first.set(Some(n));
                        }
                        Ok(())
                    },
                    |c| files.push(c),
                    |d| dirs.insert(d),
                )
                .unwrap();
                assert_eq!(first.get(), Some(stop));
                assert!(cp.sealed);
                assert_eq!(snapshot(&files, &dirs), expected);
            }
        }
    }
    #[test]
    fn mft_identity_boundary_and_forged_cursors_fail_closed() {
        let mut cp = Checkpoint::default();
        let mode = ScanMode::Existing;
        assert!(continue_scan(
            &mut Cursor::new(IMAGE),
            mode,
            &mut cp,
            |n, _| if n == 6 { Err("pause".into()) } else { Ok(()) },
            |_| {},
            |_| Ok(())
        )
        .is_err());
        assert_eq!(cp.last.as_ref().map(|p| p.0), Some(5));
        let mut changed = IMAGE.to_vec();
        changed[72] ^= 1;
        assert!(cp.verify(&mut Cursor::new(&changed), mode).is_err());
        let mut source = Cursor::new(IMAGE);
        let ntfs = Ntfs::new(&mut source).unwrap();
        let at = ntfs
            .file(&mut source, 5)
            .unwrap()
            .position()
            .value()
            .unwrap()
            .get() as usize;
        let mut changed = IMAGE.to_vec();
        changed[at + 8] ^= 1;
        assert!(cp.verify(&mut Cursor::new(&changed), mode).is_err());
        for field in ["next", "total", "count", "skipped", "aliases"] {
            let mut v = serde_json::to_value(&cp).unwrap();
            v[field] = u64::MAX.into();
            let bad: Checkpoint = match serde_json::from_value(v) {
                Ok(x) => x,
                Err(_) => continue,
            };
            assert!(bad.validate(mode).is_err());
        }
        cp.sealed = true;
        assert!(continue_scan(
            &mut Cursor::new(IMAGE),
            mode,
            &mut cp,
            |_, _| Ok(()),
            |_| panic!("no candidate"),
            |_| Ok(())
        )
        .is_err());
    }
}

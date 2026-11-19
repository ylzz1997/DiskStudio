//! Committed Btrfs directories and files on a caller-bounded, read-only source.
use crate::{
    file_metadata::{self, Metadata},
    recovery::{Candidate, Extraction, Locator, ScanMode, MAX_RESULTS},
    recovery_tree::{Directory, MAX_DEPTH, ROOT},
};
use disk_core::btrfs::{self, DirectoryEntry, Inode, RetainedRoot, Snapshot, Tree, Volume};
use std::{
    collections::HashSet,
    io::{self, Read, Seek, Write},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Node {
    subvolume: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug)]
enum History {
    Snapshot(Snapshot),
    Retained(RetainedRoot),
}

impl History {
    fn tree(self, node: Node) -> Tree {
        match self {
            Self::Snapshot(_) => node.subvolume.into(),
            Self::Retained(root) => root.into(),
        }
    }

    fn origin(self) -> u64 {
        match self {
            Self::Snapshot(snapshot) => snapshot.origin,
            Self::Retained(root) => root.subvolume,
        }
    }

    fn transaction(self) -> u64 {
        match self {
            Self::Snapshot(snapshot) => snapshot.created,
            Self::Retained(root) => root.transaction,
        }
    }
}

impl Node {
    fn id(self) -> String {
        format!("btrfs-{}-{}", self.subvolume, self.inode)
    }
}

#[derive(Clone, Debug)]
pub struct Location {
    view: [u8; 32],
    parent: Node,
    entry: DirectoryEntry,
    node: Node,
    inode: Inode,
    history: Option<History>,
}

impl Location {
    fn tree(&self, node: Node) -> Tree {
        self.history
            .map_or(node.subvolume.into(), |history| history.tree(node))
    }
}

pub(crate) fn stop(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Btrfs 读取已取消。".into())
    } else {
        Ok(())
    }
}

fn target(
    volume: &Volume<impl Read + Seek>,
    from: Node,
    entry: &DirectoryEntry,
) -> Result<Node, String> {
    if entry.target.kind == 132 {
        let root = volume
            .inventory()
            .subvolumes
            .iter()
            .find(|r| r.id == entry.target.object_id)
            .ok_or("Btrfs 目录引用了不存在的子卷。")?;
        return Ok(Node {
            subvolume: root.id,
            inode: root.root_directory,
        });
    }
    Ok(Node {
        subvolume: from.subvolume,
        inode: entry.target.object_id,
    })
}

fn display(entry: &DirectoryEntry) -> String {
    String::from_utf8_lossy(&entry.name).into_owned()
}

fn no_live_link(
    volume: &mut Volume<impl Read + Seek>,
    history: History,
    node: Node,
    inode: &Inode,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let live = volume.find_inode(history.origin(), node.inode, || stop(cancel))?;
    Ok(live.is_none_or(|live| live.generation() != inode.generation() || live.links() == 0))
}

struct HistoricalFolder {
    parent: Option<usize>,
    directory: Option<Directory>,
}

fn publish_historical_path(
    folders: &mut [HistoricalFolder],
    mut folder: usize,
    output: &mut dyn FnMut(Directory) -> Result<(), String>,
) -> Result<(), String> {
    let mut chain = Vec::new();
    while let Some(dir) = folders[folder].directory.take() {
        chain.push(dir);
        let Some(parent) = folders[folder].parent else {
            break;
        };
        folder = parent;
    }
    for directory in chain.into_iter().rev() {
        output(directory)?;
    }
    Ok(())
}

struct Scan<'a> {
    cancel: &'a AtomicBool,
    tick: &'a mut dyn FnMut(u64, u64) -> Result<(), String>,
    found: &'a mut dyn FnMut(Candidate),
    directory: &'a mut dyn FnMut(Directory) -> Result<(), String>,
    records: u64,
    files: u32,
    deleted: u32,
    deleted_links: HashSet<(u64, u64, u64, u64, Vec<u8>)>,
    unsupported: u32,
    skipped: u64,
    limited: bool,
}

impl Scan<'_> {
    fn walk(
        &mut self,
        volume: &mut Volume<impl Read + Seek>,
        root: Node,
        history: Option<History>,
    ) -> Result<(), String> {
        let locale = crate::native_locale::current();
        let id = |node: Node| match history {
            None => node.id(),
            Some(History::Snapshot(_)) => {
                format!("btrfs-snapshot-{}-{}", node.subvolume, node.inode)
            }
            Some(History::Retained(retained)) => {
                format!(
                    "btrfs-retained-{}-{}-{}",
                    retained.transaction, node.subvolume, node.inode
                )
            }
        };
        let tree = |node: Node| history.map_or(node.subvolume.into(), |point| point.tree(node));
        let retained = matches!(history, Some(History::Retained(_)));
        let mut folders = Vec::new();
        let (first_id, first_folder) = if let Some(point) = history {
            let name = match point {
                History::Snapshot(snapshot) => locale.format(
                    "Btrfs 快照 {id}",
                    &[("id", &snapshot.subvolume.to_string())],
                ),
                History::Retained(root) => locale.format(
                    "Btrfs 事务 {transaction} · 子卷 {subvolume}",
                    &[
                        ("transaction", &root.transaction.to_string()),
                        ("subvolume", &root.subvolume.to_string()),
                    ],
                ),
            };
            let folder = Directory {
                id: id(root),
                parent: Some(ROOT.to_owned()),
                name,
                ..Default::default()
            };
            let first_id = folder.id.clone();
            folders.push(HistoricalFolder {
                parent: None,
                directory: Some(folder),
            });
            (first_id, Some(0))
        } else {
            (ROOT.to_owned(), None)
        };
        let mut pending = vec![(
            root,
            first_id,
            first_folder,
            1usize + usize::from(history.is_some()),
        )];
        let mut seen = HashSet::from([root]);
        let limit = MAX_RESULTS as u64;
        while let Some((parent, directory_id, folder, depth)) = pending.pop() {
            stop(self.cancel)?;
            if self.records >= limit {
                self.limited = true;
                break;
            }
            let entries =
                match volume.read_directory(tree(parent), parent.inode, || stop(self.cancel)) {
                    Ok(entries) => entries,
                    Err(_) if retained => {
                        stop(self.cancel)?;
                        self.skipped += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            for entry in entries {
                if self.records >= limit {
                    self.limited = true;
                    return Ok(());
                }
                self.records += 1;
                (self.tick)(self.records, limit)?;
                if !matches!(entry.file_kind, 1 | 2)
                    || (entry.target.kind == 1 && entry.target.object_id == 2)
                    || (history.is_some() && entry.target.kind == 132)
                {
                    self.skipped += 1;
                    continue;
                }
                let node = target(volume, parent, &entry)?;
                let name = display(&entry);
                let (inode, readable) =
                    match volume.inode(tree(node), node.inode, || stop(self.cancel)) {
                        Ok(inode) => inode,
                        Err(_) if retained => {
                            stop(self.cancel)?;
                            self.skipped += 1;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                let expected = if entry.file_kind == 2 { 0x4000 } else { 0x8000 };
                if inode.mode() & 0xf000 != expected {
                    if retained {
                        self.skipped += 1;
                        continue;
                    }
                    return Err("Btrfs 目录条目类型与 inode 不一致。".into());
                }
                if let Some(point) = history {
                    if inode.generation() > point.transaction()
                        || entry.transaction > point.transaction()
                    {
                        if retained {
                            self.skipped += 1;
                            continue;
                        }
                        return Err("Btrfs 快照包含创建之后的记录。".into());
                    }
                    if expected == 0x8000
                        && (inode.links() == 0
                            || !no_live_link(volume, point, node, &inode, self.cancel)?
                            || !self.deleted_links.insert((
                                point.origin(),
                                node.inode,
                                inode.generation(),
                                parent.inode,
                                entry.name.clone(),
                            )))
                    {
                        continue;
                    }
                }
                let mut metadata = file_metadata::btrfs(inode.record());
                let location = Location {
                    view: volume.identity(),
                    parent,
                    entry,
                    node,
                    inode,
                    history,
                };
                if expected == 0x4000 {
                    if depth >= MAX_DEPTH || !seen.insert(node) {
                        self.skipped += 1;
                        continue;
                    }
                    let id = id(node);
                    metadata.directory_proof =
                        Some(crate::directory_metadata::Proof::Btrfs(location));
                    let child = Directory {
                        id: id.clone(),
                        parent: Some(directory_id.clone()),
                        name,
                        metadata,
                        deleted: false,
                    };
                    let child_folder = if let Some(parent) = folder {
                        let index = folders.len();
                        folders.push(HistoricalFolder {
                            parent: Some(parent),
                            directory: Some(child),
                        });
                        Some(index)
                    } else {
                        (self.directory)(child)?;
                        None
                    };
                    pending.push((node, id, child_folder, depth + 1));
                } else {
                    if let Some(folder) = folder {
                        publish_historical_path(&mut folders, folder, self.directory)?;
                    }
                    let deleted = history.is_some();
                    self.unsupported += u32::from(!readable);
                    self.deleted += u32::from(deleted);
                    (self.found)(Candidate {
                        id: self.files,
                        name,
                        size: location.inode.size(),
                        deleted,
                        method: match history {
                            Some(History::Snapshot(_)) => "Btrfs 快照候选",
                            Some(History::Retained(_)) => "Btrfs 保留事务候选",
                            None => "Btrfs",
                        }
                        .into(),
                        note: if !readable {
                            "Btrfs 不支持的编码或无数据校验的文件暂不能提取。".into()
                        } else if retained {
                            "已从保留事务核对目录记录。提取时还会核对旧范围与数据校验。".into()
                        } else if deleted {
                            "文件仍保存在只读快照中，源子卷中已无同一 inode 的链接。".into()
                        } else {
                            String::new()
                        },
                        supported: readable,
                        directory: Some(directory_id.clone()),
                        directory_note: String::new(),
                        metadata,
                        locator: Locator::Btrfs(location),
                    });
                    self.files += 1;
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn scan(
    reader: impl Read + Seek,
    mode: ScanMode,
    cancel: &AtomicBool,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
    directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    scan_volume(
        btrfs::open(reader, || stop(cancel))?,
        mode,
        cancel,
        tick,
        found,
        directory,
    )
}

pub(crate) fn scan_volume(
    mut volume: Volume<impl Read + Seek>,
    mode: ScanMode,
    cancel: &AtomicBool,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
    mut directory: impl FnMut(Directory) -> Result<(), String>,
) -> Result<String, String> {
    let root = |volume: &Volume<_>, id: u64| -> Result<Node, String> {
        let root = volume
            .inventory()
            .subvolumes
            .iter()
            .find(|root| root.id == id)
            .ok_or("缺少 Btrfs 文件树。")?;
        Ok(Node {
            subvolume: root.id,
            inode: root.root_directory,
        })
    };
    let mut scan = Scan {
        cancel,
        tick: &mut tick,
        found: &mut found,
        directory: &mut directory,
        records: 0,
        files: 0,
        deleted: 0,
        deleted_links: HashSet::new(),
        unsupported: 0,
        skipped: 0,
        limited: false,
    };
    if mode.includes(false) {
        let live = root(&volume, 5)?;
        scan.walk(&mut volume, live, None)?;
    }
    let mut checked_history = false;
    if mode.includes(true) {
        for snapshot in volume.snapshots().to_vec() {
            if scan.limited {
                break;
            }
            let historical = root(&volume, snapshot.subvolume)?;
            checked_history = true;
            scan.walk(&mut volume, historical, Some(History::Snapshot(snapshot)))?;
        }
        if !scan.limited {
            for retained in volume.retained_roots(|| stop(cancel))? {
                if scan.limited {
                    break;
                }
                let historical = Node {
                    subvolume: retained.subvolume,
                    inode: retained.root_directory,
                };
                checked_history = true;
                scan.walk(&mut volume, historical, Some(History::Retained(retained)))?;
            }
        }
    }
    volume.verify_view(|| stop(cancel))?;
    (scan.tick)(scan.records, scan.records)?;
    let locale = crate::native_locale::current();
    let summary = locale.format(
        "已读取 {files} 个 Btrfs 文件，其中 {unsupported} 个暂不能提取。跳过 {skipped} 个条目，包括特殊文件、过深目录和无法核对的旧记录。",
        &[("files", &scan.files.to_string()), ("unsupported", &scan.unsupported.to_string()), ("skipped", &scan.skipped.to_string())],
    );
    let deleted = if mode.includes(true) {
        if !checked_history && !scan.limited {
            locale
                .text("未找到可核对的 Btrfs 只读快照或保留事务。")
                .to_owned()
        } else {
            locale.format(
                "从 Btrfs 只读快照或保留事务找到 {deleted} 个删除候选。",
                &[("deleted", &scan.deleted.to_string())],
            )
        }
    } else {
        String::new()
    };
    let missing = volume.missing_members();
    let degraded = if missing > 0 {
        locale.format(
            "已在缺少 {count} 个 Btrfs 成员的情况下读取目录。部分文件可能无法提取。",
            &[("count", &missing.to_string())],
        )
    } else {
        String::new()
    };
    Ok([
        summary.as_str(),
        deleted.as_str(),
        degraded.as_str(),
        if scan.limited {
            locale.text("已达到结果或目录项上限。")
        } else {
            ""
        },
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" "))
}

fn verify(
    volume: &mut Volume<impl Read + Seek>,
    location: &Location,
    name: &str,
    cancel: &AtomicBool,
) -> Result<(Inode, bool), String> {
    if volume.identity() != location.view {
        return Err("Btrfs 已提交视图发生变化，请重新读取目录。".into());
    }
    if let Some(point) = location.history {
        let valid = match point {
            History::Snapshot(snapshot) => volume.snapshots().contains(&snapshot),
            History::Retained(root) => volume.retained_roots(|| stop(cancel))?.contains(&root),
        };
        if !valid {
            return Err("Btrfs 历史视图或源子卷已改变。".into());
        }
    }
    let actual = volume.read_directory_entry(
        location.tree(location.parent),
        location.parent.inode,
        location.entry.index,
        || stop(cancel),
    )?;
    let node = target(volume, location.parent, &actual)?;
    if actual != location.entry || node != location.node || display(&actual) != name {
        return Err("Btrfs 目录记录或名称已改变。".into());
    }
    let (inode, readable) = volume.inode(location.tree(node), node.inode, || stop(cancel))?;
    if inode != location.inode {
        return Err("Btrfs inode 记录已改变。".into());
    }
    if let Some(point) = location.history {
        if inode.generation() > point.transaction()
            || location.entry.transaction > point.transaction()
            || (inode.mode() & 0xf000 == 0x8000
                && !no_live_link(volume, point, node, &inode, cancel)?)
        {
            return Err("Btrfs 历史视图或源子卷中的文件状态已改变。".into());
        }
    }
    Ok((inode, readable))
}

pub(crate) fn directory_metadata(
    reader: impl Read + Seek,
    location: &Location,
    name: &str,
    cancel: &AtomicBool,
) -> Result<Metadata, String> {
    directory_metadata_in(
        btrfs::open(reader, || stop(cancel))?,
        location,
        name,
        cancel,
    )
}

pub(crate) fn directory_metadata_in(
    mut volume: Volume<impl Read + Seek>,
    location: &Location,
    name: &str,
    cancel: &AtomicBool,
) -> Result<Metadata, String> {
    let (inode, _) = verify(&mut volume, location, name, cancel)?;
    volume.verify_view(|| stop(cancel))?;
    Ok(file_metadata::btrfs(inode.record()))
}

pub(crate) fn extract(
    reader: impl Read + Seek,
    candidate: &Candidate,
    location: &Location,
    output: &mut impl Write,
    limit: u64,
    cancel: &AtomicBool,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Extraction, String> {
    extract_from(
        btrfs::open(reader, || stop(cancel))?,
        candidate,
        location,
        output,
        limit,
        cancel,
        tick,
    )
}

pub(crate) fn extract_from(
    mut volume: Volume<impl Read + Seek>,
    candidate: &Candidate,
    location: &Location,
    output: &mut impl Write,
    limit: u64,
    cancel: &AtomicBool,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Extraction, String> {
    let (inode, readable) = verify(&mut volume, location, &candidate.name, cancel)?;
    if !readable
        || candidate.deleted != location.history.is_some()
        || candidate.size != inode.size()
    {
        return Err("Btrfs 提取状态或容量已改变。".into());
    }
    let mut writer = Progress {
        output,
        written: 0,
        total: limit.min(candidate.size),
        tick: &mut tick,
    };
    volume.read_file(
        location.tree(location.node),
        location.node.inode,
        limit,
        &mut writer,
        || stop(cancel),
    )?;
    volume.verify_view(|| stop(cancel))?;
    verify(&mut volume, location, &candidate.name, cancel)?;
    if writer.written != writer.total {
        return Err("Btrfs 提取长度与确认范围不一致。".into());
    }
    Ok(Extraction {
        bytes: writer.written,
        metadata: file_metadata::btrfs(inode.record()),
    })
}

struct Progress<'a, W, F> {
    output: &'a mut W,
    written: u64,
    total: u64,
    tick: &'a mut F,
}

impl<W: Write, F: FnMut(u64) -> Result<(), String>> Write for Progress<'_, W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.total - self.written {
            return Err(io::Error::other("Btrfs 输出超出确认范围。"));
        }
        (self.tick)(self.written).map_err(io::Error::other)?;
        let count = self.output.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
#[path = "../../crates/disk-core/tests/fixtures/btrfs.rs"]
pub(crate) mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{recovery, recovery_tree::Catalog};
    use std::{io::Cursor, sync::Arc};

    fn browse(bytes: Vec<u8>, mode: ScanMode) -> Result<(Vec<Candidate>, Catalog), String> {
        let mut files = Vec::new();
        let mut catalog = Catalog::default();
        recovery::scan_filesystem_owned(
            Cursor::new(bytes),
            mode,
            Arc::new(AtomicBool::new(false)),
            |_, _| Ok(()),
            |candidate| files.push(candidate),
            |dir| catalog.insert(dir),
        )?;
        for candidate in &files {
            catalog.add_file(candidate);
        }
        Ok((files, catalog))
    }

    #[test]
    fn btrfs_browser_uses_the_shared_preview_and_directory_metadata_paths() {
        let bytes = fixture::image();
        let (files, catalog) = browse(bytes.clone(), ScanMode::Existing).unwrap();
        let [file] = files.as_slice() else {
            panic!("expected one ordinary file");
        };
        assert!(file.supported && !file.deleted);
        assert_eq!(file.name, "test-\u{fffd}");
        assert_eq!(file.directory.as_deref(), Some(ROOT));
        let directory = catalog
            .snapshot()
            .into_iter()
            .find(|d| d.name == "snapshot")
            .unwrap();
        assert_eq!(directory.parent.as_deref(), Some(ROOT));
        let cancel = Arc::new(AtomicBool::new(false));
        let metadata =
            crate::directory_metadata::read(Cursor::new(bytes.clone()), &directory, cancel.clone())
                .unwrap();
        assert_eq!(metadata.permissions.as_deref(), Some("0700"));
        assert_eq!(metadata.modified.unwrap().text, "2023-11-14 22:13:20");
        let mut preview = Vec::new();
        let receipt = recovery::extract_owned_with_metadata(
            Cursor::new(bytes),
            file,
            &mut preview,
            3,
            cancel,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(receipt.bytes, 3);
        assert_eq!(receipt.metadata.permissions.as_deref(), Some("0644"));
        assert_eq!(preview, [23; 3]);
        assert!(browse(fixture::image(), ScanMode::Deleted)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn btrfs_preview_and_extraction_refuse_stale_metadata_and_bad_data_before_output() {
        let bytes = fixture::image();
        let (files, _) = browse(bytes.clone(), ScanMode::AllFiles).unwrap();
        let mut renamed = bytes.clone();
        let at = (3 * fixture::MIB + 4096) as usize;
        let leaf = &mut renamed[at..at + 4096];
        let name = leaf
            .windows(6)
            .position(|bytes| bytes == b"test-\xff")
            .unwrap();
        leaf[name] = b'n';
        fixture::sign(leaf);

        let mut changed = bytes.clone();
        let superblock = &mut changed[65536..65536 + 4096];
        superblock[0x12b] = b'A';
        fixture::sign(superblock);

        let mut damaged = bytes;
        damaged[(4 * fixture::MIB + 4095) as usize] ^= 1;
        for (source, expected) in [
            (renamed, "目录记录"),
            (changed, "已提交视图"),
            (damaged, "CRC32C"),
        ] {
            let mut output = Vec::new();
            let result = recovery::extract_owned(
                Cursor::new(source),
                &files[0],
                &mut output,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            );
            assert!(result.unwrap_err().contains(expected));
            assert!(output.is_empty());
        }
    }

    #[test]
    fn btrfs_recovers_a_snapshot_file_and_ignores_the_renamed_live_inode() {
        let bytes = fixture::snapshot_image(false);
        let (files, tree) = browse(bytes.clone(), ScanMode::Deleted).unwrap();
        let [deleted] = files.as_slice() else {
            panic!("expected just the unlinked file")
        };
        assert_eq!(deleted.name, "deleted.txt");
        assert!(deleted.deleted && deleted.supported);
        let path = tree.path(deleted.directory.as_deref().unwrap()).unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[2].name, "documents");
        assert!(path[1].name.contains("300"));
        let mut output = Vec::new();
        let extract = |image, output: &mut Vec<u8>| {
            recovery::extract_owned(
                Cursor::new(image),
                deleted,
                output,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
        };
        assert_eq!(extract(bytes, &mut output).unwrap(), 4103);
        assert_eq!(output[..4096], [23; 4096]);
        assert_eq!(output[4096..], [0; 7]);
        output.clear();
        let restored = fixture::snapshot_image(true);
        assert!(extract(restored.clone(), &mut output)
            .unwrap_err()
            .contains("状态已改变"));
        assert!(output.is_empty());
        let (new, tree) = browse(restored, ScanMode::Deleted).unwrap();
        assert!(new.is_empty());
        assert_eq!(tree.snapshot().len(), 1);
    }

    #[test]
    fn btrfs_recovers_from_a_retained_transaction_without_a_snapshot() {
        let bytes = fixture::retained_image(false);
        let (files, tree) = browse(bytes.clone(), ScanMode::Deleted).unwrap();
        let [deleted] = files.as_slice() else {
            panic!("expected the one deleted inode");
        };
        assert_eq!(deleted.name, "deleted.txt");
        assert_eq!(deleted.method, "Btrfs 保留事务候选");
        assert!(deleted.deleted && deleted.supported);
        let path = tree.path(deleted.directory.as_deref().unwrap()).unwrap();
        assert_eq!(path[2].name, "documents");
        assert!(path[1].name.contains("8"));
        let extract = |bytes, output: &mut Vec<u8>| {
            recovery::extract_owned(
                Cursor::new(bytes),
                deleted,
                output,
                u64::MAX,
                Arc::new(AtomicBool::new(false)),
                |_| Ok(()),
            )
        };
        let mut output = Vec::new();
        assert_eq!(extract(bytes.clone(), &mut output).unwrap(), 4103);
        assert_eq!(output[..4096], [23; 4096]);
        assert_eq!(output[4096..], [0; 7]);
        output.clear();
        assert!(extract(fixture::retained_image(true), &mut output)
            .unwrap_err()
            .contains("状态已改变"));
        assert!(output.is_empty());

        let mut reused = bytes;
        reused[(4 * fixture::MIB) as usize + 4095] ^= 1;
        assert!(extract(reused, &mut output).unwrap_err().contains("CRC32C"));
        assert!(output.is_empty());
    }
}

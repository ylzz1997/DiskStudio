//! A bounded, single-session ISO 9660/Joliet image with El Torito boot entries.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom, Write},
};

const BLOCK: usize = 2048;
const BOOT_CATALOG: usize = 20;
pub(crate) const MAX_FILES: usize = 4096;
const MAX_DIRS: usize = 512;
const MAX_SIZE: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub path: String,
    pub size: u32,
}

pub(crate) struct Image {
    pub size: u64,
    prefix: Vec<u8>,
    files: Vec<(u64, u32)>,
}

#[derive(Default)]
struct Directory {
    name: String,
    parent: usize,
    dirs: Vec<usize>,
    files: Vec<usize>,
}

#[derive(Clone, Copy, Default)]
struct Extent {
    block: u32,
    bytes: u32,
}

struct Namespace {
    dirs: Vec<Extent>,
    directory_names: Vec<Vec<u8>>,
    file_names: Vec<Vec<u8>>,
    little_path: Extent,
    big_path: Extent,
}

pub(crate) fn component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with([' ', '.'])
        && name.encode_utf16().count() <= 62
        && !name
            .chars()
            .any(|c| c.is_control() || r#"\/:;*?"<>|"#.contains(c) || c.len_utf16() != 1)
}

fn hierarchy(members: &[Member]) -> Result<Vec<Directory>, String> {
    let mut paths = BTreeSet::new();
    let mut directories = BTreeMap::new();
    for member in members {
        let parts: Vec<_> = member.path.split('/').collect();
        if parts.len() > 8
            || member.path.encode_utf16().count() > 240
            || !parts.iter().all(|part| component(part))
            || !paths.insert(member.path.to_lowercase())
        {
            return Err("ISO 文件路径无效、过长或不唯一。".into());
        }
        for index in 1..parts.len() {
            let path = parts[..index].join("/");
            let key = (index, path.to_lowercase());
            if directories
                .insert(key, path.clone())
                .is_some_and(|old| old != path)
            {
                return Err("ISO 目录名称的大小写不一致。".into());
            }
        }
    }
    if directories.len() + 1 > MAX_DIRS || directories.keys().any(|(_, path)| paths.contains(path))
    {
        return Err("ISO 目录数量或文件与目录重名。".into());
    }
    let parent_of = |path: &str| {
        path.rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_owned()
    };
    let leaf = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
    let mut ids = BTreeMap::from([(String::new(), 0)]);
    let mut tree = vec![Directory::default()];
    for (_, path) in directories {
        let parent = ids[&parent_of(&path)];
        let index = tree.len();
        tree[parent].dirs.push(index);
        tree.push(Directory {
            name: leaf(&path),
            parent,
            ..Directory::default()
        });
        ids.insert(path, index);
    }
    for (i, file) in members.iter().enumerate() {
        tree[ids[&parent_of(&file.path)]].files.push(i);
    }
    Ok(tree)
}

fn primary(name: &str, directory: bool) -> Option<String> {
    let name = name.to_ascii_uppercase();
    let (stem, extension) = if directory {
        (name.as_str(), "")
    } else {
        name.rsplit_once('.').unwrap_or((&name, ""))
    };
    let valid = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    };
    if stem.is_empty() || name.len() > 30 || !valid(stem) || !valid(extension) {
        return None;
    }
    Some(if directory || extension.is_empty() {
        name
    } else {
        format!("{stem}.{extension}")
    })
}

fn joliet(name: &str, directory: bool) -> Vec<u8> {
    name.encode_utf16()
        .chain(if directory { "" } else { ";1" }.encode_utf16())
        .flat_map(u16::to_be_bytes)
        .collect()
}

fn assign_names(
    tree: &[Directory],
    members: &[Member],
    unicode: bool,
) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut dir_names = vec![Vec::new(); tree.len()];
    dir_names[0] = vec![0];
    let mut file_names = vec![Vec::new(); members.len()];
    for dir in tree {
        let fields = dir
            .dirs
            .iter()
            .map(|&i| (i, tree[i].name.as_str(), true))
            .chain(
                dir.files
                    .iter()
                    .map(|&i| (i, members[i].path.rsplit('/').next().unwrap(), false)),
            );
        let mut reserved: BTreeSet<_> = fields
            .clone()
            .filter_map(|(_, name, directory)| primary(name, directory))
            .collect();
        let mut used = BTreeSet::new();
        let mut alias = 0u32;
        for (i, name, directory) in fields {
            let bytes = if unicode {
                joliet(name, directory)
            } else if let Some(name) = primary(name, directory).filter(|n| used.insert(n.clone())) {
                name.into_bytes()
            } else {
                loop {
                    alias += 1;
                    let name = format!("DS_{alias:06X}");
                    if reserved.insert(name.clone()) {
                        break name.into_bytes();
                    }
                }
            };
            if directory {
                dir_names[i] = bytes;
            } else {
                file_names[i] = bytes;
            }
        }
    }
    (dir_names, file_names)
}

fn push_extent(next: &mut u32, bytes: u32) -> Extent {
    let block = *next;
    *next += bytes.div_ceil(BLOCK as u32);
    Extent { block, bytes }
}

fn both16(to: &mut [u8], n: u16) {
    to[..2].copy_from_slice(&n.to_le_bytes());
    to[2..4].copy_from_slice(&n.to_be_bytes());
}
fn both32(to: &mut [u8], n: u32) {
    to[..4].copy_from_slice(&n.to_le_bytes());
    to[4..8].copy_from_slice(&n.to_be_bytes());
}

fn record(id: &[u8], extent: Extent, dir: bool) -> Vec<u8> {
    let mut bytes = vec![0; 33 + id.len() + usize::from(id.len() % 2 == 0)];
    bytes[0] = bytes.len() as u8;
    both32(&mut bytes[2..10], extent.block);
    both32(&mut bytes[10..18], extent.bytes);
    bytes[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]);
    bytes[25] = if dir { 2 } else { 0 };
    both16(&mut bytes[28..32], 1);
    bytes[32] = id.len() as u8;
    bytes[33..33 + id.len()].copy_from_slice(id);
    bytes
}

fn directory_bytes(
    i: usize,
    tree: &[Directory],
    space: &Namespace,
    mut file: impl FnMut(usize) -> Extent,
) -> Vec<u8> {
    let dir = &tree[i];
    let mut data = record(&[0], space.dirs[i], true);
    data.extend(record(&[1], space.dirs[dir.parent], true));
    let mut children: Vec<_> = dir
        .dirs
        .iter()
        .map(|&index| (&space.directory_names[index], space.dirs[index], true))
        .chain(
            dir.files
                .iter()
                .map(|&index| (&space.file_names[index], file(index), false)),
        )
        .collect();
    children.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (name, extent, is_directory) in children {
        let next = record(name, extent, is_directory);
        if data.len() % BLOCK + next.len() > BLOCK {
            data.resize(data.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        data.extend(next);
    }
    data.resize(data.len().div_ceil(BLOCK) * BLOCK, 0);
    data
}

impl Namespace {
    fn layout(tree: &[Directory], members: &[Member], unicode: bool, next: &mut u32) -> Self {
        let (directory_names, file_names) = assign_names(tree, members, unicode);
        let size = directory_names
            .iter()
            .map(|id| 8 + id.len() + id.len() % 2)
            .sum::<usize>() as u32;
        let mut space = Self {
            dirs: vec![Extent::default(); tree.len()],
            directory_names,
            file_names,
            little_path: push_extent(next, size),
            big_path: push_extent(next, size),
        };
        for i in 0..tree.len() {
            let len = directory_bytes(i, tree, &space, |_| Extent::default()).len();
            space.dirs[i] = push_extent(next, len as u32);
        }
        space
    }
    fn table(&self, tree: &[Directory], big: bool) -> Vec<u8> {
        let mut result = Vec::new();
        let mut order = vec![0];
        let mut numbers = vec![0u16; tree.len()];
        numbers[0] = 1;
        for at in 0..tree.len() {
            let i = order[at];
            let mut children = tree[i].dirs.clone();
            children.sort_by(|&a, &b| self.directory_names[a].cmp(&self.directory_names[b]));
            for child in children {
                numbers[child] = (order.len() + 1) as u16;
                order.push(child);
            }
            let id = &self.directory_names[i];
            let block = self.dirs[i].block;
            let parent = numbers[tree[i].parent];
            result.extend([id.len() as u8, 0]);
            result.extend(if big {
                block.to_be_bytes()
            } else {
                block.to_le_bytes()
            });
            result.extend(if big {
                parent.to_be_bytes()
            } else {
                parent.to_le_bytes()
            });
            result.extend(id);
            if id.len() % 2 != 0 {
                result.push(0);
            }
        }
        result
    }
}

fn place(data: &mut [u8], at: Extent, bytes: &[u8]) {
    let offset = at.block as usize * BLOCK;
    data[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn descriptor(tag: u8) -> [u8; BLOCK] {
    let mut data = [0; BLOCK];
    data[..7].copy_from_slice(&[tag, b'C', b'D', b'0', b'0', b'1', 1]);
    data
}

fn volume(space: &Namespace, blocks: u32, unicode: bool) -> [u8; BLOCK] {
    let mut data = descriptor(if unicode { 2 } else { 1 });
    if unicode {
        data[88..91].copy_from_slice(b"%/E");
        for pair in data[8..72].chunks_exact_mut(2) {
            pair[1] = b' ';
        }
        for (pair, b) in data[40..72].chunks_exact_mut(2).zip(b"WINPE") {
            pair[1] = *b;
        }
        for pair in data[190..812].chunks_exact_mut(2) {
            pair[1] = b' ';
        }
    } else {
        data[8..72].fill(b' ');
        data[40..45].copy_from_slice(b"WINPE");
        data[190..813].fill(b' ');
    }
    both32(&mut data[80..88], blocks);
    both16(&mut data[120..124], 1);
    both16(&mut data[124..128], 1);
    both16(&mut data[128..132], BLOCK as u16);
    both32(&mut data[132..140], space.little_path.bytes);
    data[140..144].copy_from_slice(&space.little_path.block.to_le_bytes());
    data[148..152].copy_from_slice(&space.big_path.block.to_be_bytes());
    data[156..190].copy_from_slice(&record(&[0], space.dirs[0], true));
    for i in [813, 830, 847, 864] {
        data[i..i + 16].fill(b'0');
    }
    data[881] = 1;
    data
}

fn catalog(efi: Extent, bios: Option<Extent>) -> [u8; BLOCK] {
    let mut data = [0u8; BLOCK];
    data[0] = 1;
    data[1] = if bios.is_some() { 0 } else { 0xef };
    data[4..14].copy_from_slice(b"DiskStudio");
    data[30..32].copy_from_slice(&[0x55, 0xaa]);
    let sum = data[..32]
        .chunks_exact(2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .fold(0u16, u16::wrapping_add);
    data[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
    let set_entry = |record: &mut [u8], extent: Extent| {
        record[0] = 0x88;
        record[6..8].copy_from_slice(&((extent.bytes / 512) as u16).to_le_bytes());
        record[8..12].copy_from_slice(&extent.block.to_le_bytes());
    };
    if let Some(bios) = bios {
        set_entry(&mut data[32..64], bios);
        data[64..68].copy_from_slice(&[0x91, 0xef, 1, 0]);
        set_entry(&mut data[96..128], efi);
    } else {
        set_entry(&mut data[32..64], efi);
    }
    data
}

impl Image {
    pub(crate) fn prepare(
        members: &[Member],
        efi: &[u8],
        bios: Option<&[u8]>,
    ) -> Result<Self, String> {
        let valid_boot = |data: &[u8]| {
            (1024..=u16::MAX as usize * 512).contains(&data.len()) && data.len() % 512 == 0
        };
        let file_bytes: u64 = members.iter().map(|file| u64::from(file.size)).sum();
        if members.is_empty()
            || members.len() > MAX_FILES
            || file_bytes > MAX_SIZE
            || !valid_boot(efi)
            || bios.is_some_and(|data| !valid_boot(data))
        {
            return Err("ISO 文件数量、容量或启动映像超出支持范围。".into());
        }
        let tree = hierarchy(members)?;
        let mut next = (BOOT_CATALOG + 1) as u32;
        let iso = Namespace::layout(&tree, members, false, &mut next);
        let joliet = Namespace::layout(&tree, members, true, &mut next);
        let bios_extent = bios.map(|data| push_extent(&mut next, data.len() as u32));
        let efi_extent = push_extent(&mut next, efi.len() as u32);
        let prefix_len = next as usize * BLOCK;
        let file_extents: Vec<_> = members
            .iter()
            .map(|file| push_extent(&mut next, file.size))
            .collect();
        let mut prefix = vec![0; prefix_len];
        let mut boot = descriptor(0);
        boot[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
        boot[71..75].copy_from_slice(&(BOOT_CATALOG as u32).to_le_bytes());
        for (at, data) in [
            (16, volume(&iso, next, false)),
            (17, boot),
            (18, volume(&joliet, next, true)),
            (19, descriptor(255)),
            (BOOT_CATALOG, catalog(efi_extent, bios_extent)),
        ] {
            prefix[at * BLOCK..(at + 1) * BLOCK].copy_from_slice(&data);
        }
        for space in [&iso, &joliet] {
            place(&mut prefix, space.little_path, &space.table(&tree, false));
            place(&mut prefix, space.big_path, &space.table(&tree, true));
            for i in 0..tree.len() {
                let directory = directory_bytes(i, &tree, space, |index| file_extents[index]);
                place(&mut prefix, space.dirs[i], &directory);
            }
        }
        if let Some((extent, bios)) = bios_extent.zip(bios) {
            place(&mut prefix, extent, bios);
        }
        place(&mut prefix, efi_extent, efi);
        Ok(Self {
            size: u64::from(next) * BLOCK as u64,
            prefix,
            files: file_extents
                .into_iter()
                .map(|f| (u64::from(f.block) * BLOCK as u64, f.bytes))
                .collect(),
        })
    }

    pub(crate) fn write(
        &self,
        output: &mut impl Write,
        mut append: impl FnMut(usize, u32, &mut dyn Write) -> Result<(), String>,
    ) -> Result<(), String> {
        output
            .write_all(&self.prefix)
            .map_err(|err| err.to_string())?;
        let zeros = [0; BLOCK];
        for (index, &(_, size)) in self.files.iter().enumerate() {
            append(index, size, output)?;
            let padding = (BLOCK - size as usize % BLOCK) % BLOCK;
            output
                .write_all(&zeros[..padding])
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn verify_prefix(&self, reader: &mut (impl Read + Seek)) -> Result<(), String> {
        let size = reader
            .seek(SeekFrom::End(0))
            .map_err(|err| err.to_string())?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|err| err.to_string())?;
        let mut bytes = [0; BLOCK];
        for expected in self.prefix.chunks_exact(BLOCK) {
            reader
                .read_exact(&mut bytes)
                .map_err(|err| err.to_string())?;
            if expected != bytes {
                return Err("ISO 目录或启动映像回读不一致。".into());
            }
        }
        if size != self.size {
            return Err("ISO 文件的容量与制作计划不一致。".into());
        }
        Ok(())
    }

    pub(crate) fn file_extents(&self) -> &[(u64, u32)] {
        &self.files
    }
}

#[cfg(test)]
#[path = "maintenance_iso_tests.rs"]
mod tests;

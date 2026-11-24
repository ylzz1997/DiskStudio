//! Bounded lookup of existing boot files through the checked FAT reader.
use super::{at, fat_long_name, fat_name, u16at, u32at, Fs};
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::collections::BTreeSet;
use std::io::{Read, Seek};

#[path = "fat_boot_rebuild.rs"]
pub(crate) mod rebuild;

pub(crate) struct Volume(Fs);

pub(crate) struct MappedFile {
    pub bytes: Vec<u8>,
    pub clusters: Vec<u64>,
    pub cluster_size: usize,
}

impl Volume {
    pub(crate) fn open(reader: &mut (impl Read + Seek)) -> Result<Self, String> {
        let fs = Fs::load(reader)?;
        if !matches!(fs.bits, 12 | 16 | 32) {
            return Err("Windows boot inspection requires FAT12, FAT16 or FAT32".into());
        }
        Ok(Self(fs))
    }

    pub(crate) fn bits(&self) -> u8 {
        self.0.bits
    }

    #[cfg(windows)]
    pub(crate) fn boot_sha256(&self) -> [u8; 32] {
        self.0.boot
    }

    #[cfg(windows)]
    pub(crate) fn efi_files(
        &self,
        reader: &mut (impl Read + Seek),
        limit: usize,
    ) -> Result<Vec<Vec<String>>, String> {
        let mut directories = vec![(self.0.root, Vec::<String>::new())];
        let mut visited = BTreeSet::new();
        let mut files = Vec::new();
        let mut bytes_read = 0;
        while let Some((cluster, path)) = directories.pop() {
            if !visited.insert(cluster) {
                return Err("EFI 分区目录存在交叉链接或循环。".into());
            }
            let bytes = self.0.directory(reader, cluster, false, 0)?;
            bytes_read += bytes.len();
            if bytes_read > 32 * 1024 * 1024 || visited.len() + directories.len() > 1024 {
                return Err("EFI 分区目录超出浏览上限。".into());
            }
            let mut long = Vec::new();
            for (index, entry) in bytes.chunks_exact(32).enumerate() {
                match (entry[0], entry[11]) {
                    (0, _) => break,
                    (0xe5, _) => long.clear(),
                    (_, 0x0f) => {
                        if entry[0] & 0x40 != 0 {
                            long.clear();
                        }
                        if long.len() < 21 {
                            long.push(((index * 32) as u64, entry.to_vec()));
                        }
                    }
                    _ => {
                        let name = fat_long_name(entry, &long, false)
                            .unwrap_or_else(|| fat_name(entry, &[], false));
                        long.clear();
                        if entry[11] & 8 != 0
                            || name.is_empty()
                            || name == "."
                            || name == ".."
                            || name.contains(['/', '\\', ':'])
                            || name.chars().any(char::is_control)
                        {
                            continue;
                        }
                        let mut child = path.clone();
                        child.push(name.clone());
                        if entry[11] & 16 != 0 {
                            if child.len() > 32 || directories.len() >= 1024 {
                                return Err("EFI 分区目录超出浏览上限。".into());
                            }
                            let cluster =
                                u32::from(u16at(entry, 26)) | (u32::from(u16at(entry, 20)) << 16);
                            self.0.offset(cluster)?;
                            directories.push((cluster, child));
                        } else if name.to_ascii_lowercase().ends_with(".efi")
                            && (1..=limit).contains(&(u32at(entry, 28) as usize))
                        {
                            if files.len() >= 512 {
                                return Err("EFI 文件数量超出浏览上限。".into());
                            }
                            files.push(child);
                        }
                    }
                }
            }
        }
        files.sort_unstable();
        Ok(files)
    }

    pub(crate) fn read_file(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
        limit: usize,
    ) -> Result<Option<Vec<u8>>, String> {
        let Some((cluster, size, directory)) = self.lookup(reader, path)? else {
            return Ok(None);
        };
        if directory || size > limit {
            return Err("FAT boot file has an unexpected type or size".into());
        }
        self.0
            .content(reader, cluster, size as u64, false, false)
            .map(Some)
    }

    #[cfg(windows)]
    pub(crate) fn file_sha256_and_prefix(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
        limit: usize,
        prefix_limit: usize,
    ) -> Result<Option<([u8; 32], Vec<u8>)>, String> {
        self.measure_file(reader, path, limit, prefix_limit)
            .map(|value| value.map(|(digest, prefix, _)| (digest, prefix)))
    }

    #[cfg(windows)]
    pub(crate) fn measure_file(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
        limit: usize,
        prefix_limit: usize,
    ) -> Result<Option<([u8; 32], Vec<u8>, u64)>, String> {
        let Some((first, size, directory)) = self.lookup(reader, path)? else {
            return Ok(None);
        };
        if directory || size == 0 || size > limit {
            return Err("EFI 加载器的文件类型或长度无效。".into());
        }
        let chain = self
            .0
            .chain(reader, first, Some(size as u64), false, false)?;
        let mut digest = Sha256::new();
        let mut prefix = Vec::with_capacity(prefix_limit.min(size));
        let mut remaining = size;
        for cluster in chain {
            let count = remaining.min(self.0.cluster as usize);
            let chunk = at(reader, self.0.offset(cluster)?, count)?;
            digest.update(&chunk);
            let next = chunk.len().min(prefix_limit - prefix.len());
            prefix.extend_from_slice(&chunk[..next]);
            remaining -= count;
        }
        Ok(Some((digest.finalize().into(), prefix, size as u64)))
    }

    pub(crate) fn directory_exists(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
    ) -> Result<bool, String> {
        let Some((cluster, _, directory)) = self.lookup(reader, path)? else {
            return Ok(false);
        };
        if !directory {
            return Err("FAT boot path refers to a file".into());
        }
        self.0.offset(cluster)?;
        self.0.directory(reader, cluster, false, 0).map(|_| true)
    }

    pub(crate) fn mapped_file(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
        limit: usize,
    ) -> Result<MappedFile, String> {
        let (first, size, directory) = self
            .lookup(reader, path)?
            .ok_or("Existing BCD file is missing")?;
        if directory || size == 0 || size > limit {
            return Err("Existing BCD is not a regular file of supported size".into());
        }
        let cluster_size = self.0.cluster as usize;
        let chain = self
            .0
            .chain(reader, first, Some(size as u64), false, false)?;
        let mut clusters = Vec::with_capacity(chain.len());
        let mut bytes = Vec::with_capacity(size);
        for cluster in chain {
            let offset = self.0.offset(cluster)?;
            clusters.push(offset);
            bytes.extend(at(reader, offset, (size - bytes.len()).min(cluster_size))?);
        }
        Ok(MappedFile {
            bytes,
            clusters,
            cluster_size,
        })
    }

    fn lookup(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
    ) -> Result<Option<(u32, usize, bool)>, String> {
        let mut cluster = self.0.root;
        for (index, part) in path.iter().enumerate() {
            let directory = self.0.directory(reader, cluster, false, 0)?;
            let Some(slot) = directory_entry(&directory, part)? else {
                return Ok(None);
            };
            let record = &directory[slot.offset..slot.offset + 32];
            cluster = u32::from(u16at(record, 26));
            if self.0.bits == 32 {
                cluster |= u32::from(u16at(record, 20)) << 16;
            }
            let is_directory = record[11] & 16 != 0;
            if index + 1 == path.len() {
                return Ok(Some((cluster, u32at(record, 28) as usize, is_directory)));
            }
            if !is_directory {
                return Err("FAT boot path component is not a directory".into());
            }
            self.0.offset(cluster)?;
        }
        Err("FAT boot file path is empty".into())
    }
}

struct Slot {
    offset: usize,
    first: usize,
    primary: bool,
}

/// Track the full validated entry and distinguish its primary name from an alias.
fn directory_entry(bytes: &[u8], wanted: &str) -> Result<Option<Slot>, String> {
    let mut long = Vec::new();
    let mut matching = None;
    for (slot, record) in bytes.chunks_exact(32).enumerate() {
        if record[0] == 0 {
            break;
        }
        if record[0] == 0xe5 {
            long.clear();
            continue;
        }
        if record[11] == 0x0f {
            if record[0] & 0x40 != 0 {
                long.clear();
            }
            if long.len() < 21 {
                long.push(((slot * 32) as u64, record.to_vec()));
            }
            continue;
        }
        let short = fat_name(record, &[], false);
        let long_name = fat_long_name(record, &long, false);
        let primary = long_name
            .as_deref()
            .unwrap_or(&short)
            .eq_ignore_ascii_case(wanted);
        let offset = slot * 32;
        let first = if long_name.is_some() {
            long[0].0 as usize
        } else {
            offset
        };
        long.clear();
        if record[11] & 8 == 0
            && (primary || short.eq_ignore_ascii_case(wanted))
            && matching
                .replace(Slot {
                    offset,
                    first,
                    primary,
                })
                .is_some()
        {
            return Err("FAT boot path is ambiguous".into());
        }
    }
    Ok(matching)
}

#[cfg(all(test, windows))]
#[path = "../../tests/fixtures/fat_view.rs"]
mod view_fixture;

#[cfg(all(test, windows))]
#[test]
fn firmware_loader_scan_and_digest_cover_nested_fat_file() {
    use std::io::Cursor;
    let mut image = view_fixture::fat32(512);
    let start = u16at(&image, 14) as usize;
    let fat_size = u32at(&image, 36) as usize;
    let root = (start + 2 * fat_size) * 512;
    for at in [start, start + fat_size] {
        image[at * 512 + 16..at * 512 + 20].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
    }
    let file_entry = root + 2 * 512;
    image.copy_within(root + 32..root + 64, file_entry);
    image[file_entry..file_entry + 11].copy_from_slice(b"LINUX   EFI");
    image[root + 32..root + 64].fill(0);
    image[root + 32..root + 43].copy_from_slice(b"EFI        ");
    image[root + 43] = 16;
    image[root + 58] = 4;
    let mut reader = Cursor::new(image);
    let volume = Volume::open(&mut reader).unwrap();
    let files = volume.efi_files(&mut reader, 1024).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].join("/").eq_ignore_ascii_case("EFI/LINUX.EFI"));
    let path: Vec<_> = files[0].iter().map(String::as_str).collect();
    let (digest, prefix) = volume
        .file_sha256_and_prefix(&mut reader, &path, 1024, 5)
        .unwrap()
        .unwrap();
    assert_eq!(
        digest.as_slice(),
        Sha256::digest(view_fixture::FAT_CONTENT).as_slice()
    );
    assert_eq!(prefix, view_fixture::FAT_CONTENT[..5]);
    assert!(volume
        .file_sha256_and_prefix(&mut reader, &path, 5, 5)
        .is_err());
}

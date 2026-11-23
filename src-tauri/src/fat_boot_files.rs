//! Bounded lookup of existing boot files through the checked FAT reader.
use super::{at, fat_name, u16at, u32at, Fs};
use std::io::{Read, Seek};

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
        let (first, size, directory) = self.lookup(reader, path)?.ok_or("Existing BCD file is missing")?;
        if directory || size == 0 || size > limit {
            return Err("Existing BCD is not a regular file of supported size".into());
        }
        let cluster_size = self.0.cluster as usize;
        let chain = self.0.chain(reader, first, Some(size as u64), false, false)?;
        let mut clusters = Vec::with_capacity(chain.len());
        let mut bytes = Vec::with_capacity(size);
        for cluster in chain {
            let offset = self.0.offset(cluster)?;
            clusters.push(offset);
            bytes.extend(at(reader, offset, (size - bytes.len()).min(cluster_size))?);
        }
        Ok(MappedFile { bytes, clusters, cluster_size })
    }

    fn lookup(
        &self,
        reader: &mut (impl Read + Seek),
        path: &[&str],
    ) -> Result<Option<(u32, usize, bool)>, String> {
        let mut cluster = self.0.root;
        for (index, part) in path.iter().enumerate() {
            let directory = self.0.directory(reader, cluster, false, 0)?;
            let mut long = Vec::new();
            let mut matching = None;
            for record in directory.chunks_exact(32) {
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
                        long.push((0, record.to_vec()));
                    }
                    continue;
                }
                let name = fat_name(record, &long, false);
                long.clear();
                if record[11] & 8 == 0 && (name.eq_ignore_ascii_case(part)
                    || fat_name(record, &[], false).eq_ignore_ascii_case(part)) {
                    if matching.replace(record).is_some() {
                        return Err("FAT boot path is ambiguous".into());
                    }
                }
            }
            let Some(record) = matching else {
                return Ok(None);
            };
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

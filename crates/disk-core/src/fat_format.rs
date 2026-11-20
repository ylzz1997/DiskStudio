//! FAT12/16/32 metadata planning and bounded formatting. No path/device APIs.
//! Layout follows Microsoft FAT Specification 1.03 (cluster-count type rules).
use std::io::{self, Read, Seek, SeekFrom, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}
impl FatType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
        }
    }
    fn bits(self) -> u64 {
        match self {
            Self::Fat12 => 12,
            Self::Fat16 => 16,
            Self::Fat32 => 32,
        }
    }
    fn accepts(self, clusters: u64) -> bool {
        // Leave a margin below reserved/bad/EOC cluster values. FAT type is
        // derived from count, never from the informational FATxx text label.
        match self {
            Self::Fat12 => (1..=0xfed).contains(&clusters),
            Self::Fat16 => (4085..=0xffed).contains(&clusters),
            Self::Fat32 => (65525..=0x0fffffed).contains(&clusters),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    kind: FatType,
    sector: u32,
    cluster: u32,
    total_sectors: u32,
    fat_sectors: u32,
    clusters: u32,
    data_sector: u32,
    reserved: u16,
    root_entries: u16,
    label: [u8; 11],
    volume_id: u32,
    hidden: u32,
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
fn word(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn dword(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}

impl Plan {
    pub fn new(
        kind: FatType,
        size: u64,
        sector: u32,
        cluster: u32,
        label: &str,
        volume_id: u32,
        hidden: u32,
    ) -> io::Result<Self> {
        if !matches!(sector, 512 | 4096)
            || size % u64::from(sector) != 0
            || size < 1024 * 1024
            || size / u64::from(sector) > u64::from(u32::MAX)
        {
            return Err(invalid(
                "FAT 容量须至少 1 MiB、扇区对齐且不超过 32 位扇区计数范围",
            ));
        }
        if label.is_empty()
            || label != label.trim()
            || label.len() > 11
            || !label
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b" _-".contains(&b))
        {
            return Err(invalid(
                "FAT 卷标须为 1–11 个大写英文字母、数字、空格、下划线或连字符",
            ));
        }
        if cluster != 0 && (!cluster.is_power_of_two() || cluster < sector || cluster > 32768) {
            return Err(invalid(
                "FAT 簇大小须为不小于逻辑扇区的 2 次幂，最大 32 KiB",
            ));
        }
        let mut padded = [b' '; 11];
        padded[..label.len()].copy_from_slice(label.as_bytes());
        let total = (size / u64::from(sector)) as u32;
        // Prefer conventional defaults, but require the requested FAT type to
        // be exactly representable. Never silently change FAT12 into FAT16.
        let preferred = match kind {
            FatType::Fat12 => sector,
            FatType::Fat16 => 2048u32.max(sector),
            FatType::Fat32 => {
                if size > 32 << 30 {
                    32768
                } else if size > 16 << 30 {
                    16384
                } else if size > 8 << 30 {
                    8192
                } else {
                    4096
                }
            }
        }
        .max(sector);
        let mut candidates = vec![preferred];
        let mut n = sector;
        while n <= 32768 {
            if n != preferred {
                candidates.push(n);
            }
            n *= 2;
        }
        if cluster != 0 {
            candidates = vec![cluster];
        }
        for cluster in candidates {
            let reserved = if kind == FatType::Fat32 { 32u16 } else { 1 };
            let root_entries = if kind == FatType::Fat32 { 0u16 } else { 512 };
            let root_sectors = (u32::from(root_entries) * 32).div_ceil(sector);
            let Some(available) = total.checked_sub(u32::from(reserved) + root_sectors) else {
                continue;
            };
            let spc = cluster / sector;
            if available < 3 || spc > 128 {
                continue;
            }
            // Smallest FAT allocation sufficient for the remaining data.
            // Binary search avoids fixed-point oscillation at sector edges.
            let enough = |fat: u32| {
                let clusters = (available - 2 * fat) / spc;
                u64::from(fat) * u64::from(sector) * 8 >= (u64::from(clusters) + 2) * kind.bits()
            };
            let (mut lo, mut hi) = (1, (available - 1) / 2);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if enough(mid) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            let mut clusters = (available - 2 * lo) / spc;
            // At 2 GiB, a minimal FAT with 32 KiB clusters can cross the FAT16
            // classification boundary. A small amount of FAT padding keeps
            // the exact volume size and requested type without using reserved
            // cluster numbers. Do not hide large unusable areas in the FATs.
            let upper = match kind {
                FatType::Fat12 => 0xfed,
                FatType::Fat16 => 0xffed,
                FatType::Fat32 => 0x0fffffed,
            };
            if clusters > upper && clusters - upper <= 32 {
                let extra = ((clusters - upper) * spc).div_ceil(2);
                if lo + extra <= (available - 1) / 2 {
                    lo += extra;
                    clusters = (available - 2 * lo) / spc;
                }
            }
            if !enough(lo)
                || !kind.accepts(u64::from(clusters))
                || (kind != FatType::Fat32 && lo > u32::from(u16::MAX))
            {
                continue;
            }
            return Ok(Self {
                kind,
                sector,
                cluster,
                total_sectors: total,
                fat_sectors: lo,
                clusters,
                data_sector: u32::from(reserved) + 2 * lo + root_sectors,
                reserved,
                root_entries,
                label: padded,
                volume_id,
                hidden,
            });
        }
        Err(invalid(
            "容量与簇大小不能生成所选 FAT 类型；请选择其他簇大小或文件系统",
        ))
    }
    pub fn size(&self) -> u64 {
        u64::from(self.total_sectors) * u64::from(self.sector)
    }
    pub fn kind(&self) -> FatType {
        self.kind
    }
    pub fn cluster_size(&self) -> u32 {
        self.cluster
    }
    pub fn sector_size(&self) -> u32 {
        self.sector
    }
    pub fn label(&self) -> String {
        String::from_utf8_lossy(&self.label).trim_end().into()
    }
    fn root_sector(&self) -> u32 {
        u32::from(self.reserved) + 2 * self.fat_sectors
    }
    fn metadata_sectors(&self) -> u32 {
        self.data_sector
            + if self.kind == FatType::Fat32 {
                self.cluster / self.sector
            } else {
                0
            }
    }
    fn boot(&self, b: &mut [u8]) {
        b[..3].copy_from_slice(&[
            0xeb,
            if self.kind == FatType::Fat32 {
                0x58
            } else {
                0x3c
            },
            0x90,
        ]);
        b[3..11].copy_from_slice(b"MSWIN4.1");
        word(b, 11, self.sector as u16);
        b[13] = (self.cluster / self.sector) as u8;
        word(b, 14, self.reserved);
        b[16] = 2;
        word(b, 17, self.root_entries);
        if self.kind != FatType::Fat32 && self.total_sectors < 65536 {
            word(b, 19, self.total_sectors as u16);
        } else {
            dword(b, 32, self.total_sectors);
        }
        b[21] = 0xf8;
        word(b, 24, 63);
        word(b, 26, 255);
        dword(b, 28, self.hidden);
        let (ext, code) = if self.kind == FatType::Fat32 {
            dword(b, 36, self.fat_sectors);
            dword(b, 44, 2);
            word(b, 48, 1);
            word(b, 50, 6);
            (64, 90)
        } else {
            word(b, 22, self.fat_sectors as u16);
            (36, 62)
        };
        b[ext] = 0x80;
        b[ext + 2] = 0x29;
        dword(b, ext + 3, self.volume_id);
        b[ext + 7..ext + 18].copy_from_slice(&self.label);
        b[ext + 18..ext + 26].fill(b' ');
        b[ext + 18..ext + 18 + self.kind.name().len()].copy_from_slice(self.kind.name().as_bytes());
        // Nonbootable data filesystem: a short 16-bit halt loop, no DOS loader.
        b[code..code + 4].copy_from_slice(&[0xfa, 0xf4, 0xeb, 0xfd]);
        b[510] = 0x55;
        b[511] = 0xaa;
    }
    fn fill_sector(&self, index: u32, out: &mut [u8]) {
        out.fill(0);
        if index == 0 || (self.kind == FatType::Fat32 && index == 6) {
            self.boot(out);
            return;
        }
        if self.kind == FatType::Fat32 && (index == 1 || index == 7) {
            dword(out, 0, 0x41615252);
            dword(out, 484, 0x61417272);
            dword(out, 488, self.clusters - 1);
            dword(out, 492, 3);
            dword(out, 508, 0xaa550000);
            return;
        }
        if self.kind == FatType::Fat32 && (index == 2 || index == 8) {
            // FAT32 backup boot region covers three sectors.
            out[510] = 0x55;
            out[511] = 0xaa;
            return;
        }
        if index == u32::from(self.reserved) || index == u32::from(self.reserved) + self.fat_sectors
        {
            match self.kind {
                FatType::Fat12 => out[..3].copy_from_slice(&[0xf8, 0xff, 0xff]),
                FatType::Fat16 => out[..4].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff]),
                FatType::Fat32 => {
                    dword(out, 0, 0x0ffffff8);
                    dword(out, 4, 0x0fffffff);
                    dword(out, 8, 0x0fffffff);
                }
            }
        } else if index == self.root_sector() {
            out[..11].copy_from_slice(&self.label);
            out[11] = 0x08;
        }
    }
    /// Quick format within the caller's seekable volume. It must already be
    /// locked and bounded to exactly size(); this code never opens anything.
    /// Every write is sector-aligned. Primary VBR is committed last; this is
    /// not a transactional or power-loss-safe filesystem conversion.
    pub fn format<R: Read + Write + Seek>(
        &self,
        io: &mut R,
        mut progress: impl FnMut(u64, u64) -> io::Result<()>,
    ) -> io::Result<()> {
        if io.seek(SeekFrom::End(0))? != self.size() {
            return Err(invalid("格式化范围与已确认容量不同"));
        }
        let sector = self.sector as usize;
        let total = u64::from(self.metadata_sectors()) * u64::from(self.sector);
        let mut buffer = vec![0u8; 1024 * 1024];
        progress(0, total)?;
        io.seek(SeekFrom::Start(0))?;
        io.write_all(&buffer[..sector])?;
        io.flush()?;
        let mut index = 1;
        while index < self.metadata_sectors() {
            let count = (self.metadata_sectors() - index).min((buffer.len() / sector) as u32);
            for i in 0..count {
                self.fill_sector(
                    index + i,
                    &mut buffer[i as usize * sector..(i as usize + 1) * sector],
                );
            }
            io.seek(SeekFrom::Start(u64::from(index) * u64::from(self.sector)))?;
            io.write_all(&buffer[..count as usize * sector])?;
            index += count;
            progress(u64::from(index) * u64::from(self.sector), total)?;
        }
        io.flush()?;
        self.fill_sector(0, &mut buffer[..sector]);
        io.seek(SeekFrom::Start(0))?;
        io.write_all(&buffer[..sector])?;
        io.flush()?;
        self.verify(io)
    }
    /// Full format of exactly the caller's bounded volume. Old data is
    /// replaced in every logical sector, including unused tail sectors.
    /// Primary VBR is published last; failures are not rolled back or retried.
    pub fn format_full<R: Read + Write + Seek>(
        &self,
        io: &mut R,
        mut progress: impl FnMut(u64, u64) -> io::Result<()>,
    ) -> io::Result<FullFormatVerification> {
        if io.seek(SeekFrom::End(0))? != self.size() {
            return Err(invalid("完整格式化范围与已确认容量不同"));
        }
        let sector = self.sector as usize;
        let total = self
            .size()
            .checked_mul(2)
            .ok_or_else(|| invalid("容量溢出"))?;
        let sectors = self.size() / u64::from(self.sector);
        let mut buffer = vec![0u8; 1024 * 1024];
        progress(0, total)?;
        io.seek(SeekFrom::Start(0))?;
        io.write_all(&buffer[..sector])?;
        io.flush()?;
        let mut index = 1u64;
        while index < sectors {
            let count = (sectors - index).min((buffer.len() / sector) as u64) as usize;
            let bytes = count * sector;
            self.fill_range(index, &mut buffer[..bytes]);
            io.seek(SeekFrom::Start(index * u64::from(self.sector)))?;
            io.write_all(&buffer[..bytes])?;
            index += count as u64;
            progress(index * u64::from(self.sector), total)?;
        }
        io.flush()?;
        self.fill_sector(0, &mut buffer[..sector]);
        io.seek(SeekFrom::Start(0))?;
        io.write_all(&buffer[..sector])?;
        io.flush()?;
        self.verify_full_progress(io, |read| progress(self.size() + read, total))
    }
    fn fill_range(&self, first_sector: u64, buffer: &mut [u8]) {
        buffer.fill(0);
        let sector = self.sector as usize;
        let metadata = u64::from(self.metadata_sectors());
        let count = (metadata.saturating_sub(first_sector)).min((buffer.len() / sector) as u64);
        for i in 0..count {
            self.fill_sector(
                (first_sector + i) as u32,
                &mut buffer[i as usize * sector..(i as usize + 1) * sector],
            );
        }
    }

    /// Materialize just the reserved sectors, tables and initial root. A
    /// caller building an image can provide the remaining zero-filled area.
    pub fn initialized_prefix(&self, limit: usize) -> io::Result<Vec<u8>> {
        let len = usize::try_from(u64::from(self.metadata_sectors()) * u64::from(self.sector))
            .ok()
            .filter(|&len| len <= limit)
            .ok_or_else(|| invalid("FAT 格式化元数据超过内存预算"))?;
        let mut bytes = vec![0; len];
        self.fill_range(0, &mut bytes);
        Ok(bytes)
    }
    pub fn verify_full<R: Read + Seek>(&self, io: &mut R) -> io::Result<FullFormatVerification> {
        self.verify_full_progress(io, |_| Ok(()))
    }
    fn verify_full_progress<R: Read + Seek>(
        &self,
        io: &mut R,
        mut progress: impl FnMut(u64) -> io::Result<()>,
    ) -> io::Result<FullFormatVerification> {
        use sha2::{Digest, Sha256};
        if io.seek(SeekFrom::End(0))? != self.size() {
            return Err(invalid("完整格式化核对范围变化"));
        }
        let mut expected = vec![0u8; 1024 * 1024];
        let mut actual = vec![0u8; expected.len()];
        let mut expected_hash = Sha256::new();
        let mut actual_hash = Sha256::new();
        io.seek(SeekFrom::Start(0))?;
        let mut offset = 0;
        while offset < self.size() {
            let bytes = (self.size() - offset).min(expected.len() as u64) as usize;
            self.fill_range(offset / u64::from(self.sector), &mut expected[..bytes]);
            io.read_exact(&mut actual[..bytes])?;
            expected_hash.update(&expected[..bytes]);
            actual_hash.update(&actual[..bytes]);
            if actual[..bytes] != expected[..bytes] {
                return Err(invalid(
                    "完整格式化读回不一致；文件系统元数据或数据区存在差异",
                ));
            }
            offset += bytes as u64;
            progress(offset)?;
        }
        let digest = actual_hash.finalize();
        if digest != expected_hash.finalize() {
            return Err(invalid("完整格式化 SHA-256 核对失败"));
        }
        Ok(FullFormatVerification {
            bytes: self.size(),
            sha256: format!("{digest:x}"),
        })
    }
    /// Exact readback of all generated metadata, including both complete FATs.
    pub fn verify<R: Read + Seek>(&self, io: &mut R) -> io::Result<()> {
        if io.seek(SeekFrom::End(0))? != self.size() {
            return Err(invalid("FAT 核对范围变化"));
        }
        let sector = self.sector as usize;
        let mut expected = vec![0u8; 1024 * 1024];
        let mut actual = vec![0u8; expected.len()];
        io.seek(SeekFrom::Start(0))?;
        let mut index = 0;
        while index < self.metadata_sectors() {
            let count = (self.metadata_sectors() - index).min((expected.len() / sector) as u32);
            for i in 0..count {
                self.fill_sector(
                    index + i,
                    &mut expected[i as usize * sector..(i as usize + 1) * sector],
                );
            }
            let n = count as usize * sector;
            io.read_exact(&mut actual[..n])?;
            if expected[..n] != actual[..n] {
                return Err(io::Error::other("FAT 元数据写后核对失败"));
            }
            index += count;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FullFormatVerification {
    pub bytes: u64,
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    const MIB: u64 = 1024 * 1024;
    // Entirely synthetic sparse storage. Missing bytes contain old data, so
    // tests detect uninitialized FATs/directories. No actual filesystem I/O.
    struct Sparse {
        size: u64,
        pos: u64,
        pages: BTreeMap<u64, Box<[u8; 4096]>>,
        strict_sector: u64,
        fail_write: Option<u64>,
        writes: u64,
    }
    impl Sparse {
        fn new(size: u64, sector: u32) -> Self {
            Self {
                size,
                pos: 0,
                pages: BTreeMap::new(),
                strict_sector: u64::from(sector),
                fail_write: None,
                writes: 0,
            }
        }
    }
    impl Seek for Sparse {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let p = match from {
                SeekFrom::Start(n) => n as i128,
                SeekFrom::Current(n) => self.pos as i128 + n as i128,
                SeekFrom::End(n) => self.size as i128 + n as i128,
            };
            if p < 0 || p > self.size as i128 {
                return Err(invalid("Outside synthetic extent"));
            }
            self.pos = p as u64;
            Ok(self.pos)
        }
    }
    impl Read for Sparse {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min((self.size - self.pos) as usize);
            for (i, b) in out[..n].iter_mut().enumerate() {
                let p = self.pos + i as u64;
                *b = self
                    .pages
                    .get(&(p / 4096))
                    .map_or(0xa5, |v| v[(p % 4096) as usize]);
            }
            self.pos += n as u64;
            Ok(n)
        }
    }
    impl Write for Sparse {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if self.fail_write == Some(self.writes) {
                return Err(io::Error::other("Injected write failure"));
            }
            if self.pos + input.len() as u64 > self.size
                || (self.strict_sector != 0
                    && (self.pos % self.strict_sector != 0
                        || input.len() as u64 % self.strict_sector != 0))
            {
                return Err(invalid("Unaligned/out-of-range write"));
            }
            self.writes += 1;
            let mut used = 0;
            while used < input.len() {
                let page = self.pos / 4096;
                let within = (self.pos % 4096) as usize;
                let n = (4096 - within).min(input.len() - used);
                self.pages
                    .entry(page)
                    .or_insert_with(|| Box::new([0xa5; 4096]))[within..within + n]
                    .copy_from_slice(&input[used..used + n]);
                self.pos += n as u64;
                used += n;
            }
            Ok(input.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn read_u16(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
    }
    fn read_u32(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
    }

    #[test]
    fn independent_filesystem_reader_writes_and_reads_all_fat_types_and_large_fat32() {
        for sector in [512, 4096] {
            for (kind, size) in [
                (FatType::Fat12, 2 * MIB),
                (FatType::Fat16, 128 * MIB),
                (FatType::Fat16, 2048 * MIB),
                (FatType::Fat32, 1024 * MIB),
                (FatType::Fat32, 64 * 1024 * MIB),
            ] {
                let plan = Plan::new(kind, size, sector, 0, "DS_DATA", 0x12345678, 2048).unwrap();
                let mut stream = Sparse::new(size, sector);
                plan.format(&mut stream, |_, _| Ok(())).unwrap();
                // Quick format does not erase data outside metadata regions.
                stream
                    .seek(SeekFrom::Start(size - u64::from(sector)))
                    .unwrap();
                let mut untouched = vec![0; sector as usize];
                stream.read_exact(&mut untouched).unwrap();
                assert!(untouched.iter().all(|b| *b == 0xa5));
                stream.seek(SeekFrom::Start(0)).unwrap();
                stream.strict_sector = 0; // This independent filesystem uses byte I/O.
                {
                    let fs = fatfs::FileSystem::new(&mut stream, fatfs::FsOptions::new()).unwrap();
                    assert_eq!(format!("{:?}", fs.fat_type()).to_uppercase(), kind.name());
                    assert_eq!(fs.volume_label(), "DS_DATA");
                    assert_eq!(fs.cluster_size(), plan.cluster_size());
                    let root = fs.root_dir();
                    let mut file = root.create_file("跨簇内容.bin").unwrap();
                    let data = (0..plan.cluster_size() * 3 + 17)
                        .map(|n| (n % 251) as u8)
                        .collect::<Vec<_>>();
                    file.write_all(&data).unwrap();
                    file.flush().unwrap();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    let mut actual = vec![];
                    file.read_to_end(&mut actual).unwrap();
                    assert_eq!(actual, data);
                    drop(file);
                    drop(root);
                    fs.unmount().unwrap();
                }
                // Reopen independently after metadata/FSInfo updates.
                stream.seek(SeekFrom::Start(0)).unwrap();
                let fs = fatfs::FileSystem::new(&mut stream, fatfs::FsOptions::new()).unwrap();
                assert!(fs.root_dir().open_file("跨簇内容.bin").is_ok());
                fs.unmount().unwrap();
            }
        }
    }
    #[test]
    fn boot_backup_fsinfo_and_fat_copies_are_consistent() {
        for sector in [512, 4096] {
            let p = Plan::new(FatType::Fat32, 1024 * MIB, sector, 4096, "DATA", 42, 2048).unwrap();
            let mut s = Sparse::new(p.size(), sector);
            p.format(&mut s, |_, _| Ok(())).unwrap();
            let mut boot = vec![0; sector as usize];
            s.seek(SeekFrom::Start(0)).unwrap();
            s.read_exact(&mut boot).unwrap();
            assert_eq!(read_u16(&boot, 11), sector as u16);
            assert_eq!(read_u32(&boot, 28), 2048);
            assert_eq!(read_u32(&boot, 44), 2);
            assert_eq!(read_u16(&boot, 50), 6);
            let data = p.total_sectors - u32::from(read_u16(&boot, 14)) - 2 * read_u32(&boot, 36);
            let clusters = data / u32::from(boot[13]);
            assert!(clusters >= 65525);
            let mut backup = boot.clone();
            s.seek(SeekFrom::Start(6 * u64::from(sector))).unwrap();
            s.read_exact(&mut backup).unwrap();
            assert_eq!(boot, backup);
            s.seek(SeekFrom::Start(u64::from(sector))).unwrap();
            s.read_exact(&mut backup).unwrap();
            assert_eq!(read_u32(&backup, 488), clusters - 1);
            assert_eq!(read_u32(&backup, 508), 0xaa550000);
            // Corruption in the second FAT must be detected by readback.
            let offset = u64::from(u32::from(p.reserved) + p.fat_sectors + 1) * u64::from(sector);
            s.seek(SeekFrom::Start(offset)).unwrap();
            s.write_all(&vec![0x5a; sector as usize]).unwrap();
            assert!(p.verify(&mut s).is_err());
        }
    }
    #[test]
    fn geometry_boundaries_cannot_overflow_or_change_requested_type() {
        for sector in [0, 1, 512, 4096, u32::MAX] {
            for size in [
                0,
                1,
                MIB - 1,
                MIB,
                16 * MIB,
                128 * MIB,
                2048 * MIB,
                u64::MAX,
                1u64 << 41,
                1u64 << 44,
            ] {
                for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
                    for cluster in [0, 1, 512, 4096, 32768, 65536, u32::MAX] {
                        if let Ok(p) = Plan::new(kind, size, sector, cluster, "DATA", 1, 0) {
                            assert!(kind.accepts(u64::from(p.clusters)));
                            assert_eq!(p.size(), size);
                            assert!(
                                u64::from(p.data_sector) * u64::from(sector)
                                    + u64::from(p.clusters) * u64::from(p.cluster)
                                    <= size
                            );
                            assert!(
                                u64::from(p.fat_sectors) * u64::from(sector) * 8
                                    >= (u64::from(p.clusters) + 2) * kind.bits()
                            );
                        }
                    }
                }
            }
        }
        assert!(Plan::new(FatType::Fat32, 64 * 1024 * MIB, 512, 32768, "DATA", 1, 0).is_ok());
        assert!(Plan::new(FatType::Fat16, 64 * 1024 * MIB, 512, 0, "DATA", 1, 0).is_err());
        for label in [
            "",
            " DATA",
            "DATA ",
            "中文",
            "TOO_LONG_LABEL",
            "A/BC",
            "A\0B",
        ] {
            assert!(Plan::new(FatType::Fat16, 128 * MIB, 512, 0, label, 1, 0).is_err());
        }
    }
    #[test]
    fn precommit_failure_or_cancellation_leaves_primary_boot_invalid() {
        let p = Plan::new(FatType::Fat16, 128 * MIB, 512, 0, "DATA", 1, 0).unwrap();
        for cancel in [false, true] {
            let mut s = Sparse::new(p.size(), 512);
            if !cancel {
                s.fail_write = Some(1);
            }
            assert!(p
                .format(&mut s, |done, _| if cancel && done > 0 {
                    Err(io::Error::other("cancel"))
                } else {
                    Ok(())
                })
                .is_err());
            s.seek(SeekFrom::Start(0)).unwrap();
            let mut b = [0; 512];
            s.read_exact(&mut b).unwrap();
            assert_eq!(b, [0; 512]);
        }
        let mut wrong = Sparse::new(p.size() + 512, 512);
        assert!(p.format(&mut wrong, |_, _| Ok(())).is_err());
        assert_eq!(wrong.writes, 0);
    }
    #[test]
    fn full_format_replaces_the_complete_bounded_volume_and_mounts_independently() {
        use sha2::{Digest, Sha256};
        for (kind, size, sector, cluster) in [
            (FatType::Fat12, 2 * MIB, 4096, 4096),
            (FatType::Fat16, 8 * MIB, 512, 512),
            (FatType::Fat32, 34 * MIB, 512, 512),
        ] {
            let plan = Plan::new(kind, size, sector, cluster, "FULL_DATA", 42, 2048).unwrap();
            let guard = 4096;
            let mut storage = vec![0xa5; size as usize + 2 * guard];
            let mut last = 0;
            {
                let mut io = std::io::Cursor::new(&mut storage[guard..guard + size as usize]);
                let proof = plan
                    .format_full(&mut io, |done, total| {
                        assert!(done >= last && done <= total);
                        last = done;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(last, size * 2);
                assert_eq!(proof.bytes, size);
                assert_eq!(proof.sha256, format!("{:x}", Sha256::digest(io.get_ref())));
                assert!(
                    io.get_ref()[plan.metadata_sectors() as usize * sector as usize..]
                        .iter()
                        .all(|b| *b == 0)
                );
                io.seek(SeekFrom::Start(0)).unwrap();
                let fs = fatfs::FileSystem::new(&mut io, fatfs::FsOptions::new()).unwrap();
                assert_eq!(format!("{:?}", fs.fat_type()).to_uppercase(), kind.name());
                assert_eq!(fs.volume_label(), "FULL_DATA");
                assert_eq!(fs.root_dir().iter().count(), 0);
                fs.unmount().unwrap();
            }
            assert!(storage[..guard].iter().all(|b| *b == 0xa5));
            assert!(storage[guard + size as usize..].iter().all(|b| *b == 0xa5));
        }
    }
    #[test]
    fn full_readback_detects_old_tail_data_and_write_failures_never_retry() {
        let plan = Plan::new(FatType::Fat12, 2 * MIB, 512, 512, "DATA", 42, 0).unwrap();
        let mut io = std::io::Cursor::new(vec![0xa5; plan.size() as usize]);
        plan.format(&mut io, |_, _| Ok(())).unwrap();
        assert!(plan.verify_full(&mut io).is_err());
        plan.format_full(&mut io, |_, _| Ok(())).unwrap();
        let end = plan.size() as usize - 1;
        io.get_mut()[end] = 1;
        assert!(plan.verify_full(&mut io).is_err());
        for cancel in [false, true] {
            let mut io = Sparse::new(plan.size(), 512);
            if !cancel {
                io.fail_write = Some(1);
            }
            assert!(plan
                .format_full(&mut io, |done, _| if cancel && done > 0 {
                    Err(io::Error::other("cancel"))
                } else {
                    Ok(())
                })
                .is_err());
            assert_eq!(io.writes, if cancel { 2 } else { 1 });
            io.seek(SeekFrom::Start(0)).unwrap();
            let mut boot = [1; 512];
            io.read_exact(&mut boot).unwrap();
            assert_eq!(boot, [0; 512]);
        }
        let mut wrong = Sparse::new(plan.size() + 512, 512);
        assert!(plan.format_full(&mut wrong, |_, _| Ok(())).is_err());
        assert_eq!(wrong.writes, 0);
    }
}

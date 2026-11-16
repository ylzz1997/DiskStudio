//! Additional standalone container metadata, encoded from owned logical bytes.
//! VHDX: MS-VHDX sections 2.2, 2.5, 2.6. VMDK: hosted sparse v1.
//! Parallels: extended structured HDS layout (QEMU/OpenVZ public definition).
use super::*;
const MIB: u64 = CHUNK as u64;

fn align(n: u64, unit: u64) -> u64 {
    n.div_ceil(unit) * unit
}
fn crc32c(b: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in b {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn stamp_crc(b: &mut [u8]) {
    b[4..8].fill(0);
    let crc = crc32c(b);
    put32(b, 4, crc, false);
}
// Fixed schema GUID constants in their on-disk byte order, not arbitrary input.
fn guid(b: &mut [u8], at: usize, text: &str) {
    for i in 0..16 {
        b[at + i] = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("schema GUID");
    }
}
fn bat_size(o: &Options) -> u64 {
    let blocks = o.size.div_ceil(o.format.block_size());
    let ratio = (1 << 23) * o.sector / o.format.block_size();
    align((blocks + (blocks - 1) / ratio) * 8, MIB)
}
pub(super) fn vhdx_data_start(o: &Options) -> u64 {
    3 * MIB + bat_size(o)
}

pub(super) struct VmdkLayout {
    pub rgd: u64,
    pub gd: u64,
    pub dir_bytes: u64,
    pub tables: u64,
    pub data: u64,
}
pub(super) fn vmdk_layout(blocks: u64) -> VmdkLayout {
    let tables = blocks.div_ceil(512);
    let dir_bytes = align512(tables * 4);
    let rgd = 21 * 512;
    let gd = rgd + dir_bytes + tables * 2048;
    VmdkLayout {
        rgd,
        gd,
        dir_bytes,
        tables,
        data: align(gd + dir_bytes + tables * 2048, MIB),
    }
}
pub(super) fn validate_vmdk_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.encode_utf16().count() > 220
        || name.trim() != name
        || name.ends_with('.')
        || !name.to_lowercase().ends_with(".vmdk")
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return Err("VMDK 描述符需要普通的 .vmdk 文件名，不能包含路径或控制字符。".into());
    }
    Ok(())
}

impl<W: Read + Write + Seek> Encoder<W> {
    // A VHDX payload block is 8 MiB while transport stays at 1 MiB. Allocate
    // lazily, explicitly zero the entire new block before any payload writes,
    // and retain earlier all-zero slices when a later slice becomes nonzero.
    pub(super) fn push_vhdx(&mut self, bytes: &[u8]) -> Result<(), String> {
        let unit = self.options.format.block_size();
        let block = (self.received / unit) as usize;
        if self.map[block] == EMPTY
            && (self.options.format == Format::VhdxFixed || bytes.iter().any(|v| *v != 0))
        {
            self.io.seek(SeekFrom::Start(self.next)).map_err(err)?;
            let zero = vec![0; CHUNK];
            for _ in 0..unit / MIB {
                self.io.write_all(&zero).map_err(err)?;
            }
            self.map[block] = self.allocated;
            self.allocated += 1;
            self.next += unit;
        }
        if self.map[block] != EMPTY {
            let at = self.data_start + u64::from(self.map[block]) * unit + self.received % unit;
            self.io.seek(SeekFrom::Start(at)).map_err(err)?;
            self.io.write_all(bytes).map_err(err)?;
        }
        Ok(())
    }
    pub(super) fn vhdx_metadata(&self) -> Vec<(u64, Vec<u8>)> {
        let mut first = vec![0; CHUNK];
        first[..8].copy_from_slice(b"vhdxfile");
        for (i, c) in "DiskStudio".encode_utf16().enumerate() {
            first[8 + i * 2..10 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        for (at, seq) in [(64 * 1024, 1), (128 * 1024, 2)] {
            let h = &mut first[at..at + 4096];
            h[..4].copy_from_slice(b"head");
            put64(h, 8, seq, false);
            h[16..32].copy_from_slice(&self.identity.created);
            h[32..48].copy_from_slice(&self.identity.modified);
            h[66..68].copy_from_slice(&1u16.to_le_bytes());
            put32(h, 68, CHUNK as u32, false);
            put64(h, 72, MIB, false);
            stamp_crc(h);
        }
        for at in [192 * 1024, 256 * 1024] {
            let r = &mut first[at..at + 65536];
            r[..4].copy_from_slice(b"regi");
            put32(r, 8, 2, false);
            guid(r, 16, "6677c22d23f600429d64115e9bfd4a08");
            put64(r, 32, 3 * MIB, false);
            put32(r, 40, bat_size(&self.options) as u32, false);
            put32(r, 44, 1, false);
            guid(r, 48, "06a27c8b90479a4bb8fe575f050f886e");
            put64(r, 64, 2 * MIB, false);
            put32(r, 72, CHUNK as u32, false);
            put32(r, 76, 1, false);
            stamp_crc(r);
        }
        let mut meta = vec![0; CHUNK];
        meta[..8].copy_from_slice(b"metadata");
        meta[10..12].copy_from_slice(&5u16.to_le_bytes());
        let items = [
            ("3767a1ca36fa434db3b633f0aa44e76b", 8, 4),
            ("2442a52f1bcd7648b2115dbed83bf4b8", 8, 6),
            ("ab12cabee6b2234593efc309e000c746", 16, 6),
            ("1dbf41816fa90947ba47f233a8faab5f", 4, 6),
            ("c748a3cd5d4471449cc9e9885251c556", 4, 6),
        ];
        for (i, (id, len, flags)) in items.iter().enumerate() {
            let entry = 32 + i * 32;
            guid(&mut meta, entry, id);
            put32(&mut meta, entry + 16, (65536 + i * 16) as u32, false);
            put32(&mut meta, entry + 20, *len, false);
            put32(&mut meta, entry + 24, *flags, false);
        }
        put32(
            &mut meta,
            65536,
            self.options.format.block_size() as u32,
            false,
        );
        put32(
            &mut meta,
            65540,
            u32::from(self.options.format == Format::VhdxFixed),
            false,
        );
        put64(&mut meta, 65552, self.options.size, false);
        meta[65568..65584].copy_from_slice(&self.identity.created);
        put32(&mut meta, 65584, self.options.sector as u32, false);
        put32(&mut meta, 65600, self.options.sector as u32, false);
        let mut bat = vec![0; bat_size(&self.options) as usize];
        let unit = self.options.format.block_size();
        let ratio = (1 << 23) * self.options.sector / unit;
        for (i, slot) in self.map.iter().enumerate() {
            let pos = (i as u64 + i as u64 / ratio) as usize * 8;
            let value = if *slot == EMPTY {
                2
            } else {
                (self.data_start + u64::from(*slot) * unit) | 6
            };
            put64(&mut bat, pos, value, false);
        }
        // Explicit zero log: LogGuid remains zero, so no replay is required.
        vec![
            (0, first),
            (MIB, vec![0; CHUNK]),
            (2 * MIB, meta),
            (3 * MIB, bat),
        ]
    }
    pub(super) fn vmdk_metadata(&self) -> Vec<(u64, Vec<u8>)> {
        let l = vmdk_layout(self.map.len() as u64);
        let mut b = vec![0; l.data as usize];
        put32(&mut b, 0, 0x564d444b, false);
        put32(&mut b, 4, 1, false);
        put32(&mut b, 8, 3, false); // newline detection and redundant grain directory
        put64(&mut b, 12, self.options.size / 512, false);
        put64(&mut b, 20, MIB / 512, false);
        put64(&mut b, 28, 1, false);
        put64(&mut b, 36, 20, false);
        put32(&mut b, 44, 512, false);
        put64(&mut b, 48, l.rgd / 512, false);
        put64(&mut b, 56, l.gd / 512, false);
        put64(&mut b, 64, l.data / 512, false);
        b[73..77].copy_from_slice(&[10, 32, 13, 10]);
        let cid = u32::from_le_bytes(self.identity.modified[..4].try_into().unwrap()) & 0x7fffffff;
        let desc = format!("# Disk DescriptorFile\nversion=1\nencoding=\"UTF-8\"\nCID={cid:08x}\nparentCID=ffffffff\ncreateType=\"monolithicSparse\"\n\nRW {} SPARSE \"{}\"\n\nddb.adapterType=\"lsilogic\"\nddb.virtualHWVersion=\"4\"\nddb.geometry.cylinders=\"{}\"\nddb.geometry.heads=\"255\"\nddb.geometry.sectors=\"63\"\nddb.logicalSectorSize=\"512\"\n", self.options.size/512, self.identity.file_name, (self.options.size/512/255/63).max(1));
        b[512..512 + desc.len()].copy_from_slice(desc.as_bytes());
        for base in [l.rgd, l.gd] {
            for table in 0..l.tables {
                let at = base + l.dir_bytes + table * 2048;
                put32(
                    &mut b,
                    (base + table * 4) as usize,
                    (at / 512) as u32,
                    false,
                );
            }
            for (i, slot) in self.map.iter().enumerate() {
                let value = if *slot == EMPTY {
                    0
                } else {
                    ((self.data_start + u64::from(*slot) * MIB) / 512) as u32
                };
                put32(&mut b, (base + l.dir_bytes) as usize + i * 4, value, false);
            }
        }
        vec![(0, b)]
    }
    pub(super) fn parallels_metadata(&self) -> Vec<(u64, Vec<u8>)> {
        let mut b = vec![0; self.data_start as usize];
        b[..16].copy_from_slice(b"WithouFreSpacExt");
        put32(&mut b, 16, 2, false);
        put32(&mut b, 20, 16, false);
        put32(
            &mut b,
            24,
            (self.options.size / 512 / 16 / 32) as u32,
            false,
        );
        put32(&mut b, 28, (MIB / 512) as u32, false);
        put32(&mut b, 32, self.map.len() as u32, false);
        put64(&mut b, 36, self.options.size / 512, false);
        put32(&mut b, 48, (self.data_start / 512) as u32, false);
        for (i, slot) in self.map.iter().enumerate() {
            let value = if *slot == EMPTY {
                0
            } else {
                (self.data_start / MIB) as u32 + slot
            };
            put32(&mut b, 64 + i * 4, value, false);
        }
        vec![(0, b)]
    }
}

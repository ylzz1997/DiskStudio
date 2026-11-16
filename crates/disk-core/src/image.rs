use super::{free, validate, Disk, Partition, MIB};
fn u32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64le(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn byte_lba(n: u64, sector: u64) -> Result<u64, String> {
    n.checked_mul(sector).ok_or("分区偏移溢出。".into())
}
// Bytes and size describe the logical disk. The frontend unwraps supported
// containers using bounded File slices before submitting these bytes.
pub fn parse_image(head: &[u8], size: u64, filename: &str) -> Result<Disk, String> {
    if head.len() < 512
        || head.len() > MIB as usize
        || size < 4 * MIB
        || size > 1 << 43
        || head.len() as u64 > size
    {
        return Err("镜像大小无效或头部读取范围超限。".into());
    }
    if filename.len() > 255 || filename.contains(['/', '\\', ':', '\0']) {
        return Err("只接受普通镜像文件名。".into());
    }
    let lower = filename.to_lowercase();
    if ![
        ".img", ".raw", ".vhd", ".avhd", ".vhdx", ".vdi", ".vmdk", ".hdd", ".hds",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
    {
        return Err(
            "仅接受 RAW/IMG 或已解码 VHD/AVHD/VHDX/VDI/VMDK/HDD/HDS 的逻辑磁盘字节。".into(),
        );
    }
    let sector = [512usize, 4096]
        .into_iter()
        .find(|s| head.get(*s..*s + 8) == Some(b"EFI PART"));
    if [".vhd", ".avhd", ".vdi", ".vmdk", ".hdd", ".hds"]
        .iter()
        .any(|ext| lower.ends_with(ext))
        && sector == Some(4096)
    {
        return Err(
            "当前 VHD v1/VDI/VMDK/HDD/HDS 使用 512 B 逻辑扇区，不能按 4Kn GPT 解释。".into(),
        );
    }
    let mut parts = Vec::new();
    let (scheme, sector_size, first, last) = if let Some(s) = sector {
        if head.len() < s + 92 {
            return Err("GPT 头部不完整。".into());
        }
        let h = &head[s..];
        let hsize = u32le(h, 12) as usize;
        if !(92..=s).contains(&hsize) || s + hsize > head.len() {
            return Err("GPT 头部长度无效。".into());
        }
        let mut header = h[..hsize].to_vec();
        header[16..20].fill(0);
        if crc32fast::hash(&header) != u32le(h, 16) {
            return Err("GPT 头部校验失败；当前版本不会自动修复镜像。".into());
        }
        if u64le(h, 24) != 1 || u64le(h, 32) >= size / s as u64 {
            return Err("GPT 头部位置不合法。".into());
        }
        let first = byte_lba(u64le(h, 40), s as u64)?;
        let last = byte_lba(u64le(h, 48).checked_add(1).ok_or("LBA 溢出。")?, s as u64)?;
        let at = byte_lba(u64le(h, 72), s as u64)?;
        let count = u32le(h, 80) as u64;
        let stride = u32le(h, 84) as u64;
        if count == 0 || count > 128 || !(128..=4096).contains(&stride) || stride % 128 != 0 {
            return Err("GPT 条目数量或长度暂不支持。".into());
        }
        let end = at.checked_add(count * stride).ok_or("GPT 表溢出。")?;
        if at < 2 * s as u64
            || end > head.len() as u64
            || end > first
            || first >= last
            || last > size
        {
            return Err("GPT 分区表超出头部读取范围或可用区域无效。".into());
        }
        if crc32fast::hash(&head[at as usize..end as usize]) != u32le(h, 88) {
            return Err("GPT 分区表校验失败。".into());
        }
        for n in 0..count {
            let entry = &head[(at + n * stride) as usize..(at + (n + 1) * stride) as usize];
            if entry[..16].iter().all(|b| *b == 0) {
                continue;
            }
            let start = byte_lba(u64le(entry, 32), s as u64)?;
            let finish = byte_lba(
                u64le(entry, 40).checked_add(1).ok_or("分区末端溢出。")?,
                s as u64,
            )?;
            if start < first || finish > last || finish <= start {
                return Err("GPT 分区越界或长度无效。".into());
            }
            let utf16: Vec<u16> = entry[56..128]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take_while(|c| *c != 0)
                .collect();
            let label = String::from_utf16_lossy(&utf16);
            let system = entry[..16]
                == [
                    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0, 0xa0, 0xc9,
                    0x3e, 0xc9, 0x3b,
                ];
            parts.push(part(
                n,
                if label.is_empty() {
                    format!("分区 {}", n + 1)
                } else {
                    label
                },
                "未知",
                start,
                finish - start,
                system,
            ));
        }
        ("GPT", s as u64, first, last)
    } else {
        if head[510..512] != [0x55, 0xaa] {
            return Err("未识别到 GPT/MBR 分区表。请使用包含分区表的完整磁盘镜像。".into());
        }
        for n in 0..4 {
            let i = 446 + n * 16;
            let t = head[i + 4];
            if t == 0 {
                continue;
            }
            if t == 0xee {
                return Err("保护性 MBR 存在，但 GPT 头部损坏或不可读取。".into());
            }
            if [0x05, 0x0f, 0x85].contains(&t) {
                return Err("镜像包含扩展分区，请提供 EBR 扇区进行完整检查。".into());
            }
            let start = u32le(head, i + 8) as u64 * 512;
            let len = u32le(head, i + 12) as u64 * 512;
            if start < 512 || len == 0 || start + len > size {
                return Err("MBR 分区范围无效。".into());
            }
            parts.push(part(
                n as u64,
                format!("分区 {}", n + 1),
                "未知",
                start,
                len,
                false,
            ));
        }
        ("MBR", 512, 512, size / 512 * 512)
    };
    parts.sort_by_key(|p| p.start);
    let mut all = Vec::new();
    let mut cursor = first;
    for p in parts {
        if p.start < cursor {
            return Err("检测到重叠分区。".into());
        }
        if p.start > cursor {
            all.push(free(cursor, p.start - cursor, format!("gap-{cursor}")));
        }
        cursor = p.start + p.size;
        all.push(p);
    }
    if cursor < last {
        all.push(free(cursor, last - cursor, format!("gap-{cursor}")));
    }
    let d = Disk {
        id: format!("image-{:x}-{size}", crc32fast::hash(head)),
        name: filename
            .rsplit_once('.')
            .map_or(filename, |(stem, _)| stem)
            .into(),
        filename: filename.into(),
        size,
        sector_size,
        scheme: scheme.into(),
        kind: "image".into(),
        partitions: all,
    };
    validate(&d)?;
    Ok(d)
}
fn part(n: u64, name: String, fs: &str, start: u64, size: u64, system: bool) -> Partition {
    Partition {
        id: format!("partition-{n}"),
        name,
        fs: fs.into(),
        start,
        size,
        used: None,
        color: ["coral", "violet", "blue", "mint"][n as usize % 4].into(),
        system,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn mbr() -> Vec<u8> {
        let mut h = vec![0; 512];
        h[510] = 0x55;
        h[511] = 0xaa;
        h[450] = 0x0c;
        h[454..458].copy_from_slice(&2048u32.to_le_bytes());
        h[458..462].copy_from_slice(&4096u32.to_le_bytes());
        h
    }
    #[test]
    fn imports_mbr_read_only_with_unknown_usage() {
        let d = parse_image(&mbr(), 8 * MIB, "example.img").unwrap();
        assert_eq!(d.kind, "image");
        assert_eq!(d.partitions[1].used, None);
    }
    #[test]
    fn rejects_truncated_and_device_names() {
        assert!(parse_image(&[0; 12], 8 * MIB, "a.img").is_err());
        assert!(parse_image(&mbr(), 8 * MIB, "\\\\.\\PhysicalDrive0").is_err());
    }
    #[test]
    fn rejects_overlapping_and_extended_partitions() {
        let mut h = mbr();
        let e = h[446..462].to_vec();
        h[462..478].copy_from_slice(&e);
        assert!(parse_image(&h, 8 * MIB, "bad.img").is_err());
        h[450] = 0x0f;
        assert!(parse_image(&h, 8 * MIB, "bad.img").is_err());
    }
    #[test]
    fn arbitrary_input_never_panics() {
        for n in [0, 1, 511, 512, 1024, 4096, 8192] {
            assert!(parse_image(&vec![255; n], 8 * MIB, "random.img").is_err());
        }
    }
    fn gpt(sector: usize) -> Vec<u8> {
        let mut h = vec![0u8; 1024 * 1024];
        h[510] = 0x55;
        h[511] = 0xaa;
        h[450] = 0xee;
        let entry = sector * 2;
        h[entry] = 1;
        h[entry + 32..entry + 40].copy_from_slice(&(MIB / sector as u64).to_le_bytes());
        h[entry + 40..entry + 48].copy_from_slice(&(3 * MIB / sector as u64 - 1).to_le_bytes());
        h[entry + 56..entry + 58].copy_from_slice(&('测' as u16).to_le_bytes());
        let table_crc = crc32fast::hash(&h[entry..entry + 128 * 128]);
        let b = &mut h[sector..sector + 92];
        b[..8].copy_from_slice(b"EFI PART");
        b[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        b[12..16].copy_from_slice(&92u32.to_le_bytes());
        b[24..32].copy_from_slice(&1u64.to_le_bytes());
        b[32..40].copy_from_slice(&(8 * MIB / sector as u64 - 1).to_le_bytes());
        b[40..48].copy_from_slice(&(MIB / sector as u64).to_le_bytes());
        b[48..56].copy_from_slice(&(7 * MIB / sector as u64 - 1).to_le_bytes());
        b[72..80].copy_from_slice(&2u64.to_le_bytes());
        b[80..84].copy_from_slice(&128u32.to_le_bytes());
        b[84..88].copy_from_slice(&128u32.to_le_bytes());
        b[88..92].copy_from_slice(&table_crc.to_le_bytes());
        let crc = crc32fast::hash(b);
        b[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }
    #[test]
    fn supports_512_and_4096_gpt_with_utf16_names() {
        for sector in [512, 4096] {
            let disk = parse_image(&gpt(sector), 8 * MIB, "sample.img").unwrap();
            assert_eq!(disk.sector_size, sector as u64);
            assert_eq!(disk.partitions[0].name, "测");
            assert_eq!(disk.partitions[0].size, 2 * MIB);
        }
    }
    #[test]
    fn rejects_corrupt_gpt_checksums_and_truncated_table() {
        let mut h = gpt(512);
        h[512 + 16] ^= 1;
        assert!(parse_image(&h, 8 * MIB, "bad.img").is_err());
        let mut h = gpt(512);
        h[1024 + 56] ^= 1;
        assert!(parse_image(&h, 8 * MIB, "bad.img").is_err());
        assert!(parse_image(&gpt(4096)[..9000], 8 * MIB, "cut.img").is_err());
    }
}

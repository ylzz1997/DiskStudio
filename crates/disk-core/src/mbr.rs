//! Bounded MBR/EBR decoding from supplied bytes only. No I/O.
use crate::{free, validate, Disk, Partition, MIB};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_EBRS: usize = 64;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EbrSector {
    pub offset: u64,
    pub bytes: Vec<u8>,
}
pub(crate) struct Layout {
    sector: u64,
    parts: Vec<Partition>,
    tables: Vec<u64>,
    pub extended: Option<(u64, u64)>,
    pub logical_count: usize,
}
pub(crate) enum Scan {
    Need(u64),
    Complete(Layout),
}
pub(crate) struct Problem {
    pub incomplete: bool,
    pub detail: String,
}
fn bad(detail: impl Into<String>) -> Problem {
    Problem {
        incomplete: false,
        detail: detail.into(),
    }
}
fn unsupported(detail: &str) -> Problem {
    Problem {
        incomplete: true,
        detail: detail.into(),
    }
}
pub(crate) fn validate_sectors(
    sectors: &[EbrSector],
    size: u64,
    sector: u64,
) -> Result<(), String> {
    let mut offsets = HashSet::new();
    if !matches!(sector, 512 | 4096)
        || sectors.len() > MAX_EBRS
        || sectors.iter().any(|s| {
            s.bytes.len() as u64 != sector
                || s.offset < sector
                || s.offset % sector != 0
                || s.offset.checked_add(sector).is_none_or(|end| end > size)
                || !offsets.insert(s.offset)
        })
    {
        return Err("EBR 输入须为最多 64 个不重复、按逻辑扇区对齐且位于磁盘内的完整扇区。".into());
    }
    Ok(())
}

/// Returns only the next validated 512-byte range, never a path or device name.
/// Structural failures stop further reads; final inspection supplies diagnostics.
pub fn next_ebr(mbr: &[u8], size: u64, sectors: &[EbrSector]) -> Result<Option<u64>, String> {
    next_ebr_with_sector(mbr, size, 512, sectors)
}

pub fn next_ebr_with_sector(
    mbr: &[u8],
    size: u64,
    sector: u64,
    sectors: &[EbrSector],
) -> Result<Option<u64>, String> {
    if !matches!(sector, 512 | 4096)
        || mbr.len() as u64 != sector
        || !(4 * MIB..=1 << 43).contains(&size)
    {
        return Err("MBR 读取计划的头部或镜像容量无效。".into());
    }
    validate_sectors(sectors, size, sector)?;
    Ok(match scan(mbr, size, sector, sectors) {
        Ok(Scan::Need(offset)) => Some(offset),
        _ => None,
    })
}
#[derive(Clone, Copy)]
struct Entry {
    kind: u8,
    start: u64,
    len: u64,
}
fn extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
}
fn entry(bytes: &[u8], sector: u64) -> Result<Option<Entry>, Problem> {
    if !matches!(bytes[0], 0 | 0x80) {
        return Err(bad("分区条目含非法启动标记。"));
    }
    if bytes[4] == 0 {
        return if bytes.iter().all(|b| *b == 0) {
            Ok(None)
        } else {
            Err(bad("未使用的分区条目含非零数据。"))
        };
    }
    let start = u64::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap())) * sector;
    let len = u64::from(u32::from_le_bytes(bytes[12..16].try_into().unwrap())) * sector;
    if len == 0 {
        return Err(bad("分区条目长度为零。"));
    }
    Ok(Some(Entry {
        kind: bytes[4],
        start,
        len,
    }))
}
fn overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}
fn part(number: usize, start: u64, len: u64, logical: bool) -> Partition {
    Partition {
        id: format!("partition-{}", number - 1),
        name: format!("{}分区 {number}", if logical { "逻辑" } else { "主" }),
        fs: "未知".into(),
        start,
        size: len,
        used: None,
        color: ["coral", "violet", "blue", "mint"][(number - 1) % 4].into(),
        system: false,
    }
}
pub(crate) fn scan(
    mbr: &[u8],
    size: u64,
    sector: u64,
    sectors: &[EbrSector],
) -> Result<Scan, Problem> {
    if !matches!(sector, 512 | 4096) || mbr.len() < sector as usize {
        return Err(bad("MBR 缺少完整的逻辑扇区。"));
    }
    if mbr[510..512] != [0x55, 0xaa] {
        return Err(bad("缺少 MBR 的 55AA 签名。"));
    }
    if size % sector != 0 {
        return Err(bad("容量不是逻辑扇区的整数倍。"));
    }
    let mut layout = Layout {
        sector,
        parts: vec![],
        tables: vec![],
        extended: None,
        logical_count: 0,
    };
    let mut primary_ranges = Vec::new();
    for (i, slot) in mbr[446..510].chunks_exact(16).enumerate() {
        let Some(e) = entry(slot, sector)? else {
            continue;
        };
        if e.kind == 0xee {
            return Err(bad("保护性 MBR 需要使用 GPT 检查。"));
        }
        let end = e.start + e.len; // Both fields are bounded u32 sector counts.
        if e.start < sector || end > size {
            return Err(bad("MBR 主条目越出镜像范围。"));
        }
        if primary_ranges.iter().any(|r| overlap(*r, (e.start, end))) {
            return Err(bad("MBR 主条目互相重叠（含扩展分区容器）。"));
        }
        primary_ranges.push((e.start, end));
        if extended(e.kind) {
            if layout.extended.is_some() {
                return Err(unsupported("暂不支持多个扩展分区容器，未导入部分布局。"));
            }
            layout.extended = Some((e.start, end));
        } else {
            layout.parts.push(part(i + 1, e.start, e.len, false));
        }
    }
    if let Some((base, limit)) = layout.extended {
        let mut at = base;
        loop {
            if layout.tables.contains(&at) {
                return Err(bad("EBR 链包含循环或重复链路。"));
            }
            if layout.tables.len() == MAX_EBRS {
                return Err(unsupported("EBR 链超过 64 个扇区的检查上限。"));
            }
            if at < base || at + sector > limit {
                return Err(bad("EBR 位置越出扩展分区。"));
            }
            if layout
                .parts
                .iter()
                .any(|p| overlap((at, at + sector), (p.start, p.start + p.size)))
            {
                return Err(bad("EBR 分区表扇区与分区数据范围重叠。"));
            }
            let Some(block) = sectors.get(layout.tables.len()) else {
                return Ok(Scan::Need(at));
            };
            if block.offset != at || block.bytes.len() as u64 != sector {
                return Err(bad("提供的 EBR 扇区与链路请求不一致。"));
            }
            let bytes = &block.bytes;
            if bytes[510..512] != [0x55, 0xaa] {
                return Err(bad(format!("偏移 {at} B 的 EBR 缺少 55AA 签名。")));
            }
            if bytes[478..510].iter().any(|b| *b != 0) {
                return Err(unsupported(
                    "EBR 使用第 3、4 个条目，当前不支持此扩展布局。",
                ));
            }
            layout.tables.push(at);
            if let Some(e) = entry(&bytes[446..462], sector)? {
                if extended(e.kind) {
                    return Err(unsupported(
                        "EBR 第一个条目为链路，当前只支持常规数据/链路布局。",
                    ));
                }
                if e.kind == 0xee {
                    return Err(bad("逻辑分区使用了 GPT 保护类型。"));
                }
                let start = at + e.start;
                let end = start + e.len;
                if e.start < sector || start < base || end > limit {
                    return Err(bad("逻辑分区越出扩展分区范围或覆盖自身 EBR。"));
                }
                if layout
                    .parts
                    .iter()
                    .any(|p| overlap((start, end), (p.start, p.start + p.size)))
                {
                    return Err(bad("逻辑分区的数据范围互相重叠。"));
                }
                if layout
                    .tables
                    .iter()
                    .any(|t| overlap((start, end), (*t, *t + sector)))
                {
                    return Err(bad("逻辑分区覆盖了 EBR 分区表扇区。"));
                }
                layout
                    .parts
                    .push(part(5 + layout.logical_count, start, e.len, true));
                layout.logical_count += 1;
            }
            let Some(link) = entry(&bytes[462..478], sector)? else {
                break;
            };
            if !extended(link.kind) {
                return Err(unsupported(
                    "EBR 第二个条目不是扩展链路，当前不支持此布局。",
                ));
            }
            let next = base + link.start;
            if next + link.len > limit {
                return Err(bad("EBR 链路声明的范围越出扩展分区。"));
            }
            at = next;
        }
    }
    if sectors.len() != layout.tables.len() {
        return Err(bad("包含不属于当前 EBR 链的多余扇区。"));
    }
    Ok(Scan::Complete(layout))
}
impl Layout {
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }
    pub fn into_disk(self, size: u64, filename: &str) -> Result<Disk, String> {
        // EBR metadata is reserved, not free space. Flatten only non-overlapping
        // data extents; split free gaps at the extended-container boundaries.
        let mut events: Vec<_> = self
            .parts
            .into_iter()
            .map(|p| (p.start, p.start + p.size, Some(p)))
            .collect();
        events.extend(
            self.tables
                .into_iter()
                .map(|at| (at, at + self.sector, None)),
        );
        events.sort_by_key(|e| e.0);
        let mut parts = Vec::new();
        let mut cursor = self.sector;
        let gap = |start: u64, end: u64, parts: &mut Vec<Partition>| {
            let mut boundaries = vec![start, end];
            if let Some((base, limit)) = self.extended {
                boundaries.extend(
                    [base, limit]
                        .into_iter()
                        .filter(|at| *at > start && *at < end),
                );
            }
            boundaries.sort_unstable();
            for pair in boundaries.windows(2).filter(|p| p[0] < p[1]) {
                let mut p = free(pair[0], pair[1] - pair[0], format!("gap-{}", pair[0]));
                if self
                    .extended
                    .is_some_and(|(base, limit)| pair[0] >= base && pair[1] <= limit)
                {
                    p.name = "扩展分区内空闲".into();
                }
                parts.push(p);
            }
        };
        for (start, end, p) in events {
            gap(cursor, start, &mut parts);
            cursor = end;
            if let Some(p) = p {
                parts.push(p);
            }
        }
        gap(cursor, size, &mut parts);
        let d = Disk {
            id: String::new(),
            name: filename
                .rsplit_once('.')
                .map_or(filename, |(stem, _)| stem)
                .into(),
            filename: filename.into(),
            size,
            sector_size: self.sector,
            scheme: "MBR".into(),
            kind: "image".into(),
            partitions: parts,
        };
        validate(&d)?;
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_check::{inspect_image_with_ebrs, CheckState};
    const SIZE: u64 = 16 * MIB;
    fn put(bytes: &mut [u8], slot: usize, kind: u8, lba: u32, sectors: u32) {
        let e = &mut bytes[446 + slot * 16..462 + slot * 16];
        e.fill(0);
        e[4] = kind;
        e[8..12].copy_from_slice(&lba.to_le_bytes());
        e[12..16].copy_from_slice(&sectors.to_le_bytes());
    }
    fn table() -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        bytes
    }
    fn fixture() -> (Vec<u8>, Vec<EbrSector>) {
        let mut mbr = table();
        put(&mut mbr, 0, 7, 2048, 2048);
        put(&mut mbr, 1, 0x0f, 8192, 16384);
        let mut a = table();
        put(&mut a, 0, 0x83, 2048, 2048);
        put(&mut a, 1, 0x0f, 8192, 8192);
        let mut b = table();
        put(&mut b, 0, 0x0c, 2048, 2048);
        (
            mbr,
            vec![
                EbrSector {
                    offset: 4 * MIB,
                    bytes: a,
                },
                EbrSector {
                    offset: 8 * MIB,
                    bytes: b,
                },
            ],
        )
    }
    fn report(mbr: &[u8], ebrs: &[EbrSector]) -> crate::table_check::ImageInspection {
        inspect_image_with_ebrs(mbr, &[0; 512], SIZE, "logical.img", ebrs).unwrap()
    }
    fn failed(mbr: &[u8], ebrs: &[EbrSector], text: &str) {
        let result = report(mbr, ebrs);
        assert!(result.disk.is_none());
        assert_eq!(result.report.state, CheckState::Failed);
        assert!(result.parse_error.unwrap().contains(text));
    }
    #[test]
    fn relative_bases_and_reserved_metadata_are_preserved() {
        let (mbr, sectors) = fixture();
        assert_eq!(next_ebr(&mbr, SIZE, &[]).unwrap(), Some(4 * MIB));
        assert_eq!(next_ebr(&mbr, SIZE, &sectors[..1]).unwrap(), Some(8 * MIB));
        assert_eq!(next_ebr(&mbr, SIZE, &sectors).unwrap(), None);
        let result = report(&mbr, &sectors);
        assert_eq!(result.report.state, CheckState::Warning); // MBR sector-size assumption.
        let d = result.disk.unwrap();
        let data: Vec<_> = d.partitions.iter().filter(|p| p.fs != "—").collect();
        assert_eq!(
            data.iter()
                .map(|p| (p.name.as_str(), p.start))
                .collect::<Vec<_>>(),
            vec![
                ("主分区 1", MIB),
                ("逻辑分区 5", 5 * MIB),
                ("逻辑分区 6", 9 * MIB)
            ]
        );
        assert!(data
            .iter()
            .all(|p| p.fs == "未知" && p.used.is_none() && !p.system));
        for at in [4 * MIB, 8 * MIB] {
            assert!(!d
                .partitions
                .iter()
                .any(|p| overlap((at, at + 512), (p.start, p.start + p.size))));
        }
        assert_eq!(
            d.partitions.iter().map(|p| p.size).sum::<u64>(),
            SIZE - 3 * 512
        );
        assert!(d.partitions.iter().any(|p| p.name == "扩展分区内空闲"));
    }
    #[test]
    fn all_three_extended_types_and_empty_container_are_supported() {
        for kind in [0x05, 0x0f, 0x85] {
            let (mut mbr, mut sectors) = fixture();
            mbr[466] = kind;
            sectors[0].bytes[466] = kind;
            assert!(report(&mbr, &sectors).disk.is_some());
            sectors[0].bytes[446..510].fill(0);
            assert!(report(&mbr, &sectors[..1]).disk.is_some());
        }
    }
    #[test]
    fn missing_chain_does_not_import_partial_primary_or_logical_layout() {
        let (mbr, sectors) = fixture();
        for supplied in [&sectors[..0], &sectors[..1]] {
            let result = report(&mbr, supplied);
            assert!(result.disk.is_none());
            assert_eq!(result.report.state, CheckState::Unchecked);
        }
    }
    #[test]
    fn loops_and_self_links_stop_before_another_read() {
        for relative in [0, 8192] {
            let (mbr, mut sectors) = fixture();
            put(&mut sectors[1].bytes, 1, 0x0f, relative, 2048);
            failed(&mbr, &sectors, "循环");
            assert_eq!(next_ebr(&mbr, SIZE, &sectors).unwrap(), None);
        }
    }
    #[test]
    fn refuses_out_of_container_data_links_and_primary_overlap() {
        let (mbr, sectors) = fixture();
        let mut bad_data = sectors.clone();
        put(&mut bad_data[1].bytes, 0, 7, 2048, 8192);
        failed(&mbr, &bad_data, "范围");
        let mut bad_link = sectors.clone();
        put(&mut bad_link[0].bytes, 1, 0x0f, 16384, 2048);
        failed(&mbr, &bad_link[..1], "链路");
        assert_eq!(next_ebr(&mbr, SIZE, &bad_link[..1]).unwrap(), None);
        let mut overlap_mbr = mbr.clone();
        put(&mut overlap_mbr, 2, 7, 8192, 2048);
        failed(&overlap_mbr, &sectors, "主条目");
    }
    #[test]
    fn data_overlap_and_table_overlap_are_rejected() {
        let (mbr, mut sectors) = fixture();
        // Second EBR precedes the first data extent; both data extents at 5 MiB.
        put(&mut sectors[0].bytes, 1, 0x0f, 1024, 15360);
        sectors[1].offset = 4 * MIB + MIB / 2;
        put(&mut sectors[1].bytes, 0, 7, 1024, 2048);
        failed(&mbr, &sectors, "数据范围");
        let (mbr, mut sectors) = fixture();
        put(&mut sectors[0].bytes, 0, 7, 2048, 8192);
        failed(&mbr, &sectors, "分区表扇区");
        assert_eq!(next_ebr(&mbr, SIZE, &sectors[..1]).unwrap(), None);
    }
    #[test]
    fn validates_ebr_signature_flags_empty_slots_and_zero_length() {
        for (byte, value) in [(510, 0), (446, 0x81), (450, 0), (458, 0)] {
            let (mbr, mut sectors) = fixture();
            if byte == 458 {
                sectors[0].bytes[458..462].fill(0);
            } else {
                sectors[0].bytes[byte] = value;
            }
            let r = report(&mbr, &sectors);
            assert_eq!(r.report.state, CheckState::Failed);
            assert!(r.disk.is_none());
        }
    }
    #[test]
    fn alternate_ebr_layouts_are_unchecked_not_reported_as_complete() {
        for slot in [0, 1, 2, 3] {
            let (mbr, mut sectors) = fixture();
            put(
                &mut sectors[0].bytes,
                slot,
                if slot == 0 { 0x0f } else { 7 },
                2048,
                2048,
            );
            let r = report(&mbr, &sectors);
            assert_eq!(r.report.state, CheckState::Unchecked);
            assert!(r.disk.is_none());
        }
    }
    #[test]
    fn hostile_chunk_contracts_and_unreferenced_bytes_are_rejected() {
        let (mbr, sectors) = fixture();
        for bad in [
            vec![EbrSector {
                offset: u64::MAX,
                bytes: vec![0; 512],
            }],
            vec![EbrSector {
                offset: 1,
                bytes: vec![0; 512],
            }],
            vec![EbrSector {
                offset: SIZE,
                bytes: vec![0; 512],
            }],
            vec![EbrSector {
                offset: MIB,
                bytes: vec![0; 511],
            }],
            vec![sectors[0].clone(); 2],
            vec![sectors[0].clone(); 65],
        ] {
            assert!(next_ebr(&mbr, SIZE, &bad).is_err());
            assert!(inspect_image_with_ebrs(&mbr, &[0; 512], SIZE, "a.img", &bad).is_err());
        }
        failed(&mbr, &[sectors[1].clone()], "链路请求");
        let mut extra = sectors.clone();
        extra.push(EbrSector {
            offset: 14 * MIB,
            bytes: table(),
        });
        failed(&mbr, &extra, "多余");
    }
    #[test]
    fn bounded_chain_limit_and_empty_link_nodes() {
        let mut mbr = table();
        put(&mut mbr, 0, 0x0f, 2048, 30000);
        let mut sectors = Vec::new();
        for n in 0..MAX_EBRS {
            let mut bytes = table();
            put(&mut bytes, 1, 0x0f, (n as u32 + 1) * 2, 2);
            sectors.push(EbrSector {
                offset: MIB + n as u64 * 1024,
                bytes,
            });
        }
        let r = report(&mbr, &sectors);
        assert_eq!(r.report.state, CheckState::Unchecked);
        assert!(r.parse_error.unwrap().contains("64"));
        assert_eq!(next_ebr(&mbr, SIZE, &sectors).unwrap(), None);
        sectors.last_mut().unwrap().bytes[462..478].fill(0);
        assert!(report(&mbr, &sectors).disk.is_some());
    }
    #[test]
    fn snapshot_conflicts_and_ebr_identity_changes_are_detected() {
        let (mbr, mut sectors) = fixture();
        let old = report(&mbr, &sectors).disk.unwrap().id;
        sectors[1].bytes[446] = 0x80;
        assert_ne!(old, report(&mbr, &sectors).disk.unwrap().id);
        let mut head = vec![0; MIB as usize];
        head[..512].copy_from_slice(&mbr);
        put(&mut head, 1, 0x0f, 1024, 16384);
        sectors[0].offset = MIB / 2;
        assert!(inspect_image_with_ebrs(&head, &[0; 512], SIZE, "a.img", &sectors).is_err());
    }
    #[test]
    fn arbitrary_input_and_small_buffers_never_panic_or_plan_out_of_bounds() {
        for len in [0, 1, 511, 512, 513, 4096] {
            let bytes = vec![255; len];
            assert!(!matches!(next_ebr(&bytes, SIZE, &[]), Ok(Some(_))));
        }
        let (mbr, _) = fixture();
        for size in [0, MIB, u64::MAX] {
            assert!(next_ebr(&mbr, size, &[]).is_err());
        }
    }
}

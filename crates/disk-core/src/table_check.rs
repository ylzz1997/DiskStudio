//! Read-only partition-table diagnostics over bounded caller-supplied bytes.
//! No file paths, device access, repair, or inference of filesystem contents.
use crate::mbr::{self, EbrSector, Scan};
use crate::{parse_image, Disk, MIB};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, ops::Range};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckState {
    Passed,
    Warning,
    Failed,
    Unchecked,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub code: String,
    pub title: String,
    pub state: CheckState,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableReport {
    pub filename: String,
    pub scheme: String,
    pub sector_size: Option<u64>,
    pub state: CheckState,
    pub items: Vec<CheckItem>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInspection {
    #[serde(skip)]
    pub mbr_container: Option<(u64, u64)>,
    #[serde(skip)]
    pub gpt_identity: Option<GptIdentity>,
    pub disk: Option<Disk>,
    pub parse_error: Option<String>,
    pub report: TableReport,
}

#[derive(PartialEq, Eq)]
pub struct GptIdentity {
    /// Disk GUID in the mixed-endian byte order stored in the header.
    pub disk_guid: [u8; 16],
    pub partitions: Vec<GptPartitionIdentity>,
}

#[derive(PartialEq, Eq)]
pub struct GptPartitionIdentity {
    /// One-based GPT entry slot, including positions of unused slots.
    pub number: u32,
    /// Unique partition and type GUIDs in on-disk byte order.
    pub guid: [u8; 16],
    pub kind: [u8; 16],
    pub attributes: u64,
    pub range: Range<u64>,
}
impl TableReport {
    fn add(&mut self, code: &str, title: &str, state: CheckState, detail: impl Into<String>) {
        self.items.push(CheckItem {
            code: code.into(),
            title: title.into(),
            state,
            detail: detail.into(),
        });
    }
    fn finish(&mut self) {
        self.state = if self.items.iter().any(|i| i.state == CheckState::Failed) {
            CheckState::Failed
        } else if self.items.iter().any(|i| i.state == CheckState::Unchecked) {
            CheckState::Unchecked
        } else if self.items.iter().any(|i| i.state == CheckState::Warning) {
            CheckState::Warning
        } else {
            CheckState::Passed
        };
    }
}
struct Ends<'a> {
    head: &'a [u8],
    tail: &'a [u8],
    size: u64,
}
impl<'a> Ends<'a> {
    fn get(&self, at: u64, len: u64) -> Option<&'a [u8]> {
        let end = at.checked_add(len)?;
        if end > self.size {
            return None;
        }
        if end <= self.head.len() as u64 {
            return self.head.get(at as usize..end as usize);
        }
        let base = self.size.checked_sub(self.tail.len() as u64)?;
        if at < base {
            return None;
        }
        self.tail.get((at - base) as usize..(end - base) as usize)
    }
}
fn u32le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64le(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
struct Gpt<'a> {
    first: u64,
    last: u64,
    guid: &'a [u8],
    count: usize,
    stride: usize,
    table: &'a [u8],
}
impl Gpt<'_> {
    fn agrees(&self, other: &Self) -> bool {
        self.guid == other.guid
            && self.first == other.first
            && self.last == other.last
            && self.count == other.count
            && self.stride == other.stride
            && self.table == other.table
    }

    fn partitions(&self, sector: u64) -> impl Iterator<Item = (u32, &[u8], Range<u64>)> {
        self.table
            .chunks_exact(self.stride)
            .enumerate()
            .filter(|(_, entry)| entry[..16].iter().any(|byte| *byte != 0))
            .map(move |(slot, entry)| {
                let start = u64le(entry, 32) * sector;
                let end = (u64le(entry, 40) + 1) * sector;
                (slot as u32 + 1, entry, start..end)
            })
    }
}
type Problem = (CheckState, String);
fn invalid(message: &str) -> Problem {
    (CheckState::Failed, message.into())
}
fn gpt<'a>(ends: &Ends<'a>, sector: u64, backup: bool) -> Result<Gpt<'a>, Problem> {
    let last_lba = ends.size / sector - 1;
    let own = if backup { last_lba } else { 1 };
    let alternate = if backup { 1 } else { last_lba };
    let h = ends
        .get(own * sector, sector)
        .ok_or_else(|| (CheckState::Unchecked, "头部未包含在读取范围内。".into()))?;
    if &h[..8] != b"EFI PART" {
        return Err(invalid("缺少 EFI PART 签名。"));
    }
    let length = u32le(h, 12) as usize;
    if !(92..=sector as usize).contains(&length) {
        return Err(invalid("GPT 头部长度越界。"));
    }
    let mut crc = crc32fast::Hasher::new();
    crc.update(&h[..16]);
    crc.update(&[0; 4]);
    crc.update(&h[20..length]);
    if crc.finalize() != u32le(h, 16) {
        return Err(invalid("头部 CRC32 不匹配。"));
    }
    if u32le(h, 8) != 0x10000 || length != 92 {
        return Err((CheckState::Unchecked, "GPT 版本或扩展头部暂不支持。".into()));
    }
    if u32le(h, 20) != 0 || h[length..].iter().any(|b| *b != 0) {
        return Err(invalid("GPT 头部保留区域不是零。"));
    }
    if u64le(h, 24) != own || u64le(h, 32) != alternate {
        return Err(invalid("主/备头部 LBA 指针与文件容量不符。"));
    }
    let first = u64le(h, 40);
    let last = u64le(h, 48);
    let at = u64le(h, 72);
    let count = u32le(h, 80) as u64;
    let stride = u32le(h, 84) as u64;
    if count == 0 || stride < 128 || !stride.is_power_of_two() {
        return Err(invalid("GPT 条目数量或长度无效。"));
    }
    let table_len = count
        .checked_mul(stride)
        .ok_or_else(|| invalid("条目表长度溢出。"))?;
    if count > 4096 || table_len > MIB {
        return Err((
            CheckState::Unchecked,
            "条目表超过 1 MiB 或 4096 条的检查上限。".into(),
        ));
    }
    let reserved = table_len.max(16384).div_ceil(sector);
    if first < 2 + reserved || first > last || last >= last_lba.saturating_sub(reserved) {
        return Err(invalid("可用 LBA 范围侵入分区表保留空间或倒置。"));
    }
    let table_end = at
        .checked_add(table_len.div_ceil(sector))
        .ok_or_else(|| invalid("条目表位置溢出。"))?;
    if (!backup && (at < 2 || table_end > first))
        || (backup && (at <= last || table_end > last_lba))
    {
        return Err(invalid("分区条目表位置侵入可用空间或 GPT 头部。"));
    }
    if h[56..72].iter().all(|b| *b == 0) {
        return Err(invalid("磁盘 GUID 为空。"));
    }
    let table = ends
        .get(
            at.checked_mul(sector)
                .ok_or_else(|| invalid("条目偏移溢出。"))?,
            table_len,
        )
        .ok_or_else(|| {
            (
                CheckState::Unchecked,
                "条目表位于首尾各 1 MiB 的读取范围之外。".into(),
            )
        })?;
    if crc32fast::hash(table) != u32le(h, 88) {
        return Err(invalid("分区条目表 CRC32 不匹配。"));
    }
    Ok(Gpt {
        first,
        last,
        guid: &h[56..72],
        count: count as usize,
        stride: stride as usize,
        table,
    })
}
fn entries(g: &Gpt<'_>, sector: u64) -> Result<(usize, usize), String> {
    let mut guids = HashSet::new();
    let mut ranges = Vec::new();
    let mut unaligned = 0;
    for (n, e) in g.table.chunks_exact(g.stride).enumerate() {
        if e[..16].iter().all(|b| *b == 0) {
            if e.iter().any(|b| *b != 0) {
                return Err(format!("第 {} 个未使用条目含非零数据。", n + 1));
            }
            continue;
        }
        if e[16..32].iter().all(|b| *b == 0) || !guids.insert(&e[16..32]) {
            return Err(format!("第 {} 个分区 GUID 为空或重复。", n + 1));
        }
        let start = u64le(e, 32);
        let last = u64le(e, 40);
        if start < g.first || last > g.last || start > last {
            return Err(format!("第 {} 个分区越过可用边界或长度无效。", n + 1));
        }
        if u64le(e, 48) & 0x0000_ffff_ffff_fff8 != 0 || e[128..].iter().any(|b| *b != 0) {
            return Err(format!("第 {} 个分区的保留属性或扩展字段不是零。", n + 1));
        }
        let name: Vec<_> = e[56..128]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|c| *c != 0)
            .collect();
        if String::from_utf16(&name).is_err() {
            return Err(format!("第 {} 个分区名称包含无效 UTF-16。", n + 1));
        }
        ranges.push((start, last, n + 1));
        unaligned += usize::from(start * sector % 4096 != 0);
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 <= pair[0].1 {
            return Err(format!("第 {}、{} 个分区重叠。", pair[0].2, pair[1].2));
        }
    }
    Ok((ranges.len(), unaligned))
}

/// A GPT copy whose header, array CRC, and entries all passed independent
/// checks. The other copy and the protective MBR are not used here.
pub struct CheckedGptCopy<'a> {
    pub header: &'a [u8],
    pub array: &'a [u8],
    pub array_offset: u64,
    pub entry_stride: usize,
    pub first_usable: u64,
    pub last_usable: u64,
}

pub fn check_gpt_copy<'a>(
    head: &'a [u8],
    tail: &'a [u8],
    size: u64,
    sector: u64,
    backup: bool,
) -> Result<CheckedGptCopy<'a>, (CheckState, String)> {
    if !matches!(sector, 512 | 4096)
        || !(4 * MIB..=1 << 43).contains(&size)
        || size % sector != 0
        || !(sector as usize..=MIB as usize).contains(&head.len())
        || !(sector as usize..=MIB as usize).contains(&tail.len())
    {
        return Err(invalid("GPT 副本读取几何无效。"));
    }
    let ends = Ends { head, tail, size };
    let checked = gpt(&ends, sector, backup)?;
    entries(&checked, sector).map_err(|reason| (CheckState::Failed, reason))?;
    let header_at = if backup { size - sector } else { sector };
    let header = ends.get(header_at, sector).ok_or((
        CheckState::Unchecked, "GPT 头部超出读取范围。".into(),
    ))?;
    Ok(CheckedGptCopy {
        header,
        array: checked.table,
        array_offset: u64le(header, 72) * sector,
        entry_stride: checked.stride,
        first_usable: checked.first,
        last_usable: checked.last,
    })
}

pub(crate) struct CheckedGpt {
    pub(crate) partitions: Vec<([u8; 16], Range<u64>)>,
    pub(crate) digest: [u8; 32],
}

/// Return typed byte ranges and the layout digest from independently validated GPT copies.
pub(crate) fn checked_gpt(
    head: &[u8],
    tail: &[u8],
    size: u64,
    sector: u64,
) -> Result<CheckedGpt, String> {
    let ends = Ends { head, tail, size };
    let primary = gpt(&ends, sector, false).map_err(|(_, reason)| reason)?;
    let backup = gpt(&ends, sector, true).map_err(|(_, reason)| reason)?;
    if !primary.agrees(&backup) {
        return Err("主备 GPT 的磁盘身份、范围或分区条目不一致。".into());
    }
    entries(&primary, sector)?;
    let partitions = primary
        .partitions(sector)
        .map(|(_, entry, range)| (entry[..16].try_into().unwrap(), range))
        .collect();
    let mut digest = Sha256::new();
    digest.update(size.to_le_bytes());
    digest.update(&head[..2 * sector as usize]);
    digest.update(&tail[tail.len() - sector as usize..]);
    digest.update(primary.table);
    Ok(CheckedGpt { partitions, digest: digest.finalize().into() })
}

/// Head and tail are bytes from the same file, capped separately at 1 MiB.
/// Invalid tables still produce a report; invalid input contracts are errors.
pub fn inspect_image(
    head: &[u8],
    tail: &[u8],
    size: u64,
    filename: &str,
) -> Result<ImageInspection, String> {
    inspect_image_with_ebrs(head, tail, size, filename, &[])
}

pub fn inspect_image_with_ebrs(
    head: &[u8],
    tail: &[u8],
    size: u64,
    filename: &str,
    ebrs: &[EbrSector],
) -> Result<ImageInspection, String> {
    inspect_image_with_geometry(head, tail, size, filename, None, ebrs)
}

/// An MBR has no stored logical sector size. A caller with device, container,
/// or user-selected geometry must pass it explicitly.
pub fn inspect_image_with_geometry(
    head: &[u8],
    tail: &[u8],
    size: u64,
    filename: &str,
    sector: Option<u64>,
    ebrs: &[EbrSector],
) -> Result<ImageInspection, String> {
    if !(4 * MIB..=1 << 43).contains(&size)
        || !(512..=MIB as usize).contains(&head.len())
        || !(512..=MIB as usize).contains(&tail.len())
        || filename.len() > 255
        || filename.contains(['/', '\\', ':', '\0'])
        || !crate::image::logical_image_name(filename)
    {
        return Err(
            "仅接受受支持的镜像文件名与逻辑磁盘首尾各不超过 1 MiB 的字节数据。"
                .into(),
        );
    }
    let mbr_sector = sector.unwrap_or(512);
    if !matches!(mbr_sector, 512 | 4096)
        || head.len() < mbr_sector as usize
        || tail.len() < mbr_sector as usize
    {
        return Err("逻辑扇区大小无效或首尾读取窗口不足一个完整扇区。".into());
    }
    mbr::validate_sectors(ebrs, size, mbr_sector)?;
    let ends = Ends { head, tail, size };
    for block in ebrs {
        if ends
            .get(block.offset, mbr_sector)
            .is_some_and(|old| old != block.bytes)
        {
            return Err("EBR 与首尾读取窗口中的相同位置内容不一致，请重新打开镜像。".into());
        }
    }
    let mut parsed = parse_image(head, size, filename);
    let mut mbr_container = None;
    let mut gpt_identity = None;
    let mut report = TableReport {
        filename: filename.into(),
        scheme: "未知".into(),
        sector_size: None,
        state: CheckState::Unchecked,
        items: Vec::new(),
    };
    let primary: Vec<u64> = [512, 4096]
        .into_iter()
        .filter(|s| ends.get(*s, 8) == Some(b"EFI PART"))
        .collect();
    let backup: Vec<u64> = [512, 4096]
        .into_iter()
        .filter(|s| size % s == 0 && ends.get(size - s, 8) == Some(b"EFI PART"))
        .collect();
    let protective = (0..4).any(|n| head[450 + n * 16] == 0xee);
    let candidates: HashSet<u64> = primary.iter().chain(&backup).copied().collect();
    if candidates.len() > 1 {
        report.add(
            "sector-ambiguity",
            "逻辑扇区大小",
            CheckState::Failed,
            "同时发现不同扇区大小的 GPT 签名，不能唯一确定布局。",
        );
    } else if !primary.is_empty() || protective && !backup.is_empty() {
        let sector = *candidates.iter().next().unwrap();
        report.scheme = "GPT".into();
        report.sector_size = Some(sector);
        if size % sector != 0 {
            report.add(
                "file-size",
                "镜像容量",
                CheckState::Failed,
                "文件长度不是逻辑扇区的整数倍。",
            );
        } else {
            let occupied: Vec<_> = (0..4)
                .map(|n| &head[446 + n * 16..462 + n * 16])
                .filter(|e| e.iter().any(|b| *b != 0))
                .collect();
            let pmbr_structure_ok = head[510..512] == [0x55, 0xaa]
                && occupied.len() == 1
                && occupied[0][0] == 0
                && occupied[0][4] == 0xee
                && u32le(occupied[0], 8) == 1;
            let expected = (size / sector - 1).min(u32::MAX as u64);
            let (pmbr_state, pmbr_detail) = if !pmbr_structure_ok {
                (
                    CheckState::Failed,
                    "签名或保护条目结构无效；要求唯一的非活动 0xEE 条目、起始 LBA 1，其他条目为空。".into(),
                )
            } else {
                let actual = u64::from(u32le(occupied[0], 12));
                if actual == expected {
                    (
                        CheckState::Passed,
                        "单个保护条目覆盖磁盘，签名和范围有效。".into(),
                    )
                } else if actual == u64::from(u32::MAX) {
                    // Windows-created GPT disks can saturate this field even below
                    // 2 TiB. Linux block/partitions/efi.c also accepts this encoding.
                    // Preserve the original bytes; this is not a repair or permission
                    // to weaken either GPT copy's independent integrity checks.
                    (CheckState::Warning, format!("保护条目长度为 0xFFFFFFFF，超过当前磁盘的 {expected} 个可保护扇区；按兼容写法保留原始字节，未修复或改写。"))
                } else {
                    (
                        CheckState::Failed,
                        format!("保护条目长度为 {actual} 个扇区，应为 {expected}；覆盖范围无效。"),
                    )
                }
            };
            report.add("protective-mbr", "保护性 MBR", pmbr_state, pmbr_detail);
            let p = gpt(&ends, sector, false);
            let b = gpt(&ends, sector, true);
            for (code, title, result) in [
                ("primary-gpt", "主 GPT 头部与条目表", &p),
                ("backup-gpt", "备份 GPT 头部与条目表", &b),
            ] {
                match result {
                    Ok(_) => report.add(
                        code,
                        title,
                        CheckState::Passed,
                        "位置、结构、头部 CRC32 和条目表 CRC32 通过。",
                    ),
                    Err((state, detail)) => report.add(code, title, state.clone(), detail.clone()),
                }
            }
            let mut copies_agree = false;
            if let (Ok(p), Ok(b)) = (&p, &b) {
                copies_agree = p.agrees(b);
                report.add(
                    "gpt-agreement",
                    "主备表一致性",
                    if copies_agree {
                        CheckState::Passed
                    } else {
                        CheckState::Failed
                    },
                    if copies_agree {
                        "磁盘 GUID、可用范围、条目数量及完整条目内容一致。"
                    } else {
                        "两份表各自可读但内容不一致，不能自动决定哪份较新。"
                    },
                );
            } else {
                report.add(
                    "gpt-agreement",
                    "主备表一致性",
                    CheckState::Unchecked,
                    "至少一份表未通过校验，无法比较；未执行修复。",
                );
            }
            for (code, title, copy) in [
                ("primary-layout", "主表分区布局", &p),
                ("backup-layout", "备份表分区布局", &b),
            ] {
                if let Ok(g) = copy {
                    match entries(g, sector) {
                        Ok((count, unaligned)) => report.add(
                            code,
                            title,
                            if unaligned == 0 {
                                CheckState::Passed
                            } else {
                                CheckState::Warning
                            },
                            format!(
                                "{count} 个分区：GUID 唯一、无重叠且均在可用范围内。{}",
                                if unaligned == 0 {
                                    "起点均按 4 KiB 对齐。".into()
                                } else {
                                    format!("{unaligned} 个起点未按 4 KiB 对齐（兼容性提示）。")
                                }
                            ),
                        ),
                        Err(e) => report.add(code, title, CheckState::Failed, e),
                    }
                }
            }
            if let Ok(primary) = p {
                let layout_checked = report.items.iter().all(|item| {
                    matches!(item.state, CheckState::Passed | CheckState::Warning)
                });
                if copies_agree && layout_checked {
                    gpt_identity = Some(GptIdentity {
                        disk_guid: primary.guid.try_into().unwrap(),
                        partitions: primary
                            .partitions(sector)
                            .map(|(number, entry, range)| GptPartitionIdentity {
                                number,
                                guid: entry[16..32].try_into().unwrap(),
                                kind: entry[..16].try_into().unwrap(),
                                attributes: u64le(entry, 48),
                                range,
                            })
                            .collect(),
                    });
                }
            }
        }
    } else {
        report.scheme = if protective { "GPT" } else { "MBR" }.into();
        if !backup.is_empty() {
            report.add(
                "stale-backup",
                "残留 GPT 备份",
                CheckState::Warning,
                "文件尾部有 GPT 签名，但主表和保护性 MBR 不匹配；不使用残留备份替代当前 MBR。",
            );
        }
        if protective {
            report.add(
                "gpt-missing",
                "GPT 头部",
                CheckState::Failed,
                "存在保护性 MBR，但首尾均未找到可确认的 GPT 头部。",
            );
        } else {
            report.sector_size = Some(mbr_sector);
            if sector.is_some() {
                report.add(
                    "mbr-sector",
                    "MBR 扇区大小",
                    CheckState::Passed,
                    "MBR 与 EBR 条目按提供的逻辑扇区大小解释。",
                );
            } else {
                report.add(
                    "mbr-sector",
                    "MBR 扇区大小",
                    CheckState::Warning,
                    "MBR 不记录扇区大小，当前按 512 字节解释；不能据此验证 4Kn 镜像。",
                );
            }
            let (state, detail) = match mbr::scan(head, size, mbr_sector, ebrs) {
                Ok(Scan::Complete(layout)) => {
                    if let Some((base, end)) = layout.extended {
                        mbr_container = Some((base, end - base));
                        report.add("ebr-chain", "扩展分区与逻辑分区", CheckState::Passed,
                            format!("容器偏移 {base} B，容量 {} B；{} 个 EBR、{} 个逻辑分区。链路无循环，数据与分区表不重叠。", end - base, layout.table_count(), layout.logical_count));
                    }
                    parsed = layout.into_disk(size, filename);
                    match &parsed {
                        Ok(_) => (
                            CheckState::Passed,
                            "签名、范围、重叠和启动标记通过；未检查文件系统或启动代码。".into(),
                        ),
                        Err(e) => (CheckState::Unchecked, e.clone()),
                    }
                }
                Ok(Scan::Need(offset)) => {
                    let message = format!("尚缺偏移 {offset} B 的 EBR 扇区，未导入部分布局。");
                    parsed = Err(message.clone());
                    (CheckState::Unchecked, message)
                }
                Err(e) => {
                    parsed = Err(e.detail.clone());
                    (
                        if e.incomplete {
                            CheckState::Unchecked
                        } else {
                            CheckState::Failed
                        },
                        e.detail,
                    )
                }
            };
            report.add("mbr-layout", "MBR 分区布局", state, detail);
        }
    }
    if report.scheme != "MBR" && !ebrs.is_empty() {
        return Err("非 MBR 镜像不接受 EBR 扇区。".into());
    }
    let (mut disk, mut parse_error) = match parsed {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(e)),
    };
    if sector
        .zip(report.sector_size)
        .is_some_and(|(provided, detected)| provided != detected)
    {
        report.add(
            "sector-mismatch",
            "逻辑扇区大小",
            CheckState::Failed,
            "提供的逻辑扇区大小与分区表不一致。",
        );
        disk = None;
        parse_error = Some("提供的逻辑扇区大小与分区表不一致。".into());
    }
    // A diagnostic report is useful even if there is no trustworthy primary
    // layout to display. Never silently promote a backup or a conflicting table.
    if report.items.iter().any(|i| {
        (matches!(
            i.code.as_str(),
            "sector-ambiguity" | "primary-layout" | "gpt-agreement" | "file-size"
        ) && i.state == CheckState::Failed)
            || i.code == "primary-gpt" && i.state != CheckState::Passed
    }) {
        disk = None;
        parse_error = Some("分区表存在冲突或无效布局，未导入部分结果。".into());
    }
    if let Some(disk) = &mut disk {
        // Both read windows belong to the opened snapshot. Different backup
        // contents or basenames must not overwrite another imported report.
        let mut ebr_crc = crc32fast::Hasher::new();
        for sector in ebrs {
            ebr_crc.update(&sector.offset.to_le_bytes());
            ebr_crc.update(&sector.bytes);
        }
        disk.id = format!(
            "image-{:x}-{:x}-{:x}-{:x}-{size}",
            crc32fast::hash(head),
            crc32fast::hash(tail),
            ebr_crc.finalize(),
            crc32fast::hash(filename.as_bytes())
        );
        if disk.scheme == "MBR" && disk.sector_size == 4096 {
            disk.id.push_str("-4096");
        }
    }
    report.finish();
    Ok(ImageInspection {
        mbr_container: disk.as_ref().and(mbr_container),
        gpt_identity: disk.as_ref().and(gpt_identity),
        disk,
        parse_error,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SIZE: usize = 8 * 1024 * 1024;
    fn header_crc(h: &mut [u8]) {
        h[16..20].fill(0);
        let n = u32le(h, 12) as usize;
        let crc = crc32fast::hash(&h[..n]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    fn fixture(sector: usize) -> Vec<u8> {
        let mut bytes = vec![0; SIZE];
        let last = (SIZE / sector - 1) as u64;
        bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        bytes[450] = 0xee;
        bytes[454..458].copy_from_slice(&1u32.to_le_bytes());
        bytes[458..462].copy_from_slice(&(last as u32).to_le_bytes());
        let mut table = vec![0u8; 16384];
        table[..16].fill(1);
        table[16..32].fill(2);
        table[32..40].copy_from_slice(&(MIB / sector as u64).to_le_bytes());
        table[40..48].copy_from_slice(&(3 * MIB / sector as u64 - 1).to_le_bytes());
        table[56..58].copy_from_slice(&('测' as u16).to_le_bytes());
        let crc = crc32fast::hash(&table);
        for (own, other, at) in [(1, last, 2), (last, 1, last - 16384 / sector as u64)] {
            bytes[at as usize * sector..at as usize * sector + table.len()].copy_from_slice(&table);
            let h = &mut bytes[own as usize * sector..(own as usize + 1) * sector];
            h[..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&own.to_le_bytes());
            h[32..40].copy_from_slice(&other.to_le_bytes());
            h[40..48].copy_from_slice(&(2 + 16384 / sector as u64).to_le_bytes());
            h[48..56].copy_from_slice(&(last - 16384 / sector as u64 - 1).to_le_bytes());
            h[56..72].fill(3);
            h[72..80].copy_from_slice(&at.to_le_bytes());
            h[80..84].copy_from_slice(&128u32.to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&crc.to_le_bytes());
            header_crc(h);
        }
        bytes
    }
    fn inspect(bytes: &[u8]) -> ImageInspection {
        inspect_image(
            &bytes[..MIB as usize],
            &bytes[bytes.len() - MIB as usize..],
            bytes.len() as u64,
            "test.img",
        )
        .unwrap()
    }
    fn state(i: &ImageInspection, code: &str) -> CheckState {
        i.report
            .items
            .iter()
            .find(|v| v.code == code)
            .unwrap()
            .state
            .clone()
    }
    fn table_mutate(bytes: &mut [u8], backup: bool, edit: impl FnOnce(&mut [u8])) {
        let h = if backup { SIZE - 512 } else { 512 };
        let at = u64le(bytes, h + 72) as usize * 512;
        edit(&mut bytes[at..at + 16384]);
        let crc = crc32fast::hash(&bytes[at..at + 16384]);
        bytes[h + 88..h + 92].copy_from_slice(&crc.to_le_bytes());
        header_crc(&mut bytes[h..h + 512]);
    }
    #[test]
    fn gpt_identity_preserves_empty_slots_before_a_partition() {
        let mut bytes = fixture(512);
        for backup in [false, true] {
            table_mutate(&mut bytes, backup, |table| {
                table.copy_within(..128, 15 * 128);
                table[..128].fill(0);
            });
        }
        let identity = inspect(&bytes).gpt_identity.unwrap();
        assert_eq!(identity.partitions[0].number, 16);
        assert_eq!(identity.partitions[0].range, MIB..3 * MIB);
    }
    #[test]
    fn independently_checks_both_copies_on_512_and_4k_images() {
        for sector in [512, 4096] {
            let bytes = fixture(sector);
            let original = bytes.clone();
            let i = inspect(&bytes);
            assert_eq!(i.report.state, CheckState::Passed);
            assert_eq!(i.report.sector_size, Some(sector as u64));
            assert_eq!(state(&i, "gpt-agreement"), CheckState::Passed);
            let identity = i.gpt_identity.unwrap();
            assert_eq!(identity.disk_guid, [3; 16]);
            assert_eq!(identity.partitions.len(), 1);
            assert_eq!(identity.partitions[0].guid, [2; 16]);
            assert_eq!(identity.partitions[0].kind, [1; 16]);
            assert_eq!(identity.partitions[0].range, MIB..3 * MIB);
            assert_eq!(
                i.disk
                    .unwrap()
                    .partitions
                    .iter()
                    .filter(|p| p.fs != "—")
                    .count(),
                1
            );
            assert_eq!(bytes, original);
        }
    }
    #[test]
    fn decoded_vhd_uses_logical_capacity_and_preserves_source_name() {
        let bytes = fixture(512);
        for extension in ["vhd", "avhd"] {
            let result = inspect_image(
                &bytes[..MIB as usize],
                &bytes[SIZE - MIB as usize..],
                SIZE as u64,
                &format!("离线磁盘.{extension}"),
            )
            .unwrap();
            assert_eq!(result.report.state, CheckState::Passed);
            assert_eq!(result.report.filename, format!("离线磁盘.{extension}"));
            let disk = result.disk.unwrap();
            assert_eq!(disk.filename, format!("离线磁盘.{extension}"));
            assert_eq!(disk.name, "离线磁盘");
            assert_eq!(disk.size, SIZE as u64);
            assert_eq!(disk.kind, "image");
            assert!(disk
                .partitions
                .iter()
                .filter(|p| p.fs != "—")
                .all(|p| p.used.is_none()));
            assert!(inspect_image(
                &bytes[..MIB as usize],
                &bytes[SIZE - MIB as usize..],
                SIZE as u64,
                "unsupported.iso"
            )
            .is_err());
            let four_k = fixture(4096);
            let rejected = inspect_image(
                &four_k[..MIB as usize],
                &four_k[SIZE - MIB as usize..],
                SIZE as u64,
                &format!("invalid-sector.{extension}"),
            )
            .unwrap();
            assert!(rejected.disk.is_none());
            assert!(rejected.parse_error.unwrap().contains("512 B"));
        }
    }
    #[test]
    fn corrupt_primary_still_reports_valid_backup_without_using_it_as_primary() {
        let mut bytes = fixture(512);
        bytes[528] ^= 1;
        let i = inspect(&bytes);
        assert_eq!(state(&i, "primary-gpt"), CheckState::Failed);
        assert_eq!(state(&i, "backup-gpt"), CheckState::Passed);
        assert!(i.disk.is_none());
    }
    #[test]
    fn decoded_vhdx_keeps_unicode_basename_and_checks_512_and_4k_gpt() {
        for sector in [512, 4096] {
            let bytes = fixture(sector);
            for filename in ["备份.磁盘.vhdx", "备份.磁盘.AVHDX"] {
                let inspected = inspect_image_with_geometry(
                    &bytes[..MIB as usize],
                    &bytes[SIZE - MIB as usize..],
                    SIZE as u64,
                    filename,
                    Some(sector as u64),
                    &[],
                )
                .unwrap();
                assert_eq!(inspected.report.state, CheckState::Passed);
                let disk = inspected.disk.unwrap();
                assert_eq!(disk.name, "备份.磁盘");
                assert_eq!(disk.filename, filename);
                assert_eq!(disk.sector_size, sector as u64);
                assert_eq!(disk.size, SIZE as u64);
                assert_eq!(disk.kind, "image");
            }
        }
    }
    #[test]
    fn decoded_512_byte_containers_keep_filename_and_reject_4kn_gpt() {
        for filename in [
            "备份.磁盘.VDI",
            "备份.磁盘.VMDK",
            "备份.磁盘.HDD",
            "备份.磁盘.HDS",
            "DiskDescriptor.xml",
        ] {
            for sector in [512, 4096] {
                let bytes = fixture(sector);
                let inspected = inspect_image(
                    &bytes[..MIB as usize],
                    &bytes[SIZE - MIB as usize..],
                    SIZE as u64,
                    filename,
                )
                .unwrap();
                if sector == 4096 {
                    assert!(inspected.disk.is_none());
                    assert!(inspected.parse_error.unwrap().contains("512 B"));
                } else {
                    assert_eq!(inspected.report.state, CheckState::Passed);
                    let disk = inspected.disk.unwrap();
                    assert_eq!(disk.name, filename.rsplit_once('.').unwrap().0);
                    assert_eq!(disk.filename, filename);
                    assert_eq!(disk.size, SIZE as u64);
                    assert_eq!(disk.kind, "image");
                    assert_eq!(disk.sector_size, 512);
                }
            }
        }
    }
    #[test]
    fn reports_bad_backup_and_keeps_read_only_primary_layout() {
        let mut bytes = fixture(512);
        bytes[SIZE - 512] = 0;
        let i = inspect(&bytes);
        assert_eq!(i.report.state, CheckState::Failed);
        assert_eq!(state(&i, "backup-gpt"), CheckState::Failed);
        assert!(i.disk.is_some());
    }
    #[test]
    fn catches_crc_valid_but_different_tables() {
        let mut bytes = fixture(512);
        table_mutate(&mut bytes, true, |t| {
            t[56..58].copy_from_slice(&('异' as u16).to_le_bytes())
        });
        let i = inspect(&bytes);
        assert_eq!(state(&i, "backup-gpt"), CheckState::Passed);
        assert_eq!(state(&i, "gpt-agreement"), CheckState::Failed);
        assert!(i.gpt_identity.is_none());
        assert!(i.disk.is_none());
    }
    #[test]
    fn validates_duplicate_guids_overlaps_and_out_of_range_even_with_correct_crcs() {
        for mode in 0..3 {
            let mut bytes = fixture(512);
            for backup in [false, true] {
                table_mutate(&mut bytes, backup, |t| {
                    let first = t[..128].to_vec();
                    t[128..256].copy_from_slice(&first);
                    if mode != 0 {
                        t[144..160].fill(4);
                    }
                    if mode == 0 {
                        t[160..168].copy_from_slice(&8192u64.to_le_bytes());
                        t[168..176].copy_from_slice(&10239u64.to_le_bytes());
                    }
                    if mode == 2 {
                        t[168..176].copy_from_slice(&u64::MAX.to_le_bytes());
                    }
                });
            }
            let i = inspect(&bytes);
            assert_eq!(state(&i, "primary-gpt"), CheckState::Passed);
            assert_eq!(state(&i, "primary-layout"), CheckState::Failed);
            assert!(i.disk.is_none());
        }
    }
    #[test]
    fn validates_pointers_reserved_fields_disk_guid_and_minimum_reserved_space() {
        for field in [20, 24, 32, 40, 48, 56, 72] {
            let mut bytes = fixture(512);
            let h = &mut bytes[512..1024];
            match field {
                20 => h[20] = 1,
                56 => h[56..72].fill(0),
                48 => h[48..56].copy_from_slice(&(SIZE as u64 / 512 - 2).to_le_bytes()),
                _ => h[field..field + 8].fill(0),
            }
            header_crc(h);
            let i = inspect(&bytes);
            assert_eq!(
                state(&i, "primary-gpt"),
                CheckState::Failed,
                "field {field}"
            );
            assert!(i.disk.is_none());
        }
    }
    #[test]
    fn rejects_hybrid_mbr_and_wrong_coverage() {
        for hybrid in [false, true] {
            let mut bytes = fixture(512);
            if hybrid {
                bytes[466] = 7;
            } else {
                bytes[458] = 0;
            }
            assert_eq!(
                state(&inspect(&bytes), "protective-mbr"),
                CheckState::Failed
            );
        }
    }
    #[test]
    fn saturated_protective_length_is_a_warning_without_relaxing_structure_or_gpt() {
        for sector in [512, 4096] {
            let mut bytes = fixture(sector);
            bytes[458..462].copy_from_slice(&u32::MAX.to_le_bytes());
            let original = bytes.clone();
            let i = inspect(&bytes);
            assert_eq!(state(&i, "protective-mbr"), CheckState::Warning);
            assert_eq!(i.report.state, CheckState::Warning);
            assert!(i.disk.is_some());
            assert_eq!(bytes, original);
            for (at, value) in [(446, 0x80), (450, 7), (454, 2), (466, 7), (510, 0)] {
                let mut bad = bytes.clone();
                bad[at] = value;
                assert_eq!(state(&inspect(&bad), "protective-mbr"), CheckState::Failed);
            }
            let expected = SIZE / sector - 1;
            for count in [0, 1, expected as u32 - 1, expected as u32 + 1, u32::MAX - 1] {
                let mut bad = bytes.clone();
                bad[458..462].copy_from_slice(&count.to_le_bytes());
                assert_eq!(state(&inspect(&bad), "protective-mbr"), CheckState::Failed);
            }
            bytes[sector + 16] ^= 1;
            assert_eq!(inspect(&bytes).report.state, CheckState::Failed);
        }
    }
    #[test]
    fn mbr_does_not_infer_filesystem_and_does_not_trust_stale_backup() {
        let mut bytes = fixture(512);
        bytes[512..1024].fill(0);
        bytes[450] = 0x0c;
        bytes[454..458].copy_from_slice(&2048u32.to_le_bytes());
        bytes[458..462].copy_from_slice(&4096u32.to_le_bytes());
        let i = inspect(&bytes);
        assert_eq!(i.report.scheme, "MBR");
        assert_eq!(state(&i, "stale-backup"), CheckState::Warning);
        assert_eq!(i.disk.unwrap().partitions[1].fs, "未知");
    }
    #[test]
    fn does_not_pass_incomplete_ebr_chain() {
        let mut bytes = fixture(512);
        bytes[512..1024].fill(0);
        bytes[SIZE - 512..].fill(0);
        bytes[450] = 0x0f;
        let i = inspect(&bytes);
        assert_eq!(i.report.state, CheckState::Unchecked);
        assert!(i.disk.is_none());
    }
    #[test]
    fn mbr_invalid_boot_flags_and_nonempty_unused_slots_fail_layout_check() {
        for bad_boot in [false, true] {
            let mut bytes = fixture(512);
            bytes[512..1024].fill(0);
            bytes[SIZE - 512..].fill(0);
            bytes[450] = 7;
            bytes[454..458].copy_from_slice(&2048u32.to_le_bytes());
            bytes[458..462].copy_from_slice(&4096u32.to_le_bytes());
            if bad_boot {
                bytes[446] = 0x81;
            } else {
                bytes[462 + 8] = 1;
            }
            assert_eq!(state(&inspect(&bytes), "mbr-layout"), CheckState::Failed);
        }
    }
    #[test]
    fn bounded_truncated_and_arbitrary_buffers_never_panic() {
        let bytes = fixture(4096);
        let i = inspect_image(
            &bytes[..4096],
            &bytes[SIZE - 4096..],
            SIZE as u64,
            "partial.img",
        )
        .unwrap();
        assert!(i.disk.is_none());
        assert_ne!(i.report.state, CheckState::Passed);
        for n in [0, 1, 511, 512, 4096, 8192, MIB as usize, MIB as usize + 1] {
            let b = vec![255; n];
            let _ = inspect_image(&b, &b, SIZE as u64, "random.img");
        }
        assert!(inspect_image(
            &bytes[..512],
            &bytes[SIZE - 512..],
            SIZE as u64,
            "C:\\bad.img"
        )
        .is_err());
    }
    #[test]
    fn rejects_misaligned_size_and_ambiguous_sector_signatures() {
        let mut bytes = fixture(512);
        bytes.push(0);
        assert!(inspect(&bytes).disk.is_none());
        bytes.pop();
        bytes[4096..4104].copy_from_slice(b"EFI PART");
        assert_eq!(
            state(&inspect(&bytes), "sector-ambiguity"),
            CheckState::Failed
        );
    }
    #[test]
    fn backup_changes_and_different_filenames_do_not_reuse_an_open_image_id() {
        let mut bytes = fixture(512);
        let old = inspect(&bytes).disk.unwrap().id;
        bytes[SIZE - 512 + 16] ^= 1;
        let damaged = inspect(&bytes).disk.unwrap().id;
        assert_ne!(old, damaged);
        let renamed = inspect_image(
            &bytes[..MIB as usize],
            &bytes[SIZE - MIB as usize..],
            SIZE as u64,
            "another.img",
        )
        .unwrap()
        .disk
        .unwrap()
        .id;
        assert_ne!(damaged, renamed);
    }
}

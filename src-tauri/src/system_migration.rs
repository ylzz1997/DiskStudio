//! Logical target for Windows migration. All I/O is through bounded streams.
use crate::{
    backup_archive::Source,
    boot_inspection::virtualization::{self as boot, SystemHive, WindowsBoot},
    image_table_conversion::{Overlay, Patch},
    system_virtualization::Firmware,
    table_archive::{self, Archive, Kind},
    table_restore,
    used_space::Span,
};
use disk_core::{table_check::GptIdentity, MIB};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Seek, SeekFrom},
};
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    windows: WindowsBoot,
    before: Archive,
    after: Archive,
    patches: Vec<Patch>,
    adds_efi_fallback: bool,
    partitions: usize,
    system_digest: String,
    system_hive: SystemHive,
    changes: Vec<VolumeChange>,
    mappings: Vec<Mapping>,
    selection: Option<SelectionPreview>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum PartitionSelection {
    #[default]
    All,
    Selected {
        partitions: Vec<u32>,
    },
}

impl PartitionSelection {
    pub(crate) fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Self::Selected { partitions } = self {
            if partitions.len() > 128
                || partitions.contains(&0)
                || partitions.iter().collect::<BTreeSet<_>>().len() != partitions.len()
            {
                return Err("迁移分区选择包含无效或重复的分区。".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub(crate) struct Options {
    pub(crate) sizes: Vec<PartitionSize>,
    pub(crate) selection: PartitionSelection,
}

impl Options {
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_sizes(&self.sizes)?;
        self.selection.validate()
    }
}

pub(crate) struct PartitionNumber {
    pub(crate) number: u32,
    pub(crate) slot: u32,
    pub(crate) can_resize: bool,
    pub(crate) can_omit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectionPreview {
    copied: Vec<PlannedPartition>,
    skipped: Vec<PlannedPartition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedPartition {
    partition: u32,
    offset: u64,
    bytes: u64,
    required: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PartitionSize {
    pub(crate) partition: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) start_bytes: Option<u64>,
}

pub(crate) fn validate_sizes(sizes: &[PartitionSize]) -> Result<(), String> {
    let mut slots = BTreeSet::new();
    if sizes.len() > 128
        || sizes.iter().any(|p| {
            p.partition == 0
                || !slots.insert(p.partition)
                || (p.bytes.is_none() && p.start_bytes.is_none())
                || p.bytes
                    .is_some_and(|n| !(16 * MIB..=1 << 43).contains(&n) || n % MIB != 0)
                || p.start_bytes
                    .is_some_and(|n| !(MIB..1 << 43).contains(&n) || n % MIB != 0)
        })
    {
        return Err("请选择互不重复的迁移分区，并填写有效的 MiB 起点或容量。".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VolumeChange {
    pub(crate) partition: u32,
    #[serde(skip)]
    slot: u32,
    pub(crate) offset: u64,
    pub(crate) new_offset: u64,
    pub(crate) old_bytes: u64,
    pub(crate) new_bytes: u64,
    pub(crate) serial: u64,
    pub(crate) cluster: u64,
    #[serde(skip)]
    fs_bytes: u64,
    #[serde(skip)]
    boot_move: Option<BootMove>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BootMove {
    backup: u64,
    before: [u8; 4],
    after: [u8; 4],
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    firmware: Firmware,
    source_bytes: u64,
    target_bytes: u64,
    sector: u64,
    boot_offset: u64,
    windows_offset: u64,
    partitions: usize,
    relocated_gpt: bool,
    adds_efi_fallback: bool,
    partition_changes: Vec<VolumeChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partition_selection: Option<SelectionPreview>,
}

fn target_table(before: &Archive, size: u64, firmware: Firmware) -> Result<Archive, String> {
    changed_table(before, size, firmware, &[], &[])
}

fn changed_table(
    before: &Archive,
    size: u64,
    firmware: Firmware,
    changes: &[VolumeChange],
    omitted: &[u32],
) -> Result<Archive, String> {
    let sector = before.sector;
    if !(4 * MIB..=1 << 43).contains(&size) || size % sector != 0 {
        return Err("系统迁移目标容量或逻辑扇区大小无效。".into());
    }
    if firmware == Firmware::Bios {
        if !changes.is_empty() || !omitted.is_empty() {
            return Err("选择迁移分区与调整 NTFS 布局需要 GPT/UEFI。".into());
        }
        if sector != 512 {
            return Err("BIOS/MBR 系统迁移需要 512 字节逻辑扇区。".into());
        }
        if size < before.size {
            return Err("BIOS/MBR 系统迁移要求目标盘至少与源盘等大。".into());
        }
        let mut after = before.clone();
        after.size = size;
        after.validate()?;
        return Ok(after);
    }
    let layout = table_restore::archive_layout(before)?;
    let mut after = before.clone();
    let [mbr, primary, array, backup_array, backup] = after.extents.as_mut_slice() else {
        return Err("缺少完整的源 GPT 分区表。".into());
    };
    if [
        &mbr.kind,
        &primary.kind,
        &array.kind,
        &backup_array.kind,
        &backup.kind,
    ] != [
        &Kind::Mbr,
        &Kind::PrimaryHeader,
        &Kind::PrimaryEntries,
        &Kind::BackupEntries,
        &Kind::BackupHeader,
    ] || mbr.bytes[450] != 0xee
        || mbr.bytes[462..510].iter().any(|&byte| byte != 0)
    {
        return Err("系统迁移需要完整的非混合 GPT 分区表。".into());
    }
    let edits = !changes.is_empty() || !omitted.is_empty();
    if size == before.size && !edits {
        return Ok(after);
    }
    if edits && u32::from_le_bytes(primary.bytes[84..88].try_into().unwrap()) != 128 {
        return Err("调整迁移分区需要标准 GPT 分区条目。".into());
    }
    let mut expected = layout.entries.clone();
    for &slot in omitted {
        let index = expected
            .iter()
            .position(|entry| entry.slot == slot)
            .ok_or("迁移分区的原生 GPT 标识缺失。")?;
        expected.remove(index);
        let start = (slot as usize - 1) * 128;
        for entries in [&mut array.bytes, &mut backup_array.bytes] {
            entries
                .get_mut(start..start + 128)
                .ok_or("GPT 分区条目缺失。")?
                .fill(0);
        }
    }
    for change in changes {
        let entry = expected
            .iter_mut()
            .find(|entry| entry.slot == change.slot)
            .ok_or("迁移调容分区已不存在。")?;
        if entry.offset != change.offset
            || entry.size != change.old_bytes
            || entry.kind != crate::partitions::policy::BASIC_DATA
            || entry.attributes != 0
        {
            return Err("迁移调容分区的范围、类型或保护状态已变化。".into());
        }
        let slot = usize::try_from(entry.slot - 1).map_err(|e| e.to_string())?;
        let range = slot * 128 + 32..slot * 128 + 48;
        let start = change.new_offset / sector;
        let end = change
            .new_offset
            .checked_add(change.new_bytes)
            .ok_or("目标分区越界。")?
            / sector
            - 1;
        for entries in [&mut array.bytes, &mut backup_array.bytes] {
            let extent = entries.get_mut(range.clone()).ok_or("GPT 分区条目缺失。")?;
            extent[..8].copy_from_slice(&start.to_le_bytes());
            extent[8..].copy_from_slice(&end.to_le_bytes());
        }
        entry.offset = change.new_offset;
        entry.size = change.new_bytes;
    }
    let last = size / sector - 1;
    let (backup_lba, usable) = if size == before.size {
        (backup_array.offset / sector, layout.last)
    } else {
        let array = last
            .checked_sub(backup_array.bytes.len() as u64 / sector)
            .ok_or("目标磁盘无法容纳备份 GPT。")?;
        let usable = array
            .checked_sub(1)
            .filter(|&end| end >= layout.first)
            .ok_or("目标磁盘无法容纳 GPT 可用范围。")?;
        (array, usable)
    };
    let mut intervals: Vec<_> = expected
        .iter()
        .map(|entry| (entry.offset, entry.offset + entry.size))
        .collect();
    intervals.sort_unstable();
    if intervals
        .iter()
        .any(|&(start, end)| start < layout.first * sector || end > (usable + 1) * sector)
    {
        return Err("至少一个分区越过目标磁盘的 GPT 可用边界。".into());
    }
    if intervals.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("调整后的目标分区范围重叠。".into());
    }
    // Preserve disk and partition identifiers, slot numbers, names, and boot code.
    mbr.bytes[458..462].copy_from_slice(&(last.min(u32::MAX as u64) as u32).to_le_bytes());
    for (header, current, peer) in [(&mut primary.bytes, 1, last), (&mut backup.bytes, last, 1)] {
        if header[12..16] != 92u32.to_le_bytes() {
            return Err("改变系统迁移目标容量需要标准 GPT 头。".into());
        }
        header[24..32].copy_from_slice(&current.to_le_bytes());
        header[32..40].copy_from_slice(&peer.to_le_bytes());
        header[48..56].copy_from_slice(&usable.to_le_bytes());
        if edits {
            let bytes = if current == 1 {
                &array.bytes
            } else {
                &backup_array.bytes
            };
            let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
            let payload = bytes.get(..count * 128).ok_or("GPT 分区条目长度无效。")?;
            header[88..92].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        }
        if current != 1 {
            header[72..80].copy_from_slice(&backup_lba.to_le_bytes());
        }
        header[16..20].fill(0);
        let crc = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    after.size = size;
    backup_array.offset = backup_lba * sector;
    backup.offset = last * sector;
    let output = table_restore::archive_layout(&after)?;
    if output.guid != layout.guid || output.entries != expected || output.first != layout.first {
        return Err("目标 GPT 未保留迁移前的磁盘和分区身份。".into());
    }
    Ok(after)
}

fn table_patches(
    reader: &mut impl Source,
    before: &Archive,
    after: &Archive,
    changes: &[VolumeChange],
    mappings: &[Mapping],
) -> Result<Vec<Patch>, String> {
    if after == before || after.extents == before.extents {
        return Ok(Vec::new());
    }
    let sector = after.sector;
    let mut sectors = BTreeMap::new();
    for extent in &before.extents {
        if matches!(extent.kind, Kind::BackupEntries | Kind::BackupHeader) {
            for i in 0..(extent.bytes.len() as u64 / sector) {
                let offset = extent.offset + i * sector;
                if offset < after.size && !changes.iter().any(|part| part.target_contains(offset)) {
                    sectors.insert(offset, vec![0; sector as usize]);
                }
            }
        }
    }
    for extent in &after.extents {
        for (i, bytes) in extent.bytes.chunks_exact(sector as usize).enumerate() {
            sectors.insert(extent.offset + i as u64 * sector, bytes.to_vec());
        }
    }
    let mut padded = Padded::new(reader, before.size, after.size, mappings);
    let mut patches = Vec::new();
    for (offset, after_bytes) in sectors {
        let original = table_archive::read_at(&mut padded, offset, sector as usize, after.size)?;
        if original != after_bytes {
            patches.push(Patch {
                offset,
                before: original,
                after: after_bytes,
            });
        }
    }
    Ok(patches)
}

fn volume_changes(
    source: &mut impl Source,
    table: &Archive,
    layout: &table_restore::Layout,
    numbers: &[PartitionNumber],
    sizes: &[PartitionSize],
    omitted: &[u32],
) -> Result<Vec<VolumeChange>, String> {
    let mut changes = Vec::with_capacity(sizes.len());
    for requested in sizes {
        let number = numbers
            .iter()
            .find(|row| row.number == requested.partition)
            .ok_or("迁移分区的 Windows 编号缺失。")?;
        let slot = number.slot;
        if omitted.contains(&slot) {
            return Err("请先将要调整起点或容量的分区加入迁移。".into());
        }
        let entry = layout
            .entries
            .iter()
            .find(|entry| entry.slot == slot)
            .ok_or("所选迁移分区已不存在。")?;
        if !number.can_resize
            || entry.kind != crate::partitions::policy::BASIC_DATA
            || entry.attributes != 0
        {
            return Err("请选择无保护的 GPT NTFS 分区。".into());
        }
        let new_offset = requested.start_bytes.unwrap_or(entry.offset);
        let new_bytes = requested.bytes.unwrap_or(entry.size);
        let resizes = new_bytes != entry.size;
        if new_offset == entry.offset && !resizes {
            continue;
        }
        let mut volume =
            crate::used_space::View::bounded(&mut *source, entry.offset, entry.size, table.size)
                .map_err(|e| e.to_string())?;
        let ntfs = ntfs::Ntfs::new(&mut volume).map_err(|e| e.to_string())?;
        crate::ntfs_allocation::require_clean(&ntfs, &mut volume)?;
        let cluster = u64::from(ntfs.cluster_size());
        if u64::from(ntfs.sector_size()) != table.sector
            || ntfs.size() + table.sector > entry.size
            || !cluster.is_power_of_two()
            || !(table.sector..=65536).contains(&cluster)
            || resizes && (new_bytes - table.sector) / cluster * cluster == ntfs.size()
        {
            return Err("源 NTFS 几何与分区范围不一致，或请求未改变 NTFS 容量。".into());
        }
        let boot_move = if new_offset == entry.offset {
            None
        } else {
            Some(BootMove::prepare(
                &mut volume,
                entry.size,
                new_offset,
                table.sector,
            )?)
        };
        changes.push(VolumeChange {
            partition: requested.partition,
            slot,
            offset: entry.offset,
            new_offset,
            serial: ntfs.serial_number(),
            old_bytes: entry.size,
            new_bytes,
            cluster,
            fs_bytes: ntfs.size(),
            boot_move,
        });
    }
    changes.sort_by_key(|change| change.new_offset);
    Ok(changes)
}

fn selected_partitions(
    source: &mut impl Source,
    windows: &WindowsBoot,
    layout: &table_restore::Layout,
    numbers: &[PartitionNumber],
    selection: &PartitionSelection,
    sector: u64,
) -> Result<(Vec<u32>, Option<SelectionPreview>), String> {
    let PartitionSelection::Selected { partitions } = selection else {
        return Ok((Vec::new(), None));
    };
    if partitions
        .iter()
        .any(|number| !numbers.iter().any(|row| row.number == *number))
    {
        return Err("迁移分区的 Windows 编号缺失。".into());
    }
    let parse = |id: &str| Uuid::parse_str(id).map_err(|error| error.to_string());
    let dependencies = windows.gpt_dependencies(source, sector, parse(&layout.guid)?)?;
    let mut found = BTreeSet::new();
    let mut omitted = Vec::new();
    let mut copied = Vec::new();
    let mut skipped = Vec::new();
    for entry in &layout.entries {
        let row = numbers
            .iter()
            .find(|row| row.slot == entry.slot)
            .ok_or("所选迁移分区与 GPT 槽位无法唯一对应。")?;
        let id = parse(&entry.guid)?;
        let boot_dependency = dependencies.contains(&id);
        if boot_dependency {
            found.insert(id);
        }
        let required = boot_dependency
            || entry.offset == windows.boot_offset()
            || entry.offset == windows.windows_offset()
            || !table_restore::ordinary(entry)
            || !row.can_omit;
        let part = PlannedPartition {
            partition: row.number,
            offset: entry.offset,
            bytes: entry.size,
            required,
        };
        if required || partitions.contains(&row.number) {
            copied.push(part);
        } else {
            omitted.push(entry.slot);
            skipped.push(part);
        }
    }
    if found != dependencies {
        return Err("Windows BCD 引用了源 GPT 上缺失的分区。".into());
    }
    copied.sort_by_key(|part| part.partition);
    skipped.sort_by_key(|part| part.partition);
    Ok((omitted, Some(SelectionPreview { copied, skipped })))
}

impl BootMove {
    fn prepare(
        volume: &mut impl Source,
        length: u64,
        start: u64,
        sector: u64,
    ) -> Result<Self, String> {
        let primary = table_archive::read_at(volume, 0, sector as usize, length)?;
        let tail = length - sector;
        let mut backup = tail;
        if primary[..512] != table_archive::read_at(volume, tail, sector as usize, length)?[..512] {
            backup = crate::recovery::ntfs_reconstruct::backup_extent(&primary, sector)
                .filter(|(at, bytes)| *at != tail && *bytes <= length)
                .map(|(at, _)| at)
                .ok_or("迁移移动需要一致的 NTFS 主备引导扇区。")?;
            if primary[..512]
                != table_archive::read_at(volume, backup, sector as usize, length)?[..512]
            {
                return Err("迁移移动需要一致的 NTFS 主备引导扇区。".into());
            }
        }
        let hidden =
            u32::try_from(start / sector).map_err(|_| "目标起点超出 NTFS 引导扇区的记录范围。")?;
        Ok(Self {
            backup,
            before: primary[28..32].try_into().unwrap(),
            after: hidden.to_le_bytes(),
        })
    }
}

impl VolumeChange {
    pub(crate) fn resizes(&self) -> bool {
        self.old_bytes != self.new_bytes
    }

    fn target_contains(&self, at: u64) -> bool {
        (self.new_offset..self.new_offset + self.new_bytes).contains(&at)
    }

    fn boot_patches(&self, sector: u64) -> impl Iterator<Item = Patch> + '_ {
        self.boot_move.iter().flat_map(move |boot| {
            [0, boot.backup]
                .into_iter()
                .filter(move |&at| at + sector <= self.new_bytes && boot.before != boot.after)
                .map(move |at| Patch {
                    offset: self.new_offset + at + 28,
                    before: boot.before.to_vec(),
                    after: boot.after.to_vec(),
                })
        })
    }

    fn verify(&self, reader: &mut impl Source, sector: u64) -> Result<(), String> {
        let end = self.new_offset + self.new_bytes;
        let backup = if self.resizes() {
            end - sector
        } else {
            self.new_offset
                + self
                    .boot_move
                    .as_ref()
                    .ok_or("缺少 NTFS 移动凭据。")?
                    .backup
        };
        let primary = table_archive::read_at(reader, self.new_offset, sector as usize, end)?;
        let mirror = table_archive::read_at(reader, backup, sector as usize, end)?;
        let boot_length = if self.resizes() { sector as usize } else { 512 };
        if primary[..boot_length] != mirror[..boot_length]
            || self
                .boot_move
                .as_ref()
                .is_some_and(|boot| primary[28..32] != boot.after)
        {
            return Err("迁移后的 NTFS 主备引导扇区不一致或起点错误。".into());
        }
        let mut volume =
            crate::used_space::View::bounded(reader, self.new_offset, self.new_bytes, end)
                .map_err(|e| e.to_string())?;
        let ntfs = ntfs::Ntfs::new(&mut volume).map_err(|e| e.to_string())?;
        let fs_bytes = if self.resizes() {
            (self.new_bytes - sector) / self.cluster * self.cluster
        } else {
            self.fs_bytes
        };
        if ntfs.serial_number() != self.serial
            || u64::from(ntfs.cluster_size()) != self.cluster
            || u64::from(ntfs.sector_size()) != sector
            || ntfs.size() != fs_bytes
        {
            return Err("迁移后的 NTFS 标识或几何与预览不同。".into());
        }
        if self.resizes() {
            crate::ntfs_allocation::require_resize_result(&ntfs, &mut volume)
        } else {
            crate::ntfs_allocation::require_clean(&ntfs, &mut volume)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Mapping {
    start: u64,
    end: u64,
    source: Option<u64>,
}

fn stream_mappings(
    changes: &[VolumeChange],
    usable_end: u64,
    selected: Option<(&table_restore::Layout, u64)>,
) -> Vec<Mapping> {
    let mut bounds = BTreeSet::new();
    for part in changes {
        bounds.extend([
            part.offset.min(usable_end),
            (part.offset + part.old_bytes).min(usable_end),
            part.new_offset,
            part.new_offset + part.old_bytes.min(part.new_bytes),
            part.new_offset + part.new_bytes,
        ]);
    }
    if let Some((layout, sector)) = selected {
        bounds.extend([layout.first * sector, usable_end]);
        for entry in &layout.entries {
            bounds.extend([entry.offset, entry.offset + entry.size]);
        }
    }
    let ends: Vec<_> = bounds.into_iter().collect();
    // Every target interval is assigned from the immutable source. This works
    // even when a new partition overlaps its old position or two trade places.
    ends.windows(2)
        .filter_map(|p| {
            let [start, end] = [p[0], p[1]];
            if let Some(part) = changes.iter().find(|part| part.target_contains(start)) {
                let at = start - part.new_offset;
                let source = (at < part.old_bytes).then_some(part.offset + at);
                (source != Some(start)).then_some(Mapping { start, end, source })
            } else {
                let clear = if let Some((layout, sector)) = selected {
                    (layout.first * sector..usable_end).contains(&start)
                        && !layout
                            .entries
                            .iter()
                            .any(|entry| (entry.offset..entry.offset + entry.size).contains(&start))
                } else {
                    changes
                        .iter()
                        .any(|part| (part.offset..part.offset + part.old_bytes).contains(&start))
                };
                clear.then_some(Mapping {
                    start,
                    end,
                    source: None,
                })
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn test_partition_numbers(layout: &table_restore::Layout) -> Vec<PartitionNumber> {
    layout
        .entries
        .iter()
        .map(|entry| PartitionNumber {
            number: entry.slot,
            slot: entry.slot,
            can_resize: true,
            can_omit: true,
        })
        .collect()
}

impl Plan {
    #[cfg(test)]
    pub(crate) fn prepare(
        source: &mut impl Source,
        source_size: u64,
        target_size: u64,
        sector: u64,
        expected_ranges: Option<&[Span]>,
    ) -> Result<Self, String> {
        Self::prepare_resized(
            source,
            source_size,
            target_size,
            sector,
            expected_ranges,
            &[],
        )
    }

    #[cfg(test)]
    pub(crate) fn prepare_resized(
        source: &mut impl Source,
        source_size: u64,
        target_size: u64,
        sector: u64,
        expected_ranges: Option<&[Span]>,
        sizes: &[PartitionSize],
    ) -> Result<Self, String> {
        Self::prepare_layout(
            source,
            source_size,
            target_size,
            sector,
            expected_ranges,
            &Options {
                sizes: sizes.to_vec(),
                ..Options::default()
            },
            |layout| Ok(test_partition_numbers(layout)),
        )
    }

    pub(crate) fn prepare_layout(
        source: &mut impl Source,
        source_size: u64,
        target_size: u64,
        sector: u64,
        expected_ranges: Option<&[Span]>,
        options: &Options,
        partition_numbers: impl FnOnce(&table_restore::Layout) -> Result<Vec<PartitionNumber>, String>,
    ) -> Result<Self, String> {
        Self::prepare_layout_with_hive(
            source, source_size, target_size, sector, expected_ranges, options,
            SystemHive::Clean, partition_numbers,
        )
    }

    pub(crate) fn prepare_layout_with_hive(
        source: &mut impl Source,
        source_size: u64,
        target_size: u64,
        sector: u64,
        expected_ranges: Option<&[Span]>,
        options: &Options,
        system_hive: SystemHive,
        partition_numbers: impl FnOnce(&table_restore::Layout) -> Result<Vec<PartitionNumber>, String>,
    ) -> Result<Self, String> {
        options.validate()?;
        let (windows, before) = boot::check_windows(source, source_size, sector, expected_ranges)?;
        let system_digest = windows.system_digest(source, sector, false, system_hive)?;
        let (changes, omitted, mut selection) =
            if options.sizes.is_empty() && options.selection.is_all() {
                (Vec::new(), Vec::new(), None)
            } else {
                if windows.firmware != Firmware::Uefi {
                    return Err("选择迁移分区与调整 NTFS 布局需要 GPT/UEFI。".into());
                }
                let layout = table_restore::archive_layout(&before)?;
                let numbers = partition_numbers(&layout)?;
                if numbers.len() != layout.entries.len()
                    || numbers
                        .iter()
                        .map(|row| row.number)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != numbers.len()
                    || numbers
                        .iter()
                        .map(|row| row.slot)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != numbers.len()
                {
                    return Err("所选迁移分区与 GPT 槽位无法唯一对应。".into());
                }
                let (omitted, selection) = selected_partitions(
                    source,
                    &windows,
                    &layout,
                    &numbers,
                    &options.selection,
                    sector,
                )?;
                let changes =
                    volume_changes(source, &before, &layout, &numbers, &options.sizes, &omitted)?;
                (changes, omitted, selection)
            };
        if let Some(selection) = &mut selection {
            for part in &mut selection.copied {
                if let Some(change) = changes
                    .iter()
                    .find(|change| change.partition == part.partition)
                {
                    part.offset = change.new_offset;
                    part.bytes = change.new_bytes;
                }
            }
        }
        let after = if changes.is_empty() && omitted.is_empty() {
            target_table(&before, target_size, windows.firmware)?
        } else {
            changed_table(&before, target_size, windows.firmware, &changes, &omitted)?
        };
        let partitions = after.validate()?.partitions;
        let mappings = if changes.is_empty() && selection.is_none() {
            Vec::new()
        } else {
            let layout = table_restore::archive_layout(&after)?;
            stream_mappings(
                &changes,
                (layout.last + 1) * sector,
                selection.as_ref().map(|_| (&layout, sector)),
            )
        };
        let mut patches = table_patches(source, &before, &after, &changes, &mappings)?;
        patches.extend(changes.iter().flat_map(|part| part.boot_patches(sector)));
        let fallback = boot::fallback(source, source_size, sector, &windows)?;
        let adds_efi_fallback = !fallback.is_empty();
        patches.extend(fallback);
        patches.sort_by_key(|p| p.offset);
        if patches
            .windows(2)
            .any(|p| p[0].offset + p[0].after.len() as u64 > p[1].offset)
            || patches
                .last()
                .is_some_and(|p| p.offset + p.after.len() as u64 > target_size)
        {
            return Err("启动文件和目标分区表修改范围重叠或越界。".into());
        }
        let result = Self {
            windows,
            before,
            after,
            patches,
            adds_efi_fallback,
            partitions,
            system_digest,
            system_hive,
            changes,
            mappings,
            selection,
        };
        if result.volume_resizes().next().is_none() {
            result.verify_output(&mut result.reader(source).map_err(|e| e.to_string())?)?;
        }
        Ok(result)
    }

    pub(crate) fn preview(&self) -> Preview {
        Preview {
            firmware: self.windows.firmware,
            source_bytes: self.before.size,
            target_bytes: self.after.size,
            sector: self.before.sector,
            boot_offset: self.windows.boot_offset(),
            windows_offset: self
                .system_change()
                .map_or(self.windows.windows_offset(), |part| part.new_offset),
            partitions: self.partitions,
            relocated_gpt: self.windows.firmware == Firmware::Uefi
                && self.before.size != self.after.size,
            adds_efi_fallback: self.adds_efi_fallback,
            partition_changes: self.changes.clone(),
            partition_selection: self.selection.clone(),
        }
    }

    pub(crate) fn adds_efi_fallback(&self) -> bool {
        self.adds_efi_fallback
    }

    pub(crate) fn hot_source_matches(
        &self,
        windows: Span,
        available: impl Fn(Span) -> bool,
    ) -> Result<(), String> {
        if self.windows.windows_offset() != windows.start {
            return Err("BCD 的默认 Windows 安装与正在运行的系统卷不同。".into());
        }
        let selected = |offset: u64| {
            self.selection.as_ref().is_none_or(|selection|
                !selection.skipped.iter().any(|part| part.offset == offset))
        };
        if table_restore::archive_layout(&self.before)?.entries.iter().any(|entry| {
            selected(entry.offset) && !available(Span { start: entry.offset, end: entry.offset + entry.size })
        }) {
            return Err("本次热迁移包含无法一致读取的分区。请选择支持的 NTFS 分区或使用 WinPE。".into());
        }
        Ok(())
    }

    pub(crate) fn mbr_signature(&self) -> Option<[u8; 4]> {
        if self.windows.firmware != Firmware::Bios {
            return None;
        }
        self.before
            .extents
            .iter()
            .find(|extent| extent.kind == Kind::Mbr)?
            .bytes
            .get(440..444)?
            .try_into()
            .ok()
    }

    pub(crate) fn verify_native_gpt(&self, native: &table_restore::Layout) -> Result<(), String> {
        if !table_restore::archive_layout(&self.after)?.agrees(native) {
            return Err("迁移后的 Windows 原生 GPT 布局与预览不同。".into());
        }
        Ok(())
    }

    pub(crate) fn volume_resizes(&self) -> impl Iterator<Item = &VolumeChange> {
        self.changes.iter().filter(|part| part.resizes())
    }

    fn system_change(&self) -> Option<&VolumeChange> {
        self.changes
            .iter()
            .find(|part| part.offset == self.windows.windows_offset())
    }

    pub(crate) fn reader<R: Read + Seek>(
        &self,
        reader: R,
    ) -> io::Result<Overlay<'_, Padded<'_, R>>> {
        let source = Padded::new(reader, self.before.size, self.after.size, &self.mappings);
        Overlay::new(source, &self.patches, self.after.size)
    }

    pub(crate) fn verify_output(&self, reader: &mut impl Source) -> Result<(), String> {
        let (windows, table) =
            boot::check_windows(reader, self.after.size, self.after.sector, None)?;
        let expected = self.windows.with_system_extent(
            self.system_change()
                .map(|part| (part.new_offset, part.new_bytes)),
        );
        for part in &self.changes {
            part.verify(reader, self.after.sector)?;
        }
        if windows != expected
            || table != self.after
            || windows.system_digest(
                reader,
                self.after.sector,
                self.system_change().is_some_and(VolumeChange::resizes),
                self.system_hive,
            )? != self.system_digest
            || !boot::fallback(reader, self.after.size, self.after.sector, &windows)?.is_empty()
        {
            return Err("目标系统的分区表、启动链或 EFI 回退入口与预览不同。".into());
        }
        Ok(())
    }

    pub(crate) fn gpt_identity(&self) -> Result<Option<(GptIdentity, uuid::Uuid)>, String> {
        if self.windows.firmware == Firmware::Bios {
            return Ok(None);
        }
        let layout = table_restore::archive_layout(&self.before)?;
        let guid = |value: &str| uuid::Uuid::parse_str(value).map_err(|e| e.to_string());
        let boot = layout
            .entries
            .iter()
            .find(|entry| entry.offset == self.windows.boot_offset())
            .ok_or("EFI 系统分区不在核对过的 GPT 中。")?;
        Ok(Some((
            GptIdentity {
                disk_guid: guid(&layout.guid)?.to_bytes_le(),
                partitions: layout
                    .entries
                    .iter()
                    .map(|entry| {
                        Ok(disk_core::table_check::GptPartitionIdentity {
                            number: entry.slot,
                            guid: guid(&entry.guid)?.to_bytes_le(),
                            kind: guid(&entry.kind)?.to_bytes_le(),
                            attributes: entry.attributes,
                            range: entry.offset..entry.offset + entry.size,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            },
            guid(&boot.guid)?,
        )))
    }
}

pub(crate) struct Padded<'a, R> {
    source: R,
    copied: u64,
    size: u64,
    offset: u64,
    mappings: &'a [Mapping],
}
impl<'a, R> Padded<'a, R> {
    fn new(source: R, copied: u64, size: u64, mappings: &'a [Mapping]) -> Self {
        Self {
            source,
            copied,
            size,
            offset: 0,
            mappings,
        }
    }
}
impl<R: Read + Seek> Read for Padded<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes = (self.size - self.offset).min(buffer.len() as u64) as usize;
        let start = self.offset;
        let end = start + bytes as u64;
        let mut index = self.mappings.partition_point(|range| range.end <= start);
        while self.offset < end {
            let (next, source) = match self.mappings.get(index) {
                Some(range) if self.offset >= range.start => {
                    let next = range.end.min(end);
                    if next == range.end {
                        index += 1;
                    }
                    (
                        next,
                        range.source.map(|from| from + self.offset - range.start),
                    )
                }
                range => {
                    let next = range.map_or(end, |range| end.min(range.start));
                    if self.offset < self.copied {
                        (next.min(self.copied), Some(self.offset))
                    } else {
                        (next, None)
                    }
                }
            };
            let output = &mut buffer[(self.offset - start) as usize..(next - start) as usize];
            if let Some(source) = source {
                self.source.seek(SeekFrom::Start(source))?;
                self.source.read_exact(output)?;
            } else {
                output.fill(0);
            }
            self.offset = next;
        }
        Ok(bytes)
    }
}
impl<R> Seek for Padded<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.offset) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.size) + i128::from(n),
        };
        if !(0..=i128::from(self.size)).contains(&position) {
            return Err(io::Error::other(
                "Migration stream seek outside target geometry",
            ));
        }
        self.offset = position as u64;
        Ok(self.offset)
    }
}

#[cfg(test)]
#[path = "system_migration_tests.rs"]
mod tests;

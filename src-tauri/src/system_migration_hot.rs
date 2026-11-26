//! Build one bounded disk view from a VSS set and individually verified GPT
//! and protected extents. Never read a live data partition for the copy.
use super::*;
use crate::{
    data_windows::{HotMigrationSource, HotPartitionKind},
    snapshot::{self, Snapshot},
    table_archive,
    used_space::{self, Span},
};
use std::io::{self, Cursor};

#[path = "system_hot_backup.rs"]
mod backup;
pub(in crate::data_tasks) use backup::HotBackup;

pub(in crate::data_tasks) const HOT_TITLE: &str = "热迁移 Windows";
const UNSUPPORTED_PART: &str =
    "本次热迁移包含无法一致读取的分区。请选择支持的 NTFS 分区或使用 WinPE。";
const GUARD_CHUNK: usize = MIB as usize;

pub(crate) struct HotMigration {
    selection: Selection,
    source: HotMigrationSource,
    sector: u64,
    targets: Vec<TargetPlan<()>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::data_tasks) struct HotPreview {
    sector: u64,
    targets: Vec<HotTargetPreview>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HotTargetPreview {
    destination: String,
    bytes: u64,
    partition_selection: PartitionSelection,
    partition_sizes: Vec<PartitionSize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replaced: Vec<ReplacedPartition>,
}

impl HotMigration {
    pub(in crate::data_tasks) fn prepare(
        selection: Selection,
        destinations: Vec<Destination>,
        locale: &Locale,
    ) -> Result<(Self, String, String, String), String> {
        validate_destinations(&selection, destinations.iter())?;
        let source = HotMigrationSource::bind(&selection)?;
        let sector = source
            .resolved
            .disk
            .logical_sector_size
            .ok_or("源盘逻辑扇区大小未知。")?;
        let targets = bind_targets(destinations, &source.resolved)?;
        check_requested(&source, &targets)?;
        let from = source.resolved.label.clone();
        let to = targets
            .iter()
            .map(|target| target.label.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        let warning = locale.sentences(&[
            OVERWRITE_WARNING,
            "确认后创建同一时点的非持久 VSS 快照。快照提供文件系统一致性，应用事务可能需要自行恢复。",
            "NTFS 快照中已核实的空闲范围将在目标盘清零。",
            "各副本保留源盘的磁盘和分区标识。启动任一副本前，请断开源盘和其余副本。",
        ]);
        Ok((
            Self {
                selection,
                source,
                sector,
                targets,
            },
            from,
            to,
            warning,
        ))
    }

    pub(in crate::data_tasks) fn size(&self) -> u64 {
        self.targets.iter().map(|target| target.size).sum()
    }

    pub(in crate::data_tasks) fn preview(&self) -> HotPreview {
        HotPreview {
            sector: self.sector,
            targets: self
                .targets
                .iter()
                .map(|target| HotTargetPreview {
                    destination: target.label.clone(),
                    bytes: target.size,
                    partition_selection: target.destination.partition_selection.clone(),
                    partition_sizes: target.destination.partition_sizes.clone(),
                    replaced: target.before.replaced.clone(),
                })
                .collect(),
        }
    }

    pub(in crate::data_tasks) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let total = self.size();
        let tick = |phase: &str, done| progress(id, cancel, phase, done, total);
        let locale = native_locale::current();
        tick("核对 Windows 迁移来源", 0)?;
        let physical = device::device(&self.source.resolved, false)?;
        self.source.revalidate(&self.selection, &physical)?;
        let mut image = HotImage::capture(
            &self.selection, &self.source, &physical, |number| requested(&self.targets, number), cancel, &tick,
        )?;
        let open = || image.reader(&physical);
        let verify = || {
            tick("核对热迁移源布局与启动范围", 0)?;
            self.source.revalidate(&self.selection, &physical)?;
            image.verify(&physical, cancel, || tick("核对热迁移源布局与启动范围", 0))
        };
        verify()?;
        let mut targets = Vec::with_capacity(self.targets.len());
        for target in self.targets {
            let plan = (|| {
                let plan = read_plan(
                    &self.selection,
                    &self.source.resolved,
                    &physical,
                    open()?,
                    target.size,
                    &target.destination.options(),
                    SystemHive::SnapshotLogs,
                )?;
                image.verify_plan(&plan, &self.source)?;
                Ok(plan)
            })()
            .map_err(|error: String| on_target(&locale, &target.label, &error))?;
            targets.push(target.with_plan(plan));
        }
        let (mut message, hash) = execute_targets(
            &targets,
            self.source.resolved.disk.number,
            self.source.resolved.size,
            self.sector,
            open,
            verify,
            cancel,
            &tick,
            &locale,
        )?;
        if let Err(error) = image.set.complete_registry_copy(|| Ok(())) {
            let warning = locale.text("目标已校验并保持脱机，VSS 完成通知失败。");
            message.push_str(&format!("\n{warning} {error}"));
        }
        Ok((message, hash))
    }
}

fn requested(targets: &[TargetPlan<()>], number: u32) -> bool {
    targets
        .iter()
        .any(|target| match &target.destination.partition_selection {
            PartitionSelection::All => true,
            PartitionSelection::Selected { partitions } => partitions.contains(&number),
        })
}

fn check_requested(source: &HotMigrationSource, targets: &[TargetPlan<()>]) -> Result<(), String> {
    if source.partitions.iter().any(|part| {
        requested(targets, part.number) && matches!(&part.kind, HotPartitionKind::Unavailable)
    }) {
        return Err(UNSUPPORTED_PART.into());
    }
    Ok(())
}

struct HotImage {
    table: Archive,
    guarded: Vec<(Span, Vec<[u8; 32]>)>,
    set: Snapshot,
    volumes: Vec<(Span, used_space::Plan)>,
}

impl HotImage {
    fn capture(
        selection: &Selection,
        source: &HotMigrationSource,
        physical: &File,
        includes: impl Fn(u32) -> bool,
        cancel: &AtomicBool,
        tick: &impl Fn(&str, u64) -> Result<(), String>,
    ) -> Result<Self, String> {
        let check = || tick("核对热迁移源布局与启动范围", 0);
        let sector = source.resolved.disk.logical_sector_size.ok_or("源盘逻辑扇区大小未知。")?;
        let mut disk =
            Region::for_metadata(physical, 0, source.resolved.size).map_err(device::err)?;
        let table = table_archive::capture(
            &mut disk,
            source.resolved.size,
            sector,
            "0".repeat(64),
            false,
            check,
        )?;
        let mut guarded = Vec::new();
        let mut snap_details = Vec::new();
        let mut guard_bytes = 0;
        for part in &source.partitions {
            check()?;
            match &part.kind {
                HotPartitionKind::CheckedPhysical => {
                    guard_bytes += part.span.size();
                    if part.span.size() > 2 << 30 || guard_bytes > 4 << 30 {
                        return Err("热迁移需核对的启动或恢复范围超过上限。".into());
                    }
                    let mut input = Region::new(physical, part.span.start, part.span.size())
                        .map_err(device::err)?;
                    guarded.push((part.span, fingerprint(&mut input, part.span.size(), check)?));
                }
                HotPartitionKind::Snapshot(root)
                    if part.span == source.windows || includes(part.number) =>
                {
                    source.verify_snapshot_volume(selection, root, part.span)?;
                    let boot = table_archive::read_at(
                        &mut disk,
                        part.span.start,
                        sector as usize,
                        source.resolved.size,
                    )?;
                    if boot.get(3..11) != Some(b"NTFS    ") {
                        return Err("热迁移仅支持物理引导扇区可核对的未加密 NTFS 卷。".into());
                    }
                    snap_details.push((part.span, root, boot));
                }
                _ => {}
            }
        }
        let snapshot_volumes: Vec<_> = snap_details
            .iter()
            .map(|(span, root, boot)| snapshot::Volume {
                root,
                bytes: span.size(),
                sector,
                original_boot: Some(boot),
            })
            .collect();
        let set =
            snapshot::create_migration_set(&snapshot_volumes, || tick("正在创建 VSS 快照集", 0))?;
        let mut volumes = Vec::with_capacity(snap_details.len());
        for (index, (span, root, _)) in snap_details.into_iter().enumerate() {
            tick("核对快照中的 NTFS 分配信息", 0)?;
            source.verify_snapshot_volume(selection, root, span)?;
            let mut view =
                Region::for_file_view(set.file(index)?, 0, span.size()).map_err(device::err)?;
            volumes.push((
                span,
                snapshot_allocation(&mut view, span.size(), sector, cancel)?,
            ));
        }
        Ok(Self {
            table,
            guarded,
            set,
            volumes,
        })
    }

    fn covers(&self, span: Span) -> bool {
        self.volumes.iter().any(|(range, _)| *range == span)
            || self.guarded.iter().any(|(range, _)| *range == span)
    }

    fn verify_plan(&self, plan: &Plan, source: &HotMigrationSource) -> Result<(), String> {
        if plan.gpt_identity()?.map(|(_, boot)| boot) != Some(source.efi) {
            return Err("BCD 启动分区与当前 Windows 的 EFI 分区不同。".into());
        }
        plan.hot_source_matches(source.windows, |span| self.covers(span))
    }

    fn verify(
        &self,
        physical: &File,
        cancel: &AtomicBool,
        check: impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut disk = Region::for_metadata(physical, 0, self.table.size).map_err(device::err)?;
        self.table.matches(&mut disk, &check)?;
        for (span, before) in &self.guarded {
            let mut input = Region::new(physical, span.start, span.size()).map_err(device::err)?;
            if fingerprint(&mut input, span.size(), &check)? != *before {
                return Err("快照期间启动或恢复范围已变化。".into());
            }
        }
        for (index, (span, allocation)) in self.volumes.iter().enumerate() {
            check()?;
            let mut view = Region::for_file_view(self.set.file(index)?, 0, span.size())
                .map_err(device::err)?;
            allocation.proof.verify(&mut view, cancel)?;
        }
        Ok(())
    }

    fn reader<'a>(&'a self, physical: &'a File) -> Result<DiskReader<'a>, String> {
        let mut segments = Vec::new();
        for entry in &self.table.extents {
            let span = Span {
                start: entry.offset,
                end: entry.offset + entry.bytes.len() as u64,
            };
            segments.push(Segment {
                span,
                data: Box::new(Cursor::new(entry.bytes.as_slice())),
            });
        }
        for (span, hashes) in &self.guarded {
            let input = Region::new(physical, span.start, span.size()).map_err(device::err)?;
            segments.push(Segment {
                span: *span,
                data: Box::new(GuardReader::new(input, span.size(), hashes)),
            });
        }
        for (index, (span, allocation)) in self.volumes.iter().enumerate() {
            let input = Region::for_file_view(self.set.file(index)?, 0, span.size())
                .map_err(device::err)?;
            segments.push(Segment {
                span: *span,
                data: Box::new(SnapshotReader {
                    input,
                    free: allocation.free_ranges(),
                    size: span.size(),
                    pos: 0,
                }),
            });
        }
        DiskReader::new(self.table.size, segments).map_err(device::err)
    }
}

fn snapshot_allocation(
    input: &mut impl Source,
    size: u64,
    sector: u64,
    cancel: &AtomicBool,
) -> Result<used_space::Plan, String> {
    let mut builder = used_space::Builder::new(size, sector)?;
    let mut reader =
        used_space::Audit::aligned(input, 0, size, sector, &mut builder.proof, cancel)?;
    let fs = ntfs::Ntfs::new(&mut reader).map_err(device::err)?;
    let volume_end = fs
        .size()
        .checked_add(sector)
        .filter(|&end| end <= size)
        .ok_or("NTFS 快照文件系统范围超出分区。")?;
    if u64::from(fs.sector_size()) != sector {
        return Err("NTFS 快照逻辑扇区与源分区不同。".into());
    }
    let map = crate::ntfs_allocation::inspect(&mut reader, cancel)?;
    if map.free.first().is_some_and(|free| free.start < 8192) {
        return Err("NTFS 分配信息与保留引导范围冲突。".into());
    }
    for free in map.free {
        builder.free(free.start, free.size())?;
    }
    builder.free(volume_end, size - volume_end)?;
    builder.finish()
}

// VSS preserves the file system view. Bytes in free clusters and padding
// have no stable file contents and must not enter the reproducible stream.
struct SnapshotReader<'a, R> {
    input: R,
    free: &'a [Span],
    size: u64,
    pos: u64,
}

impl<R: Read + Seek> Read for SnapshotReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let next = self
            .free
            .get(self.free.partition_point(|span| span.end <= self.pos));
        let (end, zero) = match next {
            Some(span) if span.start <= self.pos => (span.end, true),
            Some(span) => (span.start, false),
            None => (self.size, false),
        };
        let count = (end - self.pos).min(output.len() as u64) as usize;
        let output = &mut output[..count];
        if zero {
            output.fill(0);
        } else if count != 0 {
            self.input.seek(SeekFrom::Start(self.pos))?;
            self.input.read_exact(output)?;
        }
        self.pos += count as u64;
        Ok(count)
    }
}

impl<R> Seek for SnapshotReader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        bounded_seek(&mut self.pos, self.size, from)
    }
}

fn fingerprint(
    input: &mut impl Read,
    size: u64,
    checkpoint: impl Fn() -> Result<(), String>,
) -> Result<Vec<[u8; 32]>, String> {
    let mut bytes = vec![0; GUARD_CHUNK];
    let mut hashes = Vec::new();
    for offset in (0..size).step_by(GUARD_CHUNK) {
        checkpoint()?;
        let count = (size - offset).min(GUARD_CHUNK as u64) as usize;
        input.read_exact(&mut bytes[..count]).map_err(device::err)?;
        hashes.push(Sha256::digest(&bytes[..count]).into());
    }
    Ok(hashes)
}

struct Segment<'a> {
    span: Span,
    data: Box<dyn Source + 'a>,
}

struct DiskReader<'a> {
    size: u64,
    pos: u64,
    segments: Vec<Segment<'a>>,
}

impl<'a> DiskReader<'a> {
    fn new(size: u64, mut segments: Vec<Segment<'a>>) -> io::Result<Self> {
        segments.sort_by_key(|entry| entry.span.start);
        let mut end = 0;
        for entry in &segments {
            if entry.span.start < end || entry.span.end <= entry.span.start || entry.span.end > size
            {
                return Err(io::Error::other("热迁移源范围重叠或越界。"));
            }
            end = entry.span.end;
        }
        Ok(Self {
            size,
            pos: 0,
            segments,
        })
    }
}

impl Read for DiskReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let index = self
            .segments
            .partition_point(|entry| entry.span.end <= self.pos);
        let entry = self.segments.get_mut(index);
        let end = match entry.as_ref() {
            Some(entry) if entry.span.start <= self.pos => entry.span.end,
            Some(entry) => entry.span.start,
            None => self.size,
        };
        let count = (end - self.pos).min(bytes.len() as u64) as usize;
        if let Some(entry) = entry.filter(|entry| self.pos >= entry.span.start) {
            entry
                .data
                .seek(SeekFrom::Start(self.pos - entry.span.start))?;
            entry.data.read_exact(&mut bytes[..count])?;
        } else {
            bytes[..count].fill(0);
        }
        self.pos += count as u64;
        Ok(count)
    }
}

impl Seek for DiskReader<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        bounded_seek(&mut self.pos, self.size, from)
    }
}

struct GuardReader<'a, R> {
    input: R,
    size: u64,
    pos: u64,
    hashes: &'a [[u8; 32]],
    bytes: Vec<u8>,
    loaded: Option<usize>,
}

impl<'a, R> GuardReader<'a, R> {
    fn new(input: R, size: u64, hashes: &'a [[u8; 32]]) -> Self {
        Self {
            input,
            size,
            pos: 0,
            hashes,
            bytes: Vec::new(),
            loaded: None,
        }
    }
}

impl<R: Read + Seek> Read for GuardReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.pos == self.size {
            return Ok(0);
        }
        let index = (self.pos / GUARD_CHUNK as u64) as usize;
        let offset = index as u64 * GUARD_CHUNK as u64;
        if self.loaded != Some(index) {
            self.loaded = None;
            let count = (self.size - offset).min(GUARD_CHUNK as u64) as usize;
            self.bytes.resize(count, 0);
            self.input.seek(SeekFrom::Start(offset))?;
            self.input.read_exact(&mut self.bytes)?;
            let hash: [u8; 32] = Sha256::digest(&self.bytes).into();
            if self.hashes.get(index) != Some(&hash) {
                return Err(io::Error::other("读取时启动或恢复范围已变化。"));
            }
            self.loaded = Some(index);
        }
        let at = (self.pos - offset) as usize;
        let count = output.len().min(self.bytes.len() - at);
        output[..count].copy_from_slice(&self.bytes[at..at + count]);
        self.pos += count as u64;
        Ok(count)
    }
}

impl<R> Seek for GuardReader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        bounded_seek(&mut self.pos, self.size, from)
    }
}

fn bounded_seek(pos: &mut u64, size: u64, from: SeekFrom) -> io::Result<u64> {
    let next = match from {
        SeekFrom::Start(offset) => i128::from(offset),
        SeekFrom::Current(offset) => i128::from(*pos) + i128::from(offset),
        SeekFrom::End(offset) => i128::from(size) + i128::from(offset),
    };
    if !(0..=i128::from(size)).contains(&next) {
        return Err(io::Error::other("热迁移源读取范围越界。"));
    }
    *pos = next as u64;
    Ok(*pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_source_reads_only_bound_gpt_and_snapshot_extents() {
        let item = |start, bytes: &'static [u8]| Segment {
            span: Span {
                start,
                end: start + bytes.len() as u64,
            },
            data: Box::new(Cursor::new(bytes)),
        };
        let mut disk = DiskReader::new(11, vec![item(5, b"vss"), item(0, b"gpt")]).unwrap();
        let mut bytes = [0xff; 11];
        disk.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"gpt\0\0vss\0\0\0");
        assert!(disk.seek(SeekFrom::End(1)).is_err());
        assert!(disk.seek(SeekFrom::Start(12)).is_err());
        disk.seek(SeekFrom::Start(2)).unwrap();
        disk.read_exact(&mut bytes[..5]).unwrap();
        assert_eq!(&bytes[..5], b"t\0\0vs");
        assert!(DiskReader::new(9, vec![item(0, b"gpt"), item(2, b"vss")]).is_err());
        assert!(DiskReader::new(8, vec![item(6, b"bad")]).is_err());
    }

    #[test]
    fn hot_source_refuses_a_changed_boot_block_before_exposing_its_bytes() {
        let mut stable = vec![0x23; GUARD_CHUNK + 512];
        let hashes =
            fingerprint(&mut Cursor::new(&stable), stable.len() as u64, || Ok(())).unwrap();
        stable[GUARD_CHUNK + 10] ^= 1;
        let mut view = GuardReader::new(Cursor::new(&stable), stable.len() as u64, &hashes);
        let mut bytes = [0xf1; 2];
        view.seek(SeekFrom::Start(GUARD_CHUNK as u64 - 1)).unwrap();
        assert_eq!(view.read(&mut bytes).unwrap(), 1);
        assert_eq!(bytes, [0x23, 0xf1]);
        assert!(view.read(&mut bytes[1..]).is_err());
        assert_eq!(bytes, [0x23, 0xf1]);
        assert_eq!(view.stream_position().unwrap(), GUARD_CHUNK as u64);
    }

    #[test]
    fn snapshot_stream_is_stable_when_only_free_bytes_change() {
        let free = [Span { start: 2, end: 5 }, Span { start: 7, end: 8 }];
        for raw in [b"ab123cd4", b"ab789cd0"] {
            let mut reader = SnapshotReader {
                input: Cursor::new(raw),
                free: &free,
                size: raw.len() as u64,
                pos: 0,
            };
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            assert_eq!(output, b"ab\0\0\0cd\0");
            reader.seek(SeekFrom::Start(4)).unwrap();
            let mut output = [0xff; 4];
            reader.read_exact(&mut output).unwrap();
            assert_eq!(&output, b"\0cd\0");
        }
    }
}

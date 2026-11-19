//! Copy-on-write resize of a single-device Btrfs filesystem.
use super::*;
use btrfs_disk::superblock::csum_superblock;
use btrfs_transaction::{Filesystem, Transaction};
use std::io::{self, Write};

#[path = "btrfs_relocation.rs"]
mod relocation;
use relocation::{touches_super_stripe, Relocation};

const MIN_SIZE: u64 = 128 * 1024 * 1024;
const ALLOCATION_LIMIT: usize = 1_000_000;

pub struct ResizePlan {
    before: u64,
    after: u64,
    minimum: u64,
    committed: Super,
    mapping: BTreeMap<u64, Chunk>,
    relocation: Relocation,
    files: BTreeMap<u64, BlockRef>,
    checksum: Option<BlockRef>,
    label: [u8; 256],
    protected: BTreeMap<u64, u64>,
}

impl ResizePlan {
    pub fn before(&self) -> u64 {
        self.before
    }

    /// Lowest boundary that would keep all stripes in their original positions.
    pub fn minimum(&self) -> u64 {
        self.minimum
    }

    pub fn relocation_bytes(&self) -> u64 {
        self.relocation.bytes()
    }

    pub fn proof(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.committed.identity);
        hash.update(self.after.to_le_bytes());
        self.relocation.bind_moves(&mut hash);
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn super_mirror(
    reader: &mut (impl Read + Seek),
    offset: u64,
    size: u64,
) -> Result<([u8; SUPER_BYTES], Super), String> {
    let mut bytes = [0; SUPER_BYTES];
    read_at(reader, offset, &mut bytes)?;
    if &bytes[64..72] != b"_BHRfS_M" {
        return Err("Btrfs 超级块缺失。".into());
    }
    let sb = Super::parse(&bytes, offset, size)?;
    Ok((bytes, sb))
}

fn consistent_mirrors(
    reader: &mut (impl Read + Seek),
    size: u64,
    identity: [u8; 32],
) -> Result<(), String> {
    for offset in SUPER_OFFSETS {
        if offset + SUPER_BYTES as u64 <= size
            && super_mirror(reader, offset, size)?.1.identity != identity
        {
            return Err("Btrfs 超级块副本不一致。".into());
        }
    }
    Ok(())
}

pub fn plan_resize(reader: &mut (impl Read + Seek), after: u64) -> Result<ResizePlan, String> {
    let before = reader.seek(SeekFrom::End(0)).map_err(err)?;
    let mut volume = open(reader, || Ok(()))?;
    let (raw, sb) = super_mirror(&mut volume.reader.primary, SUPER_OFFSETS[0], before)?;
    let label = word::<256>(&raw, 0x12b)?;
    let label_end = label
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(label.len());
    if sb.identity != volume.identity()
        || sb.device.size != before
        || volume
            .mapping
            .values()
            .any(|chunk| !matches!(chunk.kind & !7, 0 | DUP))
        || sb.sector_size != 4096
        || le64(&raw, 0x38)? != 1
        || le64(&raw, 0xb4)? & !3 != 0
        || volume.current.checksum.is_none()
        || std::str::from_utf8(&label[..label_end]).is_err()
        || label[label_end..].iter().any(|&byte| byte != 0)
        || after == before
        || after % u64::from(sb.sector_size) != 0
    {
        return Err("Btrfs 缩放需要容量匹配、干净卸载且受支持的 4 KiB 单设备文件系统。".into());
    }
    consistent_mirrors(&mut volume.reader.primary, before, sb.identity)?;
    if after < MIN_SIZE {
        return Err(format!("Btrfs 目标大小至少为 {MIN_SIZE} 字节。"));
    }
    let balance = Key {
        object_id: u64::MAX - 3,
        kind: 248,
        offset: 0,
    };
    scan_tree(
        &mut volume.reader,
        &sb,
        &volume.mapping,
        sb.root,
        ROOT_TREE,
        Some((balance, balance)),
        |_, _| Err("Btrfs 存在未完成的块组平衡操作，请在 Linux 完成后再缩放。".into()),
        || Ok(()),
    )?;
    let devices = volume.verify_device_extents()?;
    let minimum = if let Some((&start, item)) = devices.last_key_value() {
        MIN_SIZE.max(start + le64(item, 24)?)
    } else {
        MIN_SIZE
    };
    let relocation = Relocation::plan(&sb, &volume.mapping, &devices, after)?;
    let protected = protected_extents(&mut volume, &relocation.mapping)?;
    Ok(ResizePlan {
        before,
        after,
        minimum,
        committed: sb,
        mapping: volume.mapping,
        relocation,
        files: volume.current.roots,
        checksum: volume.current.checksum,
        label,
        protected,
    })
}

fn protected_extents(
    volume: &mut Volume<impl Read + Seek>,
    target: &BTreeMap<u64, Chunk>,
) -> Result<BTreeMap<u64, u64>, String> {
    let sb = &volume.superblock;
    let sector = u64::from(sb.sector_size);
    let root = volume.current.extent.ok_or("Btrfs 缺少分配树。")?;
    let mut protected = BTreeMap::new();
    scan_tree(
        &mut volume.reader,
        sb,
        &volume.mapping,
        root,
        2,
        None,
        |key, item| {
            let length = match key.kind {
                168 => key.offset,
                169 => u64::from(sb.node_size),
                _ => return Ok(()),
            };
            if length == 0
                || length % sector != 0
                || key.object_id % sector != 0
                || le64(item, 0)? == 0
                || !(1..=sb.generation).contains(&le64(item, 8)?)
                || !matches!(le64(item, 16)? & 3, 1 | 2)
            {
                return Err("Btrfs 分配记录的范围、代次或引用无效。".into());
            }
            let (&start, chunk) = volume
                .mapping
                .range(..=key.object_id)
                .next_back()
                .ok_or("Btrfs 分配记录缺少块组。")?;
            let distance = key.object_id - start;
            if distance
                .checked_add(length)
                .is_none_or(|end| end > chunk.length)
            {
                return Err("Btrfs 分配记录跨越块组。".into());
            }
            let moved = chunk
                .stripes
                .iter()
                .zip(&target[&start].stripes)
                .filter_map(|(old, new)| (old.offset != new.offset).then_some(new.offset));
            for stripe in chunk
                .stripes
                .iter()
                .map(|stripe| stripe.offset)
                .chain(moved)
            {
                let start = stripe + distance;
                let end = start + length;
                if protected.len() >= ALLOCATION_LIMIT
                    || overlaps(&protected, start, end)
                    || touches_super_stripe(start, end)
                {
                    return Err("Btrfs 分配范围重叠、触及超级块或超过检查上限。".into());
                }
                protected.insert(start, end);
            }
            Ok(())
        },
        || Ok(()),
    )?;
    Ok(protected)
}

fn overlaps(ranges: &BTreeMap<u64, u64>, start: u64, end: u64) -> bool {
    ranges
        .range(..end)
        .next_back()
        .is_some_and(|(_, &limit)| limit > start)
}

/// The stream spans the larger of the old and new partition capacities.
/// The caller keeps the old partition intact through shrink, or grows its
/// boundary before calling.
/// Write::flush on the supplied stream must flush the actual storage.
pub fn resize(stream: &mut (impl Read + Write + Seek), plan: &ResizePlan) -> Result<(), String> {
    let (_, current) = super_mirror(stream, SUPER_OFFSETS[0], plan.before)?;
    if current.identity != plan.committed.identity
        || stream.seek(SeekFrom::End(0)).map_err(err)? != plan.before.max(plan.after)
    {
        return Err("Btrfs 文件系统或目标范围与预览不同。".into());
    }
    let next = current
        .generation
        .checked_add(1)
        .ok_or("Btrfs 事务代次溢出。")?;
    plan.relocation.copy_and_verify(stream)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
        let guarded = CommitIo {
            stream: &mut *stream,
            plan,
            next,
            publishing: false,
            capacity: plan.before,
        };
        let mut fs = Filesystem::open(guarded)?;
        plan.relocation.remap(&plan.mapping, &current, &mut fs)?;
        let device = fs
            .reader_mut()
            .devices_mut()
            .get_mut(&current.device.id)
            .ok_or_else(|| io::Error::other("Missing Btrfs transaction device"))?;
        device.capacity = plan.after;
        let mut transaction = Transaction::start(&mut fs)?;
        if transaction.set_device_total_bytes(&mut fs, current.device.id, plan.after)?
            != plan.before
        {
            return Err(io::Error::other("Btrfs device capacity changed"));
        }
        plan.relocation
            .edit_trees(&plan.mapping, &current, &mut fs, &mut transaction)?;
        fs.superblock.total_bytes = plan.after;
        transaction.commit(&mut fs)
    }))
    .map_err(|_| "Btrfs 缩放事务异常结束。")?;
    result.map_err(err)?;
    stream.flush().map_err(err)?;
    let mut volume = open(
        CommitIo {
            stream,
            plan,
            next,
            publishing: true,
            capacity: plan.after,
        },
        || Ok(()),
    )?;
    let (raw, sb) = super_mirror(&mut volume.reader.primary, SUPER_OFFSETS[0], plan.after)?;
    if sb.fsid != current.fsid
        || sb.algorithm != current.algorithm
        || sb.device.id != current.device.id
        || sb.device.uuid != current.device.uuid
        || sb.device.size != plan.after
        || sb.generation != next
        || word::<256>(&raw, 0x12b)? != plan.label
        || sb.bootstrap != plan.relocation.system
        || volume.mapping != plan.relocation.mapping
        || volume.current.roots != plan.files
        || volume.current.checksum != plan.checksum
    {
        return Err("Btrfs 缩放后的容量、身份、块组或文件树与预期不符。".into());
    }
    consistent_mirrors(&mut volume.reader.primary, plan.after, sb.identity)?;
    volume.verify_device_extents()?;
    volume.verify_view(|| Ok(()))
}

struct CommitIo<'a, R> {
    stream: &'a mut R,
    plan: &'a ResizePlan,
    next: u64,
    publishing: bool,
    capacity: u64,
}

impl<R: Read> Read for CommitIo<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<R: Seek> Seek for CommitIo<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let from = match from {
            SeekFrom::End(delta) => SeekFrom::Start(
                self.capacity
                    .checked_add_signed(delta)
                    .ok_or_else(|| io::Error::other("Btrfs seek outside the device"))?,
            ),
            from => from,
        };
        self.stream.seek(from)
    }
}

impl<R: Seek + Write> Write for CommitIo<'_, R> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let start = self.stream.stream_position()?;
        let end = start
            .checked_add(data.len() as u64)
            .ok_or_else(|| io::Error::other("Btrfs write overflow"))?;
        let sector = u64::from(self.plan.committed.sector_size);
        if end > self.plan.after || start % sector != 0 || end % sector != 0 {
            return Err(io::Error::other("Btrfs write outside the aligned target"));
        }
        if SUPER_OFFSETS.contains(&start) && data.len() == SUPER_BYTES {
            let mut bytes: [u8; SUPER_BYTES] = data.try_into().unwrap();
            let read = |at| le64(&bytes, at).map_err(io::Error::other);
            if &bytes[64..72] != b"_BHRfS_M"
                || le16(&bytes, 0xc4).map_err(io::Error::other)?
                    != self.plan.committed.algorithm.to_raw()
                || !checksum_matches(self.plan.committed.algorithm, &bytes[32..], &bytes[..32])
                || read(0x70)? != self.plan.after
                || ![self.plan.before, self.plan.after].contains(&read(0xd1)?)
            {
                return Err(io::Error::other(
                    "Unexpected Btrfs device size in superblock",
                ));
            }
            // v0.13.0 serializes the cached pre-transaction dev_item over
            // the updated primary device fields. The chunk tree is already
            // updated. Keep the single-device mirror's capacity coherent.
            bytes[0xd1..0xd9].copy_from_slice(&self.plan.after.to_le_bytes());
            csum_superblock(&mut bytes)?;
            let sb = Super::parse(&bytes, start, self.plan.after).map_err(io::Error::other)?;
            if sb.fsid != self.plan.committed.fsid
                || sb.generation != self.next
                || sb.device.size != self.plan.after
                || sb.bootstrap != self.plan.relocation.system
            {
                return Err(io::Error::other("Unexpected Btrfs superblock publication"));
            }
            if !self.publishing {
                self.stream.flush()?;
                self.publishing = true;
            }
            self.stream.write_all(&bytes)?;
            return Ok(data.len());
        }
        if self.publishing
            || overlaps(&self.plan.protected, start, end)
            || touches_super_stripe(start, end)
        {
            return Err(io::Error::other(
                "Btrfs transaction would overwrite committed blocks",
            ));
        }
        let mapped_metadata = |logical: u64| {
            self.plan
                .relocation
                .mapping
                .range(..=logical)
                .next_back()
                .is_some_and(|(&key, chunk)| {
                    let delta = logical - key;
                    chunk.kind & (SYSTEM | METADATA) != 0
                        && delta
                            .checked_add(data.len() as u64)
                            .is_some_and(|limit| limit <= chunk.length)
                        && chunk
                            .stripes
                            .iter()
                            .any(|stripe| stripe.offset + delta == start)
                        && chunk.stripes.iter().all(|stripe| {
                            let physical = stripe.offset + delta;
                            !touches_super_stripe(physical, physical + data.len() as u64)
                        })
                })
        };
        let valid_metadata = data.len() == self.plan.committed.node_size as usize
            && word::<16>(data, 32).ok() == Some(self.plan.committed.fsid)
            && le64(data, 80).ok() == Some(self.next)
            && checksum_matches(self.plan.committed.algorithm, &data[32..], &data[..32])
            && le64(data, 48).is_ok_and(mapped_metadata);
        if !valid_metadata {
            return Err(io::Error::other("Unexpected Btrfs transaction write"));
        }
        self.stream.write_all(data)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

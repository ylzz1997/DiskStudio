//! Native Ext2/3/4 tools, restricted to one inherited handle and one partition extent.
//! Called only after an explicit partition preview/confirmation, never inventory.
use crate::{
    data_io::Region,
    data_tasks::Selection,
    data_windows as device,
    partitions::{policy, Action, ExtFileSystem, Outcome, Request},
};
use policy::{Disk, Partition};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    mem::{offset_of, size_of},
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
};
use windows_sys::Win32::{Storage::FileSystem::FILE_SHARE_READ, System::Ioctl::*};

const ASSETS: [(&str, &[u8]); 4] = [
    ("mke2fs.exe", include_bytes!("../ext4-engine/mke2fs.exe")),
    ("e2fsck.exe", include_bytes!("../ext4-engine/e2fsck.exe")),
    (
        "resize2fs.exe",
        include_bytes!("../ext4-engine/resize2fs.exe"),
    ),
    ("mke2fs.conf", include_bytes!("../ext4-engine/mke2fs.conf")),
];
struct Engine {
    directory: PathBuf,
    held: Vec<File>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.held.clear();
        for (name, _) in ASSETS {
            let _ = fs::remove_file(self.directory.join(name));
        }
        let _ = fs::remove_dir(&self.directory);
    }
}
impl Engine {
    fn new() -> Result<Self, String> {
        let directory =
            std::env::temp_dir().join(format!("DiskStudio-ext4-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).map_err(error)?;
        let mut engine = Self {
            directory,
            held: vec![],
        };
        for (name, bytes) in ASSETS {
            let path = engine.directory.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(0)
                .open(&path)
                .map_err(error)?;
            file.write_all(bytes).map_err(error)?;
            file.sync_all().map_err(error)?;
            drop(file);
            let mut held = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(path)
                .map_err(error)?;
            let mut actual = vec![];
            held.read_to_end(&mut actual).map_err(error)?;
            if actual != bytes {
                return Err("Ext 引擎完整性校验失败。".into());
            }
            engine.held.push(held);
        }
        Ok(engine)
    }
    fn run(
        &self,
        tool: &str,
        args: &[&str],
        file: &File,
        offset: u64,
        length: u64,
        sector: u64,
        write: bool,
    ) -> Result<String, String> {
        if !["mke2fs.exe", "e2fsck.exe", "resize2fs.exe"].contains(&tool) {
            return Err("未知 Ext 操作。".into());
        }
        let mut child = Command::new(self.directory.join(tool))
            .args(args)
            .current_dir(&self.directory)
            .creation_flags(0x08000000)
            .env_clear()
            .env(
                "SystemRoot",
                std::env::var_os("SystemRoot").ok_or("Windows 系统目录不可用。")?,
            )
            .env("LC_ALL", "C")
            .env("MKE2FS_CONFIG", "mke2fs.conf")
            .env("E2FSCK_CONFIG", "NUL")
            .env("DISKSTUDIO_EXT4_OFFSET", offset.to_string())
            .env("DISKSTUDIO_EXT4_LENGTH", length.to_string())
            .env("DISKSTUDIO_EXT4_SECTOR", sector.to_string())
            .env("DISKSTUDIO_EXT4_WRITE", if write { "1" } else { "0" })
            .stdin(Stdio::from(file.try_clone().map_err(error)?))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(error)?;
        // Drain both pipes even after the retained log reaches its bound.
        let out = child.stdout.take().ok_or("Ext 输出管道不可用")?;
        let err = child.stderr.take().ok_or("Ext 错误管道不可用")?;
        let stdout = std::thread::spawn(move || collect(out));
        let stderr = std::thread::spawn(move || collect(err));
        let status = child.wait().map_err(error)?;
        let output = stdout.join().map_err(|_| "Ext 输出线程失败")??;
        let errors = stderr.join().map_err(|_| "Ext 错误线程失败")??;
        if !status.success() {
            return Err(format!(
                "{tool} 未完成（代码 {:?}）：{} {}",
                status.code(),
                tail(&output),
                tail(&errors)
            ));
        }
        Ok(output)
    }
}
fn collect(mut input: impl Read) -> Result<String, String> {
    let mut output = vec![];
    let mut data = [0; 8192];
    loop {
        let n = input.read(&mut data).map_err(error)?;
        if n == 0 {
            break;
        }
        if output.len() + n > 65536 {
            output.drain(..(output.len() + n - 65536));
        }
        output.extend_from_slice(&data[..n]);
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}
fn tail(s: &str) -> String {
    s.chars()
        .rev()
        .take(1600)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}
fn error(e: impl std::fmt::Display) -> String {
    format!("Ext：{e}")
}
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
#[derive(Debug)]
struct Superblock {
    blocks: u64,
    block_size: u64,
    file_system: ExtFileSystem,
    uuid: [u8; 16],
    label: Vec<u8>,
}
fn parse_super(b: &[u8], length: u64) -> Result<Superblock, String> {
    if b.len() != 1024 || u16_at(b, 56) != 0xef53 || u32_at(b, 24) > 2 {
        return Err("仅支持已确认、块大小为 1/2/4 KiB 的 Ext2/3/4 文件系统。".into());
    }
    let block_size = 1024u64 << u32_at(b, 24);
    let compat = u32_at(b, 92);
    let incompat = u32_at(b, 96);
    let ro = u32_at(b, 100);
    if compat & !0x3c != 0
        || incompat & !0x2c2 != 0
        || ro & !0x47b != 0
        || u16_at(b, 58) != 1
        || u32_at(b, 232) != 0
    {
        return Err("Ext 文件系统未干净卸载或含暂不支持的特性；请在 Linux 检查并正常卸载后再试。当前不支持加密、内联数据、bigalloc、外部日志、MMP 和 fast_commit。".into());
    }
    // Ext generations share the magic. Classify their on-disk feature set;
    // never infer a filesystem from the GPT Linux data partition type.
    let file_system = if incompat & 0x2c0 != 0 || ro & 0x478 != 0 {
        ExtFileSystem::Ext4
    } else if compat & 4 != 0 {
        ExtFileSystem::Ext3
    } else {
        ExtFileSystem::Ext2
    };
    let blocks = u32_at(b, 4) as u64
        | if incompat & 0x80 != 0 {
            (u32_at(b, 336) as u64) << 32
        } else {
            0
        };
    if blocks == 0
        || blocks.checked_mul(block_size).is_none_or(|n| n > length)
        || b[104..120].iter().all(|x| *x == 0)
    {
        return Err("Ext 容量或 UUID 无效，已拒绝操作。".into());
    }
    let label = b[120..136].split(|x| *x == 0).next().unwrap().to_vec();
    Ok(Superblock {
        blocks,
        block_size,
        file_system,
        uuid: b[104..120].try_into().unwrap(),
        label,
    })
}
fn super_bytes(file: &File, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    let mut region =
        Region::new(file.try_clone().map_err(error)?, offset, length).map_err(error)?;
    // Raw 4Kn devices require sector-sized reads. Read the first aligned 4 KiB
    // and slice the superblock, rather than issuing a 1 KiB raw-device request.
    let mut b = vec![0; 4096];
    region.read_exact(&mut b).map_err(error)?;
    Ok(b[1024..2048].to_vec())
}
pub fn is_action(action: &Action) -> bool {
    matches!(action, Action::ExtFormat { .. } | Action::ExtResize { .. })
}
fn resolved(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
) -> Result<device::Resolved, String> {
    // Resolve the entire disk to lock every Windows volume and reject paging,
    // system/boot and ambiguous associations. The selected extent is separate.
    let selection = Selection {
        target: request.target.clone(),
        partition: None,
        layout_key: crate::disk_identity::layout_key(
            disk.partition_style,
            parts
                .iter()
                .map(|p| (p.partition_number, p.offset, p.size, p.guid.clone()))
                .collect(),
        ),
    };
    device::resolve(&selection, true, true)
}
pub fn preview(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    p: &Partition,
) -> Result<String, String> {
    let resolved = resolved(request, disk, parts)?;
    let file = device::device(&resolved, false)?;
    check_native_target(&native_layout(&file, parts)?, p)?;
    let before = super_bytes(&file, p.offset, p.size)?;
    if let Action::ExtResize { size, .. } = request.action {
        let superblock = parse_super(&before, p.size)?;
        let engine = Engine::new()?;
        engine.run(
            "e2fsck.exe",
            &["-f", "-n", "diskstudio"],
            &file,
            p.offset,
            p.size,
            disk.logical_sector_size.unwrap(),
            false,
        )?;
        let estimate = engine.run(
            "resize2fs.exe",
            &["-P", "diskstudio"],
            &file,
            p.offset,
            p.size,
            disk.logical_sector_size.unwrap(),
            false,
        )?;
        let minimum = estimate
            .lines()
            .find_map(|l| l.strip_prefix("Estimated minimum size of the filesystem: "))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or("Ext 未提供可信的最小容量。")?;
        // -P is an estimate, not proof. Keep 16 MiB headroom; resize2fs still
        // performs its own minimum-size check before relocating any blocks.
        let minimum = minimum
            .checked_mul(superblock.block_size)
            .and_then(|n| n.checked_add(16 * policy::MIB))
            .ok_or("Ext 最小容量溢出")?;
        if size < minimum {
            return Err(format!(
                "目标容量过小；至少保留约 {:.1} MiB，并留出可用空间。",
                minimum as f64 / policy::MIB as f64
            ));
        }
    }
    if super_bytes(&file, p.offset, p.size)? != before {
        return Err("检查期间 Ext 元数据发生变化，请重新预览。".into());
    }
    Ok(format!("{:x}", Sha256::digest(&before)))
}

const HEADER: usize = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
const ENTRY: usize = size_of::<PARTITION_INFORMATION_EX>();
fn guid(g: windows_sys::core::GUID) -> String {
    uuid::Uuid::from_fields(g.data1, g.data2, g.data3, &g.data4).to_string()
}
fn entry(b: &[u8], i: usize) -> PARTITION_INFORMATION_EX {
    unsafe {
        b.as_ptr()
            .add(HEADER + i * ENTRY)
            .cast::<PARTITION_INFORMATION_EX>()
            .read_unaligned()
    }
}
fn check_native_target(b: &[u8], selected: &Partition) -> Result<(), String> {
    let matching: Vec<_> = (0..u32_at(b, 4) as usize)
        .map(|i| entry(b, i))
        .filter(|p| p.PartitionNumber == selected.partition_number)
        .collect();
    if matching.len() != 1
        || matching[0].IsServicePartition
        || unsafe { matching[0].Anonymous.Gpt.Attributes } & 0x1000000000000001 != 0
    {
        return Err("原生 GPT 分区身份不唯一或设置了只读/平台保护属性。".into());
    }
    Ok(())
}
#[cfg(test)]
#[test]
fn sector_native_target_rejects_protected_attributes_and_unknown_types() {
    let mut bytes = vec![0; HEADER + ENTRY];
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    let selected = Partition {
        partition_number: 1,
        ..Default::default()
    };
    let mut entry: PARTITION_INFORMATION_EX = unsafe { std::mem::zeroed() };
    entry.PartitionNumber = 1;
    let guid = windows_sys::core::GUID {
        data1: 0xebd0a0a2,
        data2: 0xb9e5,
        data3: 0x4433,
        data4: [0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7],
    };
    entry.Anonymous.Gpt.PartitionType = guid;
    for attributes in [
        0,
        0x8000000000000000,
        1,
        0x1000000000000000,
        0x2000000000000000,
        0x4000000000000000,
    ] {
        entry.Anonymous.Gpt.Attributes = attributes;
        unsafe {
            bytes
                .as_mut_ptr()
                .add(HEADER)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(entry)
        };
        assert_eq!(
            check_native_data_target(&bytes, &selected).is_ok(),
            attributes == 0 || attributes == 0x8000000000000000
        );
    }
    entry.Anonymous.Gpt.Attributes = 0;
    entry.Anonymous.Gpt.PartitionType.data1 = 0;
    unsafe {
        bytes
            .as_mut_ptr()
            .add(HEADER)
            .cast::<PARTITION_INFORMATION_EX>()
            .write_unaligned(entry)
    };
    assert!(check_native_data_target(&bytes, &selected).is_err());
}
pub(crate) fn check_native_data_target(b: &[u8], selected: &Partition) -> Result<(), String> {
    check_native_target(b, selected)?;
    let matching = (0..u32_at(b, 4) as usize)
        .map(|i| entry(b, i))
        .find(|p| p.PartitionNumber == selected.partition_number)
        .ok_or("原生分区不存在")?;
    let gpt = unsafe { matching.Anonymous.Gpt };
    // Required, read-only, shadow-copy and hidden attributes are never writable.
    if guid(gpt.PartitionType) != policy::BASIC_DATA || gpt.Attributes & 0x7000000000000001 != 0 {
        return Err("扇区写入只支持无保护属性的普通 GPT 数据分区。".into());
    }
    Ok(())
}
pub(crate) fn layout_fingerprint(b: &[u8], new_size: Option<(u32, u64)>) -> Vec<String> {
    // Compare fields, excluding API rewrite flags and struct padding. Every
    // partition GUID/type/name/attribute and the disk GUID must be preserved.
    let gpt = unsafe {
        b.as_ptr()
            .add(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous))
            .cast::<DRIVE_LAYOUT_INFORMATION_GPT>()
            .read_unaligned()
    };
    let mut key = vec![format!(
        "{}:{}:{}:{}",
        guid(gpt.DiskId),
        gpt.StartingUsableOffset,
        gpt.UsableLength,
        gpt.MaxPartitionCount
    )];
    for i in 0..u32_at(b, 4) as usize {
        let p = entry(b, i);
        let g = unsafe { p.Anonymous.Gpt };
        let size = new_size
            .filter(|(n, _)| *n == p.PartitionNumber)
            .map(|(_, size)| size as i64)
            .unwrap_or(p.PartitionLength);
        key.push(format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{:?}",
            p.PartitionStyle,
            p.StartingOffset,
            size,
            p.PartitionNumber,
            p.IsServicePartition,
            guid(g.PartitionType),
            guid(g.PartitionId),
            g.Attributes,
            g.Name
        ));
    }
    key
}
pub(crate) fn native_layout(file: &File, expected: &[&Partition]) -> Result<Vec<u8>, String> {
    let b = read_native_layout(file)?;
    let count = u32_at(&b, 4) as usize;
    let active: Vec<_> = (0..count)
        .map(|i| entry(&b, i))
        .filter(|p| p.PartitionLength != 0)
        .collect();
    if active.len() != expected.len() {
        return Err("原生分区数量与 Windows 元数据不一致。".into());
    }
    for p in active {
        if p.PartitionStyle != PARTITION_STYLE_GPT
            || !expected.iter().any(|e| {
                let gpt = unsafe { p.Anonymous.Gpt };
                p.PartitionNumber == e.partition_number
                    && p.StartingOffset as u64 == e.offset
                    && p.PartitionLength as u64 == e.size
                    && e.guid.as_deref().map(policy::guid) == Some(guid(gpt.PartitionId))
                    && e.gpt_type.as_deref().map(policy::guid) == Some(guid(gpt.PartitionType))
            })
        {
            return Err("原生分区边界、类型或身份与选择不一致。".into());
        }
    }
    Ok(b)
}
fn read_native_layout(file: &File) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; HEADER + 128 * ENTRY];
    let n = device::control(file, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut b)?;
    if n < HEADER || u32_at(&b, 0) != PARTITION_STYLE_GPT as u32 {
        return Err("设备句柄未报告 GPT 布局。".into());
    }
    let count = u32_at(&b, 4) as usize;
    if count > 128 || n < HEADER + count * ENTRY {
        return Err("原生分区布局不完整。".into());
    }
    b.truncate(HEADER + count * ENTRY);
    Ok(b)
}
pub(crate) fn restore_native(
    file: &File,
    expected: Option<&[&Partition]>,
    sector: u64,
) -> Result<(crate::table_restore::Layout, Vec<String>, Vec<String>), String> {
    let bytes = match expected {
        Some(parts) => native_layout(file, parts)?,
        None => read_native_layout(file)?,
    };
    let layout = restore_model(&bytes, sector)?;
    let protected = (0..u32_at(&bytes, 4) as usize)
        .map(|i| entry(&bytes, i))
        .filter(|p| p.PartitionLength != 0 && p.IsServicePartition)
        .map(|p| guid(unsafe { p.Anonymous.Gpt.PartitionId }))
        .collect();
    Ok((layout, protected, layout_fingerprint(&bytes, None)))
}
fn restore_model(bytes: &[u8], sector: u64) -> Result<crate::table_restore::Layout, String> {
    use crate::table_restore::{Entry, Layout};
    let gpt = unsafe {
        bytes
            .as_ptr()
            .add(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous))
            .cast::<DRIVE_LAYOUT_INFORMATION_GPT>()
            .read_unaligned()
    };
    if !matches!(sector, 512 | 4096)
        || gpt.StartingUsableOffset <= 0
        || gpt.UsableLength <= 0
        || gpt.StartingUsableOffset as u64 % sector != 0
        || gpt.UsableLength as u64 % sector != 0
        || gpt.MaxPartitionCount == 0
        || gpt.MaxPartitionCount > 128
        || guid(gpt.DiskId) == uuid::Uuid::nil().to_string()
    {
        return Err("原生 GPT 几何或磁盘 GUID 无效。".into());
    }
    let first = gpt.StartingUsableOffset as u64 / sector;
    let last = first
        .checked_add(gpt.UsableLength as u64 / sector)
        .and_then(|n| n.checked_sub(1))
        .ok_or("原生 GPT 几何溢出")?;
    let mut entries = vec![];
    let mut ids = std::collections::HashSet::new();
    let mut numbers = std::collections::HashSet::new();
    for i in 0..u32_at(bytes, 4) as usize {
        let p = entry(bytes, i);
        if p.PartitionLength == 0 {
            continue;
        }
        let g = unsafe { p.Anonymous.Gpt };
        let id = guid(g.PartitionId);
        if p.PartitionStyle != PARTITION_STYLE_GPT
            || p.StartingOffset < 0
            || p.PartitionLength < 0
            || p.StartingOffset as u64 % sector != 0
            || p.PartitionLength as u64 % sector != 0
            || p.PartitionNumber == 0
            || !numbers.insert(p.PartitionNumber)
            || !ids.insert(id.clone())
            || id == uuid::Uuid::nil().to_string()
        {
            return Err("原生 GPT 分区字段缺失或不唯一。".into());
        }
        entries.push(Entry {
            slot: p.PartitionNumber,
            guid: id,
            kind: guid(g.PartitionType),
            offset: p.StartingOffset as u64,
            size: p.PartitionLength as u64,
            attributes: g.Attributes,
            name: g.Name.to_vec(),
        });
    }
    Ok(Layout {
        guid: guid(gpt.DiskId),
        first,
        last,
        count: gpt.MaxPartitionCount,
        entries,
    })
}
fn resize_layout(
    file: &File,
    original: &[u8],
    partition: &Partition,
    size: u64,
) -> Result<(), String> {
    let mut next = original.to_vec();
    let count = u32_at(&next, 4) as usize;
    let mut found = 0;
    for i in 0..count {
        let mut p = entry(&next, i);
        p.RewritePartition = false;
        if p.PartitionNumber == partition.partition_number {
            let gpt = unsafe { p.Anonymous.Gpt };
            if p.IsServicePartition || gpt.Attributes & 0x1000000000000001 != 0 {
                return Err("原生 GPT 分区受到保护。".into());
            }
            p.PartitionLength = i64::try_from(size).map_err(error)?;
            p.RewritePartition = true;
            found += 1;
        }
        unsafe {
            next.as_mut_ptr()
                .add(HEADER + i * ENTRY)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(p);
        }
    }
    if found != 1 {
        return Err("原生目标分区不唯一。".into());
    }
    device::control(file, IOCTL_DISK_SET_DRIVE_LAYOUT_EX, &next, &mut [])?;
    file.sync_all().map_err(error)
}
pub fn execute(
    request: &Request,
    disk: &Disk,
    parts: &[&Partition],
    p: &Partition,
    proof: &str,
) -> Result<Outcome, String> {
    let r = resolved(request, disk, parts)?;
    let engine = Engine::new()?;
    let _locks = device::lock(&r)?;
    device::verify_locks(&_locks, &r, parts)?;
    // WMI's provider needs to open the disk itself. Complete its fresh metadata
    // validation while volumes are locked, before denying other write handles.
    let fresh = resolved(request, disk, parts)?;
    // Locked volumes can disappear from WMI associations. Their retained
    // handles must still describe the original, exact partition extents.
    if fresh.disk.number != disk.number
        || fresh
            .volumes
            .iter()
            .any(|v| !r.volumes.iter().any(|old| old.eq_ignore_ascii_case(v)))
    {
        return Err("锁定期间磁盘身份或卷关联发生变化。".into());
    }
    device::verify_locks(&_locks, &r, parts)?;
    let file = device::exclusive_device(&fresh)?;
    let layout = native_layout(&file, parts)?;
    check_native_target(&layout, p)?;
    let before = super_bytes(&file, p.offset, p.size)?;
    if format!("{:x}", Sha256::digest(&before)) != proof {
        return Err("文件系统元数据与预览不同，请重新预览。".into());
    }
    let sector = disk.logical_sector_size.ok_or("扇区大小未知")?;
    let old = if matches!(request.action, Action::ExtResize { .. }) {
        Some(parse_super(&before, p.size)?)
    } else {
        None
    };
    device::offline(&file)?;
    let work = || -> Result<(), String> {
        if layout_fingerprint(&native_layout(&file, parts)?, None)
            != layout_fingerprint(&layout, None)
        {
            return Err("脱机后分区布局发生变化。".into());
        }
        let target_size = match &request.action {
            Action::ExtFormat {
                label,
                file_system,
                block_size,
                ..
            } => {
                let label_hex = label
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                engine.run(
                    "mke2fs.exe",
                    &[
                        "-q",
                        "-F",
                        "-t",
                        file_system.name(),
                        "-b",
                        &block_size.to_string(),
                        "-I",
                        "256",
                        "-L",
                        &label_hex,
                        "-E",
                        "lazy_itable_init=0,lazy_journal_init=0,nodiscard",
                        "diskstudio",
                    ],
                    &file,
                    p.offset,
                    p.size,
                    sector,
                    true,
                )?;
                p.size
            }
            Action::ExtResize { size, .. } => {
                engine.run(
                    "e2fsck.exe",
                    &["-f", "-n", "diskstudio"],
                    &file,
                    p.offset,
                    p.size,
                    sector,
                    false,
                )?;
                // Only a clean checked FS reaches this noninteractive pass.
                engine.run(
                    "e2fsck.exe",
                    &["-f", "-p", "diskstudio"],
                    &file,
                    p.offset,
                    p.size,
                    sector,
                    true,
                )?;
                if *size > p.size {
                    resize_layout(&file, &layout, p, *size)?;
                }
                engine.run(
                    "resize2fs.exe",
                    &[
                        "diskstudio",
                        &(*size / old.as_ref().ok_or("缺少 Ext 元数据")?.block_size).to_string(),
                    ],
                    &file,
                    p.offset,
                    p.size.max(*size),
                    sector,
                    true,
                )?;
                engine.run(
                    "e2fsck.exe",
                    &["-f", "-n", "diskstudio"],
                    &file,
                    p.offset,
                    *size,
                    sector,
                    false,
                )?;
                if *size < p.size {
                    resize_layout(&file, &layout, p, *size)?;
                }
                *size
            }
            _ => return Err("不匹配的 Ext 操作".into()),
        };
        file.sync_all().map_err(error)?;
        let after = parse_super(&super_bytes(&file, p.offset, target_size)?, target_size)?;
        if after.blocks * after.block_size != target_size
            || old.as_ref().is_some_and(|old| {
                old.uuid != after.uuid
                    || old.label != after.label
                    || old.block_size != after.block_size
                    || old.file_system != after.file_system
            })
        {
            return Err("Ext 容量、UUID 或卷标后验核对失败。".into());
        }
        if let Action::ExtFormat {
            label,
            file_system,
            block_size,
            ..
        } = &request.action
        {
            if after.label != label.as_bytes()
                || after.file_system != *file_system
                || after.block_size != u64::from(*block_size)
            {
                return Err("Ext 文件系统类型、块大小或卷标核对失败。".into());
            }
        }
        engine.run(
            "e2fsck.exe",
            &["-f", "-n", "diskstudio"],
            &file,
            p.offset,
            target_size,
            sector,
            false,
        )?;
        let mut updated: Vec<_> = parts.iter().map(|p| (*p).clone()).collect();
        for e in &mut updated {
            if e.partition_number == p.partition_number {
                e.size = target_size;
            }
        }
        let after_layout = native_layout(&file, &updated.iter().collect::<Vec<_>>())?;
        if layout_fingerprint(&after_layout, None)
            != layout_fingerprint(&layout, Some((p.partition_number, target_size)))
        {
            return Err("分区表其他字段发生变化，结果未通过完整核对。".into());
        }
        let mut state = [0; 16];
        if device::control(&file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut state)? < 16
            || u64::from_le_bytes(state[8..16].try_into().unwrap()) & 1 == 0
        {
            return Err("无法确认磁盘保持脱机。".into());
        }
        Ok(())
    };
    work().map_err(|e| format!("{e} 磁盘已保持脱机；操作可能部分完成。请核对分区与文件系统状态，不要直接重试；不会自动回滚。"))?;
    Ok(Outcome {
        verified: true,
        message: format!(
            "{}已完成，文件系统检查、容量和分区边界核对通过。磁盘保持脱机，可在 Linux 中挂载使用。",
            request.action.title()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    #[test]
    fn native_gpt_protection_and_unrelated_fields_are_never_ignored() {
        let mut b = vec![0u8; HEADER + ENTRY];
        b[0..4].copy_from_slice(&(PARTITION_STYLE_GPT as u32).to_le_bytes());
        b[4..8].copy_from_slice(&1u32.to_le_bytes());
        let mut p = PARTITION_INFORMATION_EX::default();
        p.PartitionStyle = PARTITION_STYLE_GPT;
        p.PartitionNumber = 7;
        p.StartingOffset = policy::MIB as i64;
        p.PartitionLength = 128 * policy::MIB as i64;
        unsafe {
            b.as_mut_ptr()
                .add(HEADER)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(p);
        }
        let selected = Partition {
            partition_number: 7,
            ..Default::default()
        };
        assert!(check_native_target(&b, &selected).is_ok());
        let original = layout_fingerprint(&b, None);
        p.RewritePartition = true;
        unsafe {
            b.as_mut_ptr()
                .add(HEADER)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(p);
        }
        assert_eq!(layout_fingerprint(&b, None), original);
        let mut g = unsafe { p.Anonymous.Gpt };
        g.Attributes = 0x1000000000000000;
        p.Anonymous.Gpt = g;
        unsafe {
            b.as_mut_ptr()
                .add(HEADER)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(p);
        }
        assert!(check_native_target(&b, &selected).is_err());
        assert_ne!(layout_fingerprint(&b, None), original);
        g.Attributes = 0;
        g.Name[0] = 65;
        p.Anonymous.Gpt = g;
        unsafe {
            b.as_mut_ptr()
                .add(HEADER)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(p);
        }
        assert_ne!(layout_fingerprint(&b, None), original);
        assert!(check_native_target(
            &b,
            &Partition {
                partition_number: 8,
                ..Default::default()
            }
        )
        .is_err());
    }
    #[test]
    fn embedded_engine_formats_and_resizes_all_ext_types_and_block_sizes() {
        for fs in [
            ExtFileSystem::Ext2,
            ExtFileSystem::Ext3,
            ExtFileSystem::Ext4,
        ] {
            for (block, sector) in [(1024, 512), (2048, 512), (4096, 512), (4096, 4096)] {
                format_resize_file(fs, block, sector);
            }
        }
    }
    fn format_resize_file(fs: ExtFileSystem, block: u64, sector: u64) {
        let engine = Engine::new().unwrap();
        let path = engine.directory.join("synthetic-test.img");
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mib = policy::MIB;
        let max = 192 * mib;
        let guard = vec![0xa7; mib as usize];
        file.set_len(max + 2 * mib).unwrap();
        file.write_all(&guard).unwrap();
        file.seek(SeekFrom::Start(max + mib)).unwrap();
        file.write_all(&guard).unwrap();
        engine
            .run(
                "mke2fs.exe",
                &[
                    "-q",
                    "-F",
                    "-t",
                    fs.name(),
                    "-b",
                    &block.to_string(),
                    "-I",
                    "256",
                    "-L",
                    "4c696e7578e8b584e69699",
                    "-E",
                    "lazy_itable_init=0,lazy_journal_init=0,nodiscard",
                    "diskstudio",
                ],
                &file,
                mib,
                128 * mib,
                sector,
                true,
            )
            .unwrap();
        let original =
            parse_super(&super_bytes(&file, mib, 128 * mib).unwrap(), 128 * mib).unwrap();
        assert_eq!(original.label, "Linux资料".as_bytes());
        assert_eq!(original.file_system, fs);
        assert_eq!(original.block_size, block);
        for (old, new) in [(128 * mib, 96 * mib), (96 * mib, max)] {
            engine
                .run(
                    "e2fsck.exe",
                    &["-f", "-n", "diskstudio"],
                    &file,
                    mib,
                    old,
                    sector,
                    false,
                )
                .unwrap();
            engine
                .run(
                    "e2fsck.exe",
                    &["-f", "-p", "diskstudio"],
                    &file,
                    mib,
                    old,
                    sector,
                    true,
                )
                .unwrap();
            engine
                .run(
                    "resize2fs.exe",
                    &["diskstudio", &(new / block).to_string()],
                    &file,
                    mib,
                    old.max(new),
                    sector,
                    true,
                )
                .unwrap();
            engine
                .run(
                    "e2fsck.exe",
                    &["-f", "-n", "diskstudio"],
                    &file,
                    mib,
                    new,
                    sector,
                    false,
                )
                .unwrap();
            let after = parse_super(&super_bytes(&file, mib, new).unwrap(), new).unwrap();
            assert_eq!(after.blocks * block, new);
            assert_eq!(after.uuid, original.uuid);
            assert_eq!(after.label, original.label);
            assert_eq!(after.file_system, fs);
            assert_eq!(after.block_size, block);
        }
        let mut check = vec![0; guard.len()];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut check).unwrap();
        assert_eq!(check, guard);
        file.seek(SeekFrom::Start(max + mib)).unwrap();
        file.read_exact(&mut check).unwrap();
        assert_eq!(check, guard);
        assert_eq!(file.metadata().unwrap().len(), max + 2 * mib);
        drop(file);
        fs::remove_file(path).unwrap();
    }
    fn sample() -> Vec<u8> {
        let mut b = vec![0; 1024];
        b[56..58].copy_from_slice(&0xef53u16.to_le_bytes());
        b[58..60].copy_from_slice(&1u16.to_le_bytes());
        for (at, n) in [
            (4, 32768u32),
            (24, 2),
            (92, 0x3c),
            (96, 0x2c2),
            (100, 0x46b),
        ] {
            b[at..at + 4].copy_from_slice(&n.to_le_bytes());
        }
        b[104] = 1;
        b
    }
    #[test]
    fn ext4_rejects_dirty_unsupported_and_overlong_filesystems() {
        let valid = sample();
        assert!(parse_super(&valid, 128 * policy::MIB).is_ok());
        for (at, n) in [
            (58, 2u32),
            (96, 0x2c6),
            (96, 0x102c2),
            (100, 0x66b),
            (232, 1),
            (24, 3),
        ] {
            let mut b = valid.clone();
            b[at..at + 4].copy_from_slice(&n.to_le_bytes());
            assert!(parse_super(&b, 128 * policy::MIB).is_err());
        }
        assert!(parse_super(&valid, 64 * policy::MIB).is_err());
        assert!(parse_super(&[], 0).is_err());
    }
}

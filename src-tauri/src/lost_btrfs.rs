//! Checksummed Btrfs member discovery using a bounded, hashed evidence stream.
use super::{canceled, Candidate, Probe, Range};
use disk_core::btrfs::{self, RECOVERY_SUPERBLOCK_BYTES};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};

pub(super) const BUDGET: u64 = 128 * 1024 * 1024;
const BUDGET_ERROR: &str = "Btrfs 候选元数据超过搜索预算，请缩小范围。";

struct Evidence<'a, 'source, R> {
    probe: &'a mut Probe<'source, R>,
    available: &'a mut u64,
    position: u64,
    failed: Option<String>,
}

impl<R: Read + Seek> Read for Evidence<'_, '_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let len = (self.probe.size - self.position).min(out.len() as u64);
        if len == 0 {
            return Ok(0);
        }
        let Some(remaining) = self.available.checked_sub(len) else {
            self.failed = Some(BUDGET_ERROR.into());
            return Err(io::Error::other(BUDGET_ERROR));
        };
        *self.available = remaining;
        if let Err(error) = self
            .probe
            .read_exact(self.position, &mut out[..len as usize])
        {
            self.failed = Some(error.clone());
            return Err(io::Error::other(error));
        }
        self.position += len;
        Ok(len as usize)
    }
}

impl<R> Seek for Evidence<'_, '_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.probe.size) + i128::from(delta),
        };
        if !(0..=i128::from(self.probe.size)).contains(&position) {
            return Err(io::Error::other("Btrfs evidence seek outside member"));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

pub(super) fn probe<R: Read + Seek>(
    reader: &mut R,
    range: Range,
    start: u64,
    anchor: u64,
    cancel: &AtomicBool,
    available: &mut u64,
) -> Result<Option<Candidate>, String> {
    let Some(size) = range
        .offset
        .checked_add(range.size)
        .filter(|&end| end <= i64::MAX as u64)
        .and_then(|end| end.checked_sub(start))
    else {
        return Err("Btrfs 搜索范围无效。".into());
    };
    if !matches!(range.sector, 512 | 4096)
        || start < range.offset
        || start % range.sector != 0
        || anchor
            .checked_add(RECOVERY_SUPERBLOCK_BYTES as u64)
            .is_none_or(|end| end > size)
    {
        return Ok(None);
    }
    let mut probe = Probe {
        reader,
        start,
        size,
        hash: Sha256::new(),
        cancel,
    };
    let mut bytes = [0; RECOVERY_SUPERBLOCK_BYTES];
    *available = available
        .checked_sub(bytes.len() as u64)
        .ok_or(BUDGET_ERROR)?;
    probe.read_exact(anchor, &mut bytes)?;
    let Ok(size) = btrfs::recovery_member_size(&bytes, anchor, size, range.sector) else {
        return Ok(None);
    };
    if !probe.limit(size, range.sector) {
        return Ok(None);
    }
    let mut evidence = Evidence {
        probe: &mut probe,
        available,
        position: 0,
        failed: None,
    };
    let member = btrfs::inspect_recovery_member(&mut evidence, || canceled(cancel));
    if let Some(error) = evidence.failed.take() {
        return Err(error);
    }
    canceled(cancel)?;
    let Ok(member) = member else {
        return Ok(None);
    };
    if member.size != size {
        return Ok(None);
    }
    let checked = (|| -> Result<_, String> {
        let mut volume = btrfs::open_recovered(&mut evidence, || canceled(cancel))?;
        let root = volume
            .inventory()
            .subvolumes
            .first()
            .ok_or("Btrfs 缺少顶层目录。")?;
        if volume.identity() != member.identity
            || volume
                .inode(root.id, root.root_directory, || canceled(cancel))?
                .0
                .mode()
                & 0xf000
                != 0x4000
        {
            return Err("Btrfs 成员容量或顶层目录无效。".into());
        }
        let primary = volume.has_current_primary()?;
        let missing = volume.missing_members();
        volume.verify_view(|| canceled(cancel))?;
        Ok((primary, missing != 0))
    })();
    if let Some(error) = evidence.failed {
        return Err(error);
    }
    canceled(cancel)?;
    if checked.is_err() && member.total_members == 1 {
        return Ok(None);
    }
    let warning = match checked {
        Ok((false, _)) => "主超级块未通过分区登记检查。可浏览和提取通过校验的数据。",
        Ok((true, true)) => {
            "该范围是多设备 Btrfs 的一个成员。缺少其他成员时，只能提取仍可通过校验的数据。"
        }
        Ok((true, false)) => "容量来自 Btrfs 设备记录。原分区末端可能位于文件系统边界之后。",
        Err(_) => "单个成员无法验证目录。请选择同一提交的其他成员，联合核对卷和文件。",
    };
    let evidence = if checked.is_ok() {
        "Btrfs 超级块、块组映射、设备范围与顶层目录已校验。"
    } else {
        "Btrfs 超级块和系统条带范围已校验，目录需联合核对。"
    };
    Ok(Some(Candidate {
        id: uuid::Uuid::new_v4().to_string(),
        filesystem: "Btrfs".into(),
        offset: start,
        size,
        evidence: evidence.into(),
        warning: warning.into(),
        can_restore: matches!(checked, Ok((true, _))),
        can_browse: checked.is_ok(),
        btrfs_member_count: Some(member.total_members),
        fingerprint: format!("{:x}", probe.hash.finalize()),
        btrfs_anchor: Some(anchor),
        btrfs_member: Some(member),
    }))
}

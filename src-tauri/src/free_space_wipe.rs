//! Free-space wiping on an already locked, bounded partition. No path/device
//! opening here. The caller retains the locks and keeps the disk offline.
use crate::{maintenance_io as engine, surface_scan::Range, used_space};
use serde::Serialize;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub filesystem: String,
    pub free_bytes: u64,
    pub preserved_bytes: u64,
    pub allocation_sha256: String,
    pub preserved_sha256: Option<String>,
}

pub fn summary(plan: &used_space::Plan) -> Result<Summary, String> {
    let volume = plan.volumes.first().ok_or("缺少文件系统分配信息")?;
    if plan.volumes.len() != 1
        || volume.offset != 0
        || volume.size != plan.size
        || !matches!(
            volume.filesystem.as_str(),
            "NTFS" | "FAT12" | "FAT16" | "FAT32" | "exFAT"
        )
    {
        return Err("清除空闲空间目前支持完整的普通 NTFS、FAT12/16/32、exFAT 数据分区。未知格式与 Ext 不执行清除。".into());
    }
    if plan.skipped == 0 {
        return Err("未找到可确认的空闲簇，未发送任何清除写入。".into());
    }
    Ok(Summary {
        filesystem: volume.filesystem.clone(),
        free_bytes: plan.skipped,
        preserved_bytes: plan.copied(),
        allocation_sha256: plan.digest.clone(),
        preserved_sha256: None,
    })
}

pub enum Progress<'a> {
    Preserve { after: bool, done: u64, total: u64 },
    Wipe(&'a engine::WipeProgress),
}

pub fn execute<R: Read + Write + Seek>(
    io: &mut R,
    plan: &used_space::Plan,
    options: &engine::WipeOptions,
    cancel: &AtomicBool,
    progress: &mut engine::WipeProgress,
    random: impl FnMut(&mut [u8]) -> Result<(), String>,
    sync: impl FnMut(&mut R) -> std::io::Result<()>,
    mut publish: impl FnMut(Progress<'_>),
) -> Result<Summary, String> {
    let mut summary = summary(plan)?;
    if io.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? != plan.size {
        return Err("空闲空间清除与已验证分区容量不一致".into());
    }
    options.validate(Range {
        offset: 0,
        size: plan.size,
        sector: plan.sector,
    })?;
    plan.proof.verify(io, cancel)?;
    let before = plan.retained_digest(io, cancel, |done, total| {
        publish(Progress::Preserve {
            after: false,
            done,
            total,
        })
    })?;
    plan.proof.verify(io, cancel)?;
    let ranges = plan
        .free_ranges()
        .iter()
        .map(|s| Range {
            offset: s.start,
            size: s.size(),
            sector: plan.sector,
        })
        .collect::<Vec<_>>();
    engine::wipe_ranges(io, &ranges, options, cancel, progress, random, sync, |p| {
        publish(Progress::Wipe(p))
    })?;
    plan.proof.verify(io, cancel)?;
    let after = plan.retained_digest(io, cancel, |done, total| {
        publish(Progress::Preserve {
            after: true,
            done,
            total,
        })
    })?;
    if before != after {
        return Err("空闲空间已写入，但保留区域 SHA-256 与写前不一致，不能视为成功。".into());
    }
    plan.proof.verify(io, cancel)?;
    summary.preserved_sha256 = Some(after);
    Ok(summary)
}

#[cfg(test)]
#[path = "free_space_wipe_tests.rs"]
mod tests;

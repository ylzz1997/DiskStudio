//! NTFS allocation maps over a locked, audited, bounded stream.
use crate::used_space::{stop, Allocation};
use ntfs::{attribute_value::NtfsAttributeValue, Ntfs, NtfsAttributeType, NtfsFileFlags};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
// Persistent short-name/tunneling policy; it is not a dirty or repair flag.
// On-disk definition: https://dfir.ru/2019/01/19/ntfs-today/
// Keep every other state bit rejected, including dirty + this policy bit.
const SHORT_NAMES_DISABLED: u16 = 0x0080;
fn at(r: &mut (impl Read + Seek), offset: u64, n: usize) -> Result<Vec<u8>, String> {
    r.seek(SeekFrom::Start(offset)).map_err(err)?;
    let mut b = vec![0; n];
    r.read_exact(&mut b).map_err(err)?;
    Ok(b)
}
fn retain_runs(value: &NtfsAttributeValue<'_, '_>, map: &mut Allocation) -> Result<(), String> {
    match value {
        NtfsAttributeValue::NonResident(v) => {
            let mut count = 0;
            for run in v.data_runs() {
                count += 1;
                if count > 131072 {
                    return Err("NTFS 元数据数据段超过分析预算。".into());
                }
                let run = run.map_err(err)?;
                let start = run
                    .data_position()
                    .value()
                    .ok_or("NTFS 分配元数据不能包含稀疏段")?
                    .get();
                map.require(start, run.allocated_size())?;
            }
            if count == 0 {
                return Err("NTFS 元数据没有有效数据段".into());
            }
        }
        NtfsAttributeValue::Resident(_) => {}
        NtfsAttributeValue::AttributeListNonResident(_) => {
            return Err("跨属性列表的 NTFS 分配元数据暂不能按已用空间处理。".into())
        }
    }
    Ok(())
}
pub fn inspect(r: &mut (impl Read + Seek), cancel: &AtomicBool) -> Result<Allocation, String> {
    let size = r.seek(SeekFrom::End(0)).map_err(err)?;
    let boot = at(r, 0, 512)?;
    let fs = Ntfs::new(r).map_err(err)?;
    let sector = fs.sector_size() as u64;
    let cluster = fs.cluster_size() as u64;
    if !matches!(sector, 512 | 4096)
        || cluster < sector
        || cluster > 2 * 1024 * 1024
        || !cluster.is_power_of_two()
        || fs.size() < 16 * cluster
        || fs.size().checked_add(sector).is_none_or(|n| n > size)
    {
        return Err("NTFS 容量、簇或备份引导位置无效。".into());
    }
    if at(r, fs.size(), 512)? != boot {
        return Err("NTFS 主备引导扇区不一致。".into());
    }
    let volume = fs.file(r, 3).map_err(err)?;
    let mut attrs = volume.attributes();
    let mut found = false;
    while let Some(item) = attrs.next(r) {
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.ty().map_err(err)? != NtfsAttributeType::VolumeInformation {
            continue;
        }
        if found || !attr.is_resident() || attr.value_length() != 12 {
            return Err("NTFS 卷状态不唯一或无效".into());
        }
        found = true;
        let mut data = [0; 12];
        attr.value(r)
            .map_err(err)?
            .attach(r)
            .read_exact(&mut data)
            .map_err(err)?;
        let flags = u16at(&data, 10);
        if data[8] != 3 || data[9] != 1 || flags & !SHORT_NAMES_DISABLED != 0 {
            return Err(format!(
                "按已用空间处理要求干净的 NTFS 3.1 卷，当前版本 {}.{}、状态 0x{flags:04X} 有待处理或不受支持。",
                data[8], data[9]
            ));
        }
    }
    if !found {
        return Err("NTFS 卷状态缺失。".into());
    }
    let mut map = Allocation::new("NTFS", size);
    map.require(0, cluster)?;
    map.require(fs.size(), sector)?;
    let mirror = u64at(&boot, 56)
        .checked_mul(cluster)
        .ok_or("NTFS MFT 镜像位置溢出")?;
    map.require(mirror, (fs.file_record_size() as u64 * 4).max(cluster))?;
    let mft = fs.file(r, 0).map_err(err)?;
    let mft_item = mft.data(r, "").ok_or("NTFS $MFT 数据缺失")?.map_err(err)?;
    retain_runs(
        &mft_item
            .to_attribute()
            .map_err(err)?
            .value(r)
            .map_err(err)?,
        &mut map,
    )?;
    let bitmap = fs.file(r, 6).map_err(err)?;
    if !bitmap.flags().contains(NtfsFileFlags::IN_USE)
        || bitmap.flags().contains(NtfsFileFlags::IS_DIRECTORY)
    {
        return Err("NTFS $Bitmap 记录状态无效。".into());
    }
    let item = bitmap
        .data(r, "")
        .ok_or("NTFS $Bitmap 数据缺失")?
        .map_err(err)?;
    let attr = item.to_attribute().map_err(err)?;
    let clusters = fs.size() / cluster;
    let needed = clusters.div_ceil(8);
    let raw = attr.record_bytes();
    if raw.len() < 24
        || u16at(raw, 12) != 0
        || needed > 64 * 1024 * 1024
        || attr.value_length() < needed
        || attr.value_length() > needed.div_ceil(8) * 8
    {
        return Err("NTFS 分配位图大小、压缩或加密状态不支持。".into());
    }
    if !attr.is_resident()
        && (raw.len() < 64 || u64at(raw, 16) != 0 || u16at(raw, 34) != 0 || u64at(raw, 56) < needed)
    {
        return Err("NTFS 分配位图没有完整初始化或不是完整的首段。".into());
    }
    let value = attr.value(r).map_err(err)?;
    retain_runs(&value, &mut map)?;
    let mut bytes = vec![0; needed as usize];
    value.attach(r).read_exact(&mut bytes).map_err(err)?;
    for c in 0..clusters {
        if c % 65536 == 0 {
            stop(cancel)?;
        }
        if bytes[c as usize / 8] & (1 << (c % 8)) == 0 {
            map.free(c * cluster, cluster)?;
        }
    }
    map.validate()?;
    Ok(map)
}

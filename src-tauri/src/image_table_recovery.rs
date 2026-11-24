//! Archive and restore table sectors from ordinary logical-image bytes.
//! The binding commits to the full healthy image, with archived table bytes
//! substituted for the current table sectors when validating a damaged source.
use super::{
    table_archive::{self, Archive, Kind, IMAGE_VERSION},
    table_restore, u32le, Conversion, PartitionChange, Preview, Scheme, MIB,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

fn binding(
    reader: &mut (impl Read + Seek),
    archive: &Archive,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<String, String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; MIB as usize];
    let mut position = 0;
    let tables = archive.extents.iter().filter(|e| e.kind != Kind::Boot);
    for table in tables.map(Some).chain(std::iter::once(None)) {
        let end = table.map_or(archive.size, |e| e.offset);
        if position < end {
            reader
                .seek(SeekFrom::Start(position))
                .map_err(|e| e.to_string())?;
            while position < end {
                tick(position)?;
                let count = (end - position).min(MIB) as usize;
                reader
                    .read_exact(&mut buffer[..count])
                    .map_err(|e| e.to_string())?;
                hash.update(&buffer[..count]);
                position += count as u64;
            }
        }
        if let Some(table) = table {
            hash.update(&table.bytes);
            position += table.bytes.len() as u64;
        }
    }
    tick(archive.size)?;
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn backup(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Archive, String> {
    let mut archive =
        table_archive::capture(reader, size, sector, "0".repeat(64), true, || tick(0))?;
    archive.source = binding(reader, &archive, &mut tick)?;
    archive.version = IMAGE_VERSION;
    archive.matches(reader, || tick(size))?;
    Ok(archive)
}

pub(crate) fn verify_binding(
    reader: &mut (impl Read + Seek),
    archive: &Archive,
    tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<(), String> {
    if binding(reader, archive, tick)? != archive.source {
        return Err("镜像在备份表扇区之外的数据与备份不一致，未生成恢复副本。".into());
    }
    Ok(())
}

pub(crate) fn prepare(
    reader: &mut (impl Read + Seek),
    size: u64,
    sector: u64,
    mut archive: Archive,
    archive_name: String,
) -> Result<Conversion, String> {
    if archive.version != IMAGE_VERSION || archive.size != size || archive.sector != sector {
        return Err("请选择与当前容量和逻辑扇区一致的镜像分区表备份。".into());
    }
    let info = archive.validate()?;
    let (scheme, disk_id) = if info.scheme == "GPT" {
        (Scheme::Gpt, table_restore::archive_layout(&archive)?.guid)
    } else {
        let signature = u32le(&archive.extents[0].bytes[440..444]);
        (Scheme::Mbr, format!("{signature:08X}"))
    };
    let mut retained = Vec::new();
    archive.extents.retain(|extent| {
        let keep = extent.kind != Kind::Boot;
        if !keep {
            retained.push((extent.offset, extent.bytes.clone()));
        }
        keep
    });
    // The output parser emits a conventional table-only archive for comparison.
    archive.version = 1;
    archive.includes_boot = false;
    Conversion::replace_table(
        reader,
        retained,
        archive,
        Preview {
            from: scheme,
            to: scheme,
            sector,
            disk_id,
            partitions: info
                .layout
                .into_iter()
                .map(|p| PartitionChange {
                    offset: p.offset,
                    size: p.size,
                    old_type: String::new(),
                    new_type: p.name,
                    logical: false,
                })
                .collect(),
            table_bytes: 0,
            placement: None,
            restored_from: Some(archive_name),
            gpt_repair: None,
            registration: None,
        },
    )
}

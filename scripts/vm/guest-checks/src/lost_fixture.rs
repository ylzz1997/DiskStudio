//! Only used inside the isolated Windows VM. Seeds a formerly unregistered
//! filesystem into a verified empty B extent; no existing partition is removed.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
const AT: u64 = 14 * GIB;
const BYTES: &[u8] = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");
fn read(file: &mut std::fs::File, at: u64, len: usize) -> Result<Vec<u8>, String> {
    if at.checked_add(len as u64).is_none_or(|end| end > 16 * GIB) || len > 2 * 1024 * 1024 {
        return Err("STOP: invalid fixture read".into());
    }
    let mut b = vec![0; len];
    file.seek(SeekFrom::Start(at))
        .and_then(|_| file.read_exact(&mut b))
        .map_err(|e| format!("fixture read at {at}, {len} bytes: {e}"))?;
    Ok(b)
}
fn context(expected: u32) -> Result<(Selection, partitions::platform::TableReadContext), String> {
    let disks = inventory()?;
    let b = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(B))
        .ok_or("B absent")?;
    let a = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(A))
        .ok_or("A absent")?;
    if b.number != 2
        || a.number != 1
        || a.number_of_partitions != Some(19)
        || b.number_of_partitions != Some(expected)
        || b.logical_sector_size != Some(512)
        || b.is_read_only != Some(false)
        || b.is_clustered != Some(false)
    {
        return Err("STOP: exact A/B fixture metadata mismatch".into());
    }
    let source = selection(B, None)?;
    let (ctx, _, _) = partitions::platform::table_restore_context(&source, None)?;
    Ok((source, ctx))
}
pub fn seed() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    let (source, ctx) = context(9)?;
    if ctx
        .parts
        .iter()
        .any(|p| p.offset < AT + BYTES.len() as u64 && AT < p.offset + p.size)
    {
        return Err("STOP: seed extent is not free".into());
    }
    let locks = data_windows::lock(&ctx.resolved)?;
    let (fresh, _, _) = partitions::platform::table_restore_context(&source, Some((&ctx, &locks)))?;
    if fresh.fingerprint != ctx.fingerprint {
        return Err("STOP: metadata changed while locking".into());
    }
    let parts = fresh.parts.iter().collect::<Vec<_>>();
    data_windows::verify_locks(&locks, &fresh.resolved, &parts)?;
    let mut file = data_windows::exclusive_device(&fresh.resolved)?;
    let before =
        table_archive::capture(&mut file, 16 * GIB, 512, "b".repeat(64), false, || Ok(()))?;
    let native = extfs::restore_native(&file, Some(&parts), 512)?.0;
    if !native.agrees(&table_restore::archive_layout(&before)?) {
        return Err("STOP: native/raw layout mismatch".into());
    }
    let old = read(&mut file, AT, BYTES.len())?;
    if old.iter().any(|b| *b != 0) {
        return Err("STOP: target bytes are not all zero; seed is never retried".into());
    }
    let head = read(&mut file, 0, 1024 * 1024)?;
    let tail = read(&mut file, 16 * GIB - 1024 * 1024, 1024 * 1024)?;
    data_windows::offline(&file)?;
    if !extfs::restore_native(&file, Some(&parts), 512)?
        .0
        .agrees(&native)
        || read(&mut file, AT, BYTES.len())? != old
    {
        return Err("STOP: state changed after offline; no fixture write".into());
    }
    file.seek(SeekFrom::Start(AT)).map_err(err)?;
    // The embedded byte array need not have device buffer alignment. Use the
    // same owned streaming buffer style as data jobs, with bounded requests.
    let payload = BYTES.to_vec();
    let mut n = 0;
    for chunk in payload.chunks(1024 * 1024) {
        let written = file.write(chunk).map_err(|e| {
            format!(
                "fixture write at {}, {} bytes: {e}; no retry",
                AT + n as u64,
                chunk.len()
            )
        })?;
        if written != chunk.len() {
            return Err(format!("short fixture write at {n}; no retry"));
        }
        n += written;
    }
    if n != BYTES.len() {
        return Err("STOP: short fixture write; disk remains offline, no retry".into());
    }
    file.sync_all().map_err(err)?;
    if read(&mut file, AT, BYTES.len())? != BYTES
        || read(&mut file, 0, head.len())? != head
        || read(&mut file, 16 * GIB - tail.len() as u64, tail.len())? != tail
    {
        return Err("STOP: fixture/table readback mismatch".into());
    }
    Ok(format!("PASS: B only, 2 MiB NTFS fixture at LBA {}. Full SHA-256 {:x}. Existing 9 GPT entries and table windows unchanged. B remains offline. A untouched.\n\nSearch the final unallocated range in the production application, preview the unregistered 2 MiB NTFS candidate, then restore its GPT record.",AT/512,Sha256::digest(BYTES)))
}
pub fn verify() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    let (_, ctx) = context(10)?;
    if ctx.resolved.disk.is_offline != Some(true) {
        return Err("STOP: B must remain offline".into());
    }
    let mut file = data_windows::device(&ctx.resolved, false)?;
    let table = table_archive::capture(&mut file, 16 * GIB, 512, "b".repeat(64), false, || Ok(()))?;
    let layout = table_restore::archive_layout(&table)?;
    if !layout
        .agrees(&extfs::restore_native(&file, Some(&ctx.parts.iter().collect::<Vec<_>>()), 512)?.0)
    {
        return Err("STOP: raw/native GPT disagree".into());
    }
    let found = layout
        .entries
        .iter()
        .filter(|e| {
            e.offset == AT
                && e.size == BYTES.len() as u64
                && e.kind == partitions::policy::BASIC_DATA
                && e.attributes == 0
        })
        .count();
    if found != 1 || read(&mut file, AT, BYTES.len())? != BYTES {
        return Err("STOP: recovered entry or complete filesystem bytes differ".into());
    }
    Ok(format!("PASS: production restore registered B's 2 MiB NTFS at LBA {}. 10 GPT entries, raw/native agreement, B offline. Full filesystem SHA-256 unchanged: {:x}. A still has 19 partitions. No data writes in this verification.",AT/512,Sha256::digest(BYTES)))
}
pub fn inspect_seed() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    let (_, ctx) = context(9)?;
    let mut file = data_windows::device(&ctx.resolved, false)?;
    let bytes = read(&mut file, AT, BYTES.len())?;
    let table = table_archive::capture(&mut file, 16 * GIB, 512, "b".repeat(64), false, || Ok(()))?;
    if !table_restore::archive_layout(&table)?
        .agrees(&extfs::restore_native(&file, Some(&ctx.parts.iter().collect::<Vec<_>>()), 512)?.0)
    {
        return Err("STOP: native/raw mismatch".into());
    }
    Ok(format!("READ-ONLY INSPECTION: B has 9 valid GPT records. Offline={:?}. Target {} bytes at LBA {}: allZero={}, equalsFixture={}. SHA-256 {:x}. No data writes or disk attribute changes.",ctx.resolved.disk.is_offline,bytes.len(),AT/512,bytes.iter().all(|b|*b==0),bytes==BYTES,Sha256::digest(&bytes)))
}

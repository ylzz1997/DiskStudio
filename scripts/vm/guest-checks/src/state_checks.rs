//! Independent read-only A/B GPT comparison for the production state UI.
use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_DISK_ATTRIBUTES;
const RECORD: &str = "C:\\ProgramData\\DiskStudio-device-state-baseline.json";
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    serial: String,
    partitions: u32,
    table_sha256: String,
}
fn snapshot(check_fixture: bool) -> Result<Vec<Record>, String> {
    let disks = inventory()?;
    let mut records = vec![];
    for (serial, number, count) in [(A, 1, 19), (B, 2, 10)] {
        let d = disks
            .iter()
            .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
            .ok_or("test disk missing")?;
        if d.number != number
            || d.number_of_partitions != Some(count)
            || d.is_offline != Some(number == 2)
            || d.is_read_only != Some(false)
            || d.logical_sector_size != Some(512)
        {
            return Err(format!("STOP: require A/19 online, B/10 offline, both writable and 512-byte sectors; disk {} count={:?}, offline={:?}, readonly={:?}, sector={:?}", d.number,d.number_of_partitions,d.is_offline,d.is_read_only,d.logical_sector_size));
        }
        let source = selection(serial, None)?;
        // This independent check only reads raw GPT and fixed fixture extents.
        // It must not enumerate/lock every filesystem volume: that validation
        // belongs to mutating jobs and can block on offline or damaged volumes.
        // Keep full inventory, cross-provider, layout, opened-handle identity
        // and raw/native GPT agreement checks for both isolated test disks.
        let ctx = partitions::platform::table_inspect_context(&source)?;
        let mut file = data_windows::device(&ctx.resolved, false)?;
        let mut attrs = [0; 16];
        if data_windows::control(&file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attrs)? != 16
            || u64::from_le_bytes(attrs[8..16].try_into().unwrap()) != u64::from(number == 2)
        {
            return Err("STOP: native attributes mismatch".into());
        }
        let table = table_archive::capture(
            &mut file,
            16 * GIB,
            512,
            if number == 1 { "a" } else { "b" }.repeat(64),
            false,
            || Ok(()),
        )?;
        if !table_restore::archive_layout(&table)?.agrees(
            &extfs::restore_native(&file, Some(&ctx.parts.iter().collect::<Vec<_>>()), 512)?.0,
        ) {
            return Err("STOP: raw/native GPT mismatch".into());
        }
        if number == 2 && check_fixture {
            let fixture = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");
            let mut bytes = vec![0; fixture.len()];
            file.seek(SeekFrom::Start(14 * GIB)).map_err(err)?;
            file.read_exact(&mut bytes).map_err(err)?;
            if bytes != fixture {
                return Err("STOP: B recovered filesystem changed".into());
            }
        }
        records.push(Record {
            serial: serial.into(),
            partitions: count,
            table_sha256: format!("{:x}", Sha256::digest(table.encode()?)),
        });
    }
    Ok(records)
}
pub fn baseline() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    if Path::new(RECORD).exists() {
        return Err(
            "STOP: baseline already exists; never overwrite or repeat state changes".into(),
        );
    }
    let records = snapshot(true)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(RECORD)
        .map_err(err)?;
    out.write_all(&serde_json::to_vec_pretty(&records).map_err(err)?)
        .map_err(err)?;
    out.sync_all().map_err(err)?;
    Ok("PASS: A/19 online and B/10 offline, both writable; raw/native GPT agrees. GPT digests saved for independent comparison. B's complete recovered 2 MiB NTFS equals the fixture. No A/B writes. Use the production UI on A: read-only, writable, offline, online; then run Device-State-Verify.".into())
}
pub fn verify() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    inventory()?;
    let bytes = fs::read(RECORD).map_err(err)?;
    if bytes.len() > 8192 {
        return Err("STOP: invalid baseline size".into());
    }
    let before: Vec<Record> = serde_json::from_slice(&bytes).map_err(err)?;
    if before != snapshot(true)? {
        return Err("STOP: GPT differs from the baseline".into());
    }
    Ok("PASS: A/19 online and B/10 offline, both writable, matching initial states. Both complete GPT archives are byte-identical to the baseline; raw/native layouts agree. B's complete 2 MiB recovered NTFS remains byte-identical to the fixture. No A/B writes in this verification. Filesystem metadata written by Windows while A was online is outside this GPT comparison.".into())
}

/// Separate read-only acceptance path. Never invokes the maintenance backend,
/// creates a baseline, brings a disk online, or opens a writable device handle.
pub fn verify_maintenance_zero() -> Result<String, String> {
    let baseline = verify()?;
    let _guard = OPERATION.try_lock().map_err(err)?;
    Ok(format!("{baseline}\n\n{}", maintenance_zero()?))
}

fn maintenance_zero() -> Result<String, String> {
    inventory()?;
    let source = selection(B, Some(9))?;
    let resolved = data_windows::resolve(&source, false, false)?;
    const OFFSET: u64 = 25_464_832 * 512;
    const SIZE: u64 = 512 * 1024 * 1024;
    if resolved.disk.number != 2
        || resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(B)
        || resolved.disk.size != 16 * GIB
        || resolved.disk.is_offline != Some(true)
        || resolved.disk.is_read_only != Some(false)
        || resolved.disk.logical_sector_size != Some(512)
        || resolved.offset != OFFSET
        || resolved.size != SIZE
    {
        return Err("STOP: expected exact offline B/9 512 MiB test extent".into());
    }
    let mut file = data_windows::device(&resolved, false)?;
    file.seek(SeekFrom::Start(OFFSET)).map_err(err)?;
    let mut bytes = vec![0u8; 1024 * 1024];
    let mut hash = Sha256::new();
    for block in 0..512 {
        file.read_exact(&mut bytes).map_err(err)?;
        if bytes.iter().any(|v| *v != 0) {
            return Err(format!(
                "STOP: nonzero byte in B/9 MiB block {block}; no writes performed"
            ));
        }
        hash.update(&bytes);
    }
    let after = data_windows::resolve(&source, false, false)?;
    if after.offset != resolved.offset
        || after.size != resolved.size
        || after.disk.is_offline != Some(true)
        || serde_json::to_vec(&after.disk).map_err(err)?
            != serde_json::to_vec(&resolved.disk).map_err(err)?
    {
        return Err("STOP: B identity/layout/state changed during verification".into());
    }
    Ok(format!(
        "B9: all 512 MiB equal 00; SHA-256 {:x}. B remains offline.",
        hash.finalize()
    ))
}

// Diagnostics preserve the strict verifier's failure. Checking GPT and B9
// independently must not rewrite the immutable baseline or repair B10.
pub fn inspect_maintenance() -> Result<String, String> {
    let _guard = OPERATION.try_lock().map_err(err)?;
    inventory()?;
    let baseline = fs::read(RECORD).map_err(err)?;
    if baseline.len() > 8192 {
        return Err("STOP: invalid baseline size".into());
    }
    let before: Vec<Record> = serde_json::from_slice(&baseline).map_err(err)?;
    let current = snapshot(false)?;
    let gpt = if before == current {
        "PASS: both GPT archives unchanged"
    } else {
        "FAIL: GPT baseline mismatch"
    };
    let zero = maintenance_zero().unwrap_or_else(|e| format!("B9 FAIL: {e}"));
    let source = selection(B, Some(10))?;
    let resolved = data_windows::resolve(&source, false, false)?;
    let fixture = include_bytes!("../../../../tests/fixtures/ntfs-testfs1.img");
    if resolved.disk.number != 2
        || resolved.disk.size != 16 * GIB
        || resolved.disk.is_offline != Some(true)
        || resolved.disk.is_read_only != Some(false)
        || resolved.disk.logical_sector_size != Some(512)
        || resolved
            .disk
            .serial_number
            .as_deref()
            .map(disk_identity::clean)
            != Some(B)
        || resolved.offset != 14 * GIB
        || resolved.size != fixture.len() as u64
    {
        return Err("STOP: exact offline B10 fixture extent required".into());
    }
    let mut file = data_windows::device(&resolved, false)?;
    file.seek(SeekFrom::Start(resolved.offset)).map_err(err)?;
    let mut actual = vec![0; fixture.len()];
    file.read_exact(&mut actual).map_err(err)?;
    let diffs = fixture
        .iter()
        .zip(&actual)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let streams = compare_streams(fixture, &actual)
        .unwrap_or_else(|e| format!("File comparison incomplete: {e}"));
    let after = data_windows::resolve(&source, false, false)?;
    if after.offset != resolved.offset
        || after.size != resolved.size
        || serde_json::to_vec(&after.disk).map_err(err)?
            != serde_json::to_vec(&resolved.disk).map_err(err)?
    {
        return Err("STOP: B changed during read-only inspection".into());
    }
    Ok(format!("READ-ONLY DIAGNOSTICS (not a clean acceptance)\n{gpt}\n{zero}\nB10: {} changed bytes; first {:?}, last {:?}.\nFirst offsets: {:?}\nExpected SHA: {:x}\nActual SHA: {:x}\n{streams}\nNo disk writes, online changes or baseline updates.",diffs.len(),diffs.first(),diffs.last(),&diffs[..diffs.len().min(8)],Sha256::digest(fixture),Sha256::digest(&actual)))
}

fn stream_digests(bytes: &[u8]) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut reader = std::io::Cursor::new(bytes);
    let mut candidates = Vec::new();
    recovery::scan_ntfs(
        &mut reader,
        recovery::ScanMode::Existing,
        |_, _| Ok(()),
        |c| candidates.push(c),
    )?;
    if candidates.is_empty() {
        return Err("no existing NTFS files parsed".into());
    }
    let mut result = std::collections::BTreeMap::new();
    for c in candidates {
        if c.size > 16 * GIB {
            return Err("unexpected file size".into());
        }
        let recovery::Locator::Ntfs { record, .. } = &c.locator else {
            return Err("non-NTFS candidate".into());
        };
        let key = format!("{record}:{}", c.name);
        // Stream into a digest, never create an output file or allocate by a
        // potentially corrupt logical size. Named ADS are outside this check.
        let mut digest = DigestSink(Sha256::new());
        recovery::extract(&mut reader, &c, &mut digest, c.size, |_| Ok(()))?;
        let value = format!("{}:{:x}", c.size, digest.0.finalize());
        if result.insert(key, value).is_some() {
            return Err("duplicate file identity".into());
        }
    }
    Ok(result)
}
struct DigestSink(Sha256);
impl Write for DigestSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn compare_streams(expected: &[u8], actual: &[u8]) -> Result<String, String> {
    let before = stream_digests(expected)?;
    let after = stream_digests(actual)?;
    let keys = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>();
    let changed = keys
        .iter()
        .filter(|key| before.get(**key) != after.get(**key))
        .map(|key| (*key).clone())
        .collect::<Vec<_>>();
    Ok(format!("Unnamed existing file streams: {} before / {} after; {} unchanged.\nChanged/added/removed streams: {}.\nThis does not cover ADS, unused space or directory metadata.", before.len(),after.len(), keys.len()-changed.len(), if changed.is_empty() { "none".into() } else { changed.join(", ") }))
}

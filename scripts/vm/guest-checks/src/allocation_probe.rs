//! Explicit guest-only diagnostic. No writes, repair, format, dismount or
//! offline transition. The root inventory guard precedes every device open.
use super::*;
use std::io::Read;
fn information(r: &data_windows::Resolved) -> Result<String, String> {
    let mut reader = data_windows::reader(r)?;
    let ntfs = ntfs::Ntfs::new(&mut reader).map_err(err)?;
    let volume = ntfs.file(&mut reader, 3).map_err(err)?;
    let mut attributes = volume.attributes();
    let mut values = vec![];
    while let Some(item) = attributes.next(&mut reader) {
        let item = item.map_err(err)?;
        let attr = item.to_attribute().map_err(err)?;
        if attr.ty().map_err(err)? != ntfs::NtfsAttributeType::VolumeInformation {
            continue;
        }
        if !attr.is_resident() || attr.value_length() != 12 {
            return Err("Unexpected volume-information attribute".into());
        }
        let mut raw = [0; 12];
        attr.value(&mut reader)
            .map_err(err)?
            .attach(&mut reader)
            .read_exact(&mut raw)
            .map_err(err)?;
        values.push(format!(
            "NTFS {}.{}; flags 0x{:04x}; raw {:02x?}",
            raw[8],
            raw[9],
            u16::from_le_bytes([raw[10], raw[11]]),
            raw
        ));
    }
    if values.len() != 1 {
        return Err("Ambiguous volume-information records".into());
    }
    Ok(values.remove(0))
}
pub fn inspect() -> Result<String, String> {
    inventory()?;
    let mut report = vec!["READ-ONLY ALLOCATION DIAGNOSTIC (A/B only)".to_string()];
    for (label, serial) in [("A", A), ("B", B)] {
        let (number, _) = part(serial, 2 * GIB)?;
        let source = selection(serial, Some(number))?;
        let resolved = data_windows::resolve(&source, true, false)?;
        if resolved.size != 2 * GIB
            || resolved.disk.is_boot != Some(false)
            || resolved.disk.is_system != Some(false)
        {
            return Err("Unexpected diagnostic scope".into());
        }
        report.push(format!(
            "{label} partition {number}; before lock: {}",
            information(&resolved)?
        ));
        let locks = data_windows::lock(&resolved)?;
        let fresh = data_windows::resolve(&source, true, false)?;
        report.push(format!("{label} locked: {}", information(&fresh)?));
        drop(locks);
    }
    report.push(
        "No filesystem repair, source writes, destination open or automatic retry performed."
            .into(),
    );
    Ok(report.join("\n\n"))
}

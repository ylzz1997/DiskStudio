//! RAR block framing and checksums. Packed streams are never decompressed.
use super::{PResult, Probe, ProbeError};
use crate::signature_zip::Budget;
use std::io::{Read, Seek};

const RAR4: &[u8] = b"Rar!\x1a\x07\0";
const RAR5: &[u8] = b"Rar!\x1a\x07\x01\0";

pub(super) fn starts(bytes: &[u8]) -> bool {
    bytes.starts_with(RAR4) || bytes.starts_with(RAR5)
}

pub(super) fn inspect<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    signature: &[u8; 8],
    budget: &mut Budget,
) -> PResult<u64> {
    if signature.starts_with(RAR4) {
        rar4(probe, budget)
    } else if signature == RAR5 {
        rar5(probe, budget)
    } else {
        Err(ProbeError::Invalid)
    }
}

fn rar4<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    budget: &mut Budget,
) -> PResult<u64> {
    let mut at = RAR4.len() as u64;
    let mut bytes = Vec::new();
    loop {
        let prefix = probe.bytes::<7>(at)?;
        let kind = prefix[2];
        let flags = le16(&prefix[3..5]);
        let size = usize::from(le16(&prefix[5..7]));
        let first = at == RAR4.len() as u64;
        require((7..=u16::MAX as usize).contains(&size) && first == (kind == 0x73))?;
        let end = probe.bounded(at + size as u64)?;
        budget.charge(size as u64).map_err(ProbeError::Failed)?;
        bytes.resize(size, 0);
        probe.read_at(at, &mut bytes)?;
        require(
            bytes.starts_with(&prefix) && le16(&bytes[..2]) == crc32fast::hash(&bytes[2..]) as u16,
        )?;
        let packed = match kind {
            // Excludes volume, embedded old-style comment and encrypted headers.
            0x73 => {
                require(size == 13 && flags & !0x005c == 0)?;
                0
            }
            0x74 | 0x7a => {
                // File and service headers share the fixed fields. A large
                // header stores the upper packed and unpacked sizes before its name.
                let large = flags & 0x0100 != 0;
                let name_at = if large { 40 } else { 32 };
                require(flags & 0x8000 != 0 && flags & 0x040f == 0 && size >= name_at)?;
                let name_size = usize::from(le16(&bytes[26..28]));
                require(
                    name_size != 0
                        && name_at + name_size <= size
                        && matches!(bytes[25], 0x30..=0x35),
                )?;
                let upper = if large {
                    u64::from(le32(&bytes[32..36])) << 32
                } else {
                    0
                };
                let packed = upper | u64::from(le32(&bytes[7..11]));
                if bytes[25] == 0x30 {
                    let upper = if large {
                        u64::from(le32(&bytes[36..40])) << 32
                    } else {
                        0
                    };
                    require(packed == upper | u64::from(le32(&bytes[11..15])))?;
                }
                packed
            }
            0x7b => {
                // The optional four-byte archive data CRC is part of the header.
                require((flags == 0 && size == 7) || (flags == 2 && size == 11))?;
                return Ok(end);
            }
            _ => return Err(ProbeError::Invalid),
        };
        at = probe.bounded(end.checked_add(packed).ok_or(ProbeError::Invalid)?)?;
    }
}

fn rar5<R: Read + Seek, T: FnMut() -> Result<(), String>>(
    probe: &mut Probe<'_, R, T>,
    budget: &mut Budget,
) -> PResult<u64> {
    let mut at = RAR5.len() as u64;
    let mut bytes = Vec::new();
    loop {
        let prefix = probe.bytes::<7>(at)?;
        let mut size_field = &prefix[4..];
        let size = vint(&mut size_field)? as usize;
        require(size >= 3)?;
        let fields_at = prefix.len() - size_field.len();
        let header_len = fields_at + size;
        let end = probe.bounded(at + header_len as u64)?;
        budget
            .charge(header_len as u64)
            .map_err(ProbeError::Failed)?;
        bytes.resize(header_len, 0);
        probe.read_at(at, &mut bytes)?;
        require(bytes.starts_with(&prefix) && le32(&bytes[..4]) == crc32fast::hash(&bytes[4..]))?;

        let mut fields = &bytes[fields_at..];
        let kind = vint(&mut fields)?;
        let flags = vint(&mut fields)?;
        require((at == RAR5.len() as u64) == (kind == 1) && flags & !0x67 == 0)?;
        let extra_size = if flags & 1 != 0 {
            vint(&mut fields)?
        } else {
            0
        };
        let packed = if flags & 2 != 0 {
            vint(&mut fields)?
        } else {
            0
        };
        require(extra_size <= fields.len() as u64 && (flags & 1 == 0 || extra_size > 0))?;
        let (mut fields, extra) = fields.split_at(fields.len() - extra_size as usize);
        match kind {
            1 => {
                require(flags & !1 == 0 && vint(&mut fields)? & !0x1c == 0 && fields.is_empty())?;
                extra_records(extra, false)?;
            }
            2 | 3 => {
                file5(&mut fields, kind, packed)?;
                extra_records(extra, true)?;
            }
            5 => {
                require(flags == 0 && vint(&mut fields)? == 0 && fields.is_empty())?;
                return Ok(end);
            }
            _ => return Err(ProbeError::Invalid),
        }
        at = probe.bounded(end.checked_add(packed).ok_or(ProbeError::Invalid)?)?;
    }
}

fn file5(fields: &mut &[u8], kind: u64, packed: u64) -> PResult<()> {
    let flags = vint(fields)?;
    require(flags & !0xf == 0 && (flags & 1 == 0 || kind == 2 && packed == 0))?;
    let unpacked = vint(fields)?;
    vint(fields)?; // Host attributes.
    let fixed = 4 * ((flags & 2 != 0) as usize + (flags & 4 != 0) as usize);
    *fields = fields.get(fixed..).ok_or(ProbeError::Invalid)?;
    let compression = vint(fields)?;
    let method = (compression >> 7) & 7;
    require(method <= 5 && (method != 0 || flags & 8 != 0 || unpacked == packed))?;
    require(vint(fields)? <= 1)?;
    let name_size = vint(fields)?;
    require(name_size != 0 && name_size == fields.len() as u64)?;
    require(std::str::from_utf8(fields).is_ok() && !fields.contains(&0))
}

fn extra_records(mut extra: &[u8], file: bool) -> PResult<()> {
    while !extra.is_empty() {
        let n = vint(&mut extra)?;
        require(n > 0 && n <= extra.len() as u64)?;
        let (mut record, rest) = extra.split_at(n as usize);
        let kind = vint(&mut record)?;
        // Type 1 is the locator in a main header and encryption in a file header.
        require(kind != 0 && !(file && kind == 1))?;
        extra = rest;
    }
    Ok(())
}

fn vint(bytes: &mut &[u8]) -> PResult<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or(ProbeError::Invalid)?;
        *bytes = rest;
        require(shift != 63 || byte <= 1)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ProbeError::Invalid)
}

fn require(valid: bool) -> PResult<()> {
    valid.then_some(()).ok_or(ProbeError::Invalid)
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("two-byte field"))
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte field"))
}

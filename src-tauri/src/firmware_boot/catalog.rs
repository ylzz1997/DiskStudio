use std::collections::BTreeSet;

const GLOBAL_VENDOR: uuid::Uuid = uuid::Uuid::from_u128(0x8be4df61_93ca_11d2_aa0d_00e098032b8c);
const NAME_OFFSET: usize = 20;
pub(super) const MAX_DISCOVERED: usize = 1024;
const INVALID_CATALOG: &str = "Windows 返回的固件变量名列表无效。";

pub(super) fn parse_names(mut bytes: &[u8]) -> Result<BTreeSet<u16>, String> {
    let mut found = BTreeSet::new();
    while !bytes.is_empty() {
        if bytes.len() < NAME_OFFSET + 2 {
            return Err(INVALID_CATALOG.into());
        }
        let next = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let length = if next == 0 {
            bytes.len()
        } else if next >= NAME_OFFSET + 2 && next < bytes.len() && next % 2 == 0 {
            next
        } else {
            return Err(INVALID_CATALOG.into());
        };
        let (record, tail) = bytes.split_at(length);
        let tail_name = &record[NAME_OFFSET..];
        let name_len = tail_name
            .chunks_exact(2)
            .position(|unit| unit == [0, 0])
            .ok_or(INVALID_CATALOG)?;
        if record[4..NAME_OFFSET] == GLOBAL_VENDOR.to_bytes_le() && name_len == 8 {
            let units: [u16; 8] = std::array::from_fn(|i| {
                u16::from_le_bytes([tail_name[2 * i], tail_name[2 * i + 1]])
            });
            if let Ok(name) = String::from_utf16(&units) {
                if let Some(digits) = name.strip_prefix("Boot") {
                    if digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        if digits.bytes().any(|byte| byte.is_ascii_lowercase()) {
                            return Err("固件启动项变量名使用了非标准大小写。".into());
                        }
                        let id = u16::from_str_radix(digits, 16).map_err(|_| INVALID_CATALOG)?;
                        if !found.insert(id) || found.len() > MAX_DISCOVERED {
                            return Err("固件启动项编号重复或超出支持的数量。".into());
                        }
                    }
                }
            }
        }
        bytes = tail;
    }
    Ok(found)
}

#[cfg(windows)]
pub(super) fn enumerate() -> Result<Option<BTreeSet<u16>>, String> {
    use std::ffi::c_void;
    use windows_sys::core::{s, w};
    use windows_sys::Win32::{
        Foundation::{
            STATUS_ACCESS_DENIED, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH,
            STATUS_INVALID_INFO_CLASS, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
            STATUS_PRIVILEGE_NOT_HELD,
        },
        System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
    };
    const NAME_INFORMATION: u32 = 1;
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    type NtEnumerate = unsafe extern "system" fn(u32, *mut c_void, *mut u32) -> i32;
    let module = unsafe { GetModuleHandleW(w!("ntdll.dll")) };
    let Some(symbol) = (!module.is_null())
        .then(|| unsafe { GetProcAddress(module, s!("NtEnumerateSystemEnvironmentValuesEx")) })
        .flatten()
    else {
        return Ok(None);
    };
    // Use the native name-only information class. Read values for selected
    // Boot#### names through the public Win32 firmware API.
    // ABI reference https://github.com/winsiderss/systeminformer/blob/master/phnt/include/ntexapi.h
    let call: NtEnumerate = unsafe { std::mem::transmute(symbol) };
    let mut desired: usize = 4096;
    for _ in 0..8 {
        let mut words = vec![0u64; desired.div_ceil(8)];
        let capacity = words.len() * 8;
        let mut size = capacity as u32;
        let status = unsafe { call(NAME_INFORMATION, words.as_mut_ptr().cast(), &mut size) };
        let size = size as usize;
        match status {
            0 if size <= capacity => {
                let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), size) };
                return parse_names(bytes).map(Some);
            }
            STATUS_BUFFER_TOO_SMALL | STATUS_INFO_LENGTH_MISMATCH
                if size > capacity && size <= MAX_BYTES =>
            {
                desired = size;
            }
            STATUS_INVALID_INFO_CLASS
            | STATUS_NOT_IMPLEMENTED
            | STATUS_NOT_SUPPORTED
            | STATUS_ACCESS_DENIED
            | STATUS_PRIVILEGE_NOT_HELD => return Ok(None),
            _ => {
                return Err(format!(
                    "NtEnumerateSystemEnvironmentValuesEx (0x{:08X})",
                    status as u32
                ))
            }
        }
    }
    Err("固件变量名列表持续变化，无法完成读取。".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(records: &[(&str, uuid::Uuid)]) -> Vec<u8> {
        let mut result = Vec::new();
        for (index, (name, vendor)) in records.iter().enumerate() {
            let name: Vec<u8> = name
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect();
            let record_len = (NAME_OFFSET + name.len()).next_multiple_of(4);
            let next = if index + 1 == records.len() {
                0
            } else {
                record_len as u32
            };
            let end = result.len() + record_len;
            result.extend(next.to_le_bytes());
            result.extend(vendor.to_bytes_le());
            result.extend(name);
            result.resize(end, 0);
        }
        result
    }

    #[test]
    fn enumerates_only_boot_names_in_the_global_namespace() {
        let buffer = catalog(&[
            ("BootOrder", GLOBAL_VENDOR),
            ("BootNext", GLOBAL_VENDOR),
            ("Bootdesc", GLOBAL_VENDOR),
            ("Boot0002", GLOBAL_VENDOR),
            ("BootFFFF", GLOBAL_VENDOR),
            ("Boot00AB", uuid::Uuid::from_u128(7)),
            ("Boot00A1", GLOBAL_VENDOR),
        ]);
        assert_eq!(
            parse_names(&buffer).unwrap(),
            BTreeSet::from([2, 0xA1, 0xFFFF])
        );
    }

    #[test]
    fn rejects_invalid_record_offsets_missing_terminators_and_duplicate_ids() {
        let mut record = catalog(&[("Boot0002", GLOBAL_VENDOR)]);
        assert!(parse_names(&record[..NAME_OFFSET + 16]).is_err());
        record[..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_names(&record).is_err());
        record[..4].copy_from_slice(&4000u32.to_le_bytes());
        assert!(parse_names(&record).is_err());
        assert!(parse_names(&catalog(&[("Boot00aa", GLOBAL_VENDOR)])).is_err());
        assert!(parse_names(&catalog(&[("Boot0002", GLOBAL_VENDOR); 2])).is_err());
    }
}

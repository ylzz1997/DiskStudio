use super::{inspect::hex, Certificate};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::FILETIME,
    Security::Cryptography::{
        CertCreateCertificateContext, CertFreeCertificateContext, CertNameToStrW, CERT_CONTEXT,
        CERT_X500_NAME_STR, CRYPT_INTEGER_BLOB, X509_ASN_ENCODING,
    },
};

struct Context(*const CERT_CONTEXT);

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateContext(self.0) };
    }
}

pub(super) fn decode(bytes: &[u8]) -> Option<Certificate> {
    if !is_one_der_sequence(bytes) {
        return None;
    }
    let pointer = unsafe {
        CertCreateCertificateContext(
            X509_ASN_ENCODING,
            bytes.as_ptr(),
            bytes.len().try_into().ok()?,
        )
    };
    if pointer.is_null() {
        return None;
    }
    let context = Context(pointer);
    let info = unsafe { (*context.0).pCertInfo.as_ref() }?;
    let serial = &info.SerialNumber;
    // Crypt32 represents certificate serial integers in little-endian order.
    let serial = (!serial.pbData.is_null() && (1..=128).contains(&serial.cbData)).then(|| {
        let bytes = unsafe { std::slice::from_raw_parts(serial.pbData, serial.cbData as usize) };
        hex(bytes.iter().rev())
    });
    Some(Certificate {
        subject: name(&info.Subject),
        issuer: name(&info.Issuer),
        not_before: utc(info.NotBefore),
        not_after: utc(info.NotAfter),
        serial,
    })
}

fn is_one_der_sequence(bytes: &[u8]) -> bool {
    let [0x30, size, tail @ ..] = bytes else {
        return false;
    };
    if *size < 128 {
        return tail.len() == usize::from(*size);
    }
    let n = usize::from(size & 127);
    let Some((length, content)) = tail.split_at_checked(n) else {
        return false;
    };
    if !(1..=4).contains(&n) || length[0] == 0 {
        return false;
    }
    let length = length
        .iter()
        .fold(0usize, |value, byte| (value << 8) | usize::from(*byte));
    length >= 128 && length == content.len()
}

fn name(blob: &CRYPT_INTEGER_BLOB) -> Option<String> {
    let length =
        unsafe { CertNameToStrW(X509_ASN_ENCODING, blob, CERT_X500_NAME_STR, null_mut(), 0) };
    if !(2..=2048).contains(&length) {
        return None;
    }
    let mut utf16 = vec![0u16; length as usize];
    let result = unsafe {
        CertNameToStrW(
            X509_ASN_ENCODING,
            blob,
            CERT_X500_NAME_STR,
            utf16.as_mut_ptr(),
            length,
        )
    };
    if result != length || utf16.pop() != Some(0) {
        return None;
    }
    String::from_utf16(&utf16).ok()
}

fn utc(file_time: FILETIME) -> Option<String> {
    let ticks = u64::from(file_time.dwHighDateTime) << 32 | u64::from(file_time.dwLowDateTime);
    let unix_seconds = i64::try_from(ticks / 10_000_000).ok()? - 11_644_473_600;
    let utc = time::OffsetDateTime::from_unix_timestamp(unix_seconds).ok()?;
    Some(format!(
        "{}T{:02}:{:02}:{:02}Z",
        utc.date(),
        utc.hour(),
        utc.minute(),
        utc.second()
    ))
}

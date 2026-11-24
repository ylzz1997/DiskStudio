use super::{
    chain,
    pe::sha1,
    pkcs::{embedded_certificates, signed_data, Der},
    TimestampEvidence, TimestampStatus,
};
use crate::firmware_boot::secure_boot::{certificate_subject, Report};
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use std::ptr::null_mut;
use time::{Date, OffsetDateTime, Time};
use windows_sys::Win32::Security::Cryptography::{
    szOID_ENHANCED_KEY_USAGE, szOID_KEY_USAGE, CertFindExtension, CertFreeCertificateContext,
    CertGetIntendedKeyUsage, CryptMemFree, CryptVerifyTimeStampSignature, CERT_CONTEXT,
    CERT_DIGITAL_SIGNATURE_KEY_USAGE, CERT_NON_REPUDIATION_KEY_USAGE, CRYPT_TIMESTAMP_CONTEXT,
    X509_ASN_ENCODING,
};

const MICROSOFT_RFC3161: &[u8] = b"\x2b\x06\x01\x04\x01\x82\x37\x03\x03\x01";
const SIGNATURE_TIMESTAMP: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x09\x10\x02\x0e";
const LEGACY_COUNTERSIGNATURE: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x09\x06";
const TST_INFO: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x09\x10\x01\x04";
const ESS_CERT: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x09\x10\x02\x0c";
const ESS_CERT_V2: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x09\x10\x02\x2f";
const TIMESTAMP_EKU: &[u8] = b"\x30\x0a\x06\x08\x2b\x06\x01\x05\x05\x07\x03\x08";
const SHA256_OID: &[u8] = b"\x60\x86\x48\x01\x65\x03\x04\x02\x01";
const MAX_ATTRIBUTES: usize = 64;
const MAX_TOKENS: usize = 8;
const MAX_TOKEN_SIZE: usize = 1024 * 1024;
const MAX_CERT_SIZE: usize = 64 * 1024;

struct NativeTimestamp {
    time: *mut CRYPT_TIMESTAMP_CONTEXT,
    signer: *mut CERT_CONTEXT,
}

impl Drop for NativeTimestamp {
    fn drop(&mut self) {
        if !self.time.is_null() {
            unsafe { CryptMemFree(self.time.cast()) };
        }
        if !self.signer.is_null() {
            unsafe { CertFreeCertificateContext(self.signer) };
        }
    }
}

impl TimestampEvidence {
    fn new(status: TimestampStatus) -> Self {
        Self {
            status,
            signed_at: None,
            subject: None,
            anchor_sha256: None,
            trusted_at: None,
        }
    }
}

pub(super) fn inspect(
    encoded_signer: &[u8],
    firmware: Option<&Report>,
) -> Option<TimestampEvidence> {
    let signer = signer_info(encoded_signer)?;
    let attrs = attributes(signer.unsigned_attrs)?;
    let mut tokens = Vec::new();
    let mut legacy = false;
    for (oid, values) in attrs {
        if oid == LEGACY_COUNTERSIGNATURE {
            legacy = true;
        }
        if !matches!(oid, MICROSOFT_RFC3161 | SIGNATURE_TIMESTAMP) {
            continue;
        }
        let mut values = Der::new(values);
        if values.remaining.is_empty() {
            return Some(TimestampEvidence::new(TimestampStatus::Invalid));
        }
        while !values.remaining.is_empty() {
            let value = values.encoded(0x30)?;
            if tokens.len() >= MAX_TOKENS || value.len() > MAX_TOKEN_SIZE {
                return Some(TimestampEvidence::new(TimestampStatus::Unsupported));
            }
            tokens.push(value);
        }
    }
    if tokens.is_empty() {
        return legacy.then(|| TimestampEvidence::new(TimestampStatus::Unsupported));
    }
    tokens
        .into_iter()
        .map(|token| verify_token(token, signer.signature, firmware))
        .min_by_key(|item| match item.status {
            TimestampStatus::Verified => (0, item.trusted_at),
            TimestampStatus::Untrusted => (1, None),
            TimestampStatus::Unavailable => (2, None),
            TimestampStatus::Invalid => (3, None),
            TimestampStatus::Unsupported => (4, None),
        })
}

fn verify_token(bytes: &[u8], signature: &[u8], firmware: Option<&Report>) -> TimestampEvidence {
    let mut result = TimestampEvidence::new(TimestampStatus::Invalid);
    let Some(token) = token(bytes, signature) else {
        return result;
    };
    let mut native = NativeTimestamp {
        time: null_mut(),
        signer: null_mut(),
    };
    if unsafe {
        CryptVerifyTimeStampSignature(
            bytes.as_ptr(),
            bytes.len() as u32,
            signature.as_ptr(),
            signature.len() as u32,
            null_mut(),
            &mut native.time,
            &mut native.signer,
            null_mut(),
        )
    } == 0
    {
        return result;
    }
    let Some(leaf) = (unsafe { native.signer.as_ref() }) else {
        return result;
    };
    if native.time.is_null()
        || leaf.pbCertEncoded.is_null()
        || !(1..=MAX_CERT_SIZE).contains(&(leaf.cbCertEncoded as usize))
    {
        return result;
    }
    let der =
        unsafe { std::slice::from_raw_parts(leaf.pbCertEncoded, leaf.cbCertEncoded as usize) };
    if !token.certificates.contains(&der)
        || !timestamp_usage(leaf)
        || !ess_signer_matches(token.signed_attrs, der)
    {
        return result;
    }
    result.signed_at = Some(format_time(token.at));
    result.subject = certificate_subject(der);
    let Some(firmware) = firmware else {
        result.status = TimestampStatus::Unavailable;
        return result;
    };
    match chain::timestamp_anchor(leaf, der, &token.certificates, firmware, token.at) {
        Ok(anchor) => {
            result.status = TimestampStatus::Verified;
            result.anchor_sha256 = Some(anchor);
            result.trusted_at = Some(token.at);
        }
        Err(status) => result.status = status,
    }
    result
}

fn timestamp_usage(leaf: &CERT_CONTEXT) -> bool {
    let Some(info) = (unsafe { leaf.pCertInfo.as_ref() }) else {
        return false;
    };
    if info.rgExtension.is_null() {
        return false;
    }
    let usage = unsafe {
        CertFindExtension(szOID_ENHANCED_KEY_USAGE, info.cExtension, info.rgExtension).as_ref()
    };
    let dedicated_eku = usage.is_some_and(|extension| {
        extension.fCritical != 0
            && !extension.Value.pbData.is_null()
            && extension.Value.cbData as usize == TIMESTAMP_EKU.len()
            && unsafe {
                std::slice::from_raw_parts(extension.Value.pbData, TIMESTAMP_EKU.len())
                    == TIMESTAMP_EKU
            }
    });
    if !dedicated_eku {
        return false;
    }
    if unsafe { CertFindExtension(szOID_KEY_USAGE, info.cExtension, info.rgExtension) }.is_null() {
        return true;
    }
    let mut key_usage = [0u8; 2];
    let decoded =
        unsafe { CertGetIntendedKeyUsage(X509_ASN_ENCODING, info, key_usage.as_mut_ptr(), 2) != 0 };
    decoded
        && u32::from(key_usage[0])
            & (CERT_DIGITAL_SIGNATURE_KEY_USAGE | CERT_NON_REPUDIATION_KEY_USAGE)
            != 0
}

struct Token<'a> {
    at: OffsetDateTime,
    certificates: Vec<&'a [u8]>,
    signed_attrs: &'a [u8],
}

fn token<'a>(bytes: &'a [u8], signature: &[u8]) -> Option<Token<'a>> {
    if !(1..=MAX_TOKEN_SIZE).contains(&bytes.len()) || signature.is_empty() {
        return None;
    }
    let (encoded, mut signed) = signed_data(bytes)?;
    if encoded.len() != bytes.len() {
        return None;
    }
    signed.element(0x02)?;
    signed.element(0x31)?;
    let mut content = Der::new(signed.element(0x30)?);
    if content.element(0x06)? != TST_INFO {
        return None;
    }
    let tst_content = Der::new(content.only(0xa0)?).only(0x04)?;
    let mut tst = Der::new(Der::new(tst_content).only(0x30)?);
    if tst.element(0x02)? != [1] {
        return None;
    }
    tst.element(0x06)?;
    let mut imprint = Der::new(tst.element(0x30)?);
    let algorithm = algorithm_oid(imprint.element(0x30)?)?;
    if digest(algorithm, signature, false)?.as_slice() != imprint.only(0x04)? {
        return None;
    }
    tst.element(0x02)?;
    let at = generalized_time(tst.element(0x18)?)?;
    let certificates = embedded_certificates(&mut signed)?;
    if signed.remaining.first() == Some(&0xa1) {
        signed.element(0xa1)?;
    }
    let mut signers = Der::new(signed.only(0x31)?);
    let signer = signers.encoded(0x30)?;
    if !signers.remaining.is_empty() {
        return None;
    }
    Some(Token {
        at,
        certificates,
        signed_attrs: signer_info(signer)?.signed_attrs,
    })
}

struct SignerInfo<'a> {
    signed_attrs: &'a [u8],
    signature: &'a [u8],
    unsigned_attrs: &'a [u8],
}

fn signer_info(bytes: &[u8]) -> Option<SignerInfo<'_>> {
    let mut signer = Der::new(Der::new(bytes).only(0x30)?);
    signer.element(0x02)?;
    let identifier = *signer.remaining.first()?;
    if !matches!(identifier, 0x30 | 0x80) {
        return None;
    }
    signer.element(identifier)?;
    signer.element(0x30)?;
    let signed_attrs = if signer.remaining.first() == Some(&0xa0) {
        signer.element(0xa0)?
    } else {
        &[]
    };
    signer.element(0x30)?;
    let signature = signer.element(0x04)?;
    let unsigned_attrs = if signer.remaining.is_empty() {
        &[]
    } else {
        signer.only(0xa1)?
    };
    Some(SignerInfo {
        signed_attrs,
        signature,
        unsigned_attrs,
    })
}

fn attributes(bytes: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    let mut source = Der::new(bytes);
    let mut attrs = Vec::new();
    while !source.remaining.is_empty() {
        if attrs.len() >= MAX_ATTRIBUTES {
            return None;
        }
        let mut attr = Der::new(source.element(0x30)?);
        attrs.push((attr.element(0x06)?, attr.only(0x31)?));
    }
    Some(attrs)
}

fn ess_signer_matches(signed_attrs: &[u8], certificate: &[u8]) -> bool {
    let Some(attrs) = attributes(signed_attrs) else {
        return false;
    };
    let mut present = false;
    for (oid, values) in attrs {
        if matches!(oid, ESS_CERT | ESS_CERT_V2) {
            present = true;
            if !ess_digest_matches(oid, values, certificate).unwrap_or(false) {
                return false;
            }
        }
    }
    present
}

fn ess_digest_matches(oid: &[u8], values: &[u8], certificate: &[u8]) -> Option<bool> {
    let mut outer = Der::new(Der::new(values).only(0x30)?);
    let mut ids = Der::new(outer.element(0x30)?);
    let mut id = Der::new(ids.element(0x30)?);
    let algorithm = if oid == ESS_CERT {
        b"\x2b\x0e\x03\x02\x1a".as_slice()
    } else if id.remaining.first() == Some(&0x30) {
        algorithm_oid(id.element(0x30)?)?
    } else {
        SHA256_OID
    };
    let expected = id.element(0x04)?;
    Some(digest(algorithm, certificate, true)? == expected)
}

fn algorithm_oid(bytes: &[u8]) -> Option<&[u8]> {
    let mut id = Der::new(bytes);
    let algorithm = id.element(0x06)?;
    if !id.remaining.is_empty() && !id.only(0x05)?.is_empty() {
        return None;
    }
    Some(algorithm)
}

fn digest(algorithm: &[u8], bytes: &[u8], certificate_id: bool) -> Option<Vec<u8>> {
    Some(match algorithm {
        b"\x2b\x0e\x03\x02\x1a" if certificate_id => sha1([bytes, &[], &[]])?.to_vec(),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x04" => Sha224::digest(bytes).to_vec(),
        SHA256_OID => Sha256::digest(bytes).to_vec(),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x02" => Sha384::digest(bytes).to_vec(),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x03" => Sha512::digest(bytes).to_vec(),
        _ => return None,
    })
}

fn generalized_time(source: &[u8]) -> Option<OffsetDateTime> {
    let bytes = source.strip_suffix(b"Z")?;
    let (fields, remainder) = bytes.split_at_checked(14)?;
    let decimal = |digits: &[u8]| {
        if digits.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(digits).ok()?.parse::<u32>().ok()
        } else {
            None
        }
    };
    let nanos = if remainder.is_empty() {
        0
    } else {
        let fraction = remainder.strip_prefix(b".")?;
        if !(1..=9).contains(&fraction.len()) || fraction.last() == Some(&b'0') {
            return None;
        }
        decimal(fraction)? * 10u32.pow((9 - fraction.len()) as u32)
    };
    let date = Date::from_calendar_date(
        decimal(&fields[..4])?.try_into().ok()?,
        u8::try_from(decimal(&fields[4..6])?)
            .ok()?
            .try_into()
            .ok()?,
        u8::try_from(decimal(&fields[6..8])?).ok()?,
    )
    .ok()?;
    let time = Time::from_hms_nano(
        u8::try_from(decimal(&fields[8..10])?).ok()?,
        u8::try_from(decimal(&fields[10..12])?).ok()?,
        u8::try_from(decimal(&fields[12..14])?).ok()?,
        nanos,
    )
    .ok()?;
    Some(date.with_time(time).assume_utc())
}

fn format_time(at: OffsetDateTime) -> String {
    format!(
        "{}T{:02}:{:02}:{:02}.{:09}Z",
        at.date(),
        at.hour(),
        at.minute(),
        at.second(),
        at.nanosecond()
    )
}

#[cfg(test)]
pub(super) fn fixture_parts(encoded: &[u8]) -> (&[u8], &[u8], Vec<&[u8]>) {
    let signer = signer_info(encoded).unwrap();
    let timestamp = attributes(signer.unsigned_attrs)
        .unwrap()
        .into_iter()
        .find(|(oid, _)| *oid == MICROSOFT_RFC3161)
        .unwrap();
    let bytes = Der::new(timestamp.1).encoded(0x30).unwrap();
    let parsed = token(bytes, signer.signature).unwrap();
    (bytes, signer.signature, parsed.certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_uses_utc_and_preserves_nanos() {
        let at = generalized_time(b"20261123143000.123456789Z").unwrap();
        assert_eq!(at.nanosecond(), 123_456_789);
        for invalid in [
            b"20261123143000.120Z".as_slice(),
            b"20261123143000+0100",
            b"20261123143000,1Z",
            b"20261323143000Z",
            b"20261123143000.1234567891Z",
        ] {
            assert!(generalized_time(invalid).is_none());
        }
    }

    #[test]
    fn rejects_a_valid_timestamp_for_a_different_signature() {
        let message = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            include_str!("timestamp-fixture.b64")
                .lines()
                .collect::<String>(),
        )
        .unwrap();
        let encoded = super::super::pkcs::signer_for_test(&message);
        let (bytes, signature, _) = fixture_parts(&encoded);
        assert_eq!(
            verify_token(bytes, signature, None).status,
            TimestampStatus::Unavailable
        );
        let mut altered = signature.to_vec();
        altered[0] ^= 1;
        assert_eq!(
            verify_token(bytes, &altered, None).status,
            TimestampStatus::Invalid
        );
    }
}

use super::{chain, hex, pe::Algorithm, pe::ImageDigest, timestamp, Signature, SignatureStatus};
use crate::firmware_boot::secure_boot::Report;
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::Security::Cryptography::{
    CertFreeCertificateContext, CryptGetMessageSignerCount, CryptMsgClose, CryptMsgGetParam,
    CryptMsgOpenToDecode, CryptMsgUpdate, CryptVerifyMessageSignature, CERT_CONTEXT,
    CMSG_ENCODED_SIGNER, CRYPT_VERIFY_MESSAGE_PARA, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

const ENCODING: u32 = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;
const MAX_SIGNERS: i32 = 8;
const MAX_EMBEDDED_CERTIFICATES: usize = 64;
const MAX_CONTENT: usize = 64 * 1024;
const SIGNED_DATA: &[u8] = b"\x2a\x86\x48\x86\xf7\x0d\x01\x07\x02";
const SPC_INDIRECT_DATA: &[u8] = b"\x2b\x06\x01\x04\x01\x82\x37\x02\x01\x04";
const SPC_PE_IMAGE_DATA: &[u8] = b"\x2b\x06\x01\x04\x01\x82\x37\x02\x01\x0f";

struct Certificate(*mut CERT_CONTEXT);

impl Drop for Certificate {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CertFreeCertificateContext(self.0) };
        }
    }
}

struct Message(*mut c_void);

impl Drop for Message {
    fn drop(&mut self) {
        unsafe { CryptMsgClose(self.0) };
    }
}

impl Message {
    fn decode(bytes: &[u8]) -> Option<Self> {
        let length: u32 = bytes.len().try_into().ok()?;
        let pointer = unsafe { CryptMsgOpenToDecode(ENCODING, 0, 0, 0, null(), null()) };
        if pointer.is_null() {
            return None;
        }
        let message = Self(pointer);
        (unsafe { CryptMsgUpdate(message.0, bytes.as_ptr(), length, 1) } != 0).then_some(message)
    }

    fn signer(&self, index: u32) -> Option<Vec<u8>> {
        let mut length = 0;
        if unsafe { CryptMsgGetParam(self.0, CMSG_ENCODED_SIGNER, index, null_mut(), &mut length) }
            == 0
            || !(1..=1_048_576).contains(&length)
        {
            return None;
        }
        let mut bytes = vec![0; length as usize];
        if unsafe {
            CryptMsgGetParam(
                self.0,
                CMSG_ENCODED_SIGNER,
                index,
                bytes.as_mut_ptr().cast(),
                &mut length,
            )
        } == 0
            || length == 0
            || length as usize > bytes.len()
        {
            return None;
        }
        bytes.truncate(length as usize);
        Some(bytes)
    }
}

pub(super) fn inspect(
    bytes: &[u8],
    digests: &[ImageDigest],
    firmware: Option<&Report>,
) -> Vec<Signature> {
    let Some(signed) = signed_image_digest(bytes) else {
        return vec![Signature::new(SignatureStatus::Invalid)];
    };
    let SignedImage {
        message,
        content,
        algorithm,
        expected,
        certificates,
    } = signed;
    let Some(actual) = digests
        .iter()
        .find(|entry| Some(entry.algorithm) == algorithm)
    else {
        return vec![Signature::new(SignatureStatus::Unsupported)];
    };
    let Ok(message_len) = u32::try_from(message.len()) else {
        return vec![Signature::new(SignatureStatus::Unsupported)];
    };
    let count = unsafe { CryptGetMessageSignerCount(ENCODING, message.as_ptr(), message_len) };
    if !(1..=MAX_SIGNERS).contains(&count) {
        let status = if count > MAX_SIGNERS {
            SignatureStatus::Unsupported
        } else {
            SignatureStatus::Invalid
        };
        return vec![Signature::new(status)];
    }
    let parameters = CRYPT_VERIFY_MESSAGE_PARA {
        cbSize: size_of::<CRYPT_VERIFY_MESSAGE_PARA>() as u32,
        dwMsgAndCertEncodingType: ENCODING,
        ..Default::default()
    };
    let decoded_message = Message::decode(message);
    (0..count)
        .map(|signer| {
            let mut signature = Signature::new(SignatureStatus::Invalid);
            signature.algorithm = Some(actual.algorithm.label());
            signature.signed_digest = Some(hex(expected));
            let mut decoded = vec![0; MAX_CONTENT];
            let mut length = decoded.len() as u32;
            let mut certificate = Certificate(null_mut());
            let verified = unsafe {
                CryptVerifyMessageSignature(
                    &parameters,
                    signer as u32,
                    message.as_ptr(),
                    message_len,
                    decoded.as_mut_ptr(),
                    &mut length,
                    &mut certificate.0,
                )
            };
            if verified == 0 || decoded.get(..length as usize) != Some(content) {
                return signature;
            }
            let Some(context) = (unsafe { certificate.0.as_ref() }) else {
                return signature;
            };
            if context.pbCertEncoded.is_null()
                || !(1..=MAX_CONTENT).contains(&(context.cbCertEncoded as usize))
            {
                return signature;
            }
            let der = unsafe {
                std::slice::from_raw_parts(context.pbCertEncoded, context.cbCertEncoded as usize)
            };
            signature.certificate_sha256 = Some(format!("{:x}", Sha256::digest(der)));
            signature.subject = crate::firmware_boot::secure_boot::certificate_subject(der);
            signature.status = if actual.bytes == expected {
                signature.timestamp = decoded_message
                    .as_ref()
                    .and_then(|message| message.signer(signer as u32))
                    .and_then(|bytes| timestamp::inspect(&bytes, firmware));
                signature.certificate_trust = Some(chain::assess(
                    context,
                    der,
                    certificates.as_deref(),
                    firmware,
                    signature
                        .timestamp
                        .as_ref()
                        .and_then(|timestamp| timestamp.trusted_at),
                ));
                SignatureStatus::Verified
            } else {
                SignatureStatus::DigestMismatch
            };
            signature
        })
        .collect()
}

pub(super) struct Der<'a> {
    pub(super) remaining: &'a [u8],
}

impl<'a> Der<'a> {
    pub(super) fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    pub(super) fn element(&mut self, tag: u8) -> Option<&'a [u8]> {
        let [actual, first, tail @ ..] = self.remaining else {
            return None;
        };
        if *actual != tag {
            return None;
        }
        let (length, data) = if *first < 128 {
            (usize::from(*first), tail)
        } else {
            let length_size = usize::from(first & 127);
            if !(1..=4).contains(&length_size) {
                return None;
            }
            let (encoded, data) = tail.split_at_checked(length_size)?;
            let length = encoded
                .iter()
                .fold(0usize, |value, &byte| value << 8 | usize::from(byte));
            if encoded[0] == 0 || length < 128 {
                return None;
            }
            (length, data)
        };
        let (value, remaining) = data.split_at_checked(length)?;
        self.remaining = remaining;
        Some(value)
    }

    pub(super) fn only(&mut self, tag: u8) -> Option<&'a [u8]> {
        let value = self.element(tag)?;
        self.remaining.is_empty().then_some(value)
    }

    pub(super) fn encoded(&mut self, tag: u8) -> Option<&'a [u8]> {
        let start = self.remaining;
        self.element(tag)?;
        Some(&start[..start.len() - self.remaining.len()])
    }
}

pub(super) fn certificate_tbs(bytes: &[u8]) -> Option<&[u8]> {
    let mut certificate = Der::new(Der::new(bytes).only(0x30)?);
    let tbs = certificate.encoded(0x30)?;
    certificate.element(0x30)?;
    certificate.only(0x03)?;
    Some(tbs)
}

struct SignedImage<'a> {
    message: &'a [u8],
    content: &'a [u8],
    algorithm: Option<Algorithm>,
    expected: &'a [u8],
    certificates: Option<Vec<&'a [u8]>>,
}

pub(super) fn embedded_certificates<'a>(signed: &mut Der<'a>) -> Option<Vec<&'a [u8]>> {
    let mut certificates = Vec::new();
    if signed.remaining.first() != Some(&0xa0) {
        return Some(certificates);
    }
    let mut encoded = Der::new(signed.element(0xa0)?);
    while !encoded.remaining.is_empty() {
        if certificates.len() >= MAX_EMBEDDED_CERTIFICATES {
            return None;
        }
        let cert = encoded.encoded(0x30)?;
        certificate_tbs(cert)?;
        if cert.len() > MAX_CONTENT {
            return None;
        }
        certificates.push(cert);
    }
    Some(certificates)
}

pub(super) fn signed_data(bytes: &[u8]) -> Option<(&[u8], Der<'_>)> {
    let mut outer = Der::new(bytes);
    let mut info = Der::new(outer.element(0x30)?);
    if outer.remaining.len() > 7
        || outer.remaining.iter().any(|&byte| byte != 0)
        || info.element(0x06)? != SIGNED_DATA
    {
        return None;
    }
    let message = &bytes[..bytes.len() - outer.remaining.len()];
    let signed = Der::new(Der::new(info.only(0xa0)?).only(0x30)?);
    Some((message, signed))
}

fn signed_image_digest(bytes: &[u8]) -> Option<SignedImage<'_>> {
    let (message, mut signed) = signed_data(bytes)?;
    signed.element(0x02)?;
    signed.element(0x31)?;
    let mut content_info = Der::new(signed.element(0x30)?);
    if content_info.element(0x06)? != SPC_INDIRECT_DATA {
        return None;
    }
    let content = content_info.only(0xa0)?;
    if content.len() > MAX_CONTENT {
        return None;
    }
    let mut indirect = Der::new(Der::new(content).only(0x30)?);
    let mut attributes = Der::new(indirect.element(0x30)?);
    if attributes.element(0x06)? != SPC_PE_IMAGE_DATA {
        return None;
    }
    let mut digest = Der::new(indirect.only(0x30)?);
    let mut oid = Der::new(digest.element(0x30)?);
    let algorithm = match oid.element(0x06)? {
        b"\x2b\x0e\x03\x02\x1a" => Some(Algorithm::Sha1),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x04" => Some(Algorithm::Sha224),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x01" => Some(Algorithm::Sha256),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x02" => Some(Algorithm::Sha384),
        b"\x60\x86\x48\x01\x65\x03\x04\x02\x03" => Some(Algorithm::Sha512),
        _ => None,
    };
    if !oid.remaining.is_empty() && !oid.only(0x05)?.is_empty() {
        return None;
    }
    let expected = digest.only(0x04)?;
    Some(SignedImage {
        message,
        content,
        algorithm,
        expected,
        certificates: embedded_certificates(&mut signed),
    })
}

#[cfg(test)]
pub(super) fn certificates_for_test(bytes: &[u8]) -> Vec<&[u8]> {
    signed_image_digest(bytes).unwrap().certificates.unwrap()
}

#[cfg(test)]
pub(super) fn signer_for_test(message: &[u8]) -> Vec<u8> {
    Message::decode(message).unwrap().signer(0).unwrap()
}

use super::{hex, pkcs, CertificateTrust, ChainStatus, RevocationStatus, TimestampStatus};
use crate::firmware_boot::secure_boot::{Report, Revocation, Status};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::{
    mem::size_of,
    ptr::{null, null_mut},
};
use time::OffsetDateTime;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Security::Cryptography::{
    szOID_PKIX_KP_TIMESTAMP_SIGNING, CertAddEncodedCertificateToStore, CertCloseStore,
    CertCreateCertificateChainEngine, CertFreeCertificateChain, CertFreeCertificateChainEngine,
    CertGetCertificateChain, CertOpenStore, CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
    CERT_CHAIN_CONTEXT, CERT_CHAIN_DISABLE_AIA, CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE,
    CERT_CHAIN_ENGINE_CONFIG, CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG, CERT_CHAIN_PARA, CERT_CONTEXT,
    CERT_STORE_ADD_USE_EXISTING, CERT_STORE_PROV_MEMORY, CERT_TRUST_IS_NOT_TIME_VALID,
    HCERTCHAINENGINE, HCERTSTORE, X509_ASN_ENCODING,
};

const MAX_CHAIN_CERTS: usize = 16;
const MAX_CERT_BYTES: usize = 64 * 1024;
const LOCAL_FLAGS: u32 = CERT_CHAIN_DISABLE_AIA
    | CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE
    | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL;

struct Store(HCERTSTORE);
impl Store {
    fn new(certificates: &[&[u8]]) -> Option<Self> {
        let handle = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null()) };
        if handle.is_null() {
            return None;
        }
        let store = Self(handle);
        for bytes in certificates {
            if !(1..=MAX_CERT_BYTES).contains(&bytes.len())
                || unsafe {
                    CertAddEncodedCertificateToStore(
                        store.0,
                        X509_ASN_ENCODING,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                        CERT_STORE_ADD_USE_EXISTING,
                        null_mut(),
                    )
                } == 0
            {
                return None;
            }
        }
        Some(store)
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        unsafe { CertCloseStore(self.0, 0) };
    }
}

struct Engine(HCERTCHAINENGINE);
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateChainEngine(self.0) };
    }
}

struct Chain(*mut CERT_CHAIN_CONTEXT);
impl Drop for Chain {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateChain(self.0) };
    }
}

pub(super) fn assess(
    leaf: &CERT_CONTEXT,
    leaf_der: &[u8],
    embedded: Option<&[&[u8]]>,
    firmware: Option<&Report>,
    timestamp: Option<OffsetDateTime>,
) -> CertificateTrust {
    let mut result = CertificateTrust {
        chain_status: ChainStatus::Unavailable,
        anchor_sha256: None,
        checked_certificates: vec![fingerprint(leaf_der)],
        revocation_status: RevocationStatus::Unavailable,
        revocation_times: Vec::new(),
    };
    let Some(firmware) = firmware else {
        return result;
    };
    let (db_status, db) = firmware.certificate_database("db");
    let anchors: Vec<_> = db
        .iter()
        .filter_map(|entry| entry.certificate_der.as_deref())
        .collect();
    let mut checked = vec![leaf_der.to_vec()];
    result.chain_status = match db_status {
        Status::Missing => ChainStatus::NoMatch,
        Status::Ready if anchors.contains(&leaf_der) => {
            result.anchor_sha256 = Some(fingerprint(leaf_der));
            ChainStatus::Matched
        }
        Status::Ready => match embedded {
            Some(certificates) if !anchors.is_empty() => {
                match build(leaf, leaf_der, certificates, &anchors, None) {
                    Ok(Some(path)) => {
                        result.anchor_sha256 = path.last().map(|cert| fingerprint(cert));
                        result.checked_certificates =
                            path.iter().map(|cert| fingerprint(cert)).collect();
                        checked = path;
                        ChainStatus::Matched
                    }
                    Ok(None)
                        if !db
                            .iter()
                            .any(|entry| entry.invalid || entry.kind.is_empty()) =>
                    {
                        ChainStatus::NoMatch
                    }
                    _ => ChainStatus::Incomplete,
                }
            }
            Some(_)
                if !db
                    .iter()
                    .any(|entry| entry.invalid || entry.kind.is_empty()) =>
            {
                ChainStatus::NoMatch
            }
            _ => ChainStatus::Incomplete,
        },
        _ => ChainStatus::Unavailable,
    };
    let revocations = check_revocations(firmware, &checked);
    result.revocation_status = revocations.status(timestamp);
    result.revocation_times = revocations
        .times
        .iter()
        .map(|(label, _)| (*label).to_string())
        .collect();
    result.revocation_times.sort_unstable();
    result.revocation_times.dedup();
    result
}

pub(super) fn timestamp_anchor(
    leaf: &CERT_CONTEXT,
    leaf_der: &[u8],
    embedded: &[&[u8]],
    firmware: &Report,
    at: OffsetDateTime,
) -> Result<String, TimestampStatus> {
    let (status, database) = firmware.certificate_database("dbt");
    match status {
        Status::Ready => {}
        Status::Missing => return Err(TimestampStatus::Untrusted),
        _ => return Err(TimestampStatus::Unavailable),
    }
    let anchors: Vec<_> = database
        .iter()
        .filter_map(|entry| entry.certificate_der.as_deref())
        .collect();
    if anchors.is_empty() {
        return Err(TimestampStatus::Untrusted);
    }
    let path = if anchors.contains(&leaf_der) {
        if !time_valid(leaf, at) {
            return Err(TimestampStatus::Untrusted);
        }
        vec![leaf_der.to_vec()]
    } else {
        build(leaf, leaf_der, embedded, &anchors, Some(at))
            .map_err(|_| TimestampStatus::Unavailable)?
            .ok_or(TimestampStatus::Untrusted)?
    };
    match check_revocations(firmware, &path).status(Some(at)) {
        RevocationStatus::NoMatch | RevocationStatus::TimestampBeforeRevocation => path
            .last()
            .map(|der| fingerprint(der))
            .ok_or(TimestampStatus::Unavailable),
        RevocationStatus::Revoked => Err(TimestampStatus::Untrusted),
        _ => Err(TimestampStatus::Unavailable),
    }
}

fn build(
    leaf: &CERT_CONTEXT,
    leaf_der: &[u8],
    embedded: &[&[u8]],
    anchors: &[&[u8]],
    at: Option<OffsetDateTime>,
) -> Result<Option<Vec<Vec<u8>>>, ()> {
    let roots = Store::new(anchors).ok_or(())?;
    let intermediates = Store::new(embedded).ok_or(())?;
    let configuration = CERT_CHAIN_ENGINE_CONFIG {
        cbSize: size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32,
        hExclusiveRoot: roots.0,
        dwExclusiveFlags: CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG,
        dwFlags: CERT_CHAIN_DISABLE_AIA | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut engine = null_mut();
    if unsafe { CertCreateCertificateChainEngine(&configuration, &mut engine) } == 0
        || engine.is_null()
    {
        return Err(());
    }
    let engine = Engine(engine);
    let mut parameters = CERT_CHAIN_PARA {
        cbSize: size_of::<CERT_CHAIN_PARA>() as u32,
        ..Default::default()
    };
    let mut usage = [szOID_PKIX_KP_TIMESTAMP_SIGNING.cast_mut()];
    let instant = if let Some(at) = at {
        parameters.RequestedUsage.Usage.cUsageIdentifier = 1;
        parameters.RequestedUsage.Usage.rgpszUsageIdentifier = usage.as_mut_ptr();
        Some(file_time(at).ok_or(())?)
    } else {
        None
    };
    let mut chain = null_mut();
    if unsafe {
        CertGetCertificateChain(
            engine.0,
            leaf,
            instant.as_ref().map_or(null(), |value| value),
            intermediates.0,
            &parameters,
            LOCAL_FLAGS,
            null(),
            &mut chain,
        )
    } == 0
        || chain.is_null()
    {
        return Err(());
    }
    let chain = Chain(chain);
    let context = unsafe { chain.0.as_ref() }.ok_or(())?;
    // EDK2's Authenticode chain validation permits a partial chain ending at
    // an enrolled intermediate and does not check certificate validity time.
    let ignored_errors = if at.is_none() {
        CERT_TRUST_IS_NOT_TIME_VALID
    } else {
        0
    };
    if context.TrustStatus.dwErrorStatus & !ignored_errors != 0 {
        return Ok(None);
    }
    if context.cChain != 1 || context.rgpChain.is_null() {
        return Err(());
    }
    let simple = unsafe { (*context.rgpChain).as_ref() }.ok_or(())?;
    let count = simple.cElement as usize;
    if !(2..=MAX_CHAIN_CERTS).contains(&count) || simple.rgpElement.is_null() {
        return Err(());
    }
    let elements = unsafe { std::slice::from_raw_parts(simple.rgpElement, count) };
    let mut result = Vec::with_capacity(count);
    for (index, element) in elements.iter().enumerate() {
        let certificate =
            unsafe { element.as_ref().and_then(|node| node.pCertContext.as_ref()) }.ok_or(())?;
        if certificate.pbCertEncoded.is_null()
            || !(1..=MAX_CERT_BYTES).contains(&(certificate.cbCertEncoded as usize))
        {
            return Err(());
        }
        if at.is_some_and(|at| !time_valid(certificate, at)) {
            return Ok(None);
        }
        let der = unsafe {
            std::slice::from_raw_parts(
                certificate.pbCertEncoded,
                certificate.cbCertEncoded as usize,
            )
        };
        let known = if index == 0 {
            der == leaf_der
        } else if index + 1 == count {
            anchors.contains(&der)
        } else {
            embedded.contains(&der)
        };
        // Reject any path aided by a cached or system-store certificate, even
        // if the Windows engine claims a match.
        if !known {
            return Err(());
        }
        result.push(der.to_vec());
    }
    Ok(Some(result))
}

const FILETIME_UNIX_EPOCH: i128 = 11_644_473_600_000_000_000;

fn file_time(at: OffsetDateTime) -> Option<FILETIME> {
    let ticks: u64 = at
        .unix_timestamp_nanos()
        .checked_add(FILETIME_UNIX_EPOCH)?
        .checked_div(100)?
        .try_into()
        .ok()?;
    Some(FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    })
}

fn time_valid(certificate: &CERT_CONTEXT, at: OffsetDateTime) -> bool {
    let Some(info) = (unsafe { certificate.pCertInfo.as_ref() }) else {
        return false;
    };
    fn nanos(value: FILETIME) -> i128 {
        let ticks = (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
        i128::from(ticks) * 100 - FILETIME_UNIX_EPOCH
    }
    (nanos(info.NotBefore)..=nanos(info.NotAfter)).contains(&at.unix_timestamp_nanos())
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Default)]
struct Revocations<'a> {
    unavailable: bool,
    permanent: bool,
    incomplete: bool,
    times: Vec<(&'a str, Option<OffsetDateTime>)>,
}

impl Revocations<'_> {
    fn status(&self, timestamp: Option<OffsetDateTime>) -> RevocationStatus {
        if self.unavailable {
            return RevocationStatus::Unavailable;
        }
        if self.permanent {
            return RevocationStatus::Revoked;
        }
        if !self.times.is_empty() {
            let Some(timestamp) = timestamp else {
                return RevocationStatus::TimestampRequired;
            };
            if self
                .times
                .iter()
                .any(|(_, utc)| utc.is_some_and(|cutoff| timestamp >= cutoff))
            {
                return RevocationStatus::Revoked;
            }
            if self.times.iter().any(|(_, utc)| utc.is_none()) {
                return RevocationStatus::TimestampRequired;
            }
            if !self.incomplete {
                return RevocationStatus::TimestampBeforeRevocation;
            }
        }
        if self.incomplete {
            RevocationStatus::Incomplete
        } else {
            RevocationStatus::NoMatch
        }
    }
}

fn check_revocations<'a>(firmware: &'a Report, certificates: &[Vec<u8>]) -> Revocations<'a> {
    let (status, dbx) = firmware.certificate_database("dbx");
    let mut result = Revocations::default();
    match status {
        Status::Missing => return result,
        Status::Ready => {}
        _ => {
            result.unavailable = true;
            return result;
        }
    }
    let tbs: Vec<_> = certificates
        .iter()
        .filter_map(|cert| pkcs::certificate_tbs(cert))
        .collect();
    let sha256: Vec<_> = tbs.iter().map(|&bytes| fingerprint(bytes)).collect();
    let sha384: Vec<_> = tbs
        .iter()
        .map(|&bytes| hex(&Sha384::digest(bytes)))
        .collect();
    let sha512: Vec<_> = tbs
        .iter()
        .map(|&bytes| hex(&Sha512::digest(bytes)))
        .collect();
    result.incomplete = tbs.len() != certificates.len();
    for entry in dbx {
        let digests = match entry.kind {
            "X.509" => {
                match entry
                    .certificate_der
                    .as_deref()
                    .and_then(pkcs::certificate_tbs)
                {
                    Some(forbidden) if sha256.contains(&fingerprint(forbidden)) => {
                        result.permanent = true;
                    }
                    Some(_) => {}
                    None => result.incomplete = true,
                }
                continue;
            }
            "X.509 TBS SHA-256" => &sha256,
            "X.509 TBS SHA-384" => &sha384,
            "X.509 TBS SHA-512" => &sha512,
            "X.509 TBS SM3" | "" => {
                result.incomplete = true;
                continue;
            }
            _ => continue,
        };
        match (entry.digest.as_ref(), &entry.revocation) {
            (Some(digest), Some(Revocation::Always)) if digests.contains(digest) => {
                result.permanent = true;
            }
            (Some(digest), Some(Revocation::At { time, utc })) if digests.contains(digest) => {
                result.times.push((time, *utc))
            }
            (Some(_), Some(_)) => {}
            _ => result.incomplete = true,
        }
    }
    result
}

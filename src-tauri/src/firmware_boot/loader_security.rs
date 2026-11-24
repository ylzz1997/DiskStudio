use serde::Serialize;
use uuid::Uuid;

#[cfg(windows)]
mod chain;
#[cfg(any(windows, test))]
mod pe;
#[cfg(windows)]
mod pkcs;
#[cfg(all(test, windows))]
mod tests;
#[cfg(windows)]
mod timestamp;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    partition: Uuid,
    file_sha256: String,
    digests: Vec<DigestCheck>,
    signatures: Vec<Signature>,
    secure_boot: Option<bool>,
    firmware_error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestCheck {
    algorithm: &'static str,
    digest: String,
    db: Option<super::secure_boot::DigestMatch>,
    dbx: Option<super::secure_boot::DigestMatch>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Signature {
    status: SignatureStatus,
    algorithm: Option<&'static str>,
    signed_digest: Option<String>,
    subject: Option<String>,
    certificate_sha256: Option<String>,
    certificate_trust: Option<CertificateTrust>,
    timestamp: Option<TimestampEvidence>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TimestampEvidence {
    status: TimestampStatus,
    signed_at: Option<String>,
    subject: Option<String>,
    anchor_sha256: Option<String>,
    #[serde(skip)]
    trusted_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum TimestampStatus {
    Verified,
    Invalid,
    Untrusted,
    Unavailable,
    Unsupported,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CertificateTrust {
    chain_status: ChainStatus,
    anchor_sha256: Option<String>,
    checked_certificates: Vec<String>,
    revocation_status: RevocationStatus,
    revocation_times: Vec<String>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum ChainStatus {
    Matched,
    NoMatch,
    Incomplete,
    Unavailable,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum RevocationStatus {
    Revoked,
    TimestampRequired,
    TimestampBeforeRevocation,
    NoMatch,
    Incomplete,
    Unavailable,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum SignatureStatus {
    Verified,
    DigestMismatch,
    Invalid,
    Unsupported,
}

#[cfg(windows)]
pub(super) fn inspect(
    bytes: &[u8],
    partition: Uuid,
    firmware: impl FnOnce() -> Result<super::secure_boot::Report, String>,
) -> Result<Report, String> {
    use sha2::{Digest, Sha256};

    let image = pe::Image::parse(bytes)?;
    let digests = image.digests();
    let records = image.certificates()?;
    let (firmware, firmware_error) = match firmware() {
        Ok(report) => (Some(report), None),
        Err(error) => (None, Some(error)),
    };
    let signatures = records
        .into_iter()
        .flat_map(|record| match record {
            pe::Certificate::Pkcs7(bytes) => pkcs::inspect(bytes, &digests, firmware.as_ref()),
            pe::Certificate::Unsupported => vec![Signature::new(SignatureStatus::Unsupported)],
        })
        .collect();
    let secure_boot = firmware.as_ref().and_then(|report| report.enabled());
    let digests = digests
        .into_iter()
        .map(|digest| {
            let algorithm = digest.algorithm.label();
            let value = hex(&digest.bytes);
            let compare = |name| {
                firmware
                    .as_ref()
                    .map(|report| report.digest_match(name, algorithm, &value))
            };
            DigestCheck {
                algorithm,
                db: compare("db"),
                dbx: compare("dbx"),
                digest: value,
            }
        })
        .collect();
    Ok(Report {
        partition,
        file_sha256: format!("{:x}", Sha256::digest(bytes)),
        digests,
        signatures,
        secure_boot,
        firmware_error,
    })
}

#[cfg(windows)]
impl Signature {
    fn new(status: SignatureStatus) -> Self {
        Self {
            status,
            algorithm: None,
            signed_digest: None,
            subject: None,
            certificate_sha256: None,
            certificate_trust: None,
            timestamp: None,
        }
    }
}

#[cfg(any(windows, test))]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing a string cannot fail");
    }
    output
}

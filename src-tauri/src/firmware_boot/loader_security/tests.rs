use super::{
    inspect,
    pe::{Algorithm, Image},
    pkcs, timestamp, ChainStatus, RevocationStatus, SignatureStatus, TimestampStatus,
};
use base64::Engine;
use sha2::{Digest, Sha256, Sha384, Sha512};
use uuid::Uuid;

const SYNTHETIC_CMS: &str = concat!(
    "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",
);
const EXPECTED_IMAGE_SHA256: &str =
    "567904b06c6d7db0ebcb0726924f3c27ef0bd3d77958f61331c85191607b8612";

// Minimal synthetic PE32+ EFI application. The certificate table can be
// independently generated with a test key without installing certificates.
fn unsigned_image() -> Vec<u8> {
    let mut bytes = vec![0; 0x400];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    for (at, value) in [
        (0x84, 0x8664u16),
        (0x86, 1),
        (0x94, 0xf0),
        (0x96, 0x22),
        (0x98, 0x20b),
        (0xc0, 6),
        (0xc8, 6),
        (0xdc, 10),
    ] {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (at, value) in [
        (0x3c, 0x80u32),
        (0x9c, 0x200),
        (0xa8, 0x1000),
        (0xac, 0x1000),
        (0xb0, 0x400000),
        (0xb8, 0x1000),
        (0xbc, 0x200),
        (0xd0, 0x2000),
        (0xd4, 0x200),
        (0xe0, 0x100000),
        (0xe8, 0x1000),
        (0xf0, 0x100000),
        (0xf8, 0x1000),
        (0x104, 16),
        (0x190, 3),
        (0x194, 0x1000),
        (0x198, 0x200),
        (0x19c, 0x200),
        (0x1ac, 0x60000020),
    ] {
        set_u32(&mut bytes, at, value);
    }
    bytes[0x188..0x18d].copy_from_slice(b".text");
    bytes[0x200..0x203].copy_from_slice(&[0x31, 0xc0, 0xc3]);
    bytes
}

fn set_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn signed_image() -> Vec<u8> {
    signed_image_with_cms(SYNTHETIC_CMS)
}

fn signed_image_with_cms(encoded: &str) -> Vec<u8> {
    let mut bytes = unsigned_image();
    let pkcs = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let length = 8 + pkcs.len();
    let aligned = (length + 7) & !7;
    set_u32(&mut bytes, 0x128, 0x400);
    set_u32(&mut bytes, 0x12c, aligned as u32);
    bytes.extend((length as u32).to_le_bytes());
    bytes.extend(0x200u16.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(pkcs);
    bytes.resize(0x400 + aligned, 0);
    bytes
}

fn review(bytes: &[u8]) -> super::Report {
    inspect(
        bytes,
        Uuid::nil(),
        || Err("test: no firmware access".into()),
    )
    .unwrap()
}

fn signature_list(id: Uuid, payload: &[u8]) -> Vec<u8> {
    let entry = 16 + payload.len() as u32;
    let mut bytes = id.to_bytes_le().to_vec();
    bytes.extend((28 + entry).to_le_bytes());
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(entry.to_le_bytes());
    bytes.extend(Uuid::nil().to_bytes_le());
    bytes.extend(payload);
    bytes
}

const X509: Uuid = uuid::uuid!("a5c059a1-94e4-4aa7-87b5-ab155c2bf072");
const TBS_SHA256: Uuid = uuid::uuid!("3bd2a492-96c0-4079-b420-fcf98ef103ed");
const TBS_SHA384: Uuid = uuid::uuid!("7076876e-80c2-4ee6-aad2-28b349a6865b");
const TBS_SHA512: Uuid = uuid::uuid!("446dbf63-2502-4cda-bcfa-2465d2b0fe9d");

fn review_databases(
    bytes: &[u8],
    allow: Option<Vec<u8>>,
    deny: Result<Option<Vec<u8>>, u32>,
) -> super::Report {
    review_policy(bytes, allow, deny, Ok(None))
}

fn review_policy(
    bytes: &[u8],
    allow: Option<Vec<u8>>,
    deny: Result<Option<Vec<u8>>, u32>,
    authorities: Result<Option<Vec<u8>>, u32>,
) -> super::Report {
    use crate::firmware_boot::{model::Value, secure_boot};
    let value = |bytes| Value {
        attributes: 7,
        bytes,
    };
    inspect(bytes, Uuid::nil(), || {
        secure_boot::capture(|name, _, _| match name {
            "SecureBoot" => Ok(Some(value(vec![1]))),
            "db" => Ok(allow.clone().map(value)),
            "dbx" => deny.clone().map(|bytes| bytes.map(value)),
            "dbt" => authorities.clone().map(|bytes| bytes.map(value)),
            _ => Ok(None),
        })
    })
    .unwrap()
}

#[test]
fn trusted_rfc3161_time_is_compared_with_the_exact_dbx_cutoff() {
    let bytes = signed_image_with_cms(
        &include_str!("timestamp-fixture.b64")
            .lines()
            .collect::<String>(),
    );
    let records = Image::parse(&bytes).unwrap().certificates().unwrap();
    let [super::pe::Certificate::Pkcs7(message)] = records.as_slice() else {
        panic!("expected timestamp fixture")
    };
    let code_certificates = pkcs::certificates_for_test(message);
    let root = find_certificate(&code_certificates, "DiskStudio Firmware Test Root");
    let leaf = find_certificate(&code_certificates, "DiskStudio Authenticode Test");
    let encoded_signer = pkcs::signer_for_test(message);
    let (_, _, tsa_certificates) = timestamp::fixture_parts(&encoded_signer);
    let tsa = find_certificate(&tsa_certificates, "DiskStudio Timestamp Test");
    let allow = signature_list(X509, root);
    let authority = signature_list(X509, root);
    let make_cutoff = |nanos: u32, offset: i16| {
        let mut payload = Sha256::digest(pkcs::certificate_tbs(leaf).unwrap()).to_vec();
        payload.extend([0xea, 0x07, 11, 23, 14, 30, 0, 0]);
        payload.extend(nanos.to_le_bytes());
        payload.extend(offset.to_le_bytes());
        payload.extend([0, 0]);
        signature_list(TBS_SHA256, &payload)
    };
    for (cutoff, zone, expected) in [
        (123_456_701, 0, RevocationStatus::TimestampBeforeRevocation),
        (123_456_700, 0, RevocationStatus::Revoked),
        (123_456_699, 0, RevocationStatus::Revoked),
        (123_456_700, 60, RevocationStatus::TimestampBeforeRevocation),
        (123_456_700, 2047, RevocationStatus::TimestampRequired),
    ] {
        let report = review_policy(
            &bytes,
            Some(allow.clone()),
            Ok(Some(make_cutoff(cutoff, zone))),
            Ok(Some(authority.clone())),
        );
        let signature = &report.signatures[0];
        assert_eq!(signature.status, SignatureStatus::Verified);
        let proof = signature.timestamp.as_ref().unwrap();
        assert_eq!(proof.status, TimestampStatus::Verified);
        assert_eq!(
            proof.signed_at.as_deref(),
            Some("2026-11-23T14:30:00.123456700Z")
        );
        assert_eq!(
            proof.subject.as_deref(),
            Some("CN=DiskStudio Timestamp Test")
        );
        assert_eq!(
            signature
                .certificate_trust
                .as_ref()
                .unwrap()
                .revocation_status,
            expected
        );
    }
    let dated = make_cutoff(123_456_701, 0);
    for (dbt, expected) in [
        (Ok(None), TimestampStatus::Untrusted),
        (Err(5), TimestampStatus::Unavailable),
        (
            Ok(Some(signature_list(X509, leaf))),
            TimestampStatus::Untrusted,
        ),
    ] {
        let report = review_policy(&bytes, Some(allow.clone()), Ok(Some(dated.clone())), dbt);
        let signature = &report.signatures[0];
        assert_eq!(signature.timestamp.as_ref().unwrap().status, expected);
        assert_eq!(
            signature
                .certificate_trust
                .as_ref()
                .unwrap()
                .revocation_status,
            RevocationStatus::TimestampRequired
        );
    }
    let mut permanent = dated.clone();
    permanent.extend(signature_list(X509, leaf));
    let report = review_policy(
        &bytes,
        Some(allow.clone()),
        Ok(Some(permanent)),
        Ok(Some(authority.clone())),
    );
    let signature = &report.signatures[0];
    assert_eq!(
        signature.timestamp.as_ref().unwrap().status,
        TimestampStatus::Verified
    );
    assert_eq!(
        signature
            .certificate_trust
            .as_ref()
            .unwrap()
            .revocation_status,
        RevocationStatus::Revoked
    );

    let mut denied = dated;
    denied.extend(signature_list(X509, tsa));
    let report = review_policy(&bytes, Some(allow), Ok(Some(denied)), Ok(Some(authority)));
    let signature = &report.signatures[0];
    assert_eq!(
        signature.timestamp.as_ref().unwrap().status,
        TimestampStatus::Untrusted
    );
    assert_eq!(
        signature
            .certificate_trust
            .as_ref()
            .unwrap()
            .revocation_status,
        RevocationStatus::TimestampRequired
    );
}

fn find_certificate<'a>(certs: &[&'a [u8]], common_name: &str) -> &'a [u8] {
    certs
        .iter()
        .copied()
        .find(|cert| {
            crate::firmware_boot::secure_boot::certificate_subject(cert)
                .is_some_and(|name| name.contains(common_name))
        })
        .unwrap()
}

#[test]
fn verified_paths_end_only_at_enrolled_certificates() {
    let bytes = signed_image();
    let records = Image::parse(&bytes).unwrap().certificates().unwrap();
    let [super::pe::Certificate::Pkcs7(message)] = records.as_slice() else {
        panic!("expected signed fixture")
    };
    let certs = pkcs::certificates_for_test(message);
    for (name, length) in [
        ("DiskStudio Firmware Test Root", 3),
        ("DiskStudio Firmware Test CA", 2),
        ("DiskStudio Authenticode Test", 1),
    ] {
        let anchor = find_certificate(&certs, name);
        let result = review_databases(&bytes, Some(signature_list(X509, anchor)), Ok(None));
        let trust = result.signatures[0].certificate_trust.as_ref().unwrap();
        assert_eq!(trust.chain_status, ChainStatus::Matched, "{name}");
        assert_eq!(
            trust.anchor_sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(anchor)).as_str())
        );
        assert_eq!(trust.checked_certificates.len(), length);
        assert_eq!(trust.revocation_status, RevocationStatus::NoMatch);
    }
    let report = review_databases(&bytes, None, Err(5));
    let trust = report.signatures[0].certificate_trust.as_ref().unwrap();
    assert_eq!(trust.chain_status, ChainStatus::NoMatch);
    assert_eq!(trust.checked_certificates.len(), 1);
    assert_eq!(trust.revocation_status, RevocationStatus::Unavailable);
}

#[test]
fn revocation_checks_the_chain_and_distinguishes_dated_records() {
    let bytes = signed_image();
    let records = Image::parse(&bytes).unwrap().certificates().unwrap();
    let [super::pe::Certificate::Pkcs7(message)] = records.as_slice() else {
        panic!("expected signed fixture")
    };
    let certs = pkcs::certificates_for_test(message);
    let root = find_certificate(&certs, "DiskStudio Firmware Test Root");
    let issuer = find_certificate(&certs, "DiskStudio Firmware Test CA");
    let allow = signature_list(X509, root);
    let result = |deny| review_databases(&bytes, Some(allow.clone()), Ok(Some(deny)));
    let certificate = result(signature_list(X509, issuer));
    let trust = certificate.signatures[0]
        .certificate_trust
        .as_ref()
        .unwrap();
    assert_eq!(trust.chain_status, ChainStatus::Matched);
    assert_eq!(trust.revocation_status, RevocationStatus::Revoked);

    let tbs = pkcs::certificate_tbs(issuer).unwrap();
    for (kind, digest) in [
        (TBS_SHA256, Sha256::digest(tbs).to_vec()),
        (TBS_SHA384, Sha384::digest(tbs).to_vec()),
        (TBS_SHA512, Sha512::digest(tbs).to_vec()),
    ] {
        let mut dated = digest.clone();
        dated.extend([0xea, 0x07, 11, 23, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut deny = signature_list(kind, &dated);
        let timed = result(deny.clone());
        let trust = timed.signatures[0].certificate_trust.as_ref().unwrap();
        assert_eq!(trust.revocation_status, RevocationStatus::TimestampRequired);
        assert_eq!(
            trust.revocation_times,
            ["2026-11-23T15:00:00.000000000+00:00"]
        );

        let mut permanent = digest;
        permanent.extend([0; 16]);
        deny.extend(signature_list(kind, &permanent));
        let blocked = result(deny);
        assert_eq!(
            blocked.signatures[0]
                .certificate_trust
                .as_ref()
                .unwrap()
                .revocation_status,
            RevocationStatus::Revoked
        );
    }
    let unknown = result(signature_list(Uuid::from_u128(123), &[0xaa; 20]));
    assert_eq!(
        unknown.signatures[0]
            .certificate_trust
            .as_ref()
            .unwrap()
            .revocation_status,
        RevocationStatus::Incomplete
    );
}

#[test]
fn digest_ignores_checksum_and_certificate_table_but_not_code() {
    let mut bytes = unsigned_image();
    let digest = |image: &[u8]| {
        Image::parse(image)
            .unwrap()
            .digests()
            .into_iter()
            .find(|item| item.algorithm == Algorithm::Sha256)
            .unwrap()
            .bytes
    };
    let expected = digest(&bytes);
    assert_eq!(super::hex(&expected), EXPECTED_IMAGE_SHA256);
    let sha1 = Image::parse(&bytes)
        .unwrap()
        .digests()
        .into_iter()
        .find(|item| item.algorithm == Algorithm::Sha1)
        .unwrap();
    assert_eq!(
        super::hex(&sha1.bytes),
        "90567b229621aaec2d9c2d1c813322d6b2c9cdf4"
    );
    assert_eq!(digest(&signed_image()), expected);
    bytes[0xd8] = 91;
    assert_eq!(digest(&bytes), expected);
    bytes[0x200] = 0;
    assert_ne!(digest(&bytes), expected);
}

#[test]
fn validates_pkcs7_and_detects_tampering_in_image_and_message() {
    let mut bytes = signed_image();
    let report = review(&bytes);
    assert_eq!(report.signatures.len(), 1);
    let signature = &report.signatures[0];
    assert_eq!(signature.status, SignatureStatus::Verified);
    assert_eq!(
        signature.signed_digest.as_deref(),
        Some(EXPECTED_IMAGE_SHA256)
    );
    assert_eq!(
        signature.subject.as_deref(),
        Some("CN=DiskStudio Authenticode Test")
    );
    assert!(report.firmware_error.is_some());
    assert!(report
        .digests
        .iter()
        .all(|item| item.db.is_none() && item.dbx.is_none()));
    assert!(review(&unsigned_image()).signatures.is_empty());
    bytes[0x200] ^= 1;
    assert_eq!(
        review(&bytes).signatures[0].status,
        SignatureStatus::DigestMismatch
    );
    let record_length = u32::from_le_bytes(bytes[0x400..0x404].try_into().unwrap()) as usize;
    bytes[0x400 + record_length - 1] ^= 1;
    assert_eq!(
        review(&bytes).signatures[0].status,
        SignatureStatus::Invalid
    );
}

#[test]
fn rejects_unbounded_and_ambiguous_exclusion_ranges() {
    for (at, value) in [
        (0x104, 17),
        (0xd4, 0x180),
        (0x19c, 0x180),
        (0x19c, 0x204),
        (0x198, u32::MAX),
        (0x128, 0x200),
        (0x128, 0x401),
        (0x12c, 0x10),
    ] {
        let mut bytes = signed_image();
        set_u32(&mut bytes, at, value);
        assert!(Image::parse(&bytes).is_err(), "offset {at:x}");
    }
    let mut bytes = signed_image();
    set_u32(&mut bytes, 0x400, u32::MAX);
    assert!(Image::parse(&bytes).unwrap().certificates().is_err());
}

#[test]
fn pe32_without_security_directory_includes_the_entire_tail() {
    let mut bytes = unsigned_image();
    bytes[0x94..0x96].copy_from_slice(&96u16.to_le_bytes());
    bytes[0x98..0x9a].copy_from_slice(&0x10bu16.to_le_bytes());
    set_u32(&mut bytes, 0xf4, 0);
    bytes.copy_within(0x188..0x1b0, 0xf8);
    let first = review(&bytes).digests;
    assert!(review(&bytes).signatures.is_empty());
    bytes.push(0x5a);
    let second = review(&bytes).digests;
    assert!(first
        .iter()
        .zip(second)
        .all(|(previous, current)| previous.digest != current.digest));
}

#[test]
#[ignore = "requires the extracted Windows installation fixture"]
fn verifies_windows_installation_efi_signature() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../.vm/windows-boot-fixture/efi/boot/bootx64.efi");
    let bytes = std::fs::read(fixture).unwrap();
    let report = review(&bytes);
    let signer = report
        .signatures
        .iter()
        .find(|signer| signer.status == SignatureStatus::Verified)
        .expect("Windows EFI signature should verify");
    let signer_sha = signer.certificate_sha256.as_ref().unwrap();
    let records = Image::parse(&bytes).unwrap().certificates().unwrap();
    let authorities: Vec<_> = records
        .into_iter()
        .filter_map(|record| match record {
            super::pe::Certificate::Pkcs7(message) => Some(pkcs::certificates_for_test(message)),
            super::pe::Certificate::Unsupported => None,
        })
        .flatten()
        .filter(|der| format!("{:x}", Sha256::digest(der)) != *signer_sha)
        .collect();
    let db: Vec<_> = authorities
        .iter()
        .flat_map(|der| signature_list(X509, der))
        .collect();
    let anchored = review_databases(&bytes, Some(db.clone()), Ok(None));
    let trust = anchored
        .signatures
        .iter()
        .filter_map(|signature| signature.certificate_trust.as_ref())
        .find(|trust| trust.chain_status == ChainStatus::Matched)
        .expect("Windows EFI signer must chain to an embedded CA enrolled in the synthetic db");
    assert!(trust.checked_certificates.len() > 1);
    let anchor_sha = trust.anchor_sha256.as_ref().unwrap();
    let anchor = authorities
        .iter()
        .find(|der| format!("{:x}", Sha256::digest(der)) == *anchor_sha)
        .unwrap();
    let revoked = review_databases(&bytes, Some(db), Ok(Some(signature_list(X509, anchor))));
    assert!(revoked
        .signatures
        .iter()
        .filter_map(|sig| sig.certificate_trust.as_ref())
        .any(|trust| trust.revocation_status == RevocationStatus::Revoked));
    println!("{}", serde_json::to_string_pretty(&revoked).unwrap());
}

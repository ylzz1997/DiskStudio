use serde::Serialize;

#[cfg(any(windows, test))]
use super::model::Value;
#[cfg(any(windows, test))]
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    flags: Vec<Flag>,
    databases: Vec<Database>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Flag {
    name: &'static str,
    enabled: Option<bool>,
    status: Status,
    error_code: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Database {
    name: &'static str,
    status: Status,
    error_code: Option<u32>,
    sha256: Option<String>,
    entries: Vec<Signature>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Status {
    Ready,
    Missing,
    Invalid,
    Unavailable,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DigestMatch {
    status: Status,
    matches: usize,
    invalid_entries: usize,
}

#[cfg(windows)]
impl Report {
    pub(super) fn enabled(&self) -> Option<bool> {
        self.flags
            .iter()
            .find(|flag| flag.name == "SecureBoot")?
            .enabled
    }

    pub(super) fn certificate_database(&self, name: &str) -> (Status, &[Signature]) {
        self.databases
            .iter()
            .find(|database| database.name == name)
            .map_or((Status::Unavailable, &[]), |database| {
                (database.status, database.entries.as_slice())
            })
    }

    pub(super) fn digest_match(&self, name: &str, algorithm: &str, digest: &str) -> DigestMatch {
        let Some(database) = self.databases.iter().find(|database| database.name == name) else {
            return DigestMatch {
                status: Status::Unavailable,
                matches: 0,
                invalid_entries: 0,
            };
        };
        let mut found = DigestMatch {
            status: database.status,
            matches: 0,
            invalid_entries: 0,
        };
        for entry in database
            .entries
            .iter()
            .filter(|entry| entry.kind == algorithm)
        {
            if entry.invalid {
                found.invalid_entries += 1;
            } else if entry.digest.as_deref() == Some(digest) {
                found.matches += 1;
            }
        }
        found
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Signature {
    signature_type: Uuid,
    owner: Uuid,
    pub(super) kind: &'static str,
    bytes: usize,
    sha256: Option<String>,
    pub(super) digest: Option<String>,
    pub(super) revocation: Option<Revocation>,
    certificate: Option<Certificate>,
    pub(super) invalid: bool,
    #[cfg(windows)]
    #[serde(skip)]
    pub(super) certificate_der: Option<Vec<u8>>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum Revocation {
    Always,
    At {
        time: String,
        #[serde(skip)]
        utc: Option<time::OffsetDateTime>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Certificate {
    subject: Option<String>,
    issuer: Option<String>,
    not_before: Option<String>,
    not_after: Option<String>,
    serial: Option<String>,
}

#[cfg(any(windows, test))]
mod inspect {
    use super::*;
    use std::fmt::Write as _;

    pub(in crate::firmware_boot) const MAX_DATABASE_BYTES: usize = 4 * 1024 * 1024;
    const MAX_DATABASE_ENTRIES: usize = 8192;
    const LIST_HEADER_LEN: usize = 28;
    const GUID_LEN: usize = 16;
    const X509: Uuid = uuid::uuid!("a5c059a1-94e4-4aa7-87b5-ab155c2bf072");
    const SHA256: Uuid = uuid::uuid!("c1c41626-504c-4092-aca9-41f936934328");
    const TBS_SHA256: Uuid = uuid::uuid!("3bd2a492-96c0-4079-b420-fcf98ef103ed");
    const FLAGS: [&str; 4] = ["SecureBoot", "SetupMode", "AuditMode", "DeployedMode"];
    const DATABASES: [&str; 5] = ["PK", "KEK", "db", "dbx", "dbt"];
    pub(in crate::firmware_boot) const CHANGED: &str =
        "安全启动固件变量在读取期间已变化，请重新读取。";

    pub(in crate::firmware_boot) fn capture(
        mut read: impl FnMut(&'static str, bool, usize) -> Result<Option<Value>, u32>,
        certificate: impl Fn(&[u8]) -> Option<Certificate>,
    ) -> Result<Report, String> {
        let mut read_flag = |name| read(name, false, 4);
        let flags = FLAGS.map(&mut read_flag);
        let databases = DATABASES.map(|name| {
            let image_namespace = matches!(name, "db" | "dbx" | "dbt");
            read(name, image_namespace, MAX_DATABASE_BYTES)
        });
        // A concurrent update can invalidate a view assembled from different
        // variables even when every individual read was well formed.
        for (name, expected) in DATABASES.into_iter().zip(&databases) {
            let image_namespace = matches!(name, "db" | "dbx" | "dbt");
            if read(name, image_namespace, MAX_DATABASE_BYTES) != *expected {
                return Err(CHANGED.into());
            }
        }
        for (name, expected) in FLAGS.into_iter().zip(&flags) {
            if read(name, false, 4) != *expected {
                return Err(CHANGED.into());
            }
        }
        Ok(Report {
            flags: FLAGS
                .into_iter()
                .zip(flags)
                .map(|(name, value)| flag(name, value))
                .collect(),
            databases: DATABASES
                .into_iter()
                .zip(databases)
                .map(|(name, value)| database(name, value, &certificate))
                .collect(),
        })
    }

    fn flag(name: &'static str, value: Result<Option<Value>, u32>) -> Flag {
        let (status, enabled, error_code) = match value {
            Ok(Some(value)) => match value.bytes.as_slice() {
                [0] => (Status::Ready, Some(false), None),
                [1] => (Status::Ready, Some(true), None),
                _ => (Status::Invalid, None, None),
            },
            Ok(None) => (Status::Missing, None, None),
            Err(code) => (Status::Unavailable, None, Some(code)),
        };
        Flag {
            name,
            enabled,
            status,
            error_code,
        }
    }

    fn database(
        name: &'static str,
        value: Result<Option<Value>, u32>,
        certificate: &impl Fn(&[u8]) -> Option<Certificate>,
    ) -> Database {
        let (status, error_code, sha256, entries) = match value {
            Ok(Some(value)) => {
                let sha256 = Some(format!("{:x}", Sha256::digest(&value.bytes)));
                match parse(&value.bytes, certificate) {
                    Some(entries) => (Status::Ready, None, sha256, entries),
                    None => (Status::Invalid, None, sha256, Vec::new()),
                }
            }
            Ok(None) => (Status::Missing, None, None, Vec::new()),
            Err(code) => (Status::Unavailable, Some(code), None, Vec::new()),
        };
        Database {
            name,
            status,
            error_code,
            sha256,
            entries,
        }
    }

    // EFI_SIGNATURE_LIST and EFI_SIGNATURE_DATA use EFI GUID byte order.
    fn parse(
        mut bytes: &[u8],
        certificate: &impl Fn(&[u8]) -> Option<Certificate>,
    ) -> Option<Vec<Signature>> {
        if bytes.len() > MAX_DATABASE_BYTES {
            return None;
        }
        let mut entries = Vec::new();
        while !bytes.is_empty() {
            let header = bytes.get(..LIST_HEADER_LEN)?;
            let signature_type = Uuid::from_bytes_le(header[..GUID_LEN].try_into().ok()?);
            let list_len = le32(&header[16..20]) as usize;
            let header_len = le32(&header[20..24]) as usize;
            let signature_len = le32(&header[24..28]) as usize;
            if signature_len <= GUID_LEN {
                return None;
            }
            let signatures = bytes.get(LIST_HEADER_LEN.checked_add(header_len)?..list_len)?;
            if signatures.len() % signature_len != 0 {
                return None;
            }
            let count = signatures.len() / signature_len;
            if entries.len().checked_add(count)? > MAX_DATABASE_ENTRIES {
                return None;
            }
            let format = signature_format(signature_type);
            for entry in signatures.chunks_exact(signature_len) {
                let owner = Uuid::from_bytes_le(entry[..GUID_LEN].try_into().ok()?);
                let payload = &entry[GUID_LEN..];
                let mut record = Signature {
                    signature_type,
                    owner,
                    kind: format.label(),
                    bytes: payload.len(),
                    sha256: None,
                    digest: None,
                    revocation: None,
                    certificate: None,
                    invalid: header_len != 0 && !matches!(format, Format::Other),
                    #[cfg(windows)]
                    certificate_der: None,
                };
                if !record.invalid {
                    match format {
                        Format::X509 => {
                            record.certificate = certificate(payload);
                            record.invalid = record.certificate.is_none();
                            #[cfg(windows)]
                            if !record.invalid {
                                record.certificate_der = Some(payload.to_vec());
                            }
                        }
                        Format::Digest(_, len) if payload.len() == len => {
                            record.digest = Some(hex(payload.iter()));
                        }
                        Format::Revoked(_, len) if payload.len() == len + 16 => {
                            record.revocation = revocation_time(&payload[len..]);
                            record.invalid = record.revocation.is_none();
                            if !record.invalid {
                                record.digest = Some(hex(payload[..len].iter()));
                            }
                        }
                        Format::Rsa(_) if payload.len() == 256 => {}
                        Format::Other => {}
                        _ => record.invalid = true,
                    }
                }
                if record.digest.is_none() {
                    record.sha256 = Some(format!("{:x}", Sha256::digest(payload)));
                }
                entries.push(record);
            }
            bytes = &bytes[list_len..];
        }
        Some(entries)
    }

    enum Format {
        X509,
        Digest(&'static str, usize),
        Revoked(&'static str, usize),
        Rsa(&'static str),
        Other,
    }

    impl Format {
        fn label(&self) -> &'static str {
            match self {
                Self::X509 => "X.509",
                Self::Digest(label, _) | Self::Revoked(label, _) | Self::Rsa(label) => label,
                Self::Other => "",
            }
        }
    }

    fn signature_format(id: Uuid) -> Format {
        use Format::{Digest, Revoked, Rsa};
        match id {
            X509 => Format::X509,
            SHA256 => Digest("SHA-256", 32),
            TBS_SHA256 => Revoked("X.509 TBS SHA-256", 32),
            id if id == uuid::uuid!("826ca512-cf10-4ac9-b187-be01496631bd") => Digest("SHA-1", 20),
            id if id == uuid::uuid!("0b6e5233-a65c-44c9-9407-d9ab83bfc8bd") => {
                Digest("SHA-224", 28)
            }
            id if id == uuid::uuid!("ff3e5307-9fd0-48c9-85f1-8ad56c701e01") => {
                Digest("SHA-384", 48)
            }
            id if id == uuid::uuid!("093e0fae-a6c4-4f50-9f1b-d41e2b89c19a") => {
                Digest("SHA-512", 64)
            }
            id if id == uuid::uuid!("57347f87-7a9b-403a-b93c-dc4afb7a0ebc") => Digest("SM3", 32),
            id if id == uuid::uuid!("7076876e-80c2-4ee6-aad2-28b349a6865b") => {
                Revoked("X.509 TBS SHA-384", 48)
            }
            id if id == uuid::uuid!("446dbf63-2502-4cda-bcfa-2465d2b0fe9d") => {
                Revoked("X.509 TBS SHA-512", 64)
            }
            id if id == uuid::uuid!("60d807e5-10b4-49a9-9331-e40437888d37") => {
                Revoked("X.509 TBS SM3", 32)
            }
            id if id == uuid::uuid!("3c5766e8-269c-4e34-aa14-ed776e85b3b6") => Rsa("RSA-2048"),
            id if id == uuid::uuid!("e2b36190-879b-4a3d-ad8d-f2e7bba32784") => {
                Rsa("RSA-2048 SHA-256")
            }
            id if id == uuid::uuid!("67f8444f-8743-48f1-a328-1eaab8736080") => {
                Rsa("RSA-2048 SHA-1")
            }
            _ => Format::Other,
        }
    }

    fn revocation_time(bytes: &[u8]) -> Option<Revocation> {
        if bytes.iter().all(|&byte| byte == 0) {
            return Some(Revocation::Always);
        }
        let year = u16::from_le_bytes(bytes[..2].try_into().ok()?);
        let date = time::Date::from_calendar_date(year.into(), bytes[2].try_into().ok()?, bytes[3])
            .ok()?;
        let nanos = le32(&bytes[8..12]);
        let time = time::Time::from_hms_nano(bytes[4], bytes[5], bytes[6], nanos).ok()?;
        // EFI_TIME's zone is minutes west of UTC, unlike an ISO 8601 offset.
        let zone = i16::from_le_bytes(bytes[12..14].try_into().ok()?);
        let (zone, utc) = match zone {
            2047 => (String::new(), None),
            -1440..=1440 => {
                let sign = if zone > 0 { '-' } else { '+' };
                let minutes = zone.unsigned_abs();
                let utc = date
                    .with_time(time)
                    .assume_utc()
                    .checked_add(time::Duration::minutes(i64::from(zone)));
                (
                    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60),
                    utc,
                )
            }
            _ => return None,
        };
        if !(1900..=9999).contains(&year) || bytes[14] & !3 != 0 {
            return None;
        }
        Some(Revocation::At {
            time: format!(
                "{date}T{:02}:{:02}:{:02}.{nanos:09}{zone}",
                time.hour(),
                time.minute(),
                time.second()
            ),
            utc,
        })
    }

    fn le32(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes.try_into().expect("four-byte UEFI field"))
    }

    pub(super) fn hex<'a>(bytes: impl Iterator<Item = &'a u8>) -> String {
        let mut result = String::new();
        for byte in bytes {
            write!(result, "{byte:02x}").expect("writing to a String cannot fail");
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const OWNER: Uuid = uuid::uuid!("11223344-5566-7788-99aa-bbccddeeff00");

        fn list(kind: Uuid, header: &[u8], payloads: &[&[u8]]) -> Vec<u8> {
            let signature_len = GUID_LEN + payloads[0].len();
            let list_len = LIST_HEADER_LEN + header.len() + signature_len * payloads.len();
            let mut bytes = kind.to_bytes_le().to_vec();
            bytes.extend((list_len as u32).to_le_bytes());
            bytes.extend((header.len() as u32).to_le_bytes());
            bytes.extend((signature_len as u32).to_le_bytes());
            bytes.extend(header);
            for payload in payloads {
                assert_eq!(payload.len() + GUID_LEN, signature_len);
                bytes.extend(OWNER.to_bytes_le());
                bytes.extend(*payload);
            }
            bytes
        }

        fn value(bytes: Vec<u8>) -> Result<Option<Value>, u32> {
            Ok(Some(Value {
                attributes: 7,
                bytes,
            }))
        }

        #[test]
        fn parses_mixed_lists_and_preserves_unrecognized_types() {
            let mut bytes = list(SHA256, &[], &[&[0xa5; 32], &[0x14; 32]]);
            bytes.extend(list(OWNER, &[1, 2, 3, 4], &[&[8, 9]]));
            bytes.extend(list(
                uuid::uuid!("3c5766e8-269c-4e34-aa14-ed776e85b3b6"),
                &[],
                &[&[1; 256]],
            ));
            let entries = parse(&bytes, &|_| None).unwrap();
            assert_eq!(entries.len(), 4);
            assert_eq!(entries[0].signature_type, SHA256);
            assert_eq!(entries[1].owner, OWNER);
            assert_eq!(entries[0].digest.as_deref(), Some("a5".repeat(32).as_str()));
            assert!(entries[0].sha256.is_none());
            assert!(entries[2].kind.is_empty());
            assert!(!entries[2].invalid);
            assert_eq!(
                entries[2].sha256,
                Some(format!("{:x}", Sha256::digest([8, 9])))
            );
            assert_eq!(entries[3].kind, "RSA-2048");
            assert!(!entries[3].invalid);
        }

        #[test]
        fn rejects_bad_bounds_and_preserves_other_databases() {
            let bytes = list(SHA256, &[], &[&[0x14; 32]]);
            for offset in [16, 20, 24] {
                let mut damaged = bytes.clone();
                damaged[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
                assert!(parse(&damaged, &|_| None).is_none());
            }
            let mut damaged = bytes.clone();
            damaged.pop();
            let report = capture(
                |name, image_namespace, cap| {
                    assert_eq!(image_namespace, matches!(name, "db" | "dbx" | "dbt"));
                    assert_eq!(
                        cap,
                        if DATABASES.contains(&name) {
                            MAX_DATABASE_BYTES
                        } else {
                            4
                        }
                    );
                    match name {
                        "db" => value(bytes.clone()),
                        "dbx" => value(damaged.clone()),
                        "KEK" => Err(5),
                        "SecureBoot" => value(vec![1]),
                        "SetupMode" => value(vec![0]),
                        "AuditMode" => value(vec![2]),
                        _ => Ok(None),
                    }
                },
                |_| None,
            )
            .unwrap();
            assert_eq!(report.flags[0].enabled, Some(true));
            assert_eq!(report.flags[1].enabled, Some(false));
            assert_eq!(report.flags[2].status, Status::Invalid);
            assert_eq!(report.flags[3].status, Status::Missing);
            assert_eq!(report.databases[0].status, Status::Missing);
            assert_eq!(report.databases[1].status, Status::Unavailable);
            assert_eq!(report.databases[1].error_code, Some(5));
            assert_eq!(report.databases[2].entries.len(), 1);
            assert_eq!(report.databases[3].status, Status::Invalid);
            assert!(report.databases[3].entries.is_empty());
        }

        #[test]
        fn rejects_oversize_database_and_too_many_entries() {
            assert!(parse(&vec![0; MAX_DATABASE_BYTES + 1], &|_| None).is_none());
            let payloads = vec![&[1][..]; MAX_DATABASE_ENTRIES + 1];
            assert!(parse(&list(OWNER, &[], &payloads), &|_| None).is_none());
        }

        #[test]
        fn decodes_revocation_hashes_and_rejects_invalid_known_entries() {
            let mut dated = [0x33; 48];
            let mut timestamp = [0; 16];
            timestamp[..2].copy_from_slice(&2026u16.to_le_bytes());
            timestamp[2..7].copy_from_slice(&[11, 23, 6, 15, 10]);
            timestamp[12..14].copy_from_slice(&(-480i16).to_le_bytes());
            dated[32..].copy_from_slice(&timestamp);
            let mut permanent = dated;
            permanent[32..].fill(0);
            let mut invalid = dated;
            invalid[34] = 13;
            let mut bytes = list(TBS_SHA256, &[], &[&dated, &permanent, &invalid]);
            bytes.extend(list(SHA256, &[], &[&[0; 31]]));
            bytes.extend(list(SHA256, &[1], &[&[0; 32]]));
            let parsed = parse(&bytes, &|_| None).unwrap();
            assert_eq!(parsed[0].digest, Some("33".repeat(32)));
            assert!(
                matches!(&parsed[0].revocation, Some(Revocation::At { time, utc: Some(utc) })
                    if time == "2026-11-23T06:15:10.000000000+08:00" && *utc == time::OffsetDateTime::from_unix_timestamp(1795385710).unwrap())
            );
            assert!(matches!(&parsed[1].revocation, Some(Revocation::Always)));
            assert!(parsed[2..]
                .iter()
                .all(|entry| entry.invalid && entry.digest.is_none()));
        }

        #[test]
        fn rejects_variable_changes_between_reads() {
            for target in ["SecureBoot", "db", "dbx", "dbt"] {
                let mut updated = false;
                let result = capture(
                    |name, _, _| {
                        if name == target {
                            if std::mem::replace(&mut updated, true) {
                                return value(vec![1]);
                            }
                            return value(vec![0]);
                        }
                        Ok(None)
                    },
                    |_| None,
                );
                assert!(matches!(result, Err(reason) if reason == CHANGED));
            }
        }

        #[cfg(windows)]
        #[test]
        fn image_digest_comparison_keeps_db_and_dbx_separate_from_tbs() {
            let digest = [0xab; 32];
            let mut revoked = [0; 48];
            revoked[..32].copy_from_slice(&digest);
            let allow = list(SHA256, &[], &[&digest]);
            let mut deny = list(TBS_SHA256, &[], &[&revoked]);
            deny.extend(list(SHA256, &[], &[&[0x23; 32]]));
            deny.extend(list(SHA256, &[], &[&[0; 31]]));
            let report = capture(
                |name, _, _| match name {
                    "db" => value(allow.clone()),
                    "dbx" => value(deny.clone()),
                    _ => Ok(None),
                },
                |_| None,
            )
            .unwrap();
            let selected = "ab".repeat(32);
            assert_eq!(report.digest_match("db", "SHA-256", &selected).matches, 1);
            let denied = report.digest_match("dbx", "SHA-256", &selected);
            assert_eq!(denied.status, Status::Ready);
            assert_eq!(denied.matches, 0);
            assert_eq!(denied.invalid_entries, 1);
            assert_eq!(
                report
                    .digest_match("dbx", "SHA-256", &"23".repeat(32))
                    .matches,
                1
            );
        }

        #[cfg(windows)]
        #[test]
        fn inspects_der_x509_and_rejects_appended_or_malformed_data() {
            use base64::Engine;
            // Self-signed synthetic P-256 certificate generated for this test.
            let leaf = base64::engine::general_purpose::STANDARD.decode(concat!(
                "MIIBtzCCAV6gAwIBAgIDASNFMAoGCCqGSM49BAMCMDoxHTAbBgNVBAMMFERpc2tTdHVkaW8gVGVzdCBSb290",
                "MRkwFwYDVQQKDBBEaXNrU3R1ZGlvIFRlc3RzMB4XDTI2MTEyMzA2MjQ1NFoXDTI3MTEyMzA2MjQ1NFowOjEd",
                "MBsGA1UEAwwURGlza1N0dWRpbyBUZXN0IFJvb3QxGTAXBgNVBAoMEERpc2tTdHVkaW8gVGVzdHMwWTATBgcq",
                "hkjOPQIBBggqhkjOPQMBBwNCAAT2l8QZLddFWpPtutlECZK59mc4lip67+0/iLyx8ZUKfBGSQyPwyL7Gvqzi",
                "u2mRGXa7CJhWXaxM6YFLWvw93lGYo1MwUTAdBgNVHQ4EFgQU66eJeg0Pzegq19PqeDhEELpmWTMwHwYDVR0j",
                "BBgwFoAU66eJeg0Pzegq19PqeDhEELpmWTMwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBC",
                "qPVkU7TXJoerCMslMymBjeJjsUokPQ4WUkUInbe9vAIgEw0jVgZpWe/ki/8RfhqvwGJpBZODQXvQb0y1A38SU4c="
            )).unwrap();
            let mut appended = leaf.clone();
            appended.push(0);
            let mut bytes = list(X509, &[], &[&leaf]);
            bytes.extend(list(X509, &[], &[&appended]));
            bytes.extend(list(X509, &[], &[&[0x30, 0]]));
            let parsed = parse(
                &bytes,
                &crate::firmware_boot::secure_boot::certificate::decode,
            )
            .unwrap();
            let cert = parsed[0].certificate.as_ref().unwrap();
            assert!(cert
                .subject
                .as_deref()
                .unwrap()
                .contains("DiskStudio Test Root"));
            assert_eq!(cert.subject, cert.issuer);
            assert_eq!(cert.serial.as_deref(), Some("012345"));
            assert_eq!(cert.not_before.as_deref(), Some("2026-11-23T06:24:54Z"));
            assert_eq!(cert.not_after.as_deref(), Some("2027-11-23T06:24:54Z"));
            assert!(parsed[1..]
                .iter()
                .all(|entry| entry.invalid && entry.certificate.is_none()));
        }
    }
}

#[cfg(windows)]
mod certificate;

#[cfg(windows)]
pub(super) fn certificate_subject(bytes: &[u8]) -> Option<String> {
    certificate::decode(bytes)?.subject
}

#[cfg(windows)]
pub(super) fn capture(
    read: impl FnMut(&'static str, bool, usize) -> Result<Option<Value>, u32>,
) -> Result<Report, String> {
    inspect::capture(read, certificate::decode)
}

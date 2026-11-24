use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use std::ops::Range;
use uuid::Uuid;

const MAX_SECTIONS: usize = 96;
const SECTION_HEADER_SIZE: usize = 40;
const MAX_CERTIFICATES: usize = 16;
const MAX_CERT_TABLE: usize = 4 * 1024 * 1024;
const PKCS7: Uuid = uuid::uuid!("4aafd29d-68df-49ee-8aa9-347d375665a7");
const INVALID: &str = "EFI 加载器的 PE 布局无效或不受支持。";
const INVALID_CERTIFICATES: &str = "EFI 加载器的证书表无效或不受支持。";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Algorithm {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA-1",
            Self::Sha224 => "SHA-224",
            Self::Sha256 => "SHA-256",
            Self::Sha384 => "SHA-384",
            Self::Sha512 => "SHA-512",
        }
    }
}

pub(super) struct ImageDigest {
    pub(super) algorithm: Algorithm,
    pub(super) bytes: Vec<u8>,
}

pub(super) struct Image<'a> {
    bytes: &'a [u8],
    checksum: Range<usize>,
    security_directory: Option<Range<usize>>,
    certificates: Option<Range<usize>>,
}

pub(super) enum Certificate<'a> {
    Pkcs7(&'a [u8]),
    Unsupported,
}

impl<'a> Image<'a> {
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if !bytes.starts_with(b"MZ") {
            return Err(INVALID);
        }
        let pe = u32_at(bytes, 0x3c)?;
        if pe < 0x40
            || !bytes
                .get(pe..)
                .is_some_and(|rest| rest.starts_with(b"PE\0\0"))
        {
            return Err(INVALID);
        }
        let coff_start = pe.checked_add(4).ok_or(INVALID)?;
        let coff = take(bytes, coff_start, 20)?;
        let sections = u16_at(coff, 2)?;
        if !(1..=MAX_SECTIONS).contains(&sections) {
            return Err(INVALID);
        }
        let optional_start = coff_start + coff.len();
        let optional = take(bytes, optional_start, u16_at(coff, 16)?)?;
        let (count_offset, directory_offset) = match u16_at(optional, 0)? {
            0x10b => (92, 96),
            0x20b => (108, 112),
            _ => return Err(INVALID),
        };
        let header_len = u32_at(optional, 60)?;
        let directory_count = u32_at(optional, count_offset)?;
        if directory_count > (optional.len() - directory_offset) / 8 {
            return Err(INVALID);
        }
        let checksum = optional_start + 64..optional_start + 68;
        let section_table = optional_start + optional.len();
        let section_bytes = take(bytes, section_table, sections * SECTION_HEADER_SIZE)?;
        if header_len < section_table + section_bytes.len() || header_len > bytes.len() {
            return Err(INVALID);
        }

        // On contiguous files the EDK2 ordered raw-section hash and the
        // additional-data hash exactly cover these bytes, without re-reading
        // ranges. Gaps or overlap make the additional-data boundary ambiguous.
        let mut raw_ranges = Vec::new();
        for section in section_bytes.chunks_exact(SECTION_HEADER_SIZE) {
            let size = u32_at(section, 16)?;
            if size != 0 {
                raw_ranges.push((u32_at(section, 20)?, size));
            }
        }
        if raw_ranges.is_empty() {
            return Err(INVALID);
        }
        raw_ranges.sort_unstable();
        let mut end = header_len;
        for (start, size) in raw_ranges {
            if start != end {
                return Err(INVALID);
            }
            end = start
                .checked_add(size)
                .filter(|&end| end <= bytes.len())
                .ok_or(INVALID)?;
        }
        let (security_directory, certificates) = if directory_count > 4 {
            let directory = directory_offset + 4 * 8;
            let start = u32_at(optional, directory)?;
            let size = u32_at(optional, directory + 4)?;
            let certificates = match (start, size) {
                (0, 0) => None,
                (start, size)
                    if start >= end
                        && start % 8 == 0
                        && (8..=MAX_CERT_TABLE).contains(&size)
                        && start.checked_add(size) == Some(bytes.len()) =>
                {
                    Some(start..bytes.len())
                }
                _ => return Err(INVALID_CERTIFICATES),
            };
            let offset = optional_start + directory;
            (Some(offset..offset + 8), certificates)
        } else {
            (None, None)
        };
        Ok(Self {
            bytes,
            checksum,
            security_directory,
            certificates,
        })
    }

    pub(super) fn certificates(&self) -> Result<Vec<Certificate<'a>>, &'static str> {
        let Some(range) = &self.certificates else {
            return Ok(Vec::new());
        };
        let mut bytes = &self.bytes[range.clone()];
        let mut records = Vec::new();
        while !bytes.is_empty() {
            if bytes.len() < 8 || records.len() >= MAX_CERTIFICATES {
                return Err(INVALID_CERTIFICATES);
            }
            let size = u32_at(bytes, 0)?;
            let revision = u16_at(bytes, 4)?;
            let kind = u16_at(bytes, 6)?;
            let record = bytes
                .get(8..size)
                .filter(|part| !part.is_empty())
                .ok_or(INVALID_CERTIFICATES)?;
            let aligned = size.checked_add(7).ok_or(INVALID_CERTIFICATES)? & !7;
            let padding = bytes.get(size..aligned).ok_or(INVALID_CERTIFICATES)?;
            if padding.iter().any(|&byte| byte != 0) {
                return Err(INVALID_CERTIFICATES);
            }
            let certificate = match (revision, kind) {
                (0x200, 0x02) => Certificate::Pkcs7(record),
                (0x200, 0x0ef1) if record.len() > 16 => {
                    let guid = Uuid::from_bytes_le(
                        record[..16].try_into().map_err(|_| INVALID_CERTIFICATES)?,
                    );
                    if guid == PKCS7 {
                        Certificate::Pkcs7(&record[16..])
                    } else {
                        Certificate::Unsupported
                    }
                }
                _ => Certificate::Unsupported,
            };
            records.push(certificate);
            bytes = &bytes[aligned..];
        }
        Ok(records)
    }

    pub(super) fn digests(&self) -> Vec<ImageDigest> {
        let mut results = vec![
            self.digest::<Sha224>(Algorithm::Sha224),
            self.digest::<Sha256>(Algorithm::Sha256),
            self.digest::<Sha384>(Algorithm::Sha384),
            self.digest::<Sha512>(Algorithm::Sha512),
        ];
        #[cfg(windows)]
        if let Some(bytes) = sha1(self.hash_ranges()) {
            results.insert(
                0,
                ImageDigest {
                    algorithm: Algorithm::Sha1,
                    bytes: bytes.to_vec(),
                },
            );
        }
        results
    }

    fn hash_ranges(&self) -> [&[u8]; 3] {
        let end = self
            .certificates
            .as_ref()
            .map_or(self.bytes.len(), |table| table.start);
        let head = &self.bytes[..self.checksum.start];
        if let Some(directory) = &self.security_directory {
            [
                head,
                &self.bytes[self.checksum.end..directory.start],
                &self.bytes[directory.end..end],
            ]
        } else {
            [head, &self.bytes[self.checksum.end..end], &[]]
        }
    }

    fn digest<D: Digest>(&self, algorithm: Algorithm) -> ImageDigest {
        let mut hash = D::new();
        for bytes in self.hash_ranges() {
            hash.update(bytes);
        }
        ImageDigest {
            algorithm,
            bytes: hash.finalize().to_vec(),
        }
    }
}

#[cfg(windows)]
pub(super) fn sha1(ranges: [&[u8]; 3]) -> Option<[u8; 20]> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Security::Cryptography::{
        BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptHashData, BCRYPT_HASH_HANDLE,
        BCRYPT_SHA1_ALG_HANDLE,
    };

    struct Hash(BCRYPT_HASH_HANDLE);
    impl Drop for Hash {
        fn drop(&mut self) {
            unsafe { BCryptDestroyHash(self.0) };
        }
    }

    let mut raw = null_mut();
    let status = unsafe {
        BCryptCreateHash(
            BCRYPT_SHA1_ALG_HANDLE,
            &mut raw,
            null_mut(),
            0,
            null(),
            0,
            0,
        )
    };
    if status != 0 || raw.is_null() {
        return None;
    }
    let hash = Hash(raw);
    for bytes in ranges.into_iter().filter(|value| !value.is_empty()) {
        if unsafe { BCryptHashData(hash.0, bytes.as_ptr(), bytes.len().try_into().ok()?, 0) } != 0 {
            return None;
        }
    }
    let mut digest = [0u8; 20];
    let status = unsafe { BCryptFinishHash(hash.0, digest.as_mut_ptr(), digest.len() as u32, 0) };
    (status == 0).then_some(digest)
}

fn take(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], &'static str> {
    bytes
        .get(at..at.checked_add(len).ok_or(INVALID)?)
        .ok_or(INVALID)
}

fn u16_at(bytes: &[u8], at: usize) -> Result<usize, &'static str> {
    Ok(usize::from(u16::from_le_bytes(
        take(bytes, at, 2)?.try_into().map_err(|_| INVALID)?,
    )))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<usize, &'static str> {
    Ok(u32::from_le_bytes(take(bytes, at, 4)?.try_into().map_err(|_| INVALID)?) as usize)
}

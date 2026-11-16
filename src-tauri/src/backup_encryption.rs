//! Authenticated, seekable envelope for the existing backup stream. No paths,
//! dialogs or devices. A caller supplies a new ordinary output file, publishes
//! it only after full archive-chain readback, and retains immutable input handles.
//! Unfinished output has no valid authenticated header. Never write plaintext
//! temporary files, automatically retry failed writes or flush on Drop.
//!
//! Format and cryptographic construction: docs/backup-encryption-format.md.
use crate::backup_archive::Output;
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{aead::AeadInPlace, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use std::io::{self, Read, Seek, SeekFrom, Write};
use zeroize::{Zeroize, Zeroizing};

pub const MAGIC: &[u8; 8] = b"DSBKENC1";
const HEADER: usize = 4096;
const PREFIX: usize = 64;
const BLOCK: usize = 1024 * 1024;
const RECORD: usize = BLOCK + 24 + 16;
const MAX_SIZE: u64 = 8 * 1024 * 1024 * 1024 * 1024;
const MEMORY_KIB: u32 = 65536;
const ITERATIONS: u32 = 3;
const LANES: u32 = 4;

fn error(message: &str) -> io::Error {
    io::Error::other(message)
}
fn crypto_error() -> io::Error {
    error("备份密码不正确，或加密内容已损坏。")
}
fn random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|_| error("系统随机数生成失败，已停止加密备份。"))
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn position(pos: u64, len: u64, from: SeekFrom) -> io::Result<u64> {
    let n = match from {
        SeekFrom::Start(n) => n as i128,
        SeekFrom::End(n) => len as i128 + n as i128,
        SeekFrom::Current(n) => pos as i128 + n as i128,
    };
    if n < 0 || n > len as i128 {
        return Err(error("定位超出加密备份范围。"));
    }
    Ok(n as u64)
}
pub fn storage_bound(plain: u64) -> io::Result<u64> {
    if plain == 0 || plain > MAX_SIZE {
        return Err(error("加密备份容量超出支持范围。"));
    }
    Ok(HEADER as u64 + plain.div_ceil(BLOCK as u64) * RECORD as u64)
}

/// No Debug/Serialize implementation. Ownership clears the retained bytes.
/// UI/IPC callers must also avoid persistence and clear their password fields.
pub struct Password(Zeroizing<String>);
impl Password {
    pub fn new(value: String) -> io::Result<Self> {
        let value = Zeroizing::new(value);
        if value.is_empty() || value.len() > 1024 {
            return Err(error("备份密码须为 1–1024 字节。"));
        }
        Ok(Self(value))
    }
    pub(crate) fn validate_creation(&self) -> io::Result<()> {
        if self.0.chars().count() < 8 {
            return Err(error("新备份密码至少需要 8 个字符。"));
        }
        Ok(())
    }
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.0.as_bytes() == other.0.as_bytes()
    }
}
impl<'de> serde::Deserialize<'de> for Password {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}
fn derive(password: &Password, prefix: &[u8; PREFIX]) -> io::Result<Zeroizing<[u8; 32]>> {
    // Fixed allowlisted costs are checked before allocation; untrusted input
    // cannot choose arbitrary memory, iterations or parallelism.
    validate_prefix(prefix)?;
    let params = Params::new(MEMORY_KIB, ITERATIONS, LANES, Some(32))
        .map_err(|_| error("密钥参数无效。"))?;
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(
            password.0.as_bytes(),
            &prefix[28..44],
            &mut *key,
            &mut *memory,
        )
        .map_err(|_| error("备份密钥派生失败。"))?;
    Ok(key)
}
fn make_prefix() -> io::Result<[u8; PREFIX]> {
    let mut p = [0; PREFIX];
    p[..8].copy_from_slice(MAGIC);
    for (offset, n) in [
        (8, BLOCK as u32),
        (12, 0x13),
        (16, MEMORY_KIB),
        (20, ITERATIONS),
        (24, LANES),
    ] {
        p[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
    }
    random(&mut p[28..60])?; // independent salt and file identity
    Ok(p)
}
fn validate_prefix(p: &[u8; PREFIX]) -> io::Result<()> {
    if &p[..8] != MAGIC
        || u32_at(p, 8) != BLOCK as u32
        || u32_at(p, 12) != 0x13
        || u32_at(p, 16) != MEMORY_KIB
        || u32_at(p, 20) != ITERATIONS
        || u32_at(p, 24) != LANES
        || p[60..].iter().any(|&b| b != 0)
    {
        return Err(error("加密备份版本或参数不支持，未派生密钥。"));
    }
    Ok(())
}
fn aad(prefix: &[u8; PREFIX], domain: u8, index: u64) -> [u8; PREFIX + 9] {
    let mut bytes = [0; PREFIX + 9];
    bytes[..PREFIX].copy_from_slice(prefix);
    bytes[PREFIX] = domain;
    bytes[PREFIX + 1..].copy_from_slice(&index.to_le_bytes());
    bytes
}
fn seal(
    key: &[u8; 32],
    prefix: &[u8; PREFIX],
    domain: u8,
    index: u64,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    let mut nonce = [0; 24];
    random(&mut nonce)?;
    let mut bytes = Zeroizing::new(data.to_vec());
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| crypto_error())?;
    let tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            &aad(prefix, domain, index),
            &mut bytes,
        )
        .map_err(|_| crypto_error())?;
    let mut out = Vec::with_capacity(data.len() + 40);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&bytes);
    out.extend_from_slice(&tag);
    Ok(out)
}
fn unseal(
    key: &[u8; 32],
    prefix: &[u8; PREFIX],
    domain: u8,
    index: u64,
    record: &[u8],
) -> io::Result<Zeroizing<Vec<u8>>> {
    if record.len() < 40 {
        return Err(crypto_error());
    }
    let mut bytes = Zeroizing::new(record[24..record.len() - 16].to_vec());
    XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| crypto_error())?
        .decrypt_in_place_detached(
            XNonce::from_slice(&record[..24]),
            &aad(prefix, domain, index),
            &mut bytes,
            Tag::from_slice(&record[record.len() - 16..]),
        )
        .map_err(|_| crypto_error())?;
    Ok(bytes)
}
fn record_at(index: u64) -> u64 {
    HEADER as u64 + index * RECORD as u64
}
fn write_once(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    if out.write(bytes)? != bytes.len() {
        return Err(error("加密输出发生短写，保留未完成文件，不重试。"));
    }
    Ok(())
}

pub struct Writer<W> {
    inner: W,
    prefix: [u8; PREFIX],
    key: Zeroizing<[u8; 32]>,
    cache: Zeroizing<Vec<u8>>,
    block: Option<u64>,
    dirty: bool,
    pos: u64,
    len: u64,
    started: bool,
    sealed: bool,
    failed: bool,
}
impl<W: Output + Read> Writer<W> {
    pub fn new(mut inner: W, password: &Password) -> io::Result<Self> {
        password.validate_creation()?;
        if inner.seek(SeekFrom::End(0))? != 0 {
            return Err(error("加密备份只接受新建的空文件。"));
        }
        let prefix = make_prefix()?;
        let key = derive(password, &prefix)?;
        Ok(Self::with_key(inner, prefix, key))
    }
    fn with_key(inner: W, prefix: [u8; PREFIX], key: Zeroizing<[u8; 32]>) -> Self {
        Self {
            inner,
            prefix,
            key,
            cache: Zeroizing::new(vec![0; BLOCK]),
            block: None,
            dirty: false,
            pos: 0,
            len: 0,
            started: false,
            sealed: false,
            failed: false,
        }
    }
    fn writable(&self) -> io::Result<()> {
        if self.failed || self.sealed || !self.started {
            return Err(error("加密输出已停止、已封存或尚未初始化。"));
        }
        Ok(())
    }
    fn persist_block(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let index = self.block.ok_or_else(|| error("加密块状态缺失。"))?;
        // A new independent 192-bit nonce for EVERY rewrite, including block 0
        // when the archive writer replaces its provisional header.
        let bytes = seal(&self.key, &self.prefix, 1, index, &self.cache)?;
        self.inner.seek(SeekFrom::Start(record_at(index)))?;
        write_once(&mut self.inner, &bytes)?;
        self.dirty = false;
        Ok(())
    }
    fn load(&mut self, index: u64) -> io::Result<()> {
        if self.block == Some(index) {
            return Ok(());
        }
        self.persist_block()?;
        self.cache.zeroize();
        self.block = None;
        if index < self.len.div_ceil(BLOCK as u64) {
            let mut bytes = vec![0; RECORD];
            self.inner.seek(SeekFrom::Start(record_at(index)))?;
            self.inner.read_exact(&mut bytes)?;
            self.cache = unseal(&self.key, &self.prefix, 1, index, &bytes)?;
        } else {
            self.cache.resize(BLOCK, 0);
        }
        self.block = Some(index);
        Ok(())
    }
    /// Returns the ordinary file only after authenticated finalization. Callers
    /// still MUST reopen, decrypt and verify the complete backup chain.
    #[cfg(test)]
    pub fn into_inner(self) -> io::Result<W> {
        if !self.sealed || self.failed {
            return Err(error("加密备份尚未成功封存。"));
        }
        Ok(self.inner)
    }
    /// Verify through a newly initialized decrypting reader without retaining
    /// the password or performing a second expensive KDF. This does not verify
    /// the archive content; callers must still run the complete chain verifier.
    pub fn into_reader(self) -> io::Result<Reader<W>> {
        if !self.sealed || self.failed {
            return Err(error("加密备份尚未成功封存。"));
        }
        let mut inner = self.inner;
        let header = read_header(&mut inner)?;
        Reader::from_header(inner, header, self.key)
    }
}
impl<W: Output + Read> Write for Writer<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writable()?;
        let result = (|| {
            if bytes.is_empty() {
                return Ok(0);
            }
            if self
                .pos
                .checked_add(bytes.len() as u64)
                .is_none_or(|n| n > MAX_SIZE)
            {
                return Err(error("加密备份容量超出范围。"));
            }
            self.load(self.pos / BLOCK as u64)?;
            let start = (self.pos % BLOCK as u64) as usize;
            let n = bytes.len().min(BLOCK - start);
            self.cache[start..start + n].copy_from_slice(&bytes[..n]);
            self.dirty = true;
            self.pos += n as u64;
            self.len = self.len.max(self.pos);
            Ok(n)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writable()?;
        let result = self.persist_block().and_then(|_| self.inner.flush());
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl<W: Output + Read> Seek for Writer<W> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        // Existing archive writers seek to zero before their initial reset.
        if !self.started && matches!(from, SeekFrom::Start(0)) && !self.failed {
            return Ok(0);
        }
        self.writable()?;
        self.pos = position(self.pos, self.len, from)?;
        Ok(self.pos)
    }
}
impl<W: Output + Read> Output for Writer<W> {
    fn reset(&mut self) -> io::Result<()> {
        if self.started || self.failed || self.sealed {
            return Err(error("加密输出不可重新开始。"));
        }
        self.started = true;
        let result = (|| {
            self.inner.reset()?;
            self.inner.seek(SeekFrom::Start(0))?;
            let mut header = vec![0; HEADER];
            header[..PREFIX].copy_from_slice(&self.prefix);
            write_once(&mut self.inner, &header)?;
            self.inner.sync()
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn sync(&mut self) -> io::Result<()> {
        self.writable()?;
        let result = (|| {
            storage_bound(self.len)?;
            self.persist_block()?;
            self.inner.sync()?;
            let mut meta = [0; 32];
            meta[..8].copy_from_slice(&self.len.to_le_bytes());
            meta[8..16].copy_from_slice(&self.len.div_ceil(BLOCK as u64).to_le_bytes());
            meta[16..24].copy_from_slice(&(RECORD as u64).to_le_bytes());
            meta[24..].copy_from_slice(b"COMPLETE");
            let sealed = seal(&self.key, &self.prefix, 0, u64::MAX, &meta)?;
            let mut header = vec![0; HEADER];
            header[..PREFIX].copy_from_slice(&self.prefix);
            header[PREFIX..PREFIX + sealed.len()].copy_from_slice(&sealed);
            if self.inner.seek(SeekFrom::End(0))? != storage_bound(self.len)? {
                return Err(error("加密输出长度不一致。"));
            }
            self.inner.seek(SeekFrom::Start(0))?;
            write_once(&mut self.inner, &header)?;
            self.inner.sync()?;
            self.sealed = true;
            self.cache.zeroize();
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

pub struct Reader<R> {
    inner: R,
    prefix: [u8; PREFIX],
    key: Zeroizing<[u8; 32]>,
    len: u64,
    pos: u64,
    block: Option<u64>,
    cache: Zeroizing<Vec<u8>>,
    failed: bool,
}
impl<R: Read + Seek> Reader<R> {
    pub fn open(mut inner: R, password: &Password) -> io::Result<Self> {
        let header = read_header(&mut inner)?;
        let prefix = header[..PREFIX].try_into().unwrap();
        let key = derive(password, &prefix)?;
        Self::from_header(inner, header, key)
    }
    fn from_header(
        mut inner: R,
        header: [u8; HEADER],
        key: Zeroizing<[u8; 32]>,
    ) -> io::Result<Self> {
        let prefix = header[..PREFIX].try_into().unwrap();
        validate_prefix(&prefix)?;
        let meta = unseal(&key, &prefix, 0, u64::MAX, &header[64..136])?;
        let len = u64_at(&meta, 0);
        if &meta[24..] != b"COMPLETE"
            || u64_at(&meta, 8) != len.div_ceil(BLOCK as u64)
            || u64_at(&meta, 16) != RECORD as u64
            || inner.seek(SeekFrom::End(0))? != storage_bound(len)?
        {
            return Err(error("加密备份长度不匹配或尚未完成。"));
        }
        Ok(Self {
            inner,
            prefix,
            key,
            len,
            pos: 0,
            block: None,
            cache: Zeroizing::new(vec![]),
            failed: false,
        })
    }
    fn load(&mut self, index: u64) -> io::Result<()> {
        if self.block == Some(index) {
            return Ok(());
        }
        self.block = None;
        self.cache.zeroize();
        let mut bytes = vec![0; RECORD];
        self.inner.seek(SeekFrom::Start(record_at(index)))?;
        self.inner.read_exact(&mut bytes)?;
        let data = unseal(&self.key, &self.prefix, 1, index, &bytes)?;
        let used = (self.len - index * BLOCK as u64).min(BLOCK as u64) as usize;
        if data[used..].iter().any(|&b| b != 0) {
            return Err(error("加密备份块末尾填充不正确。"));
        }
        self.cache = data;
        self.block = Some(index);
        Ok(())
    }
}
fn read_header(input: &mut (impl Read + Seek)) -> io::Result<[u8; HEADER]> {
    let len = input.seek(SeekFrom::End(0))?;
    if len < HEADER as u64 + RECORD as u64 || len > storage_bound(MAX_SIZE)? {
        return Err(error("加密备份长度无效。"));
    }
    let mut header = [0; HEADER];
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut header)?;
    validate_prefix(header[..PREFIX].try_into().unwrap())?;
    if header[136..].iter().any(|&b| b != 0) {
        return Err(error("加密备份保留字段不支持。"));
    }
    Ok(header)
}
impl<R: Read + Seek> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(error("加密读取已停止，不自动重试。"));
        }
        if out.is_empty() || self.pos == self.len {
            return Ok(0);
        }
        let result = (|| {
            self.load(self.pos / BLOCK as u64)?;
            let start = (self.pos % BLOCK as u64) as usize;
            let n = out
                .len()
                .min(BLOCK - start)
                .min((self.len - self.pos) as usize);
            out[..n].copy_from_slice(&self.cache[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        })();
        if result.is_err() {
            self.failed = true;
            self.cache.zeroize();
            self.block = None;
        }
        result
    }
}
impl<R: Read + Seek> Seek for Reader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if self.failed {
            return Err(error("加密读取已停止，不自动重试。"));
        }
        self.pos = position(self.pos, self.len, from)?;
        Ok(self.pos)
    }
}

#[cfg(test)]
#[path = "backup_encryption_tests.rs"]
mod tests;

//! Streaming image encoders. Ordinary owned streams only: no paths/devices.
//! VHD layout: Microsoft azure-vhd-utils footer/header definitions.
//! VDI layout: Oracle VirtualBox VDICore.h VDIHEADER1 (version 1.1).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
#[path = "image_convert_containers.rs"]
mod containers;

pub const CHUNK: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum Pass {
    Copy,
    Check,
}

pub(crate) fn feed<W: Read + Write + Seek>(
    reader: &mut impl Read,
    encoder: &mut Encoder<W>,
    size: u64,
    pass: Pass,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<(), String> {
    let mut bytes = vec![0; CHUNK];
    let mut done = 0;
    while done < size {
        tick(done)?;
        let len = (size - done).min(CHUNK as u64) as usize;
        let bytes = &mut bytes[..len];
        reader
            .read_exact(bytes)
            .map_err(|e| format!("读取失败，未跳过坏扇区：{e}"))?;
        tick(done)?;
        match pass {
            Pass::Copy => encoder.push(done, bytes)?,
            Pass::Check => {
                encoder.verify_source(done, bytes)?;
            }
        }
        done += len as u64;
    }
    tick(done)
}

const EMPTY: u32 = u32::MAX;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    Raw,
    VhdFixed,
    VhdDynamic,
    VdiDynamic,
    VdiFixed,
    VhdxDynamic,
    VhdxFixed,
    VmdkSparse,
    Parallels,
}
impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Raw => "img",
            Self::VhdFixed | Self::VhdDynamic => "vhd",
            Self::VdiDynamic | Self::VdiFixed => "vdi",
            Self::VhdxDynamic | Self::VhdxFixed => "vhdx",
            Self::VmdkSparse => "vmdk",
            Self::Parallels => "hds",
        }
    }
    fn sparse(self) -> bool {
        !matches!(self, Self::Raw | Self::VhdFixed)
    }
    fn block_size(self) -> u64 {
        if matches!(self, Self::VhdxDynamic | Self::VhdxFixed) {
            8 * CHUNK as u64
        } else {
            CHUNK as u64
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub format: Format,
    pub size: u64,
    pub sector: u64,
}
impl Options {
    pub fn validate(&self) -> Result<(), String> {
        if !(4 * CHUNK as u64..=1u64 << 43).contains(&self.size)
            || !matches!(self.sector, 512 | 4096)
            || self.size % self.sector != 0
        {
            return Err("镜像容量须为 4 MiB–8 TiB，并按逻辑扇区对齐。".into());
        }
        if !matches!(
            self.format,
            Format::Raw | Format::VhdxDynamic | Format::VhdxFixed
        ) && self.sector != 512
        {
            return Err("该格式输出仅支持 512 B 逻辑扇区；4 KiB 源请选择 RAW 或 VHDX。".into());
        }
        if matches!(self.format, Format::VhdFixed | Format::VhdDynamic)
            && self.size > 2040 * 1024u64.pow(3)
        {
            return Err("VHD v1 输出容量不能超过 2040 GiB。".into());
        }
        if self.format == Format::VmdkSparse && self.size > 2040 * 1024u64.pow(3) {
            return Err("单文件稀疏 VMDK 输出容量不能超过 2040 GiB。".into());
        }
        Ok(())
    }

    fn data_start(&self) -> u64 {
        let blocks = self.size.div_ceil(self.format.block_size());
        match self.format {
            Format::VhdDynamic => align512(1536 + blocks * 4),
            Format::VdiDynamic | Format::VdiFixed => align512(512 + blocks * 4),
            Format::VhdxDynamic | Format::VhdxFixed => containers::vhdx_data_start(self),
            Format::VmdkSparse => containers::vmdk_layout(blocks).data,
            Format::Parallels => (64 + blocks * 4).div_ceil(CHUNK as u64) * CHUNK as u64,
            _ => 0,
        }
    }

    #[cfg(windows)]
    pub(crate) fn minimum_output_bytes(&self) -> Result<u64, String> {
        self.validate()?;
        let payload = match self.format {
            Format::Raw | Format::VhdFixed => self.size,
            Format::VdiFixed | Format::VhdxFixed => self.size.next_multiple_of(self.format.block_size()),
            _ => 0,
        };
        let footer = if matches!(self.format, Format::VhdFixed | Format::VhdDynamic) { 512 } else { 0 };
        Ok(self.data_start() + payload + footer)
    }

    #[cfg(windows)]
    pub(crate) fn check_filename(&self, name: &str) -> Result<(), String> {
        if self.format == Format::VmdkSparse {
            containers::validate_vmdk_name(name)?;
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Identity {
    pub created: [u8; 16],
    pub modified: [u8; 16],
    pub vhd_timestamp: u32,
    // Native-selected basename only, used by VMDK's embedded descriptor.
    // Never interpreted as a path by the encoder.
    pub file_name: String,
}
#[cfg(windows)]
impl Identity {
    pub(crate) fn for_output(file_name: String) -> Result<Self, String> {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(err)?
            .as_secs()
            .checked_sub(946684800)
            .ok_or("系统时间无效")?;
        Ok(Self {
            created: *uuid::Uuid::new_v4().as_bytes(),
            modified: *uuid::Uuid::new_v4().as_bytes(),
            vhd_timestamp: u32::try_from(seconds).map_err(err)?,
            file_name,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verified {
    pub logical_bytes: u64,
    pub file_bytes: u64,
    pub allocated_blocks: u32,
    pub sha256: String,
}

pub struct Encoder<W> {
    io: W,
    options: Options,
    identity: Identity,
    map: Vec<u32>,
    received: u64,
    next: u64,
    data_start: u64,
    allocated: u32,
    hash: Sha256,
    source_check: Sha256,
    source_checked: u64,
    failed: bool,
}
fn align512(n: u64) -> u64 {
    n.div_ceil(512) * 512
}
fn put32(b: &mut [u8], at: usize, n: u32, be: bool) {
    b[at..at + 4].copy_from_slice(&if be { n.to_be_bytes() } else { n.to_le_bytes() });
}
fn put64(b: &mut [u8], at: usize, n: u64, be: bool) {
    b[at..at + 8].copy_from_slice(&if be { n.to_be_bytes() } else { n.to_le_bytes() });
}
fn checksum(b: &mut [u8], at: usize) {
    b[at..at + 4].fill(0);
    let sum = b.iter().fold(0u32, |s, v| s.wrapping_add(u32::from(*v)));
    put32(b, at, !sum, true);
}
// VHD CHS is compatibility geometry; exact capacity remains in the size fields.
fn geometry(size: u64) -> [u8; 4] {
    let total = (size / 512).min(65535 * 16 * 255);
    let (heads, sectors, cylinder_heads) = if total >= 65535 * 16 * 63 {
        (16, 255, total / 255)
    } else {
        let mut s = 17;
        let mut ch = total / s;
        let mut h = ch.div_ceil(1024).max(4);
        if ch >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            ch = total / s;
        }
        if ch >= h * 1024 {
            s = 63;
            h = 16;
            ch = total / s;
        }
        (h, s, ch)
    };
    let c = (cylinder_heads / heads) as u16;
    [
        c.to_be_bytes()[0],
        c.to_be_bytes()[1],
        heads as u8,
        sectors as u8,
    ]
}
impl<W: Read + Write + Seek> Encoder<W> {
    pub fn new(mut io: W, options: Options, identity: Identity) -> Result<Self, String> {
        options.validate()?;
        if identity.created == [0; 16] || identity.modified == [0; 16] {
            return Err("镜像标识不能为空。".into());
        }
        if io.seek(SeekFrom::End(0)).map_err(err)? != 0 {
            return Err("转换仅接受新建的空输出文件。".into());
        }
        if options.format == Format::VmdkSparse {
            containers::validate_vmdk_name(&identity.file_name)?;
        }
        let blocks = options.size.div_ceil(options.format.block_size());
        let map = if options.format.sparse() {
            vec![EMPTY; blocks as usize]
        } else {
            vec![]
        };
        let data_start = options.data_start();
        // Reserve bounded metadata, but do not emit a valid container signature
        // until every logical source byte has arrived. Partial files stay partial.
        let zero = vec![0; CHUNK];
        io.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut n = data_start;
        while n > 0 {
            let k = n.min(CHUNK as u64) as usize;
            io.write_all(&zero[..k]).map_err(err)?;
            n -= k as u64;
        }
        Ok(Self {
            io,
            options,
            identity,
            map,
            received: 0,
            next: data_start,
            data_start,
            allocated: 0,
            hash: Sha256::new(),
            source_check: Sha256::new(),
            source_checked: 0,
            failed: false,
        })
    }
    pub fn received(&self) -> u64 {
        self.received
    }
    pub fn verify_source(&mut self, offset: u64, bytes: &[u8]) -> Result<u64, String> {
        if self.failed
            || self.received != self.options.size
            || offset != self.source_checked
            || bytes.is_empty()
            || bytes.len() as u64 != (self.options.size - self.source_checked).min(CHUNK as u64)
        {
            self.failed = true;
            return Err("源镜像复核次序或长度不符，不能发布输出。".into());
        }
        self.source_check.update(bytes);
        self.source_checked += bytes.len() as u64;
        Ok(self.source_checked)
    }
    pub fn push(&mut self, offset: u64, bytes: &[u8]) -> Result<(), String> {
        if self.failed {
            return Err("转换已停止，不能重试写入。".into());
        }
        let expected = (self.options.size - self.received).min(CHUNK as u64) as usize;
        if offset != self.received || expected == 0 || bytes.len() != expected {
            self.failed = true;
            return Err("转换块顺序或长度不符，任务已停止。".into());
        }
        let result = (|| {
            if matches!(self.options.format, Format::VhdxDynamic | Format::VhdxFixed) {
                self.push_vhdx(bytes)?;
            } else if self.options.format.sparse() {
                if self.options.format == Format::VdiFixed || bytes.iter().any(|b| *b != 0) {
                    let block = (self.received / CHUNK as u64) as usize;
                    self.io.seek(SeekFrom::Start(self.next)).map_err(err)?;
                    if self.options.format == Format::VhdDynamic {
                        self.map[block] = u32::try_from(self.next / 512).map_err(err)?;
                        // MSB-first sector bitmap, including the zero-padded tail.
                        let mut bitmap = [0u8; 512];
                        bitmap[..CHUNK / 512 / 8].fill(0xff);
                        self.io.write_all(&bitmap).map_err(err)?;
                        self.next += 512;
                    } else {
                        self.map[block] = self.allocated;
                    }
                    self.io.write_all(bytes).map_err(err)?;
                    if bytes.len() < CHUNK {
                        self.io
                            .write_all(&vec![0; CHUNK - bytes.len()])
                            .map_err(err)?;
                    }
                    self.next += CHUNK as u64;
                    self.allocated += 1;
                }
            } else {
                self.io.seek(SeekFrom::Start(self.received)).map_err(err)?;
                self.io.write_all(bytes).map_err(err)?;
                self.next += bytes.len() as u64;
            }
            self.hash.update(bytes);
            self.received += bytes.len() as u64;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn footer(&self) -> Vec<u8> {
        let mut b = vec![0; 512];
        b[..8].copy_from_slice(b"conectix");
        put32(&mut b, 8, 2, true);
        put32(&mut b, 12, 0x10000, true);
        put64(
            &mut b,
            16,
            if self.options.format == Format::VhdFixed {
                u64::MAX
            } else {
                512
            },
            true,
        );
        put32(&mut b, 24, self.identity.vhd_timestamp, true);
        b[28..32].copy_from_slice(b"DStu");
        put32(&mut b, 32, 0x10000, true);
        b[36..40].copy_from_slice(b"Wi2k");
        put64(&mut b, 40, self.options.size, true);
        put64(&mut b, 48, self.options.size, true);
        b[56..60].copy_from_slice(&geometry(self.options.size));
        put32(
            &mut b,
            60,
            if self.options.format == Format::VhdFixed {
                2
            } else {
                3
            },
            true,
        );
        b[68..84].copy_from_slice(&self.identity.created);
        checksum(&mut b, 64);
        b
    }
    fn metadata(&self) -> Vec<(u64, Vec<u8>)> {
        match self.options.format {
            Format::Raw => vec![],
            Format::VhdFixed => vec![(self.options.size, self.footer())],
            Format::VhdDynamic => {
                let mut h = vec![0; 1024];
                h[..8].copy_from_slice(b"cxsparse");
                put64(&mut h, 8, u64::MAX, true);
                put64(&mut h, 16, 1536, true);
                put32(&mut h, 24, 0x10000, true);
                put32(&mut h, 28, self.map.len() as u32, true);
                put32(&mut h, 32, CHUNK as u32, true);
                checksum(&mut h, 36);
                let mut map = vec![0xff; (self.data_start - 1536) as usize];
                for (i, n) in self.map.iter().enumerate() {
                    put32(&mut map, i * 4, *n, true);
                }
                vec![
                    (0, self.footer()),
                    (512, h),
                    (1536, map),
                    (self.next, self.footer()),
                ]
            }
            Format::VdiDynamic | Format::VdiFixed => {
                let mut h = vec![0; 512];
                let label = b"<<< DiskStudio Virtual Disk Image >>>\n";
                h[..label.len()].copy_from_slice(label);
                put32(&mut h, 64, 0xbeda107f, false);
                put32(&mut h, 68, 0x10001, false);
                put32(&mut h, 72, 384, false);
                put32(
                    &mut h,
                    76,
                    if self.options.format == Format::VdiFixed {
                        2
                    } else {
                        1
                    },
                    false,
                );
                put32(&mut h, 340, 512, false);
                put32(&mut h, 344, self.data_start as u32, false);
                put32(&mut h, 360, 512, false);
                put64(&mut h, 368, self.options.size, false);
                put32(&mut h, 376, CHUNK as u32, false);
                put32(&mut h, 384, self.map.len() as u32, false);
                put32(&mut h, 388, self.allocated, false);
                h[392..408].copy_from_slice(&self.identity.created);
                h[408..424].copy_from_slice(&self.identity.modified);
                let mut map = vec![0; (self.data_start - 512) as usize];
                for (i, n) in self.map.iter().enumerate() {
                    put32(
                        &mut map,
                        i * 4,
                        if *n == EMPTY { 0xfffffffe } else { *n },
                        false,
                    );
                }
                vec![(0, h), (512, map)]
            }
            Format::VhdxDynamic | Format::VhdxFixed => self.vhdx_metadata(),
            Format::VmdkSparse => self.vmdk_metadata(),
            Format::Parallels => self.parallels_metadata(),
        }
    }
    /// Consumes the encoder, preventing retries after a partial finalize. The
    /// caller flushes the retained handle, then this verifies metadata and every
    /// logical byte before the caller may publish the newly created file.
    pub fn finish(
        mut self,
        mut sync: impl FnMut(&mut W) -> Result<(), String>,
        mut tick: impl FnMut(u64) -> Result<(), String>,
    ) -> Result<(W, Verified), String> {
        if self.failed
            || self.received != self.options.size
            || self.source_checked != self.options.size
        {
            return Err("转换尚未完整接收源数据，不能发布输出。".into());
        }
        if self.source_check.clone().finalize() != self.hash.clone().finalize() {
            return Err("两遍源镜像的完整逻辑内容 SHA-256 不一致，未发布输出。".into());
        }
        let meta = self.metadata();
        for (at, b) in &meta {
            tick(0)?;
            self.io.seek(SeekFrom::Start(*at)).map_err(err)?;
            self.io.write_all(b).map_err(err)?;
        }
        let file_bytes = self.next
            + if matches!(self.options.format, Format::VhdFixed | Format::VhdDynamic) {
                512
            } else {
                0
            };
        sync(&mut self.io)?;
        if self.io.seek(SeekFrom::End(0)).map_err(err)? != file_bytes {
            return Err("输出文件长度不符。".into());
        }
        for (at, b) in &meta {
            tick(0)?;
            let mut actual = vec![0; b.len()];
            self.io.seek(SeekFrom::Start(*at)).map_err(err)?;
            self.io.read_exact(&mut actual).map_err(err)?;
            if actual != *b {
                return Err("镜像头部或块映射回读不一致。".into());
            }
        }
        let mut actual = Sha256::new();
        let mut b = vec![0; CHUNK];
        let mut done = 0;
        while done < self.options.size {
            tick(done)?;
            let len = (self.options.size - done).min(CHUNK as u64) as usize;
            let slot = if self.options.format.sparse() {
                self.map[(done / self.options.format.block_size()) as usize]
            } else {
                0
            };
            if self.options.format.sparse() && slot == EMPTY {
                b[..len].fill(0);
            } else {
                let at = match self.options.format {
                    Format::VhdDynamic => u64::from(slot) * 512 + 512,
                    Format::VdiDynamic | Format::VdiFixed => {
                        self.data_start + u64::from(slot) * CHUNK as u64
                    }
                    Format::VhdxDynamic
                    | Format::VhdxFixed
                    | Format::VmdkSparse
                    | Format::Parallels => {
                        self.data_start
                            + u64::from(slot) * self.options.format.block_size()
                            + done % self.options.format.block_size()
                    }
                    _ => done,
                };
                if self.options.format == Format::VhdDynamic {
                    let mut bitmap = [0; 512];
                    self.io.seek(SeekFrom::Start(at - 512)).map_err(err)?;
                    self.io.read_exact(&mut bitmap).map_err(err)?;
                    if bitmap[..CHUNK / 512 / 8].iter().any(|v| *v != 0xff)
                        || bitmap[CHUNK / 512 / 8..].iter().any(|v| *v != 0)
                    {
                        return Err("VHD 扇区位图回读不一致。".into());
                    }
                }
                self.io.seek(SeekFrom::Start(at)).map_err(err)?;
                self.io.read_exact(&mut b[..len]).map_err(err)?;
            }
            actual.update(&b[..len]);
            done += len as u64;
            if self.options.format.sparse() && slot != EMPTY && done == self.options.size {
                let block = self.options.format.block_size();
                let mut padding = (block - done % block) % block;
                while padding > 0 {
                    tick(done)?;
                    let n = padding.min(CHUNK as u64) as usize;
                    self.io.read_exact(&mut b[..n]).map_err(err)?;
                    if b[..n].iter().any(|v| *v != 0) {
                        return Err("镜像尾块填充不符。".into());
                    }
                    padding -= n as u64;
                }
            }
        }
        tick(done)?;
        let expected = self.hash.finalize();
        if actual.finalize() != expected {
            return Err("转换输出完整逻辑内容 SHA-256 回读失败，保留 partial 文件。".into());
        }
        Ok((
            self.io,
            Verified {
                logical_bytes: done,
                file_bytes,
                allocated_blocks: self.allocated,
                sha256: format!("{expected:x}"),
            },
        ))
    }
}

#[cfg(test)]
#[path = "image_convert_tests.rs"]
mod tests;

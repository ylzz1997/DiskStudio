//! Generates a new logical disk from typed geometry. No paths, devices, mounts
//! or input files. UEFI GPT 1.0: 128 empty 128-byte entries, dual CRC headers.
use crate::image_convert::{Encoder, Options as OutputOptions, CHUNK};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, Write};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Table {
    None,
    Gpt,
    Mbr,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub output: OutputOptions,
    pub table: Table,
}
impl Options {
    pub fn validate(&self) -> Result<(), String> {
        self.output.validate()?;
        if self.table == Table::Mbr && (self.output.sector != 512 || self.output.size > 1u64 << 41)
        {
            return Err("MBR 镜像创建限 512 B 扇区、最大 2 TiB；其他容量请选择 GPT。".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub table: Table,
    pub disk_id: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Generating,
    Checking,
}
pub struct Generator {
    options: Options,
    summary: Summary,
    patches: Vec<(u64, Vec<u8>)>,
}
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, n: u64) {
    b[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
impl Generator {
    pub fn new(options: Options, id: uuid::Uuid) -> Result<Self, String> {
        options.validate()?;
        if id.is_nil() {
            return Err("新镜像磁盘标识不能为空。".into());
        }
        let mut patches = vec![];
        let mut disk_id = None;
        let sector = options.output.sector;
        let last = options.output.size / sector - 1;
        if options.table != Table::None {
            let mut mbr = vec![0; sector as usize];
            mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
            if options.table == Table::Mbr {
                // A separate random Windows MBR signature; no boot code or partitions.
                let signature = u32::from_le_bytes(id.as_bytes()[..4].try_into().unwrap()).max(1);
                put32(&mut mbr, 440, signature);
                disk_id = Some(format!("{signature:08X}"));
            } else {
                mbr[447..450].copy_from_slice(&[0, 2, 0]);
                mbr[450] = 0xee;
                mbr[451..454].fill(0xff);
                put32(&mut mbr, 454, 1);
                put32(&mut mbr, 458, last.min(u32::MAX as u64) as u32);
                disk_id = Some(id.to_string());
                let entries = 128u64 * 128;
                let array_sectors = entries.div_ceil(sector);
                let array_crc = crc32fast::hash(&vec![0; entries as usize]);
                for (at, alternate, array_at) in [(1, last, 2), (last, 1, last - array_sectors)] {
                    let mut h = vec![0; sector as usize];
                    h[..8].copy_from_slice(b"EFI PART");
                    put32(&mut h, 8, 0x10000);
                    put32(&mut h, 12, 92);
                    put64(&mut h, 24, at);
                    put64(&mut h, 32, alternate);
                    put64(&mut h, 40, 2 + array_sectors);
                    put64(&mut h, 48, last - array_sectors - 1);
                    h[56..72].copy_from_slice(&id.to_bytes_le());
                    put64(&mut h, 72, array_at);
                    put32(&mut h, 80, 128);
                    put32(&mut h, 84, 128);
                    put32(&mut h, 88, array_crc);
                    let crc = crc32fast::hash(&h[..92]);
                    put32(&mut h, 16, crc);
                    patches.push((at * sector, h));
                }
            }
            patches.push((0, mbr));
        }
        Ok(Self {
            summary: Summary {
                table: options.table,
                disk_id,
            },
            options,
            patches,
        })
    }
    pub fn summary(&self) -> Summary {
        self.summary.clone()
    }
    pub fn fill(&self, at: u64, bytes: &mut [u8]) -> Result<(), String> {
        let len = bytes.len() as u64;
        if len == 0
            || len > CHUNK as u64
            || len > self.options.output.size
            || at > self.options.output.size - len
        {
            return Err("新镜像生成范围无效。".into());
        }
        bytes.fill(0);
        for (offset, patch) in &self.patches {
            let start = at.max(*offset);
            let end = (at + len).min(*offset + patch.len() as u64);
            if start < end {
                bytes[(start - at) as usize..(end - at) as usize]
                    .copy_from_slice(&patch[(start - offset) as usize..(end - offset) as usize]);
            }
        }
        Ok(())
    }
    /// Bounded deterministic generation plus a second independent generation
    /// pass. The existing encoder then verifies all output metadata and content.
    pub fn feed<W: Read + Write + Seek>(
        &self,
        encoder: &mut Encoder<W>,
        mut tick: impl FnMut(Stage, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut bytes = vec![0; CHUNK];
        for stage in [Stage::Generating, Stage::Checking] {
            let mut at = 0;
            tick(stage, at)?;
            while at < self.options.output.size {
                let len = (self.options.output.size - at).min(CHUNK as u64) as usize;
                self.fill(at, &mut bytes[..len])?;
                match stage {
                    Stage::Generating => encoder.push(at, &bytes[..len])?,
                    Stage::Checking => {
                        encoder.verify_source(at, &bytes[..len])?;
                    }
                }
                at += len as u64;
                tick(stage, at)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "image_creation_tests.rs"]
mod tests;

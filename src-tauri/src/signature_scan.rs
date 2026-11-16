//! Bounded, read-only carving. Structure checks delimit candidates, not decoded
//! content integrity. All offsets are relative to the already selected source.
use crate::recovery::{Candidate, Locator, MAX_RESULTS};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};

pub const MAX_FILE: u64 = 256 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
const MAX_PROBES: usize = 1_000_000;
const MAX_FIELDS: usize = 65_536;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Png,
    Jpeg,
    Pdf,
    Gif,
    Bmp,
    Wav,
    Webp,
    Zip,
    Docx,
    Xlsx,
    Pptx,
}
const ALL: [Kind; 11] = [
    Kind::Png,
    Kind::Jpeg,
    Kind::Pdf,
    Kind::Gif,
    Kind::Bmp,
    Kind::Wav,
    Kind::Webp,
    Kind::Zip,
    Kind::Docx,
    Kind::Xlsx,
    Kind::Pptx,
];
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub kinds: Vec<Kind>,
    pub start: u64,
    pub length: Option<u64>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            kinds: ALL.to_vec(),
            start: 0,
            length: None,
        }
    }
}
impl Options {
    pub fn range(&self, source_size: u64) -> Result<std::ops::Range<u64>, String> {
        if self.kinds.is_empty()
            || self.kinds.len() > ALL.len()
            || self
                .kinds
                .iter()
                .enumerate()
                .any(|(i, k)| self.kinds[..i].contains(k))
        {
            return Err("请选择至少一种文件类型，类型不能重复。".into());
        }
        let end = match self.length {
            Some(n) => self.start.checked_add(n).ok_or("扫描范围溢出。")?,
            None => source_size,
        };
        if self.start >= end || end > source_size {
            return Err("扫描范围须位于所选磁盘或分区内，长度必须大于零。".into());
        }
        Ok(self.start..end)
    }
}
impl Kind {
    fn starts(self, b: &[u8]) -> bool {
        match self {
            Self::Png => b.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => b.starts_with(b"\xff\xd8\xff"),
            Self::Pdf => b.starts_with(b"%PDF-"),
            Self::Gif => b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a"),
            Self::Bmp => b.starts_with(b"BM"),
            Self::Wav => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE"),
            Self::Webp => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"),
            Self::Zip | Self::Docx | Self::Xlsx | Self::Pptx => false, // EOCD-anchored, below.
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Pdf => "pdf",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Wav => "wav",
            Self::Webp => "webp",
            Self::Zip => "zip",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
        }
    }
    fn footer(self) -> Option<&'static [u8]> {
        match self {
            Self::Png => Some(b"\0\0\0\0IEND\xaeB`\x82"),
            Self::Jpeg => Some(b"\xff\xd9"),
            Self::Pdf => Some(b"%%EOF"),
            _ => None,
        }
    }
    fn header_len(self) -> usize {
        match self {
            Self::Png => 8,
            Self::Jpeg => 3,
            Self::Pdf => 5,
            Self::Gif => 13,
            Self::Bmp => 54,
            Self::Wav | Self::Webp => 12,
            Self::Zip | Self::Docx | Self::Xlsx | Self::Pptx => 0,
        }
    }
}
fn candidate(
    id: u32,
    kind: Kind,
    start: u64,
    size: u64,
    header: Vec<u8>,
    footer: Vec<u8>,
) -> Candidate {
    Candidate {
        metadata: crate::file_metadata::Metadata::default(),
        id,
        name: format!("recovered_{start:016x}.{}", kind.extension()),
        size,
        deleted: false,
        method: "文件签名".into(),
        note: "连续数据候选；未验证解码完整性，原文件名和删除状态未知".into(),
        supported: true,
        directory: None,
        directory_note: "签名扫描无法确定原目录".into(),
        locator: Locator::Signature {
            content_hash: None,
            offset: start,
            header,
            footer,
        },
    }
}

/// Portable signature evidence, never an arbitrary extraction path or a
/// deserialized internal Candidate. Display names and policy are reconstructed.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredCandidate {
    kind: Kind,
    offset: u64,
    size: u64,
    header: Vec<u8>,
    footer: Vec<u8>,
    content_hash: Option<[u8; 32]>,
}
impl StoredCandidate {
    pub fn from_candidate(c: &Candidate) -> Result<Self, String> {
        let Locator::Signature {
            offset,
            header,
            footer,
            content_hash,
        } = &c.locator
        else {
            return Err("不是签名扫描结果。".into());
        };
        let kind = ALL
            .iter()
            .copied()
            .find(|k| c.name == format!("recovered_{offset:016x}.{}", k.extension()))
            .ok_or("签名结果名称无效")?;
        Ok(Self {
            kind,
            offset: *offset,
            size: c.size,
            header: header.clone(),
            footer: footer.clone(),
            content_hash: *content_hash,
        })
    }
    pub fn candidate(
        &self,
        id: u32,
        range: std::ops::Range<u64>,
        options: &Options,
    ) -> Result<Candidate, String> {
        let archive = matches!(self.kind, Kind::Zip | Kind::Docx | Kind::Xlsx | Kind::Pptx);
        if !options.kinds.contains(&self.kind)
            || self.size == 0
            || self.size > MAX_FILE
            || self.offset < range.start
            || self
                .offset
                .checked_add(self.size)
                .is_none_or(|end| end > range.end)
            || self.header.len() as u64 > self.size
            || self.footer.len() as u64 > self.size
            || self.header.len() > 54
            || self.footer.len() > 16
            || archive != self.content_hash.is_some()
        {
            return Err("保存的签名候选越界或类型不一致。".into());
        }
        if archive {
            if !(22..=30).contains(&self.header.len())
                || self.footer.len() != 16
                || !(self.header.starts_with(b"PK\x03\x04")
                    || self.header.starts_with(b"PK\x05\x06")
                    || self.header.starts_with(b"PK\x06\x06"))
            {
                return Err("ZIP 进度证据无效。".into());
            }
        } else if self.header.len() != self.kind.header_len()
            || !self.kind.starts(&self.header)
            || self
                .kind
                .footer()
                .map_or(self.footer.len() != 16, |expected| self.footer != expected)
        {
            return Err("签名进度的头尾证据无效。".into());
        }
        let mut c = candidate(
            id,
            self.kind,
            self.offset,
            self.size,
            self.header.clone(),
            self.footer.clone(),
        );
        if let Locator::Signature { content_hash, .. } = &mut c.locator {
            *content_hash = self.content_hash;
        }
        if archive {
            c.note="连续 ZIP 容器；保存时已检查目录与 CRC，提取前复核内容摘要。加载的结果是历史观察，不保证源内容未改变。".into();
        }
        Ok(c)
    }
}

pub fn scan<R: Read + Seek>(
    reader: &mut R,
    source_size: u64,
    options: &Options,
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    continue_scan(
        reader,
        source_size,
        options,
        &mut Checkpoint::default(),
        tick,
        found,
    )
}

/// Cursor is committed only after a complete scan chunk. Interrupted probes
/// discard that chunk's unpublished candidates, so restart cannot duplicate or
/// skip results. Carry bytes and an unterminated signature survive a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub consumed: u64,
    searched: u64,
    carry: Vec<u8>,
    active: Option<(Kind, u64, Vec<u8>)>,
    pub count: u32,
    rejected: usize,
    probes: usize,
    archive_remaining: u64,
    pub sealed: bool,
}
impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            consumed: 0,
            searched: 0,
            carry: vec![],
            active: None,
            count: 0,
            rejected: 0,
            probes: 0,
            archive_remaining: 64 * 1024 * 1024 * 1024,
            sealed: false,
        }
    }
}
impl Checkpoint {
    pub fn validate(&self, size: u64, options: &Options) -> Result<(), String> {
        if self.consumed > size
            || self.searched > size
            || self.carry.len() > 16
            || self.carry.len() as u64 > self.consumed
            || self.count as usize > MAX_RESULTS
            || self.probes > MAX_PROBES + 1
            || self.rejected as u64 > (MAX_PROBES as u64).saturating_add(size)
            || self.archive_remaining > 64 * 1024 * 1024 * 1024
        {
            return Err("签名扫描进度越界。".into());
        }
        if !self.sealed
            && (self.count as usize >= MAX_RESULTS
                || self.consumed == size
                || self.consumed % CHUNK as u64 != 0
                || self.carry.len() as u64 != self.consumed.min(16)
                || self.searched < self.consumed.saturating_sub(16))
        {
            return Err("签名扫描续扫位置无效。".into());
        }
        if let Some((kind, start, header)) = &self.active {
            if !options.kinds.contains(kind)
                || kind.footer().is_none()
                || *start > self.searched
                || *start > self.consumed
                || start
                    .checked_add(header.len() as u64)
                    .is_none_or(|end| end > size)
                || self.consumed - *start > MAX_FILE + CHUNK as u64
                || header.len() != kind.header_len()
                || !kind.starts(header)
            {
                return Err("未闭合签名的进度无效。".into());
            }
        }
        Ok(())
    }
    pub fn verify_cursor(
        &self,
        reader: &mut (impl Read + Seek),
        options: &Options,
    ) -> Result<(), String> {
        let mut verify = |at: u64, expected: &[u8]| -> Result<(), String> {
            reader
                .seek(SeekFrom::Start(at))
                .map_err(|e| e.to_string())?;
            let mut actual = vec![0; expected.len()];
            reader.read_exact(&mut actual).map_err(|e| e.to_string())?;
            if actual != expected {
                return Err("续扫边界的源内容已变化，请重新扫描。".into());
            }
            Ok(())
        };
        if !self.carry.is_empty() {
            verify(
                options.start + self.consumed - self.carry.len() as u64,
                &self.carry,
            )?;
        }
        if let Some((_, start, header)) = &self.active {
            verify(options.start + start, header)?;
        }
        Ok(())
    }
}

pub fn continue_scan<R: Read + Seek>(
    reader: &mut R,
    source_size: u64,
    options: &Options,
    checkpoint: &mut Checkpoint,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
) -> Result<String, String> {
    let range = options.range(source_size)?;
    let size = range.end - range.start;
    checkpoint.validate(size, options)?;
    if checkpoint.sealed {
        return Err("扫描已结束，不能继续。".into());
    }
    let mut buffer = vec![0; CHUNK + 16];
    let mut possible_start = [false; 256];
    for kind in &options.kinds {
        possible_start[match kind {
            Kind::Png => 0x89,
            Kind::Jpeg => 0xff,
            Kind::Pdf => b'%',
            Kind::Gif => b'G',
            Kind::Bmp => b'B',
            Kind::Wav | Kind::Webp => b'R',
            Kind::Zip | Kind::Docx | Kind::Xlsx | Kind::Pptx => b'P',
        } as usize] = true;
    }
    let (mut carry, mut consumed, mut searched) = (
        checkpoint.carry.len(),
        checkpoint.consumed,
        checkpoint.searched,
    );
    buffer[..carry].copy_from_slice(&checkpoint.carry);
    let mut active = checkpoint.active.clone();
    let (mut count, mut rejected, mut probes) =
        (checkpoint.count, checkpoint.rejected, checkpoint.probes);
    let archives = options
        .kinds
        .iter()
        .any(|k| matches!(k, Kind::Zip | Kind::Docx | Kind::Xlsx | Kind::Pptx));
    let mut archive_budget = crate::signature_zip::Budget::resume(checkpoint.archive_remaining)
        .map_err(|_| "ZIP 扫描预算无效")?;
    let result = (|| {
        while consumed < size {
            tick(consumed, size)?;
            let mut unpublished = Vec::new();
            macro_rules! stop {
                ($message:expr,$at:expr) => {{
                    *checkpoint = Checkpoint {
                        consumed: ($at).min(size),
                        searched: ($at).min(size),
                        carry: vec![],
                        active: None,
                        count,
                        rejected,
                        probes,
                        archive_remaining: archive_budget.remaining(),
                        sealed: true,
                    };
                    for candidate in unpublished.drain(..) {
                        found(candidate);
                    }
                    return Ok($message);
                }};
            }
            let n = (size - consumed).min(CHUNK as u64) as usize;
            // Probes may seek forward. Always restore this sequential scan position.
            reader
                .seek(SeekFrom::Start(range.start + consumed))
                .map_err(|e| e.to_string())?;
            reader
                .read_exact(&mut buffer[carry..carry + n])
                .map_err(|e| e.to_string())?;
            let base = consumed - carry as u64;
            let end = carry + n;
            let mut i = searched.saturating_sub(base) as usize;
            while i + 12 <= end || (consumed + n as u64 == size && i < end) {
                let at = base + i as u64;
                // EOCD anchors may occur inside a different unclosed signature.
                // Check them independently; do not let a stray PDF/JPEG header hide
                // a later, fully bounded archive.
                if archives && buffer[i..end].starts_with(b"PK\x05\x06") {
                    probes += 1;
                    if probes > MAX_PROBES {
                        stop!("达到 100 万次结构候选检查上限，扫描提前结束。".into(), at);
                    }
                    if let Some(a) = crate::signature_zip::probe(
                        reader,
                        range.start + at,
                        range.clone(),
                        &mut archive_budget,
                        || tick(at.max(consumed), size),
                    )? {
                        if options.kinds.contains(&a.kind) {
                            let mut c =
                                candidate(count, a.kind, a.start, a.size, a.header, a.footer);
                            c.note = "连续 ZIP 容器；目录、压缩流与逐项 CRC 已检查，提取前复核内容摘要。Office 仅识别包类型，未验证文档语义；原名和删除状态未知".into();
                            if let Locator::Signature { content_hash, .. } = &mut c.locator {
                                *content_hash = Some(a.digest);
                            }
                            unpublished.push(c);
                            count += 1;
                            if count as usize >= MAX_RESULTS {
                                stop!("达到结果上限，扫描提前结束。".into(), at);
                            }
                        }
                    } else {
                        rejected += 1;
                    }
                }
                if let Some((kind, start, header)) = &active {
                    let footer = kind.footer().expect("only terminated kinds remain active");
                    if at - start >= MAX_FILE {
                        active = None;
                        rejected += 1;
                    } else if buffer[i..end].starts_with(footer) {
                        let length = at + footer.len() as u64 - start;
                        if length <= MAX_FILE {
                            unpublished.push(candidate(
                                count,
                                *kind,
                                range.start + start,
                                length,
                                header.clone(),
                                footer.to_vec(),
                            ));
                            count += 1;
                        } else {
                            rejected += 1;
                        }
                        i += footer.len();
                        active = None;
                        if count as usize >= MAX_RESULTS {
                            stop!("达到结果上限，扫描提前结束。".into(), at);
                        }
                        continue;
                    }
                }
                if active.is_none() {
                    if !possible_start[buffer[i] as usize] {
                        i += 1;
                        continue;
                    }
                    if let Some(kind) = options
                        .kinds
                        .iter()
                        .copied()
                        .find(|k| k.starts(&buffer[i..end]))
                    {
                        if kind.footer().is_some() {
                            active = Some((kind, at, buffer[i..i + kind.header_len()].to_vec()));
                        } else {
                            probes += 1;
                            if probes > MAX_PROBES {
                                stop!(
                                "达到 100 万次结构候选检查上限，扫描提前结束；已找到的结果保留。"
                                    .into(),at
                            );
                            }
                            let absolute = range.start + at;
                            let result = probe(reader, kind, absolute, range.end, || {
                                tick(at.max(consumed), size)
                            })?;
                            if let Some((length, header, footer)) = result {
                                unpublished
                                    .push(candidate(count, kind, absolute, length, header, footer));
                                count += 1;
                                if count as usize >= MAX_RESULTS {
                                    stop!("达到结果上限，扫描提前结束。".into(), at);
                                }
                                i = (at + length - base) as usize;
                                continue;
                            }
                            rejected += 1;
                        }
                    }
                }
                i += 1;
            }
            searched = base + i as u64;
            consumed += n as u64;
            carry = end.min(16);
            buffer.copy_within(end - carry..end, 0);
            *checkpoint = Checkpoint {
                consumed,
                searched,
                carry: buffer[..carry].to_vec(),
                active: active.clone(),
                count,
                rejected,
                probes,
                archive_remaining: archive_budget.remaining(),
                sealed: consumed == size,
            };
            for candidate in unpublished {
                found(candidate);
            }
        }
        if active.is_some() {
            rejected += 1;
        }
        tick(size, size)?;
        Ok(format!("签名扫描完成，找到 {count} 个候选；忽略 {rejected} 个不完整、超限或不支持结构的候选。单文件上限 256 MiB，结构最多 65536 个字段；不重组碎片，不保证可解码。"))
    })();
    // Interrupted probes still consume their resource budget, even when their
    // current chunk will be retried after an explicit resume.
    checkpoint.probes = probes;
    checkpoint.archive_remaining = archive_budget.remaining();
    result
}

enum ProbeError {
    Invalid,
    Failed(String),
}
type PResult<T> = Result<T, ProbeError>;
struct Probe<'a, R, T> {
    reader: &'a mut R,
    start: u64,
    limit: u64,
    fields: usize,
    tick: T,
}
impl<R: Read + Seek, T: FnMut() -> Result<(), String>> Probe<'_, R, T> {
    fn bytes<const N: usize>(&mut self, offset: u64) -> PResult<[u8; N]> {
        self.fields += 1;
        if self.fields > MAX_FIELDS
            || offset
                .checked_add(N as u64)
                .is_none_or(|end| end > self.limit)
        {
            return Err(ProbeError::Invalid);
        }
        if self.fields % 256 == 1 {
            (self.tick)().map_err(ProbeError::Failed)?;
        }
        let mut bytes = [0; N];
        self.reader
            .seek(SeekFrom::Start(self.start + offset))
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        Ok(bytes)
    }
    fn bounded(&self, end: u64) -> PResult<u64> {
        if end > self.limit {
            Err(ProbeError::Invalid)
        } else {
            Ok(end)
        }
    }
    fn sub_blocks(&mut self, mut at: u64) -> PResult<u64> {
        loop {
            let n = self.bytes::<1>(at)?[0] as u64;
            at = self.bounded(at + 1 + n)?;
            if n == 0 {
                return Ok(at);
            }
        }
    }
    fn gif(&mut self) -> PResult<u64> {
        let h = self.bytes::<13>(0)?;
        let width = le16(&h[6..8]);
        let height = le16(&h[8..10]);
        if width == 0 || height == 0 {
            return Err(ProbeError::Invalid);
        }
        let mut at = 13
            + if h[10] & 128 != 0 {
                3u64 << ((h[10] & 7) + 1)
            } else {
                0
            };
        let mut image = false;
        loop {
            match self.bytes::<1>(at)?[0] {
                0x3b => {
                    return if image {
                        Ok(at + 1)
                    } else {
                        Err(ProbeError::Invalid)
                    }
                }
                0x2c => {
                    let d = self.bytes::<10>(at)?;
                    let w = le16(&d[5..7]);
                    let h = le16(&d[7..9]);
                    if w == 0
                        || h == 0
                        || le16(&d[1..3]) as u32 + w as u32 > width as u32
                        || le16(&d[3..5]) as u32 + h as u32 > height as u32
                        || d[9] & 0x18 != 0
                    {
                        return Err(ProbeError::Invalid);
                    }
                    at += 10
                        + if d[9] & 128 != 0 {
                            3u64 << ((d[9] & 7) + 1)
                        } else {
                            0
                        };
                    if !(2..=8).contains(&self.bytes::<1>(at)?[0]) {
                        return Err(ProbeError::Invalid);
                    }
                    if self.bytes::<1>(at + 1)?[0] == 0 {
                        return Err(ProbeError::Invalid);
                    }
                    at = self.sub_blocks(at + 1)?;
                    image = true;
                }
                0x21 => {
                    let label = self.bytes::<1>(at + 1)?[0];
                    let n = self.bytes::<1>(at + 2)?[0];
                    if !matches!(label, 0xf9 | 0xfe | 0xff | 0x01)
                        || (label == 0xf9 && n != 4)
                        || (label == 0xff && n != 11)
                        || (label == 0x01 && n != 12)
                    {
                        return Err(ProbeError::Invalid);
                    }
                    if label == 0xf9 {
                        if self.bytes::<1>(at + 7)?[0] != 0 {
                            return Err(ProbeError::Invalid);
                        }
                        at += 8;
                    } else {
                        at = self.sub_blocks(at + 2)?;
                    }
                }
                _ => return Err(ProbeError::Invalid),
            }
        }
    }
    fn bmp(&mut self) -> PResult<u64> {
        let h = self.bytes::<54>(0)?;
        let size = le32(&h[2..6]) as u64;
        let pixels = le32(&h[10..14]) as u64;
        let dib = le32(&h[14..18]) as u64;
        let width = i32::from_le_bytes(h[18..22].try_into().unwrap());
        let height = i32::from_le_bytes(h[22..26].try_into().unwrap());
        let bits = le16(&h[28..30]);
        if h[6..10] != [0; 4]
            || ![40, 108, 124].contains(&dib)
            || width <= 0
            || height == 0
            || le16(&h[26..28]) != 1
            || ![1, 4, 8, 16, 24, 32].contains(&bits)
            || le32(&h[30..34]) != 0
        {
            return Err(ProbeError::Invalid);
        }
        let colors = le32(&h[46..50]) as u64;
        let palette = if bits <= 8 {
            if colors > (1u64 << bits) {
                return Err(ProbeError::Invalid);
            }
            if colors == 0 {
                1u64 << bits
            } else {
                colors
            }
        } else {
            colors
        };
        let bytes = ((width as u64 * bits as u64).div_ceil(32) * 4)
            .checked_mul((height as i64).unsigned_abs())
            .ok_or(ProbeError::Invalid)?;
        if pixels < 14 + dib + palette * 4
            || pixels.checked_add(bytes).is_none_or(|n| n > size)
            || (le32(&h[34..38]) != 0 && le32(&h[34..38]) as u64 != bytes)
        {
            return Err(ProbeError::Invalid);
        }
        self.bounded(size)
    }
    fn riff(&mut self, kind: Kind) -> PResult<u64> {
        let h = self.bytes::<12>(0)?;
        let size = self.bounded(8 + le32(&h[4..8]) as u64)?;
        if size < 20 || size % 2 != 0 {
            return Err(ProbeError::Invalid);
        }
        let (mut at, mut format, mut data) = (12, false, false);
        let (mut extended, mut animated, mut animation_header) = (false, false, false);
        while at < size {
            if size - at < 8 {
                return Err(ProbeError::Invalid);
            }
            let c = self.bytes::<8>(at)?;
            let len = le32(&c[4..8]) as u64;
            let next = at + 8 + len + len % 2;
            if next > size {
                return Err(ProbeError::Invalid);
            }
            if kind == Kind::Wav {
                if &c[..4] == b"fmt " {
                    if format || len < 16 {
                        return Err(ProbeError::Invalid);
                    }
                    let f = self.bytes::<16>(at + 8)?;
                    if le16(&f[..2]) == 0
                        || le16(&f[2..4]) == 0
                        || le32(&f[4..8]) == 0
                        || le16(&f[12..14]) == 0
                    {
                        return Err(ProbeError::Invalid);
                    }
                    format = true;
                }
                if &c[..4] == b"data" && len > 0 {
                    data = true;
                }
            } else {
                if len % 2 != 0 && self.bytes::<1>(next - 1)?[0] != 0 {
                    return Err(ProbeError::Invalid);
                }
                match &c[..4] {
                    b"VP8 " => {
                        if len < 10 {
                            return Err(ProbeError::Invalid);
                        }
                        let f = self.bytes::<10>(at + 8)?;
                        if f[0] & 1 != 0
                            || f[3..6] != [0x9d, 1, 0x2a]
                            || le16(&f[6..8]) & 0x3fff == 0
                            || le16(&f[8..10]) & 0x3fff == 0
                        {
                            return Err(ProbeError::Invalid);
                        }
                        data = true;
                    }
                    b"VP8L" => {
                        if len < 5 {
                            return Err(ProbeError::Invalid);
                        }
                        let f = self.bytes::<5>(at + 8)?;
                        if f[0] != 0x2f || f[4] & 0xe0 != 0 {
                            return Err(ProbeError::Invalid);
                        }
                        data = true;
                    }
                    b"VP8X" => {
                        if at != 12 || len != 10 {
                            return Err(ProbeError::Invalid);
                        }
                        let f = self.bytes::<10>(at + 8)?;
                        if f[0] & 0xc1 != 0 || f[1..4] != [0; 3] {
                            return Err(ProbeError::Invalid);
                        }
                        extended = true;
                        animated = f[0] & 2 != 0;
                    }
                    b"ANIM" => {
                        if !animated || len != 6 {
                            return Err(ProbeError::Invalid);
                        }
                        animation_header = true;
                    }
                    b"ANMF" => {
                        if !extended || !animation_header || len < 24 {
                            return Err(ProbeError::Invalid);
                        }
                        data = true;
                    }
                    _ => {}
                }
            }
            at = next;
        }
        if !data || (kind == Kind::Wav && !format) || (animated && !animation_header) {
            return Err(ProbeError::Invalid);
        }
        Ok(size)
    }
}
fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b.try_into().unwrap())
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}
type Probed = (u64, Vec<u8>, Vec<u8>);
fn probe<R: Read + Seek>(
    reader: &mut R,
    kind: Kind,
    start: u64,
    end: u64,
    tick: impl FnMut() -> Result<(), String>,
) -> Result<Option<Probed>, String> {
    let mut p = Probe {
        reader,
        start,
        limit: (end - start).min(MAX_FILE),
        fields: 0,
        tick,
    };
    let result = (|| {
        let length = match kind {
            Kind::Gif => p.gif()?,
            Kind::Bmp => p.bmp()?,
            Kind::Wav | Kind::Webp => p.riff(kind)?,
            _ => return Err(ProbeError::Invalid),
        };
        let header = match kind {
            Kind::Gif => p.bytes::<13>(0)?.to_vec(),
            Kind::Bmp => p.bytes::<54>(0)?.to_vec(),
            Kind::Wav | Kind::Webp => p.bytes::<12>(0)?.to_vec(),
            _ => return Err(ProbeError::Invalid),
        };
        let footer = p
            .bytes::<16>(length.checked_sub(16).ok_or(ProbeError::Invalid)?)?
            .to_vec();
        Ok((length, header, footer))
    })();
    match result {
        Ok(v) => Ok(Some(v)),
        Err(ProbeError::Invalid) => Ok(None),
        Err(ProbeError::Failed(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};
    fn evidence(c: &Candidate) -> String {
        serde_json::to_string(&StoredCandidate::from_candidate(c).unwrap()).unwrap()
    }
    #[test]
    fn persisted_cursor_resumes_cross_chunk_headers_active_files_and_forward_probes() {
        let mut bytes = vec![0; CHUNK * 5];
        let g = gif();
        bytes[CHUNK - 7..CHUNK - 7 + g.len()].copy_from_slice(&g);
        bytes[CHUNK + 100..CHUNK + 105].copy_from_slice(b"%PDF-");
        bytes[CHUNK * 3 + 100..CHUNK * 3 + 105].copy_from_slice(b"%%EOF");
        let b = bmp();
        bytes[CHUNK * 4 + 31..CHUNK * 4 + 31 + b.len()].copy_from_slice(&b);
        let options = Options::default();
        let mut full = vec![];
        scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &options,
            |_, _| Ok(()),
            |c| full.push(evidence(&c)),
        )
        .unwrap();
        assert_eq!(full.len(), 3);
        for boundary in 1..5 {
            let mut cursor = Checkpoint::default();
            let mut resumed = vec![];
            assert!(continue_scan(
                &mut Cursor::new(&bytes),
                bytes.len() as u64,
                &options,
                &mut cursor,
                |done, _| if done >= (CHUNK * boundary) as u64 {
                    Err("pause".into())
                } else {
                    Ok(())
                },
                |c| resumed.push(evidence(&c))
            )
            .is_err());
            assert_eq!(cursor.consumed, (CHUNK * boundary) as u64);
            let json = serde_json::to_vec(&cursor).unwrap();
            let mut cursor: Checkpoint = serde_json::from_slice(&json).unwrap();
            cursor
                .verify_cursor(&mut Cursor::new(&bytes), &options)
                .unwrap();
            continue_scan(
                &mut Cursor::new(&bytes),
                bytes.len() as u64,
                &options,
                &mut cursor,
                |_, _| Ok(()),
                |c| resumed.push(evidence(&c)),
            )
            .unwrap();
            assert_eq!(resumed, full);
            assert!(cursor.sealed);
            assert_eq!(cursor.count, 3);
        }
    }
    #[test]
    fn canceled_probe_never_publishes_partial_chunk_and_consumes_budget() {
        let mut bytes = vec![0; CHUNK * 2];
        bytes[..5].copy_from_slice(b"%PDF-");
        bytes[20..25].copy_from_slice(b"%%EOF");
        let g = gif();
        bytes[100..100 + g.len()].copy_from_slice(&g);
        let mut cursor = Checkpoint::default();
        let options = Options::default();
        let mut results = vec![];
        let mut ticks = 0;
        assert!(continue_scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &options,
            &mut cursor,
            |_, _| {
                ticks += 1;
                if ticks == 2 {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |c| results.push(c)
        )
        .is_err());
        assert!(results.is_empty());
        assert_eq!(cursor.count, 0);
        assert_eq!(cursor.consumed, 0);
        assert_eq!(cursor.probes, 1);
        continue_scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &options,
            &mut cursor,
            |_, _| Ok(()),
            |c| results.push(c),
        )
        .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].id, 1);
        assert_eq!(cursor.probes, 2);
        for c in results {
            let record = StoredCandidate::from_candidate(&c).unwrap();
            assert_eq!(
                evidence(
                    &record
                        .candidate(c.id, 0..bytes.len() as u64, &options)
                        .unwrap()
                ),
                evidence(&c)
            );
        }
    }
    #[test]
    fn cursor_rejects_changed_boundary_invalid_bounds_and_sealed_resume() {
        let mut bytes = vec![0; CHUNK * 2];
        let options = Options::default();
        let mut cp = Checkpoint::default();
        let _ = continue_scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &options,
            &mut cp,
            |d, _| {
                if d >= CHUNK as u64 {
                    Err("pause".into())
                } else {
                    Ok(())
                }
            },
            |_| {},
        );
        bytes[CHUNK - 1] = 1;
        assert!(cp
            .verify_cursor(&mut Cursor::new(&bytes), &options)
            .is_err());
        let mut invalid = cp.clone();
        invalid.consumed += 1;
        assert!(invalid.validate(bytes.len() as u64, &options).is_err());
        let mut invalid = cp.clone();
        invalid.active = Some((Kind::Pdf, 0, b"bad".to_vec()));
        assert!(invalid.validate(bytes.len() as u64, &options).is_err());
        cp.sealed = true;
        assert!(continue_scan(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &options,
            &mut cp,
            |_, _| Ok(()),
            |_| panic!("must not emit")
        )
        .is_err());
    }
    fn gif() -> Vec<u8> {
        [
            b"GIF89a".as_slice(),
            &[
                1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255, 0x21, 0xfe, 3, 0x3b, b'x', b'x',
                0, // A comment containing a false trailer.
                0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0, 0x3b,
            ],
        ]
        .concat()
    }
    fn bmp() -> Vec<u8> {
        let mut b = vec![0; 58];
        b[..2].copy_from_slice(b"BM");
        b[2] = 58;
        b[10] = 54;
        b[14] = 40;
        b[18] = 1;
        b[22] = 1;
        b[26] = 1;
        b[28] = 24;
        b[34] = 4;
        b[54..57].fill(255);
        b
    }
    fn riff(form: &[u8; 4], chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0".to_vec();
        b.extend(form);
        for (kind, payload) in chunks {
            b.extend(*kind);
            b.extend((payload.len() as u32).to_le_bytes());
            b.extend(*payload);
            if payload.len() % 2 != 0 {
                b.push(0);
            }
        }
        let n = b.len() as u32 - 8;
        b[4..8].copy_from_slice(&n.to_le_bytes());
        b
    }
    fn wav() -> Vec<u8> {
        riff(
            b"WAVE",
            &[
                (
                    b"fmt ",
                    &[1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0],
                ),
                (b"data", &[128]),
            ],
        )
    }
    fn webp() -> Vec<u8> {
        riff(b"WEBP", &[(b"VP8L", &[0x2f, 0, 0, 0, 0])])
    }
    fn collect(bytes: &[u8], kinds: Vec<Kind>) -> Vec<Candidate> {
        let mut found = vec![];
        scan(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            &Options {
                kinds,
                ..Default::default()
            },
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        found
    }
    #[test]
    fn mixed_formats_export_exact_ranges_and_type_selection_is_respected() {
        let fixtures = [
            gif(),
            bmp(),
            wav(),
            webp(),
            b"%PDF-1.7\nx\n%%EOF".to_vec(),
            b"\xff\xd8\xff\xe0payload\xff\xd9".to_vec(),
            b"\x89PNG\r\n\x1a\npayload\0\0\0\0IEND\xaeB`\x82".to_vec(),
        ];
        let mut bytes = vec![7; 31];
        let mut starts = vec![];
        for file in &fixtures {
            starts.push(bytes.len());
            bytes.extend(file);
            bytes.extend([0; 17]);
        }
        let found = collect(&bytes, ALL.to_vec());
        assert_eq!(found.len(), fixtures.len());
        for (i, c) in found.iter().enumerate() {
            assert_eq!(c.id, i as u32);
            assert_eq!(c.size, fixtures[i].len() as u64);
            let Locator::Signature { offset, .. } = &c.locator else {
                panic!()
            };
            assert_eq!(*offset, starts[i] as u64);
            let mut out = vec![];
            crate::recovery::extract(&mut Cursor::new(&bytes), c, &mut out, MAX_FILE, |_| Ok(()))
                .unwrap();
            assert_eq!(out, fixtures[i]);
        }
        let subset = collect(&bytes, vec![Kind::Gif, Kind::Pdf]);
        assert_eq!(subset.len(), 2);
        assert!(subset[0].name.ends_with(".gif") && subset[1].name.ends_with(".pdf"));
    }
    #[test]
    fn structured_signatures_cross_every_header_boundary_without_duplicates() {
        for file in [gif(), bmp(), wav(), webp()] {
            for overlap in 1..=12 {
                let mut bytes = vec![0; CHUNK - overlap];
                bytes.extend(&file);
                bytes.extend([0; 20]);
                let found = collect(&bytes, ALL.to_vec());
                assert_eq!(found.len(), 1, "overlap {overlap}");
                assert_eq!(found[0].size, file.len() as u64);
            }
        }
    }
    struct Guard {
        bytes: Cursor<Vec<u8>>,
        range: std::ops::Range<u64>,
        fail_at: Option<u64>,
    }
    impl Read for Guard {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let at = self.bytes.position();
            assert!(
                at >= self.range.start && at + b.len() as u64 <= self.range.end,
                "read outside requested range"
            );
            if self.fail_at == Some(at) {
                return Err(io::Error::other("injected read failure"));
            }
            self.bytes.read(b)
        }
    }
    impl Seek for Guard {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(pos)
        }
    }
    #[test]
    fn selected_range_excludes_other_data_and_never_reads_outside_its_bounds() {
        let file = gif();
        let mut bytes = file.clone();
        bytes.extend([0; 37]);
        let start = bytes.len() as u64;
        bytes.extend(&file);
        let end = bytes.len() as u64;
        bytes.extend(&file);
        let mut reader = Guard {
            bytes: Cursor::new(bytes),
            range: start..end,
            fail_at: None,
        };
        let mut found = vec![];
        let mut last = 0;
        let source_size = reader.bytes.get_ref().len() as u64;
        scan(
            &mut reader,
            source_size,
            &Options {
                start,
                length: Some(end - start),
                ..Default::default()
            },
            |done, total| {
                assert!(done >= last && done <= total);
                last = done;
                Ok(())
            },
            |c| found.push(c),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(last, end - start);
        let Locator::Signature { offset, .. } = found[0].locator else {
            panic!()
        };
        assert_eq!(offset, start);
        let mut out = vec![];
        crate::recovery::extract(&mut reader, &found[0], &mut out, MAX_FILE, |_| Ok(())).unwrap();
        assert_eq!(out, file);
        assert!(collect(&file[..file.len() - 1], vec![Kind::Gif]).is_empty());
    }
    #[test]
    fn truncation_invalid_geometry_and_unsupported_bmp_are_not_candidates() {
        for file in [gif(), bmp(), wav(), webp()] {
            for len in 1..file.len() {
                assert!(collect(&file[..len], ALL.to_vec()).is_empty(), "len {len}");
            }
        }
        for (offset, value) in [
            (2, 54),
            (10, 10),
            (18, 0),
            (22, 0),
            (26, 2),
            (28, 3),
            (30, 1),
        ] {
            let mut b = bmp();
            b[offset] = value;
            assert!(collect(&b, vec![Kind::Bmp]).is_empty());
        }
        let mut b = bmp();
        b[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(collect(&b, vec![Kind::Bmp]).is_empty());
        let mut b = gif();
        b[6] = 0;
        assert!(collect(&b, vec![Kind::Gif]).is_empty());
    }
    #[test]
    fn riff_chunk_bounds_required_metadata_and_padding_are_checked() {
        assert!(collect(&riff(b"WAVE", &[(b"data", b"x")]), vec![Kind::Wav]).is_empty());
        assert!(collect(&riff(b"WEBP", &[(b"JUNK", b"abcdef")]), vec![Kind::Webp]).is_empty());
        let mut b = webp();
        *b.last_mut().unwrap() = 1;
        assert!(collect(&b, vec![Kind::Webp]).is_empty());
        let mut b = webp();
        b[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(collect(&b, vec![Kind::Webp]).is_empty());
        let mut b = wav();
        b[4..8].copy_from_slice(&12u32.to_le_bytes());
        assert!(collect(&b, vec![Kind::Wav]).is_empty());
    }
    #[test]
    fn input_validation_precedes_reads_and_rejects_unknown_types_or_paths() {
        for options in [
            Options {
                kinds: vec![],
                ..Default::default()
            },
            Options {
                kinds: vec![Kind::Gif, Kind::Gif],
                ..Default::default()
            },
            Options {
                start: 10,
                length: Some(0),
                ..Default::default()
            },
            Options {
                start: u64::MAX,
                length: Some(2),
                ..Default::default()
            },
            Options {
                start: 1,
                length: Some(100),
                ..Default::default()
            },
        ] {
            assert!(scan(
                &mut Cursor::new(vec![]),
                100,
                &options,
                |_, _| panic!("invalid options reached scan"),
                |_| panic!()
            )
            .is_err());
        }
        for json in [
            r#"{"kinds":["exe"]}"#,
            r#"{"path":"C:\\data"}"#,
            r#"{"start":-1}"#,
            r#"{"start":0.5}"#,
        ] {
            assert!(serde_json::from_str::<Options>(json).is_err());
        }
    }
    #[test]
    fn probe_cancellation_and_io_errors_propagate_without_reporting_a_file() {
        let b = gif();
        let mut calls = 0;
        let err = scan(
            &mut Cursor::new(&b),
            b.len() as u64,
            &Options::default(),
            |_, _| {
                calls += 1;
                if calls == 2 {
                    Err("canceled".into())
                } else {
                    Ok(())
                }
            },
            |_| panic!("canceled probe published a file"),
        )
        .unwrap_err();
        assert_eq!(err, "canceled");
        let mut reader = Guard {
            bytes: Cursor::new(b.clone()),
            range: 0..b.len() as u64,
            fail_at: Some(19),
        };
        assert!(scan(
            &mut reader,
            b.len() as u64,
            &Options::default(),
            |_, _| Ok(()),
            |_| panic!()
        )
        .unwrap_err()
        .contains("injected"));
    }
    #[test]
    fn untrusted_small_inputs_and_false_headers_never_panic() {
        let mut seed = 31u64;
        for len in 1..300 {
            let mut b = vec![0; len];
            for value in &mut b {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *value = (seed >> 32) as u8;
            }
            for prefix in [b"BM".as_slice(), b"GIF89a", b"RIFF\xff\xff\xff\xffWEBP"] {
                if b.len() >= prefix.len() {
                    b[..prefix.len()].copy_from_slice(prefix);
                }
                let _ = collect(&b, ALL.to_vec());
            }
        }
    }
    #[test]
    fn complexity_and_file_size_limits_stop_probes_and_export_rechecks_boundaries() {
        let mut complex = gif()[..19].to_vec();
        complex.extend([0x21, 0xfe]);
        for _ in 0..MAX_FIELDS {
            complex.extend([1, 0]);
        }
        complex.extend([0, 0x3b]);
        assert!(probe(
            &mut Cursor::new(&complex),
            Kind::Gif,
            0,
            complex.len() as u64,
            || Ok(())
        )
        .unwrap()
        .is_none());
        let mut oversized = bmp();
        oversized[2..6].copy_from_slice(&((MAX_FILE + 1) as u32).to_le_bytes());
        assert!(probe(
            &mut Cursor::new(&oversized),
            Kind::Bmp,
            0,
            MAX_FILE + 1,
            || Ok(())
        )
        .unwrap()
        .is_none());
        let source = wav();
        let candidates = collect(&source, vec![Kind::Wav]);
        for offset in [4, source.len() - 1] {
            let mut changed = source.clone();
            changed[offset] ^= 1;
            let mut out = vec![];
            assert!(crate::recovery::extract(
                &mut Cursor::new(&changed),
                &candidates[0],
                &mut out,
                MAX_FILE,
                |_| Ok(())
            )
            .is_err());
            assert!(out.is_empty());
        }
    }
}

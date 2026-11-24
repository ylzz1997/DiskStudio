//! Bounded, read-only carving. Structure checks delimit candidates, not decoded
//! content integrity. All offsets are relative to the already selected source.
use crate::recovery::{Candidate, Locator, MAX_RESULTS};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};

#[path = "signature_audio.rs"]
mod audio;
#[path = "signature_audiovisual.rs"]
mod audiovisual;
#[path = "signature_flac.rs"]
pub(crate) mod flac;
#[path = "signature_fragment_io.rs"]
pub(crate) mod fragment_io;
#[path = "signature_images.rs"]
mod images;
#[path = "signature_media.rs"]
mod media;
#[path = "signature_ogg_fragments.rs"]
pub(crate) mod ogg_fragments;
#[path = "signature_png.rs"]
pub(crate) mod png;
#[path = "signature_rar.rs"]
mod rar;

pub const MAX_FILE: u64 = 256 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
const MAX_PROBES: usize = 1_000_000;
const MAX_FIELDS: usize = 65_536;
const SEVEN_ZIP_MAGIC: &[u8; 6] = b"7z\xbc\xaf\x27\x1c";
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const MAX_SEVEN_ZIP_HEADER: u64 = 8 * 1024 * 1024;
const FRAGMENT_NOTE: &str = "ZIP 碎片候选。每个成员与目录记录唯一匹配，所有条目通过 CRC，预览或导出会复核完整 SHA-256。原名和删除状态未知。";
const OGG_FRAGMENT_NOTE: &str = "Ogg 逻辑流候选。已核对流序列号、完整页序列、包边界和各页 CRC。预览或导出会复核完整 SHA-256。";
const FLAC_FRAGMENT_NOTE: &str = "FLAC 碎片候选。已核对帧顺序、CRC 及解码样本的 STREAMINFO MD5。预览与导出会复核完整 SHA-256。";
const PNG_FRAGMENT_NOTE: &str = "PNG 碎片候选。已核对各块 CRC、完整 zlib 校验和以及扫描行。预览和导出会复核完整 SHA-256。";
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
    Docm,
    Xlsm,
    Pptm,
    Odt,
    Ods,
    Odp,
    SevenZip,
    Sqlite,
    Mp4,
    Mov,
    Ogg,
    Tiff,
    Ico,
    Cur,
    Rar,
    Avi,
    Aiff,
    Aifc,
    Flac,
    Midi,
    Dsf,
    Dff,
}
const ALL: [Kind; 33] = [
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
    Kind::Docm,
    Kind::Xlsm,
    Kind::Pptm,
    Kind::Odt,
    Kind::Ods,
    Kind::Odp,
    Kind::SevenZip,
    Kind::Sqlite,
    Kind::Mp4,
    Kind::Mov,
    Kind::Ogg,
    Kind::Tiff,
    Kind::Ico,
    Kind::Cur,
    Kind::Rar,
    Kind::Avi,
    Kind::Aiff,
    Kind::Aifc,
    Kind::Flac,
    Kind::Midi,
    Kind::Dsf,
    Kind::Dff,
];
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub kinds: Vec<Kind>,
    pub start: u64,
    pub length: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub free_only: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sector_aligned: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub zip_fragments: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ogg_fragments: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub flac_fragments: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub png_fragments: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            kinds: ALL.to_vec(),
            start: 0,
            length: None,
            free_only: false,
            sector_aligned: false,
            zip_fragments: false,
            ogg_fragments: false,
            flac_fragments: false,
            png_fragments: false,
        }
    }
}
impl Options {
    fn fragment_phases(&self) -> impl Iterator<Item = &'static str> {
        [
            (self.zip_fragments, "重组 ZIP 碎片"),
            (self.ogg_fragments, "重组 Ogg 碎片"),
            (self.flac_fragments, "重组 FLAC 碎片"),
            (self.png_fragments, "重组 PNG 碎片"),
        ].into_iter().filter_map(|(enabled, label)| enabled.then_some(label))
    }

    fn has_fragments(&self) -> bool {
        self.fragment_phases().next().is_some()
    }

    pub fn work(&self, bytes: u64) -> u64 {
        bytes.saturating_mul(1 + self.fragment_phases().count() as u64)
    }

    pub fn phase(&self, done: u64, size: u64) -> &'static str {
        if done >= size {
            let mut end = size;
            for label in self.fragment_phases() {
                end = end.saturating_add(size);
                if done <= end { return label }
            }
        }
        "签名扫描"
    }

    pub fn alignment(&self, sector: u64) -> Result<u64, String> {
        if !self.sector_aligned {
            return Ok(1);
        }
        if !matches!(sector, 512 | 4096) {
            return Err("按扇区对齐需要已核对的 512 字节或 4 KiB 逻辑扇区。".into());
        }
        Ok(sector)
    }

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
        if self.zip_fragments && !self.kinds.iter().any(|kind| kind.zip_family()) {
            return Err("重组 ZIP 碎片需要选择 ZIP 或一种文档包类型。".into());
        }
        if self.ogg_fragments && !self.kinds.contains(&Kind::Ogg) {
            return Err("重组 Ogg 碎片需要选择 Ogg 文件类型。".into());
        }
        if self.flac_fragments && !self.kinds.contains(&Kind::Flac) {
            return Err("重组 FLAC 碎片需要选择 FLAC 文件类型。".into());
        }
        if self.png_fragments && !self.kinds.contains(&Kind::Png) {
            return Err("重组 PNG 碎片需要选择 PNG 文件类型。".into());
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
/// Preserve the original ZIP record shape in existing scan files.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum FragmentLayout {
    Zip(crate::signature_zip::fragments::Layout),
    Ogg(ogg_fragments::Layout),
    Flac(flac::fragments::Layout),
    Png(png::fragments::Layout),
}
impl FragmentLayout {
    fn parts(&self) -> &[fragment_io::Part] {
        match self {
            Self::Zip(layout) => &layout.parts,
            Self::Ogg(layout) => &layout.runs,
            Self::Flac(layout) => &layout.flac_runs,
            Self::Png(layout) => &layout.png_runs,
        }
    }

    fn note(&self) -> &'static str {
        match self {
            Self::Zip(_) => FRAGMENT_NOTE,
            Self::Ogg(_) => OGG_FRAGMENT_NOTE,
            Self::Flac(_) => FLAC_FRAGMENT_NOTE,
            Self::Png(_) => PNG_FRAGMENT_NOTE,
        }
    }

    fn valid(&self, kind: Kind, start: u64, size: u64, range: &std::ops::Range<u64>, options: &Options) -> bool {
        match self {
            Self::Zip(layout) => kind.zip_family() && options.zip_fragments && layout.valid(start, size, range),
            Self::Ogg(layout) => kind == Kind::Ogg && options.ogg_fragments && layout.valid(start, size, range),
            Self::Flac(layout) => kind == Kind::Flac && options.flac_fragments && layout.valid(start, size, range),
            Self::Png(layout) => kind == Kind::Png && options.png_fragments && layout.valid(start, size, range),
        }
    }
}
impl Kind {
    fn zip_family(self) -> bool {
        matches!(
            self,
            Self::Zip
                | Self::Docx
                | Self::Xlsx
                | Self::Pptx
                | Self::Docm
                | Self::Xlsm
                | Self::Pptm
                | Self::Odt
                | Self::Ods
                | Self::Odp
        )
    }

    fn requires_digest(self) -> bool {
        self.zip_family()
            || matches!(
                self,
                Self::SevenZip
                    | Self::Png
                    | Self::Sqlite
                    | Self::Mp4
                    | Self::Mov
                    | Self::Ogg
                    | Self::Tiff
                    | Self::Ico
                    | Self::Cur
                    | Self::Rar
                    | Self::Avi
                    | Self::Aiff
                    | Self::Aifc
                    | Self::Flac
                    | Self::Midi
                    | Self::Dsf
                    | Self::Dff
            )
    }

    fn starts(self, b: &[u8]) -> bool {
        match self {
            Self::Png => b.starts_with(png::MAGIC),
            Self::Jpeg => b.starts_with(b"\xff\xd8\xff"),
            Self::Pdf => b.starts_with(b"%PDF-"),
            Self::Gif => b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a"),
            Self::Bmp => b.starts_with(b"BM"),
            Self::Wav => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE"),
            Self::Webp => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"),
            Self::SevenZip => b.starts_with(SEVEN_ZIP_MAGIC),
            Self::Sqlite => b.starts_with(SQLITE_MAGIC),
            Self::Mp4 | Self::Mov => media::movie_kind(b) == Some(self),
            Self::Ogg => media::ogg_start(b),
            Self::Tiff => images::tiff_start(b),
            Self::Ico | Self::Cur => images::icon_kind(b) == Some(self),
            Self::Rar => rar::starts(b),
            Self::Avi | Self::Aiff | Self::Aifc => audiovisual::kind(b) == Some(self),
            Self::Flac => b.starts_with(b"fLaC"),
            Self::Midi => audio::midi_start(b),
            Self::Dsf => audio::dsf_start(b),
            Self::Dff => audio::dff_start(b),
            Self::Zip
            | Self::Docx
            | Self::Xlsx
            | Self::Pptx
            | Self::Docm
            | Self::Xlsm
            | Self::Pptm
            | Self::Odt
            | Self::Ods
            | Self::Odp => false, // EOCD-anchored, below.
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
            Self::Docm => "docm",
            Self::Xlsm => "xlsm",
            Self::Pptm => "pptm",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::SevenZip => "7z",
            Self::Sqlite => "sqlite",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Ogg => "ogg",
            Self::Tiff => "tif",
            Self::Ico => "ico",
            Self::Cur => "cur",
            Self::Rar => "rar",
            Self::Avi => "avi",
            Self::Aiff => "aiff",
            Self::Aifc => "aifc",
            Self::Flac => "flac",
            Self::Midi => "mid",
            Self::Dsf => "dsf",
            Self::Dff => "dff",
        }
    }
    fn footer(self) -> Option<&'static [u8]> {
        match self {
            Self::Png => Some(png::IEND),
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
            Self::SevenZip => 32,
            Self::Sqlite => 100,
            Self::Mp4 | Self::Mov => 12,
            Self::Ogg => 27,
            Self::Tiff => 16,
            Self::Ico | Self::Cur => 6,
            Self::Rar => 8,
            Self::Avi | Self::Aiff | Self::Aifc => 12,
            Self::Flac => 42,
            Self::Midi => 14,
            Self::Dsf => 28,
            Self::Dff => 16,
            Self::Zip
            | Self::Docx
            | Self::Xlsx
            | Self::Pptx
            | Self::Docm
            | Self::Xlsm
            | Self::Pptm
            | Self::Odt
            | Self::Ods
            | Self::Odp => 0,
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
        note: match kind {
            Kind::Png => "连续 PNG 候选。已核对图像块顺序和各块 CRC，尚未解码像素。",
            Kind::SevenZip => "连续 7z 候选。已核对起始头和后续头的 CRC，未解压或验证文件内容。",
            Kind::Sqlite => {
                "连续 SQLite 主数据库候选。已核对文件头和首页结构，未验证其余数据库页或恢复 WAL。"
            }
            Kind::Mp4 | Kind::Mov => {
                "连续电影容器候选。已核对媒体块的采样表与数据范围，未解码音频或视频。"
            }
            Kind::Ogg => "连续 Ogg 候选。已核对各流的页序列、包边界与 CRC，未解码音频或视频。",
            Kind::Tiff => "连续 TIFF 候选。已核对图像目录和像素数据范围，未解码像素。",
            Kind::Ico | Kind::Cur => "连续图标或光标候选。已核对目录与内嵌图像范围，未解码像素。",
            Kind::Rar => "连续 RAR 候选。已核对各块头 CRC 和结束块，未解压存档内容。",
            Kind::Avi => "连续 AVI 候选。已核对流头、媒体块和现有索引，未解码音频或视频。",
            Kind::Aiff | Kind::Aifc => {
                "连续 AIFF 音频候选。已核对采样格式、帧数和声音数据范围，未解码音频。"
            }
            Kind::Flac => {
                "连续 FLAC 候选。已核对音频帧顺序、采样数和帧 CRC，未解码 PCM 或核对其 MD5。"
            }
            Kind::Midi => "连续 MIDI 候选。已核对音轨、事件范围和音轨结束标记。",
            Kind::Dsf | Kind::Dff => {
                "连续 DSD 音频候选。已核对声道、数据边界和封装结构，未解码音频。"
            }
            _ => "连续数据候选；未验证解码完整性，原文件名和删除状态未知",
        }
        .into(),
        supported: true,
        directory: None,
        directory_note: "签名扫描无法确定原目录".into(),
        locator: Locator::Signature {
            content_hash: None,
            offset: start,
            header,
            footer,
            allocation_digest: None,
            fragments: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fragments: Option<FragmentLayout>,
}
impl StoredCandidate {
    pub fn fragmented(&self) -> bool {
        self.fragments.is_some()
    }

    pub fn aligned(&self, alignment: u64) -> bool {
        self.offset & (alignment - 1) == 0
    }

    pub fn from_candidate(c: &Candidate) -> Result<Self, String> {
        let Locator::Signature {
            offset,
            header,
            footer,
            content_hash,
            fragments,
            ..
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
            fragments: fragments.clone(),
        })
    }
    pub fn candidate(
        &self,
        id: u32,
        range: std::ops::Range<u64>,
        options: &Options,
        allocation_digest: Option<&str>,
    ) -> Result<Candidate, String> {
        let archive = self.kind.zip_family();
        if !options.kinds.contains(&self.kind)
            || self.size == 0
            || self.size > MAX_FILE
            || match &self.fragments {
                Some(layout) => !layout.valid(self.kind, self.offset, self.size, &range, options),
                None => self.offset < range.start || self.offset.checked_add(self.size)
                    .is_none_or(|end| end > range.end),
            }
            || self.header.len() as u64 > self.size
            || self.footer.len() as u64 > self.size
            || self.header.len() > 100
            || self.footer.len() > 16
            || (self.kind.requires_digest() != self.content_hash.is_some()
                && !(self.kind == Kind::Png && self.fragments.is_none()))
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
        let stored_length = match self.kind {
            Kind::SevenZip => seven_zip_geometry(&self.header).map(|geometry| geometry.end),
            Kind::Sqlite => sqlite_geometry(&self.header).map(|geometry| geometry.size()),
            Kind::Avi | Kind::Aiff | Kind::Aifc => audiovisual::declared_size(&self.header),
            Kind::Flac => flac::valid_streaminfo(&self.header).then_some(self.size),
            Kind::Dsf | Kind::Dff => audio::declared_size(self.kind, &self.header),
            _ => Some(self.size),
        };
        if stored_length != Some(self.size) {
            return Err("保存的签名候选容量与文件头不一致。".into());
        }
        let mut c = candidate(
            id,
            self.kind,
            self.offset,
            self.size,
            self.header.clone(),
            self.footer.clone(),
        );
        if let Locator::Signature {
            content_hash,
            allocation_digest: digest,
            fragments,
            ..
        } = &mut c.locator
        {
            *content_hash = self.content_hash;
            *digest = allocation_digest.map(str::to_owned);
            *fragments = self.fragments.clone();
        }
        if archive {
            c.note="连续 ZIP 容器；保存时已检查目录与 CRC，提取前复核内容摘要。加载的结果是历史观察，不保证源内容未改变。".into();
        }
        if let Some(layout) = &self.fragments {
            c.note = layout.note().into();
        } else if self.kind == Kind::Png && self.content_hash.is_none() {
            c.note = "历史 PNG 候选仅记录起止签名。请重新扫描以校验图像块。".into();
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allocation_digest: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fragments_checked: bool,
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
            allocation_digest: None,
            fragments_checked: false,
        }
    }
}
impl Checkpoint {
    pub fn work_done(&self, options: &Options) -> u64 {
        if self.fragments_checked {
            options.work(self.consumed)
        } else {
            self.consumed
        }
    }

    pub fn allocation_digest(&self) -> Option<&str> {
        self.allocation_digest.as_deref()
    }

    pub fn validate(&self, size: u64, options: &Options) -> Result<(), String> {
        if self.consumed > size
            || (self.fragments_checked && (!options.has_fragments() || !self.sealed || self.consumed != size))
            || self.searched > size
            || self.carry.len() > 16
            || self.carry.len() as u64 > self.consumed
            || self.count as usize > MAX_RESULTS
            || self.probes > MAX_PROBES + 1
            || self.rejected as u64 > (MAX_PROBES as u64).saturating_add(size)
            || self.archive_remaining > 64 * 1024 * 1024 * 1024
            || self.allocation_digest.as_ref().is_some_and(|digest| {
                !options.free_only
                    || digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            || (options.free_only
                && self.allocation_digest.is_none()
                && (self.consumed != 0 || self.searched != 0 || self.count != 0 || self.sealed))
        {
            return Err("签名扫描进度越界。".into());
        }
        if !self.sealed
            && (self.count as usize >= MAX_RESULTS
                || (self.consumed == size && !options.has_fragments())
                || (!options.free_only
                    && ((self.consumed != size && self.consumed % CHUNK as u64 != 0)
                        || self.carry.len() as u64 != self.consumed.min(16)))
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

    pub fn verify_scope(
        &self,
        options: &Options,
        plan: &crate::used_space::Plan,
    ) -> Result<(), String> {
        if self
            .allocation_digest
            .as_ref()
            .is_some_and(|digest| *digest != plan.digest)
        {
            return Err("空闲簇分配信息已变化，请重新扫描。".into());
        }
        let range = options.range(plan.size)?;
        let consumed = range.start + self.consumed;
        let carry_start = consumed.saturating_sub(self.carry.len() as u64);
        if (!self.carry.is_empty() && !within_free(plan, carry_start, consumed))
            || self
                .active
                .as_ref()
                .is_some_and(|(_, start, _)| !within_free(plan, range.start + start, consumed))
        {
            return Err("续扫边界已超出核对的空闲范围。".into());
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
    tick: impl FnMut(u64, u64) -> Result<(), String>,
    found: impl FnMut(Candidate),
) -> Result<String, String> {
    continue_scan_scoped(
        reader,
        source_size,
        options,
        checkpoint,
        1,
        None,
        tick,
        found,
    )
}

fn within_free(plan: &crate::used_space::Plan, start: u64, end: u64) -> bool {
    let ranges = plan.free_ranges();
    let index = ranges.partition_point(|r| r.end <= start);
    start < end
        && ranges
            .get(index)
            .is_some_and(|r| r.start <= start && end <= r.end)
}

pub fn verify_free_candidates<'a>(
    plan: &crate::used_space::Plan,
    files: impl IntoIterator<Item = &'a Candidate>,
) -> Result<(), String> {
    for c in files {
        if let Locator::Signature {
            offset,
            allocation_digest: Some(digest),
            fragments,
            ..
        } = &c.locator
        {
            if digest != &plan.digest {
                return Err("空闲簇分配信息已变化，请重新扫描。".into());
            }
            let is_free = |start: u64, size| {
                start.checked_add(size).is_some_and(|end| within_free(plan, start, end))
            };
            if !fragments.as_ref().map_or_else(
                || is_free(*offset, c.size),
                |layout| layout.parts().iter().all(|part| is_free(part.start, part.length)),
            ) {
                return Err("候选文件的范围已不完全位于核对的空闲簇中，请重新扫描。".into());
            }
        }
    }
    Ok(())
}

pub fn continue_scan_scoped<R: Read + Seek>(
    reader: &mut R,
    source_size: u64,
    options: &Options,
    checkpoint: &mut Checkpoint,
    sector: u64,
    allocation: Option<&crate::used_space::Plan>,
    mut tick: impl FnMut(u64, u64) -> Result<(), String>,
    mut found: impl FnMut(Candidate),
) -> Result<String, String> {
    let range = options.range(source_size)?;
    let size = range.end - range.start;
    let total = options.work(size);
    let alignment = options.alignment(sector)?;
    checkpoint.validate(size, options)?;
    if checkpoint.sealed {
        return Err("扫描已结束，不能继续。".into());
    }
    if options.free_only != allocation.is_some()
        || allocation.is_some_and(|p| p.size != source_size || p.sector != sector)
    {
        return Err("仅空闲簇扫描需要已核对的分区分配信息。".into());
    }
    let allocation_digest = allocation.map(|p| p.digest.clone());
    if let Some(plan) = allocation {
        checkpoint.verify_scope(options, plan)?;
        checkpoint.allocation_digest = allocation_digest.clone();
    }
    let mut publish = |mut candidate: Candidate| {
        if let Locator::Signature {
            allocation_digest: digest,
            ..
        } = &mut candidate.locator
        {
            *digest = allocation_digest.clone();
        }
        found(candidate);
    };
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
            Kind::Zip
            | Kind::Docx
            | Kind::Xlsx
            | Kind::Pptx
            | Kind::Docm
            | Kind::Xlsm
            | Kind::Pptm
            | Kind::Odt
            | Kind::Ods
            | Kind::Odp => b'P',
            Kind::SevenZip => SEVEN_ZIP_MAGIC[0],
            Kind::Sqlite => SQLITE_MAGIC[0],
            Kind::Mp4
            | Kind::Mov
            | Kind::Ogg
            | Kind::Tiff
            | Kind::Ico
            | Kind::Cur
            | Kind::Rar
            | Kind::Avi
            | Kind::Aiff
            | Kind::Aifc
            | Kind::Flac
            | Kind::Midi
            | Kind::Dsf
            | Kind::Dff => continue,
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
    let archives = options.kinds.iter().any(|k| k.zip_family());
    let png = options.kinds.contains(&Kind::Png);
    let movies = options.kinds.contains(&Kind::Mp4) || options.kinds.contains(&Kind::Mov);
    let ogg = options.kinds.contains(&Kind::Ogg);
    let tiff = options.kinds.contains(&Kind::Tiff);
    let icons = options.kinds.contains(&Kind::Ico) || options.kinds.contains(&Kind::Cur);
    let rar = options.kinds.contains(&Kind::Rar);
    let flac = options.kinds.contains(&Kind::Flac);
    let midi = options.kinds.contains(&Kind::Midi);
    let dsf = options.kinds.contains(&Kind::Dsf);
    let dff = options.kinds.contains(&Kind::Dff);
    let audiovisual = options
        .kinds
        .iter()
        .any(|kind| matches!(kind, Kind::Avi | Kind::Aiff | Kind::Aifc));
    let lookahead = if dff || options.kinds.contains(&Kind::Sqlite) {
        16
    } else {
        12
    };
    let mut archive_budget = crate::signature_zip::Budget::resume(checkpoint.archive_remaining)?;
    let result = (|| {
        while consumed < size {
            tick(consumed, total)?;
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
                        allocation_digest: allocation_digest.clone(),
                        fragments_checked: false,
                    };
                    for candidate in unpublished.drain(..) {
                        publish(candidate);
                    }
                    return Ok($message);
                }};
            }
            let (span_start, span_end) = if let Some(plan) = allocation {
                let ranges = plan.free_ranges();
                let index = ranges.partition_point(|f| f.end <= range.start + consumed);
                let Some(span) = ranges.get(index).filter(|f| f.start < range.end) else {
                    if options.has_fragments() && consumed != 0 {
                        consumed = size;
                        checkpoint.consumed = size;
                        checkpoint.searched = size;
                        break;
                    }
                    let message = if consumed == 0 {
                        "所选范围没有可核对的空闲簇。".into()
                    } else {
                        complete_message(count, rejected)
                    };
                    stop!(message, size);
                };
                let start = span.start.saturating_sub(range.start);
                if consumed < start {
                    consumed = start;
                    searched = start;
                }
                (start, span.end.min(range.end) - range.start)
            } else {
                (0, size)
            };
            let n = (span_end - consumed).min(CHUNK as u64) as usize;
            let end_of_span = consumed + n as u64 == span_end;
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
            while i + lookahead <= end || (end_of_span && i < end) {
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
                        range.start + span_start..range.start + span_end,
                        &mut archive_budget,
                        || tick(at.max(consumed), total),
                    )? {
                        if options.kinds.contains(&a.kind) && a.start & (alignment - 1) == 0 {
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
                let aligned = (range.start + at) & (alignment - 1) == 0;
                // Structurally bounded candidates can start inside a stray open
                // PDF or JPEG anchor. Validate their ranges independently.
                let bytes = &buffer[i..end];
                let structured = aligned
                    .then(|| match buffer[i] {
                        b'O' if ogg && media::ogg_start(bytes) => Some(Kind::Ogg),
                        0x89 if png && bytes.starts_with(png::MAGIC) => Some(Kind::Png),
                        b'R' if rar && rar::starts(bytes) => Some(Kind::Rar),
                        b'f' if flac && Kind::Flac.starts(bytes) => Some(Kind::Flac),
                        b'M' if midi && audio::midi_start(bytes) => Some(Kind::Midi),
                        b'D' if dsf && audio::dsf_start(bytes) => Some(Kind::Dsf),
                        b'F' if dff && audio::dff_start(bytes) => Some(Kind::Dff),
                        b'R' | b'F' if audiovisual => {
                            audiovisual::kind(bytes).filter(|kind| options.kinds.contains(kind))
                        }
                        b'I' | b'M' if tiff && images::tiff_start(bytes) => Some(Kind::Tiff),
                        0 if movies && bytes.get(4..8) == Some(b"ftyp") => {
                            media::movie_kind(bytes).filter(|kind| options.kinds.contains(kind))
                        }
                        0 if icons && matches!(bytes.get(2), Some(1 | 2)) => {
                            images::icon_kind(bytes).filter(|kind| options.kinds.contains(kind))
                        }
                        _ => None,
                    })
                    .flatten();
                if let Some((kind, start, header)) = &active {
                    let footer = kind.footer().expect("only terminated kinds remain active");
                    if at - start >= MAX_FILE {
                        active = None;
                        rejected += 1;
                    } else if buffer[i..end].starts_with(footer) {
                        let length = at + footer.len() as u64 - start;
                        if length <= MAX_FILE && *kind == Kind::Png {
                            // Legacy paused cursors can already contain a PNG
                            // start. Verify its real end through the new parser.
                            let absolute = range.start + start;
                            let verified = probe(reader, *kind, absolute, range.start + span_end,
                                &mut archive_budget, || tick(at.max(consumed), total))?;
                            let has_verified_end = verified.is_some();
                            if let Some((parsed, header, footer)) = verified.filter(|(size, _, _)| *size == length) {
                                let mut c = candidate(count, *kind, absolute, parsed, header, footer);
                                archive_budget.charge(parsed)?;
                                let digest = crate::signature_zip::hash_range(reader, absolute, parsed, || tick(at.max(consumed), total))?;
                                if let Locator::Signature { content_hash, .. } = &mut c.locator { *content_hash = Some(digest); }
                                unpublished.push(c);
                                count += 1;
                            } else if has_verified_end {
                                i += 1;
                                continue;
                            } else {
                                rejected += 1;
                            }
                        } else if length <= MAX_FILE {
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
                let kind = structured.or_else(|| {
                    if active.is_some() || !aligned || !possible_start[buffer[i] as usize] {
                        return None;
                    }
                    options
                        .kinds
                        .iter()
                        .copied()
                        .find(|k| k.starts(&buffer[i..end]))
                });
                if let Some(kind) = kind {
                    if kind.footer().is_some() && kind != Kind::Png {
                        active = Some((kind, at, buffer[i..i + kind.header_len()].to_vec()));
                    } else {
                        probes += 1;
                        if probes > MAX_PROBES {
                            stop!(
                                "达到 100 万次结构候选检查上限，扫描提前结束；已找到的结果保留。"
                                    .into(),
                                at
                            );
                        }
                        let absolute = range.start + at;
                        let mut check_progress = || tick(at.max(consumed), total);
                        let result = probe(
                            reader,
                            kind,
                            absolute,
                            range.start + span_end,
                            &mut archive_budget,
                            &mut check_progress,
                        )?;
                        if let Some((length, header, footer)) = result {
                            let mut c = candidate(count, kind, absolute, length, header, footer);
                            if kind.requires_digest() {
                                archive_budget.charge(length)?;
                                let digest = crate::signature_zip::hash_range(
                                    reader,
                                    absolute,
                                    length,
                                    check_progress,
                                )?;
                                if let Locator::Signature { content_hash, .. } = &mut c.locator {
                                    *content_hash = Some(digest);
                                }
                            }
                            unpublished.push(c);
                            count += 1;
                            if count as usize >= MAX_RESULTS {
                                stop!("达到结果上限，扫描提前结束。".into(), at);
                            }
                            // Resume past the verified container and its embedded data.
                            active = None;
                            i = (at + length - base) as usize;
                            continue;
                        }
                        rejected += 1;
                    }
                }
                i += 1;
            }
            searched = base + i as u64;
            consumed += n as u64;
            if allocation.is_some() && end_of_span {
                rejected += usize::from(active.take().is_some());
                searched = consumed;
                carry = 0;
            } else {
                carry = end.min(16);
            }
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
                sealed: consumed == size && !options.has_fragments(),
                allocation_digest: allocation_digest.clone(),
                fragments_checked: false,
            };
            for candidate in unpublished {
                publish(candidate);
            }
        }
        if active.is_some() {
            rejected += 1;
        }
        if options.has_fragments() {
            let mut candidates = Vec::new();
            let mut limited = false;
            let mut add = |kind, start, size, header, footer, layout: FragmentLayout, digest| {
                let mut c = candidate(count + candidates.len() as u32, kind, start, size, header, footer);
                c.note = layout.note().into();
                if let Locator::Signature { content_hash, fragments, .. } = &mut c.locator {
                    *content_hash = Some(digest);
                    *fragments = Some(layout);
                }
                candidates.push(c);
            };
            let mut remaining = MAX_RESULTS - count as usize;
            if options.zip_fragments {
                let result = crate::signature_zip::fragments::search(
                    reader, range.clone(), allocation, alignment, &options.kinds,
                    remaining, &mut archive_budget,
                    |done| tick(size.saturating_add(done), total),
                )?;
                limited |= result.limited;
                remaining -= result.hits.len();
                for hit in result.hits {
                    let a = hit.archive;
                    add(a.kind, a.start, a.size, a.header, a.footer, FragmentLayout::Zip(hit.layout), a.digest);
                }
            }
            if options.ogg_fragments && remaining > 0 {
                let prior = size.saturating_mul(1 + u64::from(options.zip_fragments));
                let result = ogg_fragments::search(
                    reader, range.clone(), allocation, alignment, remaining, &mut archive_budget,
                    |done| tick(prior.saturating_add(done), total),
                )?;
                limited |= result.limited;
                remaining -= result.hits.len();
                for hit in result.hits {
                    add(Kind::Ogg, hit.start, hit.size, hit.header, hit.footer, FragmentLayout::Ogg(hit.layout), hit.digest);
                }
            }
            if options.flac_fragments && remaining > 0 {
                let prior = size.saturating_mul(1 + u64::from(options.zip_fragments) + u64::from(options.ogg_fragments));
                let result = flac::fragments::search(
                    reader, range.clone(), allocation, alignment, remaining, &mut archive_budget,
                    |done| tick(prior.saturating_add(done), total),
                )?;
                limited |= result.limited;
                remaining -= result.hits.len();
                for hit in result.hits {
                    add(Kind::Flac, hit.start, hit.size, hit.header, hit.footer, FragmentLayout::Flac(hit.layout), hit.digest);
                }
            }
            if options.png_fragments && remaining > 0 {
                let prior = size.saturating_mul(1 + u64::from(options.zip_fragments)
                    + u64::from(options.ogg_fragments) + u64::from(options.flac_fragments));
                let result = png::fragments::search(
                    reader, range.clone(), allocation, alignment, remaining, &mut archive_budget,
                    |done| tick(prior.saturating_add(done), total),
                )?;
                limited |= result.limited;
                for hit in result.hits {
                    add(Kind::Png, hit.start, hit.size, png::MAGIC.to_vec(), png::IEND.to_vec(),
                        FragmentLayout::Png(hit.layout), hit.digest);
                }
            }
            tick(total, total)?;
            let rebuilt = candidates.len() as u32;
            for c in candidates {
                publish(c);
            }
            count += rebuilt;
            checkpoint.count = count;
            checkpoint.sealed = true;
            checkpoint.fragments_checked = true;
            return Ok(crate::native_locale::current().format(
                if (options.ogg_fragments || options.flac_fragments || options.png_fragments) && limited {
                    "签名扫描找到 {count} 个候选，其中 {rebuilt} 个由碎片重组。碎片检查达到索引、匹配或结果上限，请缩小范围。"
                } else if options.ogg_fragments || options.flac_fragments || options.png_fragments {
                    "签名扫描完成，找到 {count} 个候选，其中 {rebuilt} 个由碎片重组。无法唯一匹配并校验的碎片已跳过。"
                } else if limited {
                    "签名扫描找到 {count} 个候选，含 {rebuilt} 个重组的 ZIP 容器。碎片检查达到索引、匹配或结果上限，请缩小范围。"
                } else {
                    "签名扫描完成，找到 {count} 个候选，含 {rebuilt} 个重组的 ZIP 容器。无法唯一匹配并校验的碎片已跳过。"
                },
                &[("count", &count.to_string()), ("rebuilt", &rebuilt.to_string())],
            ));
        }
        tick(size, total)?;
        Ok(complete_message(count, rejected))
    })();
    // Interrupted probes still consume their resource budget, even when their
    // current chunk will be retried after an explicit resume.
    checkpoint.probes = probes;
    checkpoint.archive_remaining = archive_budget.remaining();
    result
}

fn complete_message(count: u32, rejected: usize) -> String {
    crate::native_locale::current().format(
        "签名扫描完成，找到 {count} 个候选；忽略 {rejected} 个不完整、超限或不支持结构的候选。单文件上限 256 MiB，结构最多 65536 个字段；不重组碎片，不保证可解码。",
        &[("count", &count.to_string()), ("rejected", &rejected.to_string())],
    )
}

#[derive(Debug)]
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
        let mut bytes = [0; N];
        self.read_at(offset, &mut bytes)?;
        Ok(bytes)
    }

    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> PResult<()> {
        self.fields += 1;
        if self.fields > MAX_FIELDS
            || offset
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > self.limit)
        {
            return Err(ProbeError::Invalid);
        }
        if self.fields % 256 == 1 {
            (self.tick)().map_err(ProbeError::Failed)?;
        }
        self.reader
            .seek(SeekFrom::Start(
                self.start.checked_add(offset).ok_or(ProbeError::Invalid)?,
            ))
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        self.reader
            .read_exact(bytes)
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        Ok(())
    }
    fn bounded(&self, end: u64) -> PResult<u64> {
        if end > self.limit {
            Err(ProbeError::Invalid)
        } else {
            Ok(end)
        }
    }

    fn seven_zip(
        &mut self,
        header: &[u8; 32],
        budget: &mut crate::signature_zip::Budget,
    ) -> PResult<u64> {
        let geometry = seven_zip_geometry(header).ok_or(ProbeError::Invalid)?;
        self.bounded(geometry.end)?;
        if geometry.next == geometry.end {
            return Ok(geometry.end);
        }
        budget
            .charge(geometry.end - geometry.next)
            .map_err(ProbeError::Failed)?;
        if !matches!(self.bytes::<1>(geometry.next)?[0], 0x01 | 0x17) {
            return Err(ProbeError::Invalid);
        }
        let mut crc = crc32fast::Hasher::new();
        let mut at = geometry.next;
        let mut buffer = vec![0; 64 * 1024];
        while at < geometry.end {
            (self.tick)().map_err(ProbeError::Failed)?;
            let take = (geometry.end - at).min(buffer.len() as u64) as usize;
            self.read_at(at, &mut buffer[..take])?;
            crc.update(&buffer[..take]);
            at += take as u64;
        }
        if crc.finalize() != geometry.crc {
            return Err(ProbeError::Invalid);
        }
        Ok(geometry.end)
    }

    fn sqlite(
        &mut self,
        header: &[u8; 100],
        budget: &mut crate::signature_zip::Budget,
    ) -> PResult<u64> {
        let geometry = sqlite_geometry(header).ok_or(ProbeError::Invalid)?;
        self.bounded(geometry.size())?;
        budget
            .charge(geometry.page as u64)
            .map_err(ProbeError::Failed)?;
        let mut page = vec![0; geometry.page - 100];
        self.read_at(100, &mut page)?;
        if !sqlite_root_page(&page, header, geometry) {
            return Err(ProbeError::Invalid);
        }
        Ok(geometry.size())
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

struct SevenZipGeometry {
    next: u64,
    end: u64,
    crc: u32,
}

fn seven_zip_geometry(header: &[u8]) -> Option<SevenZipGeometry> {
    if header.len() != 32
        || !header.starts_with(SEVEN_ZIP_MAGIC)
        || header[6] != 0
        || header[7] > 4
        || crc32fast::hash(&header[12..]) != le32(&header[8..12])
    {
        return None;
    }
    let next = 32u64.checked_add(u64::from_le_bytes(header[12..20].try_into().unwrap()))?;
    let length = u64::from_le_bytes(header[20..28].try_into().unwrap());
    let crc = le32(&header[28..32]);
    if length > MAX_SEVEN_ZIP_HEADER || (length == 0 && (next != 32 || crc != 0)) {
        return None;
    }
    let end = next.checked_add(length).filter(|&end| end <= MAX_FILE)?;
    Some(SevenZipGeometry { next, end, crc })
}

#[derive(Clone, Copy)]
struct SqliteGeometry {
    page: usize,
    count: u32,
}

impl SqliteGeometry {
    fn size(self) -> u64 {
        self.page as u64 * u64::from(self.count)
    }
}

fn be16(bytes: &[u8]) -> usize {
    u16::from_be_bytes(bytes.try_into().unwrap()).into()
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

fn sqlite_geometry(header: &[u8]) -> Option<SqliteGeometry> {
    if header.len() != 100 || !header.starts_with(SQLITE_MAGIC) {
        return None;
    }
    let page = match be16(&header[16..18]) {
        1 => 65536,
        n if (512..=32768).contains(&n) && n.is_power_of_two() => n,
        _ => return None,
    };
    let count = be32(&header[28..32]);
    let geometry = SqliteGeometry { page, count };
    let free_trunk = be32(&header[32..36]);
    let free_count = be32(&header[36..40]);
    let schema = be32(&header[44..48]);
    let encoding = be32(&header[56..60]);
    if !(1..=2).contains(&header[18])
        || !(1..=2).contains(&header[19])
        || page - usize::from(header[20]) < 480
        || header[21..24] != [64, 32, 32]
        || count == 0
        || geometry.size() > MAX_FILE
        || header[24..28] != header[92..96]
        || free_count >= count
        || (free_count == 0 && free_trunk != 0)
        || (free_count != 0 && !(2..=count).contains(&free_trunk))
        || (schema == 0 && (count != 1 || encoding > 3))
        || (schema != 0 && (!(1..=4).contains(&schema) || !(1..=3).contains(&encoding)))
        || header[72..92].iter().any(|&byte| byte != 0)
    {
        return None;
    }
    Some(geometry)
}

fn sqlite_root_page(page: &[u8], header: &[u8; 100], geometry: SqliteGeometry) -> bool {
    // Page offsets still include the 100-byte database header omitted from page.
    let minimum_cell = match page[0] {
        0x05 if (2..=geometry.count).contains(&be32(&page[8..12])) => 5,
        0x0d => 2,
        _ => return false,
    };
    let usable = geometry.page - header[20] as usize;
    let cells = be16(&page[3..5]);
    let free = be16(&page[1..3]);
    let content_start = match be16(&page[5..7]) {
        0 => 65536,
        offset => offset,
    };
    let pointer_start = if minimum_cell == 5 { 12 } else { 8 };
    let pointer_end = pointer_start + cells * 2;
    if 100 + pointer_end > content_start
        || content_start > usable
        || page[7] > 60
        || (free != 0 && (free < content_start || free + 4 > usable))
        || (cells == 0 && (minimum_cell == 5 || free != 0 || content_start != usable))
        || (cells != 0 && be32(&header[44..48]) == 0)
    {
        return false;
    }
    let mut offsets = page[pointer_start..pointer_end]
        .chunks_exact(2)
        .map(be16)
        .collect::<Vec<_>>();
    offsets.sort_unstable();
    if offsets
        .windows(2)
        .any(|pair| pair[0] + minimum_cell > pair[1])
    {
        return false;
    }
    offsets.into_iter().all(|at| {
        at >= content_start
            && at + minimum_cell <= usable
            && (minimum_cell != 5 || (2..=geometry.count).contains(&be32(&page[at - 100..at - 96])))
    })
}

type Probed = (u64, Vec<u8>, Vec<u8>);
fn probe<R: Read + Seek>(
    reader: &mut R,
    kind: Kind,
    start: u64,
    end: u64,
    budget: &mut crate::signature_zip::Budget,
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
        let (length, header) = match kind {
            Kind::Gif => (p.gif()?, p.bytes::<13>(0)?.to_vec()),
            Kind::Png => return Ok((png::inspect(&mut p, budget)?, png::MAGIC.to_vec(), png::IEND.to_vec())),
            Kind::Bmp => (p.bmp()?, p.bytes::<54>(0)?.to_vec()),
            Kind::Wav | Kind::Webp => (p.riff(kind)?, p.bytes::<12>(0)?.to_vec()),
            Kind::SevenZip => {
                let header = p.bytes::<32>(0)?;
                (p.seven_zip(&header, budget)?, header.to_vec())
            }
            Kind::Sqlite => {
                let header = p.bytes::<100>(0)?;
                (p.sqlite(&header, budget)?, header.to_vec())
            }
            Kind::Mp4 | Kind::Mov => (
                media::movie(&mut p, kind, budget)?,
                p.bytes::<12>(0)?.to_vec(),
            ),
            Kind::Ogg => (media::ogg(&mut p, budget)?, p.bytes::<27>(0)?.to_vec()),
            Kind::Tiff => (images::tiff(&mut p, budget)?, p.bytes::<16>(0)?.to_vec()),
            Kind::Ico | Kind::Cur => (
                images::icon(&mut p, kind, budget)?,
                p.bytes::<6>(0)?.to_vec(),
            ),
            Kind::Rar => {
                let header = p.bytes::<8>(0)?;
                (rar::inspect(&mut p, &header, budget)?, header.to_vec())
            }
            Kind::Avi | Kind::Aiff | Kind::Aifc => {
                let header = p.bytes::<12>(0)?;
                (
                    audiovisual::inspect(&mut p, kind, &header, budget)?,
                    header.to_vec(),
                )
            }
            Kind::Flac => {
                let header = p.bytes::<42>(0)?;
                (flac::inspect(&mut p, &header, budget)?, header.to_vec())
            }
            Kind::Midi => {
                let header = p.bytes::<14>(0)?;
                (audio::midi(&mut p, &header, budget)?, header.to_vec())
            }
            Kind::Dsf => {
                let header = p.bytes::<28>(0)?;
                (audio::dsf(&mut p, &header, budget)?, header.to_vec())
            }
            Kind::Dff => {
                let header = p.bytes::<16>(0)?;
                (audio::dff(&mut p, &header, budget)?, header.to_vec())
            }
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
    const SAMPLE_7Z: &[u8] = include_bytes!("../../tests/fixtures/signature-sample.7z");
    const SAMPLE_SQLITE: &[u8] = include_bytes!("../../tests/fixtures/signature-sample.sqlite");
    const MOVIE_MP4: &[u8] = include_bytes!("../../tests/fixtures/signature-bmff-faststart.mp4");
    const MOVIE_MOV: &[u8] = include_bytes!("../../tests/fixtures/signature-bmff-tail.mov");
    const OGG_MUX: &[u8] = include_bytes!("../../tests/fixtures/signature-ogg-multiplex.ogg");
    const OGG_OPUS: &[u8] = include_bytes!("../../tests/fixtures/signature-ogg-opus.ogg");
    const CLASSIC_TIFF: &[u8] = include_bytes!("../../tests/fixtures/signature-tiff-pages.tif");
    const BIG_TIFF: &[u8] = include_bytes!("../../tests/fixtures/signature-bigtiff-tiles.tif");
    const ICON: &[u8] = include_bytes!("../../tests/fixtures/signature-icon-png.ico");
    const CURSOR: &[u8] = include_bytes!("../../tests/fixtures/signature-cursor-dib.cur");
    const RAR4: &[u8] = include_bytes!("../../tests/fixtures/signature-sample-rar4.rar");
    const RAR5: &[u8] = include_bytes!("../../tests/fixtures/signature-sample-rar5.rar");
    const AVI: &[u8] = include_bytes!("../../tests/fixtures/signature-sample.avi");
    const AIFF: &[u8] = include_bytes!("../../tests/fixtures/signature-sample.aiff");
    const AIFC: &[u8] = include_bytes!("../../tests/fixtures/signature-sample.aifc");
    const FLAC: &[u8] = include_bytes!("../../tests/fixtures/signature-audio.flac");
    const MIDI: &[u8] = include_bytes!("../../tests/fixtures/signature-audio.mid");
    const DSF: &[u8] = include_bytes!("../../tests/fixtures/signature-audio.dsf");
    const DFF: &[u8] = include_bytes!("../../tests/fixtures/signature-audio.dff");

    #[test]
    fn audio_signatures_resume_and_extract_independent_samples() {
        assert_resume_and_extract(
            [
                (CHUNK - 2, Kind::Flac, FLAC),
                (2 * CHUNK - 12, Kind::Dff, DFF),
                (2 * CHUNK + 4096, Kind::Dsf, DSF),
                (2 * CHUNK + 20 * 1024, Kind::Midi, MIDI),
            ],
            vec![Kind::Pdf, Kind::Flac, Kind::Dff, Kind::Dsf, Kind::Midi],
        );
    }

    #[test]
    fn audio_signatures_reject_inconsistent_headers_truncation_and_payload_boundaries() {
        for (kind, bytes) in [
            (Kind::Flac, FLAC),
            (Kind::Midi, MIDI),
            (Kind::Dsf, DSF),
            (Kind::Dff, DFF),
        ] {
            assert_eq!(collect(bytes, vec![kind]).len(), 1, "{kind:?}");
            assert!(
                collect(&bytes[..bytes.len() - 1], vec![kind]).is_empty(),
                "{kind:?}"
            );
        }
        let mut flac = FLAC.to_vec();
        flac[25] ^= 1; // The total sample count disagrees with the frames.
        assert!(collect(&flac, vec![Kind::Flac]).is_empty());
        let mut flac = FLAC.to_vec();
        *flac.last_mut().unwrap() ^= 1;
        assert!(collect(&flac, vec![Kind::Flac]).is_empty());
        let mut midi = MIDI.to_vec();
        let end_marker = midi.len() - 2;
        midi[end_marker] = 0x01;
        assert!(collect(&midi, vec![Kind::Midi]).is_empty());
        let mut dsf = DSF.to_vec();
        dsf[92 + 4000] = 0x80; // Padding of the left channel.
        assert!(collect(&dsf, vec![Kind::Dsf]).is_empty());
        let mut dsf = DSF.to_vec();
        dsf[20] ^= 1; // ID3 metadata offset.
        assert!(collect(&dsf, vec![Kind::Dsf]).is_empty());
        let mut dff = DFF.to_vec();
        let channel = dff.windows(4).position(|bytes| bytes == b"SRGT").unwrap();
        dff[channel..channel + 4].copy_from_slice(b"SLFT");
        assert!(collect(&dff, vec![Kind::Dff]).is_empty());
    }

    #[test]
    fn container_signatures_resume_and_extract_independent_samples() {
        assert_resume_and_extract(
            [
                (CHUNK - 6, Kind::Rar, RAR4),
                (2 * CHUNK - 7, Kind::Avi, AVI),
                (2 * CHUNK + 12 * 1024, Kind::Rar, RAR5),
                (2 * CHUNK + 16 * 1024, Kind::Aiff, AIFF),
                (2 * CHUNK + 20 * 1024, Kind::Aifc, AIFC),
            ],
            vec![Kind::Pdf, Kind::Rar, Kind::Avi, Kind::Aiff, Kind::Aifc],
        );
    }

    #[test]
    fn container_signatures_reject_truncation_and_invalid_structure() {
        for (kind, bytes) in [
            (Kind::Rar, RAR4),
            (Kind::Rar, RAR5),
            (Kind::Avi, AVI),
            (Kind::Aiff, AIFF),
            (Kind::Aifc, AIFC),
        ] {
            assert_eq!(collect(bytes, vec![kind]).len(), 1, "{kind:?}");
            assert!(collect(&bytes[..bytes.len() - 1], vec![kind]).is_empty());
        }
        for (source, header) in [(RAR4, 7), (RAR5, 8)] {
            let mut corrupt = source.to_vec();
            corrupt[header] ^= 1;
            assert!(collect(&corrupt, vec![Kind::Rar]).is_empty());
            let mut corrupt = source.to_vec();
            let last = corrupt.len() - 1;
            corrupt[last] ^= 1;
            assert!(collect(&corrupt, vec![Kind::Rar]).is_empty());
        }
        let mut avi = AVI.to_vec();
        let index = avi.windows(4).position(|window| window == b"idx1").unwrap();
        avi[index + 16] ^= 1; // First chunk pointer must address an observed packet.
        assert!(collect(&avi, vec![Kind::Avi]).is_empty());
        let mut multi = AVI.to_vec();
        multi.extend(b"RIFF\x04\0\0\0AVIX");
        assert!(collect(&multi, vec![Kind::Avi]).is_empty());
        let mut aiff = AIFF.to_vec();
        aiff[23] ^= 1; // Declared frame count no longer matches SSND.
        assert!(collect(&aiff, vec![Kind::Aiff]).is_empty());
        let mut aifc = AIFC.to_vec();
        aifc[20] ^= 1;
        assert!(collect(&aifc, vec![Kind::Aifc]).is_empty());
        assert!(collect(AIFF, vec![Kind::Aifc]).is_empty());
        assert!(collect(AIFC, vec![Kind::Aiff]).is_empty());
    }

    #[test]
    fn media_signatures_resume_and_extract_independent_samples() {
        assert_resume_and_extract(
            [
                (CHUNK - 5, Kind::Mp4, MOVIE_MP4),
                (2 * CHUNK - 4, Kind::Ogg, OGG_MUX),
                (2 * CHUNK + 24 * 1024, Kind::Mov, MOVIE_MOV),
                (2 * CHUNK + 32 * 1024, Kind::Ogg, OGG_OPUS),
            ],
            vec![Kind::Pdf, Kind::Mp4, Kind::Mov, Kind::Ogg],
        );
    }

    #[test]
    fn image_signatures_resume_and_extract_independent_samples() {
        assert_resume_and_extract(
            [
                (CHUNK - 3, Kind::Tiff, CLASSIC_TIFF),
                (2 * CHUNK - 7, Kind::Tiff, BIG_TIFF),
                (2 * CHUNK + 4096, Kind::Ico, ICON),
                (2 * CHUNK + 8192, Kind::Cur, CURSOR),
            ],
            vec![Kind::Pdf, Kind::Tiff, Kind::Ico, Kind::Cur],
        );
    }

    #[test]
    fn image_signatures_reject_cyclic_or_out_of_bounds_directories_and_bad_images() {
        for (kind, bytes) in [
            (Kind::Tiff, CLASSIC_TIFF),
            (Kind::Tiff, BIG_TIFF),
            (Kind::Ico, ICON),
            (Kind::Cur, CURSOR),
        ] {
            assert!(collect(&bytes[..bytes.len() - 1], vec![kind]).is_empty());
        }
        let mut cyclic = CLASSIC_TIFF.to_vec();
        let first = u32::from_le_bytes(cyclic[4..8].try_into().unwrap()) as usize;
        let entries = u16::from_le_bytes(cyclic[first..first + 2].try_into().unwrap()) as usize;
        let next = first + 2 + entries * 12;
        cyclic[next..next + 4].copy_from_slice(&(first as u32).to_le_bytes());
        assert!(collect(&cyclic, vec![Kind::Tiff]).is_empty());
        let mut overlap = CLASSIC_TIFF.to_vec();
        let strip_table = overlap[first + 2..next]
            .chunks_exact(12)
            .find(|entry| entry[..2] == 273u16.to_le_bytes())
            .unwrap();
        let strips = u32::from_le_bytes(strip_table[8..12].try_into().unwrap()) as usize;
        overlap[strips..strips + 4].copy_from_slice(&(first as u32).to_le_bytes());
        assert!(collect(&overlap, vec![Kind::Tiff]).is_empty());
        let mut png = ICON.to_vec();
        let embedded = u32::from_le_bytes(png[18..22].try_into().unwrap()) as usize;
        png[embedded + 29] ^= 1; // IHDR CRC
        assert!(collect(&png, vec![Kind::Ico]).is_empty());
        let mut outside = CURSOR.to_vec();
        outside[10..12].copy_from_slice(&16u16.to_le_bytes());
        assert!(collect(&outside, vec![Kind::Cur]).is_empty());
        assert!(collect(CURSOR, vec![Kind::Ico]).is_empty());
        assert!(collect(ICON, vec![Kind::Cur]).is_empty());
    }

    fn assert_resume_and_extract<const N: usize>(
        spots: [(usize, Kind, &[u8]); N],
        kinds: Vec<Kind>,
    ) {
        let mut source = vec![0; 3 * CHUNK];
        source[..5].copy_from_slice(b"%PDF-");
        for (start, _, bytes) in spots {
            source[start..start + bytes.len()].copy_from_slice(bytes);
        }
        let options = Options {
            kinds,
            ..Options::default()
        };
        let mut checkpoint = Checkpoint::default();
        let mut reader = Cursor::new(&source);
        let mut found = Vec::new();
        assert!(continue_scan(
            &mut reader,
            source.len() as u64,
            &options,
            &mut checkpoint,
            |read, _| if read < CHUNK as u64 {
                Ok(())
            } else {
                Err("pause".into())
            },
            |candidate| found.push(candidate)
        )
        .is_err());
        assert!(found.is_empty());
        let mut checkpoint: Checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        checkpoint.verify_cursor(&mut reader, &options).unwrap();
        continue_scan(
            &mut reader,
            source.len() as u64,
            &options,
            &mut checkpoint,
            |_, _| Ok(()),
            |candidate| found.push(candidate),
        )
        .unwrap();
        assert_eq!(
            found.len(),
            spots.len(),
            "{:?}",
            found.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        for (candidate, (start, kind, bytes)) in found.iter().zip(spots) {
            assert_eq!(
                candidate.name,
                format!("recovered_{start:016x}.{}", kind.extension())
            );
            assert_eq!(candidate.size, bytes.len() as u64);
            let stored = StoredCandidate::from_candidate(candidate).unwrap();
            let candidate = stored
                .candidate(candidate.id, 0..source.len() as u64, &options, None)
                .unwrap();
            let mut output = Vec::new();
            crate::recovery::extract(&mut reader, &candidate, &mut output, MAX_FILE, |_| Ok(()))
                .unwrap();
            assert_eq!(output, bytes);
            let mut changed = source.clone();
            changed[start + bytes.len() / 2] ^= 1;
            output.clear();
            assert!(crate::recovery::extract(
                &mut Cursor::new(changed),
                &candidate,
                &mut output,
                16,
                |_| Ok(())
            )
            .is_err());
            assert!(output.is_empty());
        }
        assert!(collect(MOVIE_MOV, vec![Kind::Mp4]).is_empty());
        assert!(collect(MOVIE_MP4, vec![Kind::Mov]).is_empty());
    }

    #[test]
    fn media_validation_rejects_bad_chunk_maps_and_ogg_page_damage() {
        for (kind, bytes) in [(Kind::Mp4, MOVIE_MP4), (Kind::Mov, MOVIE_MOV)] {
            assert!(collect(&bytes[..bytes.len() - 1], vec![kind]).is_empty());
            let mut corrupt = bytes.to_vec();
            let stco = bytes
                .windows(4)
                .position(|header| header == b"stco")
                .unwrap();
            corrupt[stco + 12..stco + 16].copy_from_slice(&1u32.to_be_bytes());
            assert!(collect(&corrupt, vec![kind]).is_empty());
            let mut corrupt = bytes.to_vec();
            let times = bytes
                .windows(4)
                .position(|header| header == b"stts")
                .unwrap();
            corrupt[times + 12] ^= 1;
            assert!(collect(&corrupt, vec![kind]).is_empty());
        }
        let mut corrupt = OGG_OPUS.to_vec();
        let mid = corrupt.len() / 2;
        corrupt[mid] ^= 1;
        assert!(collect(&corrupt, vec![Kind::Ogg]).is_empty());
        let page_start = OGG_OPUS[4..]
            .windows(4)
            .position(|header| header == b"OggS")
            .unwrap()
            + 4;
        let lacing =
            &OGG_OPUS[page_start + 27..page_start + 27 + usize::from(OGG_OPUS[page_start + 26])];
        let page_end =
            page_start + 27 + lacing.len() + lacing.iter().map(|&n| usize::from(n)).sum::<usize>();
        let mut repeat = OGG_OPUS[..page_end].to_vec();
        repeat.extend_from_slice(&OGG_OPUS[page_start..]);
        assert!(collect(&repeat, vec![Kind::Ogg]).is_empty());
    }

    #[test]
    fn seven_zip_and_sqlite_candidates_resume_across_the_chunk_and_recheck_full_content() {
        let mut input = vec![0; CHUNK * 3];
        let cases = [
            (CHUNK - 13, Kind::Sqlite, SAMPLE_SQLITE),
            (2 * CHUNK + 17, Kind::SevenZip, SAMPLE_7Z),
        ];
        for (at, _, sample) in cases {
            input[at..][..sample.len()].copy_from_slice(sample);
        }
        let options = Options {
            kinds: vec![Kind::SevenZip, Kind::Sqlite],
            ..Default::default()
        };
        let mut checkpoint = Checkpoint::default();
        let mut reader = Cursor::new(&input);
        let mut files = Vec::new();
        let paused = continue_scan(
            &mut reader,
            input.len() as u64,
            &options,
            &mut checkpoint,
            |done, _| {
                if done >= CHUNK as u64 {
                    Err("paused".into())
                } else {
                    Ok(())
                }
            },
            |c| files.push(c),
        );
        assert_eq!(paused.unwrap_err(), "paused");
        assert!(files.is_empty());
        let state = serde_json::to_vec(&checkpoint).unwrap();
        let mut checkpoint: Checkpoint = serde_json::from_slice(&state).unwrap();
        checkpoint.verify_cursor(&mut reader, &options).unwrap();
        continue_scan(
            &mut reader,
            input.len() as u64,
            &options,
            &mut checkpoint,
            |_, _| Ok(()),
            |c| files.push(c),
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        for ((at, kind, expected), c) in cases.into_iter().zip(files) {
            assert_eq!(c.name, format!("recovered_{at:016x}.{}", kind.extension()));
            let stored = StoredCandidate::from_candidate(&c).unwrap();
            let restored = stored
                .candidate(c.id, 0..input.len() as u64, &options, None)
                .unwrap();
            assert_eq!(restored.size, expected.len() as u64);
            let mut out = Vec::new();
            crate::recovery::extract(&mut reader, &restored, &mut out, MAX_FILE, |_| Ok(()))
                .unwrap();
            assert_eq!(out, expected);
            let mut changed = input.clone();
            changed[at + expected.len() / 2] ^= 1;
            out.clear();
            assert!(crate::recovery::extract(
                &mut Cursor::new(changed),
                &restored,
                &mut out,
                16,
                |_| Ok(())
            )
            .is_err());
            assert!(out.is_empty());
        }
    }

    #[test]
    fn seven_zip_and_sqlite_reject_bad_geometry_and_truncation() {
        for (kind, bytes, positions) in [
            (
                Kind::SevenZip,
                SAMPLE_7Z,
                vec![8, 12, 20, SAMPLE_7Z.len() - 1],
            ),
            (Kind::Sqlite, SAMPLE_SQLITE, vec![16, 21, 28, 92, 100, 104]),
        ] {
            assert!(collect(&bytes[..bytes.len() - 1], vec![kind]).is_empty());
            for i in positions {
                let mut corrupt = bytes.to_vec();
                corrupt[i] ^= 0x40;
                assert!(
                    collect(&corrupt, vec![kind]).is_empty(),
                    "{kind:?} offset {i}"
                );
            }
        }
        let mut empty = vec![0; 65536];
        empty[..16].copy_from_slice(SQLITE_MAGIC);
        empty[17] = 1;
        empty[18..24].copy_from_slice(&[1, 1, 0, 64, 32, 32]);
        empty[31] = 1;
        empty[100] = 0x0d;
        assert_eq!(collect(&empty, vec![Kind::Sqlite])[0].size, 65536);
    }

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
                        .candidate(c.id, 0..bytes.len() as u64, &options, None)
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
        let mut png_bytes = Vec::new();
        {
            let mut encoder = ::png::Encoder::new(&mut png_bytes, 2, 2);
            encoder.set_color(::png::ColorType::Grayscale);
            encoder.write_header().unwrap().write_image_data(&[1, 2, 3, 4]).unwrap();
        }
        let fixtures = [
            gif(),
            bmp(),
            wav(),
            webp(),
            b"%PDF-1.7\nx\n%%EOF".to_vec(),
            b"\xff\xd8\xff\xe0payload\xff\xd9".to_vec(),
            png_bytes,
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
    fn legacy_paused_png_cursor_checks_structure_before_accepting_the_real_iend() {
        let mut original = Vec::new();
        {
            let mut encoder = ::png::Encoder::new(&mut original, 1, 1);
            encoder.set_color(::png::ColorType::Grayscale);
            encoder.write_header().unwrap().write_image_data(&[42]).unwrap();
        }
        // A private ancillary chunk can contain an IEND-shaped byte sequence.
        // The old cursor has already processed the header before the pause.
        let mut source = original[..33].to_vec();
        let mut data = vec![0x40; CHUNK + 57];
        data[CHUNK..CHUNK + png::IEND.len()].copy_from_slice(png::IEND);
        source.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let begin = source.len();
        source.extend_from_slice(b"diSt");
        source.extend_from_slice(&data);
        source.extend_from_slice(&crc32fast::hash(&source[begin..]).to_be_bytes());
        source.extend_from_slice(&original[33..]);
        let size = source.len() as u64;
        let options = Options { kinds: vec![Kind::Png], ..Options::default() };
        let mut cursor = Checkpoint {
            consumed: CHUNK as u64,
            searched: CHUNK as u64 - 16,
            carry: source[CHUNK - 16..CHUNK].to_vec(),
            active: Some((Kind::Png, 0, png::MAGIC.to_vec())),
            ..Checkpoint::default()
        };
        let mut found = Vec::new();
        continue_scan(&mut Cursor::new(&source), size, &options, &mut cursor,
            |_, _| Ok(()), |c| found.push(c)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].size, size);
        assert!(matches!(found[0].locator, Locator::Signature { content_hash: Some(_), .. }));
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
            &mut crate::signature_zip::Budget::default(),
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
            &mut crate::signature_zip::Budget::default(),
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
#[test]
fn scoped_resume_rejects_changed_allocation_and_keeps_whole_aligned_candidates() {
    use crate::used_space::Builder;
    const SECTOR: u64 = 4096;
    let size = 4 * CHUNK as u64;
    let first = SECTOR;
    let first_end = 2 * CHUNK as u64;
    let second = 3 * CHUNK as u64;
    let mut builder = Builder::new(size, SECTOR).unwrap();
    builder.free(first, first_end - first).unwrap();
    builder.free(second, 2 * SECTOR).unwrap();
    let plan = builder.finish().unwrap();
    let mut bytes = vec![0; size as usize];
    let pdf = b"%PDF-1.4\nexample\n%%EOF";
    for at in [first, first + 121, first_end, second + SECTOR] {
        bytes[at as usize..][..pdf.len()].copy_from_slice(pdf);
    }
    // This candidate must end before the allocated gap. Its footer in the
    // next free span cannot rescue it or consume the valid second PDF.
    bytes[(first_end - SECTOR) as usize..][..5].copy_from_slice(b"%PDF-");
    bytes[second as usize..][..5].copy_from_slice(b"%%EOF");
    let options = Options {
        kinds: vec![Kind::Pdf],
        start: 3,
        free_only: true,
        sector_aligned: true,
        ..Default::default()
    };
    let mut reader = std::io::Cursor::new(&bytes);
    let mut checkpoint = Checkpoint::default();
    let mut candidates = Vec::new();
    let stopped = continue_scan_scoped(
        &mut reader,
        size,
        &options,
        &mut checkpoint,
        SECTOR,
        Some(&plan),
        |n, _| {
            if n >= CHUNK as u64 {
                Err("paused".into())
            } else {
                Ok(())
            }
        },
        |c| candidates.push(c),
    );
    assert_eq!(stopped.unwrap_err(), "paused");
    assert_eq!(candidates.len(), 1);
    let checkpoint_bytes = serde_json::to_vec(&checkpoint).unwrap();
    let mut checkpoint: Checkpoint = serde_json::from_slice(&checkpoint_bytes).unwrap();
    checkpoint.verify_cursor(&mut reader, &options).unwrap();
    let mut changed = Builder::new(size, SECTOR).unwrap();
    changed.free(second, 2 * SECTOR).unwrap();
    let changed = changed.finish().unwrap();
    assert!(checkpoint.verify_scope(&options, &changed).is_err());
    assert!(verify_free_candidates(&changed, &candidates).is_err());
    continue_scan_scoped(
        &mut reader,
        size,
        &options,
        &mut checkpoint,
        SECTOR,
        Some(&plan),
        |_, _| Ok(()),
        |c| candidates.push(c),
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].name, format!("recovered_{first:016x}.pdf"));
    assert_eq!(
        candidates[1].name,
        format!("recovered_{:016x}.pdf", second + SECTOR)
    );
    assert_eq!(checkpoint.consumed, size - options.start);
    assert!(checkpoint.sealed);
    verify_free_candidates(&plan, &candidates).unwrap();
}

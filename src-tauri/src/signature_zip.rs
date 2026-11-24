//! Read-only ZIP carving from an EOCD anchor. Offsets refer only to the supplied
//! bounded stream; entry names are never interpreted as host paths.
use crate::signature_scan::{Kind, MAX_FILE};
use flate2::{Decompress, FlushDecompress, Status};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    ops::Range,
};

const MAX_ENTRIES: u64 = 16_384;
const MAX_DIRECTORY: u64 = 8 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const MAX_XML: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_OFFICE_PACKAGE: u64 = 8 * 1024 * 1024;
const MAX_OFFICE_EXPANDED: u64 = 48 * 1024 * 1024;
const MAX_OFFICE_XML: u64 = 16 * 1024 * 1024;
const ODF_MANIFEST: &[u8] = b"META-INF/manifest.xml";
const ODF_MANIFEST_NS: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
const BLOCK: usize = 64 * 1024;
pub struct Budget {
    remaining: u64,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            remaining: 64 * 1024 * 1024 * 1024,
        }
    }
}
impl Budget {
    pub(crate) fn remaining(&self) -> u64 {
        self.remaining
    }
    pub(crate) fn resume(remaining: u64) -> std::result::Result<Self, String> {
        if remaining > 64 * 1024 * 1024 * 1024 {
            return Err("签名候选检查预算越界。".into());
        }
        Ok(Self { remaining })
    }
    pub(crate) fn charge(&mut self, n: u64) -> std::result::Result<(), String> {
        self.remaining = self.remaining.checked_sub(n).ok_or_else(|| {
            "签名候选检查达到 64 GiB 累计工作量上限。请缩小范围并开始新扫描。".to_owned()
        })?;
        Ok(())
    }
}
enum Error {
    Invalid,
    Failed(String),
}
type Result<T> = std::result::Result<T, Error>;
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}
fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
struct Probe<'a, R, T> {
    reader: &'a mut R,
    range: Range<u64>,
    budget: &'a mut Budget,
    tick: T,
}
impl<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>> Probe<'_, R, T> {
    fn read(&mut self, at: u64, n: usize) -> Result<Vec<u8>> {
        require(
            n <= BLOCK
                && at >= self.range.start
                && at
                    .checked_add(n as u64)
                    .is_some_and(|e| e <= self.range.end),
        )?;
        self.budget.charge(n as u64).map_err(Error::Failed)?;
        (self.tick)().map_err(Error::Failed)?;
        self.reader
            .seek(SeekFrom::Start(at))
            .map_err(|e| Error::Failed(e.to_string()))?;
        let mut b = vec![0; n];
        self.reader
            .read_exact(&mut b)
            .map_err(|e| Error::Failed(e.to_string()))?;
        Ok(b)
    }
}
pub struct Archive {
    pub start: u64,
    pub size: u64,
    pub kind: Kind,
    pub digest: [u8; 32],
    pub header: Vec<u8>,
    pub footer: Vec<u8>,
}

pub(crate) struct OfficePackage {
    pub kind: Kind,
    pub main: String,
    pub xml: BTreeMap<Vec<u8>, Vec<u8>>,
}

pub(crate) fn office_package(bytes: &[u8]) -> Option<OfficePackage> {
    if bytes.len() < 22 || bytes.len() as u64 > MAX_OFFICE_PACKAGE {
        return None;
    }
    // A complete preview must occupy the whole retained file. Reject ambiguous
    // end records even when they fit in a ZIP comment.
    let mut ends = (bytes.len().saturating_sub(22 + 65535)..=bytes.len() - 22).filter(|&at| {
        &bytes[at..at + 4] == b"PK\x05\x06"
            && at + 22 + usize::from(le16(bytes, at + 20)) == bytes.len()
    });
    let end_record = ends.next()?;
    if ends.next().is_some() {
        return None;
    }
    let mut reader = std::io::Cursor::new(bytes);
    let mut budget = Budget {
        remaining: 96 * 1024 * 1024,
    };
    let mut probe = Probe {
        reader: &mut reader,
        range: 0..bytes.len() as u64,
        budget: &mut budget,
        tick: || Ok(()),
    };
    let (archive, package) = inspect(&mut probe, end_record as u64, true).ok()?;
    if archive.start != 0 || archive.size != bytes.len() as u64 {
        return None;
    }
    package
}

#[derive(Debug)]
struct Entry {
    name: Vec<u8>,
    offset: u64,
    packed: u64,
    unpacked: u64,
    crc: u32,
    flags: u16,
    method: u16,
    version: u16,
    dos_time: u32,
}

// Validate all TLV fields; ZIP64 values appear only for corresponding sentinels.
fn zip64(extra: &[u8], required: [bool; 4]) -> Result<[Option<u64>; 4]> {
    let (mut at, mut seen, mut result) = (0, false, [None; 4]);
    while at < extra.len() {
        require(extra.len() - at >= 4)?;
        let id = le16(extra, at);
        let end = at + 4 + usize::from(le16(extra, at + 2));
        require(end <= extra.len())?;
        if id == 1 {
            require(!seen)?;
            seen = true;
            let mut pos = at + 4;
            for i in 0..4 {
                if required[i] {
                    let n = if i == 3 { 4 } else { 8 };
                    require(pos + n <= end)?;
                    result[i] = Some(if i == 3 {
                        le32(extra, pos).into()
                    } else {
                        le64(extra, pos)
                    });
                    pos += n;
                }
            }
            require(pos == end)?;
        }
        at = end;
    }
    require(
        required
            .iter()
            .zip(result.iter())
            .all(|(r, v)| !*r || v.is_some()),
    )?;
    Ok(result)
}

pub fn probe<R: Read + Seek>(
    reader: &mut R,
    eocd: u64,
    range: Range<u64>,
    budget: &mut Budget,
    tick: impl FnMut() -> std::result::Result<(), String>,
) -> std::result::Result<Option<Archive>, String> {
    let mut p = Probe {
        reader,
        range,
        budget,
        tick,
    };
    match inspect(&mut p, eocd, false) {
        Ok((archive, _)) => Ok(Some(archive)),
        Err(Error::Invalid) => Ok(None),
        Err(Error::Failed(e)) => Err(e),
    }
}
#[path = "signature_zip_fragments.rs"]
pub(crate) mod fragments;

struct Directory {
    cd_start: u64,
    cd_offset: u64,
    end: u64,
    entries: Vec<Entry>,
    names: BTreeSet<Vec<u8>>,
}

fn directory<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    eocd: u64,
    preview: bool,
) -> Result<Directory> {
    let h = p.read(eocd, 22)?;
    require(
        &h[..4] == b"PK\x05\x06"
            && le16(&h, 4) == 0
            && le16(&h, 6) == 0
            && le16(&h, 8) == le16(&h, 10),
    )?;
    let end = eocd
        .checked_add(22 + u64::from(le16(&h, 20)))
        .ok_or(Error::Invalid)?;
    require(end <= p.range.end)?;
    let (mut count, mut cd_size, mut cd_offset, mut cd_end) = (
        u64::from(le16(&h, 10)),
        u64::from(le32(&h, 12)),
        u64::from(le32(&h, 16)),
        eocd,
    );
    let sentinel = count == 0xffff || cd_size == 0xffffffff || cd_offset == 0xffffffff;
    let locator = if eocd - p.range.start >= 20 {
        Some(p.read(eocd - 20, 20)?)
    } else {
        None
    };
    let mut zip64_offset = None;
    if let Some(l) = locator.filter(|l| &l[..4] == b"PK\x06\x07") {
        require(eocd - p.range.start >= 76 && le32(&l, 4) == 0 && le32(&l, 16) == 1)?;
        let z = p.read(eocd - 76, 56)?;
        require(
            &z[..4] == b"PK\x06\x06"
                && le64(&z, 4) == 44
                && le16(&z, 14) == 45
                && le32(&z, 16) == 0
                && le32(&z, 20) == 0
                && le64(&z, 24) == le64(&z, 32),
        )?;
        let values = [le64(&z, 32), le64(&z, 40), le64(&z, 48)];
        for (short, long, max) in [
            (count, values[0], 0xffff),
            (cd_size, values[1], 0xffffffff),
            (cd_offset, values[2], 0xffffffff),
        ] {
            require(short == max || short == long)?;
        }
        [count, cd_size, cd_offset] = values;
        cd_end = eocd - 76;
        zip64_offset = Some(le64(&l, 8));
    } else {
        require(!sentinel)?;
    }
    require(
        count <= MAX_ENTRIES
            && cd_size <= MAX_DIRECTORY
            && cd_offset <= MAX_FILE
            && cd_size <= MAX_FILE,
    )?;
    let cd_start = cd_end.checked_sub(cd_size).ok_or(Error::Invalid)?;
    let length = cd_offset
        .checked_add(end - cd_start)
        .ok_or(Error::Invalid)?;
    require(cd_start >= p.range.start && length <= MAX_FILE)?;
    if let Some(at) = zip64_offset {
        require(cd_offset.checked_add(cd_size) == Some(at))?;
    }
    let mut at = cd_start;
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    let mut expanded = 0u64;
    for _ in 0..count {
        require(at.checked_add(46).is_some_and(|e| e <= cd_end))?;
        let c = p.read(at, 46)?;
        let (n, x, comment) = (
            usize::from(le16(&c, 28)),
            usize::from(le16(&c, 30)),
            usize::from(le16(&c, 32)),
        );
        let next = at
            .checked_add(46 + (n + x + comment) as u64)
            .ok_or(Error::Invalid)?;
        require(&c[..4] == b"PK\x01\x02" && n > 0 && n <= 4096 && next <= cd_end)?;
        let name = p.read(at + 46, n)?;
        let extra = p.read(at + 46 + n as u64, x)?;
        let required = [
            le32(&c, 24) == u32::MAX,
            le32(&c, 20) == u32::MAX,
            le32(&c, 42) == u32::MAX,
            le16(&c, 34) == u16::MAX,
        ];
        let z = zip64(&extra, required)?;
        let e = Entry {
            name,
            offset: z[2].unwrap_or(le32(&c, 42).into()),
            packed: z[1].unwrap_or(le32(&c, 20).into()),
            unpacked: z[0].unwrap_or(le32(&c, 24).into()),
            crc: le32(&c, 16),
            flags: le16(&c, 8),
            method: le16(&c, 10),
            version: le16(&c, 6),
            dos_time: le32(&c, 12),
        };
        require(
            z[3].unwrap_or(le16(&c, 34).into()) == 0
                && matches!(e.method, 0 | 8)
                && e.flags & !0x080e == 0
                && e.version <= 45
                && e.version >= if e.method == 8 { 20 } else { 10 }
                && (!required.iter().any(|v| *v) || e.version >= 45),
        )?;
        require(e.method != 0 || e.flags & 6 == 0)?;
        require(
            e.offset < cd_offset
                && e.packed <= MAX_FILE
                && e.unpacked <= MAX_EXPANDED
                && !e.name.contains(&0)
                && names.insert(e.name.clone()),
        )?;
        if e.flags & 0x800 != 0 {
            require(std::str::from_utf8(&e.name).is_ok())?;
        }
        expanded = expanded.checked_add(e.unpacked).ok_or(Error::Invalid)?;
        require(
            expanded
                <= if preview {
                    MAX_OFFICE_EXPANDED
                } else {
                    MAX_EXPANDED
                },
        )?;
        entries.push(e);
        at = next;
    }
    require(at == cd_end && (count != 0 || cd_offset == 0))?;
    entries.sort_by_key(|e| e.offset);
    if let Some(first) = entries.first() {
        require(first.offset == 0)?;
    }
    Ok(Directory {
        cd_start,
        cd_offset,
        end,
        entries,
        names,
    })
}

fn inspect<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    eocd: u64,
    preview: bool,
) -> Result<(Archive, Option<OfficePackage>)> {
    let directory = directory(p, eocd, preview)?;
    inspect_directory(p, directory, preview)
}

fn inspect_directory<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    directory: Directory,
    preview: bool,
) -> Result<(Archive, Option<OfficePackage>)> {
    let Directory {
        cd_start,
        cd_offset,
        end,
        entries,
        names,
    } = directory;
    let start = cd_start.checked_sub(cd_offset).ok_or(Error::Invalid)?;
    require(start >= p.range.start)?;
    p.range = start..end;
    let mut xml = BTreeMap::new();
    let mut preview_xml = 0;
    for (i, e) in entries.iter().enumerate() {
        let next = entries.get(i + 1).map_or(cd_start, |e| start + e.offset);
        let local = start + e.offset;
        let (data, x) = local_record(p, e, local, next)?;
        let odf_mime =
            i == 0 && e.name == b"mimetype" && e.method == 0 && x == 0 && e.unpacked <= 127;
        let metadata = e.name == b"[Content_Types].xml"
            || e.name == b"_rels/.rels"
            || e.name == ODF_MANIFEST
            || odf_mime;
        let preview_part = preview && (e.name.ends_with(b".xml") || e.name.ends_with(b".rels"));
        let capture = e.unpacked <= MAX_XML
            && (metadata || preview_part && preview_xml + e.unpacked <= MAX_OFFICE_XML);
        let bytes = content(p, e, data, capture)?;
        if capture {
            preview_xml += e.unpacked;
            xml.insert(e.name.clone(), bytes);
        }
    }
    let identity = match (office_identity(&xml, &names), odf_identity(&xml, &names)) {
        (office, None) => office,
        (None, odf) => odf,
        (Some(_), Some(_)) => None,
    };
    let kind = identity.as_ref().map_or(Kind::Zip, |(kind, _)| *kind);
    let package = if preview {
        identity.map(|(kind, main)| OfficePackage { kind, main, xml })
    } else {
        None
    };
    p.budget.charge(end - start).map_err(Error::Failed)?;
    let digest = hash_range(p.reader, start, end - start, &mut p.tick).map_err(Error::Failed)?;
    let header = p.read(start, usize::try_from((end - start).min(30)).unwrap())?;
    let footer = p.read(end - 16, 16)?;
    Ok((
        Archive {
            start,
            size: end - start,
            kind,
            digest,
            header,
            footer,
        },
        package,
    ))
}

fn local_record<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    e: &Entry,
    local: u64,
    next: u64,
) -> Result<(u64, usize)> {
    require(local.checked_add(30).is_some_and(|n| n <= next))?;
    let l = p.read(local, 30)?;
    require(
        &l[..4] == b"PK\x03\x04"
            && le16(&l, 4) == e.version
            && le16(&l, 6) == e.flags
            && le16(&l, 8) == e.method
            && le32(&l, 10) == e.dos_time,
    )?;
    let (n, x) = (usize::from(le16(&l, 26)), usize::from(le16(&l, 28)));
    let data = local
        .checked_add(30 + (n + x) as u64)
        .ok_or(Error::Invalid)?;
    require(n == e.name.len() && data <= next && p.read(local + 30, n)? == e.name)?;
    let extra = p.read(local + 30 + n as u64, x)?;
    let required = [
        le32(&l, 22) == u32::MAX,
        le32(&l, 18) == u32::MAX,
        false,
        false,
    ];
    require(required[0] == required[1] && (!required[0] || e.version >= 45))?;
    let z = zip64(&extra, required)?;
    let (unpacked, packed) = (
        z[0].unwrap_or(le32(&l, 22).into()),
        z[1].unwrap_or(le32(&l, 18).into()),
    );
    let descriptor = e.flags & 8 != 0;
    require(
        (unpacked == e.unpacked || descriptor && unpacked == 0)
            && (packed == e.packed || descriptor && packed == 0)
            && (le32(&l, 14) == e.crc || descriptor && le32(&l, 14) == 0),
    )?;
    let data_end = data.checked_add(e.packed).ok_or(Error::Invalid)?;
    require(data_end <= next)?;
    if descriptor {
        let width = if required[0] { 8 } else { 4 };
        let plain = 4 + width * 2;
        let len = usize::try_from(next - data_end).map_err(|_| Error::Invalid)?;
        require(len == plain || len == plain + 4)?;
        let d = p.read(data_end, len)?;
        let skip = if len == plain + 4 {
            require(&d[..4] == b"PK\x07\x08")?;
            4
        } else {
            0
        };
        let sizes = if width == 8 {
            (le64(&d, skip + 4), le64(&d, skip + 12))
        } else {
            (le32(&d, skip + 4).into(), le32(&d, skip + 8).into())
        };
        require(le32(&d, skip) == e.crc && sizes == (e.packed, e.unpacked))?;
    } else {
        require(data_end == next)?;
    }
    Ok((data, x))
}

fn content<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    e: &Entry,
    data: u64,
    capture: bool,
) -> Result<Vec<u8>> {
    let mut crc = crc32fast::Hasher::new();
    let (mut read, mut written) = (0u64, 0u64);
    let mut saved = Vec::new();
    let mut absorb = |b: &[u8], budget: &mut Budget| -> Result<()> {
        written += b.len() as u64;
        require(written <= e.unpacked)?;
        budget.charge(b.len() as u64).map_err(Error::Failed)?;
        crc.update(b);
        if capture {
            saved.extend_from_slice(b);
        }
        Ok(())
    };
    if e.method == 0 {
        require(e.packed == e.unpacked)?;
        while read < e.packed {
            let b = p.read(data + read, (e.packed - read).min(BLOCK as u64) as usize)?;
            read += b.len() as u64;
            absorb(&b, p.budget)?;
        }
    } else {
        let mut decoder = Decompress::new(false);
        let mut input = Vec::new();
        let mut pos = 0usize;
        let mut out = vec![0; BLOCK];
        loop {
            (p.tick)().map_err(Error::Failed)?;
            if pos == input.len() && read < e.packed {
                input = p.read(data + read, (e.packed - read).min(BLOCK as u64) as usize)?;
                read += input.len() as u64;
                pos = 0;
            }
            let before_in = decoder.total_in();
            let before_out = decoder.total_out();
            let status = decoder
                .decompress(&input[pos..], &mut out, FlushDecompress::None)
                .map_err(|_| Error::Invalid)?;
            let consumed = (decoder.total_in() - before_in) as usize;
            let produced = (decoder.total_out() - before_out) as usize;
            pos += consumed;
            absorb(&out[..produced], p.budget)?;
            if status == Status::StreamEnd {
                require(decoder.total_in() == e.packed)?;
                break;
            }
            require(consumed != 0 || produced != 0)?;
        }
    }
    require(written == e.unpacked && crc.finalize() == e.crc)?;
    Ok(saved)
}

pub fn hash_range<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    size: u64,
    mut tick: impl FnMut() -> std::result::Result<(), String>,
) -> std::result::Result<[u8; 32], String> {
    if size > MAX_FILE || start.checked_add(size).is_none() {
        return Err("签名候选范围无效。".into());
    }
    reader
        .seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut done = 0;
    let mut bytes = vec![0; BLOCK];
    while done < size {
        tick()?;
        let n = (size - done).min(BLOCK as u64) as usize;
        reader
            .read_exact(&mut bytes[..n])
            .map_err(|e| e.to_string())?;
        hash.update(&bytes[..n]);
        done += n as u64;
    }
    Ok(hash.finalize().into())
}

// Only package metadata is interpreted; no document rendering, URI fetching,
// entity expansion, macros or extraction of archive entry names to paths.
pub(crate) fn flat_xml(
    bytes: &[u8],
    namespace: &[u8],
    root: &[u8],
) -> Option<Vec<(Vec<u8>, BTreeMap<Vec<u8>, String>)>> {
    flat_xml_attributes(bytes, namespace, root, None)
}

fn flat_xml_attributes(
    bytes: &[u8],
    namespace: &[u8],
    root: &[u8],
    attribute_namespace: Option<&[u8]>,
) -> Option<Vec<(Vec<u8>, BTreeMap<Vec<u8>, String>)>> {
    use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
    std::str::from_utf8(bytes).ok()?;
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().check_end_names = true;
    let (mut depth, mut roots, mut events) = (0usize, 0usize, 0usize);
    let mut declaration = false;
    let mut rows = vec![];
    loop {
        events += 1;
        if events > 65536 {
            return None;
        }
        let (ns, event) = reader.read_resolved_event().ok()?;
        let bound = matches!(ns,ResolveResult::Bound(ref n) if n.as_ref().as_bytes()==namespace);
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if !bound
                    || depth > 1
                    || (depth == 0 && (roots != 0 || e.local_name().as_ref().as_bytes() != root))
                {
                    return None;
                }
                let mut attributes = BTreeMap::new();
                for a in e.attributes() {
                    let a = a.ok()?;
                    let (resolved, local) = reader.resolver().resolve_attribute(a.key);
                    let selected = match resolved {
                        ResolveResult::Unbound => attribute_namespace.is_none(),
                        ResolveResult::Bound(uri) => {
                            attribute_namespace == Some(uri.as_ref().as_bytes())
                        }
                        ResolveResult::Unknown(_) => return None,
                    };
                    if !selected {
                        continue;
                    }
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::default())
                        .ok()?
                        .into_owned();
                    if attributes
                        .insert(local.as_ref().as_bytes().to_vec(), value)
                        .is_some()
                    {
                        return None;
                    }
                }
                if depth == 0 {
                    roots += 1;
                } else {
                    rows.push((e.local_name().as_ref().as_bytes().to_vec(), attributes));
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            Event::Text(e) => {
                if !e.as_ref().as_bytes().iter().all(u8::is_ascii_whitespace) {
                    return None;
                }
            }
            Event::Decl(e) => {
                if declaration || roots != 0 || e.version().ok()?.as_ref() != "1.0" {
                    return None;
                }
                if let Some(encoding) = e.encoding() {
                    if !encoding.ok()?.eq_ignore_ascii_case("utf-8") {
                        return None;
                    }
                }
                declaration = true;
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => return (roots == 1 && depth == 0).then_some(rows),
            _ => return None,
        }
    }
}
pub(crate) fn part_name(name: &str) -> Option<&str> {
    let name = name.strip_prefix('/').unwrap_or(name);
    (!name.is_empty()
        && !name.contains(['\\', ':', '%', '?', '#', '\0'])
        && name
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".."))
    .then_some(name)
}
fn office_identity(
    xml: &BTreeMap<Vec<u8>, Vec<u8>>,
    names: &BTreeSet<Vec<u8>>,
) -> Option<(Kind, String)> {
    let types = flat_xml(
        xml.get(b"[Content_Types].xml".as_slice())?,
        b"http://schemas.openxmlformats.org/package/2006/content-types",
        b"Types",
    )?;
    let rels = flat_xml(
        xml.get(b"_rels/.rels".as_slice())?,
        b"http://schemas.openxmlformats.org/package/2006/relationships",
        b"Relationships",
    )?;
    let mut target = None;
    let mut ids = BTreeSet::new();
    for (tag, attrs) in &rels {
        if tag != b"Relationship" {
            return None;
        }
        if !ids.insert(attrs.get(b"Id".as_slice())?) {
            return None;
        }
        let ty = attrs.get(b"Type".as_slice())?;
        if ty
            == "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
            || ty == "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"
        {
            if target.is_some()
                || attrs
                    .get(b"TargetMode".as_slice())
                    .is_some_and(|v| v != "Internal")
            {
                return None;
            }
            target = Some(part_name(attrs.get(b"Target".as_slice())?)?);
        }
    }
    let target = target?;
    if !names.contains(target.as_bytes()) {
        return None;
    }
    let mut identified = None;
    let mut parts = BTreeSet::new();
    for (tag, attrs) in &types {
        if tag == b"Override" {
            let part = part_name(attrs.get(b"PartName".as_slice())?)?;
            if !parts.insert(part) {
                return None;
            }
            let ty = attrs.get(b"ContentType".as_slice())?;
            if part == target {
                identified = match ty.as_str() {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => Some(Kind::Docx),
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" => Some(Kind::Xlsx),
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml" => Some(Kind::Pptx),
                    "application/vnd.ms-word.document.macroEnabled.main+xml" => Some(Kind::Docm),
                    "application/vnd.ms-excel.sheet.macroEnabled.main+xml" => Some(Kind::Xlsm),
                    "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml" => Some(Kind::Pptm),
                    _ => None,
                };
            }
        } else if tag != b"Default" {
            return None;
        }
    }
    identified.map(|kind| (kind, target.to_owned()))
}

fn odf_identity(
    xml: &BTreeMap<Vec<u8>, Vec<u8>>,
    names: &BTreeSet<Vec<u8>>,
) -> Option<(Kind, String)> {
    let mime = xml.get(b"mimetype".as_slice())?;
    let kind = match mime.as_slice() {
        b"application/vnd.oasis.opendocument.text" => Kind::Odt,
        b"application/vnd.oasis.opendocument.spreadsheet" => Kind::Ods,
        b"application/vnd.oasis.opendocument.presentation" => Kind::Odp,
        _ => return None,
    };
    let manifest = flat_xml_attributes(
        xml.get(ODF_MANIFEST)?,
        ODF_MANIFEST_NS,
        b"manifest",
        Some(ODF_MANIFEST_NS),
    )?;
    let mut entries = BTreeSet::new();
    for (tag, attrs) in &manifest {
        if tag != b"file-entry" {
            return None;
        }
        let path = attrs.get(b"full-path".as_slice())?.as_bytes();
        let media_type = attrs.get(b"media-type".as_slice())?;
        if !entries.insert(path) {
            return None;
        }
        if path == b"/" {
            if media_type.as_bytes() != mime {
                return None;
            }
        } else if path == b"mimetype" || path == ODF_MANIFEST {
            return None;
        } else if !path.ends_with(b"/") && !names.contains(path) {
            return None;
        }
    }
    if !entries.contains(b"/".as_slice())
        || !entries.contains(b"content.xml".as_slice())
        || names.iter().any(|name| {
            name != b"mimetype"
                && !name.starts_with(b"META-INF/")
                && !name.ends_with(b"/")
                && !entries.contains(name.as_slice())
        })
    {
        return None;
    }
    Some((kind, "content.xml".to_owned()))
}

#[cfg(test)]
#[path = "signature_zip_tests.rs"]
mod tests;

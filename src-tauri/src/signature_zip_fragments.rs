//! A ZIP directory gives each local record's logical position and decompressed
//! CRC. Reassembly is possible when records survive individually and every
//! member has exactly one validated physical occurrence in the search scope.
use super::*;
use crate::signature_scan::fragment_io::{
    copy_verified, read_block, search_spans, valid_parts, Mapped, Part, SEARCH_BLOCK,
};
use serde::{Deserialize, Serialize};
use std::io::Write;

const MAX_MEMBERS: usize = 256;
const MAX_HEADERS: usize = 262_144;
const MAX_END_RECORDS: usize = 32_768;
const MAX_MATCH_ATTEMPTS: usize = 1_000_000;
const MAX_PER_MEMBER: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Layout {
    pub parts: Vec<Part>,
    eocd: u64,
}
impl Layout {
    pub fn valid(&self, start: u64, size: u64, range: &Range<u64>) -> bool {
        if !(2..=MAX_MEMBERS + 1).contains(&self.parts.len())
            || self.eocd.checked_add(22).is_none_or(|end| end > size)
            || self
                .parts
                .last()
                .is_none_or(|last| self.eocd < size.saturating_sub(last.length))
            || self
                .parts
                .windows(2)
                .all(|pair| pair[0].start.checked_add(pair[0].length) == Some(pair[1].start))
        {
            return false;
        }
        valid_parts(&self.parts, start, size, range)
    }
}

#[derive(Clone, Copy)]
struct Anchor {
    start: u64,
    end: u64,
}
type Headers = BTreeMap<[u8; 32], Vec<Anchor>>;
fn key(fields: &[u8], name: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(fields);
    hash.update(name);
    hash.finalize().into()
}
fn entry_key(e: &Entry) -> [u8; 32] {
    let mut fields = [0; 10];
    fields[0..2].copy_from_slice(&e.version.to_le_bytes());
    fields[2..4].copy_from_slice(&e.flags.to_le_bytes());
    fields[4..6].copy_from_slice(&e.method.to_le_bytes());
    fields[6..10].copy_from_slice(&e.dos_time.to_le_bytes());
    key(&fields, &e.name)
}
fn plausible_header<R: Read + Seek, T: FnMut() -> std::result::Result<(), String>>(
    p: &mut Probe<'_, R, T>,
    at: u64,
) -> Result<[u8; 32]> {
    let h = p.read(at, 30)?;
    let (version, flags, method, n) = (le16(&h, 4), le16(&h, 6), le16(&h, 8), le16(&h, 26));
    let min_version = if method == 8 { 20 } else { 10 };
    require(
        &h[..4] == b"PK\x03\x04"
            && matches!(method, 0 | 8)
            && flags & !0x080e == 0
            && (min_version..=45).contains(&version)
            && (1..=4096).contains(&n),
    )?;
    let name = p.read(at + 30, n.into())?;
    Ok(key(&h[4..14], &name))
}

fn assemble(
    p: &mut Probe<'_, impl Read + Seek, impl FnMut() -> std::result::Result<(), String>>,
    eocd: u64,
    directory: &Directory,
    headers: &Headers,
    alignment: u64,
    attempts: &mut usize,
) -> Result<Layout> {
    require((1..=MAX_MEMBERS).contains(&directory.entries.len()))?;
    let mut parts = Vec::with_capacity(directory.entries.len() + 1);
    for (i, entry) in directory.entries.iter().enumerate() {
        let next = directory
            .entries
            .get(i + 1)
            .map_or(directory.cd_offset, |e| e.offset);
        let length = next - entry.offset;
        require(length >= 30)?;
        let matches = headers.get(&entry_key(entry)).ok_or(Error::Invalid)?;
        require(matches.len() <= MAX_PER_MEMBER)?;
        let mut matched = None;
        for &anchor in matches {
            *attempts += 1;
            require(*attempts <= MAX_MATCH_ATTEMPTS)?;
            let Some(end) = anchor
                .start
                .checked_add(length)
                .filter(|&end| end <= anchor.end)
            else {
                continue;
            };
            if i == 0 && anchor.start % alignment != 0 {
                continue;
            }
            p.range = anchor.start..end;
            let proof = local_record(p, entry, anchor.start, end)
                .and_then(|(data, _)| content(p, entry, data, false));
            match proof {
                Ok(_) => {
                    require(matched.is_none())?;
                    matched = Some(Part {
                        start: anchor.start,
                        length,
                    });
                }
                Err(Error::Invalid) => {}
                Err(error) => return Err(error),
            }
        }
        parts.push(matched.ok_or(Error::Invalid)?);
    }
    parts.push(Part {
        start: directory.cd_start,
        length: directory.end - directory.cd_start,
    });
    Ok(Layout {
        parts,
        eocd: directory.cd_offset + (eocd - directory.cd_start),
    })
}

pub(crate) struct Hit {
    pub archive: Archive,
    pub layout: Layout,
}
pub(crate) struct Search {
    pub hits: Vec<Hit>,
    pub limited: bool,
}

pub(crate) fn search(
    reader: &mut (impl Read + Seek),
    range: Range<u64>,
    free: Option<&crate::used_space::Plan>,
    alignment: u64,
    kinds: &[Kind],
    result_limit: usize,
    budget: &mut Budget,
    mut tick: impl FnMut(u64) -> std::result::Result<(), String>,
) -> std::result::Result<Search, String> {
    let mut headers = Headers::new();
    let mut ends = Vec::new();
    let mut header_count = 0;
    let mut buffer = vec![0; SEARCH_BLOCK + 3];
    let search_size = range.end - range.start;
    let spans = search_spans(range.clone(), free);
    for span in spans {
        let mut start = span.start;
        while start < span.end {
            tick(start - range.start)?;
            let (step, n) = read_block(reader, start, span.end, &mut buffer)?;
            let mut probe = Probe {
                reader,
                range: span.clone(),
                budget,
                tick: || tick(start - range.start),
            };
            for (i, _) in buffer[..step]
                .iter()
                .enumerate()
                .filter(|&(_, byte)| *byte == b'P')
            {
                let at = start + i as u64;
                let anchor = Anchor {
                    start: at,
                    end: span.end,
                };
                if buffer[i..n].starts_with(b"PK\x03\x04") {
                    match plausible_header(&mut probe, at) {
                        Ok(key) => {
                            if header_count == MAX_HEADERS {
                                return Ok(Search {
                                    hits: vec![],
                                    limited: true,
                                });
                            }
                            headers.entry(key).or_default().push(anchor);
                            header_count += 1;
                        }
                        Err(Error::Invalid) => {}
                        Err(Error::Failed(error)) => return Err(error),
                    }
                } else if buffer[i..n].starts_with(b"PK\x05\x06") && span.end - at >= 22 {
                    if ends.len() == MAX_END_RECORDS {
                        return Ok(Search {
                            hits: vec![],
                            limited: true,
                        });
                    }
                    ends.push((span.start, anchor));
                }
            }
            start += step as u64;
        }
    }
    let (mut hits, mut attempts) = (Vec::new(), 0);
    for (span_start, anchor) in ends {
        tick(search_size)?;
        let mut probe = Probe {
            reader,
            range: span_start..anchor.end,
            budget,
            tick: || tick(search_size),
        };
        let mut inspect = || {
            let directory = directory(&mut probe, anchor.start, false)?;
            let size = directory.cd_offset + (directory.end - directory.cd_start);
            // Ordinary contiguous hits were already checked by the first scan.
            // Still try to match this directory if its linear interpretation fails.
            let layout = assemble(
                &mut probe,
                anchor.start,
                &directory,
                &headers,
                alignment,
                &mut attempts,
            )?;
            let start = layout.parts[0].start;
            require(layout.valid(start, size, &range))?;
            let mut mapped = Mapped::new(probe.reader, &layout.parts);
            let mut archive =
                probe_archive(&mut mapped, &layout, size, probe.budget, &mut probe.tick)?;
            require(kinds.contains(&archive.kind))?;
            archive.start = start;
            Ok(Hit { archive, layout })
        };
        match inspect() {
            Ok(hit) => {
                hits.push(hit);
                if hits.len() == result_limit {
                    return Ok(Search {
                        hits,
                        limited: true,
                    });
                }
            }
            Err(Error::Invalid) if attempts > MAX_MATCH_ATTEMPTS => {
                return Ok(Search {
                    hits,
                    limited: true,
                })
            }
            Err(Error::Invalid) => {}
            Err(Error::Failed(error)) => return Err(error),
        }
    }
    Ok(Search {
        hits,
        limited: false,
    })
}

fn probe_archive(
    mapped: &mut Mapped<'_, impl Read + Seek>,
    layout: &Layout,
    size: u64,
    budget: &mut Budget,
    tick: impl FnMut() -> std::result::Result<(), String>,
) -> Result<Archive> {
    let mut p = Probe {
        reader: mapped,
        range: 0..size,
        budget,
        tick,
    };
    let d = directory(&mut p, layout.eocd, false)?;
    require(d.end == size && d.entries.len() + 1 == layout.parts.len())?;
    let mut logical = 0;
    for (entry, part) in d.entries.iter().zip(&layout.parts) {
        require(entry.offset == logical)?;
        logical += part.length;
    }
    require(logical == d.cd_offset)?;
    inspect_directory(&mut p, d, false).map(|(archive, _)| archive)
}

pub(crate) fn extract(
    reader: &mut (impl Read + Seek),
    layout: &Layout,
    start: u64,
    size: u64,
    expected: &[u8; 32],
    out: &mut impl Write,
    take: u64,
    mut tick: impl FnMut(u64) -> std::result::Result<(), String>,
) -> std::result::Result<u64, String> {
    let invalid = "ZIP 碎片布局或内容已变化，请重新扫描。";
    if size > MAX_FILE || !layout.valid(start, size, &(0..u64::MAX)) {
        return Err(invalid.into());
    }
    let mut mapped = Mapped::new(reader, &layout.parts);
    match probe_archive(&mut mapped, layout, size, &mut Budget::default(), || {
        tick(0)
    }) {
        Ok(archive) if &archive.digest == expected => {}
        Err(Error::Failed(error)) => return Err(error),
        _ => return Err(invalid.into()),
    }
    copy_verified(mapped, size, expected, out, take, invalid, tick)
}

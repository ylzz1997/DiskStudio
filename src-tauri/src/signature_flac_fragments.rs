//! Reconstruct complete native FLAC streams and verify their decoded PCM
//! against STREAMINFO. Partial or ambiguous paths never become candidates.
use super::*;
use crate::{
    signature_scan::{
        fragment_io::{
            copy_verified, read_block, search_spans, valid_parts, Mapped, Part, SEARCH_BLOCK,
        },
        MAX_FILE,
    },
    signature_zip::hash_range,
    used_space::Plan,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{SeekFrom, Write},
    ops::Range,
};

const MAX_ANCHORS: usize = 4096;
const MAX_INDEX: usize = 262_144;
const MAX_RUNS: usize = 4096;
const MAX_METADATA: u64 = 32 * 1024 * 1024;
const MAX_MATCHES: usize = 64;
const MAX_FORK_DEPTH: usize = 8;
const MAX_FORKS: usize = 64;
const MAX_ATTEMPTS: usize = 1_000_000;
const INVALID: &str = "FLAC 碎片布局或内容已变化，请重新扫描。";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Layout {
    pub(crate) flac_runs: Vec<Part>,
}
impl Layout {
    pub(crate) fn valid(&self, start: u64, size: u64, range: &Range<u64>) -> bool {
        (2..=MAX_RUNS).contains(&self.flac_runs.len())
            && self.flac_runs[0].length >= 42
            && valid_parts(&self.flac_runs, start, size, range)
    }
}

#[derive(Clone, Copy)]
struct Anchor {
    start: u64,
    end: u64,
}
type Index = BTreeMap<(bool, u64), Vec<Anchor>>;
fn frame_key(bytes: &[u8]) -> Option<(bool, u64)> {
    let h = bytes.get(..4)?;
    if h[0] != 0xff
        || h[1] & 0xfe != 0xf8
        || h[2] >> 4 == 0
        || h[2] & 15 == 15
        || h[3] >> 4 > 10
        || h[3] & 1 != 0
        || h[3] & 14 == 6
    {
        return None;
    }
    let mut number_end = 4;
    let number = utf_number(|| {
        let next = bytes.get(number_end).copied().ok_or(ProbeError::Invalid)?;
        number_end += 1;
        Ok(next)
    })
    .ok()?;
    let block_bytes = match h[2] >> 4 {
        6 => 1,
        7 => 2,
        _ => 0,
    };
    let rate_bytes = match h[2] & 15 {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    let crc_at = number_end + block_bytes + rate_bytes;
    let crc = bytes
        .get(..crc_at)?
        .iter()
        .fold(0u8, |crc, &byte| CRC8[(crc ^ byte) as usize]);
    (bytes.get(crc_at) == Some(&crc)).then_some((h[1] & 1 != 0, number))
}

fn check_seek(
    points: &[SeekPoint],
    cursor: &mut usize,
    sample: u64,
    offset: u64,
    block: u32,
) -> PResult<()> {
    if let Some(p) = points
        .get(*cursor)
        .filter(|p| p.sample < sample + u64::from(block))
    {
        require((p.sample, p.offset, p.block) == (sample, offset, block))?;
        *cursor += 1;
    }
    Ok(())
}

fn verify(
    mapped: &mut Mapped<'_, impl Read + Seek>,
    layout: &Layout,
    size: u64,
    budget: &mut Budget,
    mut tick: impl FnMut() -> Result<(), String>,
) -> PResult<[u8; 42]> {
    let mut probe = Probe {
        reader: mapped,
        start: 0,
        limit: size,
        fields: 0,
        tick: &mut tick,
    };
    let header = probe.bytes::<42>(0)?;
    let info = stream(&header).ok_or(ProbeError::Invalid)?;
    let expected = &header[26..42];
    require(expected.iter().any(|&byte| byte != 0))?;
    let mut points = Vec::new();
    let audio = metadata(&mut probe, &header, budget, Some(&mut points))?;
    require(audio <= MAX_METADATA && audio <= layout.flac_runs[0].length)?;
    let mut boundaries = layout
        .flac_runs
        .iter()
        .scan(0, |end, part| {
            *end += part.length;
            Some(*end)
        })
        .peekable();
    if boundaries.peek() == Some(&audio) {
        boundaries.next();
    }
    let mut bits = Bits::new(&mut probe, budget, audio);
    let mut pcm = decode::Pcm::default();
    let mut position = Position::default();
    let mut point = 0;
    while position.samples < info.samples {
        require(position.frames < MAX_FRAMES)?;
        if position.frames % 64 == 0 {
            (bits.probe.tick)().map_err(ProbeError::Failed)?;
        }
        let start = bits.offset;
        let decoded = frame(&mut bits, info, position, Some(&mut pcm))?;
        check_seek(
            &points,
            &mut point,
            position.samples,
            start - audio,
            decoded.block,
        )?;
        let boundary = boundaries.peek().copied().ok_or(ProbeError::Invalid)?;
        require(bits.offset <= boundary)?;
        if bits.offset == boundary {
            boundaries.next();
        }
        position.advance(decoded);
    }
    require(
        bits.offset == size
            && boundaries.next().is_none()
            && point == points.len()
            && pcm.finish().as_slice() == expected,
    )?;
    Ok(header)
}

pub(crate) struct Hit {
    pub start: u64,
    pub size: u64,
    pub header: Vec<u8>,
    pub footer: Vec<u8>,
    pub digest: [u8; 32],
    pub layout: Layout,
}
pub(crate) struct Search {
    pub hits: Vec<Hit>,
    pub limited: bool,
}

#[derive(Clone)]
struct Path {
    position: Position,
    runs: Vec<Part>,
    bytes: u64,
    point: usize,
}
impl Path {
    fn add(&mut self, part: Part, frame: Frame, point: usize) -> PResult<()> {
        self.bytes += part.length;
        require(self.bytes <= MAX_FILE)?;
        self.point = point;
        self.position.advance(frame);
        if let Some(run) = self
            .runs
            .last_mut()
            .filter(|run| run.start + run.length == part.start)
        {
            run.length += part.length;
        } else {
            require(self.runs.len() < MAX_RUNS)?;
            self.runs.push(part);
        }
        Ok(())
    }
}

struct Reconstruction<'a, R, T> {
    reader: &'a mut R,
    budget: &'a mut Budget,
    tick: T,
    range: &'a Range<u64>,
    index: &'a Index,
    info: Stream,
    points: &'a [SeekPoint],
    audio: u64,
    attempts: &'a mut usize,
    forks: usize,
    limited: bool,
    ambiguous: bool,
    verified: Option<Hit>,
}
impl<R: Read + Seek, T: FnMut() -> Result<(), String>> Reconstruction<'_, R, T> {
    fn matching_frames(&mut self, path: &Path) -> PResult<Vec<(Part, Frame, usize)>> {
        let position = path.position;
        let offset = path.bytes - self.audio;
        let mut matches = Vec::new();
        for (variable, number) in [(false, position.frames), (true, position.samples)] {
            if position.variable.is_some_and(|old| old != variable) {
                continue;
            }
            let Some(anchors) = self.index.get(&(variable, number)) else {
                continue;
            };
            if anchors.len() > MAX_MATCHES {
                self.limited = true;
                return Ok(vec![]);
            }
            for &source in anchors {
                *self.attempts += 1;
                if *self.attempts > MAX_ATTEMPTS {
                    self.limited = true;
                    return Ok(vec![]);
                }
                if path
                    .runs
                    .iter()
                    .any(|run| (run.start..run.start + run.length).contains(&source.start))
                {
                    continue;
                }
                let limit = (source.end - source.start).min(MAX_FILE - path.bytes);
                let mut probe = Probe {
                    reader: self.reader,
                    start: source.start,
                    limit,
                    fields: 0,
                    tick: &mut self.tick,
                };
                let mut bits = Bits::new(&mut probe, self.budget, 0);
                match frame(&mut bits, self.info, position, None) {
                    Ok(frame) => {
                        let mut point = path.point;
                        if check_seek(
                            self.points,
                            &mut point,
                            position.samples,
                            offset,
                            frame.block,
                        )
                        .is_ok()
                        {
                            let part = Part {
                                start: source.start,
                                length: bits.offset,
                            };
                            let end = part.start + part.length;
                            if !path
                                .runs
                                .iter()
                                .any(|run| run.start < end && run.start + run.length > part.start)
                            {
                                matches.push((part, frame, point));
                            }
                        }
                    }
                    Err(ProbeError::Invalid) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(matches)
    }

    fn walk(&mut self, mut path: Path, depth: usize) -> PResult<()> {
        while path.position.samples < self.info.samples {
            require(path.position.frames < MAX_FRAMES)?;
            let mut choices = self.matching_frames(&path)?;
            match choices.len() {
                0 => return Ok(()),
                1 => {
                    let (part, frame, point) = choices.pop().unwrap();
                    path.add(part, frame, point)?;
                }
                _ => {
                    self.forks += choices.len();
                    if depth == MAX_FORK_DEPTH || self.forks > MAX_FORKS {
                        self.limited = true;
                        return Ok(());
                    }
                    for (part, frame, point) in choices {
                        let mut branch = path.clone();
                        match branch
                            .add(part, frame, point)
                            .and_then(|_| self.walk(branch, depth + 1))
                        {
                            Ok(()) | Err(ProbeError::Invalid) => {}
                            Err(error) => return Err(error),
                        }
                        if self.ambiguous || self.limited {
                            break;
                        }
                    }
                    return Ok(());
                }
            }
        }
        if path.point != self.points.len() {
            return Ok(());
        }
        let layout = Layout {
            flac_runs: path.runs,
        };
        let start = layout.flac_runs[0].start;
        let size = path.bytes;
        require(layout.valid(start, size, self.range))?;
        let mut mapped = Mapped::new(self.reader, &layout.flac_runs);
        let header = verify(&mut mapped, &layout, size, self.budget, &mut self.tick)?;
        if self.verified.is_some() {
            self.ambiguous = true;
            return Ok(());
        }
        self.budget.charge(size).map_err(ProbeError::Failed)?;
        let digest =
            hash_range(&mut mapped, 0, size, &mut self.tick).map_err(ProbeError::Failed)?;
        let mut footer = vec![0; 16];
        mapped
            .seek(SeekFrom::End(-16))
            .and_then(|_| mapped.read_exact(&mut footer))
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        self.verified = Some(Hit {
            start,
            size,
            header: header.to_vec(),
            footer,
            digest,
            layout,
        });
        Ok(())
    }
}

pub(crate) fn search(
    reader: &mut (impl Read + Seek),
    range: Range<u64>,
    free: Option<&Plan>,
    alignment: u64,
    result_limit: usize,
    budget: &mut Budget,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Search, String> {
    let mut index = Index::new();
    let mut headers = Vec::new();
    let mut indexed = 0;
    let mut buffer = vec![0; SEARCH_BLOCK + 15];
    let size = range.end - range.start;
    for span in search_spans(range.clone(), free) {
        let mut start = span.start;
        while start < span.end {
            tick(start - range.start)?;
            let (step, n) = read_block(reader, start, span.end, &mut buffer)?;
            for i in 0..step {
                let anchor = Anchor {
                    start: start + i as u64,
                    end: span.end,
                };
                match buffer[i] {
                    b'f' if buffer[i..n].starts_with(b"fLaC") => {
                        if headers.len() == MAX_ANCHORS {
                            return Ok(Search {
                                hits: vec![],
                                limited: true,
                            });
                        }
                        headers.push(anchor);
                    }
                    0xff => {
                        if let Some(key) = frame_key(&buffer[i..n]) {
                            if indexed == MAX_INDEX {
                                return Ok(Search {
                                    hits: vec![],
                                    limited: true,
                                });
                            }
                            index.entry(key).or_default().push(anchor);
                            indexed += 1;
                        }
                    }
                    _ => {}
                }
            }
            start += step as u64;
        }
    }
    let mut intact: Vec<Range<u64>> = Vec::new();
    let mut origins = Vec::new();
    for anchor in headers {
        if intact.last().is_some_and(|r| r.contains(&anchor.start)) {
            continue;
        }
        let mut probe = Probe {
            reader,
            start: anchor.start,
            limit: (anchor.end - anchor.start).min(MAX_FILE),
            fields: 0,
            tick: || tick(size),
        };
        let classification = (|| {
            let header = probe.bytes::<42>(0)?;
            let info = stream(&header).ok_or(ProbeError::Invalid)?;
            match inspect(&mut probe, &header, budget) {
                Ok(length) => Ok(Some(length)),
                Err(ProbeError::Invalid)
                    if anchor.start % alignment == 0
                        && header[26..].iter().any(|&byte| byte != 0) =>
                {
                    let mut points = Vec::new();
                    probe.fields = 0;
                    let audio = metadata(&mut probe, &header, budget, Some(&mut points))?;
                    require(audio <= MAX_METADATA)?;
                    origins.push((anchor.start, info, points, audio));
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        })();
        match classification {
            Ok(Some(length)) => intact.push(anchor.start..anchor.start + length),
            Ok(None) | Err(ProbeError::Invalid) => {}
            Err(ProbeError::Failed(error)) => return Err(error),
        }
    }
    for matches in index.values_mut() {
        matches.retain(|anchor| {
            let i = intact.partition_point(|r| r.end <= anchor.start);
            !intact.get(i).is_some_and(|r| r.contains(&anchor.start))
        });
    }
    let mut hits = Vec::new();
    let mut attempts = 0;
    let mut limited = false;
    for (start, info, points, audio) in origins {
        tick(size)?;
        let mut build = Reconstruction {
            reader,
            budget,
            tick: || tick(size),
            range: &range,
            index: &index,
            info,
            points: &points,
            audio,
            attempts: &mut attempts,
            forks: 0,
            limited: false,
            ambiguous: false,
            verified: None,
        };
        let path = Path {
            position: Position::default(),
            runs: vec![Part {
                start,
                length: audio,
            }],
            bytes: audio,
            point: 0,
        };
        if let Err(ProbeError::Failed(error)) = build.walk(path, 0) {
            return Err(error);
        }
        limited |= build.limited;
        if !build.limited && !build.ambiguous {
            hits.extend(build.verified);
            if hits.len() == result_limit {
                return Ok(Search {
                    hits,
                    limited: true,
                });
            }
        }
        if attempts > MAX_ATTEMPTS {
            break;
        }
    }
    Ok(Search { hits, limited })
}

pub(crate) fn extract(
    reader: &mut (impl Read + Seek),
    layout: &Layout,
    start: u64,
    size: u64,
    expected: &[u8; 32],
    out: &mut impl Write,
    take: u64,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<u64, String> {
    if size > MAX_FILE || !layout.valid(start, size, &(0..u64::MAX)) {
        return Err(INVALID.into());
    }
    let mut mapped = Mapped::new(reader, &layout.flac_runs);
    match verify(&mut mapped, layout, size, &mut Budget::default(), || {
        tick(0)
    }) {
        Ok(_) => {}
        Err(ProbeError::Invalid) => return Err(INVALID.into()),
        Err(ProbeError::Failed(error)) => return Err(error),
    }
    if hash_range(&mut mapped, 0, size, || tick(0))? != *expected {
        return Err(INVALID.into());
    }
    copy_verified(mapped, size, expected, out, take, INVALID, tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recovery::{extract, Candidate, Locator},
        signature_scan::{self, Checkpoint, Kind, Options, StoredCandidate},
    };
    use md5::{Digest, Md5};
    use std::io::Cursor;

    const ORIGINAL: &[u8] = include_bytes!("../../tests/fixtures/signature-audio.flac");
    // Packet positions and lengths from ffprobe, independent of our parser.
    const EDGES: [usize; 12] = [
        98, 1700, 4038, 6111, 7891, 9930, 12111, 14088, 15999, 18131, 19892, 20745,
    ];

    fn options() -> Options {
        Options {
            kinds: vec![Kind::Flac],
            flac_fragments: true,
            ..Options::default()
        }
    }
    fn scan(image: &[u8], options: &Options, plan: Option<&Plan>) -> Vec<Candidate> {
        let mut files = vec![];
        signature_scan::continue_scan_scoped(
            &mut Cursor::new(image),
            image.len() as u64,
            options,
            &mut Checkpoint::default(),
            4096,
            plan,
            |_, _| Ok(()),
            |c| files.push(c),
        )
        .unwrap();
        files
    }
    fn readback(image: &[u8], candidate: &Candidate) -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        extract(
            &mut Cursor::new(image),
            candidate,
            &mut output,
            u64::MAX,
            |_| Ok(()),
        )?;
        Ok(output)
    }
    #[test]
    fn reconstruct_independent_24_bit_flac_and_recheck_decoded_md5() {
        let mut image = vec![0x3d; 2 << 20];
        let start = (1 << 20) - 2;
        image[start..start + EDGES[0]].copy_from_slice(&ORIGINAL[..EDGES[0]]);
        for (i, edge) in EDGES.windows(2).enumerate() {
            let at = 4096 + (EDGES.len() - i) * 8192;
            image[at..at + edge[1] - edge[0]].copy_from_slice(&ORIGINAL[edge[0]..edge[1]]);
        }
        let files = scan(&image, &options(), None);
        assert_eq!(files.len(), 1);
        let serialized =
            serde_json::to_vec(&StoredCandidate::from_candidate(&files[0]).unwrap()).unwrap();
        let record: StoredCandidate = serde_json::from_slice(&serialized).unwrap();
        let returned = record
            .candidate(0, 0..image.len() as u64, &options(), None)
            .unwrap();
        assert!(record.fragmented());
        assert_eq!(readback(&image, &returned).unwrap(), ORIGINAL);
        image[start + 26] ^= 1;
        assert!(readback(&image, &returned).is_err());
        assert!(scan(&image, &options(), None).is_empty());
    }

    fn constant_frame(variable: bool, number: u8, value: i16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xf8 | u8::from(variable), 0x69, 8, number, 15];
        let crc = bytes.iter().fold(0, |crc, &b| CRC8[(crc ^ b) as usize]);
        bytes.extend([crc, 0]); // Header CRC and constant subframe.
        bytes.extend(value.to_be_bytes());
        let crc = bytes.iter().fold(0u16, |crc, &b| {
            (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ b) as usize]
        });
        bytes.extend(crc.to_be_bytes());
        bytes
    }
    fn header(seek_table: bool) -> Vec<u8> {
        let mut header = b"fLaC".to_vec();
        header.extend([if seek_table { 0 } else { 0x80 }, 0, 0, 34]);
        header.extend([0, 16, 0, 16, 0, 0, 0, 0, 0, 0]);
        let fields = (44100u64 << 44) | (15 << 36) | 32;
        header.extend(fields.to_be_bytes());
        let mut md5 = Md5::new();
        for value in [-333i16, 357] {
            for _ in 0..16 {
                md5.update(value.to_le_bytes());
            }
        }
        header.extend(md5.finalize());
        if seek_table {
            header.extend([0x83, 0, 0, 18]);
            header.extend(16u64.to_be_bytes());
            header.extend(12u64.to_be_bytes());
            header.extend(16u16.to_be_bytes());
        }
        header
    }
    #[test]
    fn md5_resolves_plausible_alternatives_but_refuses_two_valid_maps() {
        for variable in [false, true] {
            let prefix = header(true);
            let first = constant_frame(variable, 0, -333);
            let number = if variable { 16 } else { 1 };
            let second = constant_frame(variable, number, 357);
            let misleading = constant_frame(variable, number, 308);
            let mut image = vec![0; 8 * 4096];
            for (start, bytes) in [
                (4096, &prefix),
                (8193, &first),
                (16384, &second),
                (20480, &misleading),
            ] {
                image[start..start + bytes.len()].copy_from_slice(bytes);
            }
            let files = scan(&image, &options(), None);
            assert_eq!(files.len(), 1);
            let bytes = [&prefix[..], &first, &second].concat();
            assert_eq!(readback(&image, &files[0]).unwrap(), bytes);
            image[20480..20480 + second.len()].copy_from_slice(&second);
            assert!(scan(&image, &options(), None).is_empty());
            let contiguous = scan(&bytes, &options(), None);
            assert_eq!(contiguous.len(), 1);
            assert!(!StoredCandidate::from_candidate(&contiguous[0])
                .unwrap()
                .fragmented());
        }
    }

    #[test]
    fn requires_every_part_in_free_space_and_rejects_absent_md5() {
        let size = 8 * 4096;
        let mut image = vec![0; size];
        let prefix = header(false);
        let first = constant_frame(false, 0, -333);
        let second = constant_frame(false, 1, 357);
        for (start, bytes) in [(4096, &prefix), (3 * 4096 + 3, &first), (6 * 4096, &second)] {
            image[start..start + bytes.len()].copy_from_slice(bytes);
        }
        let plan = |middle| {
            let mut builder = crate::used_space::Builder::new(size as u64, 4096).unwrap();
            builder.free(4096, 4096).unwrap();
            if middle {
                builder.free(3 * 4096, 4096).unwrap();
            }
            builder.free(6 * 4096, 4096).unwrap();
            builder.finish().unwrap()
        };
        let full = plan(true);
        let partial = plan(false);
        let options = Options {
            free_only: true,
            sector_aligned: true,
            ..options()
        };
        let mut files = scan(&image, &options, Some(&full));
        assert_eq!(files.len(), 1);
        signature_scan::verify_free_candidates(&full, &files).unwrap();
        if let Locator::Signature {
            allocation_digest, ..
        } = &mut files[0].locator
        {
            *allocation_digest = Some(partial.digest.clone());
        }
        assert!(signature_scan::verify_free_candidates(&partial, &files).is_err());
        assert!(scan(&image, &options, Some(&partial)).is_empty());
        image[4096 + 26..4096 + 42].fill(0);
        assert!(scan(&image, &options, Some(&full)).is_empty());
    }
}

//! The first IDAT run remains attached to the original PNG header. Additional
//! runs are tried only at verified chunk boundaries, with the original
//! zlib state carried across each gap.
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
    io::{SeekFrom, Write},
    ops::Range,
};

const MAX_RUN_INDEX: usize = 8192;
const MAX_INDEX_CHUNKS: usize = 262_144;
const MAX_ORIGINS: usize = 4096;
const MAX_RUNS: usize = 8;
const MAX_TRIES: usize = 4096;
const MAX_SCAN_TRIES: usize = 131_072;
const MAX_TRAILER: u64 = 16 * 1024 * 1024;
const INVALID: &str = "PNG 碎片布局或内容已变化，请重新扫描。";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Layout {
    pub(crate) png_runs: Vec<Part>,
}
impl Layout {
    pub(crate) fn valid(&self, start: u64, size: u64, range: &Range<u64>) -> bool {
        (2..=MAX_RUNS).contains(&self.png_runs.len())
            && self.png_runs[0].length >= 45
            && valid_parts(&self.png_runs, start, size, range)
    }
}

#[derive(Clone, Copy)]
struct Anchor {
    start: u64,
    end: u64,
}
struct Run {
    span_end: u64,
    physical: Range<u64>,
    payloads: Vec<Range<u64>>,
}
impl Run {
    fn length(&self) -> u64 {
        self.physical.end - self.physical.start
    }
    fn inflate(
        &self,
        reader: &mut (impl Read + Seek),
        state: &mut ImageStream,
        budget: &mut Budget,
        mut tick: impl FnMut() -> Result<(), String>,
    ) -> PResult<()> {
        let mut buffer = vec![0; BLOCK];
        for payload in &self.payloads {
            let mut offset = payload.start;
            while offset < payload.end {
                tick().map_err(ProbeError::Failed)?;
                let n = (payload.end - offset).min(buffer.len() as u64) as usize;
                budget.charge(n as u64).map_err(ProbeError::Failed)?;
                reader
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| reader.read_exact(&mut buffer[..n]))
                    .map_err(|e| ProbeError::Failed(e.to_string()))?;
                state.feed(&buffer[..n], budget)?;
                offset += n as u64;
            }
        }
        Ok(())
    }
}

fn verify(
    mapped: &mut Mapped<'_, impl Read + Seek>,
    layout: &Layout,
    size: u64,
    budget: &mut Budget,
    tick: impl FnMut() -> Result<(), String>,
) -> PResult<()> {
    let mut probe = Probe {
        reader: mapped,
        start: 0,
        limit: size,
        fields: 0,
        tick,
    };
    let (info, mut at) = prefix(&mut probe, budget, true)?;
    require(at < layout.png_runs[0].length)?;
    let mut stream = ImageStream::new(info)?;
    let mut edges = layout
        .png_runs
        .iter()
        .scan(0, |end, part| {
            *end += part.length;
            Some(*end)
        })
        .peekable();
    loop {
        let chunk = chunk_header(&mut probe, at)?;
        if chunk.kind != *b"IDAT" {
            break;
        }
        let end = edges.peek().copied().ok_or(ProbeError::Invalid)?;
        require(chunk.end <= end)?;
        chunk_crc(&mut probe, budget, chunk, |bytes, budget| {
            stream.feed(bytes, budget)
        })?;
        at = chunk.end;
        if at == end {
            edges.next();
        }
    }
    // Every discontinuity must land between whole IDAT chunks. Ancillary
    // chunks and IEND stay on the same final physical run as the last IDAT.
    require(
        stream.ended
            && size - at <= MAX_TRAILER
            && edges.next() == Some(size)
            && edges.next().is_none()
            && at > size - layout.png_runs.last().unwrap().length
            && trailer(&mut probe, budget, at, true)? == size,
    )
}

pub(crate) struct Hit {
    pub start: u64,
    pub size: u64,
    pub digest: [u8; 32],
    pub layout: Layout,
}
pub(crate) struct Search {
    pub hits: Vec<Hit>,
    pub limited: bool,
}

struct Origin {
    head: u64,
    info: Image,
    first_run: usize,
}
struct Finder<'a, R, T> {
    reader: &'a mut R,
    budget: &'a mut Budget,
    tick: T,
    range: &'a Range<u64>,
    detached: &'a [usize],
    runs: &'a [Run],
    total_tries: &'a mut usize,
    tries: usize,
    limited: bool,
    ambiguous: bool,
    hit: Option<Hit>,
}
impl<R: Read + Seek, T: FnMut() -> Result<(), String>> Finder<'_, R, T> {
    fn finish(&mut self, parts: &[Part], last: &Run, size: u64) -> PResult<()> {
        let mut probe = Probe {
            reader: self.reader,
            start: last.physical.end,
            limit: MAX_TRAILER
                .min(last.span_end - last.physical.end)
                .min(MAX_FILE - size),
            fields: 0,
            tick: &mut self.tick,
        };
        let tail = trailer(&mut probe, self.budget, 0, true)?;
        let mut layout = Layout {
            png_runs: parts.to_vec(),
        };
        layout.png_runs.last_mut().unwrap().length += tail;
        let size = size + tail;
        let start = layout.png_runs[0].start;
        require(layout.valid(start, size, self.range))?;
        let mut mapped = Mapped::new(self.reader, &layout.png_runs);
        verify(&mut mapped, &layout, size, self.budget, &mut self.tick)?;
        if self.hit.is_some() {
            self.ambiguous = true;
        } else {
            self.budget.charge(size).map_err(ProbeError::Failed)?;
            let digest =
                hash_range(&mut mapped, 0, size, &mut self.tick).map_err(ProbeError::Failed)?;
            self.hit = Some(Hit {
                start,
                size,
                digest,
                layout,
            });
        }
        Ok(())
    }

    fn walk(&mut self, stream: &ImageStream, parts: &mut Vec<Part>, size: u64) -> PResult<()> {
        for &i in self.detached {
            let run = &self.runs[i];
            if size + run.length() > MAX_FILE
                || parts
                    .iter()
                    .any(|p| p.start < run.physical.end && run.physical.start < p.start + p.length)
            {
                continue;
            }
            self.tries += 1;
            *self.total_tries += 1;
            if self.tries > MAX_TRIES || *self.total_tries > MAX_SCAN_TRIES {
                self.limited = true;
                return Ok(());
            }
            let mut next = stream.clone();
            match run.inflate(self.reader, &mut next, self.budget, &mut self.tick) {
                Ok(()) => {}
                Err(ProbeError::Invalid) => continue,
                Err(error) => return Err(error),
            }
            parts.push(Part {
                start: run.physical.start,
                length: run.length(),
            });
            let next_size = size + run.length();
            let result = if next.ended {
                self.finish(parts, run, next_size)
            } else if parts.len() < MAX_RUNS {
                self.walk(&next, parts, next_size)
            } else {
                self.limited = true;
                Ok(())
            };
            parts.pop();
            match result {
                Ok(()) | Err(ProbeError::Invalid) => {}
                Err(error) => return Err(error),
            }
            if self.ambiguous || self.limited {
                break;
            }
        }
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
    let search_bytes = range.end - range.start;
    let mut buffer = vec![0; SEARCH_BLOCK + 7];
    let mut indexed = Vec::<Run>::new();
    let mut headers = Vec::<Anchor>::new();
    let mut chunks = 0;
    let mut inspected_headers = 0;
    for span in search_spans(range.clone(), free) {
        let mut start = span.start;
        let mut payload_end = start;
        while start < span.end {
            tick(start - range.start)?;
            let (step, n) = read_block(reader, start, span.end, &mut buffer)?;
            for i in 0..step {
                let at = start + i as u64;
                if at < payload_end {
                    continue;
                }
                if buffer[i..n].starts_with(MAGIC) {
                    if headers.len() == MAX_ORIGINS {
                        return Ok(Search {
                            hits: vec![],
                            limited: true,
                        });
                    }
                    headers.push(Anchor {
                        start: at,
                        end: span.end,
                    });
                }
                if buffer.get(i + 4..n).is_none_or(|b| !b.starts_with(b"IDAT")) {
                    continue;
                }
                inspected_headers += 1;
                if inspected_headers > MAX_INDEX_CHUNKS {
                    return Ok(Search {
                        hits: vec![],
                        limited: true,
                    });
                }
                let mut probe = Probe {
                    reader,
                    start: at,
                    limit: (span.end - at).min(MAX_FILE),
                    fields: 0,
                    tick: || tick(at - range.start),
                };
                let chunk = match checked_chunk(&mut probe, budget, 0) {
                    Ok(chunk) => chunk,
                    Err(ProbeError::Invalid) => continue,
                    Err(ProbeError::Failed(error)) => return Err(error),
                };
                payload_end = at + chunk.end;
                chunks += 1;
                if chunks > MAX_INDEX_CHUNKS {
                    return Ok(Search {
                        hits: vec![],
                        limited: true,
                    });
                }
                let data = at + chunk.data;
                let payload = data..data + chunk.length;
                if let Some(run) = indexed
                    .last_mut()
                    .filter(|run| run.span_end == span.end && run.physical.end == at)
                {
                    run.physical.end = payload_end;
                    run.payloads.push(payload);
                } else {
                    if indexed.len() == MAX_RUN_INDEX {
                        return Ok(Search {
                            hits: vec![],
                            limited: true,
                        });
                    }
                    indexed.push(Run {
                        span_end: span.end,
                        physical: at..payload_end,
                        payloads: vec![payload],
                    });
                }
            }
            start += step as u64;
        }
    }
    let mut origins = Vec::new();
    let mut intact = Vec::<Range<u64>>::new();
    for anchor in headers {
        if intact.last().is_some_and(|r| r.contains(&anchor.start)) {
            continue;
        }
        let mut probe = Probe {
            reader,
            start: anchor.start,
            limit: (anchor.end - anchor.start).min(MAX_FILE),
            fields: 0,
            tick: || tick(search_bytes),
        };
        // Even an intact APNG is excluded from the donateable IDAT index.
        match inspect(&mut probe, budget) {
            Ok(length) => {
                intact.push(anchor.start..anchor.start + length);
                continue;
            }
            Err(ProbeError::Invalid) => {}
            Err(ProbeError::Failed(error)) => return Err(error),
        }
        if anchor.start % alignment != 0 {
            continue;
        }
        probe.fields = 0;
        match prefix(&mut probe, budget, true) {
            Ok((info, idat)) => {
                if let Ok(i) =
                    indexed.binary_search_by_key(&(anchor.start + idat), |run| run.physical.start)
                {
                    origins.push(Origin {
                        head: anchor.start,
                        info,
                        first_run: i,
                    });
                }
            }
            Err(ProbeError::Invalid) => {}
            Err(ProbeError::Failed(error)) => return Err(error),
        }
    }
    let attached = origins
        .iter()
        .map(|origin| origin.first_run)
        .collect::<std::collections::BTreeSet<_>>();
    let detached = indexed
        .iter()
        .enumerate()
        .filter_map(|(i, run)| {
            let at = intact.partition_point(|r| r.end <= run.physical.start);
            (!attached.contains(&i)
                && !intact.get(at).is_some_and(|r| r.start < run.physical.end)
                && run.payloads.iter().any(|p| !p.is_empty()))
            .then_some(i)
        })
        .collect::<Vec<_>>();
    let mut hits = Vec::new();
    let mut total_tries = 0;
    let mut limited = false;
    for origin in origins {
        tick(search_bytes)?;
        let initial = &indexed[origin.first_run];
        let Ok(mut stream) = ImageStream::new(origin.info) else {
            continue;
        };
        match initial.inflate(reader, &mut stream, budget, || tick(search_bytes)) {
            Ok(()) if !stream.ended && initial.payloads.iter().any(|p| !p.is_empty()) => {}
            Ok(()) | Err(ProbeError::Invalid) => continue,
            Err(ProbeError::Failed(error)) => return Err(error),
        }
        let size = initial.physical.end - origin.head;
        if size >= MAX_FILE {
            continue;
        }
        let mut finder = Finder {
            reader,
            budget,
            tick: || tick(search_bytes),
            range: &range,
            detached: &detached,
            runs: &indexed,
            total_tries: &mut total_tries,
            tries: 0,
            limited: false,
            ambiguous: false,
            hit: None,
        };
        let mut parts = vec![Part {
            start: origin.head,
            length: size,
        }];
        if let Err(ProbeError::Failed(error)) = finder.walk(&stream, &mut parts, size) {
            return Err(error);
        }
        limited |= finder.limited;
        if !finder.ambiguous && !finder.limited {
            hits.extend(finder.hit);
            if hits.len() == result_limit {
                return Ok(Search {
                    hits,
                    limited: true,
                });
            }
        }
        if total_tries > MAX_SCAN_TRIES {
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
    let mut mapped = Mapped::new(reader, &layout.png_runs);
    match verify(&mut mapped, layout, size, &mut Budget::default(), || {
        tick(0)
    }) {
        Ok(()) => {}
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
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::{Cursor, Write};

    fn options() -> Options {
        Options {
            kinds: vec![Kind::Png],
            png_fragments: true,
            ..Options::default()
        }
    }
    fn scan(image: &[u8], options: &Options, plan: Option<&Plan>) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        signature_scan::continue_scan_scoped(
            &mut Cursor::new(image),
            image.len() as u64,
            options,
            &mut Checkpoint::default(),
            4096,
            plan,
            |_, _| Ok(()),
            |c| candidates.push(c),
        )
        .unwrap();
        candidates
    }
    fn chunk(kind: &[u8; 4], data: &[u8], bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = bytes.len();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&crc32fast::hash(&bytes[start..]).to_be_bytes());
    }
    fn parts(shallow: bool, adam7: bool, bad_adler: bool) -> [Vec<u8>; 3] {
        let (width, height) = (9u32, 10u32);
        let mut original = MAGIC.to_vec();
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[if shallow { 1 } else { 8 }, 0, 0, 0, u8::from(adam7)]);
        chunk(b"IHDR", &ihdr, &mut original);
        chunk(b"tEXt", b"Description\0Fragment fixture", &mut original);
        // Build zero-valued pass scanlines independently from production Rows.
        let mut image = Vec::new();
        if adam7 {
            for (columns, rows) in [(2, 2), (1, 2), (3, 1), (2, 3), (5, 2), (4, 5), (9, 5)] {
                let bytes = if shallow { (columns + 7) / 8 } else { columns };
                image.extend(vec![0; (1 + bytes) * rows]);
            }
        } else {
            for y in 0..height as u8 {
                image.push(0);
                image.extend((0..width as u8).map(|x| x * 17 ^ y * 13));
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&image).unwrap();
        let mut compressed = encoder.finish().unwrap();
        let last = compressed.len() - 1;
        if bad_adler {
            compressed[last] ^= 1;
        }
        chunk(b"IDAT", &compressed[..1], &mut original); // split the zlib header
        let mut middle = Vec::new();
        chunk(b"IDAT", &compressed[1..last], &mut middle); // split trailing Adler-32
        let mut end = Vec::new();
        chunk(b"IDAT", &compressed[last..], &mut end);
        chunk(b"tEXt", b"Author\0Fixture", &mut end);
        end.extend_from_slice(IEND);
        [original, middle, end]
    }
    fn place(parts: &[Vec<u8>; 3], addresses: [usize; 3], image: &mut [u8]) {
        for (start, bytes) in addresses.into_iter().zip(parts) {
            image[start..start + bytes.len()].copy_from_slice(bytes);
        }
    }
    #[test]
    fn reassemble_zlib_header_and_adler_across_nonadjacent_chunks() {
        for (shallow, adam7) in [(false, false), (false, true), (true, true)] {
            let parts = parts(shallow, adam7, false);
            let original = parts.concat();
            let mut decoder = ::png::Decoder::new(Cursor::new(&original))
                .read_info()
                .unwrap();
            let mut pixels = vec![0; decoder.output_buffer_size().unwrap()];
            decoder.next_frame(&mut pixels).unwrap();
            let mut image = vec![0x5e; 2 << 20];
            place(&parts, [(1 << 20) - 3, 18000, 4097], &mut image);
            let ordinary = Options {
                png_fragments: false,
                ..options()
            };
            assert!(scan(&image, &ordinary, None).is_empty());
            let with_fragments = scan(&image, &options(), None);
            assert_eq!(with_fragments.len(), 1);
            let saved = StoredCandidate::from_candidate(&with_fragments[0]).unwrap();
            assert!(saved.fragmented());
            let record = serde_json::to_vec(&saved).unwrap();
            let saved: StoredCandidate = serde_json::from_slice(&record).unwrap();
            let choice = saved
                .candidate(0, 0..image.len() as u64, &options(), None)
                .unwrap();
            let mut out = vec![];
            extract(
                &mut Cursor::new(&image),
                &choice,
                &mut out,
                MAX_FILE,
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(out, original);
            image[18000 + 10] ^= 1;
            out.clear();
            assert!(extract(&mut Cursor::new(&image), &choice, &mut out, 1, |_| Ok(())).is_err());
            assert!(out.is_empty());
            assert!(scan(&image, &options(), None).is_empty());
        }
    }
    #[test]
    fn refuse_duplicate_runs_bad_adler_and_unlinked_trailers() {
        let parts = parts(false, false, false);
        let mut image = vec![0; 8 * 4096];
        let positions = [4096, 8192, 12288];
        place(&parts, positions, &mut image);
        image[16384..16384 + parts[1].len()].copy_from_slice(&parts[1]);
        assert!(scan(&image, &options(), None).is_empty());
        image[16384..16384 + parts[1].len()].fill(0);
        let corrupt = self::parts(false, false, true);
        place(&corrupt, positions, &mut image);
        assert!(scan(&image, &options(), None).is_empty());
        place(&parts, positions, &mut image);
        image.copy_within(12288 + 13..12288 + parts[2].len(), 24576);
        image[12288 + 13..12288 + parts[2].len()].fill(0);
        assert!(scan(&image, &options(), None).is_empty());
        let contiguous = scan(&parts.concat(), &options(), None);
        assert_eq!(contiguous.len(), 1);
        assert!(!StoredCandidate::from_candidate(&contiguous[0])
            .unwrap()
            .fragmented());
    }
    #[test]
    fn check_free_space_and_keep_legacy_png_checkpoints_readable() {
        let parts = parts(false, false, false);
        let size = 8 * 4096;
        let mut image = vec![0; size];
        place(&parts, [4096, 3 * 4096 + 13, 6 * 4096], &mut image);
        let plan = |allow_middle| {
            let mut b = crate::used_space::Builder::new(size as u64, 4096).unwrap();
            b.free(4096, 4096).unwrap();
            if allow_middle {
                b.free(3 * 4096, 4096).unwrap();
            }
            b.free(6 * 4096, 4096).unwrap();
            b.finish().unwrap()
        };
        let all = plan(true);
        let partial = plan(false);
        let options = Options {
            free_only: true,
            sector_aligned: true,
            ..options()
        };
        let mut files = scan(&image, &options, Some(&all));
        assert_eq!(files.len(), 1);
        signature_scan::verify_free_candidates(&all, &files).unwrap();
        if let Locator::Signature {
            allocation_digest, ..
        } = &mut files[0].locator
        {
            *allocation_digest = Some(partial.digest.clone());
        }
        assert!(signature_scan::verify_free_candidates(&partial, &files).is_err());
        assert!(scan(&image, &options, Some(&partial)).is_empty());

        let ordinary = Options {
            free_only: false,
            png_fragments: false,
            ..options
        };
        let original = parts.concat();
        let candidate = &scan(&original, &ordinary, None)[0];
        let saved = StoredCandidate::from_candidate(candidate).unwrap();
        let mut old = serde_json::to_value(saved).unwrap();
        old["contentHash"] = serde_json::Value::Null;
        let record: StoredCandidate = serde_json::from_value(old).unwrap();
        assert!(record
            .candidate(0, 0..original.len() as u64, &ordinary, None)
            .unwrap()
            .note
            .contains("历史"));
    }

    #[test]
    fn zlib_stream_drains_multiple_output_buffers_and_rejects_excess_rows() {
        let info = Image {
            width: 1023,
            height: 128,
            color: 0,
            depth: 8,
            channels: 1,
            interlaced: false,
        };
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&vec![0; 128 * 1024]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut stream = ImageStream::new(info).unwrap();
        let mut budget = Budget::default();
        for bytes in compressed.chunks(3) {
            assert!(stream.feed(bytes, &mut budget).is_ok());
        }
        assert!(stream.ended);
        let mut wrong = ImageStream::new(Image {
            height: 127,
            ..info
        })
        .unwrap();
        assert!(wrong.feed(&compressed, &mut budget).is_err());
    }
}

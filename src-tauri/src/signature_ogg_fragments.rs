//! Index intact Ogg pages by logical stream and sequence number. Pages of a
//! complete contiguous Ogg container are left to ordinary signature carving.
use super::{
    fragment_io::{
        copy_verified, read_block, search_spans, valid_parts, Mapped, Part, SEARCH_BLOCK,
    },
    media::{ogg_page, OggOrder, OggPage},
    Probe, ProbeError, MAX_FILE,
};
use crate::{
    signature_zip::{hash_range, Budget},
    used_space::Plan,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
};

const MAX_ANCHORS: usize = 1_000_000;
const MAX_PAGES: usize = 262_144;
const MAX_STREAM_PAGES: usize = 16_384;
const MAX_RUNS: usize = 4_096;
const INVALID: &str = "Ogg 碎片布局或内容已变化，请重新扫描。";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Layout {
    pub(crate) runs: Vec<Part>,
}
impl Layout {
    pub(crate) fn valid(&self, start: u64, size: u64, range: &Range<u64>) -> bool {
        (2..=MAX_RUNS).contains(&self.runs.len())
            && self.runs.iter().all(|run| run.length >= 27)
            && self
                .runs
                .windows(2)
                .all(|pair| pair[0].start.checked_add(pair[0].length) != Some(pair[1].start))
            && valid_parts(&self.runs, start, size, range)
    }
}

#[derive(Clone, Copy)]
struct PhysicalPage {
    at: u64,
    page: OggPage,
}

fn contiguous_containers(pages: &[PhysicalPage]) -> Vec<Range<u64>> {
    let mut containers = Vec::new();
    let mut active: Option<(u64, u64, OggOrder)> = None;
    for &PhysicalPage { at, page } in pages {
        if active.as_ref().is_some_and(|(_, next, _)| at < *next) {
            // An embedded capture pattern need not interrupt the containing page.
            continue;
        }
        if let Some((start, end, mut order)) = active.take().filter(|(_, next, _)| at == *next) {
            if end - start + page.length <= MAX_FILE {
                if let Ok(complete) = order.push(page) {
                    if complete {
                        containers.push(start..at + page.length);
                    } else {
                        active = Some((start, at + page.length, order));
                    }
                    continue;
                }
            }
        }
        if page.first {
            let mut order = OggOrder::default();
            if let Ok(complete) = order.push(page) {
                if complete {
                    containers.push(at..at + page.length);
                } else {
                    active = Some((at, at + page.length, order));
                }
            }
        }
    }
    containers
}

fn stream_parts(mut pages: Vec<PhysicalPage>) -> Option<(u64, Layout)> {
    if !(2..=MAX_STREAM_PAGES).contains(&pages.len()) {
        return None;
    }
    pages.sort_unstable_by_key(|located| located.page.sequence);
    let count = pages.len();
    let mut order = OggOrder::default();
    let mut size = 0;
    let mut runs = Vec::<Part>::new();
    for (i, PhysicalPage { at, page }) in pages.into_iter().enumerate() {
        // The whole serial is rejected on any duplicate, gap or extra page
        // after EOS, even if another subset could produce a playable file.
        if page.sequence as usize != i || order.push(page).ok()? != (i + 1 == count) {
            return None;
        }
        size += page.length;
        if size > MAX_FILE {
            return None;
        }
        if let Some(run) = runs.last_mut().filter(|run| run.start + run.length == at) {
            run.length += page.length;
        } else {
            runs.push(Part {
                start: at,
                length: page.length,
            });
        }
    }
    Some((size, Layout { runs }))
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

pub(crate) fn search(
    reader: &mut (impl Read + Seek),
    range: Range<u64>,
    free: Option<&Plan>,
    alignment: u64,
    result_limit: usize,
    budget: &mut Budget,
    mut tick: impl FnMut(u64) -> Result<(), String>,
) -> Result<Search, String> {
    let mut pages = Vec::new();
    let mut anchors = 0;
    let mut buffer = vec![0; SEARCH_BLOCK + 3];
    let mut body = Vec::new();
    let search_size = range.end - range.start;
    for span in search_spans(range.clone(), free) {
        let mut start = span.start;
        while start < span.end {
            tick(start - range.start)?;
            let (step, n) = read_block(reader, start, span.end, &mut buffer)?;
            for (i, _) in buffer[..step]
                .iter()
                .enumerate()
                .filter(|&(_, byte)| *byte == b'O')
            {
                if !buffer[i..n].starts_with(b"OggS") {
                    continue;
                }
                anchors += 1;
                if anchors > MAX_ANCHORS {
                    return Ok(Search {
                        hits: vec![],
                        limited: true,
                    });
                }
                let at = start + i as u64;
                let mut p = Probe {
                    reader,
                    start: at,
                    limit: span.end - at,
                    fields: 0,
                    tick: || tick(at - range.start),
                };
                match ogg_page(&mut p, budget, 0, &mut body) {
                    Ok(page) => {
                        if pages.len() == MAX_PAGES {
                            return Ok(Search {
                                hits: vec![],
                                limited: true,
                            });
                        }
                        pages.push(PhysicalPage { at, page });
                    }
                    Err(ProbeError::Invalid) => {}
                    Err(ProbeError::Failed(error)) => return Err(error),
                }
            }
            start += step as u64;
        }
    }
    let containers = contiguous_containers(&pages);
    let mut streams = BTreeMap::<u32, Vec<PhysicalPage>>::new();
    for page in pages {
        streams.entry(page.page.serial).or_default().push(page);
    }
    let mut hits = Vec::new();
    for pages in streams.into_values() {
        tick(search_size)?;
        let Some((size, layout)) = stream_parts(pages) else {
            continue;
        };
        let start = layout.runs[0].start;
        if start % alignment != 0
            || !layout.valid(start, size, &range)
            || layout.runs.iter().any(|run| {
                let i = containers.partition_point(|r| r.end <= run.start);
                containers
                    .get(i)
                    .is_some_and(|r| r.start < run.start + run.length)
            })
        {
            continue;
        }
        let mut check = || tick(search_size);
        match verify(reader, &layout, budget, &mut check) {
            Ok(()) => {}
            Err(ProbeError::Invalid) => continue,
            Err(ProbeError::Failed(error)) => return Err(error),
        }
        let mut mapped = Mapped::new(reader, &layout.runs);
        budget.charge(size)?;
        let digest = hash_range(&mut mapped, 0, size, &mut check)?;
        let mut header = vec![0; 27];
        let mut footer = vec![0; 16];
        mapped
            .rewind()
            .and_then(|_| mapped.read_exact(&mut header))
            .map_err(|e| e.to_string())?;
        mapped
            .seek(SeekFrom::End(-16))
            .and_then(|_| mapped.read_exact(&mut footer))
            .map_err(|e| e.to_string())?;
        hits.push(Hit {
            start,
            size,
            header,
            footer,
            digest,
            layout,
        });
        if hits.len() == result_limit {
            return Ok(Search {
                hits,
                limited: true,
            });
        }
    }
    hits.sort_unstable_by_key(|hit| hit.start);
    Ok(Search {
        hits,
        limited: false,
    })
}

fn verify(
    reader: &mut (impl Read + Seek),
    layout: &Layout,
    budget: &mut Budget,
    mut tick: impl FnMut() -> Result<(), String>,
) -> super::PResult<()> {
    let mut serial = None;
    let mut order = OggOrder::default();
    let mut body = Vec::new();
    let mut count = 0;
    for (i, run) in layout.runs.iter().enumerate() {
        let mut p = Probe {
            reader,
            start: run.start,
            limit: run.length,
            fields: 0,
            tick: &mut tick,
        };
        let mut offset = 0;
        while offset < run.length {
            let page = ogg_page(&mut p, budget, offset, &mut body)?;
            offset += page.length;
            count += 1;
            if *serial.get_or_insert(page.serial) != page.serial
                || count > MAX_STREAM_PAGES
                || order.push(page)? != (i + 1 == layout.runs.len() && offset == run.length)
            {
                return Err(ProbeError::Invalid);
            }
        }
    }
    Ok(())
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
    match verify(reader, layout, &mut Budget::default(), || tick(0)) {
        Ok(()) => {}
        Err(ProbeError::Invalid) => return Err(INVALID.into()),
        Err(ProbeError::Failed(error)) => return Err(error),
    }
    let mut mapped = Mapped::new(reader, &layout.runs);
    if hash_range(&mut mapped, 0, size, || tick(0))? != *expected {
        return Err(INVALID.into());
    }
    copy_verified(mapped, size, expected, out, take, INVALID, tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recovery::{extract, Candidate},
        signature_scan::{self, Checkpoint, Kind, Options, StoredCandidate},
    };
    use std::io::Cursor;

    const OPUS: &[u8] = include_bytes!("../../tests/fixtures/signature-ogg-opus.ogg");
    const MULTIPLEX: &[u8] = include_bytes!("../../tests/fixtures/signature-ogg-multiplex.ogg");

    fn options() -> Options {
        Options {
            kinds: vec![Kind::Ogg],
            ogg_fragments: true,
            ..Options::default()
        }
    }
    fn disperse(image: &mut [u8], positions: [usize; 3]) {
        for (start, bytes) in positions
            .into_iter()
            .zip([&OPUS[..47], &OPUS[47..135], &OPUS[135..]])
        {
            image[start..start + bytes.len()].copy_from_slice(bytes);
        }
    }
    fn scan(image: &[u8], options: &Options, plan: Option<&Plan>) -> Vec<Candidate> {
        let mut found = Vec::new();
        signature_scan::continue_scan_scoped(
            &mut Cursor::new(image),
            image.len() as u64,
            options,
            &mut Checkpoint::default(),
            4096,
            plan,
            |_, _| Ok(()),
            |c| found.push(c),
        )
        .unwrap();
        found
    }
    #[test]
    fn recover_out_of_order_ogg_pages_across_a_scan_boundary_exactly() {
        let mut image = vec![0x6e; 3 << 20];
        disperse(&mut image, [(1 << 20) - 2, 2 << 20, 4096]);
        assert!(scan(
            &image,
            &Options {
                ogg_fragments: false,
                ..options()
            },
            None
        )
        .is_empty());
        let files = scan(&image, &options(), None);
        assert_eq!(files.len(), 1);
        let stored = StoredCandidate::from_candidate(&files[0]).unwrap();
        assert!(stored.fragmented());
        let decoded: StoredCandidate =
            serde_json::from_slice(&serde_json::to_vec(&stored).unwrap()).unwrap();
        let candidate = decoded
            .candidate(0, 0..image.len() as u64, &options(), None)
            .unwrap();
        let mut output = Vec::new();
        extract(
            &mut Cursor::new(&image),
            &candidate,
            &mut output,
            u64::MAX,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(output, OPUS);
        image[(2 << 20) + 31] ^= 1;
        output.clear();
        assert!(extract(
            &mut Cursor::new(&image),
            &candidate,
            &mut output,
            10,
            |_| Ok(())
        )
        .is_err());
        assert!(output.is_empty());
        assert!(scan(&image, &options(), None).is_empty());
    }

    #[test]
    fn reject_ambiguous_or_incomplete_streams_without_splitting_existing_multiplex() {
        let mut image = vec![0; 65536];
        image[16384..16384 + MULTIPLEX.len()].copy_from_slice(MULTIPLEX);
        disperse(&mut image, [4096, 8192, 32768]);
        let hits = scan(&image, &options(), None);
        assert_eq!(hits.len(), 2);
        assert!(!StoredCandidate::from_candidate(&hits[0])
            .unwrap()
            .fragmented());
        assert!(StoredCandidate::from_candidate(&hits[1])
            .unwrap()
            .fragmented());
        image.copy_within(8192..8192 + 88, 49152);
        assert_eq!(scan(&image, &options(), None).len(), 1);
        image[49152..49152 + 88].fill(0);
        image[8192 + 70..8192 + 88].fill(0);
        image[57344..57344 + 18].copy_from_slice(&OPUS[117..135]);
        assert_eq!(scan(&image, &options(), None).len(), 1);
    }

    #[test]
    fn disjoint_free_runs_exclude_an_allocated_page_and_keep_start_alignment() {
        let mut image = vec![0; 8 * 4096];
        disperse(&mut image, [4096, 3 * 4096 + 5, 6 * 4096]);
        let plan = |middle| {
            let mut builder = crate::used_space::Builder::new(image.len() as u64, 4096).unwrap();
            builder.free(4096, 4096).unwrap();
            if middle {
                builder.free(3 * 4096, 4096).unwrap();
            }
            builder.free(6 * 4096, 4096).unwrap();
            builder.finish().unwrap()
        };
        let options = Options {
            sector_aligned: true,
            free_only: true,
            ..options()
        };
        let complete = plan(true);
        let incomplete = plan(false);
        let mut files = scan(&image, &options, Some(&complete));
        assert_eq!(files.len(), 1);
        signature_scan::verify_free_candidates(&complete, &files).unwrap();
        if let crate::recovery::Locator::Signature {
            allocation_digest, ..
        } = &mut files[0].locator
        {
            *allocation_digest = Some(incomplete.digest.clone());
        }
        assert!(signature_scan::verify_free_candidates(&incomplete, &files).is_err());
        assert!(scan(&image, &options, Some(&incomplete)).is_empty());
    }

    #[test]
    fn combined_fragment_passes_pause_and_resume_without_duplicate_results() {
        let mut image = vec![0; 16384];
        disperse(&mut image, [4096, 8192, 12288]);
        let size = image.len() as u64;
        let options = Options {
            kinds: vec![Kind::Ogg, Kind::Zip],
            zip_fragments: true,
            ..options()
        };
        let mut checkpoint = Checkpoint::default();
        let mut output = Vec::new();
        assert!(signature_scan::continue_scan(
            &mut Cursor::new(&image),
            size,
            &options,
            &mut checkpoint,
            |done, total| if done == total {
                Err("pause".into())
            } else {
                Ok(())
            },
            |file| output.push(file)
        )
        .is_err());
        assert!(output.is_empty());
        assert_eq!(checkpoint.work_done(&options), size);
        let mut checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        signature_scan::continue_scan(
            &mut Cursor::new(&image),
            size,
            &options,
            &mut checkpoint,
            |_, _| Ok(()),
            |file| output.push(file),
        )
        .unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(checkpoint.work_done(&options), size * 3);
    }

    #[test]
    fn a_packet_can_continue_across_displaced_pages_when_both_flags_agree() {
        fn page(sequence: u32, flags: u8, granule: u64, lacing: u8) -> Vec<u8> {
            let mut bytes = b"OggS\0".to_vec();
            bytes.push(flags);
            bytes.extend(granule.to_le_bytes());
            bytes.extend(0xcbac_105eu32.to_le_bytes());
            bytes.extend(sequence.to_le_bytes());
            bytes.extend([0; 4]);
            bytes.extend([1, lacing]);
            bytes.extend(std::iter::repeat_n(0xa1, lacing.into()));
            let checksum = bytes.iter().fold(0u32, |crc, &byte| {
                (0..8).fold(crc ^ ((byte as u32) << 24), |crc, _| {
                    let polynomial = if crc & 0x8000_0000 == 0 {
                        0
                    } else {
                        0x04c1_1db7
                    };
                    (crc << 1) ^ polynomial
                })
            });
            bytes[22..26].copy_from_slice(&checksum.to_le_bytes());
            bytes
        }
        let first = page(0, 2, u64::MAX, 255);
        let mut image = vec![0; 8192];
        image[4096..4096 + first.len()].copy_from_slice(&first);
        for flags in [4, 5] {
            let last = page(1, flags, 100, 8);
            image[1024..1024 + last.len()].copy_from_slice(&last);
            let files = scan(&image, &options(), None);
            assert_eq!(files.len(), usize::from(flags == 5));
        }
    }
}

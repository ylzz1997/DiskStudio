//! Bounded, fixed-width byte conditions. No device access in this module.
use crate::sector_data::{
    self, compile_atoms, expression, CompiledPattern, Pattern, SearchResult, MAX_HITS,
};
pub use crate::surface_scan::Direction;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
const MAX_CONDITIONS: usize = 16;
const MAX_SPAN: usize = 65536;
const CHUNK: usize = 65536;
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Any,
    All,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Condition {
    pub pattern: Pattern,
    pub offset: u32,
    #[serde(default)]
    pub ignore_ascii_case: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_wildcard: Option<char>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector_byte: Option<u32>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Query {
    pub conditions: Vec<Condition>,
    pub mode: Mode,
}
pub enum CompiledQuery {
    Single(CompiledPattern),
    Combined {
        regex: regex::bytes::Regex,
        span: usize,
        origin: usize,
        placement: Option<Placement>,
    },
}
impl CompiledQuery {
    pub fn len(&self) -> usize {
        match self {
            Self::Single(p) => p.len(),
            Self::Combined { span, .. } => *span,
        }
    }
}
pub struct PlacedCondition {
    offset: usize,
    atoms: Vec<(u8, u8)>,
    base_remainder: Option<u64>,
}

pub enum Placement {
    All {
        sector_size: u64,
        remainder: u64,
    },
    Any {
        sector_size: u64,
        conditions: Vec<PlacedCondition>,
    },
}

impl Placement {
    fn accepts(&self, base: u64, window: &[u8], index: usize) -> bool {
        match self {
            Self::All {
                sector_size,
                remainder,
            } => base % sector_size == *remainder,
            Self::Any {
                sector_size,
                conditions,
            } => {
                let remainder = base % sector_size;
                conditions.iter().any(|condition| {
                    if condition
                        .base_remainder
                        .is_some_and(|required| required != remainder)
                    {
                        return false;
                    }
                    let at = index + condition.offset;
                    window
                        .get(at..at + condition.atoms.len())
                        .is_some_and(|bytes| {
                            bytes
                                .iter()
                                .zip(&condition.atoms)
                                .all(|(&byte, &(value, mask))| byte & mask == value)
                        })
                })
            }
        }
    }
}
impl Query {
    #[cfg(test)]
    pub fn compile(&self) -> Result<CompiledQuery, String> {
        self.compile_for(Direction::Forward, 512)
    }
    pub fn compile_for(
        &self,
        direction: Direction,
        sector_size: u64,
    ) -> Result<CompiledQuery, String> {
        if !matches!(sector_size, 512 | 4096) {
            return Err("容量或扇区大小无效。".into());
        }
        if self.conditions.is_empty() || self.conditions.len() > MAX_CONDITIONS {
            return Err("搜索条件须为 1–16 项。".into());
        }
        let mut conditions = Vec::new();
        let mut span = 0;
        for condition in &self.conditions {
            let atoms = condition
                .pattern
                .atoms(condition.ignore_ascii_case, condition.text_wildcard)?;
            let offset = condition.offset as usize;
            let end = offset
                .checked_add(atoms.len())
                .filter(|&end| end <= MAX_SPAN)
                .ok_or("条件偏移与长度合计须在 64 KiB 内。")?;
            span = span.max(end);
            let base_remainder = condition
                .sector_byte
                .map(u64::from)
                .map(|byte| {
                    if byte >= sector_size {
                        return Err("条件的扇区内偏移超出逻辑扇区。");
                    }
                    Ok(
                        (sector_size + byte - u64::from(condition.offset) % sector_size)
                            % sector_size,
                    )
                })
                .transpose()?;
            conditions.push(PlacedCondition {
                offset,
                atoms,
                base_remainder,
            });
        }
        let requires_alignment = conditions.iter().any(|c| c.base_remainder.is_some());
        if direction == Direction::Forward
            && self.conditions.len() == 1
            && self.conditions[0].offset == 0
            && !requires_alignment
        {
            return Ok(CompiledQuery::Single(compile_atoms(&conditions[0].atoms)?));
        }
        // Offsets before the first condition carry no constraints. Search from
        // that condition and translate the result back to the requested base,
        // rather than compiling a long wildcard prefix at every byte position.
        let origin = conditions.iter().map(|c| c.offset).min().unwrap();
        let pattern = if self.mode == Mode::All {
            let mut merged = vec![(0u8, 0u8); span - origin];
            for PlacedCondition { offset, atoms, .. } in &conditions {
                for (i, &(v, m)) in atoms.iter().enumerate() {
                    let (old_v, old_m) = merged[offset - origin + i];
                    if (old_v ^ v) & old_m & m != 0 {
                        return Err("全部匹配的条件在重叠位置相互矛盾。".into());
                    }
                    merged[offset - origin + i] = (old_v | v, old_m | m);
                }
            }
            expression(&merged)
        } else {
            conditions
                .iter()
                .map(|PlacedCondition { offset, atoms, .. }| {
                    let prefix = if *offset == origin {
                        String::new()
                    } else {
                        format!("[\\x00-\\xFF]{{{}}}", offset - origin)
                    };
                    format!("(?:{prefix}{})", expression(atoms))
                })
                .collect::<Vec<_>>()
                .join("|")
        };
        let regex = regex::bytes::RegexBuilder::new(&pattern)
            .unicode(false)
            .size_limit(8 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
            .map_err(|_| "组合搜索条件超出编译资源限制，请减少偏移或条件。")?;
        let placement = match (requires_alignment, self.mode) {
            (false, _) => None,
            (true, Mode::Any) => Some(Placement::Any {
                sector_size,
                conditions,
            }),
            (true, Mode::All) => {
                let mut remainders = conditions.iter().filter_map(|c| c.base_remainder);
                let remainder = remainders.next().unwrap();
                if remainders.any(|next| next != remainder) {
                    return Err("全部匹配的扇区内偏移无法对齐到同一基准。".into());
                }
                Some(Placement::All {
                    sector_size,
                    remainder,
                })
            }
        };
        Ok(CompiledQuery::Combined {
            regex,
            span,
            origin,
            placement,
        })
    }
}
pub fn search_direction<R: Read + Seek>(
    reader: &mut R,
    size: u64,
    query: &CompiledQuery,
    direction: Direction,
    cancel: &AtomicBool,
    publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    if direction == Direction::Forward {
        search(reader, size, query, cancel, publish)
    } else {
        search_backward(reader, size, query, cancel, publish)
    }
}

fn search_backward<R: Read + Seek>(
    reader: &mut R,
    size: u64,
    query: &CompiledQuery,
    cancel: &AtomicBool,
    mut publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    let CompiledQuery::Combined {
        regex,
        span,
        origin,
        placement,
    } = query
    else {
        return Err("反向搜索条件未正确编译。".into());
    };
    if size == 0 || *span == 0 || *span > MAX_SPAN {
        return Err("搜索范围或条件无效。".into());
    }
    let mut result = SearchResult {
        state: "running".into(),
        checked_bytes: 0,
        total_bytes: size,
        hits: vec![],
        pattern_bytes: *span,
        message: String::new(),
    };
    let mut buffer = vec![0u8; CHUNK + span - 1];
    // Keep at most one window's base positions, independent of disk capacity.
    // Forward enumeration within the window preserves overlapping regex matches;
    // only after it finishes do we commit those bases in descending order.
    let mut candidates = Vec::<u32>::with_capacity(CHUNK);
    let (mut end, mut carry) = (size, 0usize);
    let mut last = Instant::now();
    publish(&result);
    'scan: while end > 0 {
        if cancel.load(Ordering::Relaxed) {
            result.state = "canceled".into();
            break;
        }
        // Align to the range origin, including a short final block. This avoids
        // revisiting the bounded Region reader's aligned 1 MiB cache windows.
        let start = (end - 1) / CHUNK as u64 * CHUNK as u64;
        let n = (end - start) as usize;
        buffer.copy_within(0..carry, n);
        if let Err(e) = reader
            .seek(SeekFrom::Start(start))
            .and_then(|_| reader.read_exact(&mut buffer[..n]))
        {
            result.state = "failed".into();
            result.message = format!("反向读取失败，未跳过错误范围：{e}");
            break;
        }
        let window = &buffer[..n + carry];
        candidates.clear();
        let mut from = *origin;
        while from < n + origin && from < window.len() {
            if cancel.load(Ordering::Relaxed) {
                // An unfinished forward enumeration proves no complete reverse
                // suffix of this block. Do not publish its provisional matches.
                result.state = "canceled".into();
                break 'scan;
            }
            let Some(found) = regex.find_at(window, from) else {
                break;
            };
            let position = found.start() - origin;
            if position >= n {
                break;
            }
            if found.is_empty() {
                return Err("组合条件意外匹配空数据，已停止。".into());
            }
            let base = start + position as u64;
            if placement
                .as_ref()
                .is_none_or(|rule| rule.accepts(base, window, position))
            {
                candidates.push(position as u32);
            }
            from = found.start() + 1;
        }
        for &offset in candidates.iter().rev() {
            if cancel.load(Ordering::Relaxed) {
                result.state = "canceled".into();
                break 'scan;
            }
            let position = start + u64::from(offset);
            result.hits.push(position);
            result.checked_bytes = size - position;
            if result.hits.len() == MAX_HITS {
                result.state = "limited".into();
                result.message =
                    "达到 1024 个匹配上限，反向搜索已停止；较低位置尚未检查完。".into();
                break 'scan;
            }
        }
        result.checked_bytes = size - start;
        carry = (span - 1).min(window.len());
        end = start;
        if last.elapsed().as_millis() >= 200 {
            publish(&result);
            last = Instant::now();
        }
    }
    if result.state == "running" {
        result.state = "completed".into();
        result.message = format!(
            "所选范围已反向搜索完成，找到 {} 个匹配基准位置。",
            result.hits.len()
        );
    } else if result.state == "canceled" {
        result.message = "反向搜索已停止；进度从范围末尾计算，未完成位置没有结论。".into();
    }
    publish(&result);
    Ok(result)
}
pub fn search<R: Read>(
    reader: &mut R,
    size: u64,
    query: &CompiledQuery,
    cancel: &AtomicBool,
    mut publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    let CompiledQuery::Combined {
        regex,
        span,
        origin,
        placement,
    } = query
    else {
        let CompiledQuery::Single(pattern) = query else {
            unreachable!()
        };
        return sector_data::search_compiled(reader, size, pattern, cancel, publish);
    };
    if size == 0 || *span == 0 || *span > MAX_SPAN {
        return Err("搜索范围或条件无效。".into());
    }
    let mut result = SearchResult {
        state: "running".into(),
        checked_bytes: 0,
        total_bytes: size,
        hits: vec![],
        pattern_bytes: *span,
        message: String::new(),
    };
    let mut buffer = vec![0u8; CHUNK + span - 1];
    let (mut carry, mut read_bytes) = (0usize, 0u64);
    let mut last = Instant::now();
    publish(&result);
    'scan: while read_bytes < size {
        if cancel.load(Ordering::Relaxed) {
            result.state = "canceled".into();
            break;
        }
        let n = (size - read_bytes).min(CHUNK as u64) as usize;
        if let Err(e) = reader.read_exact(&mut buffer[carry..carry + n]) {
            result.state = "failed".into();
            result.message = format!("读取失败，未跳过错误范围：{e}");
            break;
        }
        let base = read_bytes - carry as u64;
        read_bytes += n as u64;
        // Commit candidate starts only when every alternative fits in the window.
        // At EOF, shorter alternatives may still match the remaining tail.
        let ready = if read_bytes == size {
            size
        } else {
            read_bytes.saturating_sub(*span as u64 - 1)
        };
        let window = &buffer[..carry + n];
        let mut from = (result.checked_bytes - base) as usize + origin;
        while from < window.len() {
            if cancel.load(Ordering::Relaxed) {
                result.state = "canceled".into();
                break 'scan;
            }
            let Some(found) = regex.find_at(window, from) else {
                break;
            };
            let index = found.start() - origin;
            let position = base + index as u64;
            if position >= ready {
                break;
            }
            if found.is_empty() {
                return Err("组合条件意外匹配空数据，已停止。".into());
            }
            result.checked_bytes = position + 1;
            if placement
                .as_ref()
                .is_none_or(|rule| rule.accepts(position, window, index))
            {
                result.hits.push(position);
            }
            if result.hits.len() == MAX_HITS {
                result.state = "limited".into();
                result.message = "达到 1024 个匹配上限，搜索已停止；可缩小范围后继续查找。".into();
                break 'scan;
            }
            from = found.start() + 1;
        }
        result.checked_bytes = ready;
        carry = (span - 1).min(window.len());
        let end = window.len();
        buffer.copy_within(end - carry..end, 0);
        if last.elapsed().as_millis() >= 200 {
            publish(&result);
            last = Instant::now();
        }
    }
    if result.state == "running" {
        result.state = "completed".into();
        result.checked_bytes = size;
        result.message = format!(
            "所选范围已搜索完成，找到 {} 个匹配基准位置。",
            result.hits.len()
        );
    } else if result.state == "canceled" {
        result.message = "搜索已停止；未完成位置没有结论。".into();
    }
    publish(&result);
    Ok(result)
}

#[cfg(test)]
#[path = "sector_query_reverse_tests.rs"]
mod reverse_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn hex(value: &str, offset: u32) -> Condition {
        Condition {
            pattern: Pattern::Hex(value.into()),
            offset,
            ignore_ascii_case: false,
            text_wildcard: None,
            sector_byte: None,
        }
    }
    fn run(bytes: &[u8], conditions: Vec<Condition>, mode: Mode) -> SearchResult {
        let query = Query { conditions, mode }.compile().unwrap();
        search(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            &query,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap()
    }
    #[test]
    fn offsets_and_all_conditions_locate_base_instead_of_individual_markers() {
        let mut data = vec![0; 2048];
        for base in [0, 512, 1024] {
            data[base + 82..base + 87].copy_from_slice(b"FAT32");
        }
        for base in [0, 1024] {
            data[base + 510..base + 512].copy_from_slice(&[0x55, 0xaa]);
        }
        let r = run(
            &data,
            vec![hex("46 41 54 33 32", 82), hex("55 AA", 510)],
            Mode::All,
        );
        assert_eq!(r.hits, vec![0, 1024]);
        assert_eq!(r.checked_bytes, 2048);
        assert_eq!(
            run(&data, vec![hex("46 41 54 33 32", 82)], Mode::Any).hits,
            vec![0, 512, 1024]
        );
    }
    #[test]
    fn any_merges_duplicates_in_order_and_keeps_short_tail_matches() {
        let r = run(
            b"ABABAQ",
            vec![hex("41 42 41", 0), hex("41", 0), hex("51", 0)],
            Mode::Any,
        );
        assert_eq!(r.hits, vec![0, 2, 4, 5]);
        assert_eq!(
            run(b"AB", vec![hex("41 42 43 44", 0), hex("42", 0)], Mode::Any).hits,
            vec![1]
        );
        assert!(run(b"AB", vec![hex("41", 1)], Mode::All).hits.is_empty());
    }
    #[test]
    fn cross_chunk_candidates_are_not_lost_duplicated_or_misordered() {
        let mut data = vec![0; CHUNK + 100];
        data[CHUNK - 6..CHUNK + 2].copy_from_slice(b"ABCDEFAB");
        let r = run(
            &data,
            vec![hex("41 42 43 44 45 46", 0), hex("41 42", 0)],
            Mode::Any,
        );
        assert_eq!(r.hits, vec![(CHUNK - 6) as u64, CHUNK as u64]);
        let r = run(
            &data,
            vec![hex("41 42", 0), hex("45 46 41 42", 4)],
            Mode::All,
        );
        assert_eq!(r.hits, vec![(CHUNK - 6) as u64]);
    }
    #[test]
    fn mask_intersections_text_folding_and_conflicts_are_exact() {
        assert_eq!(
            run(
                &[0x4f, 0x41, 0x5f],
                vec![hex("4?", 0), hex("?F", 0)],
                Mode::All
            )
            .hits,
            vec![0]
        );
        assert!(Query {
            conditions: vec![hex("4?", 0), hex("5?", 0)],
            mode: Mode::All
        }
        .compile()
        .is_err());
        let text = Condition {
            pattern: Pattern::Utf8("a".into()),
            offset: 0,
            ignore_ascii_case: true,
            text_wildcard: None,
            sector_byte: None,
        };
        assert_eq!(
            run(b"Aa", vec![text, hex("41", 0)], Mode::All).hits,
            vec![0]
        );
        let text = Condition {
            pattern: Pattern::Utf16Le("Ł".into()),
            offset: 0,
            ignore_ascii_case: true,
            text_wildcard: None,
            sector_byte: None,
        };
        assert_eq!(
            run(&[0x41, 1, 0x61, 1], vec![text, hex("?? 01", 0)], Mode::All).hits,
            vec![0]
        );
    }
    #[test]
    fn text_wildcards_match_one_encoded_unit_and_preserve_literals() {
        let mut utf8 = Condition {
            pattern: Pattern::Utf8(r"a!\!\\z".into()),
            offset: 0,
            ignore_ascii_case: true,
            text_wildcard: Some('!'),
            sector_byte: None,
        };
        let data = [
            b'A', 0xff, b'!', b'\\', b'z', b'a', 0xc3, 0xa9, b'!', b'\\', b'Z',
        ];
        assert_eq!(run(&data, vec![utf8.clone()], Mode::All).hits, vec![0]);
        utf8.text_wildcard = None;
        assert_eq!(
            run(br"A!\!\\Z", vec![utf8.clone()], Mode::All).hits,
            vec![0]
        );
        let wide = Condition {
            pattern: Pattern::Utf16Le("Ł*a".into()),
            text_wildcard: Some('*'),
            ..utf8.clone()
        };
        let data = [0x41, 1, 0xff, 0x82, b'A', 0, b'a', 1, 0xff, 0x82, b'A', 0];
        assert_eq!(run(&data, vec![wide], Mode::Any).hits, vec![0]);
        for marker in [' ', '\\', 'é'] {
            let query = Query {
                mode: Mode::All,
                conditions: vec![Condition {
                    text_wildcard: Some(marker),
                    ..utf8.clone()
                }],
            };
            assert!(query.compile().is_err());
        }
    }

    #[test]
    fn sector_placements_match_across_chunks_and_consider_every_any_branch() {
        let mut data = vec![0; CHUNK + 4096];
        for at in [0, 511, 600, CHUNK - 2, CHUNK + 10] {
            data[at..at + 4].copy_from_slice(b"ABCD");
        }
        // Both regex branches match at 511, but only the shorter one permits
        // that placement.
        let long = Condition {
            sector_byte: Some(0),
            ..hex("41 42 43 44", 0)
        };
        let short = Condition {
            sector_byte: Some(511),
            ..hex("41 42", 0)
        };
        let any = Query {
            mode: Mode::Any,
            conditions: vec![long, short],
        };
        let prefix = Condition {
            sector_byte: Some(4094),
            ..hex("41 42", 0)
        };
        let suffix = Condition {
            sector_byte: Some(0),
            ..hex("43 44", 2)
        };
        let all = Query {
            mode: Mode::All,
            conditions: vec![prefix, suffix],
        };
        for direction in [Direction::Forward, Direction::Backward] {
            for (query, mut expected) in [(&any, vec![0, 511]), (&all, vec![(CHUNK - 2) as u64])] {
                if direction == Direction::Backward {
                    expected.reverse();
                }
                let compiled = query.compile_for(direction, 4096).unwrap();
                let result = search_direction(
                    &mut Cursor::new(&data),
                    data.len() as u64,
                    &compiled,
                    direction,
                    &AtomicBool::new(false),
                    |_| {},
                )
                .unwrap();
                assert_eq!(result.hits, expected);
                assert_eq!(result.checked_bytes, data.len() as u64);
            }
        }
        for (query, size) in [
            (all.clone(), 512),
            (
                Query {
                    mode: Mode::All,
                    conditions: vec![
                        all.conditions[0].clone(),
                        Condition {
                            sector_byte: Some(1),
                            ..all.conditions[1].clone()
                        },
                    ],
                },
                4096,
            ),
        ] {
            assert!(query.compile_for(Direction::Forward, size).is_err());
        }
    }

    #[test]
    fn bounded_typed_queries_reject_invalid_or_excessive_inputs() {
        assert!(Query {
            conditions: vec![],
            mode: Mode::Any
        }
        .compile()
        .is_err());
        assert!(Query {
            conditions: vec![hex("00", 0); 17],
            mode: Mode::Any
        }
        .compile()
        .is_err());
        assert!(Query {
            conditions: vec![hex("00 00", 65535)],
            mode: Mode::Any
        }
        .compile()
        .is_err());
        assert!(Query {
            conditions: vec![hex("00", u32::MAX)],
            mode: Mode::Any
        }
        .compile()
        .is_err());
        assert!(Query {
            conditions: vec![hex("?G", 0)],
            mode: Mode::Any
        }
        .compile()
        .is_err());
        assert!(
            serde_json::from_str::<Query>(r#"{"mode":"any","conditions":[],"path":"x"}"#).is_err()
        );
        assert!(serde_json::from_str::<Condition>(
            r#"{"pattern":{"encoding":"hex","value":"AA"},"offset":-1}"#
        )
        .is_err());
        assert_eq!(
            Query {
                conditions: vec![hex("00", 65535)],
                mode: Mode::All
            }
            .compile()
            .unwrap()
            .len(),
            65536
        );
    }
    #[test]
    fn composite_scope_limits_cancellation_and_read_errors_do_not_invent_coverage() {
        let query = Query {
            conditions: vec![hex("??", 1), hex("??", 2)],
            mode: Mode::Any,
        }
        .compile()
        .unwrap();
        let mut reader = Cursor::new(vec![0; 70000]);
        let r = search(&mut reader, 70000, &query, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.state, "limited");
        assert_eq!(r.hits, (0..1024).collect::<Vec<u64>>());
        assert_eq!(r.checked_bytes, 1024);
        let mut reader = Cursor::new(b"ABCDE");
        let r = search(&mut reader, 2, &query, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(reader.position(), 2);
        assert_eq!(r.hits, vec![0]);
        let flag = AtomicBool::new(true);
        let mut reader = Cursor::new(vec![0; 100]);
        assert_eq!(
            search(&mut reader, 100, &query, &flag, |_| {})
                .unwrap()
                .state,
            "canceled"
        );
        assert_eq!(reader.position(), 0);
        let query = Query {
            conditions: vec![hex("FF", 0), hex("AA", 10)],
            mode: Mode::All,
        }
        .compile()
        .unwrap();
        let r = search(
            &mut Cursor::new(vec![0; CHUNK + 10]),
            (2 * CHUNK) as u64,
            &query,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(r.state, "failed");
        assert_eq!(r.checked_bytes, (CHUNK - 10) as u64);
        assert!(r.hits.is_empty());
    }
    #[test]
    fn combined_results_match_independent_oracle_for_both_modes() {
        let mut data: Vec<u8> = (0..CHUNK + 77)
            .map(|i| ((i * 73 + i / 17) % 256) as u8)
            .collect();
        for at in [3, CHUNK - 4, CHUNK + 6] {
            data[at + 2] = 0xa5;
            data[at + 7] = 0x5f;
        }
        for mode in [Mode::Any, Mode::All] {
            let expected: Vec<u64> = (0..data.len())
                .filter(|&i| {
                    let a = data.get(i + 2).is_some_and(|b| b & 0xf0 == 0xa0);
                    let b = data.get(i + 7).is_some_and(|b| b & 0x0f == 15);
                    if mode == Mode::Any {
                        a || b
                    } else {
                        a && b
                    }
                })
                .map(|i| i as u64)
                .take(MAX_HITS)
                .collect();
            assert_eq!(
                run(&data, vec![hex("A?", 2), hex("?F", 7)], mode).hits,
                expected
            );
        }
    }
}

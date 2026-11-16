//! Byte search and bounded sector reads; no device discovery or writes.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
pub const MAX_PATTERN: usize = 4096;
pub const MAX_HITS: usize = 1024;
const CHUNK: usize = 1024 * 1024;
#[derive(Clone, Deserialize, Serialize)]
#[serde(
    tag = "encoding",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Pattern {
    Hex(String),
    Utf8(String),
    Utf16Le(String),
}
impl Pattern {
    pub(crate) fn atoms(&self, fold: bool) -> Result<Vec<(u8, u8)>, String> {
        if let Self::Hex(s) = self {
            if fold {
                return Err("十六进制搜索不使用文本大小写选项。".into());
            }
            if s.len() > MAX_PATTERN * 3 {
                return Err("搜索条件过长。".into());
            }
            let compact: Vec<_> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            if compact.is_empty()
                || compact.len() % 2 != 0
                || compact.len() / 2 > MAX_PATTERN
                || !compact.iter().all(|b| b.is_ascii_hexdigit() || *b == b'?')
            {
                return Err("请输入完整的 Hex 字节；? 表示一个十六进制位。".into());
            }
            return Ok(compact
                .chunks_exact(2)
                .map(|pair| {
                    let nibble = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
                    (
                        nibble(pair[0]) * 16 + nibble(pair[1]),
                        (if pair[0] == b'?' { 0 } else { 0xf0 })
                            | (if pair[1] == b'?' { 0 } else { 0x0f }),
                    )
                })
                .collect());
        }
        let bytes = self.bytes()?;
        Ok(bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                let is_ascii_unit = !matches!(self, Self::Utf16Le(_))
                    || (i % 2 == 0 && bytes.get(i + 1) == Some(&0));
                if fold && is_ascii_unit && b.is_ascii_alphabetic() {
                    (b.to_ascii_uppercase(), 0xdf)
                } else {
                    (b, 0xff)
                }
            })
            .collect())
    }
    pub fn compile(&self, ignore_ascii_case: bool) -> Result<CompiledPattern, String> {
        let masked_hex = matches!(self, Self::Hex(s) if s.contains('?'));
        if !masked_hex && !ignore_ascii_case {
            return Ok(CompiledPattern::Exact(self.bytes()?));
        }
        let mut expression = String::new();
        let len;
        match self {
            Self::Hex(s) => {
                if ignore_ascii_case {
                    return Err("十六进制搜索不使用文本大小写选项。".into());
                }
                if s.len() > MAX_PATTERN * 3 {
                    return Err("搜索条件过长。".into());
                }
                let compact: Vec<_> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
                if compact.is_empty()
                    || compact.len() % 2 != 0
                    || compact.len() / 2 > MAX_PATTERN
                    || !compact.iter().all(|b| b.is_ascii_hexdigit() || *b == b'?')
                {
                    return Err(
                        "请输入完整的 Hex 字节；? 表示一个十六进制位，例如 46 4? ?? 33 32。".into(),
                    );
                }
                len = compact.len() / 2;
                for pair in compact.chunks_exact(2) {
                    let nibble = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
                    let mask = (if pair[0] == b'?' { 0 } else { 0xf0 })
                        | (if pair[1] == b'?' { 0 } else { 0x0f });
                    let value = nibble(pair[0]) * 16 + nibble(pair[1]);
                    if mask == 0 {
                        expression.push_str("[\\x00-\\xFF]");
                    } else if mask == 0xf0 {
                        expression.push_str(&format!("[\\x{value:02X}-\\x{:02X}]", value + 15));
                    } else if mask == 255 {
                        expression.push_str(&format!("\\x{value:02X}"));
                    } else {
                        expression.push('[');
                        for byte in 0..=255u8 {
                            if byte & mask == value {
                                expression.push_str(&format!("\\x{byte:02X}"));
                            }
                        }
                        expression.push(']');
                    }
                }
            }
            Self::Utf8(_) => {
                let bytes = self.bytes()?;
                len = bytes.len();
                for byte in bytes {
                    ascii_atom(&mut expression, byte, ignore_ascii_case);
                }
            }
            Self::Utf16Le(_) => {
                let bytes = self.bytes()?;
                len = bytes.len();
                for unit in bytes.chunks_exact(2) {
                    // Fold only ASCII code points, never a coincidental ASCII-valued byte
                    // within a different UTF-16 code unit (e.g. U+0141).
                    ascii_atom(&mut expression, unit[0], ignore_ascii_case && unit[1] == 0);
                    ascii_atom(&mut expression, unit[1], false);
                }
            }
        }
        let regex = regex::bytes::RegexBuilder::new(&expression)
            .unicode(false)
            .size_limit(8 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
            .map_err(|_| "搜索条件超出编译资源限制，请缩短条件。")?;
        Ok(CompiledPattern::Masked { regex, len })
    }
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        let value = match self {
            Self::Hex(s) | Self::Utf8(s) | Self::Utf16Le(s) => s,
        };
        if value.len() > MAX_PATTERN * 3 {
            return Err("搜索条件过长。".into());
        }
        let bytes = match self {
            Self::Hex(s) => {
                let compact: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
                if compact.len() % 2 != 0 || !compact.iter().all(u8::is_ascii_hexdigit) {
                    return Err("请输入完整的十六进制字节，例如 46 41 54 33 32。".into());
                }
                compact
                    .chunks_exact(2)
                    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                    .collect()
            }
            Self::Utf8(s) => s.as_bytes().to_vec(),
            Self::Utf16Le(s) => s.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        };
        if bytes.is_empty() || bytes.len() > MAX_PATTERN {
            return Err("搜索条件须为 1–4096 字节。".into());
        }
        Ok(bytes)
    }
}
fn ascii_atom(expression: &mut String, byte: u8, fold: bool) {
    if fold && byte.is_ascii_alphabetic() {
        expression.push_str(&format!(
            "[\\x{:02X}\\x{:02X}]",
            byte.to_ascii_lowercase(),
            byte.to_ascii_uppercase()
        ));
    } else {
        expression.push_str(&format!("\\x{byte:02X}"));
    }
}
pub enum CompiledPattern {
    Exact(Vec<u8>),
    Masked {
        regex: regex::bytes::Regex,
        len: usize,
    },
}
impl CompiledPattern {
    pub fn len(&self) -> usize {
        match self {
            Self::Exact(bytes) => bytes.len(),
            Self::Masked { len, .. } => *len,
        }
    }
}
pub fn search_compiled<R: Read>(
    reader: &mut R,
    size: u64,
    pattern: &CompiledPattern,
    cancel: &AtomicBool,
    publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    match pattern {
        CompiledPattern::Exact(bytes) => search(reader, size, bytes, cancel, publish),
        CompiledPattern::Masked { regex, len } => {
            search_masked(reader, size, regex, *len, cancel, publish)
        }
    }
}
fn search_masked<R: Read>(
    reader: &mut R,
    size: u64,
    regex: &regex::bytes::Regex,
    len: usize,
    cancel: &AtomicBool,
    mut publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    if size == 0 || len == 0 || len > MAX_PATTERN {
        return Err("搜索范围或条件无效。".into());
    }
    // Keep exactly m-1 bytes across bounded reads. Every match has fixed width,
    // so earlier windows cannot contain a match that ends in this new window.
    const MASK_CHUNK: usize = 64 * 1024;
    let mut buffer = vec![0; MASK_CHUNK + len - 1];
    let mut carry = 0;
    let mut result = SearchResult {
        state: "running".into(),
        checked_bytes: 0,
        total_bytes: size,
        hits: vec![],
        pattern_bytes: len,
        message: String::new(),
    };
    let mut last = Instant::now();
    publish(&result);
    'scan: while result.checked_bytes < size {
        if cancel.load(Ordering::Relaxed) {
            result.state = "canceled".into();
            break;
        }
        let n = (size - result.checked_bytes).min(MASK_CHUNK as u64) as usize;
        if let Err(e) = reader.read_exact(&mut buffer[carry..carry + n]) {
            result.state = "failed".into();
            result.message = format!("读取失败，未跳过错误范围：{e}");
            break;
        }
        let base = result.checked_bytes - carry as u64;
        let window = &buffer[..carry + n];
        let mut from = 0;
        let mut examined_end = carry;
        while from < window.len() {
            if cancel.load(Ordering::Relaxed) {
                result.checked_bytes = base + examined_end as u64;
                result.state = "canceled".into();
                break 'scan;
            }
            let Some(found) = regex.find_at(window, from) else {
                break;
            };
            // Fixed width and explicit byte-only compilation prevent empty matches.
            if found.len() != len {
                return Err("搜索条件宽度异常，已停止。".into());
            }
            result.hits.push(base + found.start() as u64);
            examined_end = found.end();
            if result.hits.len() == MAX_HITS {
                result.checked_bytes = base + found.end() as u64;
                result.state = "limited".into();
                result.message = "达到 1024 个匹配上限，搜索已停止；可缩小范围后继续查找。".into();
                break 'scan;
            }
            from = found.start() + 1; // Include overlapping matches.
        }
        result.checked_bytes += n as u64;
        carry = (len - 1).min(window.len());
        let end = window.len();
        buffer.copy_within(end - carry..end, 0);
        if last.elapsed().as_millis() >= 200 {
            publish(&result);
            last = Instant::now();
        }
    }
    if result.state == "running" {
        result.state = "completed".into();
        result.message = format!("所选范围已搜索完成，找到 {} 个匹配。", result.hits.len());
    } else if result.state == "canceled" {
        result.message = "搜索已停止，仅保留已检查范围内的匹配。".into();
    }
    publish(&result);
    Ok(result)
}
pub fn read_sector<R: Read + Seek>(
    reader: &mut R,
    size: u64,
    sector_size: u64,
    sector: u64,
) -> Result<Vec<u8>, String> {
    if !matches!(sector_size, 512 | 4096) || size % sector_size != 0 || sector >= size / sector_size
    {
        return Err("扇区位置超出所选范围。".into());
    }
    let offset = sector.checked_mul(sector_size).ok_or("扇区位置溢出")?;
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; sector_size as usize];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("扇区未完整读出：{e}"))?;
    Ok(bytes)
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub state: String,
    pub checked_bytes: u64,
    pub total_bytes: u64,
    pub hits: Vec<u64>,
    pub pattern_bytes: usize,
    pub message: String,
}
pub fn search<R: Read>(
    reader: &mut R,
    size: u64,
    pattern: &[u8],
    cancel: &AtomicBool,
    mut publish: impl FnMut(&SearchResult),
) -> Result<SearchResult, String> {
    if size == 0 || pattern.is_empty() || pattern.len() > MAX_PATTERN {
        return Err("搜索范围或条件无效。".into());
    }
    // KMP preserves overlapping matches across every buffer boundary, in O(n+m).
    let mut prefix = vec![0; pattern.len()];
    let mut matched = 0;
    for i in 1..pattern.len() {
        while matched > 0 && pattern[i] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[i] == pattern[matched] {
            matched += 1;
        }
        prefix[i] = matched;
    }
    matched = 0;
    let mut result = SearchResult {
        state: "running".into(),
        checked_bytes: 0,
        total_bytes: size,
        hits: vec![],
        pattern_bytes: pattern.len(),
        message: String::new(),
    };
    let mut buffer = vec![0; CHUNK];
    let mut last = Instant::now();
    publish(&result);
    'scan: while result.checked_bytes < size {
        if cancel.load(Ordering::Relaxed) {
            result.state = "canceled".into();
            result.message = "搜索已停止，仅保留已检查范围内的匹配。".into();
            break;
        }
        let n = (size - result.checked_bytes).min(CHUNK as u64) as usize;
        if let Err(e) = reader.read_exact(&mut buffer[..n]) {
            result.state = "failed".into();
            result.message = format!("读取失败，未跳过错误范围：{e}");
            break;
        }
        for (i, &byte) in buffer[..n].iter().enumerate() {
            if i % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                result.checked_bytes += i as u64;
                result.state = "canceled".into();
                result.message = "搜索已停止，仅保留已检查范围内的匹配。".into();
                break 'scan;
            }
            while matched > 0 && byte != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if byte == pattern[matched] {
                matched += 1;
            }
            if matched == pattern.len() {
                result
                    .hits
                    .push(result.checked_bytes + i as u64 + 1 - pattern.len() as u64);
                matched = prefix[matched - 1];
                if result.hits.len() == MAX_HITS {
                    result.checked_bytes += i as u64 + 1;
                    result.state = "limited".into();
                    result.message =
                        "达到 1024 个匹配上限，搜索已停止；可缩小范围后继续查找。".into();
                    break 'scan;
                }
            }
        }
        result.checked_bytes += n as u64;
        if last.elapsed().as_millis() >= 200 {
            publish(&result);
            last = Instant::now();
        }
    }
    if result.state == "running" {
        result.state = "completed".into();
        result.message = format!("所选范围已搜索完成，找到 {} 个匹配。", result.hits.len());
    }
    publish(&result);
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn masked(bytes: &[u8], pattern: Pattern, fold: bool) -> SearchResult {
        search_compiled(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            &pattern.compile(fold).unwrap(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap()
    }
    #[test]
    fn nibble_wildcards_match_binary_bytes_and_overlap_across_chunks() {
        let result = masked(
            &[0x4f, 0xff, 0, 0x41, 0xaf, 10, 0x40, 0xbe, 0],
            Pattern::Hex("4? ?F ??".into()),
            false,
        );
        assert_eq!(result.hits, vec![0, 3]);
        assert_eq!(
            masked(&[0, 10, 255], Pattern::Hex("??".into()), false).hits,
            vec![0, 1, 2]
        );
        let mut bytes = vec![0; 65536 + 10];
        bytes[65534..65539].copy_from_slice(b"ababa");
        let result = masked(&bytes, Pattern::Hex("61 ?? 61".into()), false);
        assert_eq!(result.hits, vec![65534, 65536]);
        assert_eq!(result.checked_bytes, bytes.len() as u64);
    }
    #[test]
    fn wildcard_matches_agree_with_independent_byte_mask_oracle() {
        let mut state = 0x12345678u32;
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        };
        let mut bytes: Vec<u8> = (0..70000).map(|_| next()).collect();
        for len in [2, 3, 17, 65, 4096] {
            let masks: Vec<u8> = (0..len).map(|i| [0xff, 0xf0, 0x0f, 0][i % 4]).collect();
            let values: Vec<u8> = masks.iter().map(|&mask| next() & mask).collect();
            let at = 65535 - len / 2;
            for i in 0..len {
                bytes[at + i] = values[i] | (next() & !masks[i]);
            }
            let pattern: String = values
                .iter()
                .zip(&masks)
                .map(|(&v, &m)| {
                    let hi = if m & 0xf0 == 0 {
                        "?".into()
                    } else {
                        format!("{:X}", v >> 4)
                    };
                    let lo = if m & 0x0f == 0 {
                        "?".into()
                    } else {
                        format!("{:X}", v & 15)
                    };
                    format!("{hi}{lo} ")
                })
                .collect();
            let expected: Vec<u64> = bytes
                .windows(len)
                .enumerate()
                .filter(|(_, window)| {
                    window
                        .iter()
                        .zip(&masks)
                        .zip(&values)
                        .all(|((&b, &m), &v)| b & m == v)
                })
                .map(|(i, _)| i as u64)
                .take(MAX_HITS)
                .collect();
            assert_eq!(
                masked(&bytes, Pattern::Hex(pattern), false).hits,
                expected,
                "length {len}"
            );
        }
    }
    #[test]
    fn text_folding_is_ascii_only_and_utf16_preserves_non_ascii_units() {
        assert_eq!(
            masked("aBc ABC abc".as_bytes(), Pattern::Utf8("AbC".into()), true).hits,
            vec![0, 4, 8]
        );
        assert!(masked("à".as_bytes(), Pattern::Utf8("À".into()), true)
            .hits
            .is_empty());
        let data: Vec<u8> = "aŁZ ašZ AŁz"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            masked(&data, Pattern::Utf16Le("AŁz".into()), true).hits,
            vec![0, 16]
        );
        assert_eq!(
            masked(b"[A].* [a].*", Pattern::Utf8("[a].*".into()), true).hits,
            vec![0, 6]
        );
    }
    #[test]
    fn masked_validation_and_resource_bounds_are_checked_before_reading() {
        for text in ["?", "???", "?G", "0x??", "?é", "?*"] {
            assert!(Pattern::Hex(text.into()).compile(false).is_err());
        }
        assert!(Pattern::Hex("??".repeat(MAX_PATTERN + 1))
            .compile(false)
            .is_err());
        assert!(Pattern::Hex("??".into()).compile(true).is_err());
        assert_eq!(
            Pattern::Hex("??".repeat(MAX_PATTERN))
                .compile(false)
                .unwrap()
                .len(),
            MAX_PATTERN
        );
        let mut reader = Cursor::new(b"AAAAZZ");
        let result = search_compiled(
            &mut reader,
            3,
            &Pattern::Hex("41 ?? ?? 41".into()).compile(false).unwrap(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(result.hits.is_empty());
        assert_eq!(reader.position(), 3);
    }
    #[test]
    fn masked_limit_cancel_and_failed_reads_preserve_honest_coverage() {
        let bytes = vec![0; 70000];
        let r = masked(&bytes, Pattern::Hex("?? ??".into()), false);
        assert_eq!(r.state, "limited");
        assert_eq!(r.hits.len(), 1024);
        assert_eq!(r.checked_bytes, 1025);
        let flag = AtomicBool::new(false);
        let pattern = Pattern::Hex("FF ??".into()).compile(false).unwrap();
        let mut reader = Cursor::new(&bytes);
        let r = search_compiled(&mut reader, bytes.len() as u64, &pattern, &flag, |_| {
            flag.store(true, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(r.state, "canceled");
        assert_eq!(reader.position(), 0);
        assert_eq!(r.checked_bytes, 0);
        let r = search_compiled(
            &mut Cursor::new(vec![0; 65536 + 10]),
            131072,
            &pattern,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(r.state, "failed");
        assert_eq!(r.checked_bytes, 65536);
        assert!(r.hits.is_empty());
    }
    #[test]
    fn patterns_validate_and_encode_without_losing_unicode() {
        assert_eq!(
            Pattern::Hex("46 41\n54 33 32".into()).bytes().unwrap(),
            b"FAT32"
        );
        for bad in ["", "0", "GG", "0xFF", "??"] {
            assert!(Pattern::Hex(bad.into()).bytes().is_err());
        }
        assert_eq!(
            Pattern::Utf8("中🙂".into()).bytes().unwrap(),
            "中🙂".as_bytes()
        );
        assert_eq!(
            Pattern::Utf16Le("🙂".into()).bytes().unwrap(),
            vec![0x3d, 0xd8, 0x42, 0xde]
        );
        assert!(Pattern::Utf8("x".repeat(MAX_PATTERN + 1)).bytes().is_err());
    }
    #[test]
    fn sector_bounds_and_short_reads_never_return_adjacent_or_partial_data() {
        let mut reader = Cursor::new(vec![7; 8192]);
        assert_eq!(
            read_sector(&mut reader, 8192, 4096, 1).unwrap(),
            vec![7; 4096]
        );
        assert_eq!(reader.position(), 8192);
        assert!(read_sector(&mut reader, 8192, 4096, 2).is_err());
        assert!(read_sector(&mut reader, 8192, 0, 0).is_err());
        assert!(read_sector(&mut reader, 8192, 512, u64::MAX).is_err());
        assert!(read_sector(&mut Cursor::new(vec![0; 511]), 512, 512, 0).is_err());
    }
    #[test]
    fn overlapping_and_cross_buffer_matches_are_exact_and_unique() {
        let mut bytes = vec![b'x'; CHUNK + 20];
        bytes[CHUNK - 2..CHUNK + 3].copy_from_slice(b"ababa");
        let result = search(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            b"aba",
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.hits, vec![(CHUNK - 2) as u64, CHUNK as u64]);
        assert_eq!(result.state, "completed");
        let result = search(
            &mut Cursor::new(b"aaaaa"),
            5,
            b"aaa",
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.hits, vec![0, 1, 2]);
    }
    #[test]
    fn hit_limit_cancel_and_read_failure_keep_partial_coverage_explicit() {
        let r = search(
            &mut Cursor::new(vec![0; 4096]),
            4096,
            &[0],
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(r.state, "limited");
        assert_eq!(r.checked_bytes, 1024);
        assert_eq!(r.hits.len(), MAX_HITS);
        let mut reader = Cursor::new(vec![0; 4096]);
        let flag = AtomicBool::new(false);
        let r = search(&mut reader, 4096, &[0], &flag, |_| {
            flag.store(true, Ordering::Relaxed)
        })
        .unwrap();
        assert_eq!(r.state, "canceled");
        assert_eq!(reader.position(), 0);
        let r = search(
            &mut Cursor::new(vec![0; 512]),
            1024,
            &[0],
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(r.state, "failed");
        assert_eq!(r.checked_bytes, 0);
        assert!(r.hits.is_empty());
    }
    #[test]
    fn search_never_reads_beyond_declared_scope_or_invents_long_matches() {
        let mut reader = Cursor::new(b"ABCABC");
        let r = search(&mut reader, 3, b"ABCABC", &AtomicBool::new(false), |_| {}).unwrap();
        assert!(r.hits.is_empty());
        assert_eq!(reader.position(), 3);
    }
}

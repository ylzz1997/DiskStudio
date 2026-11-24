//! Bounded, memory-only result queries. Display dates never authorize output metadata.
use super::{Catalog, Filter};
use crate::{file_metadata::Stamp, recovery::Candidate};
use serde::Deserialize;
use std::cmp::Ordering;
use time::{Date, Month, Time, UtcOffset};

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NameMatch {
    #[default]
    Contains,
    Exact,
    Prefix,
    Extension,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortBy {
    #[default]
    Scan,
    Name,
    Size,
    Extension,
    Modified,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DateField {
    Created,
    #[default]
    Modified,
    Accessed,
    Changed,
}

pub fn date(value: &str) -> Option<Date> {
    let b = value.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || b.iter()
            .enumerate()
            .any(|(i, c)| i != 4 && i != 7 && !c.is_ascii_digit())
    {
        return None;
    }
    let year = value[..4].parse::<i32>().ok()?;
    if year == 0 {
        return None;
    }
    Date::from_calendar_date(
        year,
        Month::try_from(value[5..7].parse::<u8>().ok()?).ok()?,
        value[8..].parse().ok()?,
    )
    .ok()
}

fn recorded_day(stamp: &Stamp) -> Option<Date> {
    if stamp.text.len() != 10 && stamp.text.as_bytes().get(10) != Some(&b' ') {
        return None;
    }
    date(stamp.text.get(..10)?)
}

/// Sorting may interpret saved display text, but must not populate instant_ns
/// (the separate fresh-record authority used by export_metadata).
fn instant(stamp: &Stamp) -> Option<i128> {
    let day = recorded_day(stamp)?;
    let clock = stamp.text.get(11..)?;
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let b = hms.as_bytes();
    if b.len() != 8
        || b[2] != b':'
        || b[5] != b':'
        || b.iter()
            .enumerate()
            .any(|(i, c)| i != 2 && i != 5 && !c.is_ascii_digit())
        || fraction.len() > 9
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (clock.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()? * 10u32.pow(9 - fraction.len() as u32)
    };
    let clock = Time::from_hms_nano(
        hms[..2].parse().ok()?,
        hms[3..5].parse().ok()?,
        hms[6..].parse().ok()?,
        nanos,
    )
    .ok()?;
    let zone = stamp.zone.as_bytes();
    let seconds = if zone == b"UTC" {
        0
    } else {
        if zone.len() != 9
            || &zone[..3] != b"UTC"
            || !matches!(zone[3], b'+' | b'-')
            || zone[6] != b':'
            || [4, 5, 7, 8].into_iter().any(|i| !zone[i].is_ascii_digit())
        {
            return None;
        }
        let hours = stamp.zone[4..6].parse::<i32>().ok()?;
        let minutes = stamp.zone[7..9].parse::<i32>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (hours * 3600 + minutes * 60) * if zone[3] == b'-' { -1 } else { 1 }
    };
    Some(
        day.with_time(clock)
            .assume_offset(UtcOffset::from_whole_seconds(seconds).ok()?)
            .unix_timestamp_nanos(),
    )
}

fn extension(name: &str) -> &str {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or("", |(_, ext)| ext)
}

pub struct Query<'a> {
    filter: &'a Filter,
    needle: String,
    from: Option<Date>,
    to: Option<Date>,
}
impl<'a> Query<'a> {
    pub fn new(query: &str, filter: &'a Filter, catalog: &Catalog) -> Result<Self, String> {
        if query.len() > 200 || query.chars().any(char::is_control) {
            return Err("搜索内容不得超过 200 字节或包含控制字符。".into());
        }
        filter.validate(catalog)?;
        let parse = |s: &Option<String>| {
            s.as_deref()
                .map(|v| date(v).ok_or("日期必须是有效的 YYYY-MM-DD。".to_owned()))
                .transpose()
        };
        let from = parse(&filter.date_from)?;
        let to = parse(&filter.date_to)?;
        if matches!((from, to), (Some(a), Some(b)) if a > b) {
            return Err("开始日期不能晚于结束日期。".into());
        }
        let needle = if filter.case_sensitive {
            query.into()
        } else {
            query.to_lowercase()
        };
        Ok(Self {
            filter,
            needle,
            from,
            to,
        })
    }
    pub fn matches(&self, c: &Candidate, catalog: &Catalog) -> bool {
        if !self.filter.matches(c, catalog) {
            return false;
        }
        if !self.needle.is_empty() {
            let name = if self.filter.case_sensitive {
                c.name.clone()
            } else {
                c.name.to_lowercase()
            };
            let matched = match self.filter.name_match {
                NameMatch::Contains => name.contains(&self.needle),
                NameMatch::Exact => name == self.needle,
                NameMatch::Prefix => name.starts_with(&self.needle),
                NameMatch::Extension => {
                    let needle = self.needle.strip_prefix('.').unwrap_or(&self.needle);
                    !needle.is_empty() && extension(&name) == needle
                }
            };
            if !matched {
                return false;
            }
        }
        if self.from.is_some() || self.to.is_some() {
            let stamp = match self.filter.date_field {
                DateField::Created => &c.metadata.created,
                DateField::Modified => &c.metadata.modified,
                DateField::Accessed => &c.metadata.accessed,
                DateField::Changed => &c.metadata.changed,
            };
            let Some(day) = stamp.as_ref().and_then(recorded_day) else {
                return false;
            };
            if self.from.is_some_and(|d| day < d) || self.to.is_some_and(|d| day > d) {
                return false;
            }
        }
        true
    }
}

pub fn ordered<'a>(
    files: &'a [Candidate],
    catalog: &Catalog,
    query: &str,
    filter: &Filter,
) -> Result<Vec<&'a Candidate>, String> {
    let query = Query::new(query, filter, catalog)?;
    let mut matched: Vec<_> = files.iter().filter(|c| query.matches(c, catalog)).collect();
    // Calculate potentially expensive keys once per file, never inside O(n log n) comparisons.
    enum Key {
        Id(u32),
        Text(String),
        Size(u64),
        Instant(Option<i128>),
    }
    let mut rows: Vec<_> = matched
        .drain(..)
        .map(|c| {
            let key = match filter.sort_by {
                SortBy::Scan => Key::Id(c.id),
                SortBy::Name => Key::Text(c.name.to_lowercase()),
                SortBy::Size => Key::Size(c.size),
                SortBy::Extension => Key::Text(extension(&c.name).to_lowercase()),
                SortBy::Modified => Key::Instant(c.metadata.modified.as_ref().and_then(instant)),
            };
            (c, key)
        })
        .collect();
    rows.sort_unstable_by(|(a, ka), (b, kb)| {
        // Unknown timezone or date-only values always follow comparable instants,
        // including descending order. Never assume the host timezone.
        let order = match (ka, kb) {
            (Key::Id(a), Key::Id(b)) => a.cmp(b),
            (Key::Size(a), Key::Size(b)) => a.cmp(b),
            (Key::Text(a), Key::Text(b)) => a.cmp(b),
            (Key::Instant(Some(a)), Key::Instant(Some(b))) => a.cmp(b),
            (Key::Instant(None), Key::Instant(Some(_))) => return Ordering::Greater,
            (Key::Instant(Some(_)), Key::Instant(None)) => return Ordering::Less,
            _ => Ordering::Equal,
        };
        let order = if matches!(filter.sort_direction, Direction::Desc) {
            order.reverse()
        } else {
            order
        };
        order.then(a.id.cmp(&b.id))
    });
    Ok(rows.into_iter().map(|(c, _)| c).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        file_metadata::Metadata,
        recovery::Locator,
        recovery_tree::{select_ids, ROOT},
    };
    fn candidate(id: u32) -> Candidate {
        Candidate {
            id,
            name: format!("file-{id}.txt"),
            size: u64::from(id),
            deleted: false,
            method: "NTFS 文件表".into(),
            note: String::new(),
            supported: true,
            directory: Some(ROOT.into()),
            directory_note: String::new(),
            metadata: Metadata::default(),
            locator: Locator::Signature {
                offset: 0,
                header: vec![],
                footer: vec![],
                content_hash: None,
                allocation_digest: None,
                fragments: None,
            },
        }
    }
    #[test]
    fn shared_offline_and_native_query_contract() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/recovery-query-cases.json")).unwrap();
        let files: Vec<_> = data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let mut c = candidate(f["id"].as_u64().unwrap() as u32);
                c.name = f["name"].as_str().unwrap().into();
                c.size = f["size"].as_str().unwrap().parse().unwrap();
                c.supported = f["supported"].as_bool().unwrap();
                c.metadata.modified = serde_json::from_value(f["modified"].clone()).unwrap();
                c
            })
            .collect();
        let catalog = Catalog::default();
        for case in data["cases"].as_array().unwrap() {
            let label = case["label"].as_str().unwrap();
            let result = serde_json::from_value::<Filter>(case["filter"].clone())
                .map_err(|e| e.to_string())
                .and_then(|filter| {
                    let query = case["query"].as_str().unwrap();
                    let results = ordered(&files, &catalog, query, &filter)?;
                    let mut selected = select_ids(&files, &catalog, query, &filter)?;
                    let mut selectable: Vec<_> = results
                        .iter()
                        .filter(|c| c.supported)
                        .map(|c| c.id)
                        .collect();
                    selected.sort_unstable();
                    selectable.sort_unstable();
                    assert_eq!(selected, selectable, "selection differs: {label}");
                    Ok(results.into_iter().map(|c| c.id).collect::<Vec<_>>())
                });
            if case["error"] == true {
                assert!(result.is_err(), "{label}");
            } else {
                let expected: Vec<u32> = serde_json::from_value(case["expected"].clone()).unwrap();
                assert_eq!(result.unwrap(), expected, "{label}");
            }
        }
        assert!(files.iter().all(|c| c
            .metadata
            .modified
            .as_ref()
            .is_none_or(|s| s.instant_ns.is_none())));
    }
    #[test]
    fn sorting_precedes_pagination_and_retains_all_identities() {
        let files: Vec<_> = (0..305).map(candidate).collect();
        let filter: Filter =
            serde_json::from_str(r#"{"sortBy":"size","sortDirection":"desc"}"#).unwrap();
        let results = ordered(&files, &Catalog::default(), "", &filter).unwrap();
        assert_eq!(results[0].id, 304);
        assert_eq!(results[100].id, 204);
        assert_eq!(results[300].id, 4);
        assert_eq!(results.last().unwrap().id, 0);
        assert_eq!(results.len(), files.len());
    }
    #[test]
    fn query_budget_dates_and_untrusted_display_metadata_fail_closed() {
        let catalog = Catalog::default();
        let filter = Filter::default();
        assert!(Query::new(&"中".repeat(66), &filter, &catalog).is_ok());
        assert!(Query::new(&"中".repeat(67), &filter, &catalog).is_err());
        for d in ["0001-01-01", "2000-02-29", "9999-12-31"] {
            assert!(date(d).is_some());
        }
        for d in ["1900-02-29", "2024-13-01", "2024-01-00", "２０２４-01-01"] {
            assert!(date(d).is_none());
        }
        for (text, zone) in [
            ("2024-01-01 24:00:00", "UTC"),
            ("2024-01-01 00:00:00.", "UTC"),
            ("2024-01-01 00:00:00", "UTC+24:00"),
            ("2024-01-01 00:00:00", "UTC+08:60"),
            ("2024-01-01 00:00:00", "UTC\n"),
        ] {
            let stamp = Stamp {
                text: text.into(),
                zone: zone.into(),
                precision: "1 s".into(),
                instant_ns: Some(1),
            };
            assert!(instant(&stamp).is_none());
        }
    }
}

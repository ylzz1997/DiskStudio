//! Shared by metadata display and partition management, to prevent joining two
//! independently read snapshots by a reusable Windows disk number alone.
use sha2::{Digest, Sha256};

pub fn clean(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
}

pub fn key(
    serial: Option<&str>,
    unique_id: Option<&str>,
    model: Option<&str>,
    size: u64,
    sector_size: Option<u64>,
    bus_type: Option<u16>,
) -> Option<String> {
    let serial = serial.map(clean).filter(|s| !s.is_empty())?;
    let bytes = serde_json::to_vec(&(
        serial,
        unique_id.map(clean),
        model.map(clean),
        size,
        sector_size,
        bus_type,
    ))
    .ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

pub fn layout_key(style: Option<u16>, mut parts: Vec<(u32, u64, u64, Option<String>)>) -> String {
    parts.sort_by_key(|p| (p.1, p.0));
    for p in &mut parts {
        p.3 =
            p.3.as_deref()
                .map(|s| clean(s).trim_matches(['{', '}']).to_ascii_lowercase());
    }
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(style, parts)).expect("plain metadata is serializable")
        )
    )
}

/// Windows includes the extended container as partition zero. Accept its
/// overlap only with complete data partitions strictly inside the container.
pub(crate) fn extended_mbr(
    sector: Option<u64>,
    size: u64,
    rows: impl IntoIterator<Item = (u32, u64, u64, Option<u16>)>,
) -> Option<(u64, u64)> {
    let sector = sector.filter(|n| matches!(n, 512 | 4096))?;
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_unstable_by_key(|p| p.1);
    let mut ids = std::collections::HashSet::new();
    let mut container = None;
    let mut data_end = sector;
    for &(number, start, len, ty) in &rows {
        let end = start.checked_add(len)?;
        if end > size || len == 0 || start < sector || (start | len) % sector != 0
            || !ids.insert(number)
        {
            return None;
        }
        if matches!(ty, Some(0x05 | 0x0f | 0x85)) {
            if number != 0 || container.replace((start, end)).is_some() {
                return None;
            }
        } else {
            if number == 0 || start < data_end {
                return None;
            }
            data_end = end;
        }
    }
    let (base, limit) = container?;
    rows.iter().filter(|r| r.0 != 0).all(|&(_, start, len, _)| {
        start + len <= base || start >= limit || (start >= base + sector && start + len <= limit)
    }).then_some((base, limit - base))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extended_mbr_allows_only_nested_non_overlapping_data_and_an_explicit_container() {
        let sectors = [
            (1, 4096, 8192, Some(6)),
            (0, 12288, 32768, Some(5)),
            (2, 16384, 8192, Some(6)),
            (3, 28672, 8192, Some(6)),
        ];
        assert_eq!(extended_mbr(Some(4096), 65536, sectors), Some((12288, 32768)));
        for (row, substitution) in [
            (1, (0, 12288, 32768, None)),
            (2, (2, 12288, 8192, Some(6))),
            (3, (3, 40960, 8192, Some(6))),
            (3, (3, 20480, 8192, Some(6))),
            (3, (3, 28672, 8192, Some(5))),
        ] {
            let mut bad = sectors;
            bad[row] = substitution;
            assert_eq!(extended_mbr(Some(4096), 65536, bad), None);
        }
    }
    #[test]
    fn replaced_disks_cannot_inherit_display_identity() {
        let key_for = |serial| {
            key(
                serial,
                Some("vendor-id"),
                Some("model"),
                123,
                Some(512),
                Some(11),
            )
        };
        assert_ne!(key_for(Some("one")), key_for(Some("two")));
        assert_eq!(key_for(Some("one \0")), key_for(Some("one")));
        assert_eq!(key_for(Some(" \0")), None);
    }
}

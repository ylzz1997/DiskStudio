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

#[cfg(test)]
mod tests {
    use super::*;
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

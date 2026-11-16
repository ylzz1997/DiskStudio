//! Parses a frozen guest-only failure report; never opens a device or infers a
//! path from screenshots. The only returned value is a constrained fixture leaf.
use serde_json::Value;
pub(super) const FAILED_HELPER: &str =
    "1f07a32e6ec62aee24c5641fb9a13dd55934e597cabd64b37fd794b617b9a000";
const DESKTOP: &str = "538afa2b0066c9bfce8436d403e0bbdbfd495b518fd39cbb5b7680f26824bfca";
const PRIOR: &str = "e363a460de281ab85d5769a50785ff1a34a282313c1409f389de0ea0e034f2d3";

pub(super) fn fixture_name(text: &str) -> Result<String, String> {
    if text.len() > 512 * 1024 {
        return Err("Previous log too large".into());
    }
    let events = text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let bad = || "Saved failed fixture identity or event sequence differs".to_string();
    if events.len() != 35
        || events[0]["event"] != "start"
        || events[0]["desktop"] != DESKTOP
        || events[0]["helper"] != FAILED_HELPER
        || events[1]["event"] != "prior-pass"
        || events[1]["fs"] != "FAT12"
        || events[1]["previousHelper"] != PRIOR
        || events[1]["sentinelRecheckedAfterRestart"] != true
    {
        return Err(bad());
    }
    let sequence = [
        "case-begin",
        "fixture-begin",
        "phase",
        "full-backup-pass",
        "phase",
        "incremental-backup-pass",
        "phase",
        "free-wipe-result",
        "phase",
        "case-pass",
    ];
    for (index, fs) in ["FAT16", "FAT32", "exFAT"].iter().enumerate() {
        let case = &events[2 + index * 10..12 + index * 10];
        if case
            .iter()
            .zip(sequence)
            .any(|(event, kind)| event["event"] != kind || event["fs"] != *fs)
            || case[2]["phase"] != "full-backup"
            || case[4]["phase"] != "incremental-backup"
            || case[6]["phase"] != "free-wipe"
            || case[8]["phase"] != "incremental-restore"
            || case[7]["report"]["state"] != "completed"
            || case[7]["report"]["diskLeftOffline"] != true
            || case[9]["gptUnchanged"] != true
            || case[9]["allLayoutsUnchanged"] != true
            || case[9]["sentinelBytes"] != 65553
        {
            return Err(bad());
        }
    }
    let tail = &events[32..];
    if tail[0]["event"] != "case-begin"
        || tail[0]["fs"] != "NTFS"
        || tail[1]["event"] != "fixture-begin"
        || tail[1]["fs"] != "NTFS"
        || tail[1]["partition"] != 2
        || tail[1]["offset"] != 16 * 1024 * 1024
        || tail[1]["size"] != 2u64 * 1024 * 1024 * 1024
        || tail[2]["event"] != "failed"
        || tail[2]["error"] != "Sentinel file missing or changed size"
        || tail[2]["automaticRetry"] != false
        || tail[2]["rollback"] != false
    {
        return Err(bad());
    }
    let name = tail[1]["sentinel"].as_str().ok_or_else(bad)?;
    if name.len() != 12
        || !name.ends_with(".BIN")
        || !name.as_bytes()[..8].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(bad());
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn events() -> Vec<Value> {
        let mut rows = vec![
            json!({"event":"start","desktop":DESKTOP,"helper":FAILED_HELPER}),
            json!({"event":"prior-pass","fs":"FAT12","previousHelper":PRIOR,"sentinelRecheckedAfterRestart":true}),
        ];
        for fs in ["FAT16", "FAT32", "exFAT"] {
            for event in ["case-begin", "fixture-begin"] {
                rows.push(json!({"event":event,"fs":fs}));
            }
            for (phase, done) in [
                ("full-backup", "full-backup-pass"),
                ("incremental-backup", "incremental-backup-pass"),
                ("free-wipe", "free-wipe-result"),
                ("incremental-restore", "case-pass"),
            ] {
                rows.push(json!({"event":"phase","fs":fs,"phase":phase}));
                rows.push(json!({"event":done,"fs":fs,"report":{"state":"completed","diskLeftOffline":true},"gptUnchanged":true,"allLayoutsUnchanged":true,"sentinelBytes":65553}));
            }
        }
        rows.extend([json!({"event":"case-begin","fs":"NTFS"}),json!({"event":"fixture-begin","fs":"NTFS","partition":2,"offset":16777216,"size":2147483648u64,"sentinel":"0123ABCD.BIN"}),json!({"event":"failed","error":"Sentinel file missing or changed size","automaticRetry":false,"rollback":false})]);
        rows
    }
    fn text(rows: &[Value]) -> String {
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn reads_exact_saved_leaf_and_refuses_paths_or_ambiguous_history() {
        let rows = events();
        assert_eq!(fixture_name(&text(&rows)).unwrap(), "0123ABCD.BIN");
        for bad in [
            "../ABCDE.BIN",
            "0123ABCD:BIN",
            "0123ABCD.bin",
            "Z123ABCD.BIN",
            "C:\\x.BIN",
        ] {
            let mut r = rows.clone();
            r[33]["sentinel"] = json!(bad);
            assert!(fixture_name(&text(&r)).is_err());
        }
        for (index, key, value) in [
            (0, "helper", json!("other")),
            (1, "sentinelRecheckedAfterRestart", json!(false)),
            (31, "allLayoutsUnchanged", json!(false)),
            (33, "partition", json!(1)),
            (33, "size", json!(1)),
            (34, "automaticRetry", json!(true)),
        ] {
            let mut r = rows.clone();
            r[index][key] = value;
            assert!(fixture_name(&text(&r)).is_err());
        }
        let mut r = rows.clone();
        r.push(json!({"event":"retried"}));
        assert!(fixture_name(&text(&r)).is_err());
        let mut r = rows;
        r.swap(2, 3);
        assert!(fixture_name(&text(&r)).is_err());
    }
}

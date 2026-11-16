use super::*;

fn nvme_log() -> Vec<u8> {
    let mut bytes = vec![0; 512];
    bytes[1..3].copy_from_slice(&300u16.to_le_bytes());
    bytes[3] = 100;
    bytes[4] = 10;
    bytes
}
fn value<'a>(health: &'a Health, label: &str) -> &'a str {
    &health
        .metrics
        .iter()
        .find(|m| m.label == label)
        .unwrap()
        .value
}
fn checksum(page: &mut [u8]) {
    page[511] = 0;
    page[511] = 0u8.wrapping_sub(page.iter().fold(0u8, |s, &b| s.wrapping_add(b)));
}
fn ata_log() -> Vec<u8> {
    let mut page = vec![0; 512];
    page[..2].copy_from_slice(&16u16.to_le_bytes());
    page[2] = 5;
    page[3] = 3;
    page[5] = 80;
    page[6] = 70;
    page[7..13].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
    checksum(&mut page);
    page
}
fn thresholds() -> Vec<u8> {
    let mut page = vec![0; 512];
    page[..2].copy_from_slice(&16u16.to_le_bytes());
    // Not the same slot as the attribute; association must be by ID.
    page[26] = 5;
    page[27] = 90;
    checksum(&mut page);
    page
}
#[test]
fn nvme_preserves_full_counters_and_does_not_invent_remaining_life() {
    let mut bytes = nvme_log();
    bytes[5] = 201;
    let large = (1u128 << 100) + 123;
    bytes[128..144].copy_from_slice(&large.to_le_bytes());
    bytes[32..48].copy_from_slice(&u128::MAX.to_le_bytes());
    let health = nvme(&bytes).unwrap();
    assert_eq!(value(&health, "综合温度"), "26.85");
    assert_eq!(value(&health, "寿命已使用"), "201");
    assert_eq!(value(&health, "通电时间"), large.to_string());
    assert_eq!(value(&health, "累计读取量"), u128::MAX.to_string());
    let json = serde_json::to_string(&health).unwrap();
    assert!(json.contains(&format!("\"{}\"", large)));
    assert!(!json.contains("剩余寿命"));
    bytes[5] = 255;
    assert_eq!(value(&nvme(&bytes).unwrap(), "寿命已使用"), "≥255");
}
#[test]
fn nvme_unknown_temperatures_and_warning_bits_are_not_healthy_defaults() {
    let mut bytes = nvme_log();
    bytes[0] = 0xff;
    bytes[1..3].fill(0);
    bytes[200..202].copy_from_slice(&305u16.to_le_bytes());
    bytes[202..204].fill(255);
    let health = nvme(&bytes).unwrap();
    assert_eq!(health.warnings.len(), 6);
    assert!(health.summary.contains("报告关键警告"));
    assert_eq!(value(&health, "综合温度"), "未知");
    assert_eq!(value(&health, "温度传感器 1"), "31.85");
    assert!(!health.metrics.iter().any(|m| m.label == "温度传感器 2"));
    assert!(nvme(&[0; 512]).is_err());
    for length in [0, 31, 511, 513] {
        assert!(nvme(&vec![0; length]).is_err());
    }
    bytes[3] = 101;
    assert!(nvme(&bytes).is_err());
    bytes[3] = 100;
    bytes[4] = 101;
    assert!(nvme(&bytes).is_err());
}
#[test]
fn nvme_data_unit_scaling_retains_exact_raw_value_and_rounding_note() {
    let mut bytes = nvme_log();
    bytes[32..48].copy_from_slice(&2_147_484u128.to_le_bytes());
    let health = nvme(&bytes).unwrap();
    let m = health
        .metrics
        .iter()
        .find(|m| m.label == "累计读取量")
        .unwrap();
    assert_eq!(m.value, "1.00");
    assert_eq!(m.unit, "TiB（约）");
    assert!(m.detail.as_ref().unwrap().contains("2147484"));
    assert!(m.detail.as_ref().unwrap().contains("向上取整"));
}
#[test]
fn nvme_descriptor_rejects_truncation_and_wrong_relative_offsets() {
    let mut bytes = vec![0; 560];
    for (offset, value) in [
        (0, 48u32),
        (4, 48),
        (8, 3),
        (12, 2),
        (16, 2),
        (24, 40),
        (28, 512),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[48..].copy_from_slice(&nvme_log());
    assert_eq!(nvme_descriptor(&bytes).unwrap(), &bytes[48..]);
    for length in [0, 47, 48, 559] {
        assert!(nvme_descriptor(&bytes[..length]).is_err());
    }
    for (offset, value) in [
        (0, 49u32),
        (4, 0),
        (8, 2),
        (12, 1),
        (16, 1),
        (24, 39),
        (24, u32::MAX),
        (28, 511),
        (28, u32::MAX),
    ] {
        let mut invalid = bytes.clone();
        invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(
            nvme_descriptor(&invalid).is_err(),
            "offset {offset}, value {value}"
        );
    }
}
#[test]
fn ata_thresholds_use_ids_and_preserve_vendor_defined_raw_bytes() {
    let attributes = ata_log();
    let thresholds = thresholds();
    let health = ata(&attributes, Some(&thresholds)).unwrap();
    let a = &health.attributes[0];
    assert_eq!((a.id, a.current, a.worst, a.flags), (5, 80, 70, 3));
    assert_eq!(a.threshold, Some(90));
    assert!(a.threshold_reached);
    assert_eq!(a.raw, "01 02 03 04 05 06");
    assert_eq!(health.warnings.len(), 1);
    let health = ata(&attributes, None).unwrap();
    assert_eq!(health.attributes[0].threshold, None);
    assert!(!health.attributes[0].threshold_reached);
    assert_eq!(health.summary, "SMART 属性已读取");
    assert!(!health
        .metrics
        .iter()
        .any(|m| m.label.contains("温度") || m.label.contains("通电")));
}
#[test]
fn ata_zero_normalized_values_or_thresholds_do_not_imply_pass_or_failure() {
    let mut attrs = ata_log();
    let mut limits = thresholds();
    attrs[5] = 0;
    checksum(&mut attrs);
    assert!(!ata(&attrs, Some(&limits)).unwrap().attributes[0].threshold_reached);
    attrs[5] = 80;
    checksum(&mut attrs);
    limits[27] = 0;
    checksum(&mut limits);
    assert!(!ata(&attrs, Some(&limits)).unwrap().attributes[0].threshold_reached);
    limits[27] = 80;
    checksum(&mut limits);
    assert!(ata(&attrs, Some(&limits)).unwrap().attributes[0].threshold_reached);
}
#[test]
fn ata_rejects_bad_checksums_duplicate_ids_and_empty_pages() {
    let mut attrs = ata_log();
    attrs[100] ^= 1;
    assert!(ata(&attrs, None).is_err());
    checksum(&mut attrs);
    attrs[14] = 5;
    checksum(&mut attrs);
    assert!(ata(&attrs, None).is_err());
    let attrs = ata_log();
    let mut limits = thresholds();
    limits[2] = 5;
    checksum(&mut limits);
    assert!(ata(&attrs, Some(&limits)).is_err());
    let mut empty = vec![0; 512];
    empty[0] = 16;
    checksum(&mut empty);
    assert!(ata(&empty, None).is_err());
    for length in [0, 511, 513] {
        assert!(ata(&vec![0; length], None).is_err());
    }
    let mut bad_revision = ata_log();
    bad_revision[..2].fill(0);
    checksum(&mut bad_revision);
    assert!(ata(&bad_revision, None).is_err());
}
#[test]
fn ata_driver_payload_and_typed_health_ipc_fail_closed() {
    let mut out = vec![0; 528];
    out[..4].copy_from_slice(&512u32.to_le_bytes());
    out[16..].copy_from_slice(&ata_log());
    assert_eq!(ata_payload(&out).unwrap(), &out[16..]);
    assert!(ata_payload(&out[..527]).is_err());
    for offset in [0, 4, 5] {
        let mut bad = out.clone();
        bad[offset] = 1;
        assert!(ata_payload(&bad).is_err());
    }
    use crate::health_tasks::Request;
    let target = "a".repeat(64);
    let valid = serde_json::json!({"target": target, "layoutKey": "layout"});
    assert!(serde_json::from_value::<Request>(valid.clone()).is_ok());
    assert!(serde_json::from_str::<Request>(r#"{"target":"id","layoutKey":"layout"}"#).is_err());
    for field in ["path", "command", "partition", "protocol"] {
        let mut unknown = valid.clone();
        unknown[field] = serde_json::json!("x");
        assert!(serde_json::from_value::<Request>(unknown).is_err());
    }
}

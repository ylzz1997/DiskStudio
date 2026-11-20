//! Fixed health-log decoding only. No device access, commands, or persistence.
#[cfg(test)]
#[path = "health_data_tests.rs"]
mod tests;
use crate::native_locale::Locale;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metric {
    pub label: String,
    pub value: String,
    pub unit: String,
    pub detail: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribute {
    pub id: u8,
    pub current: u8,
    pub worst: u8,
    pub threshold: Option<u8>,
    pub flags: u16,
    pub raw: String,
    pub threshold_reached: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub protocol: String,
    pub summary: String,
    pub warnings: Vec<String>,
    pub metrics: Vec<Metric>,
    pub attributes: Vec<Attribute>,
    pub notes: Vec<String>,
    pub nvme_diagnostics: Option<NvmeDiagnostics>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Reading<T> {
    Available { value: T },
    Unsupported,
    Unavailable { reason: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeDiagnostics {
    pub error_log: Reading<NvmeErrorLog>,
    pub self_test_log: Reading<NvmeSelfTestLog>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeErrorLog {
    pub controller_limit: usize,
    pub read_limit: usize,
    pub entries: Vec<NvmeErrorEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeErrorEntry {
    pub count: String,
    pub queue_id: Option<u16>,
    pub command_id: Option<u16>,
    pub status_type: u8,
    pub status_code: u8,
    pub do_not_retry: bool,
    pub namespace_id: Option<u32>,
    pub lba: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeSelfTestLog {
    pub current_operation: u8,
    pub completion_percent: Option<u8>,
    pub entries: Vec<NvmeSelfTestEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeSelfTestEntry {
    pub test_code: u8,
    pub outcome: u8,
    pub power_on_hours: String,
    pub namespace_id: Option<u32>,
    pub failing_lba: Option<String>,
    pub status_type: Option<u8>,
    pub status_code: Option<u8>,
}

const NVME_ERROR_ENTRY_BYTES: usize = 64;
const MAX_NVME_ERROR_ENTRIES: usize = 64;
const NVME_SELF_TEST_BYTES: usize = 4 + 20 * 28;

#[derive(Clone, Copy)]
pub(crate) enum NvmeQuery {
    Health,
    Controller,
    Errors(usize),
    SelfTest,
}
impl NvmeQuery {
    pub(crate) fn wire_fields(self) -> (u32, u32, usize) {
        match self {
            Self::Controller => (1, 1, 4096),
            Self::Health => (2, 2, 512),
            Self::Errors(entries) => (2, 1, (entries * NVME_ERROR_ENTRY_BYTES).max(512)),
            Self::SelfTest => (2, 6, NVME_SELF_TEST_BYTES),
        }
    }
}

pub(crate) struct NvmeController {
    firmware: Option<String>,
    version: u32,
    warning_temperature: u16,
    critical_temperature: u16,
    pub(crate) error_limit: usize,
    pub(crate) self_test: bool,
}
impl NvmeController {
    pub(crate) fn error_read_limit(&self) -> usize {
        self.error_limit.min(MAX_NVME_ERROR_ENTRIES)
    }

    pub(crate) fn add_metrics(&self, metrics: &mut Vec<Metric>) {
        let version = if self.version == 0 {
            "未知".into()
        } else {
            format!(
                "{}.{}.{}",
                self.version >> 16,
                (self.version >> 8) & 0xff,
                self.version & 0xff,
            )
        };
        metrics.extend([
            metric(
                "NVMe 固件版本",
                self.firmware.as_deref().unwrap_or("未知"),
                "",
            ),
            metric("NVMe 协议版本", version, ""),
            metric("警告温度阈值", temperature(self.warning_temperature), "°C"),
            metric("临界温度阈值", temperature(self.critical_temperature), "°C"),
        ]);
    }
}
fn metric(label: &str, value: impl ToString, unit: &str) -> Metric {
    Metric {
        label: label.into(),
        value: value.to_string(),
        unit: unit.into(),
        detail: None,
    }
}
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn qword(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn counter(bytes: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(bytes[offset..offset + 16].try_into().unwrap())
}
fn temperature(kelvin: u16) -> String {
    if matches!(kelvin, 0 | 65535) {
        "未知".into()
    } else {
        format!("{:.2}", f64::from(kelvin) - 273.15)
    }
}
fn data_units(locale: &Locale, label: &str, units: u128) -> Metric {
    // Multiplication may overflow even though the device's 128-bit counter is valid.
    let mut m = if let Some(bytes) = units.checked_mul(512_000) {
        let divisor = 1u128 << 40;
        metric(
            label,
            format!(
                "{}.{:02}",
                bytes / divisor,
                (bytes % divisor) * 100 / divisor
            ),
            "TiB（约）",
        )
    } else {
        metric(label, units, "数据单元")
    };
    m.detail = Some(locale.format(
        "原始值 {units}；每个数据单元为 512,000 字节，设备按单元向上取整。",
        &[("units", &units.to_string())],
    ));
    m
}

/// STORAGE_PROTOCOL_DATA_DESCRIPTOR, whose offsets are relative to its inner
/// 40-byte protocol structure, not the start of the outer 48-byte descriptor.
pub(crate) fn nvme_descriptor(bytes: &[u8], query: NvmeQuery) -> Result<&[u8], String> {
    let (data_type, data_value, required) = query.wire_fields();
    if bytes.len() < 48
        || dword(bytes, 0) != 48
        || dword(bytes, 4) != 48
        || dword(bytes, 8) != 3
        || dword(bytes, 12) != data_type
        || dword(bytes, 16) != data_value
    {
        return Err("NVMe 返回的诊断描述符无效。".into());
    }
    let offset = dword(bytes, 24) as usize;
    let length = dword(bytes, 28) as usize;
    let start = 8usize.checked_add(offset).ok_or("NVMe 日志偏移溢出")?;
    let end = start.checked_add(length).ok_or("NVMe 日志长度溢出")?;
    if offset < 40 || length < required || end > bytes.len() {
        return Err("NVMe 诊断数据不完整或超出返回范围。".into());
    }
    Ok(&bytes[start..start + required])
}

pub(crate) fn nvme_controller(bytes: &[u8]) -> Result<NvmeController, String> {
    if bytes.len() != 4096 || bytes.iter().all(|&byte| byte == 0) {
        return Err("NVMe 控制器识别数据无效。".into());
    }
    let firmware = std::str::from_utf8(&bytes[64..72])
        .ok()
        .map(|text| text.trim_matches([' ', '\0']))
        .filter(|value| {
            !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        })
        .map(str::to_owned);
    Ok(NvmeController {
        firmware,
        version: dword(bytes, 80),
        warning_temperature: word(bytes, 266),
        critical_temperature: word(bytes, 268),
        error_limit: usize::from(bytes[262]) + 1,
        self_test: word(bytes, 256) & (1 << 4) != 0,
    })
}

pub(crate) fn nvme_errors(
    bytes: &[u8],
    controller: &NvmeController,
) -> Result<NvmeErrorLog, String> {
    let read_limit = controller.error_read_limit();
    let range = bytes
        .get(..read_limit * NVME_ERROR_ENTRY_BYTES)
        .ok_or("NVMe 错误记录不完整。")?;
    let entries = range
        .chunks_exact(NVME_ERROR_ENTRY_BYTES)
        .filter_map(|entry| {
            let count = qword(entry, 0);
            if count == 0 {
                return None;
            }
            let queue = word(entry, 8);
            let command = word(entry, 10);
            let status = word(entry, 12);
            let status_type = ((status >> 9) & 7) as u8;
            let namespace = dword(entry, 24);
            let lba = qword(entry, 16);
            let has_namespace = !matches!(namespace, 0 | u32::MAX);
            Some(NvmeErrorEntry {
                count: count.to_string(),
                queue_id: (queue != u16::MAX).then_some(queue),
                command_id: (command != u16::MAX).then_some(command),
                status_type,
                status_code: ((status >> 1) & 0xff) as u8,
                do_not_retry: status & (1 << 15) != 0,
                namespace_id: has_namespace.then_some(namespace),
                lba: (has_namespace && (lba != 0 || status_type == 2)).then(|| lba.to_string()),
            })
        })
        .collect();
    Ok(NvmeErrorLog {
        controller_limit: controller.error_limit,
        read_limit,
        entries,
    })
}

pub(crate) fn nvme_self_tests(bytes: &[u8]) -> Result<NvmeSelfTestLog, String> {
    if bytes.len() != NVME_SELF_TEST_BYTES || bytes.iter().all(|&byte| byte == 0) {
        return Err("NVMe 自检日志长度或字段无效。".into());
    }
    let current_operation = bytes[0] & 0xf;
    let completion = bytes[1] & 0x7f;
    if current_operation != 0 && completion > 100 {
        return Err("NVMe 自检进度无效。".into());
    }
    let entries = bytes[4..]
        .chunks_exact(28)
        .filter_map(|entry| {
            let outcome = entry[0] & 0xf;
            if outcome == 0xf {
                return None;
            }
            let valid = entry[2];
            Some(NvmeSelfTestEntry {
                test_code: entry[0] >> 4,
                outcome,
                power_on_hours: qword(entry, 4).to_string(),
                namespace_id: (valid & 1 != 0).then(|| dword(entry, 12)),
                failing_lba: (valid & 2 != 0).then(|| qword(entry, 16).to_string()),
                status_type: (valid & 4 != 0).then_some(entry[24] & 7),
                status_code: (valid & 8 != 0).then_some(entry[25]),
            })
        })
        .collect();
    Ok(NvmeSelfTestLog {
        current_operation,
        completion_percent: (current_operation != 0).then_some(completion),
        entries,
    })
}
pub fn nvme(bytes: &[u8], locale: &Locale) -> Result<Health, String> {
    if bytes.len() != 512 || bytes.iter().all(|&b| b == 0) || bytes[3] > 100 || bytes[4] > 100 {
        return Err("NVMe 健康日志长度或字段无效，不能据此判断健康状态。".into());
    }
    let warning = bytes[0];
    let mut warnings = vec![];
    for (bit, text) in [
        (0, "备用空间低于阈值"),
        (1, "温度超出设备阈值"),
        (2, "设备可靠性降低"),
        (3, "介质已进入只读状态"),
        (4, "易失性存储备份装置异常"),
    ] {
        if warning & (1 << bit) != 0 {
            warnings.push(text.into());
        }
    }
    if warning & 0xe0 != 0 {
        warnings.push(locale.format(
            "设备还报告未解析的关键警告位 0x{bits}",
            &[("bits", &format!("{:02X}", warning & 0xe0))],
        ));
    }
    let mut metrics = vec![
        metric("综合温度", temperature(word(bytes, 1)), "°C"),
        metric(
            "寿命已使用",
            if bytes[5] == 255 {
                "≥255".into()
            } else {
                bytes[5].to_string()
            },
            "%",
        ),
        metric("可用备用空间", bytes[3], "%"),
        metric("备用空间阈值", bytes[4], "%"),
        data_units(locale, "累计读取量", counter(bytes, 32)),
        data_units(locale, "累计写入量", counter(bytes, 48)),
    ];
    for (offset, label, unit) in [
        (64, "读取命令", "次"),
        (80, "写入命令", "次"),
        (96, "控制器忙碌时间", "分钟"),
        (112, "通电次数", "次"),
        (128, "通电时间", "小时"),
        (144, "非正常关机", "次"),
        (160, "介质与数据完整性错误", "次"),
        (176, "错误日志条目", "条"),
    ] {
        metrics.push(metric(label, counter(bytes, offset), unit));
    }
    metrics.push(metric("温度警告累计时间", dword(bytes, 192), "分钟"));
    metrics.push(metric("临界温度累计时间", dword(bytes, 196), "分钟"));
    for i in 0..8 {
        let value = word(bytes, 200 + i * 2);
        if !matches!(value, 0 | 65535) {
            metrics.push(metric(
                &locale.format("温度传感器 {number}", &[("number", &(i + 1).to_string())]),
                temperature(value),
                "°C",
            ));
        }
    }
    Ok(Health {
        protocol: "nvme".into(),
        summary: if warning == 0 {
            "设备未报告关键警告".into()
        } else {
            "设备报告关键警告".into()
        },
        warnings,
        metrics,
        attributes: vec![],
        notes: vec![
            "这是控制器报告的健康日志；共享控制器的命名空间可能显示相同累计值。".into(),
            "寿命已使用为厂商估计，可超过 100%；达到 100% 不等于设备已经故障。".into(),
            "没有关键警告不保证介质完好；本次读取没有执行自检或坏道扫描。".into(),
        ],
        nvme_diagnostics: None,
    })
}
pub fn ata_payload(bytes: &[u8]) -> Result<&[u8], String> {
    if bytes.len() < 528 || dword(bytes, 0) != 512 || bytes[4] != 0 || bytes[5] != 0 {
        return Err("ATA SMART 请求失败或返回数据不完整。".into());
    }
    Ok(&bytes[16..528])
}
fn ata_page(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() != 512
        || matches!(word(bytes, 0), 0 | 65535)
        || bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) != 0
    {
        return Err("ATA SMART 页长度、版本或校验和无效。".into());
    }
    Ok(())
}
pub fn ata(
    attributes: &[u8],
    thresholds: Option<&[u8]>,
    locale: &Locale,
) -> Result<Health, String> {
    ata_page(attributes)?;
    let mut limits = [None; 256];
    if let Some(page) = thresholds {
        ata_page(page)?;
        for entry in page[2..362].chunks_exact(12) {
            let id = entry[0] as usize;
            if id == 0 {
                continue;
            }
            if limits[id].is_some() {
                return Err("ATA SMART 阈值包含重复属性编号。".into());
            }
            limits[id] = Some(entry[1]);
        }
    }
    let mut seen = HashSet::new();
    let mut values = vec![];
    for entry in attributes[2..362].chunks_exact(12) {
        let id = entry[0];
        if id == 0 {
            continue;
        }
        if !seen.insert(id) {
            return Err("ATA SMART 包含重复属性编号。".into());
        }
        let threshold = limits[id as usize];
        values.push(Attribute {
            id,
            flags: word(entry, 1),
            current: entry[3],
            worst: entry[4],
            threshold,
            raw: entry[5..11]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
            threshold_reached: threshold.is_some_and(|t| t > 0 && entry[3] > 0 && entry[3] <= t),
        });
    }
    if values.is_empty() {
        return Err("ATA SMART 没有可用属性，不能据此判断健康状态。".into());
    }
    let warnings: Vec<String> = values
        .iter()
        .filter(|a| a.threshold_reached)
        .map(|a| {
            locale.format(
                "属性 {id} 的当前归一化值已达到设备报告的阈值",
                &[("id", &format!("{:02X}", a.id))],
            )
        })
        .collect();
    let summary = if warnings.is_empty() {
        "SMART 属性已读取".into()
    } else {
        locale.format(
            "{count} 项属性达到阈值",
            &[("count", &warnings.len().to_string())],
        )
    };
    Ok(Health { protocol:"ata".into(), summary, warnings, metrics:vec![metric("属性页版本",word(attributes,0),"")],attributes:values,
        nvme_diagnostics: None,
        notes:vec!["ATA 属性编号、原始值及单位由厂商定义；这里按设备原始字节顺序展示，不推断温度、小时或寿命。".into(),
            "归一化值与阈值比较仅供判断对应属性，不等同于设备整体健康结论。未提供阈值的项目保持未知。".into(),
            "本次读取没有启用 SMART、改变设备设置或执行自检。".into()] })
}

//! Fixed, query-only SMART commands on a freshly verified device handle.
use crate::{
    data_tasks::Selection,
    data_windows as platform,
    health_data::{self, Health, NvmeDiagnostics, NvmeQuery, Reading},
    native_locale::{self, Locale},
};
use std::fs::File;
use windows_sys::Win32::System::Ioctl::*;

fn put(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn nvme_request(query: NvmeQuery) -> Vec<u8> {
    let (data_type, request_value, length) = query.wire_fields();
    let mut bytes = vec![0; 48 + length];
    put(&mut bytes, 0, StorageAdapterProtocolSpecificProperty as u32);
    put(&mut bytes, 4, PropertyStandardQuery as u32);
    put(&mut bytes, 8, ProtocolTypeNvme as u32);
    put(&mut bytes, 12, data_type);
    put(&mut bytes, 16, request_value); // Controller scope.
    put(&mut bytes, 24, 40);
    put(&mut bytes, 28, length as u32);
    if data_type == NVMeDataTypeLogPage as u32 {
        put(&mut bytes, 44, 1); // Retain any asynchronous event.
    }
    bytes
}

fn nvme_page(file: &File, query: NvmeQuery) -> Result<Vec<u8>, String> {
    let input = nvme_request(query);
    let mut output = vec![0; input.len()];
    let n = platform::control(file, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut output)?;
    let returned = output.get(..n).ok_or("NVMe 返回长度超过缓冲区")?;
    Ok(health_data::nvme_descriptor(returned, query)?.to_vec())
}

fn reading<T>(result: Result<T, String>, locale: &Locale) -> Reading<T> {
    match result {
        Ok(value) => Reading::Available { value },
        Err(reason) => Reading::Unavailable {
            reason: locale.text(&reason).to_owned(),
        },
    }
}

fn nvme(file: &File, locale: &Locale) -> Result<Health, String> {
    let health_page = nvme_page(file, NvmeQuery::Health).map_err(|e| {
        locale.format(
            "当前设备或驱动未能提供 NVMe 健康日志：{error}",
            &[("error", locale.text(&e))],
        )
    })?;
    let mut health = health_data::nvme(&health_page, locale)?;
    health.nvme_diagnostics = Some(
        match nvme_page(file, NvmeQuery::Controller)
            .and_then(|page| health_data::nvme_controller(&page))
        {
            Ok(controller) => {
                controller.add_metrics(&mut health.metrics);
                let error_log = reading(
                    nvme_page(file, NvmeQuery::Errors(controller.error_read_limit()))
                        .and_then(|page| health_data::nvme_errors(&page, &controller)),
                    locale,
                );
                let self_test_log = if controller.self_test {
                    reading(
                        nvme_page(file, NvmeQuery::SelfTest)
                            .and_then(|page| health_data::nvme_self_tests(&page)),
                        locale,
                    )
                } else {
                    Reading::Unsupported
                };
                NvmeDiagnostics {
                    error_log,
                    self_test_log,
                }
            }
            Err(err) => {
                let reason = locale.format(
                    "NVMe 控制器识别不可用：{error}",
                    &[("error", locale.text(&err))],
                );
                NvmeDiagnostics {
                    error_log: Reading::Unavailable {
                        reason: reason.clone(),
                    },
                    self_test_log: Reading::Unavailable { reason },
                }
            }
        },
    );
    Ok(health)
}
#[derive(Clone, Copy)]
enum AtaPage {
    Attributes,
    Thresholds,
}
fn ata_request(page: AtaPage) -> [u8; 32] {
    let mut bytes = [0; 32];
    put(&mut bytes, 0, 512);
    bytes[4] = match page {
        AtaPage::Attributes => READ_ATTRIBUTES as u8,
        AtaPage::Thresholds => READ_THRESHOLDS as u8,
    };
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[7] = SMART_CYL_LOW as u8;
    bytes[8] = SMART_CYL_HI as u8;
    bytes[10] = SMART_CMD as u8;
    // bDriveNumber is opaque and ignored by Windows; the verified handle selects
    // the device. No enable/disable, self-test, autosave or write-log command.
    bytes
}
fn ata_read(file: &File, page: AtaPage) -> Result<Vec<u8>, String> {
    let mut output = [0; 528];
    let n = platform::control(file, SMART_RCV_DRIVE_DATA, &ata_request(page), &mut output)?;
    Ok(health_data::ata_payload(output.get(..n).ok_or("ATA 返回长度超过缓冲区")?)?.to_vec())
}
fn ata(file: &File, locale: &Locale) -> Result<Health, String> {
    let mut version = [0; 24];
    let n = platform::control(file, SMART_GET_VERSION, &[], &mut version)?;
    if n < 24 || u32::from_le_bytes(version[4..8].try_into().unwrap()) & CAP_SMART_CMD == 0 {
        return Err("当前驱动未报告 ATA SMART 读取支持；未改变设备设置。".into());
    }
    let attributes = ata_read(file, AtaPage::Attributes).map_err(|e| {
        locale.format(
            "无法读取 ATA SMART 属性；设备可能未启用 SMART 或驱动不支持：{error}",
            &[("error", locale.text(&e))],
        )
    })?;
    let thresholds = ata_read(file, AtaPage::Thresholds);
    // Missing optional thresholds never suppress a readable attribute page or
    // silently turn absent data into a passing health assessment.
    let mut health = match &thresholds {
        Ok(page) => match health_data::ata(&attributes, Some(page), locale) {
            Ok(health) => health,
            Err(e) => {
                let mut health = health_data::ata(&attributes, None, locale)?;
                health
                    .notes
                    .push(locale.format("阈值页未采用：{error}", &[("error", locale.text(&e))]));
                health
            }
        },
        Err(e) => {
            let mut health = health_data::ata(&attributes, None, locale)?;
            health
                .notes
                .push(locale.format("阈值不可用：{error}", &[("error", locale.text(&e))]));
            health
        }
    };
    health.metrics.push(health_data::Metric {
        label: "SMART 接口版本".into(),
        value: format!("{}.{}", version[0], version[1]),
        unit: String::new(),
        detail: None,
    });
    Ok(health)
}
pub fn read(selection: &Selection) -> Result<Health, String> {
    let resolved = platform::resolve(selection, false, false)?;
    let bus = resolved.disk.bus_type.ok_or("无法确认设备连接方式")?;
    if !matches!(bus, 3 | 11 | 17) {
        return Err("当前连接方式尚不支持 SMART 读取。USB 桥接、RAID 和其他协议需要对应的受控读取实现；不能据此判断设备健康。".into());
    }
    // The legacy ATA SMART_RCV_DRIVE_DATA IOCTL requires READ|WRITE handle access
    // even for read commands. Only the two fixed reads above are ever sent.
    let file = platform::device(&resolved, bus != 17)?;
    let mut descriptor = vec![0; 65536];
    let n = platform::control(
        &file,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &[0; 12],
        &mut descriptor,
    )?;
    if n < 36
        || n > descriptor.len()
        || u32::from_le_bytes(descriptor[28..32].try_into().unwrap()) != u32::from(bus)
    {
        return Err("设备句柄的连接方式与元数据不一致，已拒绝 SMART 请求。".into());
    }
    let locale = native_locale::current();
    let health = if bus == 17 {
        nvme(&file, &locale)?
    } else {
        ata(&file, &locale)?
    };
    let fresh = platform::resolve(selection, false, false)?;
    if fresh.disk.number != resolved.disk.number
        || fresh.disk.bus_type != Some(bus)
        || fresh.disk.size != resolved.disk.size
        || fresh.disk.logical_sector_size != resolved.disk.logical_sector_size
    {
        return Err("健康信息读取后设备身份或参数发生变化，请刷新后重试。".into());
    }
    Ok(health)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_query_buffers_match_sdk_layouts_and_never_encode_mutations() {
        assert_eq!(std::mem::size_of::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>(), 48);
        assert_eq!(std::mem::size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>(), 40);
        assert_eq!(
            std::mem::offset_of!(STORAGE_PROPERTY_QUERY, AdditionalParameters),
            8
        );
        assert_eq!(std::mem::offset_of!(SENDCMDINPARAMS, bBuffer), 32);
        assert_eq!(std::mem::offset_of!(SENDCMDOUTPARAMS, bBuffer), 16);
        assert_eq!(std::mem::size_of::<GETVERSIONINPARAMS>(), 24);
        assert_eq!(
            std::mem::offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataRequestSubValue4),
            36
        );
        for (query, kind, page, size) in [
            (NvmeQuery::Health, NVMeDataTypeLogPage, 2, 512),
            (NvmeQuery::Controller, NVMeDataTypeIdentify, 1, 4096),
            (NvmeQuery::Errors(1), NVMeDataTypeLogPage, 1, 512),
            (NvmeQuery::Errors(64), NVMeDataTypeLogPage, 1, 4096),
            (NvmeQuery::SelfTest, NVMeDataTypeLogPage, 6, 564),
        ] {
            let nvme = nvme_request(query);
            let field = |offset| u32::from_le_bytes(nvme[offset..offset + 4].try_into().unwrap());
            assert_eq!(field(0), 49);
            assert_eq!(field(8), ProtocolTypeNvme as u32);
            assert_eq!(field(12), kind as u32);
            assert_eq!(field(16), page);
            assert_eq!(field(24), 40);
            assert_eq!(field(28), size);
            assert_eq!(nvme.len(), 48 + size as usize);
            assert_eq!(field(44), u32::from(kind == NVMeDataTypeLogPage));
            assert!(nvme[32..44].iter().all(|&b| b == 0));
            assert!(nvme[48..].iter().all(|&b| b == 0));
        }
        for (page, feature) in [(AtaPage::Attributes, 0xd0), (AtaPage::Thresholds, 0xd1)] {
            let bytes = ata_request(page);
            assert_eq!(&bytes[4..12], &[feature, 1, 1, 0x4f, 0xc2, 0, 0xb0, 0]);
            assert!(bytes[12..].iter().all(|&b| b == 0));
        }
    }
}

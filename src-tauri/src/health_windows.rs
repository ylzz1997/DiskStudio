//! Fixed, query-only SMART commands on a freshly verified device handle.
use crate::{
    data_tasks::Selection,
    data_windows as platform,
    health_data::{self, Health},
    native_locale::{self, Locale},
};
use std::fs::File;
use windows_sys::Win32::System::Ioctl::*;

fn put(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn nvme_request() -> [u8; 560] {
    let mut bytes = [0; 560];
    put(&mut bytes, 0, StorageAdapterProtocolSpecificProperty as u32);
    put(&mut bytes, 4, PropertyStandardQuery as u32);
    put(&mut bytes, 8, ProtocolTypeNvme as u32);
    put(&mut bytes, 12, NVMeDataTypeLogPage as u32);
    put(&mut bytes, 16, 2); // NVME_LOG_PAGE_HEALTH_INFO, controller/global scope.
    put(&mut bytes, 24, 40);
    put(&mut bytes, 28, 512);
    bytes
}
fn nvme(file: &File, locale: &Locale) -> Result<Health, String> {
    let input = nvme_request();
    let mut output = [0; 560];
    let n = platform::control(file, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut output).map_err(
        |e| {
            locale.format(
                "当前设备或驱动未能提供 NVMe 健康日志：{error}",
                &[("error", locale.text(&e))],
            )
        },
    )?;
    let returned = output.get(..n).ok_or("NVMe 返回长度超过缓冲区")?;
    health_data::nvme(health_data::nvme_descriptor(returned)?, locale)
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
        let nvme = nvme_request();
        assert_eq!(u32::from_le_bytes(nvme[0..4].try_into().unwrap()), 49);
        assert_eq!(&nvme[8..20], &[3, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0]);
        assert!(nvme[32..].iter().all(|&b| b == 0));
        for (page, feature) in [(AtaPage::Attributes, 0xd0), (AtaPage::Thresholds, 0xd1)] {
            let bytes = ata_request(page);
            assert_eq!(&bytes[4..12], &[feature, 1, 1, 0x4f, 0xc2, 0, 0xb0, 0]);
            assert!(bytes[12..].iter().all(|&b| b == 0));
        }
    }
}

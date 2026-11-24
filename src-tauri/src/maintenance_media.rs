//! Inspect the supplied WinPE boot files without mounting or interpreting the WIM.
use crate::{boot_inspection, fat_recovery::boot_files::Volume};
use serde::{Deserialize, Serialize};
use std::io::Cursor;

pub(crate) const WIM_PATH: &str = "sources/boot.wim";
pub(crate) const EFI_BCD: &str = "efi/microsoft/boot/bcd";
pub(crate) const BIOS_BCD: &str = "boot/bcd";
pub(crate) const FALLBACK: &str = "efi/boot/bootx64.efi";
pub(crate) const SDI_PATH: &str = "boot/boot.sdi";
pub(crate) const APP_PATH: &str = "DiskStudio.exe";
pub(crate) const APP_KEY: &str = "diskstudio.exe";

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Mode {
    Uefi,
    UefiAndBios,
}
impl Mode {
    pub(crate) fn bios(self) -> bool {
        self == Self::UefiAndBios
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub bios: bool,
    pub wim_image: u32,
    pub file_count: usize,
    pub wim_bytes: u64,
    pub adk: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb: Option<UsbPreview>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsbPreview {
    pub new_files: usize,
    pub reused_files: usize,
    pub new_bytes: u64,
    pub files: Vec<UsbFile>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsbFile {
    pub path: String,
    pub reused: bool,
}

pub(crate) fn wim_index(header: &[u8], size: u64) -> Result<u32, String> {
    let unsupported = "boot.wim 的头、分卷信息或可启动映像索引无效。";
    if header.len() < 148 || &header[..8] != b"MSWIM\0\0\0" {
        return Err(unsupported.into());
    }
    let u32_at = |at| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
    let u64_at = |at| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
    let header_size = u64::from(u32_at(8));
    let boot_index = u32_at(120);
    let image_count = u32_at(44);
    let resource_bytes = u64_at(96) & 0x00ff_ffff_ffff_ffff;
    let resource_offset = u64_at(104);
    if !(148..=4096).contains(&header_size)
        || header_size > size
        || header[40..44] != [1, 0, 1, 0]
        || boot_index == 0
        || boot_index > image_count
        || resource_bytes == 0
        || resource_offset < header_size
        || resource_offset
            .checked_add(resource_bytes)
            .is_none_or(|end| end > size)
    {
        return Err(unsupported.into());
    }
    Ok(boot_index)
}

pub(crate) fn check_efi(boot_image: &[u8]) -> Result<(), String> {
    if boot_image.len() < 512
        || boot_image[11..13] != [0, 2]
        || boot_image[510..512] != [0x55, 0xaa]
    {
        return Err("EFI 光盘启动映像不是可核对的 FAT 启动卷。".into());
    }
    let mut reader = Cursor::new(boot_image);
    let volume = Volume::open(&mut reader)?;
    let file = volume
        .read_file(
            &mut reader,
            &["EFI", "BOOT", "BOOTX64.EFI"],
            16 * 1024 * 1024,
        )?
        .ok_or("EFI 光盘启动映像中缺少 BOOTX64.EFI。")?;
    if boot_inspection::pe_machine(&file, &[10]) != Ok(0x8664) {
        return Err("EFI 光盘启动映像中的加载器不是 x64 EFI 应用。".into());
    }
    Ok(())
}

pub(crate) fn check_fallback(data: &[u8]) -> Result<(), String> {
    if boot_inspection::pe_machine(data, &[10]) != Ok(0x8664) {
        return Err("WinPE 介质需要 x64 EFI 回退加载器。".into());
    }
    Ok(())
}

pub(crate) fn check_bios(image: &[u8]) -> Result<(), String> {
    if !matches!(image.len(), 2048 | 4096)
        || !image.ends_with(&[0x55, 0xaa])
        || !image.windows(7).any(|word| word == b"CDBOOT:")
    {
        return Err("找不到可核对的 Windows BIOS 光盘启动映像。".into());
    }
    Ok(())
}

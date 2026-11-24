//! Portable machine configuration for a checked, offline Windows boot disk.
use crate::image_convert::{Format, Identity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Platform {
    Vmware,
    VirtualBox,
}
impl Platform {
    pub(crate) fn format(self) -> Format {
        match self {
            Self::Vmware => Format::VmdkSparse,
            Self::VirtualBox => Format::VdiDynamic,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vmware => "VMware",
            Self::VirtualBox => "VirtualBox",
        }
    }
    pub(crate) fn disk_name(self) -> &'static str {
        match self {
            Self::Vmware => "System.vmdk",
            Self::VirtualBox => "System.vdi",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Firmware {
    Bios,
    Uefi,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Controller {
    Sata,
    Nvme,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Options {
    pub platform: Platform,
    pub name: String,
    pub cpus: u8,
    pub memory_mib: u32,
}
impl Options {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || self.name.len() > 48
            || self.name.trim() != self.name
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'-'))
            || crate::recovery_export::reserved(&self.name)
        {
            return Err(
                "虚拟机名称须为 1–48 个英文字母、数字、空格、连字符或下划线，首尾不可有空格。"
                    .into(),
            );
        }
        if !(1..=32).contains(&self.cpus)
            || !(1024..=65536).contains(&self.memory_mib)
            || self.memory_mib % 4 != 0
        {
            return Err(
                "虚拟机需要 1–32 个处理器及 1024–65536 MiB 内存，内存须按 4 MiB 对齐。".into(),
            );
        }
        Ok(())
    }
    pub(crate) fn config_name(&self) -> String {
        let extension = match self.platform {
            Platform::Vmware => "vmx",
            Platform::VirtualBox => "vbox",
        };
        format!("{}.{}", self.name, extension)
    }
    pub(crate) fn config(
        &self,
        firmware: Firmware,
        controller: Controller,
        machine: Uuid,
        identity: &Identity,
    ) -> Result<Vec<u8>, String> {
        self.validate()?;
        if identity.file_name != self.platform.disk_name()
            || machine.is_nil()
            || identity.created == [0; 16]
            || (controller == Controller::Nvme && firmware != Firmware::Uefi)
        {
            return Err("虚拟机硬件、固件或磁盘标识不一致。".into());
        }
        let name = &self.name;
        let cpus = self.cpus;
        let memory = self.memory_mib;
        let disk = &identity.file_name;
        let template = match self.platform {
            Platform::Vmware => {
                let firmware = if firmware == Firmware::Uefi {
                    "efi"
                } else {
                    "bios"
                };
                let controller = if controller == Controller::Nvme {
                    "nvme"
                } else {
                    "sata"
                };
                format!(
                    r#".encoding = "UTF-8"
config.version = "8"
virtualHW.version = "19"
displayName = "{name}"
guestOS = "windows9-64"
numvcpus = "{cpus}"
memsize = "{memory}"
firmware = "{firmware}"
{controller}0.present = "TRUE"
{controller}0:0.present = "TRUE"
{controller}0:0.fileName = "{disk}"
ethernet0.present = "FALSE"
usb.present = "FALSE"
isolation.tools.copy.disable = "TRUE"
isolation.tools.paste.disable = "TRUE"
isolation.tools.hgfs.disable = "TRUE"
"#
                )
            }
            Platform::VirtualBox => {
                let firmware = if firmware == Firmware::Uefi {
                    "EFI"
                } else {
                    "BIOS"
                };
                let (bus, kind) = if controller == Controller::Nvme {
                    ("NVMe", "NVMe")
                } else {
                    ("SATA", "AHCI")
                };
                // VDI stores the UUID's first three integer fields in little endian.
                let disk_uuid = Uuid::from_bytes_le(identity.created);
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<VirtualBox xmlns="http://www.virtualbox.org/" version="1.20-windows">
  <Machine uuid="{{{machine}}}" name="{name}" OSType="Windows10_64">
    <MediaRegistry>
      <HardDisks><HardDisk uuid="{{{disk_uuid}}}" location="{disk}" format="VDI" type="Normal"/></HardDisks>
    </MediaRegistry>
    <Platform architecture="x86">
      <CPU count="{cpus}"/>
    </Platform>
    <Hardware>
      <Memory RAMSize="{memory}"/>
      <Firmware type="{firmware}"><ACPI enabled="true"/><IOAPIC enabled="true"/></Firmware>
      <Boot><Order position="1" device="HardDisk"/></Boot>
      <Display controller="VBoxSVGA" VRAMSize="128"/>
    </Hardware>
    <StorageControllers>
      <StorageController name="{bus}" type="{kind}" PortCount="1" Bootable="true">
        <AttachedDevice type="HardDisk" port="0" device="0"><Image uuid="{{{disk_uuid}}}"/></AttachedDevice>
      </StorageController>
    </StorageControllers>
  </Machine>
</VirtualBox>
"#
                )
            }
        };
        Ok(template.into_bytes())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub platform: Platform,
    pub name: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub firmware: Firmware,
    pub controller: Controller,
    pub driver: &'static str,
    pub adds_efi_fallback: bool,
    pub config: String,
    pub disk: &'static str,
}

#[cfg(test)]
#[path = "system_virtualization_tests.rs"]
mod tests;

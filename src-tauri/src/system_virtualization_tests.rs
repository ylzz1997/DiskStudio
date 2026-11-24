use super::*;
use crate::image_convert::{Encoder, Options as ImageOptions, CHUNK};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Seek, SeekFrom};

#[test]
fn package_uses_relative_disk_names_and_the_verified_vdi_uuid() {
    let id = Uuid::parse_str("05acda5a-9c27-4266-8812-2f35f41a7111").unwrap();
    let machine = Uuid::parse_str("1ea3b753-52a0-441e-8d79-3dea4d221abc").unwrap();
    let mut fixture = vec![0; 6 * CHUNK];
    fixture[16384..16400].copy_from_slice(b"offline windows!");
    for platform in [Platform::Vmware, Platform::VirtualBox] {
        let settings = Options {
            platform,
            name: "Office copy".into(),
            cpus: 2,
            memory_mib: 4096,
        };
        let identity = Identity {
            created: id.to_bytes_le(),
            modified: machine.to_bytes_le(),
            vhd_timestamp: 0,
            file_name: platform.disk_name().into(),
        };
        let config = settings
            .config(Firmware::Uefi, Controller::Nvme, machine, &identity)
            .unwrap();
        let config = String::from_utf8(config).unwrap();
        assert!(config.contains(platform.disk_name()));
        assert!(!config.contains(":\\"));
        if platform == Platform::VirtualBox {
            assert!(config.contains(&format!("uuid=\"{{{id}}}\" location=\"System.vdi\"")));
            assert!(config.contains(&format!("<Image uuid=\"{{{id}}}\"")));
            assert!(config.contains("type=\"NVMe\""));
            assert!(config.contains("<Firmware type=\"EFI\">"));
        } else {
            assert!(config.contains("nvme0:0.fileName = \"System.vmdk\""));
            assert!(config.contains("firmware = \"efi\""));
        }
        let options = ImageOptions {
            format: platform.format(),
            size: fixture.len() as u64,
            sector: 512,
        };
        let mut disk = Encoder::new(Cursor::new(Vec::new()), options, identity).unwrap();
        for (i, bytes) in fixture.chunks(CHUNK).enumerate() {
            disk.push((i * CHUNK) as u64, bytes).unwrap();
        }
        for (i, bytes) in fixture.chunks(CHUNK).enumerate() {
            disk.verify_source((i * CHUNK) as u64, bytes).unwrap();
        }
        let (mut file, report) = disk.finish(|_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(report.sha256, format!("{:x}", Sha256::digest(&fixture)));
        if platform == Platform::VirtualBox {
            assert_eq!(&file.get_ref()[392..408], &id.to_bytes_le());
        }
        file.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(file.position(), report.file_bytes);
        if let Some(directory) = std::env::var_os("DISKSTUDIO_SYSTEM_VM_FIXTURE_DIR") {
            use std::{fs::OpenOptions, io::Write, path::PathBuf};
            let directory = PathBuf::from(directory).join(platform.name());
            std::fs::create_dir(&directory).unwrap();
            for (name, bytes) in [
                (platform.disk_name().to_owned(), file.get_ref().as_slice()),
                (settings.config_name(), config.as_bytes()),
                ("Expected.img".into(), fixture.as_slice()),
            ] {
                let mut artifact = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(name))
                    .unwrap();
                artifact.write_all(bytes).unwrap();
            }
        }
    }
}

#[test]
fn rejects_injection_wrong_disk_and_legacy_nvme_start() {
    let mut settings = Options {
        platform: Platform::VirtualBox,
        name: "Windows".into(),
        cpus: 2,
        memory_mib: 4096,
    };
    let id = Identity {
        created: [1; 16],
        modified: [2; 16],
        vhd_timestamp: 0,
        file_name: "System.vdi".into(),
    };
    assert!(settings
        .config(Firmware::Bios, Controller::Nvme, Uuid::new_v4(), &id)
        .is_err());
    assert!(settings
        .config(Firmware::Bios, Controller::Sata, Uuid::new_v4(), &id)
        .is_ok());
    for name in ["..\\Escape", "Bad\"\\n", " leading", "CON", "AUX"] {
        settings.name = name.into();
        assert!(settings.validate().is_err());
    }
    settings.name = "Windows".into();
    settings.memory_mib = 1025;
    assert!(settings.validate().is_err());
    settings.memory_mib = 4096;
    settings.platform = Platform::Vmware;
    assert!(settings
        .config(Firmware::Uefi, Controller::Sata, Uuid::new_v4(), &id)
        .is_err());
}

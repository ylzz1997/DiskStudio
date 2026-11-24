//! Provision one disposable VM disk with the serviced WinPE test image.
#[path = "../winpe_lab_disks.rs"]
mod winpe_lab_disks;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitCode};

const SERIAL: &str = "DS-TEST-B-0001      _00000001.";
const INPUT: &str = r"X:\dsadk";
const WORK: &str = r"R:\";
const PACKAGES: [&str; 5] = ["WMI", "NetFx", "Scripting", "PowerShell", "StorageWMI"];

fn dism(log: &str, arguments: &[&str]) -> io::Result<()> {
    let status = Command::new(r"X:\Windows\System32\dism.exe")
        .args([
            "/English",
            r"/ScratchDir:R:\scratch",
            &format!(r"/LogPath:R:\logs\{log}.log"),
        ])
        .args(arguments)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "DISM {log} stopped with {status}"
        )));
    }
    Ok(())
}

fn service() -> Result<(), Box<dyn std::error::Error>> {
    let disks = winpe_lab_disks::disk_inventory()?;
    if !matches!(disks.as_slice(), [disk] if disk.index == 0 && disk.serial_number.trim() == SERIAL)
    {
        return Err("Expected only the verified Test B device as disk 0".into());
    }
    winpe_lab_disks::verify_blank_disk(0, "DS-TEST-B-0001", SERIAL, 512)?;
    winpe_lab_disks::check_drive_letter('R')?;
    let input = Path::new(INPUT);
    let inputs = PACKAGES.map(|kind| {
        [
            format!("WinPE-{kind}.cab"),
            format!("WinPE-{kind}_en-us.cab"),
        ]
    });
    for name in inputs
        .iter()
        .flatten()
        .map(String::as_str)
        .chain(["winpe.wim", "dsprobe.exe"])
    {
        let metadata = input.join(name).symlink_metadata()?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!("The verified input is missing: {name}").into());
        }
    }
    println!("Verified fixed VM identity and sole blank Test B disk");
    winpe_lab_disks::prepare_volume(0, 'R', "DS-WINPE")?;
    let work = Path::new(WORK);
    for folder in ["scratch", "logs", "image"] {
        fs::create_dir(work.join(folder))?;
    }
    dism(
        "apply",
        &[
            "/Apply-Image",
            r"/ImageFile:X:\dsadk\winpe.wim",
            "/Index:1",
            r"/ApplyDir:R:\image",
            "/CheckIntegrity",
        ],
    )?;
    for name in inputs.iter().flatten() {
        println!("Installing {name}");
        dism(
            name,
            &[
                r"/Image:R:\image",
                "/Add-Package",
                &format!(r"/PackagePath:{INPUT}\{name}"),
            ],
        )?;
    }
    dism("installed-packages", &[r"/Image:R:\image", "/Get-Packages"])?;
    fs::copy(input.join("dsprobe.exe"), work.join(r"image\dsprobe.exe"))?;
    dism(
        "capture",
        &[
            "/Capture-Image",
            r"/ImageFile:R:\winpe-storage.wim",
            r"/CaptureDir:R:\image",
            "/Name:DiskStudio storage tests",
            "/Bootable",
            "/Compress:fast",
            "/CheckIntegrity",
            "/Verify",
        ],
    )?;
    let image = work.join("winpe-storage.wim");
    OpenOptions::new().write(true).open(&image)?.sync_all()?;
    let mut file = File::open(&image)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let report = format!(
        "SHA-256 {:x}\r\nBytes {}\r\n",
        digest.finalize(),
        file.metadata()?.len()
    );
    let proof = work.join("winpe-storage.txt");
    fs::write(&proof, &report)?;
    OpenOptions::new().write(true).open(&proof)?.sync_all()?;
    println!("WINPE STORAGE IMAGE READY\n{report}");
    Ok(())
}

fn main() -> ExitCode {
    match service() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("WINPE STORAGE IMAGE STOPPED: {error}");
            ExitCode::FAILURE
        }
    }
}

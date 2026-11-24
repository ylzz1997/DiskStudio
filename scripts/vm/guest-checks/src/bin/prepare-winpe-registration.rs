//! Prepare only the two disposable WinPE registration fixture disks.
#[cfg(test)]
#[path = "../winpe_registration_4kn_fixture.rs"]
mod fixtures;
#[path = "../winpe_lab_disks.rs"]
mod winpe_lab_disks;

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

const SERIAL_A: &str = "DS-TEST-A-0001      _00000001.";
const SERIAL_B: &str = "DS-TEST-B-0001      _00000001.";
const SAMPLES: [(&str, &str); 3] = [
    (
        "library.img",
        "5f40ce09e5ec81ddc6cd18615ae0120b0d4b34d489728cc88c871fdc5d35897a",
    ),
    (
        "searchboth.img",
        "4914dc488eefb29f2327437f44a5053cb3da429d3adbb457f430451f69f7c9ff",
    ),
    (
        "searchfree.img",
        "57ee19129664e720b4235921ff71e5315d583bdce6d83cff9d7a5e863caf985f",
    ),
];
const SAMPLES_4KN: [(&str, &str); 3] = [
    (
        "library.img",
        "2cedd46e91f5691317892cb6eada90eab1aa1fb4076355dd650a34d24d838afe",
    ),
    (
        "searchboth.img",
        "0ffa44c7c84b781b9bca34ae921d6f4f87a55a9d9e474b7f281b1ca0c8e247fe",
    ),
    (
        "searchfree.img",
        "a7903b0627137069077b35355fa59516ab50f7cd535343e85a9c092394259a93",
    ),
];

fn create_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn lab_pair() -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let disks = winpe_lab_disks::disk_inventory()?;
    let (a, b) = match disks.as_slice() {
        [a, b] if a.serial_number.trim() == SERIAL_A && b.serial_number.trim() == SERIAL_B => {
            (a, b)
        }
        [b, a] if a.serial_number.trim() == SERIAL_A && b.serial_number.trim() == SERIAL_B => {
            (a, b)
        }
        _ => return Err("Expected only the full Test A and Test B namespace serials".into()),
    };
    if a.index == b.index {
        return Err("Test A and Test B reported the same disk index".into());
    }
    Ok((a.index, b.index))
}

fn verify_physical_fixtures() -> Result<(), Box<dyn std::error::Error>> {
    let (a, b) = lab_pair()?;
    winpe_lab_disks::verify_disk_geometry(a, "DS-TEST-A-0001", SERIAL_A, 4096)?;
    winpe_lab_disks::verify_disk_geometry(b, "DS-TEST-B-0001", SERIAL_B, 4096)?;
    let letters: Vec<_> = ('C'..='Z')
        .filter(|&letter| {
            letter != 'X'
                && winpe_lab_disks::verify_volume(b, letter).is_ok()
                && Path::new(&format!("{letter}:\\output\\existing.img")).is_file()
        })
        .collect();
    let [letter] = letters.as_slice() else {
        return Err("Expected exactly one verified Test B output volume".into());
    };
    println!("WINPE PHYSICAL FIXTURES VERIFIED\nDisk {a}: {SERIAL_A}\nDisk {b}: {SERIAL_B}\n4Kn, 16 GiB per disk. Output {letter}:\\output");
    Ok(())
}

fn verify_recovered_disk() -> Result<(), Box<dyn std::error::Error>> {
    let (a, b) = lab_pair()?;
    let target = winpe_lab_disks::verify_disk_geometry(a, "DS-TEST-A-0001", SERIAL_A, 4096)?;
    winpe_lab_disks::verify_disk_geometry(b, "DS-TEST-B-0001", SERIAL_B, 4096)?;
    winpe_lab_disks::verify_offline(&target)?;
    println!("WINPE GPT RECOVERY: TEST A STILL OFFLINE\nDisk {a}: {SERIAL_A}");
    Ok(())
}

fn prepare(sector: u32, samples: &[(&str, &str)]) -> Result<(), Box<dyn std::error::Error>> {
    let (a, b) = lab_pair()?;
    winpe_lab_disks::verify_blank_disk(a, "DS-TEST-A-0001", SERIAL_A, sector)?;
    winpe_lab_disks::verify_blank_disk(b, "DS-TEST-B-0001", SERIAL_B, sector)?;
    winpe_lab_disks::check_drive_letter('P')?;
    winpe_lab_disks::check_drive_letter('T')?;

    let mut files = Vec::with_capacity(samples.len());
    for &(name, expected) in samples {
        let file = Path::new(r"X:\dsinput").join(name);
        let bytes = fs::read(file)?;
        if format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(format!("Fixture digest mismatch: {name}").into());
        }
        files.push((name, expected, bytes));
    }

    winpe_lab_disks::prepare_volume(a, 'P', "DS-REGISTER-A")?;
    winpe_lab_disks::prepare_volume(b, 'T', "DS-REGISTER-B")?;
    winpe_lab_disks::verify_volume(a, 'P')?;
    winpe_lab_disks::verify_volume(b, 'T')?;
    let source = Path::new(r"P:\cases");
    let target = Path::new(r"T:\output");
    fs::create_dir(source)?;
    fs::create_dir(target)?;
    for (name, expected, bytes) in files {
        let path = source.join(name);
        create_file(&path, &bytes)?;
        if format!("{:x}", Sha256::digest(fs::read(path)?)) != expected {
            return Err(format!("Source volume readback mismatch: {name}").into());
        }
    }
    create_file(&target.join("existing.img"), b"Existing Test B file\r\n")?;
    println!("WINPE REGISTRATION FIXTURES READY\nVerified A: P:\\cases\nVerified B: T:\\output");
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let result = match (args.next(), args.next()) {
        (None, None) => prepare(512, &SAMPLES),
        (Some(mode), None) if mode == "--4kn" => prepare(4096, &SAMPLES_4KN),
        (Some(mode), None) if mode == "--verify-physical-4kn" => verify_physical_fixtures(),
        (Some(mode), None) if mode == "--verify-gpt-recovered" => verify_recovered_disk(),
        _ => return ExitCode::FAILURE,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("WINPE REGISTRATION FIXTURES STOPPED: {error}");
            ExitCode::FAILURE
        }
    }
}

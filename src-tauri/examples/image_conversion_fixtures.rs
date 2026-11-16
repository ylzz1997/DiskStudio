//! Explicit synthetic-file integration corpus. No devices or VM disk files.
#[path = "../src/image_convert.rs"]
mod image_convert;
use image_convert::{Encoder, Format, Identity, Options, CHUNK};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let root = project.join(".vm").join("synthetic-image-conversion");
    fs::create_dir_all(&root)?;
    let out = root.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&out)?;
    let mut data = vec![0; 24 * CHUNK + 512];
    // Synthetic MBR and distinctive bytes across allocated, zero and tail blocks.
    data[510] = 0x55;
    data[511] = 0xaa;
    data[450] = 0x07;
    data[454..458].copy_from_slice(&2048u32.to_le_bytes());
    data[458..462].copy_from_slice(&4096u32.to_le_bytes());
    for (i, b) in data[2 * CHUNK..4 * CHUNK].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    data[24 * CHUNK..].fill(0xa7);
    let mut raw = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(out.join("source.raw"))?;
    raw.write_all(&data)?;
    raw.sync_all()?;
    drop(raw);
    for (name, format) in [
        ("copy.img", Format::Raw),
        ("fixed.vhd", Format::VhdFixed),
        ("dynamic.vhd", Format::VhdDynamic),
        ("dynamic.vdi", Format::VdiDynamic),
        ("dynamic.vhdx", Format::VhdxDynamic),
        ("fixed.vhdx", Format::VhdxFixed),
        ("sparse.vmdk", Format::VmdkSparse),
        ("structured.hds", Format::Parallels),
    ] {
        let f = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(out.join(name))?;
        let mut e = Encoder::new(
            f,
            Options {
                format,
                size: data.len() as u64,
                sector: 512,
            },
            Identity {
                created: *uuid::Uuid::new_v4().as_bytes(),
                modified: *uuid::Uuid::new_v4().as_bytes(),
                vhd_timestamp: 12345,
                file_name: name.into(),
            },
        )?;
        for (i, b) in data.chunks(CHUNK).enumerate() {
            e.push((i * CHUNK) as u64, b)?;
        }
        for (i, b) in data.chunks(CHUNK).enumerate() {
            e.verify_source((i * CHUNK) as u64, b)?;
        }
        let (_file, verified) =
            e.finish(|f| f.sync_all().map_err(|e| e.to_string()), |_| Ok(()))?;
        println!(
            "{name}: {} bytes, SHA-256 {}",
            verified.file_bytes, verified.sha256
        );
    }
    for (tail, suffix) in [(4096, "4k"), (8192, "4k-even")] {
        data.resize(24 * CHUNK + tail, 0);
        let mut raw4k = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(out.join(format!("source{suffix}.raw")))?;
        raw4k.write_all(&data)?;
        raw4k.sync_all()?;
        drop(raw4k);
        for (name, format) in [
            (format!("dynamic{suffix}.vhdx"), Format::VhdxDynamic),
            (format!("fixed{suffix}.vhdx"), Format::VhdxFixed),
        ] {
            let f = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(out.join(&name))?;
            let mut e = Encoder::new(
                f,
                Options {
                    format,
                    size: data.len() as u64,
                    sector: 4096,
                },
                Identity {
                    created: *uuid::Uuid::new_v4().as_bytes(),
                    modified: *uuid::Uuid::new_v4().as_bytes(),
                    vhd_timestamp: 12345,
                    file_name: name.clone(),
                },
            )?;
            for (i, b) in data.chunks(CHUNK).enumerate() {
                e.push((i * CHUNK) as u64, b)?;
            }
            for (i, b) in data.chunks(CHUNK).enumerate() {
                e.verify_source((i * CHUNK) as u64, b)?;
            }
            let (_, v) = e.finish(|f| f.sync_all().map_err(|e| e.to_string()), |_| Ok(()))?;
            println!("{name}: {} bytes, SHA-256 {}", v.file_bytes, v.sha256);
        }
    }
    println!("CORPUS={}", out.display());
    Ok(())
}

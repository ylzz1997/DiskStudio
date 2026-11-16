//! Fixed synthetic ordinary-file corpus. Never takes a device or arbitrary path.
#[path = "../src/image_convert.rs"]
mod image_convert;
use image_convert::{Encoder, Format, Identity, Options, CHUNK};
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    for name in [".vm", ".vm/payload", ".vm/payload/ImageBrowse"] {
        let path = root.join(name);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() || !path.is_dir() {
            return Err("Expected ordinary fixture directory".into());
        }
    }
    let source = root.join(".vm/payload/ImageBrowse");
    let out = root.join(".vm/payload/ImageBrowseContainers");
    fs::create_dir(&out)?; // Refuse to replace an earlier acceptance corpus.
    let mut records = vec![];
    for (name, format, sector) in [
        ("fixed.vhd", Format::VhdFixed, 512),
        ("dynamic.vhd", Format::VhdDynamic, 512),
        ("fixed.vdi", Format::VdiFixed, 512),
        ("dynamic.vdi", Format::VdiDynamic, 512),
        ("fixed.vhdx", Format::VhdxFixed, 512),
        ("dynamic.vhdx", Format::VhdxDynamic, 512),
        ("sparse.vmdk", Format::VmdkSparse, 512),
        ("structured.hds", Format::Parallels, 512),
        ("fixed4k.vhdx", Format::VhdxFixed, 4096),
        ("dynamic4k.vhdx", Format::VhdxDynamic, 4096),
    ] {
        let input = if sector == 512 {
            "disk-gpt512.img"
        } else {
            "disk-gpt4096.img"
        };
        let path = source.join(input);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() || meta.len() != 6 * CHUNK as u64 {
            return Err("Expected 6MiB ordinary synthetic GPT file".into());
        }
        let raw = fs::read(path)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(out.join(name))?;
        let mut encoder = Encoder::new(
            file,
            Options {
                format,
                size: raw.len() as u64,
                sector,
            },
            Identity {
                created: *uuid::Uuid::new_v4().as_bytes(),
                modified: *uuid::Uuid::new_v4().as_bytes(),
                vhd_timestamp: 12345,
                file_name: name.into(),
            },
        )?;
        for (i, bytes) in raw.chunks(CHUNK).enumerate() {
            encoder.push((i * CHUNK) as u64, bytes)?;
        }
        for (i, bytes) in raw.chunks(CHUNK).enumerate() {
            encoder.verify_source((i * CHUNK) as u64, bytes)?;
        }
        let (_, verified) =
            encoder.finish(|f| f.sync_all().map_err(|e| e.to_string()), |_| Ok(()))?;
        records.push(
            serde_json::json!({"name":name,"input":input,"sector":sector,"verified":verified}),
        );
    }
    fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&records)?,
    )?;
    println!("CORPUS={}", out.display());
    Ok(())
}

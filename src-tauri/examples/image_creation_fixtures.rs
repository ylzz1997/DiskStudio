//! Explicit ordinary-file integration corpus. All files are newly generated
//! under .vm/synthetic-image-creation; never accept device or output paths.
#[path = "../src/image_convert.rs"]
mod image_convert;
#[path = "../src/image_creation.rs"]
mod image_creation;
use image_convert::{Encoder, Format, Identity, Options as OutputOptions, CHUNK};
use image_creation::{Generator, Options, Table};
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let root = project.join(".vm").join("synthetic-image-creation");
    fs::create_dir_all(&root)?;
    let out = root.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&out)?;
    let id = uuid::Uuid::parse_str("12345678-1234-4abc-8def-112233445566")?;
    let mut records = vec![];
    for (name, format, table, sector) in [
        ("gpt512.img", Format::Raw, Table::Gpt, 512),
        ("fixed.vhd", Format::VhdFixed, Table::Gpt, 512),
        ("dynamic.vhd", Format::VhdDynamic, Table::Gpt, 512),
        ("fixed.vdi", Format::VdiFixed, Table::Gpt, 512),
        ("dynamic.vdi", Format::VdiDynamic, Table::Gpt, 512),
        ("fixed.vhdx", Format::VhdxFixed, Table::Gpt, 512),
        ("dynamic.vhdx", Format::VhdxDynamic, Table::Gpt, 512),
        ("sparse.vmdk", Format::VmdkSparse, Table::Gpt, 512),
        ("structured.hds", Format::Parallels, Table::Gpt, 512),
        ("gpt4k.img", Format::Raw, Table::Gpt, 4096),
        ("fixed4k.vhdx", Format::VhdxFixed, Table::Gpt, 4096),
        ("dynamic4k.vhdx", Format::VhdxDynamic, Table::Gpt, 4096),
        ("mbr.img", Format::Raw, Table::Mbr, 512),
        ("blank.img", Format::Raw, Table::None, 512),
    ] {
        let options = Options {
            output: OutputOptions {
                format,
                size: 4 * CHUNK as u64 + sector,
                sector,
            },
            table,
        };
        let generator = Generator::new(options.clone(), id)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(out.join(name))?;
        let mut encoder = Encoder::new(
            file,
            options.output,
            Identity {
                created: *uuid::Uuid::new_v4().as_bytes(),
                modified: *uuid::Uuid::new_v4().as_bytes(),
                vhd_timestamp: 12345,
                file_name: name.into(),
            },
        )?;
        generator.feed(&mut encoder, |_, _| Ok(()))?;
        let (_, verified) =
            encoder.finish(|f| f.sync_all().map_err(|e| e.to_string()), |_| Ok(()))?;
        records.push(serde_json::json!({"name":name,"sector":sector,"table":table,"diskId":generator.summary().disk_id,"verified":verified}));
    }
    fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&records)?,
    )?;
    println!("CORPUS={}", out.display());
    Ok(())
}

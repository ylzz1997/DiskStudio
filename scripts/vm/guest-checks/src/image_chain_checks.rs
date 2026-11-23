//! Guest-only acceptance for image chains on the disposable A/B pair.
use super::*;
use cap_std::fs::{Dir, OpenOptions};
use disk_core::MIB;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const SAMPLE_NAME: &str = "file-with-12345";

pub(super) struct Fixture {
    folder: &'static str,
    manifest: &'static str,
    sources: &'static [&'static str],
    source_limit: u64,
}

pub(super) const VMDK: Fixture = Fixture {
    folder: "DS-Vmdk-Chain",
    manifest: "vmdk-chain.json",
    sources: &[
        "latest.vmdk",
        "middle.vmdk",
        "base.vmdk",
        "latest-s001.vmdk",
    ],
    source_limit: 8 * MIB,
};
pub(super) const VDI: Fixture = Fixture {
    folder: "DS-Vdi-Chain",
    manifest: "vdi-chain.json",
    sources: &["latest.vdi", "middle.vdi", "base.vdi"],
    source_limit: 8 * MIB,
};
pub(super) const VHDX_LOG: Fixture = Fixture {
    folder: "DS-Vhdx-Log",
    manifest: "vhdx-log.json",
    sources: &["pending.vhdx"],
    source_limit: 16 * MIB,
};
pub(super) const ESXI: Fixture = Fixture {
    folder: "DS-Esxi-Chain",
    manifest: "esxi-chain.json",
    sources: &[
        "latest.vmdk",
        "latest-sesparse.vmdk",
        "point.vmdk",
        "point-delta.vmdk",
        "base.vmdk",
        "base-flat.vmdk",
    ],
    source_limit: 8 * MIB,
};
pub(super) const PARALLELS: Fixture = Fixture {
    folder: "DS-Parallels-Bundle",
    manifest: "parallels-bundle.json",
    sources: &[
        "DiskDescriptor.xml",
        "lower-base.hdd",
        "lower-point.hds",
        "lower-latest.hds",
        "upper-base.hds",
        "upper-point.hds",
        "upper-latest.hds",
    ],
    source_limit: 8 * MIB,
};
pub(super) const PARALLELS_HISTORY: Fixture = Fixture {
    folder: "DS-Parallels-History",
    manifest: "parallels-history.json",
    sources: &[
        "DiskDescriptor.xml",
        "lower-base.hdd",
        "lower-point.hds",
        "lower-latest.hds",
        "upper-base.hds",
        "upper-point.hds",
        "upper-latest.hds",
        "lower-historical.hds",
        "upper-historical.hds",
    ],
    source_limit: 8 * MIB,
};

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn new_file(folder: &Dir, name: &str, data: &[u8]) -> Result<(), String> {
    let mut file = folder
        .open_with(name, OpenOptions::new().write(true).create_new(true))
        .map_err(err)?;
    file.write_all(data).map_err(err)?;
    file.sync_all().map_err(err)
}

pub(super) fn bounded(folder: &Dir, name: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    folder
        .open(name)
        .map_err(err)?
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(err)?;
    if data.is_empty() || data.len() as u64 > limit {
        return Err(format!("Invalid fixture length for {name}"));
    }
    Ok(data)
}

pub(super) fn optical() -> Result<Dir, String> {
    let exe = std::env::current_exe().map_err(err)?;
    Dir::open_ambient_dir(
        exe.parent().ok_or("Missing optical root")?,
        cap_std::ambient_authority(),
    )
    .map_err(err)
}

fn record(cd: &Dir, fixture: &Fixture) -> Result<serde_json::Value, String> {
    serde_json::from_slice(&bounded(cd, fixture.manifest, 4096)?).map_err(err)
}

pub(super) fn prepare(fixture: Fixture) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (b, source) = ext_reconstruct_checks::b_root()?;
    let (a, output) = ntfs_reconstruction_checks::verified_a_root()?;
    let cd = optical()?;
    let reference = record(&cd, &fixture)?;
    let mut sources = Vec::new();
    for &name in fixture.sources {
        let bytes = bounded(&cd, name, fixture.source_limit)?;
        if reference["files"][name] != sha(&bytes) {
            return Err(format!("Optical fixture digest differs for {name}"));
        }
        sources.push((name, bytes));
    }
    let layouts = ntfs_reconstruction_checks::table_hashes()?;
    let folder = fixture.folder;
    if a.exists(folder) || b.exists(folder) {
        return Err("The image chain acceptance directory already exists".into());
    }
    b.create_dir(folder).map_err(err)?;
    a.create_dir(folder).map_err(err)?;
    let input = b.open_dir(folder).map_err(err)?;
    let destination = a.open_dir(folder).map_err(err)?;
    destination.create_dir("recovery").map_err(err)?;
    for (name, bytes) in sources {
        new_file(&input, name, &bytes)?;
        if bounded(&input, name, fixture.source_limit)? != bytes {
            return Err(format!("Seed readback failed for {name}"));
        }
    }
    let baseline =
        serde_json::json!({ "desktop": desktop, "layouts": layouts, "reference": reference });
    new_file(
        &destination,
        "baseline.json",
        &serde_json::to_vec(&baseline).map_err(err)?,
    )?;
    Ok(format!("PASS: verified image fixture in {}. Select {} and any companion files. Recover file-with-12345 into recovery and save the RAW conversion as merged.img in {}.", source.join(folder).display(), fixture.sources[0], output.join(folder).display()))
}

pub(super) fn verify(fixture: Fixture) -> Result<String, String> {
    let desktop = btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (b, _) = ext_reconstruct_checks::b_root()?;
    let (a, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let input = b.open_dir(fixture.folder).map_err(err)?;
    let output = a.open_dir(fixture.folder).map_err(err)?;
    let reference = record(&optical()?, &fixture)?;
    let baseline: serde_json::Value =
        serde_json::from_slice(&bounded(&output, "baseline.json", 4096)?).map_err(err)?;
    if baseline["desktop"] != desktop
        || baseline["reference"] != reference
        || baseline["layouts"] != serde_json::json!(ntfs_reconstruction_checks::table_hashes()?)
    {
        return Err(
            "Image chain fixture, application or protected partition layouts differ".into(),
        );
    }
    for &name in fixture.sources {
        if reference["files"][name] != sha(&bounded(&input, name, fixture.source_limit)?) {
            return Err(format!("Source image changed: {name}"));
        }
    }
    let result = bounded(&output, "merged.img", 6 * MIB)?;
    let digest = sha(&result);
    if result.len() as u64 != 6 * MIB || reference["logicalSha256"] != digest {
        return Err("RAW output differs from the independent logical fixture".into());
    }
    let parent = output.open_dir("recovery").map_err(err)?;
    let mut entries = parent.entries().map_err(err)?;
    let entry = entries
        .next()
        .ok_or("Expected a recovery output folder")?
        .map_err(err)?;
    if entries.next().is_some()
        || !entry
            .file_name()
            .to_string_lossy()
            .starts_with("DiskStudio-recovered-")
    {
        return Err("Unexpected recovery output folder".into());
    }
    let recovered = parent.open_dir(entry.file_name()).map_err(err)?;
    let report: serde_json::Value =
        serde_json::from_slice(&bounded(&recovered, recovery_export::REPORT, 128 * 1024)?)
            .map_err(err)?;
    if report["completed"] != true
        || report["savedFiles"] != 1
        || report["failedFiles"] != 0
        || report["entries"][0]["originalName"] != SAMPLE_NAME
        || report["entries"][0]["folder"] != 0
        || report["outcomes"][0]["sha256"] != sha(b"12345")
        || report["imageSource"]["fileName"] != fixture.sources[0]
        || report["imageSource"]["offset"] != MIB.to_string()
        || report["imageSource"]["sector"] != 512
        || (!reference["snapshot"].is_null()
            && report["imageSource"]["parallelsSnapshot"] != reference["snapshot"])
    {
        return Err("The NTFS file or provenance differs".into());
    }
    let leaf = report["entries"][0]["name"]
        .as_str()
        .filter(|name| !name.contains(['/', '\\', ':']) && *name == data_io::safe_name(name))
        .ok_or("The manifest output name is invalid")?;
    if bounded(&recovered, leaf, 5)? != b"12345" {
        return Err("The exported NTFS file bytes differ".into());
    }
    Ok(format!("PASS: 6 MiB RAW SHA-256 {digest} matches the independent fixture. NTFS file contains 12345. All {} source digests, both GPTs and guest OS layout match.", fixture.sources.len()))
}

pub(super) fn verify_image_archives() -> Result<String, String> {
    use crate::backup_archive::{Archive, Chain};
    use std::io::Cursor;

    btrfs_checks::desktop()?;
    fat_sector_checks::confirm_4kn(&inventory()?)?;
    let (b, _) = ext_reconstruct_checks::b_root()?;
    let (a, _) = ntfs_reconstruction_checks::verified_a_root()?;
    let fixture = PARALLELS_HISTORY;
    let input = b.open_dir(fixture.folder).map_err(err)?;
    let output = a.open_dir(fixture.folder).map_err(err)?;
    let reference = record(&optical()?, &fixture)?;
    let baseline: serde_json::Value =
        serde_json::from_slice(&bounded(&output, "baseline.json", 4096)?).map_err(err)?;
    if baseline["reference"] != reference
        || baseline["layouts"] != serde_json::json!(ntfs_reconstruction_checks::table_hashes()?)
    {
        return Err("Image archive fixture or protected partition layouts differ".into());
    }
    for &name in fixture.sources {
        if reference["files"][name] != sha(&bounded(&input, name, fixture.source_limit)?) {
            return Err(format!("Source image changed: {name}"));
        }
    }
    let expected = bounded(&output, "merged.img", 6 * MIB)?;
    if expected.len() as u64 != 6 * MIB || reference["logicalSha256"] != sha(&expected) {
        return Err("The independent logical archive oracle differs".into());
    }
    let open = |name| -> Result<Archive, String> {
        if output.exists(format!("{name}.partial")) {
            return Err(format!("An unexpected partial exists for {name}"));
        }
        Archive::from_reader(Cursor::new(bounded(&output, name, 7 * MIB)?))
    };
    let check = |chain: &mut Chain, expect: &[u8], kind: &str| -> Result<(), String> {
        if chain.header().size != expect.len() as u64 || chain.header().sector != 512
            || chain.header().kind != kind || chain.header().sha256 != sha(expect)
        {
            return Err("Archive header differs from the independent source bytes".into());
        }
        chain.verify(|_| Ok(()))?;
        let mut logical = vec![0; expect.len()];
        chain.reader().read_exact(&mut logical).map_err(err)?;
        if logical != expect {
            return Err("Restored logical bytes differ from the verified source".into());
        }
        Ok(())
    };
    let mut full = Chain::new(open("archive-full.dsbackup")?);
    check(&mut full, &expected, "disk")?;
    if full.header().index_offset >= 6 * MIB {
        return Err("The compressed full image archive is unexpectedly large".into());
    }
    let mut child = full.prepend(open("archive-child.dsbackup")?)?;
    if child.header().index_offset != 4096 || child.depth() != 2 {
        return Err("The unchanged child must inherit every block from its full parent".into());
    }
    check(&mut child, &expected, "disk")?;
    let mut partition = Chain::new(open("archive-ntfs.dsbackup")?);
    check(&mut partition, &expected[MIB as usize..(3 * MIB) as usize], "partition")?;
    Ok("PASS: full and inherited 6 MiB archives and the 2 MiB NTFS partition match the independently checked logical bytes. Source files, both GPTs and guest OS layout match.".into())
}

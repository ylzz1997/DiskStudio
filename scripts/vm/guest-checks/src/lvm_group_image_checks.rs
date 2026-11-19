//! Isolated ordinary-file acceptance for advanced logical volumes in a VHDX.
use super::*;

const OUTPUT_BYTES: u64 = 18 * 65536;

pub(crate) struct Fixture {
    receipt: &'static str,
    container: &'static str,
    group: &'static str,
    members: &'static [usize],
    degraded: bool,
    volumes: [&'static str; 2],
    destinations: [&'static str; 2],
}

pub(crate) const RAID: Fixture = Fixture {
    receipt: "lvm-raid-images",
    container: "Lvm-Raid-Group.vhdx",
    group: "ds_lvm_raid_fixture",
    members: &[1, 2, 3, 4, 5],
    degraded: false,
    volumes: ["r1", "r6"],
    destinations: ["DSLVM-Raid-R1", "DSLVM-Raid-R6"],
};

pub(crate) const THIN: Fixture = Fixture {
    receipt: "lvm-thin-images",
    container: "Lvm-Thin-Group.vhdx",
    group: "ds_lvm_thin_fixture",
    members: &[1, 2],
    degraded: false,
    volumes: ["origin", "point1"],
    destinations: ["DSLVM-Thin-Origin", "DSLVM-Thin-Snapshot"],
};

pub(crate) const COW: Fixture = Fixture {
    receipt: "lvm-cow-images",
    container: "Lvm-Cow-Group.vhdx",
    group: "ds_lvm_cow_fixture",
    members: &[1, 2],
    degraded: false,
    volumes: ["point1", "sparse"],
    destinations: ["DSLVM-Cow-Point", "DSLVM-Cow-Sparse"],
};

pub(crate) const DEGRADED: Fixture = Fixture {
    receipt: "lvm-degraded-images",
    container: "Lvm-Raid-Group.vhdx",
    group: "ds_lvm_raid_fixture",
    members: &[1, 2, 4],
    degraded: true,
    volumes: ["r5", "r6"],
    destinations: ["DSLVM-Degraded-R5", "DSLVM-Degraded-R6"],
};

pub(crate) const RAID10: Fixture = Fixture {
    receipt: "lvm-raid10-layouts",
    container: "Lvm-Raid10-Group.vhdx",
    group: "ds_lvm_raid10_fixture",
    members: &[1, 2, 3, 4],
    degraded: false,
    volumes: ["far", "offset"],
    destinations: ["DSLVM-Raid10-Far", "DSLVM-Raid10-Offset"],
};

#[derive(Serialize, Deserialize)]
struct GroupReport {
    source: PathBuf,
    destinations: [PathBuf; 2],
    tables: [String; 2],
    os: String,
}

fn container_digest() -> Result<&'static str, String> {
    option_env!("DISKSTUDIO_LVM_CONTAINER_SHA256")
        .ok_or_else(|| "Missing immutable test image digest".into())
}

pub(crate) fn prepare(fixture: Fixture) -> Result<String, String> {
    let sha = desktop()?;
    let receipt = history_report(fixture.receipt, &sha);
    if receipt.exists() {
        return Err("Preserve the existing LVM2 image receipt".into());
    }
    let media = std::env::current_exe()
        .map_err(err)?
        .with_file_name(fixture.container);
    let mut image = File::open(&media).map_err(err)?;
    let bytes = image.metadata().map_err(err)?.len();
    let digest = container_digest()?;
    if !(1..=128 * MIB).contains(&bytes) || allocation_batch::hash_file(&media)? != digest {
        return Err("The immutable LVM2 container differs from its digest".into());
    }
    let tables = history_tables()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    online(A)?;
    online(B)?;
    let folder = images::source_directory()?;
    images::check_dir(&folder, A, None)?;
    let source = folder.join(fixture.container);
    let destinations = [
        history_destination(fixture.destinations[0])?,
        history_destination(fixture.destinations[1])?,
    ];
    if source.try_exists().map_err(err)? != fixture.degraded
        || destinations.iter().any(|dir| dir.exists())
    {
        return Err("Preserve existing LVM2 fixtures or result directories".into());
    }
    if fixture.degraded {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        let metadata = fs::symlink_metadata(&source).map_err(err)?;
        if !metadata.is_file()
            || metadata.len() != bytes
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || data_windows::file_location(&File::open(&source).map_err(err)?)?.target
                != selection(A, None)?.target
        {
            return Err("Existing RAID source is not the expected isolated fixture".into());
        }
    } else {
        data_windows::destination_folder(&folder, &selection(B, None)?.target, bytes)?;
        let mut staged = images::create_fixture(&source)?;
        if std::io::copy(&mut image, &mut staged).map_err(err)? != bytes {
            return Err("Short LVM2 fixture copy".into());
        }
        staged.sync_all().map_err(err)?;
    }
    if allocation_batch::hash_file(&source)? != digest {
        return Err("Staged LVM2 fixture differs".into());
    }
    for folder in &destinations {
        fs::create_dir(folder).map_err(err)?;
        images::check_dir(folder, B, None)?;
    }
    if history_tables()? != tables || reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("Partition layouts changed during LVM2 fixture staging".into());
    }
    let saved = GroupReport {
        source,
        destinations,
        tables,
        os,
    };
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(receipt)
        .map_err(err)?;
    serde_json::to_writer(&mut file, &saved).map_err(err)?;
    file.sync_all().map_err(err)?;
    let instructions = if fixture.degraded {
        "Select PV 1, PV 2 and PV 4. Enable degraded reads."
    } else {
        "Select all PV partitions."
    };
    Ok(format!(
        "PASS. Matching desktop and LVM2 source {}. {} Copy nested from {} to {} and {} to {}.",
        saved.source.display(),
        instructions,
        fixture.volumes[0],
        saved.destinations[0].display(),
        fixture.volumes[1],
        saved.destinations[1].display()
    ))
}

pub(crate) fn inspect(fixture: Fixture) -> Result<String, String> {
    let sha = desktop()?;
    let report: GroupReport =
        serde_json::from_reader(File::open(history_report(fixture.receipt, &sha)).map_err(err)?)
            .map_err(err)?;
    let root = images::source_directory()?;
    images::check_dir(&root, A, None)?;
    if report.source != root.join(fixture.container)
        || allocation_batch::hash_file(&report.source)? != container_digest()?
        || history_tables()? != report.tables
        || reallocation_checks::os_layout(&inventory()?)? != report.os
    {
        return Err("LVM2 input digest or protected layouts changed".into());
    }
    let digests =
        option_env!("DISKSTUDIO_LVM_GROUP_OUTPUT_SHA256").ok_or("Missing Linux output proofs")?;
    let digests: Vec<_> = digests.split(',').collect();
    if digests.len() != fixture.destinations.len() {
        return Err("Unexpected Linux output proof count".into());
    }
    for (((path, target), lv), digest) in report
        .destinations
        .iter()
        .zip(fixture.destinations)
        .zip(fixture.volumes)
        .zip(digests)
    {
        if *path != history_destination(target)? {
            return Err("LVM2 output folder binding changed".into());
        }
        images::check_dir(path, B, None)?;
        let filename = format!("{lv}.bin");
        verify_recovered(path, &[(&filename, OUTPUT_BYTES, digest)])?;
        let result = fs::read_dir(path)
            .map_err(err)?
            .next()
            .ok_or("Missing LVM2 output")?
            .map_err(err)?;
        if !result.path().join("nested").join(&filename).is_file() {
            return Err("LVM2 output lost its nested folder".into());
        }
        let exported: serde_json::Value = serde_json::from_reader(
            File::open(result.path().join(recovery_export::REPORT)).map_err(err)?,
        )
        .map_err(err)?;
        let source = &exported["imageLvmSource"];
        if source["logicalVolume"] != lv || source["volumeGroup"] != fixture.group {
            return Err("LVM2 provenance has a different logical volume".into());
        }
        if exported["lvmDegradedRaid"].as_bool().unwrap_or(false) != fixture.degraded {
            return Err("LVM2 export report has the wrong degraded-read state".into());
        }
        let members = source["members"]
            .as_array()
            .ok_or("Missing LVM2 provenance ranges")?;
        let expected = fixture
            .members
            .iter()
            .map(|index| (MIB + (*index as u64 - 1) * (SIZE + MIB)).to_string());
        if members.len() != fixture.members.len()
            || expected.into_iter().any(|offset| {
                !members.iter().any(|member| {
                    member["format"] == "VHDX"
                        && member["files"] == serde_json::json!([fixture.container])
                        && member["offset"] == offset
                        && member["size"] == SIZE.to_string()
                })
            })
        {
            return Err("LVM2 provenance differs from the original PV ranges".into());
        }
    }
    Ok(format!("PASS. Exports from {} and {} match the Linux digests and original VHDX ranges. Source image, both GPTs, and guest OS layout match.",
        fixture.volumes[0], fixture.volumes[1]))
}

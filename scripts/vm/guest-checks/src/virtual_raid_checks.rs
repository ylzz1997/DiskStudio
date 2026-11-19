//! Native RAID acceptance with ordinary files on the isolated A/B disks.
use super::*;

const FOLDER: &str = "DSRAID-Members";
const DESTINATION: &str = "DSRAID-Export";
const MEMBER_BYTES: u64 = 25 * MIB;
const NAMES: [&str; 3] = ["Virtual-Raid-1.raw", "Virtual-Raid-2.raw", "Virtual-Raid-3.raw"];
const FILE: (&str, u64, &str) = ("far.bin", 1179648, "6a4898dcd919cf9224786a66d56dc7857c140f6c8bdf0175e0bb63ac10ab9607");

#[derive(Serialize, Deserialize)]
struct Report {
    sources: [String; 3],
    tables: [String; 2],
    os: String,
}

fn receipt(sha: &str) -> PathBuf {
    history_report("virtual-raid", sha)
}

fn source() -> Result<PathBuf, String> {
    let root = PathBuf::from(r"A:\");
    lvm::images::check_dir(&root, A, None)?;
    Ok(root.join(FOLDER))
}

pub(crate) fn prepare() -> Result<String, String> {
    let sha = desktop()?;
    if receipt(&sha).exists() {
        return Err("Preserve the existing virtual RAID fixture".into());
    }
    online(A)?;
    online(B)?;
    let input = std::env::current_exe().map_err(err)?;
    let source = source()?;
    let destination = history_destination(DESTINATION)?;
    if source.try_exists().map_err(err)? || destination.try_exists().map_err(err)? {
        return Err("Preserve existing fixture directories".into());
    }
    let expected = option_env!("DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256").ok_or("Missing member digests")?;
    let expected: [&str; 3] = expected.split(',').collect::<Vec<_>>().try_into().map_err(|_| "Expected three member digests")?;
    let report = Report {
        sources: expected.map(str::to_owned),
        tables: history_tables()?,
        os: reallocation_checks::os_layout(&inventory()?)?,
    };
    for (name, digest) in NAMES.into_iter().zip(&report.sources) {
        let path = input.with_file_name(name);
        if path.metadata().map_err(err)?.len() != MEMBER_BYTES || allocation_batch::hash_file(&path)? != *digest {
            return Err("Optical member differs".into());
        }
    }
    data_windows::destination_folder(source.parent().ok_or("Missing A root")?, &selection(B, None)?.target, 3 * MEMBER_BYTES)?;
    fs::create_dir(&source).map_err(err)?;
    for (name, digest) in NAMES.into_iter().zip(&report.sources) {
        let mut raw = File::open(input.with_file_name(name)).map_err(err)?;
        let path = source.join(name);
        let mut target = lvm::images::create_fixture(&path)?;
        if std::io::copy(&mut raw, &mut target).map_err(err)? != MEMBER_BYTES {
            return Err("Short RAID member copy".into());
        }
        target.sync_all().map_err(err)?;
        if allocation_batch::hash_file(&path)? != *digest {
            return Err("Staged member failed SHA-256 readback".into());
        }
    }
    fs::create_dir(&destination).map_err(err)?;
    lvm::images::check_dir(&destination, B, None)?;
    if report.tables != history_tables()? || report.os != reallocation_checks::os_layout(&inventory()?)? {
        return Err("A/B GPT or OS layout changed during staging".into());
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(receipt(&sha)).map_err(err)?;
    serde_json::to_writer(&mut file, &report).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!("PASS. Select the three RAW files in {}. Use RAID5 left symmetric, 64 KiB, partition scope, offsets 1048576 and sizes 25165824. Export nested to {}.", source.display(), destination.display()))
}

pub(crate) fn inspect() -> Result<String, String> {
    let sha = desktop()?;
    let report: Report = serde_json::from_reader(File::open(receipt(&sha)).map_err(err)?).map_err(err)?;
    let source = source()?;
    let destination = history_destination(DESTINATION)?;
    lvm::images::check_dir(&source, A, None)?;
    lvm::images::check_dir(&destination, B, None)?;
    if report.tables != history_tables()? || report.os != reallocation_checks::os_layout(&inventory()?)? {
        return Err("Protected layouts changed".into());
    }
    for (name, hash) in NAMES.into_iter().zip(&report.sources) {
        if allocation_batch::hash_file(&source.join(name))? != *hash {
            return Err("Source member bytes changed".into());
        }
    }
    verify_recovered(&destination, &[FILE])?;
    let result = fs::read_dir(&destination).map_err(err)?.next().ok_or("Missing RAID output")?.map_err(err)?.path();
    if !result.join("nested").join(FILE.0).is_file() {
        return Err("Nested output is missing".into());
    }
    let manifest: serde_json::Value = serde_json::from_reader(
        File::open(result.join(recovery_export::REPORT)).map_err(err)?,
    ).map_err(err)?;
    let proof = &manifest["virtualRaidSource"];
    if proof["config"] != serde_json::json!({"layout":"raid5LeftSymmetric","sector":512,"stripe":65536})
        || proof["checkedRows"] != 16
        || proof["members"] != serde_json::json!(NAMES.map(|name| serde_json::json!({"fileName":name,"offset":"1048576","size":"25165824"}))) {
        return Err("Export manifest differs from the checked member order, ranges or parameters".into());
    }
    Ok("PASS. RAID5 output SHA-256, nested directory and export parameters match. Three member digests and both GPTs and OS layout match.".into())
}

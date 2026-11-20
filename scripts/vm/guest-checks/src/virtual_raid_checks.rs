//! Native RAID acceptance with ordinary files on the isolated A/B disks.
use super::*;

#[path = "virtual_raid_device_checks.rs"]
pub(crate) mod devices;

const FILE: (&str, u64, &str) = ("far.bin", 1179648, "6a4898dcd919cf9224786a66d56dc7857c140f6c8bdf0175e0bb63ac10ab9607");
const RAID5: Fixture = Fixture {
    folder: "DSRAID-Members",
    destination: "DSRAID-Export",
    label: "virtual-raid",
    names: &["Virtual-Raid-1.raw", "Virtual-Raid-2.raw", "Virtual-Raid-3.raw"],
    member_size: 25 * MIB,
    start: MIB,
    size: 24 * MIB,
    file: FILE,
};
const MD6: Fixture = Fixture {
    folder: "DSMD6-Members",
    destination: "DSMD6-Export",
    label: "virtual-raid-md6",
    names: &[
        "Virtual-Raid-Md6-0.raw",
        "Virtual-Raid-Md6-1.raw",
        "Virtual-Raid-Md6-2.raw",
        "Virtual-Raid-Md6-3.raw",
        "Virtual-Raid-Md6-4.raw",
    ],
    member_size: 12 * MIB,
    start: 0,
    size: 12 * MIB,
    file: ("proof.bin", 1179648, ""),
};
const MD_METADATA: Fixture = Fixture {
    folder: "DSMD-Members",
    destination: "DSMD-Export",
    label: "virtual-raid-md-metadata",
    names: &[
        "Virtual-Raid-Md-native-0.raw",
        "Virtual-Raid-Md-native-1.raw",
        "Virtual-Raid-Md-native-2.raw",
        "Virtual-Raid-Md-native-3.raw",
        "Virtual-Raid-Md-native-4.raw",
    ],
    member_size: 16 * MIB,
    start: MIB,
    ..MD6
};
const MD10: Fixture = Fixture {
    folder: "DSMD10-Members",
    destination: "DSMD10-Export",
    label: "virtual-raid-md10",
    names: &[
        "Virtual-Raid-Md-o2-0.raw",
        "Virtual-Raid-Md-o2-1.raw",
        "Virtual-Raid-Md-o2-2.raw",
        "Virtual-Raid-Md-o2-3.raw",
        "Virtual-Raid-Md-o2-4.raw",
    ],
    ..MD_METADATA
};
const MD10_ADVANCED: Fixture = Fixture {
    folder: "DSMD10-Advanced-Members",
    destination: "DSMD10-Advanced-Export",
    label: "virtual-raid-md10-advanced",
    names: &[
        "Virtual-Raid-Md-mixed-offset-0.raw",
        "Virtual-Raid-Md-mixed-offset-1.raw",
        "Virtual-Raid-Md-mixed-offset-2.raw",
        "Virtual-Raid-Md-mixed-offset-3.raw",
        "Virtual-Raid-Md-mixed-offset-4.raw",
        "Virtual-Raid-Md-mixed-offset-5.raw",
    ],
    ..MD10
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Raid5,
    Md6,
    MdMetadata,
    Md10,
    Md10Advanced,
}

impl Mode {
    fn fixture(self) -> Fixture {
        match self {
            Self::Raid5 => RAID5,
            Self::Md6 => MD6,
            Self::MdMetadata => MD_METADATA,
            Self::Md10 => MD10,
            Self::Md10Advanced => MD10_ADVANCED,
        }
    }

    fn variable_volume(self) -> bool {
        matches!(self, Self::Md10 | Self::Md10Advanced)
    }
}

#[derive(Clone, Copy)]
struct Fixture {
    folder: &'static str,
    destination: &'static str,
    label: &'static str,
    names: &'static [&'static str],
    member_size: u64,
    start: u64,
    size: u64,
    file: (&'static str, u64, &'static str),
}

impl Fixture {
    fn receipt(self, sha: &str) -> PathBuf {
        history_report(self.label, sha)
    }

    fn source(self, mode: Mode, recorded: Option<u32>) -> Result<(PathBuf, Option<u32>), String> {
        if !mode.variable_volume() {
            let root = PathBuf::from(r"A:\");
            lvm::images::check_dir(&root, A, None)?;
            return Ok((root.join(self.folder), None));
        }
        let disk_number = test_a()?.number;
        let snapshot = storage::read_snapshot()?;
        let disk = snapshot.disks.iter().find(|disk| disk.hardware.number == disk_number)
            .ok_or("Missing test A snapshot")?;
        let required = self.names.len() as u64 * self.member_size + 16 * MIB;
        let (partition, letter) = disk.partitions.iter().filter_map(|partition| {
            if partition.hidden || partition.base.system
                || !matches!(partition.base.fs.as_str(), "NTFS" | "FAT32" | "exFAT")
            {
                return None;
            }
            let matches = recorded.map_or_else(
                || partition.available.is_some_and(|bytes| bytes >= required),
                |number| partition.number == number,
            );
            matches.then_some((partition, partition.drive_letter.as_deref()?))
        }).max_by_key(|(partition, _)| partition.available)
            .ok_or("No suitable test A data volume for the RAID10 fixture")?;
        let root = PathBuf::from(format!("{letter}:\\"));
        lvm::images::check_dir(&root, A, Some(partition.number))?;
        Ok((root.join(self.folder), Some(partition.number)))
    }

    fn member(self, index: usize) -> serde_json::Value {
        serde_json::json!({
            "fileName": self.names[index],
            "offset": self.start.to_string(),
            "size": self.size.to_string(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Report {
    sources: Vec<String>,
    #[serde(default)]
    source_partition: Option<u32>,
    tables: [String; 2],
    os: String,
}

pub(crate) fn prepare(mode: Mode) -> Result<String, String> {
    let fixture = mode.fixture();
    let sha = desktop()?;
    if fixture.receipt(&sha).exists() {
        return Err("Preserve the existing virtual RAID fixture".into());
    }
    online(A)?;
    online(B)?;
    let input = std::env::current_exe().map_err(err)?;
    let (source, source_partition) = fixture.source(mode, None)?;
    let destination = history_destination(fixture.destination)?;
    if source.try_exists().map_err(err)? || destination.try_exists().map_err(err)? {
        return Err("Preserve existing fixture directories".into());
    }
    let expected = option_env!("DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256").ok_or("Missing member digests")?;
    let report = Report {
        sources: expected.split(',').map(str::to_owned).collect(),
        source_partition,
        tables: history_tables()?,
        os: reallocation_checks::os_layout(&inventory()?)?,
    };
    if report.sources.len() != fixture.names.len() {
        return Err("Unexpected number of member digests".into());
    }
    for (&name, digest) in fixture.names.iter().zip(&report.sources) {
        let path = input.with_file_name(name);
        if path.metadata().map_err(err)?.len() != fixture.member_size || allocation_batch::hash_file(&path)? != *digest {
            return Err("Optical member differs".into());
        }
    }
    data_windows::destination_folder(source.parent().ok_or("Missing A root")?, &selection(B, None)?.target, fixture.names.len() as u64 * fixture.member_size)?;
    fs::create_dir(&source).map_err(err)?;
    for (&name, digest) in fixture.names.iter().zip(&report.sources) {
        let mut raw = File::open(input.with_file_name(name)).map_err(err)?;
        let path = source.join(name);
        let mut target = lvm::images::create_fixture(&path)?;
        if std::io::copy(&mut raw, &mut target).map_err(err)? != fixture.member_size {
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
    let mut file = OpenOptions::new().create_new(true).write(true).open(fixture.receipt(&sha)).map_err(err)?;
    serde_json::to_writer(&mut file, &report).map_err(err)?;
    file.sync_all().map_err(err)?;
    Ok(format!("PASS. Select the RAW files in {}. Member offset {}, size {}. Export nested to {}.", source.display(), fixture.start, fixture.size, destination.display()))
}

pub(crate) fn inspect(mode: Mode) -> Result<String, String> {
    let fixture = mode.fixture();
    let sha = desktop()?;
    let report: Report = serde_json::from_reader(File::open(fixture.receipt(&sha)).map_err(err)?).map_err(err)?;
    if mode.variable_volume() && report.source_partition.is_none() {
        return Err("Missing RAID10 staging volume receipt".into());
    }
    let (source, _) = fixture.source(mode, report.source_partition)?;
    let destination = history_destination(fixture.destination)?;
    lvm::images::check_dir(&source, A, None)?;
    lvm::images::check_dir(&destination, B, None)?;
    if report.tables != history_tables()? || report.os != reallocation_checks::os_layout(&inventory()?)? {
        return Err("Protected layouts changed".into());
    }
    if report.sources.len() != fixture.names.len() {
        return Err("Missing source receipts".into());
    }
    for (&name, hash) in fixture.names.iter().zip(&report.sources) {
        if allocation_batch::hash_file(&source.join(name))? != *hash {
            return Err("Source member bytes changed".into());
        }
    }
    let (name, size, digest) = fixture.file;
    let digest = if mode != Mode::Raid5 {
        option_env!("DISKSTUDIO_VIRTUAL_RAID_MD6_FILE_SHA256").ok_or("Missing Linux file digest")?
    } else {
        digest
    };
    verify_recovered(&destination, &[(name, size, digest)])?;
    let result = fs::read_dir(&destination).map_err(err)?.next().ok_or("Missing RAID output")?.map_err(err)?.path();
    if !result.join("nested").join(name).is_file() {
        return Err("Nested output is missing".into());
    }
    let manifest: serde_json::Value = serde_json::from_reader(
        File::open(result.join(recovery_export::REPORT)).map_err(err)?,
    ).map_err(err)?;
    let proof = &manifest["virtualRaidSource"];
    let (config, checked_rows) = match mode {
        Mode::Raid5 => (serde_json::json!({"layout":"raid5LeftSymmetric","sector":512,"stripe":65536}), 16),
        Mode::Md6 | Mode::MdMetadata => (serde_json::json!({"layout":"raid6LeftSymmetric","sector":4096,"stripe":4096}), 0),
        Mode::Md10 => (serde_json::json!({"layout":{"raid10Md":{"format":"offset","copies":2,"farSets":false}},"sector":4096,"stripe":65536}), 0),
        Mode::Md10Advanced => (serde_json::json!({"layout":{"raid10MdAdvanced":{"nearCopies":2,"farCopies":2,"farOffset":true,"groups":"all"}},"sector":4096,"stripe":65536}), 0),
    };
    let members = (0..fixture.names.len()).map(|index| {
        if mode == Mode::Raid5 || index % 2 == 0 {
            fixture.member(index)
        } else {
            serde_json::json!({"fileName":null,"offset":"0","size":fixture.size.to_string()})
        }
    }).collect();
    if proof["config"] != config
        || proof["checkedRows"] != checked_rows
        || proof["members"] != serde_json::Value::Array(members)
    {
        return Err("Export manifest differs from the checked member order, ranges or parameters".into());
    }
    if mode == Mode::MdMetadata || mode.variable_volume() {
        check_md_provenance(proof, File::open(source.join(fixture.names[0])).map_err(err)?)?;
    }
    Ok("PASS. Export SHA-256, nested directory, array parameters, source members, A/B GPTs and OS layout match.".into())
}

fn check_md_provenance(
    proof: &serde_json::Value,
    mut reference: impl Read + Seek,
) -> Result<(), String> {
    let mut bytes = [0; 224];
    reference.seek(SeekFrom::Start(4096)).map_err(err)?;
    reference.read_exact(&mut bytes).map_err(err)?;
    let uuid: String = bytes[16..32].iter().map(|byte| format!("{byte:02x}")).collect();
    let events = u64::from_le_bytes(bytes[200..208].try_into().unwrap()).to_string();
    let metadata = &proof["mdMetadata"];
    if metadata["version"] != "1.2"
        || metadata["arrayUuid"] != uuid
        || metadata["events"] != events
        || !metadata["fingerprint"].as_str().is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("Linux md provenance does not match the independent member".into());
    }
    Ok(())
}

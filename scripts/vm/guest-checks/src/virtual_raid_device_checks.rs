//! Seeds and verifies a table-less RAID fixture on the isolated 4Kn A disk.
use super::{super::*, FILE};

const NAMES: [&str; 3] = [
    "Virtual-Raid-Device-1.raw",
    "Virtual-Raid-Device-2.raw",
    "Virtual-Raid-Device-Whole.raw",
];
const SIZES: [u64; 3] = [32 * MIB, 32 * MIB, 64 * MIB];
const PARTITIONS: &str = "DSRAID-Physical";
const WHOLE: &str = "DSRAID-Whole";
const MD_NAMES: [&str; 3] = [
    "Virtual-Raid-Md-alternate-0.raw",
    "Virtual-Raid-Md-alternate-1.raw",
    "Virtual-Raid-Md-alternate-2.raw",
];
const MD_SIZES: [u64; 3] = [20 * MIB, 6 * MIB, 13 * MIB];
const MD_OUTPUT: &str = "DSMD-Zoned";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Manual,
    MdZones,
}

#[derive(Clone, Copy)]
struct Fixture {
    names: [&'static str; 3],
    sizes: [u64; 3],
    destinations: &'static [&'static str],
    label: &'static str,
}

impl Mode {
    fn fixture(self) -> Fixture {
        match self {
            Self::Manual => Fixture {
                names: NAMES,
                sizes: SIZES,
                destinations: &[PARTITIONS, WHOLE],
                label: "raid-devices",
            },
            Self::MdZones => Fixture {
                names: MD_NAMES,
                sizes: MD_SIZES,
                destinations: &[MD_OUTPUT],
                label: "raid-md-zoned-devices",
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    extents: [Witness; 3],
    member_sha256: [String; 3],
    tables: [String; 2],
    os: String,
}

fn check_blank(reader: &mut (impl Read + Seek), size: u64) -> Result<(), String> {
    if size == 0 || size % MIB != 0 {
        return Err("Invalid physical RAID test extent size".into());
    }
    reader.rewind().map_err(err)?;
    let mut bytes = vec![0; MIB as usize];
    for _ in 0..size / MIB {
        reader.read_exact(&mut bytes).map_err(err)?;
        if bytes.iter().any(|&value| value != 0) {
            return Err("The new RAID test extent was not blank".into());
        }
    }
    Ok(())
}

fn optical_members(fixture: Fixture) -> Result<(Vec<File>, [String; 3]), String> {
    let digests =
        option_env!("DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256").ok_or("Missing fixture digests")?;
    let member_sha256: [String; 3] = digests
        .split(',')
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "Expected three fixture digests")?;
    let exe = std::env::current_exe().map_err(err)?;
    let mut files = Vec::new();
    for ((name, size), hash) in fixture
        .names
        .into_iter()
        .zip(fixture.sizes)
        .zip(&member_sha256)
    {
        let path = exe.with_file_name(name);
        let file = File::open(&path).map_err(err)?;
        if file.metadata().map_err(err)?.len() != size
            || allocation_batch::hash_file(&path)? != *hash
        {
            return Err("Physical RAID media differs from the independent fixture".into());
        }
        files.push(file);
    }
    Ok((files, member_sha256))
}

fn save_receipt(report: PathBuf, receipt: &Receipt) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(report)
        .map_err(err)?;
    serde_json::to_writer(&mut file, receipt).map_err(err)?;
    file.sync_all().map_err(err)
}

fn md_output(extents: &[Witness; 3], output: &Path) -> String {
    format!(
        "PASS. A remains offline. Select A partitions {} (20 MiB), {} (6 MiB), {} (13 MiB). Detect Linux md with 4096 B sectors. Search the table-less array for Ext4 at LBA 4352. Export nested to {}.",
        extents[0].partition, extents[1].partition, extents[2].partition, output.display(),
    )
}

fn new_blank_linux_extent(size: u64) -> Result<Witness, String> {
    let a = test_a()?;
    let original = format_checks::layouts(&[])?;
    let original_gpt = table_restore::archive_layout(&quick_partition_checks::table(A)?)?;
    let status = partitions::platform::status()?;
    let target = status
        .targets
        .iter()
        .find(|t| t.serial == A)
        .ok_or("Test A is unavailable")?;
    let source = selection(A, None)?;
    if source.target != target.target || source.layout_key != target.layout_key {
        return Err("Test A changed while locating a free extent".into());
    }
    let stable = data_windows::StableRead::lock(&source)?;
    let resolved = stable.resolved()?;
    let mut offset = None;
    for gap in target
        .free_ranges
        .iter()
        .rev()
        .filter(|gap| gap.size >= size)
        .take(8)
    {
        let end = gap
            .offset
            .checked_add(gap.size)
            .ok_or("Invalid free range")?;
        let start = (end - size) / MIB * MIB;
        if start < gap.offset
            || start / 4096 < original_gpt.first
            || (start + size - 1) / 4096 > original_gpt.last
            || original_gpt
                .entries
                .iter()
                .any(|entry| entry.offset < start + size && start < entry.offset + entry.size)
        {
            continue;
        }
        let mut probe = data_io::Region::new(data_windows::device(&resolved, false)?, start, size)
            .map_err(err)?;
        match check_blank(&mut probe, size) {
            Ok(()) => {
                offset = Some(start);
                break;
            }
            Err(error) if error == "The new RAID test extent was not blank" => continue,
            Err(error) => return Err(error),
        }
    }
    let offset = offset.ok_or("No verified blank RAID fixture extent on test A")?;
    stable.verify()?;
    drop(stable);
    online(A)?;
    if table_restore::archive_layout(&quick_partition_checks::table(A)?)? != original_gpt {
        return Err("Test A GPT changed before partition creation".into());
    }
    operation(
        A,
        partitions::Action::Create {
            offset,
            size,
            kind: partitions::PartitionKind::Linux,
        },
    )?;
    let selected = reallocation_checks::metadata_partition(A, None, offset / MIB, size / MIB)?;
    let mut after = table_restore::archive_layout(&quick_partition_checks::table(A)?)?;
    after.entries.retain(|entry| {
        !(entry.offset == offset
            && entry.size == size
            && entry.kind == partitions::policy::LINUX_DATA
            && entry.attributes == 0)
    });
    if after != original_gpt || format_checks::layouts(&[(a.number, selected.number)])? != original
    {
        return Err("Other partitions changed during new RAID fixture creation".into());
    }
    witness(selected.number, offset)
}

pub(crate) fn prepare(mode: Mode) -> Result<String, String> {
    let fixture = mode.fixture();
    let sha = desktop()?;
    let report = history_report(fixture.label, &sha);
    if report.exists() {
        return Err("Physical RAID fixture already prepared".into());
    }
    let (mut files, member_sha256) = optical_members(fixture)?;
    let log_name = match mode {
        Mode::Manual => "virtual-raid-4kn-devices-prepare",
        Mode::MdZones => "virtual-raid-md-zoned-4kn-devices-prepare",
    };
    let mut log = backup_protection::begin(log_name)?;
    online(B)?;
    let outputs = fixture
        .destinations
        .iter()
        .map(|name| history_destination(name))
        .collect::<Result<Vec<_>, _>>()?;
    for path in &outputs {
        if path.try_exists().map_err(err)? {
            return Err("Preserve the existing RAID check output directory".into());
        }
    }
    let initial = format_checks::layouts(&[])?;
    let [_, b] = history_tables()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    let a = test_a()?.number;
    let mut created = Vec::new();
    for size in fixture.sizes {
        let item = if mode == Mode::MdZones {
            new_blank_linux_extent(size)?
        } else {
            create_linux_fixture(A, size)?
        };
        created.push((item.partition, item.offset));
    }
    let new_partitions = created
        .iter()
        .map(|&(partition, _)| (a, partition))
        .collect::<Vec<_>>();
    if format_checks::layouts(&new_partitions)? != initial {
        return Err("Unexpected change to an existing fixture or OS layout".into());
    }
    let extents = created
        .into_iter()
        .map(|(partition, offset)| witness(partition, offset))
        .collect::<Result<Vec<_>, String>>()?;
    let extents: [Witness; 3] = extents
        .try_into()
        .map_err(|_| "Expected three test extents")?;
    for (((extent, size), file), hash) in extents
        .iter()
        .zip(fixture.sizes)
        .zip(&mut files)
        .zip(&member_sha256)
    {
        seed_linux_fixture(
            A,
            extent,
            size,
            file,
            hash,
            |reader| check_blank(reader, size),
            |_| Ok(()),
        )?;
    }
    let tables = history_tables()?;
    if tables[1] != b || reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("Protected B or OS layout changed".into());
    }
    for path in &outputs {
        fs::create_dir(path).map_err(err)?;
    }
    let output = match mode {
        Mode::Manual => format!(
            "PASS. A remains offline. RAID0: select A partitions {} then {}; use 64 KiB stripe, 4096 B sectors and table-less search. Export to {}. RAID1: use A entire disk, member offset {} and length {}, add one missing member, then search and export to {}. Ext4 starts at LBA 1024.",
            extents[0].partition, extents[1].partition, outputs[0].display(), extents[2].offset,
            SIZES[2], outputs[1].display(),
        ),
        Mode::MdZones => md_output(&extents, &outputs[0]),
    };
    let receipt = Receipt {
        extents,
        member_sha256,
        tables,
        os,
    };
    save_receipt(report, &receipt)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({
            "result":"pass", "partition": new_partitions, "wholeOffset": receipt.extents[2].offset,
        }),
    )?;
    Ok(output)
}

pub(crate) fn finish_zones() -> Result<String, String> {
    let fixture = Mode::MdZones.fixture();
    let sha = desktop()?;
    let report = history_report(fixture.label, &sha);
    let destination = history_destination(MD_OUTPUT)?;
    if report.exists()
        || destination.try_exists().map_err(err)?
        || test_a()?.is_offline != Some(true)
    {
        return Err(
            "Existing output or an unexpected source state prevents fixture continuation".into(),
        );
    }
    let (mut image_files, member_sha256) = optical_members(fixture)?;
    let mut log = backup_protection::begin("virtual-raid-md-zoned-4kn-continue")?;
    let gpt = table_restore::archive_layout(&quick_partition_checks::table(A)?)?;
    let [_, b] = history_tables()?;
    let os = reallocation_checks::os_layout(&inventory()?)?;
    let mut existing = Vec::new();
    for (index, &size) in MD_SIZES[..2].iter().enumerate() {
        let mut matches = Vec::new();
        for entry in gpt.entries.iter().filter(|entry| {
            entry.size == size
                && entry.kind == partitions::policy::LINUX_DATA
                && entry.attributes == 0
                && entry.offset % MIB == 0
        }) {
            let part =
                reallocation_checks::metadata_partition(A, None, entry.offset / MIB, size / MIB)?;
            let source = selection(A, Some(part.number))?;
            let locked = data_windows::StableRead::lock(&source)?;
            let resolved = locked.resolved()?;
            if resolved.offset != entry.offset || resolved.size != size {
                return Err("Existing RAID source geometry changed".into());
            }
            let digest =
                data_io::hash_exact(&mut data_windows::reader(&resolved)?, size, |_| Ok(()))?;
            locked.verify()?;
            if digest == member_sha256[index] {
                matches.push((part.number, entry.offset));
            }
        }
        let [matched] = matches.as_slice() else {
            return Err("Could not uniquely identify the two already seeded members".into());
        };
        existing.push(*matched);
    }
    allocation_batch::record(&mut log, serde_json::json!({"verifiedExisting":existing}))?;
    let size = MD_SIZES[2];
    let fresh = new_blank_linux_extent(size)?;
    seed_linux_fixture(
        A,
        &fresh,
        size,
        &mut image_files[2],
        &member_sha256[2],
        |reader| check_blank(reader, size),
        |_| Ok(()),
    )?;
    let extents = [
        witness(existing[0].0, existing[0].1)?,
        witness(existing[1].0, existing[1].1)?,
        fresh,
    ];
    let tables = history_tables()?;
    if tables[1] != b || reallocation_checks::os_layout(&inventory()?)? != os {
        return Err("Protected B or OS layout changed during continuation".into());
    }
    fs::create_dir(&destination).map_err(err)?;
    let output = md_output(&extents, &destination);
    let receipt = Receipt {
        extents,
        member_sha256,
        tables,
        os,
    };
    save_receipt(report, &receipt)?;
    allocation_batch::record(
        &mut log,
        serde_json::json!({"result":"pass","replacementOffset":receipt.extents[2].offset}),
    )?;
    Ok(output)
}

fn export_proof(output: &Path, file: (&str, u64, &str)) -> Result<serde_json::Value, String> {
    verify_recovered(output, &[file])?;
    let result = fs::read_dir(output)
        .map_err(err)?
        .next()
        .ok_or("Missing RAID output")?
        .map_err(err)?
        .path();
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(result.join(recovery_export::REPORT)).map_err(err)?)
            .map_err(err)?;
    if !result.join("nested").join(file.0).is_file() {
        return Err("Export did not keep the nested directory".into());
    }
    Ok(manifest["virtualRaidSource"].clone())
}

fn check_export(
    proof: &serde_json::Value,
    layout: &str,
    stripe: u64,
    members: serde_json::Value,
    range: (u64, u64),
) -> Result<(), String> {
    if proof["config"] != serde_json::json!({"layout":layout,"sector":4096,"stripe":stripe})
        || proof["members"] != members
        || proof["partition"]["offset"] != range.0.to_string()
        || proof["partition"]["size"] != range.1.to_string()
        || proof["partition"]["sector"] != 4096
        || proof["recoveredFilesystem"] != "Ext4"
    {
        return Err(format!(
            "The {layout} export directory or source proof differs"
        ));
    }
    Ok(())
}

fn member(
    disk: u32,
    partition: Option<u32>,
    extent_offset: u64,
    extent_size: u64,
    start: u64,
    len: u64,
) -> serde_json::Value {
    serde_json::json!({
        "fileName":null,
        "device":{"diskNumber":disk,"partition":partition,"extentOffset":extent_offset.to_string(),"extentSize":extent_size.to_string(),"sector":4096},
        "offset":start.to_string(),
        "size":len.to_string(),
    })
}

pub(crate) fn inspect(mode: Mode) -> Result<String, String> {
    let fixture = mode.fixture();
    let sha = desktop()?;
    let receipt: Receipt =
        serde_json::from_reader(File::open(history_report(fixture.label, &sha)).map_err(err)?)
            .map_err(err)?;
    let a = test_a()?;
    if a.is_offline != Some(true)
        || receipt.tables != history_tables()?
        || receipt.os != reallocation_checks::os_layout(&inventory()?)?
    {
        return Err("Source state, A/B GPT or OS layout changed".into());
    }
    for (index, extent) in receipt.extents.iter().enumerate() {
        let resolved = data_windows::resolve(&selection(A, Some(extent.partition))?, false, false)?;
        let size = fixture.sizes[index];
        if resolved.offset != extent.offset
            || resolved.size != size
            || data_io::hash_exact(&mut data_windows::reader(&resolved)?, size, |_| Ok(()))?
                != receipt.member_sha256[index]
        {
            return Err("A physical member changed".into());
        }
    }
    if mode == Mode::MdZones {
        let digest = option_env!("DISKSTUDIO_VIRTUAL_RAID_MD6_FILE_SHA256")
            .ok_or("Missing Linux file digest")?;
        let proof = export_proof(
            &history_destination(MD_OUTPUT)?,
            ("proof.bin", 1179648, digest),
        )?;
        let members = [0, 1, 2].map(|index| {
            let extent = &receipt.extents[index];
            member(
                a.number,
                Some(extent.partition),
                extent.offset,
                MD_SIZES[index],
                MIB,
                MD_SIZES[index] - MIB,
            )
        });
        check_export(
            &proof,
            "raid0Alternate",
            MIB,
            serde_json::json!(members),
            (17 * MIB, 16 * MIB),
        )?;
        let first = &receipt.extents[0];
        let resolved = data_windows::resolve(&selection(A, Some(first.partition))?, false, false)?;
        super::check_md_provenance(&proof, data_windows::reader(&resolved)?)?;
        return Ok("PASS. Physical Linux md RAID0 export, nested SHA-256, detected member ranges and metadata, three source hashes and protected layouts match.".into());
    }
    let raid0 = [0, 1].map(|index| {
        let extent = &receipt.extents[index];
        member(
            a.number,
            Some(extent.partition),
            extent.offset,
            SIZES[index],
            0,
            SIZES[index],
        )
    });
    check_export(
        &export_proof(&history_destination(PARTITIONS)?, FILE)?,
        "raid0",
        65536,
        serde_json::json!(raid0),
        (4 * MIB, 32 * MIB),
    )?;
    let raid1 = [
        member(
            a.number,
            None,
            0,
            a.size,
            receipt.extents[2].offset,
            SIZES[2],
        ),
        serde_json::json!({"fileName":null,"offset":"0","size":SIZES[2].to_string()}),
    ];
    check_export(
        &export_proof(&history_destination(WHOLE)?, FILE)?,
        "raid1",
        0,
        serde_json::json!(raid1),
        (4 * MIB, 32 * MIB),
    )?;
    Ok("PASS. Both RAID0 physical-partition and RAID1 whole-disk exports match the Linux file digest, exact members and recovered range. Source data and protected layouts match.".into())
}

#[cfg(test)]
mod tests {
    use super::check_blank;
    use disk_core::MIB;
    use std::io::Cursor;

    #[test]
    fn blank_raid_fixture_check_reads_both_ends_of_the_extent() {
        let mut bytes = vec![0; 2 * MIB as usize];
        assert!(check_blank(&mut Cursor::new(&bytes), 2 * MIB).is_ok());
        *bytes.last_mut().unwrap() = 1;
        assert!(check_blank(&mut Cursor::new(bytes), 2 * MIB).is_err());
    }
}

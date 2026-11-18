//! Guest-only FAT growth coverage for the two fixed NVMe sector profiles.
use super::*;
use allocation_batch::{
    check_file, fixture_selection, record, resolved, set_online, write_fixture,
};
use fat_resize_checks::{create_capacity_fixture, model, resize};
use partitions::{Action, FileSystem};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};

const MIB: u64 = 1024 * 1024;
pub(super) const SAMPLE: &str = "DSSECTOR.BIN";

pub(super) fn sample_bytes(size_mib: u64) -> Vec<u8> {
    (0..65_553)
        .map(|i| ((i * 59 + i / 137 + size_mib as usize) % 251) as u8)
        .collect()
}
const INITIAL_4KN_DRIVER: &str = "41840338705b9f43b19711ebc41992bd460b0f607e57025deb9ba522b644c00c";
const CONTINUE_4KN_DRIVER: &str =
    "1cbc593c1e6e083a4a75aba335a988fbea16383fff4b1da3f820bd9ca1a3eacb";
const CONTINUE_4KN_KIND: &str = "fat-sectors-4kn-continue";
pub const TAIL_4KN_KIND: &str = "fat-sectors-4kn-tail";
pub const FAT32_4KN_KIND: &str = "fat-sectors-4kn-fat32";

struct Case {
    fs: FileSystem,
    label: &'static str,
    small: u64,
    large: u64,
}

const CLASSIC: [Case; 2] = [
    Case {
        fs: FileSystem::Fat16,
        label: "DSSEC16",
        small: 64,
        large: 160,
    },
    Case {
        fs: FileSystem::Fat32,
        label: "DSSEC32",
        small: 320,
        large: 448,
    },
];
const FOUR_KN: [Case; 2] = [
    Case {
        fs: FileSystem::Fat12,
        label: "DSSEC12",
        small: 8,
        large: 12,
    },
    Case {
        fs: FileSystem::Exfat,
        label: "DSSECEX",
        small: 96,
        large: 1088,
    },
];

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SectorSizes {
    number: u32,
    logical_sector_size: Option<u64>,
    physical_sector_size: Option<u64>,
}

pub(super) fn confirm_4kn(disks: &[Disk]) -> Result<(), String> {
    let con =
        WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?;
    let sectors: Vec<SectorSizes> = con
        .raw_query("SELECT Number,LogicalSectorSize,PhysicalSectorSize FROM MSFT_Disk")
        .map_err(err)?;
    for serial in [A, B] {
        let disk = test_disk(disks, serial)?;
        if sectors
            .iter()
            .filter(|row| row.number == disk.number)
            .count()
            != 1
            || !sectors.iter().any(|row| {
                row.number == disk.number
                    && row.logical_sector_size == Some(4096)
                    && row.physical_sector_size == Some(4096)
            })
        {
            return Err("4Kn guest provider did not report both 4096-byte sector sizes".into());
        }
    }
    Ok(())
}

fn test_disk<'a>(disks: &'a [Disk], serial: &str) -> Result<&'a Disk, String> {
    disks
        .iter()
        .find(|disk| disk.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
        .ok_or_else(|| "Bound test disk missing".into())
}

fn raw_empty(disk: &Disk) -> bool {
    disk.partition_style == Some(0) && disk.number_of_partitions == Some(0)
}

pub fn diagnose_4kn() -> Result<String, String> {
    let disks = inventory()?;
    confirm_4kn(&disks)?;
    if !raw_empty(test_disk(&disks, B)?) {
        return Err("Expected the untouched 4Kn B disk".into());
    }
    let (selected, offset) = fixture_selection("FAT16", Some("DSSEC16"), 64 * MIB)?;
    let bound = resolved(&selected, offset, 64 * MIB, false)?;
    model(A)?;
    let mut reader = data_windows::reader(&bound)?;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    let word = |i| u16::from_le_bytes([boot[i], boot[i + 1]]);
    let sector = u64::from(word(11));
    if sector != 4096 || boot[13] != 1 || boot[16] != 2 || word(22) == 0 {
        return Err("FAT16 on-volume geometry did not match the fixture".into());
    }
    let mut headers = Vec::new();
    for copy in 0..2 {
        let start = (u64::from(word(14)) + copy * u64::from(word(22))) * sector;
        reader.seek(SeekFrom::Start(start)).map_err(err)?;
        let mut reserved = [0; 4];
        reader.read_exact(&mut reserved).map_err(err)?;
        headers.push(format!(
            "{:#06x} {:#06x}",
            u16::from_le_bytes([reserved[0], reserved[1]]),
            u16::from_le_bytes([reserved[2], reserved[3]])
        ));
    }
    let bytes = sample_bytes(64);
    let sample = allocation_batch::check_file(&selected, offset, 64 * MIB, SAMPLE, &bytes);
    let windows_state = (|| {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE},
            System::Ioctl::FSCTL_IS_VOLUME_DIRTY,
        };
        let mounted = resolved(&selected, offset, 64 * MIB, true)?;
        let [volume] = mounted.volumes.as_slice() else {
            return Err("Expected one associated volume".into());
        };
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(volume.trim_end_matches('\\'))
            .map_err(err)?;
        let mut flag = [0; 4];
        if data_windows::control(&handle, FSCTL_IS_VOLUME_DIRTY, &[], &mut flag)? != 4 {
            return Err("Missing volume state word".into());
        }
        Ok::<_, String>(u32::from_le_bytes(flag))
    })();
    let mut log = backup_protection::begin("fat-sectors-4kn-diagnose")?;
    record(
        &mut log,
        json!({"event":"read-only-diagnostic", "sector":sector,"partition":selected.partition,
        "offset":offset,"media":boot[21],"bootState":boot[37],"headers":headers,
        "windowsState":windows_state.as_ref().ok(),"windowsError":windows_state.as_ref().err(),
        "sample":sample.as_ref().ok(),"sampleError":sample.as_ref().err()}),
    )?;
    Ok(format!("FAT16 A/{} diagnostic.\nMedia={:#04x}, BootState={:#04x},\nFAT[0] FAT[1] = {}, {}.\nWindows volume state: {windows_state:?}\nSample SHA-256: {sample:?}",
        selected.partition.ok_or("Missing partition")?,boot[21],boot[37],headers[0],headers[1]))
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Geometry {
    sector: u64,
    cluster_bytes: u64,
    fat_sectors: u64,
    heap_sector: u64,
    bitmap_clusters: Option<u64>,
}

fn boot_geometry(
    case: &Case,
    selected: &Selection,
    offset: u64,
    size: u64,
) -> Result<Geometry, String> {
    let bound = resolved(selected, offset, size, false)?;
    let mut reader = data_windows::reader(&bound)?;
    let mut boot = [0; 512];
    reader.read_exact(&mut boot).map_err(err)?;
    let word = |i| u64::from(u16::from_le_bytes([boot[i], boot[i + 1]]));
    let dword = |i: usize| u64::from(u32::from_le_bytes(boot[i..i + 4].try_into().unwrap()));
    let geometry = if case.fs == FileSystem::Exfat {
        if &boot[3..11] != b"EXFAT   " {
            return Err("Missing exFAT boot identity".into());
        }
        let sector = 1u64
            .checked_shl(boot[108].into())
            .ok_or("Invalid exFAT sector shift")?;
        let cluster_bytes = sector
            .checked_shl(boot[109].into())
            .ok_or("Invalid exFAT cluster shift")?;
        if cluster_bytes != 4096 {
            return Err("Unexpected exFAT cluster size".into());
        }
        Geometry {
            sector,
            cluster_bytes,
            fat_sectors: dword(84),
            heap_sector: dword(88),
            bitmap_clusters: Some(dword(92).div_ceil(8).div_ceil(cluster_bytes)),
        }
    } else {
        let sector = word(11);
        if sector != test_sector() || boot[16] != 2 {
            return Err("Guest FAT boot sector size or mirror count differs".into());
        }
        let fat_sectors = if case.fs == FileSystem::Fat32 {
            dword(36)
        } else {
            word(22)
        };
        Geometry {
            sector,
            cluster_bytes: sector * u64::from(boot[13]),
            fat_sectors,
            heap_sector: word(14) + 2 * fat_sectors + (word(17) * 32).div_ceil(sector),
            bitmap_clusters: None,
        }
    };
    if geometry.sector != test_sector() || geometry.cluster_bytes != 4096 {
        return Err("Expected on-volume sector and cluster sizes did not match".into());
    }
    Ok(geometry)
}

/// Windows can finish clearing the mounted FAT dirty flags after a file
/// handle closes. Wait for the expected on-disk state before dispatching.
fn wait_for_clean_header(
    selected: &Selection,
    offset: u64,
    size: u64,
    fs: FileSystem,
) -> Result<(), String> {
    if fs == FileSystem::Fat12 {
        return Ok(());
    }
    let source = resolved(selected, offset, size, false)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let mut reader = data_windows::reader(&source)?;
        let mut boot = [0u8; 512];
        reader.read_exact(&mut boot).map_err(err)?;
        let word = |i| u16::from_le_bytes([boot[i], boot[i + 1]]);
        let dword = |i: usize| u32::from_le_bytes(boot[i..i + 4].try_into().unwrap());
        let clean = if fs == FileSystem::Exfat {
            let flags = word(106);
            if &boot[3..11] != b"EXFAT   " || flags & !2 != 0 {
                return Err("exFAT boot flags show another error or identity changed".into());
            }
            flags == 0
        } else {
            let sector = u64::from(word(11));
            let reserved = u64::from(word(14));
            let fat32 = fs == FileSystem::Fat32;
            let table_sectors = if fat32 {
                u64::from(dword(36))
            } else {
                u64::from(word(22))
            };
            if sector != test_sector() || reserved == 0 || table_sectors == 0 || boot[16] != 2 {
                return Err("FAT geometry changed while waiting for a clean header".into());
            }
            let mut both_clean = true;
            for start in [reserved, reserved + table_sectors] {
                reader.seek(SeekFrom::Start(start * sector)).map_err(err)?;
                let mut header = [0u8; 8];
                reader.read_exact(&mut header).map_err(err)?;
                let (entry0, entry1, dirty_bit, reserved_mask) = if fat32 {
                    (
                        u32::from_le_bytes(header[..4].try_into().unwrap()) & 0x0fffffff,
                        u32::from_le_bytes(header[4..].try_into().unwrap()) & 0x0fffffff,
                        0x08000000,
                        0x0fffff00,
                    )
                } else {
                    (
                        u32::from(u16::from_le_bytes([header[0], header[1]])),
                        u32::from(u16::from_le_bytes([header[2], header[3]])),
                        0x8000,
                        0xff00,
                    )
                };
                if entry0 != (reserved_mask | u32::from(boot[21]))
                    || (entry1 | dirty_bit) != (reserved_mask | 0xff)
                {
                    return Err("FAT reserved entries or media error flag changed".into());
                }
                both_clean &= entry1 & dirty_bit != 0;
            }
            if both_clean && fat32 && sector == 4096 {
                let (main, backup, _) = fat32_boot_pair(&mut reader)?;
                both_clean = main == backup;
            }
            both_clean
        };
        if clean {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("Timed out waiting for Windows to clear the test volume dirty flag".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn validate_case(
    log: &mut fs::File,
    case: &Case,
    offset: u64,
    seeded: Option<u32>,
) -> Result<(), String> {
    let small = case.small * MIB;
    let large = case.large * MIB;
    let bytes = sample_bytes(case.small);
    let (selected, original) = if let Some(number) = seeded {
        let (selected, actual_offset) = fixture_selection(case.fs.name(), Some(case.label), small)?;
        if actual_offset != offset || selected.partition != Some(number) {
            return Err("Previously seeded 4Kn layout differs".into());
        }
        let digest = check_file(&selected, offset, small, SAMPLE, &bytes)?;
        record(
            log,
            json!({"event":"retained-fixture-verified","partition":number,"sha256":digest}),
        )?;
        (selected, model(A)?)
    } else {
        let fixture = create_capacity_fixture(log, (case.fs, case.label, 4096), offset, small)?;
        write_fixture(&fixture.0, offset, small, SAMPLE, &bytes, true)?;
        fixture
    };
    let number = selected.partition.ok_or("Missing fixture partition")?;
    let before = boot_geometry(case, &selected, offset, small)?;
    let mut enlarged = original.clone();
    enlarged
        .entries
        .iter_mut()
        .find(|entry| entry.offset == offset && entry.size == small)
        .ok_or("Fresh GPT fixture missing from layout")?
        .size = large;
    // exFAT is allowed to use existing heap alignment padding.
    let expect_movement = (case.fs != FileSystem::Exfat).then_some(true);
    wait_for_clean_header(&selected, offset, small, case.fs)?;
    resize(
        log,
        case.fs.name(),
        &selected,
        offset,
        small,
        large,
        SAMPLE,
        &bytes,
        expect_movement,
        &enlarged,
    )?;
    let grown = boot_geometry(case, &selection(A, Some(number))?, offset, large)?;
    if grown.fat_sectors <= before.fat_sectors
        || (case.fs != FileSystem::Exfat && grown.heap_sector <= before.heap_sector)
        || (case.fs == FileSystem::Exfat && grown.bitmap_clusters <= before.bitmap_clusters)
    {
        return Err("Native test did not cross the intended FAT or bitmap capacity".into());
    }
    record(
        log,
        json!({"event":"grown-geometry-verified", "filesystem":case.fs.name(), "before":before,"grown":grown}),
    )?;
    set_online(A)?;
    let (after, actual_offset) = fixture_selection(case.fs.name(), Some(case.label), large)?;
    if after.target != selected.target || after.partition != Some(number) || actual_offset != offset
    {
        return Err("Windows did not rediscover the expanded volume".into());
    }
    wait_for_clean_header(&after, offset, large, case.fs)?;
    resize(
        log,
        case.fs.name(),
        &after,
        offset,
        large,
        small,
        SAMPLE,
        &bytes,
        None,
        &original,
    )?;
    let shrunk = boot_geometry(case, &selection(A, Some(number))?, offset, small)?;
    if shrunk.fat_sectors != grown.fat_sectors
        || shrunk.heap_sector != grown.heap_sector
        || shrunk.bitmap_clusters != before.bitmap_clusters
    {
        return Err("Returned volume geometry differs".into());
    }
    record(
        log,
        json!({"event":"case-verified", "filesystem":case.fs.name(), "partition":number, "shrunk":shrunk}),
    )
}

pub fn check() -> Result<String, String> {
    run(None)
}

pub fn continue_4kn() -> Result<String, String> {
    inventory()?;
    let path = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-fat-sectors-4kn-{INITIAL_4KN_DRIVER}.jsonl"
    ));
    if fs::metadata(&path).map_err(err)?.len() > 8192 {
        return Err("Unexpected initial 4Kn log size".into());
    }
    let source = fs::read_to_string(path).map_err(err)?;
    let events = source
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [start, profile, initialized, created, formatted, stopped] = events.as_slice() else {
        return Err("The original 4Kn run did not stop before resize confirmation".into());
    };
    if start["helper"] != INITIAL_4KN_DRIVER
        || start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || profile["event"] != "sector-profile-verified"
        || profile["logicalSector"] != 4096
        || initialized["event"] != "initialize-A-once"
        || created["event"] != "create-fixture-once"
        || created["filesystem"] != "FAT16"
        || created["size"] != 64 * MIB
        || formatted["event"] != "format-fixture-once"
        || formatted["partition"] != 2
        || stopped["event"] != "failed"
        || !stopped["error"]
            .as_str()
            .is_some_and(|error| error.contains("FAT 卷未干净卸载"))
    {
        return Err("Initial 4Kn run identity or stop reason differs".into());
    }
    let offset = created["offset"]
        .as_u64()
        .filter(|&offset| offset == 48 * MIB)
        .ok_or("Initial 4Kn FAT16 offset differs")?;
    run(Some(offset))
}

fn stopped_4kn_fat32() -> Result<(Selection, u64), String> {
    let disks = inventory()?;
    confirm_4kn(&disks)?;
    if !raw_empty(test_disk(&disks, B)?) {
        return Err("Expected the untouched 4Kn B disk".into());
    }
    let path = PathBuf::from(format!(
        r"C:\ProgramData\DiskStudio-{CONTINUE_4KN_KIND}-{CONTINUE_4KN_DRIVER}.jsonl"
    ));
    if fs::metadata(&path).map_err(err)?.len() > 8192 {
        return Err("Unexpected prior 4Kn log size".into());
    }
    let events = fs::read_to_string(path)
        .map_err(err)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let [start, profile, _, _, _, _, _, _, fat16, created, formatted, stopped] = events.as_slice()
    else {
        return Err("The earlier 4Kn run did not stop at FAT32 formatting".into());
    };
    let offset = (48 + CLASSIC[0].large + 4) * MIB;
    if start["event"] != "start"
        || start["helper"] != CONTINUE_4KN_DRIVER
        || start["desktop"].as_str() != option_env!("DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256")
        || profile["event"] != "sector-profile-verified"
        || profile["logicalSector"] != 4096
        || fat16["event"] != "case-verified"
        || fat16["filesystem"] != "FAT16"
        || fat16["partition"] != 2
        || created["event"] != "create-fixture-once"
        || created["filesystem"] != "FAT32"
        || created["offset"] != offset
        || created["size"] != CLASSIC[1].small * MIB
        || formatted["event"] != "format-fixture-once"
        || formatted["filesystem"] != "FAT32"
        || formatted["partition"] != 3
        || stopped["event"] != "failed"
        || stopped["error"] != "FAT32 主备引导扇区不一致。"
    {
        return Err("Prior 4Kn run identity or stop reason differs".into());
    }
    let selected = selection(A, Some(3))?;
    boot_geometry(&CLASSIC[1], &selected, offset, CLASSIC[1].small * MIB)?;
    Ok((selected, offset))
}

pub fn diagnose_4kn_fat32() -> Result<String, String> {
    let (selected, offset) = stopped_4kn_fat32()?;
    model(A)?;
    let bound = resolved(&selected, offset, CLASSIC[1].small * MIB, false)?;
    let mut r = data_windows::reader(&bound)?;
    let (main, backup, backup_sector) = fat32_boot_pair(&mut r)?;
    let differences = main
        .iter()
        .zip(&backup)
        .enumerate()
        .filter(|(_, (a, b))| a != b);
    let count = differences.clone().count();
    let first = differences
        .take(24)
        .map(|(i, (&a, &b))| format!("{i}:{a:02x}/{b:02x}"))
        .collect::<Vec<_>>();
    let mut log = backup_protection::begin("fat-sectors-4kn-fat32-diagnose")?;
    record(
        &mut log,
        json!({"event":"read-only-fat32-boot-comparison","partition":3,"backupSector":backup_sector,"count":count,"first":first}),
    )?;
    Ok(format!("Read-only 4Kn FAT32 A/3. Backup sector {backup_sector}; {count} differing bytes.\nOffset:primary/backup\n{}", first.join("  ")))
}

fn fat32_boot_pair(r: &mut (impl Read + Seek)) -> Result<([u8; 4096], [u8; 4096], u16), String> {
    let mut main = [0u8; 4096];
    r.seek(SeekFrom::Start(0)).map_err(err)?;
    r.read_exact(&mut main).map_err(err)?;
    let word = |i| u16::from_le_bytes([main[i], main[i + 1]]);
    let backup_sector = word(50);
    if word(11) != 4096 || backup_sector == 0 || backup_sector >= word(14) {
        return Err("FAT32 backup sector outside expected reserved area".into());
    }
    let mut backup = [0u8; 4096];
    r.seek(SeekFrom::Start(u64::from(backup_sector) * 4096))
        .map_err(err)?;
    r.read_exact(&mut backup).map_err(err)?;
    let mut repeated = [0u8; 4096];
    r.seek(SeekFrom::Start(0)).map_err(err)?;
    r.read_exact(&mut repeated).map_err(err)?;
    if repeated != main {
        return Err("FAT32 primary boot sector changed during the diagnostic".into());
    }
    Ok((main, backup, backup_sector))
}

pub fn tail_4kn() -> Result<String, String> {
    let (selected, offset) = stopped_4kn_fat32()?;
    let mut log = backup_protection::begin(TAIL_4KN_KIND)?;
    let result = (|| {
        let tail_offset = offset + (CLASSIC[1].large + 4) * MIB;
        let tail_end = tail_offset
            + FOUR_KN
                .iter()
                .map(|case| (case.large + 4) * MIB)
                .sum::<u64>();
        let layout = model(A)?;
        if layout.entries.len() != 3
            || layout
                .entries
                .iter()
                .any(|p| p.offset + p.size > tail_offset)
        {
            return Err("4Kn A layout differs from the previous FAT32 stop".into());
        }
        let eligible = partitions::status().targets.into_iter().any(|t| {
            t.available
                && t.serial == A
                && t.target == selected.target
                && t.layout_key == selected.layout_key
                && t.free_ranges.iter().any(|range| {
                    range.offset <= tail_offset && range.offset + range.size >= tail_end
                })
        });
        if !eligible {
            return Err("The remaining fixed range is no longer available".into());
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(r"C:\ProgramData\DiskStudio-fat-sectors-4kn-tail-20261117.once")
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        record(
            &mut log,
            json!({"event":"sector-profile-verified","logicalSector":4096,"bothPhysical4Kn":true}),
        )?;
        let mut next = tail_offset;
        for case in &FOUR_KN {
            validate_case(&mut log, case, next, None)?;
            next += (case.large + 4) * MIB;
        }
        let disks = inventory()?;
        confirm_4kn(&disks)?;
        if test_disk(&disks, A)?.is_offline != Some(true) || !raw_empty(test_disk(&disks, B)?) {
            return Err("Final 4Kn A offline or B untouched witness differs".into());
        }
        Ok("PASS: 4Kn FAT12 and exFAT grew their allocation structures, preserved samples on growth and shrink, matched GPT and boot geometry. A left offline, B preserved.".into())
    })();
    record(
        &mut log,
        json!({"event":if result.is_ok() {"passed"} else {"failed"},"error":result.as_ref().err(),"result":result.as_ref().ok()}),
    )?;
    result
}

pub fn resume_4kn_fat32() -> Result<String, String> {
    let (_, offset) = stopped_4kn_fat32()?;
    let mut log = backup_protection::begin(FAT32_4KN_KIND)?;
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(r"C:\ProgramData\DiskStudio-fat-sectors-4kn-fat32-20261117.once")
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        set_online(A)?;
        validate_case(&mut log, &CLASSIC[1], offset, Some(3))?;
        let disks = inventory()?;
        confirm_4kn(&disks)?;
        if test_disk(&disks, A)?.is_offline != Some(true) || !raw_empty(test_disk(&disks, B)?) {
            return Err("Final 4Kn A offline or B untouched witness differs".into());
        }
        Ok("PASS: 4Kn FAT32 reused the original sample, expanded its FAT, matched GPT and boot geometry and preserved the data through shrink. A left offline, B preserved.".into())
    })();
    record(
        &mut log,
        json!({"event":if result.is_ok() {"passed"} else {"failed"},"error":result.as_ref().err(),"result":result.as_ref().ok()}),
    )?;
    result
}

fn run(prepared: Option<u64>) -> Result<String, String> {
    let four_kn = test_sector() == 4096;
    let (kind, marker) = if prepared.is_some() {
        (
            CONTINUE_4KN_KIND,
            r"C:\ProgramData\DiskStudio-fat-sectors-4kn-continue-20261117.once",
        )
    } else if four_kn {
        (
            "fat-sectors-4kn",
            r"C:\ProgramData\DiskStudio-fat-sectors-4kn-20261117.once",
        )
    } else {
        (
            "fat-sectors-512",
            r"C:\ProgramData\DiskStudio-fat-sectors-512-20261117.once",
        )
    };
    let mut log = backup_protection::begin(kind)?;
    let result = (|| {
        let disks = inventory()?;
        if four_kn {
            confirm_4kn(&disks)?;
            if !raw_empty(test_disk(&disks, B)?)
                || (prepared.is_none() && !raw_empty(test_disk(&disks, A)?))
            {
                return Err(
                    "4Kn disposable pair does not match the expected starting state".into(),
                );
            }
        } else if model(A)?.guid == model(B)?.guid {
            return Err("A and B share a disk GUID".into());
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(err)?
            .sync_all()
            .map_err(err)?;
        record(
            &mut log,
            json!({"event":"sector-profile-verified", "logicalSector":test_sector(),"bothPhysical4Kn":four_kn}),
        )?;
        set_online(A)?;
        if four_kn && prepared.is_none() {
            record(&mut log, json!({"event":"initialize-A-once"}))?;
            operation(A, Action::Initialize {})?;
        }
        let b_gpt = if four_kn {
            None
        } else {
            Some(quick_partition_checks::table(B)?.encode()?)
        };
        let cases = CLASSIC
            .iter()
            .chain(FOUR_KN.iter().take(if four_kn { FOUR_KN.len() } else { 0 }))
            .collect::<Vec<_>>();
        let required: u64 = cases.iter().map(|case| (case.large + 4) * MIB).sum::<u64>() + 32 * MIB;
        let target = partitions::status()
            .targets
            .into_iter()
            .find(|target| target.available && target.serial == A)
            .ok_or("Exact A target unavailable")?;
        let mut offset = if let Some(offset) = prepared {
            let after_fixture = offset + CLASSIC[0].small * MIB;
            if !target.free_ranges.iter().any(|range| {
                range.offset == after_fixture
                    && range.size >= required - 32 * MIB - CLASSIC[0].small * MIB
            }) {
                return Err("Verified 4Kn continuation range no longer available".into());
            }
            offset
        } else {
            target
                .free_ranges
                .iter()
                .find(|range| range.size >= required)
                .ok_or("Missing contiguous test-only unallocated range on A")?
                .offset
                + 32 * MIB
        };
        for (index, case) in cases.into_iter().enumerate() {
            let seeded = (index == 0 && prepared.is_some()).then_some(2);
            validate_case(&mut log, case, offset, seeded)?;
            offset += (case.large + 4) * MIB;
        }
        let final_disks = inventory()?;
        if four_kn {
            confirm_4kn(&final_disks)?;
            if !raw_empty(test_disk(&final_disks, B)?) {
                return Err("The untouched B fixture is no longer empty RAW".into());
            }
        } else if quick_partition_checks::table(B)?.encode()?
            != b_gpt.ok_or("Missing original B GPT")?
        {
            return Err("B GPT changed".into());
        }
        if test_disk(&final_disks, A)?.is_offline != Some(true) {
            return Err("A was not left offline".into());
        }
        let tested = if four_kn {
            "FAT12/FAT16/FAT32/exFAT"
        } else {
            "FAT16/FAT32"
        };
        Ok(format!("PASS: {tested} at {}-byte logical sectors. Each grew its FAT, preserved the sample across growth and shrink, and matched independent GPT and boot geometry. A left offline, B preserved.", test_sector()))
    })();
    record(
        &mut log,
        json!({"event":if result.is_ok() {"passed"} else {"failed"}, "error":result.as_ref().err(),"result":result.as_ref().ok()}),
    )?;
    result
}

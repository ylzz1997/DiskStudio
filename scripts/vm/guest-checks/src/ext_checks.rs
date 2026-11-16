//! Ext2/3 acceptance on four NEW Linux partitions only. No existing fixtures
//! are formatted. A persisted manifest binds numbers to GUIDs and offsets.
use super::*;
use partitions::{policy::Partition, Action, ExtFileSystem, PartitionKind};
use serde::Serialize;

const MANIFEST: &str = r"C:\ProgramData\DiskStudio-ext23-fixtures.json";
#[derive(Serialize, Deserialize)]
struct Fixture {
    serial: String,
    number: u32,
    offset: u64,
    guid: String,
    file_system: ExtFileSystem,
    block_size: u32,
    label: String,
}
type Layout = (u32, u32, u64, u64, String, String);
#[derive(Serialize, Deserialize)]
struct Manifest {
    phase: String,
    baseline: Vec<Layout>,
    fixtures: Vec<Fixture>,
}
fn parts() -> Result<Vec<Partition>, String> {
    inventory()?;
    WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage").map_err(err)?
        .raw_query("SELECT __PATH,DiskNumber,PartitionNumber,Offset,Size,Guid,GptType,DriveLetter,IsBoot,IsSystem,IsReadOnly,IsOffline,IsHidden,IsShadowCopy FROM MSFT_Partition").map_err(err)
}
fn layout_without(fixtures: &[Fixture]) -> Result<Vec<Layout>, String> {
    let mut layout = parts()?
        .into_iter()
        .filter(|p| !fixtures.iter().any(|f| p.guid.as_deref() == Some(&f.guid)))
        .map(|p| {
            Ok((
                p.disk_number,
                p.partition_number,
                p.offset,
                p.size,
                p.guid.ok_or("Missing partition GUID")?,
                p.gpt_type.ok_or("Missing GPT type")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    layout.sort();
    if layout.is_empty() {
        return Err("Missing baseline".into());
    }
    Ok(layout)
}
fn save(m: &Manifest, first: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true);
    if first {
        options.create_new(true);
    } else {
        options.truncate(true);
    }
    let mut f = options.open(MANIFEST).map_err(err)?;
    f.write_all(&serde_json::to_vec_pretty(m).map_err(err)?)
        .map_err(err)?;
    f.sync_all().map_err(err)
}
fn verify(f: &Fixture, size: u64) -> Result<(), String> {
    let disks = inventory()?;
    let d = disks
        .iter()
        .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(f.serial.as_str()))
        .ok_or("Fixture disk missing")?;
    let candidates = parts()?
        .into_iter()
        .filter(|p| p.disk_number == d.number && p.partition_number == f.number)
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err("Ambiguous fixture partition".into());
    }
    let p = &candidates[0];
    if p.offset != f.offset
        || p.size != size
        || p.guid.as_deref() != Some(&f.guid)
        || p.gpt_type
            .as_deref()
            .map(partitions::policy::guid)
            .as_deref()
            != Some(partitions::policy::LINUX_DATA)
        || p.is_boot != Some(false)
        || p.is_system != Some(false)
    {
        return Err("Fixture identity/layout changed".into());
    }
    Ok(())
}
pub fn format() -> Result<String, String> {
    inventory()?;
    if Path::new(MANIFEST).exists() {
        return Err("Fixture manifest exists; inspect instead of rerunning".into());
    }
    let mut m = Manifest {
        phase: "creating".into(),
        baseline: layout_without(&[])?,
        fixtures: vec![],
    };
    save(&m, true)?; // A partial run cannot silently create or format again.
    for (serial, blocks) in [(A, [1024, 2048]), (B, [4096, 4096])] {
        online(serial)?;
        let status = partitions::status();
        let t = status
            .targets
            .iter()
            .find(|t| t.serial == serial)
            .ok_or("Target missing")?;
        let base = t
            .free_ranges
            .iter()
            .find(|r| r.size >= 3 * GIB)
            .ok_or("Need 3 GiB free range")?
            .offset;
        for (index, file_system) in [ExtFileSystem::Ext2, ExtFileSystem::Ext3]
            .into_iter()
            .enumerate()
        {
            online(serial)?;
            let offset = base + index as u64 * 3 * GIB / 2;
            operation(
                serial,
                Action::Create {
                    offset,
                    size: GIB,
                    kind: PartitionKind::Linux,
                },
            )?;
            let d = inventory()?
                .into_iter()
                .find(|d| d.serial_number.as_deref().map(disk_identity::clean) == Some(serial))
                .ok_or("Disk missing")?;
            let candidates = parts()?
                .into_iter()
                .filter(|p| p.disk_number == d.number && p.offset == offset && p.size == GIB)
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                return Err("New partition ambiguous".into());
            }
            let p = &candidates[0];
            let fixture = Fixture {
                serial: serial.into(),
                number: p.partition_number,
                offset,
                guid: p.guid.clone().ok_or("New GUID missing")?,
                file_system,
                block_size: blocks[index],
                label: format!(
                    "DS{}-{}",
                    if serial == A { "A" } else { "B" },
                    file_system.name()
                ),
            };
            m.fixtures.push(fixture);
            save(&m, false)?;
            if layout_without(&m.fixtures)? != m.baseline {
                return Err("Unrelated layout changed".into());
            }
            let f = m.fixtures.last().unwrap();
            verify(f, GIB)?;
            operation(
                serial,
                Action::ExtFormat {
                    partition: f.number,
                    label: f.label.clone(),
                    file_system,
                    block_size: f.block_size,
                },
            )?;
        }
    }
    m.phase = "formatted".into();
    save(&m, false)?;
    Ok("PASS: four NEW 1 GiB Ext2/Ext3 filesystems, A 1/2 KiB blocks, B 4 KiB blocks. Existing partitions and OS layout unchanged. Next: Ubuntu populate.".into())
}
pub fn resize() -> Result<String, String> {
    inventory()?;
    let mut m: Manifest = serde_json::from_slice(&fs::read(MANIFEST).map_err(err)?).map_err(err)?;
    if m.phase != "formatted" || m.fixtures.len() != 4 {
        return Err("Expected complete format manifest".into());
    }
    for serial in [A, B] {
        if m.fixtures.iter().filter(|f| f.serial == serial).count() != 2 {
            return Err("Fixture serial mismatch".into());
        }
    }
    if layout_without(&m.fixtures)? != m.baseline {
        return Err("Unrelated baseline changed".into());
    }
    for f in &m.fixtures {
        verify(f, GIB)?;
    }
    m.phase = "resizing".into();
    save(&m, false)?;
    for f in &m.fixtures {
        for (before, size) in [(GIB, 384 * 1024 * 1024), (384 * 1024 * 1024, 3 * GIB / 2)] {
            online(&f.serial)?;
            verify(f, before)?;
            operation(
                &f.serial,
                Action::ExtResize {
                    partition: f.number,
                    size,
                },
            )?;
            verify(f, size)?;
            if layout_without(&m.fixtures)? != m.baseline {
                return Err("Unrelated layout changed after resize".into());
            }
        }
    }
    m.phase = "resized".into();
    save(&m, false)?;
    Ok("PASS: four Ext2/Ext3 partitions resized 1 GiB -> 384 MiB -> 1.5 GiB. Native fsck and GPT checks passed; all other partitions and OS layout unchanged. Next: Ubuntu independent hashes and metadata verification.".into())
}

//! Acceptance harness only. Every mutation is confined to the two allowlisted
//! virtual disks; the production backend itself contains no VM identity logic.
use super::*;
use partitions::{Action, PartitionKind};
fn linux_partition(serial: &str) -> Result<u32, String> {
    inventory()?;
    let status = partitions::status();
    let target = status
        .targets
        .iter()
        .find(|t| t.serial == serial)
        .ok_or("Missing test target")?;
    if target.ext_partitions.len() != 1 {
        return Err(format!("Expected one ext4 candidate on {serial}"));
    }
    Ok(target.ext_partitions[0])
}
pub fn online_for_ui() -> Result<String, String> {
    inventory()?;
    for serial in [A, B] {
        online(serial)?;
        let p = linux_partition(serial)?;
        if data_windows::resolve(&selection(serial, Some(p))?, false, false)?.size != 2 * GIB {
            return Err("UI fixture must be exactly 2 GiB".into());
        }
    }
    Ok("PASS: allowlisted A/B online, both Linux partitions exactly 2 GiB. Use the production UI to shrink A to 0.5 GiB; do not format. Then run Ext4-After-UI once.".into())
}
pub fn check(format: bool, resume_created: bool, after_ui: bool) -> Result<String, String> {
    inventory()?;
    let os_before = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS missing")?
        .hardware
        .layout_key
        .ok_or("OS layout identity missing")?;
    for serial in [A, B] {
        online(serial)?;
        if format {
            let status = partitions::status();
            let t = status
                .targets
                .iter()
                .find(|t| t.serial == serial)
                .ok_or("Test target missing")?;
            if t.ext_partitions.is_empty() {
                let gap = t
                    .free_ranges
                    .iter()
                    .find(|g| g.size >= 4 * GIB)
                    .ok_or("Need a contiguous 4 GiB test range")?;
                operation(
                    serial,
                    Action::Create {
                        offset: gap.offset,
                        size: 2 * GIB,
                        kind: PartitionKind::Linux,
                    },
                )?;
            } else if !resume_created {
                return Err(
                    "Linux partition already exists; inspect before explicitly resuming".into(),
                );
            }
            let partition = linux_partition(serial)?;
            if resume_created {
                use std::io::{Read, Seek, SeekFrom};
                let selected = selection(serial, Some(partition))?;
                let target = data_windows::resolve(&selected, false, false)?;
                if target.size != 2 * GIB {
                    return Err("Unexpected existing Linux partition size".into());
                }
                let mut reader = data_windows::reader(&target)?;
                reader.seek(SeekFrom::Start(1080)).map_err(err)?;
                let mut magic = [0; 2];
                reader.read_exact(&mut magic).map_err(err)?;
                if magic == [0x53, 0xef] {
                    return Err(
                        "Existing ext filesystem detected; refusing to repeat formatting".into(),
                    );
                }
            }
            operation(
                serial,
                Action::ExtFormat {
                    partition,
                    file_system: partitions::ExtFileSystem::Ext4,
                    block_size: 4096,
                    label: if serial == A {
                        "DS-A资料"
                    } else {
                        "DS-B资料"
                    }
                    .into(),
                },
            )?;
        } else {
            let partition = linux_partition(serial)?;
            // Populated independently in Ubuntu. Exercise both ordering paths.
            let current =
                data_windows::resolve(&selection(serial, Some(partition))?, false, false)?.size;
            if after_ui && serial == A {
                if current != GIB / 2 {
                    return Err(
                        "A must have been shrunk to 512 MiB through the production UI".into(),
                    );
                }
            } else {
                if current != 2 * GIB {
                    return Err("Expected untouched 2 GiB populated fixture".into());
                }
                operation(
                    serial,
                    Action::ExtResize {
                        partition,
                        size: GIB / 2,
                    },
                )?;
                online(serial)?;
            }
            operation(
                serial,
                Action::ExtResize {
                    partition,
                    size: 3 * GIB,
                },
            )?;
        }
    }
    let os_after = storage::read_snapshot()?
        .disks
        .into_iter()
        .find(|d| d.hardware.is_system)
        .ok_or("OS missing after tests")?
        .hardware
        .layout_key
        .ok_or("OS layout identity missing after tests")?;
    if os_before != os_after {
        return Err("Guest OS partition layout changed".into());
    }
    Ok(if format {"PASS: production backend formatted two 2 GiB ext4 partitions; native fsck and metadata checks passed. Both disks remain offline. Next: Ubuntu mount/populate/hash baseline."} else {"PASS: both populated ext4 partitions completed 2 GiB → 512 MiB → 3 GiB; native fsck and GPT checks passed. OS layout unchanged. Next: Ubuntu independent content verification."}.into())
}

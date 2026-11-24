//! Explicit selection and planning for a batch of independently bound USB disks.
use super::{
    quick::{Format, Progress},
    Extent, Target,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Selection {
    pub target: Target,
    pub revision: String,
    pub partition: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub devices: Vec<Selection>,
    pub format: Format,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Candidate {
    pub target: Target,
    pub revision: String,
    pub name: String,
    pub number: u32,
    pub serial: String,
    pub size: u64,
    pub sector_size: u64,
    pub partitions: Vec<PartitionChoice>,
    pub unpartitioned: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PartitionChoice {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub drive_letter: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Recipe {
    Existing { partition: u32, extent: Extent },
    New { initialize: bool, extent: Extent },
}

impl Recipe {
    pub(super) fn extent(&self) -> &Extent {
        match self {
            Self::Existing { extent, .. } | Self::New { extent, .. } => extent,
        }
    }

    pub(super) fn operation(&self) -> Operation {
        match self {
            Self::Existing { .. } => Operation::Format,
            Self::New {
                initialize: true, ..
            } => Operation::Initialize,
            Self::New {
                initialize: false, ..
            } => Operation::Create,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Operation {
    Initialize,
    Create,
    Format,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevicePlan {
    pub number: u32,
    pub name: String,
    pub serial: String,
    pub partition: Option<u32>,
    pub extent: Extent,
    pub operation: Operation,
    pub drive_letter: Option<String>,
    pub old_file_system: Option<String>,
    pub old_label: Option<String>,
    pub cluster_size: u32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    pub token: String,
    pub format: Format,
    pub devices: Vec<DevicePlan>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ResultState {
    Verified,
    Stopped,
    Skipped,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceResult {
    pub device: DevicePlan,
    pub state: ResultState,
    pub completed: Vec<Operation>,
    pub failure: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Outcome {
    pub devices: Vec<DeviceResult>,
}

#[cfg(any(windows, test))]
pub(super) mod model {
    use super::*;
    use crate::partitions::{policy, Action};
    use std::collections::HashSet;

    const USB: u16 = 7; // MSFT_Disk.BusType USB.
    const MAX_TARGETS: usize = 32;
    const FRONT_GUARD: u64 = 32 * policy::MIB;
    const MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";

    pub(crate) fn validate_selection(devices: &[Selection]) -> Result<(), String> {
        let mut targets = HashSet::new();
        if devices.is_empty()
            || devices.len() > MAX_TARGETS
            || devices
                .iter()
                .any(|choice| !targets.insert(&choice.target.0))
        {
            return Err("请选择 1–32 块不同的 USB 磁盘，每盘最多选择一个目标。".into());
        }
        Ok(())
    }

    pub(in crate::partitions) fn usb(disk: &policy::Disk) -> Result<(), String> {
        if disk.bus_type != Some(USB) {
            return Err("所选设备未由 Windows 确认为 USB 磁盘。".into());
        }
        policy::manageable(disk)
    }

    pub(in crate::partitions) fn recipe(
        disk: &policy::Disk,
        all: &[policy::Partition],
        selected: Option<u32>,
        format: &Format,
    ) -> Result<Recipe, String> {
        usb(disk)?;
        let parts = policy::layout(disk, all)?;
        let recipe = if let Some(number) = selected {
            policy::validate(
                disk,
                all,
                &Action::Format {
                    partition: number,
                    file_system: format.file_system,
                    label: format.label.clone(),
                    cluster_size: format.cluster_size,
                    options: format.options,
                },
            )?;
            let part = parts
                .iter()
                .find(|part| part.partition_number == number)
                .ok_or("分区已发生变化。")?;
            Recipe::Existing {
                partition: number,
                extent: Extent {
                    offset: part.offset,
                    size: part.size,
                },
            }
        } else {
            if !parts.is_empty() || disk.is_boot != Some(false) || disk.is_system != Some(false) {
                return Err("创建新卷需要 Windows 确认没有分区的非系统 USB 磁盘。".into());
            }
            let initialize = disk.partition_style == Some(0);
            if initialize {
                policy::validate(disk, all, &Action::Initialize {})?;
            }
            let offset = if initialize { FRONT_GUARD } else { policy::MIB };
            let end = (disk.size - policy::MIB) / policy::MIB * policy::MIB;
            Recipe::New {
                initialize,
                extent: Extent {
                    offset,
                    size: end - offset,
                },
            }
        };
        format.options.validate(
            format.file_system,
            recipe.extent().size,
            disk.logical_sector_size.ok_or("缺少扇区大小")?,
            &format.label,
            format.cluster_size,
        )?;
        Ok(recipe)
    }

    // Windows may add an MSR during RAW-to-GPT initialization. Only its
    // ordinary 16 MiB reserved entry strictly before the planned range is allowed.
    pub(in crate::partitions) fn reserved(
        parts: &[&policy::Partition],
        extent: &Extent,
        initialize: bool,
    ) -> Result<Option<policy::Partition>, String> {
        match parts {
            [] => Ok(None),
            [part]
                if initialize
                    && part.gpt_type.as_deref().map(policy::guid).as_deref() == Some(MSR)
                    && part.size == 16 * policy::MIB
                    && part
                        .offset
                        .checked_add(part.size)
                        .is_some_and(|end| end <= extent.offset)
                    && part.is_boot == Some(false)
                    && part.is_system == Some(false)
                    && part.is_offline == Some(false)
                    && part
                        .drive_letter
                        .as_deref()
                        .is_none_or(|letter| policy::clean(letter).is_empty()) =>
            {
                Ok(Some((*part).clone()))
            }
            _ => Err("新建阶段读取到意外的分区布局，已停止。".into()),
        }
    }

    pub(in crate::partitions) fn run_ordered(
        prepared: &[DevicePlan],
        mut run: impl FnMut(usize, &mut Vec<Operation>) -> Result<(), String>,
        mut progress: impl FnMut(Progress),
    ) -> Outcome {
        use crate::partitions::quick::Phase;
        let total = prepared.len();
        let mut devices = Vec::with_capacity(total);
        let mut stopped = false;
        for (index, device) in prepared.iter().enumerate() {
            let mut completed = Vec::new();
            let (state, failure) = if stopped {
                (ResultState::Skipped, None)
            } else {
                progress(Progress {
                    phase: Phase::Running,
                    completed: index,
                    total,
                    current: Some(index),
                });
                match run(index, &mut completed) {
                    Ok(()) => {
                        progress(Progress {
                            phase: Phase::Verified,
                            completed: index + 1,
                            total,
                            current: None,
                        });
                        (ResultState::Verified, None)
                    }
                    Err(error) => {
                        stopped = true;
                        progress(Progress {
                            phase: Phase::Stopped,
                            completed: index,
                            total,
                            current: Some(index),
                        });
                        (ResultState::Stopped, Some(error))
                    }
                }
            };
            devices.push(DeviceResult {
                device: device.clone(),
                state,
                completed,
                failure,
            });
        }
        Outcome { devices }
    }
}

pub(crate) fn candidates() -> Result<Vec<Candidate>, String> {
    #[cfg(windows)]
    return super::platform::usb_format::candidates();
    #[cfg(not(windows))]
    Err("批量 USB 格式化需要 Windows。".into())
}
pub(crate) fn prepare(request: Request) -> Result<Preview, String> {
    #[cfg(windows)]
    return super::platform::usb_format::prepare(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("批量 USB 格式化需要 Windows。".into())
    }
}
pub(crate) fn execute(token: String, progress: impl FnMut(Progress)) -> Result<Outcome, String> {
    #[cfg(windows)]
    return super::platform::usb_format::execute(token, progress);
    #[cfg(not(windows))]
    {
        let _ = (token, progress);
        Err("批量 USB 格式化需要 Windows。".into())
    }
}
pub(crate) fn discard(token: &str) {
    #[cfg(windows)]
    super::platform::usb_format::discard(token);
    #[cfg(not(windows))]
    let _ = token;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partitions::{policy, FileSystem};

    fn disk() -> policy::Disk {
        policy::Disk {
            number: 2,
            path: "synthetic-disk".into(),
            size: 8 * policy::GIB,
            logical_sector_size: Some(4096),
            bus_type: Some(7),
            partition_style: Some(2),
            number_of_partitions: Some(1),
            is_boot: Some(false),
            is_system: Some(false),
            is_offline: Some(false),
            is_read_only: Some(false),
            is_clustered: Some(false),
            ..Default::default()
        }
    }

    fn partition() -> policy::Partition {
        policy::Partition {
            path: "synthetic-partition".into(),
            disk_number: 2,
            partition_number: 1,
            offset: policy::MIB,
            size: 7 * policy::GIB,
            guid: Some("01020304-0506-4708-8009-000000000000".into()),
            gpt_type: Some(policy::BASIC_DATA.into()),
            is_boot: Some(false),
            is_system: Some(false),
            is_read_only: Some(false),
            is_offline: Some(false),
            is_hidden: Some(false),
            is_shadow_copy: Some(false),
            ..Default::default()
        }
    }

    fn format() -> Format {
        Format {
            file_system: FileSystem::Exfat,
            label: "BATCH".into(),
            cluster_size: 0,
            options: Default::default(),
        }
    }

    #[test]
    fn rejects_repeated_physical_targets_and_invalid_batch_sizes() {
        let first = Selection {
            target: Target("a".repeat(64)),
            revision: "a1".into(),
            partition: Some(1),
        };
        let same_disk = Selection {
            partition: Some(2),
            ..first.clone()
        };
        let second = Selection {
            target: Target("b".repeat(64)),
            ..first.clone()
        };
        assert!(model::validate_selection(&[]).is_err());
        assert!(model::validate_selection(&[first.clone(), same_disk]).is_err());
        assert!(model::validate_selection(&[first, second]).is_ok());
        let oversized: Vec<_> = (0..33)
            .map(|index| Selection {
                target: Target(format!("{index:064x}")),
                revision: "a1".into(),
                partition: None,
            })
            .collect();
        assert!(model::validate_selection(&oversized).is_err());
    }

    #[test]
    fn format_requires_authoritative_usb_bus_and_ordinary_writable_gpt_target() {
        let part = partition();
        assert!(matches!(
            model::recipe(&disk(), &[part.clone()], Some(1), &format()),
            Ok(Recipe::Existing { .. })
        ));
        for bus in [None, Some(11), Some(17)] {
            let other = policy::Disk {
                bus_type: bus,
                ..disk()
            };
            assert!(model::recipe(&other, &[part.clone()], Some(1), &format()).is_err());
        }
        for other in [
            policy::Partition {
                is_boot: Some(true),
                ..part.clone()
            },
            policy::Partition {
                is_shadow_copy: None,
                ..part.clone()
            },
            policy::Partition {
                is_hidden: Some(true),
                ..part.clone()
            },
        ] {
            assert!(model::recipe(&disk(), &[other], Some(1), &format()).is_err());
        }
        assert!(model::recipe(&disk(), &[part], None, &format()).is_err());
        let mbr = policy::Disk {
            partition_style: Some(1),
            number_of_partitions: Some(0),
            ..disk()
        };
        assert!(model::recipe(&mbr, &[], None, &format()).is_err());
    }

    #[test]
    fn blank_usb_preserves_only_the_expected_gpt_reservation() {
        let raw = policy::Disk {
            partition_style: Some(0),
            number_of_partitions: Some(0),
            ..disk()
        };
        let plan = model::recipe(&raw, &[], None, &format()).unwrap();
        let Recipe::New {
            initialize: true,
            extent,
        } = plan
        else {
            panic!("expected blank RAW recipe");
        };
        assert_eq!(extent.offset, 32 * policy::MIB);
        assert_eq!(extent.offset + extent.size, raw.size - policy::MIB);
        let msr = policy::Partition {
            offset: policy::MIB,
            size: 16 * policy::MIB,
            gpt_type: Some("e3c9e316-0b5c-4db8-817d-f92df00215ae".into()),
            ..partition()
        };
        assert!(model::reserved(&[&msr], &extent, true).is_ok());
        assert!(model::reserved(&[&msr], &extent, false).is_err());
        let outside = policy::Partition {
            offset: extent.offset,
            ..msr.clone()
        };
        assert!(model::reserved(&[&outside], &extent, true).is_err());
        assert!(model::reserved(&[&partition()], &extent, true).is_err());
        let existing_gpt = policy::Disk {
            partition_style: Some(2),
            ..raw
        };
        assert!(matches!(
            model::recipe(&existing_gpt, &[], None, &format()),
            Ok(Recipe::New {
                initialize: false,
                ..
            })
        ));
    }

    #[test]
    fn device_failure_is_retained_and_the_remaining_devices_are_skipped() {
        let views = (0..3)
            .map(|number| DevicePlan {
                number,
                name: "USB".into(),
                serial: format!("USB-{number}"),
                partition: None,
                extent: Extent {
                    offset: policy::MIB,
                    size: policy::GIB,
                },
                operation: Operation::Create,
                drive_letter: None,
                old_file_system: None,
                old_label: None,
                cluster_size: 0,
            })
            .collect::<Vec<_>>();
        let mut called = Vec::new();
        let mut events = Vec::new();
        let report = model::run_ordered(
            &views,
            |index, stages| {
                called.push(index);
                stages.push(Operation::Create);
                if index == 1 {
                    return Err("provider failure".into());
                }
                stages.push(Operation::Format);
                Ok(())
            },
            |progress| events.push(progress),
        );
        assert_eq!(called, [0, 1]);
        assert_eq!(report.devices[0].state, ResultState::Verified);
        assert_eq!(report.devices[1].state, ResultState::Stopped);
        assert_eq!(report.devices[1].completed, [Operation::Create]);
        assert_eq!(
            report.devices[1].failure.as_deref(),
            Some("provider failure")
        );
        assert_eq!(report.devices[2].state, ResultState::Skipped);
        assert_eq!(
            events.last().unwrap().phase,
            crate::partitions::quick::Phase::Stopped
        );
        assert_eq!(events.last().unwrap().completed, 1);
    }
}

//! Restore a fixed point of an open backup without reopening any paths.
use super::{
    progress, resize_clone, write_device, OpenedBackup, Selection, OVERWRITE_WARNING,
    PARTITION_OVERWRITE_WARNING,
};
use crate::{data_io, data_windows as device, native_locale::Locale, used_space};
use disk_core::btrfs;
use std::{io::SeekFrom, sync::atomic::AtomicBool};

enum Destination {
    Disk {
        selection: Selection,
        before: device::Resolved,
    },
    Partition {
        target: resize_clone::PartitionTarget,
        plan: PartitionPlan,
    },
}

enum PartitionPlan {
    Original,
    FatResize([u8; 32]),
    BtrfsOriginal([u8; 32]),
    BtrfsResize { resize: String, allocation: String },
}

pub(crate) struct Restore {
    opened: OpenedBackup,
    partition: Option<u32>,
    size: u64,
    target: Destination,
}

impl Restore {
    pub(super) fn prepare(
        opened: OpenedBackup,
        partition: Option<u32>,
        destination: Selection,
        resize: bool,
        locale: &Locale,
    ) -> Result<(Self, String, String, u64, String), String> {
        if opened.source_disks.contains(&destination.target) {
            return Err("目标磁盘上存有此备份或其依赖文件。".into());
        }
        if partition.is_some() != destination.partition.is_some() || (resize && partition.is_none())
        {
            return Err("还原范围须匹配。调整容量仅适用于单个分区。".into());
        }
        opened.validate()?;
        let browser = &opened.browser;
        let point = browser
            .info
            .points
            .iter()
            .find(|point| point.id == browser.info.selected_point)
            .ok_or("备份还原点不存在")?;
        let size = browser.restore_size(partition)?;
        let sector = browser.info.sector;
        let point_label = match point.ordinal {
            Some(ordinal) => locale.format(
                "此前第 {layer} 层增量备份",
                &[("layer", &ordinal.to_string())],
            ),
            None => locale.text(&point.label).to_owned(),
        };
        let source = format!("{} / {point_label}", browser.info.name);
        let source_part = partition
            .map(|number| {
                browser
                    .info
                    .partitions
                    .iter()
                    .find(|part| part.id == number)
                    .ok_or("备份分区选择无效")
            })
            .transpose()?;
        let source = if let Some(part) = source_part {
            let name = if browser.info.kind == "partition" {
                locale.text(&part.name)
            } else {
                &part.name
            };
            format!("{source} / {name}")
        } else {
            source
        };
        let integrity = "写入前复核所选还原点及其完整依赖链。";
        let (target, label, bytes, warning) = if let Some(part) = source_part {
            let (target, label) = if part.btrfs {
                resize_clone::PartitionTarget::prepare_btrfs(&destination, sector, locale)?
            } else {
                resize_clone::PartitionTarget::prepare(&destination, sector, locale)?
            };
            let range = target.range;
            check_capacity(size, range.size, resize)?;
            let cancel = AtomicBool::new(false);
            let (plan, detail) = if part.btrfs {
                let mut reader = browser.restore_reader(partition)?;
                if resize {
                    let plan = btrfs::plan_resize(&mut reader, range.size)?;
                    let (_, allocation) = resize_clone::btrfs_view(reader, &plan, range, &cancel)?;
                    let detail = locale.format(
                        "Btrfs · {old} B → {new} B · 预计搬移 {moved} B 的条带。保留卷 UUID、文件和子卷，确认空闲区写零。完整回读 SHA-256，目标 GPT 保持原样。",
                        &[("old", &plan.before().to_string()), ("new", &range.size.to_string()), ("moved", &plan.relocation_bytes().to_string())],
                    );
                    (
                        PartitionPlan::BtrfsResize {
                            resize: plan.proof(),
                            allocation: allocation.digest,
                        },
                        detail,
                    )
                } else {
                    let mut volume = btrfs::open(reader, || used_space::stop(&cancel))?;
                    volume.verify_view(|| used_space::stop(&cancel))?;
                    (PartitionPlan::BtrfsOriginal(volume.identity()), locale.text(
                        "恢复 Btrfs 分区的原始字节并核对完整 SHA-256。保留卷 UUID、文件和子卷，目标 GPT 保持原样。",
                    ).to_owned())
                }
            } else if resize {
                let mut reader = browser.restore_reader(partition)?;
                let plan = resize_clone::resized_plan(&mut reader, range)?;
                resize_clone::resized_view(&plan, reader, range, &cancel)?;
                (
                    PartitionPlan::FatResize(plan.fingerprint()),
                    resize_clone::resize_detail(locale, &plan),
                )
            } else {
                (
                    PartitionPlan::Original,
                    locale
                        .text("恢复所选分区的原始字节并完整回读。目标 GPT 分区表保持原样。")
                        .to_owned(),
                )
            };
            let warning = locale.sentences(&[
                integrity,
                &detail,
                PARTITION_OVERWRITE_WARNING,
                if part.btrfs {
                    "副本保留原 Btrfs UUID，请与原卷分别挂载。"
                } else {
                    ""
                },
            ]);
            (
                Destination::Partition { target, plan },
                label,
                range.size,
                warning,
            )
        } else {
            let before = device::resolve(&destination, true, true)?;
            if Some(sector) != before.disk.logical_sector_size {
                return Err("备份类型、容量或逻辑扇区与目标不匹配。".into());
            }
            check_capacity(size, before.size, false)?;
            let label = before.label.clone();
            (
                Destination::Disk {
                    selection: destination,
                    before,
                },
                label,
                size,
                locale.sentences(&[
                    integrity,
                    if browser.info.table_issue.is_some() {
                        "此还原点的分区表无法解析或未通过校验。整盘还原会保留归档中的分区表原始状态。"
                    } else {
                        ""
                    },
                    OVERWRITE_WARNING,
                ]),
            )
        };
        opened.validate()?;
        Ok((
            Self {
                opened,
                partition,
                size,
                target,
            },
            source,
            label,
            bytes,
            warning,
        ))
    }

    pub(super) fn execute(
        self,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let locale = crate::native_locale::current();
        let validate_source = || {
            if super::state().opened_backup.as_ref().is_none_or(|current| {
                current.browser.info.id != self.opened.browser.info.id
                    || current.browser.info.selected_point
                        != self.opened.browser.info.selected_point
            }) {
                return Err("备份还原点已变化，请重新选择。".into());
            }
            self.opened.validate()
        };
        validate_source()?;
        let browser = &self.opened.browser;
        let verified = browser
            .verify_content(|done, total| progress(id, cancel, "校验还原点", done, total))?;
        validate_source()?;
        let mut reader = browser.restore_reader(self.partition)?;
        if let Destination::Partition { target, plan } = &self.target {
            let range = target.range;
            let hash = match plan {
                PartitionPlan::FatResize(expected) => {
                    progress(id, cancel, "检查文件系统分配信息", 0, range.size)?;
                    let plan = resize_clone::resized_plan(&mut reader, range)?;
                    if plan.fingerprint() != *expected {
                        return Err("备份分区的分配计划与预览不同。".into());
                    }
                    let (view, allocation) =
                        resize_clone::resized_view(&plan, reader, range, cancel)?;
                    target.write(
                        id,
                        cancel,
                        &mut allocation.masked(view),
                        None,
                        |check| plan.verify(check),
                        validate_source,
                    )?
                }
                PartitionPlan::BtrfsResize { resize, allocation } => {
                    progress(id, cancel, "检查文件系统分配信息", 0, range.size)?;
                    let plan = btrfs::plan_resize(&mut reader, range.size)?;
                    let (view, map) = resize_clone::btrfs_view(reader, &plan, range, cancel)?;
                    if plan.proof() != *resize || map.digest != *allocation {
                        return Err("备份分区的分配计划与预览不同。".into());
                    }
                    let identity = view.identity();
                    target.write(
                        id,
                        cancel,
                        &mut map.masked(view),
                        None,
                        |check| resize_clone::verify_btrfs_copy(check, identity, cancel),
                        validate_source,
                    )?
                }
                PartitionPlan::Original | PartitionPlan::BtrfsOriginal(_) => {
                    let expected = if self.size == browser.info.size {
                        verified
                    } else {
                        hash_extent(id, cancel, &mut reader, self.size)?
                    };
                    target.write(
                        id,
                        cancel,
                        &mut reader,
                        Some(&expected),
                        |check| match plan {
                            PartitionPlan::BtrfsOriginal(identity) => {
                                resize_clone::verify_btrfs_copy(check, *identity, cancel)
                            }
                            _ => Ok(()),
                        },
                        validate_source,
                    )?
                }
            };
            let message = locale.text("所选备份分区已还原并回读校验。目标磁盘保持脱机。");
            return Ok((message.into(), Some(hash)));
        }
        let Destination::Disk { selection, before } = self.target else {
            unreachable!()
        };
        let dst = device::resolve(&selection, true, true)?;
        if !dst.same_data_binding(&before) {
            return Err("目标盘身份、布局或保护与预览不同。".into());
        }
        let locks = device::lock(&dst)?;
        let dst = device::resolve_locked(&selection, true, &dst, &locks)?;
        validate_source()?;
        let target = device::exclusive_device(&dst)?;
        let result = write_device(id, cancel, &mut reader, target, &dst, Some(&verified))?;
        validate_source()?;
        Ok(result)
    }
}

fn hash_extent(
    id: &str,
    cancel: &AtomicBool,
    reader: &mut (impl crate::backup_archive::Source + ?Sized),
    size: u64,
) -> Result<String, String> {
    let mut reader = reader;
    let expected = data_io::hash_exact(&mut reader, size, |done| {
        progress(id, cancel, "校验所选备份范围", done, size)
    })?;
    reader.seek(SeekFrom::Start(0)).map_err(device::err)?;
    Ok(expected)
}

fn check_capacity(source: u64, target: u64, resize: bool) -> Result<(), String> {
    match (resize, source == target) {
        (false, false) => Err("原尺寸还原要求目标容量相同。".into()),
        (true, true) => Err("调整容量还原要求选择不同容量的目标分区。".into()),
        _ => Ok(()),
    }
}

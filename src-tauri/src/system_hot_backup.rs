//! A boot-checked backup of the running Windows partition or its whole GPT disk.
use super::*;
use crate::{
    backup_archive::{self, AllocationPolicy, Chain},
    backup_files, backup_secrets,
    data_tasks::{backup_output_bound, BackupOptions},
    used_space::View,
};
use std::{path::Path, sync::atomic::Ordering};

pub(in crate::data_tasks) struct HotBackup {
    selection: Selection,
    source: HotMigrationSource,
    scope: Span,
    partition_only: bool,
}

impl HotBackup {
    pub(in crate::data_tasks) fn bind(
        selection: &Selection,
        observed: &device::Resolved,
    ) -> Result<Option<Self>, String> {
        let partition_only = selection.partition.is_some();
        if partition_only && !HotMigrationSource::is_windows_partition(selection, observed)? {
            return Ok(None);
        }
        let scope = Span {
            start: observed.offset,
            end: observed
                .offset
                .checked_add(observed.size)
                .ok_or("备份范围无效")?,
        };
        if observed.size > backup_archive::MAX_SIZE {
            return Err("Windows 热备份范围超出归档上限。".into());
        }
        let selection = Selection {
            partition: None,
            ..selection.clone()
        };
        let source = HotMigrationSource::bind(&selection)?;
        if source.resolved.disk != observed.disk
            || scope.end > source.resolved.size
            || partition_only && scope != source.windows
        {
            return Err("Windows 热备份的源盘或分区已变化。".into());
        }
        if !partition_only
            && source
                .partitions
                .iter()
                .any(|part| matches!(&part.kind, HotPartitionKind::Unavailable))
        {
            return Err(
                "整盘热备份包含无法一致读取的分区。请选择 Windows 分区或使用 WinPE。".into(),
            );
        }
        Ok(Some(Self {
            selection,
            source,
            scope,
            partition_only,
        }))
    }

    pub(in crate::data_tasks) fn execute(
        self,
        path: &Path,
        destination: &crate::partitions::Target,
        options: &BackupOptions,
        mut parent: Option<Chain>,
        id: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Option<String>), String> {
        let size = self.scope.size();
        let source = &self.source;
        let target = &self.selection.target;
        let sector = source
            .resolved
            .disk
            .logical_sector_size
            .ok_or("源盘逻辑扇区大小未知。")?;
        let kind = if self.partition_only {
            "partition"
        } else {
            "disk"
        };
        let tick = |phase: &str, done: u64| progress(id, cancel, phase, done, size);
        tick("准备备份", 0)?;
        let sources = std::slice::from_ref(target);
        let (_, actual) =
            device::destination_location(path.parent().ok_or("输出目录无效")?, sources, 0)?;
        if &actual != destination {
            return Err("备份输出目录所在磁盘已变化。".into());
        }
        if options.incremental != parent.is_some() {
            return Err("增量备份依赖缺失".into());
        }
        if let Some(parent) = &mut parent {
            parent.verify(|done| tick("验证基础备份", done))?;
        }
        let password = if options.encryption {
            let password = backup_secrets::ask(
                backup_secrets::Purpose::Create,
                backup_files::backup_name(path)?,
                || tick("等待设置备份密码", 0),
            )?;
            if password.is_none() {
                cancel.store(true, Ordering::SeqCst);
                return Err("已取消加密备份，未创建输出。".into());
            }
            password
        } else {
            None
        };

        let physical = device::device(&source.resolved, false)?;
        source.revalidate(&self.selection, &physical)?;
        let mut image = HotImage::capture(
            &self.selection,
            source,
            &physical,
            |_| !self.partition_only,
            cancel,
            &tick,
        )?;
        let options_for_boot = Options {
            selection: if self.partition_only {
                PartitionSelection::Selected {
                    partitions: Vec::new(),
                }
            } else {
                PartitionSelection::All
            },
            ..Options::default()
        };
        let plan = read_plan(
            &self.selection,
            &source.resolved,
            &physical,
            image.reader(&physical)?,
            source.resolved.size,
            &options_for_boot,
            SystemHive::SnapshotLogs,
        )?;
        image.verify_plan(&plan, source)?;
        let verify_boot = || {
            let mut full = plan.reader(image.reader(&physical)?).map_err(device::err)?;
            plan.verify_output(&mut full)
        };
        verify_boot()?;
        let reader = plan.reader(image.reader(&physical)?).map_err(device::err)?;
        let mut content = View::bounded(reader, self.scope.start, size, source.resolved.size)
            .map_err(device::err)?;
        let ranges = (!self.partition_only).then_some(source.resolved.partition_ranges.as_slice());
        tick("检查文件系统分配信息", 0)?;
        let allocation = used_space::inspect(&mut content, size, sector, ranges, cancel)?;
        allocation.proof.verify(&mut content, cancel)?;
        if !self.partition_only {
            plan.verify_output(&mut allocation.masked(&mut content))?;
        }
        let payload = allocation.backup_payload_bound(backup_archive::CHUNK as u64)?;
        let required = backup_output_bound(payload, size, options.encryption)?;
        if let Some(parent) = &parent {
            parent.validate_sources()?;
        }
        source.revalidate(&self.selection, &physical)?;
        let (mut output, publication) = backup_files::create_excluding(
            path,
            sources,
            Some(destination),
            required,
            password.as_ref(),
        )?;
        drop(password);
        let header = backup_archive::create_with_settings(
            &mut allocation.masked(&mut content),
            &mut output,
            kind,
            size,
            sector,
            options.compression(),
            parent.as_ref(),
            Some(AllocationPolicy {
                skipped_bytes: allocation.skipped,
                map_sha256: allocation.digest.clone(),
            }),
            |done| {
                tick(
                    if options.compression {
                        "并行压缩备份"
                    } else {
                        "备份"
                    },
                    done,
                )
            },
        )?;
        allocation.proof.verify(&mut content, cancel)?;
        let repeated = data_io::hash_exact(&mut allocation.masked(&mut content), size, |done| {
            tick("再次读取备份源", done)
        })?;
        if repeated != header.sha256 {
            return Err("Windows 快照再次读取的 SHA-256 与备份不同。".into());
        }
        drop(content);
        verify_boot()?;
        source.revalidate(&self.selection, &physical)?;
        image.verify(&physical, cancel, || tick("核对热备份源布局与启动范围", 0))?;
        backup_files::verify_output(output, &publication, parent, |done| {
            tick("校验备份链", done)
        })?;
        image
            .set
            .complete_registry_copy(|| tick("完成 VSS 快照会话", size))?;
        drop(image);
        tick("保存已验证备份", size)?;
        publication.publish()?;
        let locale = native_locale::current();
        let message = locale.text("Windows 热备份完成，已核对快照来源、启动链和全部归档内容。");
        Ok((
            format!("{message}\n{}", path.display()),
            Some(header.sha256),
        ))
    }
}

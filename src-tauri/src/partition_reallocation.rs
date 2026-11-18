//! Plan exact GPT geometry before authorizing a multi-step operation.
//! Windows still has to prove volume eligibility, native extents and resize bounds.
#[cfg(any(test, windows))]
use super::{policy, Extent};
use super::{quick, Target};
#[cfg(any(test, windows))]
use crate::table_restore::{Entry as GptEntry, Layout};
#[cfg(any(test, windows))]
use policy::{Disk, Partition, MIB, MIN_RESIZE_SIZE};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Target,
    pub revision: String,
    pub operation: Operation,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preview {
    #[serde(flatten)]
    pub confirmation: super::Plan,
    pub steps: Vec<Step>,
    pub journals: Vec<String>,
    pub merge_folder: Option<String>,
}

pub(crate) fn prepare(request: Request) -> Result<Option<Preview>, String> {
    #[cfg(windows)]
    {
        super::platform::reallocation::prepare(request)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("空间重新分配当前仅支持 Windows 桌面版。".into())
    }
}

pub(crate) fn execute(
    token: String,
    progress: impl FnMut(quick::Progress),
) -> Result<super::Outcome, String> {
    #[cfg(windows)]
    {
        super::platform::reallocation::execute(&token, progress)
    }
    #[cfg(not(windows))]
    {
        let _ = (token, progress);
        Err("空间重新分配当前仅支持 Windows 桌面版。".into())
    }
}

pub(crate) fn discard(token: String) {
    #[cfg(windows)]
    super::platform::reallocation::discard(&token);
    #[cfg(not(windows))]
    let _ = token;
}

pub(crate) fn stop(token: String) -> bool {
    #[cfg(windows)]
    {
        super::platform::reallocation::stop(&token)
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        false
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum Operation {
    Split {
        partition: u32,
        new_size: u64,
        format: Option<quick::Format>,
    },
    Allocate {
        partition: u32,
        free_offset: u64,
        amount: u64,
    },
    Transfer {
        donor: u32,
        receiver: u32,
        amount: u64,
    },
    Merge {
        keep: u32,
        absorb: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PartitionRef {
    #[serde(skip_serializing)]
    pub guid: String,
    pub number: u32,
    pub offset: u64,
    pub size: u64,
}

#[cfg(any(test, windows))]
impl PartitionRef {
    fn from_partition(p: &Partition) -> Result<Self, String> {
        Ok(Self {
            guid: policy::guid(p.guid.as_deref().ok_or("缺少 GPT 分区标识。")?),
            number: p.partition_number,
            offset: p.offset,
            size: p.size,
        })
    }

    pub fn resolve<'a>(&self, parts: &[&'a Partition]) -> Result<&'a Partition, String> {
        let mut found = parts
            .iter()
            .copied()
            .filter(|p| p.guid.as_deref().map(policy::guid).as_ref() == Some(&self.guid));
        let p = found.next().ok_or("确认的 GPT 分区已不存在。")?;
        if found.next().is_some()
            || (p.offset, p.size) != (self.offset, self.size)
            || !policy::formattable(p)
        {
            return Err("确认的 GPT 分区范围、唯一性或保护状态已变化。".into());
        }
        Ok(p)
    }

    fn gpt_index(&self, entries: &[GptEntry]) -> Result<usize, String> {
        let index = entries
            .iter()
            .position(|entry| entry.guid == self.guid)
            .ok_or("完整 GPT 中缺少确认的分区。")?;
        let entry = &entries[index];
        if (entry.offset, entry.size) != (self.offset, self.size)
            || entry.kind != policy::BASIC_DATA
            || entry.attributes & !(1 << 63) != 0
        {
            return Err("完整 GPT 中确认的分区范围、类型或保护属性已变化。".into());
        }
        Ok(index)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Step {
    Resize {
        partition: PartitionRef,
        new_size: u64,
    },
    Move {
        partition: PartitionRef,
        new_offset: u64,
    },
    Create {
        entry: quick::Entry,
    },
    Format {
        entry: quick::Entry,
    },
    CopyAndRemove {
        keep: PartitionRef,
        absorb: PartitionRef,
    },
    Offline,
}

#[cfg(any(test, windows))]
impl Step {
    pub fn title(&self) -> String {
        match self {
            Self::Resize {
                partition,
                new_size,
            } => format!("调整分区 {} 至 {} MiB", partition.number, new_size / MIB,),
            Self::Move {
                partition,
                new_offset,
            } => format!("移动分区 {} 至 {} MiB", partition.number, new_offset / MIB,),
            Self::Create { entry } => format!(
                "新建尾部分区（起点 {} MiB，容量 {} MiB）",
                entry.offset / MIB,
                entry.size / MIB
            ),
            Self::Format { entry } => format!("格式化新分区（起点 {} MiB）", entry.offset / MIB),
            Self::CopyAndRemove { keep, absorb } => {
                format!("将分区 {} 合并到分区 {}", absorb.number, keep.number)
            }
            Self::Offline => "将磁盘脱机并核对最终布局".into(),
        }
    }

    /// Compare raw/native layout models whenever a step returns. A new array
    /// slot or runtime partition number may represent the same GPT entry.
    pub fn verify_geometry(&self, before: &Layout, after: &Layout) -> Result<(), String> {
        let mut expected = before.clone();
        match self {
            Self::Resize {
                partition,
                new_size,
            } => {
                let index = partition.gpt_index(&expected.entries)?;
                expected.entries[index].size = *new_size;
            }
            Self::Move {
                partition,
                new_offset,
            } => {
                let index = partition.gpt_index(&expected.entries)?;
                expected.entries[index].offset = *new_offset;
            }
            Self::Create { entry } => {
                let mut found = after.entries.iter().filter(|created| {
                    (created.offset, created.size) == (entry.offset, entry.size)
                        && created.kind == entry.role.gpt_type()
                        && created.attributes & !(1 << 63) == 0
                        && !before.entries.iter().any(|p| p.guid == created.guid)
                });
                let created = found.next().ok_or("GPT 中缺少新建的精确范围和类型。")?;
                if found.next().is_some() {
                    return Err("新建分区在 GPT 中不唯一。".into());
                }
                expected.entries.push(created.clone());
            }
            Self::Format { .. } | Self::Offline => {}
            Self::CopyAndRemove { keep, absorb } => {
                keep.gpt_index(&expected.entries)?;
                let index = absorb.gpt_index(&expected.entries)?;
                expected.entries.remove(index);
            }
        }
        if !after.agrees(&expected) {
            return Err("实际 GPT 与确认步骤不同；其他布局字段有变化或步骤尚未完成。".into());
        }
        Ok(())
    }
}

/// Partition numbers in a step are for display. Execution must bind the current
/// provider object by the stable GUID and the exact pre-step byte range.
#[cfg(any(test, windows))]
pub(crate) fn plan(
    disk: &Disk,
    partitions: &[Partition],
    gpt: &Layout,
    operation: &Operation,
) -> Result<Vec<Step>, String> {
    policy::manageable(disk)?;
    if disk.partition_style != Some(2) {
        return Err("空间重新分配要求已初始化的 GPT 磁盘。".into());
    }
    let parts = policy::layout(disk, partitions)?;
    let sector = disk.logical_sector_size.ok_or("缺少逻辑扇区大小。")?;
    let first_usable = gpt.first.checked_mul(sector).ok_or("GPT 起点越界。")?;
    let end_usable = gpt
        .last
        .checked_add(1)
        .and_then(|last| last.checked_mul(sector))
        .ok_or("GPT 末端越界。")?;
    if disk.size % sector != 0
        || first_usable >= end_usable
        || end_usable > disk.size
        || gpt.entries.len() != parts.len()
        || gpt.count < parts.len() as u32
        || gpt.count == 0
        || gpt.count > 128
        || parts.iter().any(|part| {
            part.offset % sector != 0
                || part.size % sector != 0
                || part.offset < first_usable
                || part.offset + part.size > end_usable
                || !gpt.entries.iter().any(|entry| {
                    part.guid.as_deref().map(policy::guid).as_ref() == Some(&entry.guid)
                        && part.gpt_type.as_deref().map(policy::guid).as_ref() == Some(&entry.kind)
                        && (part.offset, part.size) == (entry.offset, entry.size)
                })
        })
    {
        return Err("Windows 分区布局与完整 GPT 不一致，或扇区范围无效。".into());
    }

    let mut planner = Planner {
        disk,
        gpt,
        sector,
        first_usable: first_usable.max(MIB),
        end_usable: end_usable.min(disk.size - MIB),
        parts: parts.iter().map(|p| (*p).clone()).collect(),
        steps: Vec::new(),
    };
    match operation {
        Operation::Split {
            partition,
            new_size,
            format,
        } => {
            planner.split(*partition, *new_size, format.as_ref())?;
        }
        Operation::Allocate {
            partition,
            free_offset,
            amount,
        } => {
            let receiver = planner.index(*partition)?;
            let mut gap = policy::free_ranges(disk, &parts)
                .into_iter()
                .find(|gap| gap.offset == *free_offset && gap.size >= *amount)
                .ok_or("所选未分配范围或容量已经改变。")?;
            let limit = (gap.offset + gap.size).min(planner.end_usable) / MIB * MIB;
            gap.offset = gap.offset.max(planner.first_usable.div_ceil(MIB) * MIB);
            gap.size = limit.saturating_sub(gap.offset);
            planner.allocate(receiver, &gap, *amount)?;
        }
        Operation::Transfer {
            donor,
            receiver,
            amount,
        } => {
            planner.transfer(*donor, *receiver, *amount)?;
        }
        Operation::Merge { keep, absorb } => planner.merge(*keep, *absorb)?,
    }
    Ok(planner.steps)
}

#[cfg(any(test, windows))]
struct Planner<'a> {
    disk: &'a Disk,
    gpt: &'a Layout,
    sector: u64,
    first_usable: u64,
    end_usable: u64,
    parts: Vec<Partition>,
    steps: Vec<Step>,
}

#[cfg(any(test, windows))]
impl Planner<'_> {
    fn index(&self, number: u32) -> Result<usize, String> {
        self.parts
            .iter()
            .position(|part| part.partition_number == number)
            .ok_or_else(|| "所选分区已经改变。".into())
    }

    fn bind(&self, index: usize) -> Result<PartitionRef, String> {
        let part = &self.parts[index];
        if !policy::formattable(part) {
            return Err(format!(
                "分区 {} 受保护、类型不支持或状态不完整。",
                part.partition_number
            ));
        }
        let result = PartitionRef::from_partition(part)?;
        let gpt = self
            .gpt
            .entries
            .iter()
            .find(|entry| entry.guid == result.guid)
            .ok_or("缺少 GPT 分区标识。")?;
        if gpt.attributes & !(1 << 63) != 0 {
            return Err(format!(
                "分区 {} 的 GPT 属性受保护。",
                part.partition_number
            ));
        }
        Ok(result)
    }

    fn check_range(&self, index: usize, offset: u64, size: u64) -> Result<(), String> {
        let stop = offset.checked_add(size).ok_or("分区结束位置溢出。")?;
        if offset < self.first_usable
            || stop > self.end_usable
            || self
                .parts
                .iter()
                .enumerate()
                .any(|(i, p)| i != index && p.offset < stop && offset < p.offset + p.size)
        {
            return Err("步骤中的分区范围越界或与其他分区重叠。".into());
        }
        Ok(())
    }

    fn resize(&mut self, index: usize, new_size: u64) -> Result<(), String> {
        let partition = self.bind(index)?;
        if new_size < MIN_RESIZE_SIZE || new_size % MIB != 0 || new_size == partition.size {
            return Err("调整后的分区须至少 64 MiB，容量须为不同的整 MiB。".into());
        }
        self.check_range(index, partition.offset, new_size)?;
        self.parts[index].size = new_size;
        self.steps.push(Step::Resize {
            partition,
            new_size,
        });
        Ok(())
    }

    fn shift(&mut self, index: usize, new_offset: u64) -> Result<(), String> {
        let partition = self.bind(index)?;
        let ordered = self
            .parts
            .iter()
            .all(|p| p.is_boot == Some(false) && p.is_system == Some(false));
        if self.disk.is_boot != Some(false) || self.disk.is_system != Some(false) || !ordered {
            return Err("需要搬移中间分区时，整块磁盘须为非启动、非系统磁盘。".into());
        }
        disk_core::partition_move::Range::new(
            partition.offset,
            new_offset,
            partition.size,
            self.disk.size,
            self.sector,
        )?;
        self.check_range(index, new_offset, partition.size)?;
        self.parts[index].offset = new_offset;
        self.steps.push(Step::Move {
            partition,
            new_offset,
        });
        Ok(())
    }

    fn split(
        &mut self,
        number: u32,
        new_size: u64,
        format: Option<&quick::Format>,
    ) -> Result<(), String> {
        let index = self.index(number)?;
        let original = self.bind(index)?;
        if self.parts.len() == self.gpt.count as usize
            || original.offset % MIB != 0
            || original.size % MIB != 0
        {
            return Err("拆分需要空闲 GPT 条目，且原起点及容量须按 MiB 对齐。".into());
        }
        self.check_range(index, original.offset, original.size)?;
        check_amount(new_size)?;
        let retained_size = original
            .size
            .checked_sub(new_size)
            .ok_or("新分区超过原分区容量。")?;
        let mut format = format.cloned();
        if let Some(format) = &mut format {
            format.label = format.label.trim().to_owned();
            if format.file_system.fat_type().is_some() {
                format.label = format.label.to_ascii_uppercase();
            }
            format.options.validate(
                format.file_system,
                new_size,
                self.sector,
                &format.label,
                format.cluster_size,
            )?;
        }
        self.resize(index, retained_size)?;
        let entry = quick::Entry {
            offset: original.offset + retained_size,
            size: new_size,
            role: quick::Role::Windows,
            format,
        };
        self.steps.push(Step::Create {
            entry: entry.clone(),
        });
        if entry.format.is_some() {
            self.steps.push(Step::Format { entry });
        }
        Ok(())
    }

    fn allocate(&mut self, receiver: usize, gap: &Extent, amount: u64) -> Result<(), String> {
        check_amount(amount)?;
        let target = self.bind(receiver)?;
        let gap_end = gap.offset + gap.size;
        if amount > gap.size {
            return Err("分配量超过所选未分配范围。".into());
        }

        // Transfer space through the intervening entries, keeping each one's
        // length and order. Shift the entry bordering the gap first.
        if target.offset + target.size <= gap.offset {
            for index in (receiver + 1..self.parts.len()).rev() {
                let p = &self.parts[index];
                if p.offset + p.size <= gap.offset {
                    self.shift(index, p.offset + amount)?;
                }
            }
        } else if gap_end <= target.offset {
            for index in 0..=receiver {
                let p = &self.parts[index];
                if p.offset >= gap_end {
                    self.shift(index, p.offset - amount)?;
                }
            }
        } else {
            return Err("所选未分配范围与目标分区重叠。".into());
        }
        self.resize(receiver, target.size + amount)
    }

    fn transfer(&mut self, donor: u32, receiver: u32, amount: u64) -> Result<(), String> {
        if donor == receiver {
            return Err("释放空间和接收空间的分区必须不同。".into());
        }
        check_amount(amount)?;
        let donor = self.index(donor)?;
        let receiver = self.index(receiver)?;
        let original = self.bind(donor)?;
        let size = original
            .size
            .checked_sub(amount)
            .ok_or("释放量超过原分区容量。")?;
        self.resize(donor, size)?;
        self.allocate(
            receiver,
            &Extent {
                offset: original.offset + size,
                size: amount,
            },
            amount,
        )
    }

    fn merge(&mut self, keep: u32, absorb: u32) -> Result<(), String> {
        let receiver = self.index(keep)?;
        let removed = self.index(absorb)?;
        if receiver.abs_diff(removed) != 1 {
            return Err("合并要求两个同盘相邻的普通数据分区。".into());
        }
        let keep = self.bind(receiver)?;
        let absorb = self.bind(removed)?;
        let start = keep.offset.min(absorb.offset);
        let end = (keep.offset + keep.size).max(absorb.offset + absorb.size);
        self.steps.push(Step::CopyAndRemove { keep, absorb });
        self.parts.remove(removed);
        let receiver = receiver.min(removed);
        if self.parts[receiver].offset != start {
            self.shift(receiver, start)?;
        }
        self.resize(receiver, end - start)
    }
}

#[cfg(any(test, windows))]
fn check_amount(size: u64) -> Result<(), String> {
    if size < MIB || size % MIB != 0 {
        return Err("空间大小须至少 1 MiB，且为整 MiB。".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "partition_reallocation_tests.rs"]
mod tests;

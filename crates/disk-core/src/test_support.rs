//! Historical in-memory model helpers compiled only for unit tests.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: String,
    pub r#type: String,
    pub partition_id: String,
    pub name: Option<String>,
    pub fs: Option<String>,
    pub size: Option<u64>,
}
pub fn sample_disks() -> Result<Vec<Disk>, String> {
    serde_json::from_str(include_str!("../../../tests/fixtures/samples.json"))
        .map_err(|e| e.to_string())
}
fn name(value: &Option<String>) -> Result<String, String> {
    let n = value.as_ref().ok_or("缺少名称。")?;
    if n.trim().is_empty()
        || n.chars().count() > 48
        || n.chars().any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err("名称应为 1–48 个可显示字符。".into());
    }
    Ok(n.clone())
}
fn fs(value: &Option<String>) -> Result<String, String> {
    match value.as_deref() {
        Some(n @ ("NTFS" | "FAT32" | "exFAT")) => Ok(n.into()),
        _ => Err("不支持的文件系统。".into()),
    }
}
pub fn apply_operations(mut disk: Disk, operations: Vec<Operation>) -> Result<Disk, String> {
    if disk.kind != "sample" {
        return Err("本机磁盘和导入镜像均不可修改；只允许提交内存样本变更。".into());
    }
    if operations.len() > 128 {
        return Err("一次最多处理 128 项变更。".into());
    }
    validate(&disk)?;
    for op in operations {
        let i = disk
            .partitions
            .iter()
            .position(|p| p.id == op.partition_id)
            .ok_or("目标分区已改变。")?;
        let p = disk.partitions[i].clone();
        if p.system {
            return Err("样本中的 EFI 系统分区受保护。".into());
        }
        if op.r#type != "create" && p.fs == "—" {
            return Err("此操作需要选择一个分区。".into());
        }
        match op.r#type.as_str() {
            "create" => {
                let size = op.size.ok_or("缺少容量。")?;
                if p.fs != "—" || size < MIB || size > p.size || size % MIB != 0 {
                    return Err("新分区容量无效或超过未分配空间。".into());
                }
                disk.partitions[i] = Partition {
                    id: op.id.clone(),
                    name: name(&op.name)?,
                    fs: fs(&op.fs)?,
                    size,
                    used: Some(0),
                    color: "mint".into(),
                    ..p.clone()
                };
                if size < p.size {
                    disk.partitions.insert(
                        i + 1,
                        free(p.start + size, p.size - size, format!("{}-free", op.id)),
                    );
                }
            }
            "delete" => disk.partitions[i] = free(p.start, p.size, p.id),
            "rename" => disk.partitions[i].name = name(&op.name)?,
            "format" => {
                disk.partitions[i].fs = fs(&op.fs)?;
                disk.partitions[i].name = name(&op.name)?;
                disk.partitions[i].used = Some(0);
            }
            "resize" => {
                let size = op.size.ok_or("缺少容量。")?;
                let next_free = disk
                    .partitions
                    .get(i + 1)
                    .filter(|n| n.fs == "—" && n.start == p.start + p.size)
                    .map_or(0, |n| n.size);
                if size < p.used.unwrap_or(p.size).max(MIB)
                    || size > p.size + next_free
                    || size % MIB != 0
                {
                    return Err("容量不能小于已用空间、跨越相邻分区或未按 MiB 对齐。".into());
                }
                disk.partitions[i].size = size;
                if size > p.size {
                    let delta = size - p.size;
                    let next = &mut disk.partitions[i + 1];
                    next.start += delta;
                    next.size -= delta;
                    if next.size == 0 {
                        disk.partitions.remove(i + 1);
                    }
                } else if size < p.size {
                    disk.partitions.insert(
                        i + 1,
                        free(p.start + size, p.size - size, format!("{}-free", op.id)),
                    );
                }
            }
            _ => return Err("未知操作。".into()),
        }
        let mut merged: Vec<Partition> = Vec::new();
        for p in disk.partitions.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.fs == "—" && p.fs == "—" && last.start + last.size == p.start {
                    last.size += p.size;
                    continue;
                }
            }
            merged.push(p);
        }
        disk.partitions = merged;
        validate(&disk)?;
    }
    Ok(disk)
}

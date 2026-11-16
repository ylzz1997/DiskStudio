import type { Disk, Operation, Partition } from "./models";
import { GiB, MiB } from "./models";
export function validateDisk(disk: Disk) {
  if (
    !disk ||
    !["sample", "image"].includes(disk.kind) ||
    ![512, 4096].includes(disk.sectorSize) ||
    !Array.isArray(disk.partitions) ||
    ![disk.id, disk.name, disk.filename, disk.scheme].every(
      (s) => typeof s === "string" && s.length > 0 && s.length <= 255,
    )
  )
    throw new Error("磁盘模型不完整。");
  if (
    !Number.isSafeInteger(disk.size) ||
    disk.size < 4 * MiB ||
    disk.size > 2 ** 43
  )
    throw new Error("磁盘容量超出支持范围。");
  if (disk.partitions.length > 256) throw new Error("分区数量超过限制。");
  const ids = new Set<string>();
  let end = 0;
  for (const p of disk.partitions) {
    if (
      !p ||
      ![p.id, p.name, p.fs].every(
        (s) => typeof s === "string" && s.length > 0 && s.length <= 255,
      ) ||
      !["blue", "coral", "violet", "mint", "free"].includes(p.color) ||
      typeof p.system !== "boolean" ||
      (p.used !== null && !Number.isSafeInteger(p.used))
    )
      throw new Error("分区模型不完整。");
    if (ids.has(p.id)) throw new Error("分区标识重复。");
    ids.add(p.id);
    if (
      ![p.start, p.size].every(Number.isSafeInteger) ||
      p.start < end ||
      p.size <= 0 ||
      p.start + p.size > disk.size ||
      p.start % disk.sectorSize ||
      p.size % disk.sectorSize
    )
      throw new Error("分区布局重叠、越界或未按扇区对齐。");
    if (p.used !== null && (p.used < 0 || p.used > p.size))
      throw new Error("分区占用空间无效。");
    end = p.start + p.size;
  }
}
const free = (start: number, size: number, id: string): Partition => ({
  id,
  name: "未分配",
  fs: "—",
  start,
  size,
  used: 0,
  color: "free",
  system: false,
});
export function applyOperations(input: Disk, operations: Operation[]): Disk {
  if (operations.length > 128) throw new Error("一次最多处理 128 项变更。");
  if (input.kind !== "sample")
    throw new Error("本机磁盘和导入镜像保持只读；只允许提交内存样本变更。");
  validateDisk(input);
  const disk: Disk = structuredClone(input);
  for (const op of operations) {
    if (!["create", "resize", "format", "rename", "delete"].includes(op.type))
      throw new Error("未知操作。");
    const i = disk.partitions.findIndex((p) => p.id === op.partitionId);
    if (i < 0) throw new Error("目标分区已改变，请重新选择。");
    const p = disk.partitions[i];
    if (p.system) throw new Error("样本中的 EFI 系统分区受保护。");
    if (op.type === "create") {
      if (p.fs !== "—") throw new Error("请先选择未分配空间。");
      const size = op.size ?? 0;
      if (
        !Number.isSafeInteger(size) ||
        size < MiB ||
        size > p.size ||
        size % MiB
      )
        throw new Error("容量必须按 MiB 对齐且不能超出可用空间。");
      validateName(op.name ?? "");
      validateFs(op.fs ?? "");
      const created: Partition = {
        ...p,
        id: op.id,
        name: op.name!,
        fs: op.fs!,
        size,
        used: 0,
        color: "mint",
      };
      disk.partitions.splice(
        i,
        1,
        created,
        ...(size < p.size
          ? [free(p.start + size, p.size - size, `${op.id}-free`)]
          : []),
      );
    } else if (p.fs === "—") throw new Error("此操作需要选择一个分区。");
    else if (op.type === "delete")
      disk.partitions[i] = free(p.start, p.size, p.id);
    else if (op.type === "rename") {
      validateName(op.name ?? "");
      p.name = op.name!;
    } else if (op.type === "format") {
      validateFs(op.fs ?? "");
      validateName(op.name ?? "");
      p.fs = op.fs!;
      p.name = op.name!;
      p.used = 0;
    } else if (op.type === "resize") {
      const size = op.size ?? 0;
      const next = disk.partitions[i + 1];
      const max =
        p.size +
        (next?.fs === "—" && next.start === p.start + p.size ? next.size : 0);
      if (
        !Number.isSafeInteger(size) ||
        size < Math.max(MiB, p.used ?? p.size) ||
        size > max ||
        size % MiB
      )
        throw new Error("容量超出范围：不能小于已用空间，也不能跨越相邻分区。");
      const delta = size - p.size;
      p.size = size;
      if (delta > 0) {
        next.start += delta;
        next.size -= delta;
        if (!next.size) disk.partitions.splice(i + 1, 1);
      }
      if (delta < 0)
        disk.partitions.splice(
          i + 1,
          0,
          free(p.start + size, -delta, `${op.id}-free`),
        );
    }
    disk.partitions = disk.partitions.reduce<Partition[]>((all, part) => {
      const last = all.at(-1);
      if (
        last?.fs === "—" &&
        part.fs === "—" &&
        last.start + last.size === part.start
      )
        last.size += part.size;
      else all.push(part);
      return all;
    }, []);
    validateDisk(disk);
  }
  return disk;
}
function validateName(name: string) {
  if (!name.trim() || name.length > 48 || /[\x00-\x1f\\/]/.test(name))
    throw new Error("名称应为 1–48 个可显示字符。");
}
function validateFs(fs: string) {
  if (!["NTFS", "FAT32", "exFAT"].includes(fs))
    throw new Error("不支持的文件系统。");
}
export function createSample(name: string, sizeGiB: number, id: string): Disk {
  validateName(name);
  if (!Number.isInteger(sizeGiB) || sizeGiB < 1 || sizeGiB > 2048)
    throw new Error("容量应为 1–2048 GiB 的整数。");
  return {
    id,
    name,
    filename: `${name}.img`,
    size: sizeGiB * GiB,
    sectorSize: 512,
    scheme: "GPT",
    kind: "sample",
    partitions: [free(MiB, sizeGiB * GiB - 2 * MiB, `${id}-free`)],
  };
}
export function operationLabel(op: Operation, disk: Disk): string {
  const name =
    disk.partitions.find((p) => p.id === op.partitionId)?.name ?? "分区";
  return {
    resize: `调整 ${name} 的容量`,
    format: `格式化 ${name} 为 ${op.fs}`,
    delete: `删除 ${name}`,
    create: `新建分区 ${op.name}`,
    rename: `将 ${name} 重命名为 ${op.name}`,
  }[op.type];
}

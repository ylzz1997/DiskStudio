import type { Disk, Partition, PartitionTarget } from "./types";

export interface FilesystemSupport {
  name: string;
  browse: string;
  recover: string;
  format: string;
  resize: string;
  allocatedCopy: string;
  freeWipe: string;
}
// Engine capabilities, not device authorization. Inventory never probes a
// filesystem to populate this table; each explicit job revalidates its source.
export const filesystemSupport: readonly FilesystemSupport[] = [
  {
    name: "NTFS",
    browse: "支持",
    recover: "删除记录候选",
    format: "支持",
    resize: "末端缩放",
    allocatedCopy: "干净的 3.1 卷",
    freeWipe: "干净的 3.1 卷",
  },
  {
    name: "FAT12 / FAT16 / FAT32",
    browse: "支持",
    recover: "连续空闲簇候选",
    format: "支持",
    resize: "末端缩放 · 现有分配表",
    allocatedCopy: "分配表校验",
    freeWipe: "分配表校验",
  },
  {
    name: "exFAT",
    browse: "支持",
    recover: "不含删除的碎片链",
    format: "支持",
    resize: "末端缩放 · 现有位图簇链",
    allocatedCopy: "位图校验",
    freeWipe: "位图校验",
  },
  {
    name: "Ext2 / Ext3 / Ext4",
    browse: "干净卸载的卷",
    recover: "未实现",
    format: "1 / 2 / 4 KiB 块",
    resize: "末端缩放 · 特性限制",
    allocatedCopy: "未初始化组完整保留",
    freeWipe: "未实现",
  },
  {
    name: "ReFS / APFS / HFS+ / Btrfs / XFS / 其他",
    browse: "未实现",
    recover: "未实现",
    format: "未实现",
    resize: "未实现",
    allocatedCopy: "逐扇区保留",
    freeWipe: "未实现",
  },
];

export function isFatResizeFilesystem(filesystem: string) {
  return ["FAT", "FAT12", "FAT16", "FAT32", "EXFAT"].includes(
    filesystem.toUpperCase(),
  );
}

export function selectedFilesystemSupport(
  disk: Disk,
  partition: Partition,
  target?: PartitionTarget,
) {
  const coherent =
    target?.available &&
    target.diskId === disk.id &&
    target.target === disk.hardware?.identity &&
    target.layoutKey === disk.hardware?.layoutKey;
  const number = partition.number;
  const windows = !!(
    coherent &&
    number != null &&
    target.formatPartitions.includes(number)
  );
  const ext = !!(
    coherent &&
    number != null &&
    target.extPartitions.includes(number)
  );
  const ntfsResize = !!(
    coherent &&
    number != null &&
    partition.fs.toUpperCase() === "NTFS" &&
    target.resizePartitions.includes(number)
  );
  const fatResize = !!(
    coherent &&
    number != null &&
    isFatResizeFilesystem(partition.fs) &&
    target.fatResizePartitions.includes(number)
  );
  return {
    // The partition GUID may permit inspection, but cannot establish an Ext FS.
    filesystem: partition.fs,
    format: windows
      ? "NTFS、FAT12/16/32、exFAT"
      : ext
        ? "Ext2、Ext3、Ext4"
        : "当前分区不可格式化",
    resize: ntfsResize
      ? "NTFS 末端缩放，容量范围由 Windows 检查"
      : ext
        ? "需显式检查 Ext 文件系统及支持的特性"
        : fatResize
          ? "需显式检查 FAT 或 exFAT 的完整目录、分配与尾部空间"
          : "当前分区不可调整大小",
  };
}

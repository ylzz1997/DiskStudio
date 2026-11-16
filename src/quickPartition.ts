import { Channel, invoke } from "@tauri-apps/api/core";
import type { QuickProgress } from "./quickProgress";
import { native } from "./backend";
import { MiB } from "./types";
import type {
  Disk,
  FormatOptions,
  PartitionPlan,
  PartitionTarget,
  WindowsFileSystem,
} from "./types";

export type QuickRole = "windows" | "linux" | "efi" | "reserved";
export const quickRoleNames: Record<QuickRole, string> = {
  windows: "Windows 数据",
  linux: "Linux 数据",
  efi: "ESP 条目",
  reserved: "MSR 条目",
};
export interface QuickEntry {
  offset: number;
  size: number;
  role: QuickRole;
  format: {
    fileSystem: WindowsFileSystem;
    label: string;
    clusterSize: number;
    options: FormatOptions;
  } | null;
}
export type QuickStep =
  | {
      type: "delete";
      partition: number;
      offset: number;
      size: number;
      guid: string;
    }
  | { type: "create" | "format"; entry: QuickEntry };
export interface QuickPreview extends PartitionPlan {
  steps: QuickStep[];
  retainedPartitions: number[];
}
export async function prepareQuickPartition(
  target: PartitionTarget,
  alignment: number,
  deletePartitions: number[],
  entries: QuickEntry[],
): Promise<QuickPreview> {
  if (!native) throw new Error("快速分区在 Windows 桌面版中提供。");
  return invoke("quick_partition_prepare", {
    request: {
      target: target.target,
      revision: target.revision,
      alignment,
      deletePartitions,
      entries,
    },
  });
}
export async function executeQuickPartition(
  token: string,
  onProgress: (event: QuickProgress) => void,
): Promise<{ message: string; verified: boolean }> {
  if (!native) throw new Error("浏览器不执行磁盘操作。");
  const progress = new Channel<QuickProgress>();
  progress.onmessage = onProgress;
  try {
    return await invoke("quick_partition_execute", { token, progress });
  } finally {
    progress.onmessage = () => {};
  }
}
export async function discardQuickPartition(token: string) {
  if (native) await invoke("quick_partition_discard", { token });
}

// Suggestions only. Native validation uses a fresh complete provider inventory.
export function quickFreeRanges(
  disk: Disk,
  deleted: number[],
  alignment: number,
) {
  if (
    !Number.isSafeInteger(alignment) ||
    ![1, 2, 4, 8, 16].includes(alignment / MiB)
  )
    throw new Error("对齐必须为 1、2、4、8 或 16 MiB。");
  const parts = disk.partitions
    .filter((p) => p.number == null || !deleted.includes(p.number))
    .sort((a, b) => a.start - b.start);
  const ranges: { offset: number; size: number }[] = [];
  let end = MiB;
  for (const p of [...parts, { start: disk.size - MiB, size: 0 }]) {
    const offset = Math.ceil(end / alignment) * alignment;
    const stop = Math.floor(p.start / MiB) * MiB;
    if (stop > offset) ranges.push({ offset, size: stop - offset });
    end = Math.max(end, p.start + p.size);
  }
  return ranges;
}
export function divideQuickRange(
  range: { offset: number; size: number },
  count: number,
  alignment: number,
): QuickEntry[] {
  if (
    !Number.isInteger(count) ||
    count < 1 ||
    count > 32 ||
    !Number.isSafeInteger(range.offset + range.size) ||
    ![1, 2, 4, 8, 16].includes(alignment / MiB) ||
    range.offset % alignment !== 0
  )
    throw new Error("分区数量或空间对齐无效。");
  const size = Math.floor(range.size / count / alignment) * alignment;
  if (size < 64 * MiB)
    throw new Error(
      "均分后的每个分区至少需要 64 MiB；可在明细中单独设置更小的 FAT 分区。",
    );
  return Array.from({ length: count }, (_, i) => ({
    offset: range.offset + size * i,
    size:
      i === count - 1 ? Math.floor((range.size - i * size) / MiB) * MiB : size,
    role: "windows",
    format: {
      fileSystem: "NTFS",
      label: `DATA${i + 1}`,
      clusterSize: 0,
      options: { mode: "quick", ntfsCompression: false, ntfsShortNames: false },
    },
  }));
}

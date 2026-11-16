import type { Disk } from "./types";
export type MaintenanceMode = "benchmark" | "wipe" | "wipeFree" | "trim";
export type MaintenanceOperation =
  | {
      kind: "wipe" | "wipeFree";
      options: {
        passes: number;
        pattern: { kind: "random" } | { kind: "bytes"; hex: string };
      };
    }
  | {
      kind: "benchmark";
      options: {
        access: "sequential" | "random";
        blockSize: number;
        operations: number;
        write: boolean;
      };
    }
  | { kind: "trim" };
export interface MaintenancePreview {
  token: string;
  title: string;
  target: string;
  offset: number;
  size: number;
  sectorSize: number;
  destructive: boolean;
  warning: string;
}
export interface MaintenanceReport {
  id: string;
  source: { target: string; layoutKey: string; partition: number | null };
  title: string;
  target: string;
  operation: MaintenanceOperation;
  offset: number;
  size: number;
  state: string;
  phase: string;
  message: string;
  startedAt: number;
  diskLeftOffline: boolean;
  freeSpace: null | {
    filesystem: string;
    freeBytes: number;
    preservedBytes: number;
    allocationSha256: string;
    preservedSha256: string | null;
  };
  wipe: null | {
    pass: number;
    phase: string;
    writtenBytes: number;
    readbackBytes: number;
    verifiedPasses: number;
    passSha256: string[];
    elapsedMs: number;
  };
  benchmark: null | {
    operations: number;
    transferredBytes: number;
    verifiedBytes: number;
    elapsedMs: number;
    ioNanoseconds: number;
    minMicroseconds: number;
    maxMicroseconds: number;
    p50Microseconds: number;
    p95Microseconds: number;
    mibPerSecond: number;
    iops: number;
  };
}
export function maintenanceRange(
  start: string,
  count: string,
  size: number,
  sector: number,
) {
  if (!/^\d+$/.test(start) || !/^\d+$/.test(count))
    throw Error("扇区位置与数量须为非负整数。");
  const startSector = Number(start),
    sectorCount = Number(count);
  if (
    ![startSector, sectorCount, size, sector].every(Number.isSafeInteger) ||
    ![512, 4096].includes(sector) ||
    startSector < 0 ||
    sectorCount < 1 ||
    size % sector !== 0 ||
    startSector > size / sector - sectorCount
  )
    throw Error("扇区范围为空或超出所选磁盘/分区。");
  return { startSector, sectorCount };
}
export function maintenanceTarget(disk: Disk, partition: string) {
  if (!disk.hardware?.identity || !disk.hardware.layoutKey)
    throw Error("请刷新磁盘信息后重试。");
  const part = partition
    ? disk.partitions.find((p) => p.number === Number(partition))
    : undefined;
  if (partition && !part) throw Error("所选分区已经变化。");
  return {
    source: {
      target: disk.hardware.identity,
      layoutKey: disk.hardware.layoutKey,
      partition: part?.number ?? null,
    },
    offset: part?.start ?? 0,
    size: part?.size ?? disk.size,
  };
}
export function wipePattern(random: boolean, input: string) {
  if (random) return { kind: "random" } as const;
  const hex = input.replace(/\s+/g, "").toUpperCase();
  if (!/^(?:[0-9A-F]{2}){1,32}$/.test(hex))
    throw Error("请输入 1–32 字节十六进制填充值，例如 00 或 A5FF。");
  return { kind: "bytes", hex } as const;
}

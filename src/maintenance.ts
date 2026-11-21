import type { Disk } from "./types";
export type MaintenanceMode = "benchmark" | "wipe" | "wipeFree" | "trim";
export type BenchmarkMode = "read" | "write" | "mixed";
export type BenchmarkAccess = "sequential" | "random";
interface BenchmarkOptions {
  access: BenchmarkAccess;
  blockSize: number;
  operations: number;
  write: boolean;
  queueDepth: number;
  readPercent?: number;
}
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
      options: BenchmarkOptions;
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
    readOperations: number;
    writeOperations: number;
    readBytes: number;
    writeBytes: number;
    peakQueueDepth: number;
    samples: BenchmarkSample[];
  };
}
export interface BenchmarkSample {
  elapsedNanoseconds: number;
  readBytes: number;
  writeBytes: number;
}
export function benchmarkOptions({
  access,
  blockSize,
  operations,
  mode,
  queueDepth,
  readPercent,
}: {
  access: BenchmarkAccess;
  blockSize: number;
  operations: string;
  mode: BenchmarkMode;
  queueDepth: string;
  readPercent: string;
}): BenchmarkOptions {
  const count = Number(operations);
  if (!/^\d+$/.test(operations) || count < 1 || count > 65536)
    throw Error("请求次数须为 1–65536。");
  const depth = mode === "read" ? Number(queueDepth) : 1;
  if (
    mode === "read" &&
    (!/^\d+$/.test(queueDepth) || depth < 1 || depth > 32 || depth > count)
  )
    throw Error(
      "只读队列深度须为 1–32 且不超过请求次数。含写入的测速使用 QD1。",
    );
  const percent = Number(readPercent);
  if (
    mode === "mixed" &&
    (!/^\d+$/.test(readPercent) || percent < 1 || percent > 99 || count < 2)
  )
    throw Error("混合测速至少需要两个请求，读取比例须为 1–99%。");
  return {
    access,
    blockSize,
    operations: count,
    write: mode !== "read",
    queueDepth: depth,
    ...(mode === "mixed" ? { readPercent: percent } : {}),
  };
}
export function benchmarkThroughput(samples: BenchmarkSample[]) {
  let previous: BenchmarkSample = {
    elapsedNanoseconds: 0,
    readBytes: 0,
    writeBytes: 0,
  };
  return samples.flatMap((sample) => {
    const span = sample.elapsedNanoseconds - previous.elapsedNanoseconds;
    if (
      span <= 0 ||
      sample.readBytes < previous.readBytes ||
      sample.writeBytes < previous.writeBytes
    )
      return [];
    const toMiBPerSecond = 1e9 / 1048576 / span;
    const interval = {
      startSeconds: previous.elapsedNanoseconds / 1e9,
      endSeconds: sample.elapsedNanoseconds / 1e9,
      readMiBPerSecond:
        (sample.readBytes - previous.readBytes) * toMiBPerSecond,
      writeMiBPerSecond:
        (sample.writeBytes - previous.writeBytes) * toMiBPerSecond,
    };
    previous = sample;
    return [interval];
  });
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

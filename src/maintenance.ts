import type { Disk } from "./types";
export type MaintenanceMode = "benchmark" | "wipe" | "wipeFree" | "trim";
export type BenchmarkMode = "read" | "write" | "mixed";
export type BenchmarkAccess = "sequential" | "random";
export interface BenchmarkOptions {
  access: BenchmarkAccess;
  blockSize: number;
  operations: number;
  durationSeconds?: number;
  write: boolean;
  queueDepth: number;
  threadCount?: number;
  readPercent?: number;
}
export interface BenchmarkForm {
  access: BenchmarkAccess;
  blockSize: number;
  operations: string;
  mode: BenchmarkMode;
  queueDepth: string;
  threadCount?: string;
  readPercent: string;
  limit?: "requests" | "duration";
  duration?: string;
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
  | {
      kind: "fileBenchmark";
      options: BenchmarkOptions;
      additionalWorkloads?: BenchmarkOptions[];
      repetitions?: number;
      fileSize: number;
      zeroFill: boolean;
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
  requiresAcknowledgment: boolean;
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
  benchmark: BenchmarkStats | null;
  fileBenchmarkRuns?: FileBenchmarkRun[];
}
export interface FileBenchmarkRun {
  round: number;
  workload: number;
  state: "running" | "completed" | "canceled" | "failed";
  stats: BenchmarkStats | null;
}
export interface BenchmarkStats {
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
  workerThreads?: number;
  approximatePercentiles?: boolean;
  samples: BenchmarkSample[];
}
export interface BenchmarkSample {
  elapsedNanoseconds: number;
  readBytes: number;
  writeBytes: number;
}
export function benchmarkOptions(
  {
    access,
    blockSize,
    operations,
    mode,
    queueDepth,
    threadCount,
    readPercent,
    limit,
    duration,
  }: BenchmarkForm,
  file = false,
): BenchmarkOptions {
  const timed = limit === "duration";
  const seconds = Number(duration);
  const count = timed ? 0 : Number(operations);
  if (timed && (!/^\d+$/.test(duration ?? "") || seconds < 5 || seconds > 60))
    throw Error("测速时长须为 5–60 秒，且不能同时指定请求次数。");
  if (!timed && (!/^\d+$/.test(operations) || count < 1 || count > 65536))
    throw Error("请求次数须为 1–65536。");
  const depth = mode === "read" ? Number(queueDepth) : 1;
  const threads = mode === "read" && file ? Number(threadCount ?? "1") : 1;
  if (
    mode === "read" &&
    (!/^\d+$/.test(queueDepth) ||
      depth < 1 ||
      depth > (file ? 512 : 32) ||
      (!timed && depth > count) ||
      (file &&
        (!/^\d+$/.test(threadCount ?? "1") ||
          threads < 1 ||
          threads > 64 ||
          threads > depth)))
  )
    throw Error(
      file
        ? "文件读取支持 QD1–QD512 与 1–64 个线程。线程数不能超过队列深度，按次数运行时队列深度不能超过请求数。含写入的测速使用单线程 QD1。"
        : "只读队列深度须为 1–32 且不超过请求次数。含写入的测速使用 QD1。",
    );
  const percent = Number(readPercent);
  if (
    mode === "mixed" &&
    (!/^\d+$/.test(readPercent) ||
      percent < 1 ||
      percent > 99 ||
      (!timed && count < 2))
  )
    throw Error("混合测速至少需要两个请求，读取比例须为 1–99%。");
  return {
    access,
    blockSize,
    operations: count,
    ...(timed ? { durationSeconds: seconds } : {}),
    write: mode !== "read",
    queueDepth: depth,
    ...(file && mode === "read" ? { threadCount: threads } : {}),
    ...(mode === "mixed" ? { readPercent: percent } : {}),
  };
}

export type BenchmarkUnit = "MiB/s" | "MB/s" | "IOPS";

export function benchmarkRates(
  stats: BenchmarkStats | null,
  unit: BenchmarkUnit,
) {
  const seconds = (stats?.samples.at(-1)?.elapsedNanoseconds ?? 0) / 1e9;
  if (!stats || seconds <= 0) return null;
  if (unit === "IOPS")
    return [stats.readOperations / seconds, stats.writeOperations / seconds];
  const divisor = unit === "MiB/s" ? 1048576 : 1000000;
  return [
    stats.readBytes / seconds / divisor,
    stats.writeBytes / seconds / divisor,
  ];
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
export function benchmarkFileSize(input: string) {
  const size = Number(input);
  if (!/^\d+$/.test(input) || size < 1 || size > 4095)
    throw Error("文件大小须为 1–4095 MiB 的整数。");
  return size * 1048576;
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

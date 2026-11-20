export interface SurfaceSnapshot {
  id: string;
  target: string;
  source: string;
  startedAt: number;
  partition: number | null;
  options: { startSector: number; sectorCount: number };
  loaded: boolean;
  resumeCount: number;
  report: {
    state:
      "running" | "completed" | "paused" | "canceled" | "failed" | "timedOut";
    offset: number;
    size: number;
    sectorSize: number;
    blockSize: number;
    checkedBytes: number;
    readableBytes: number;
    failedBytes: number;
    elapsedMs: number;
    latencyBlocks: number[];
    cells: { checkedBytes: number; failedBytes: number; worst: number }[];
    failures: {
      startSector: number;
      sectorCount: number;
      code: number;
      elapsedMs: number;
    }[];
    omittedFailures: number;
    message: string;
    readTimeoutSeconds?: number;
    timedOutRead?: { startSector: number; sectorCount: number };
    refinement?: {
      reads: number;
      failedSectors: number;
      unresolvedSectors: number;
      ranges: {
        startSector: number;
        sectorCount: number;
        code: number;
        resolution: "sectorReadFailed" | "unresolved";
      }[];
      omittedRanges: number;
    };
  };
}

export interface SurfaceStatus {
  scans: {
    target: string;
    state: SurfaceSnapshot["report"]["state"];
    checkedBytes: number;
    size: number;
    pausing: boolean;
  }[];
  selected: SurfaceSnapshot | null;
  repairAvailable: boolean;
}

export interface SurfaceRepairBlock {
  startLba: number;
  sectorCount: number;
  failed: { lba: number; code: number }[];
  sha256: string;
}

export interface SurfaceRepairPreview {
  token: string;
  target: string;
  range: { offset: number; size: number; sector: number };
  logicalSectorSize: number;
  physicalSectorSize: number;
  excludedSectors: number;
  unresolvedSectors: number;
  nowReadableSectors: number;
  blocks: SurfaceRepairBlock[];
}

export interface SurfaceRepairOutcome {
  state: "completed" | "failed" | "canceled";
  plannedBlocks: number;
  attemptedBlocks: number;
  verifiedBlocks: SurfaceRepairBlock[];
  diskLeftOffline: boolean | null;
  message: string;
}

export type SurfaceReportFormat = "json" | "text" | "excel";

export function surfaceCanResume(snapshot: SurfaceSnapshot | null) {
  return (
    !!snapshot &&
    ["paused", "canceled", "failed", "timedOut"].includes(
      snapshot.report.state,
    ) &&
    snapshot.report.checkedBytes < snapshot.report.size &&
    snapshot.resumeCount < 1024
  );
}
export function surfaceReadTimeout(text: string): number | null {
  if (text === "") return null;
  const seconds = Number(text);
  if (!/^\d+$/.test(text) || seconds < 1 || seconds > 120)
    throw Error("单次读取超时须为 1 到 120 秒。");
  return seconds;
}
export function surfaceRange(
  startText: string,
  countText: string,
  size: number,
  sector: number,
) {
  if (
    ![512, 4096].includes(sector) ||
    !Number.isSafeInteger(size) ||
    size % sector !== 0
  )
    throw Error("磁盘容量或扇区信息不完整，请刷新。 ");
  if (!/^\d+$/.test(startText) || !/^\d+$/.test(countText))
    throw Error("起始扇区和数量须为整数。 ");
  const startSector = Number(startText),
    sectorCount = Number(countText);
  if (
    !Number.isSafeInteger(startSector) ||
    !Number.isSafeInteger(sectorCount) ||
    sectorCount < 1 ||
    startSector > size / sector - sectorCount
  )
    throw Error("检测范围超出所选磁盘或分区。 ");
  return { startSector, sectorCount };
}

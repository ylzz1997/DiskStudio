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
    state: "running" | "completed" | "paused" | "canceled" | "failed";
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
  };
}
export function surfaceCanResume(snapshot: SurfaceSnapshot | null) {
  return (
    !!snapshot &&
    ["paused", "canceled", "failed"].includes(snapshot.report.state) &&
    snapshot.report.checkedBytes < snapshot.report.size &&
    snapshot.resumeCount < 1024
  );
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

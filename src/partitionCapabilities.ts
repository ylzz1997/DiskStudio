import type { Disk, PartitionStatus } from "./types";

// The display snapshot and operation capabilities are read independently.
// Disk numbers can be reused after hot-plugging; never join by number alone.
export function matchPartitionTarget(
  disk: Disk,
  status: PartitionStatus | null,
) {
  if (!status?.enabled || !disk.hardware?.identity || !disk.hardware.layoutKey)
    return undefined;
  const candidates = status.targets.filter(
    (target) =>
      target.diskId === disk.id &&
      target.target === disk.hardware!.identity &&
      target.layoutKey === disk.hardware!.layoutKey,
  );
  return candidates.length === 1 ? candidates[0] : undefined;
}

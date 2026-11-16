import type { DiskSnapshot, PartitionStatus } from "./types";
import { errorText } from "./partitionOutcome";

// Both callbacks are fixed no-input metadata commands. Keeping the results
// separate preserves a valid inventory when only the capability query fails.
export async function readLocalState(
  disks: () => Promise<DiskSnapshot>,
  capabilities: () => Promise<PartitionStatus>,
) {
  const [inventory, access] = await Promise.allSettled([
    disks(),
    capabilities(),
  ]);
  const errors: string[] = [];
  if (inventory.status === "rejected")
    errors.push(`磁盘信息：${errorText(inventory.reason)}`);
  if (access.status === "rejected")
    errors.push(`操作能力：${errorText(access.reason)}`);
  return {
    snapshot: inventory.status === "fulfilled" ? inventory.value : null,
    access:
      inventory.status === "fulfilled" && access.status === "fulfilled"
        ? access.value
        : null,
    error: errors.join("；"),
  };
}

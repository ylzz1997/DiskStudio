import { describe, expect, it } from "vitest";
import { matchPartitionTarget } from "./partitionCapabilities";
import type { Disk, PartitionStatus } from "./types";

const disk = {
  id: "local-7",
  kind: "physical",
  hardware: { identity: "original", layoutKey: "layout-1" },
} as Disk;
const status: PartitionStatus = {
  enabled: true,
  reason: null,
  targets: [
    {
      diskId: "local-7",
      target: "original",
      layoutKey: "layout-1",
      available: true,
      reason: null,
      name: "磁盘 7",
      serial: "synthetic",
      revision: "revision-1",
      freeRanges: [],
      formatPartitions: [],
      deletePartitions: [],
      resizePartitions: [],
      extPartitions: [],
      metadataPartitions: [],
      availableDriveLetters: [],
    },
  ],
};

describe("joining display and operation snapshots", () => {
  it("matches one coherent disk but rejects replacements and changed layouts", () => {
    expect(matchPartitionTarget(disk, status)).toBe(status.targets[0]);
    for (const change of [
      { target: "replacement" },
      { layoutKey: "layout-2" },
    ]) {
      expect(
        matchPartitionTarget(disk, {
          ...status,
          targets: [{ ...status.targets[0], ...change }],
        }),
      ).toBeUndefined();
    }
    expect(
      matchPartitionTarget(disk, {
        ...status,
        targets: [...status.targets, ...status.targets],
      }),
    ).toBeUndefined();
    expect(matchPartitionTarget(disk, null)).toBeUndefined();
  });
});

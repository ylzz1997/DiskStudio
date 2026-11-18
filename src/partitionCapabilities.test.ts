import { describe, expect, it } from "vitest";
import { matchPartitionTarget } from "./partitionCapabilities";
import { mergeCandidates } from "./PartitionReallocationDialog";
import type { Disk, Partition, PartitionStatus } from "./types";

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
      fatResizePartitions: [],
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

it("only offers immediately neighbouring deletable NTFS volumes for merge", () => {
  const makePartition = (number: number): Partition => ({
    id: String(number),
    name: String(number),
    fs: "NTFS",
    start: number * 100,
    size: 100,
    used: null,
    color: "blue",
    system: false,
    number,
  });
  const [left, keep, right, far] = [1, 2, 3, 4].map(makePartition);
  const target = {
    ...status.targets[0],
    formatPartitions: [1, 2, 3, 4],
    resizePartitions: [1, 2, 3, 4],
    deletePartitions: [1, 3, 4],
  };
  const gpt = {
    ...disk,
    scheme: "GPT",
    partitions: [far, keep, left, right],
    hardware: { ...disk.hardware!, isSystem: false },
  };
  expect(mergeCandidates(gpt, keep, target)).toEqual([left, right]);
  expect(
    mergeCandidates(gpt, keep, { ...target, deletePartitions: [1, 4] }),
  ).toEqual([left]);
  expect(
    mergeCandidates(
      { ...gpt, hardware: { ...gpt.hardware, isSystem: true } },
      keep,
      target,
    ),
  ).toEqual([right]);
  expect(mergeCandidates({ ...gpt, kind: "image" }, keep, target)).toHaveLength(
    0,
  );
});

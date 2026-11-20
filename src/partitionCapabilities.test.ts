import { describe, expect, it } from "vitest";
import { matchPartitionTarget } from "./partitionCapabilities";
import { mergeCandidates } from "./PartitionReallocationDialog";
import { physicalMemberOptions } from "./LvmBrowserDialog";
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

it("offers a whole-disk read scope when a data disk only has an MSR restriction", () => {
  const reserved: Partition = {
    id: "msr", name: "Reserved", fs: "", start: 1048576, size: 16777216,
    used: null, color: "gray", system: true, number: 1, role: "MSR 保留",
  };
  const dataDisk = {
    ...disk,
    size: 64000000,
    partitions: [reserved],
    hardware: { ...disk.hardware!, number: 7, isSystem: false },
  };
  const labels = (key: string) => key;
  const scopes = (candidate: Disk) =>
    physicalMemberOptions([candidate], labels).map(({ source }) => source.partition);
  expect(scopes(dataDisk)).toEqual([null]);
  expect(scopes({
    ...dataDisk,
    hardware: { ...dataDisk.hardware, isSystem: true },
  })).toEqual([]);
  expect(scopes({
    ...dataDisk,
    partitions: [{ ...reserved, role: "系统分区" }],
  })).toEqual([]);
});

import { describe, it, expect } from "vitest";
import { applyOperations, createSample, validateDisk } from "./domain";
import samples from "../fixtures/samples.json";
import type { Disk, Operation } from "./models";
import { GiB, MiB } from "./models";
const fixture = (i = 0) => structuredClone(samples[i]) as Disk;
const op = (
  type: Operation["type"],
  partitionId: string,
  size?: number,
): Operation => ({
  id: "test",
  type,
  partitionId,
  size,
  name: "New",
  fs: "FAT32",
});
describe("sample editing contract", () => {
  it("preserves input on failure and rejects read-only images", () => {
    const original = fixture();
    const snapshot = structuredClone(original);
    expect(() =>
      applyOperations(original, [
        op("delete", "archive"),
        op("resize", "missing", GiB),
      ]),
    ).toThrow();
    expect(original).toEqual(snapshot);
    expect(() =>
      applyOperations({ ...original, kind: "image" }, [
        op("delete", "archive"),
      ]),
    ).toThrow("只读");
  });
  it("never resizes across a neighbor or below occupied bytes", () => {
    expect(() =>
      applyOperations(fixture(), [op("resize", "workspace", 41 * GiB)]),
    ).toThrow();
    expect(() =>
      applyOperations(fixture(), [op("resize", "workspace", GiB)]),
    ).toThrow();
    expect(() => applyOperations(fixture(), [op("delete", "efi")])).toThrow(
      "受保护",
    );
  });
  it("rejects every editing operation for physical disk metadata", () => {
    const disk = { ...fixture(), kind: "physical" as const };
    for (const type of [
      "create",
      "resize",
      "format",
      "delete",
      "rename",
    ] as const) {
      expect(() => applyOperations(disk, [op(type, "workspace", GiB)])).toThrow(
        "只读",
      );
    }
    expect(disk.partitions).toEqual(fixture().partitions);
  });
  it("coalesces adjacent free extents and conserves disk geometry", () => {
    const original = fixture(1);
    const small = applyOperations(original, [
      op("resize", "backup-data", 16 * GiB),
    ]);
    expect(small.partitions).toHaveLength(2);
    const large = applyOperations(small, [
      op("resize", "backup-data", 24 * GiB),
    ]);
    expect(large.partitions.map((p) => [p.start, p.size])).toEqual(
      original.partitions.map((p) => [p.start, p.size]),
    );
  });
  it("creates within an unallocated extent and clears formatted sample usage", () => {
    const disk = createSample("测试", 8, "new");
    const result = applyOperations(disk, [op("create", "new-free", 4 * GiB)]);
    expect(result.partitions[0].name).toBe("New");
    expect(result.partitions.reduce((n, p) => n + p.size, 0)).toBe(
      8 * GiB - 2 * MiB,
    );
    expect(
      applyOperations(fixture(), [op("format", "workspace")]).partitions[1]
        .used,
    ).toBe(0);
  });
  it("rejects malformed saved models and overflowing coordinates", () => {
    expect(() => validateDisk({ ...fixture(), sectorSize: 0 })).toThrow();
    const disk = fixture();
    disk.partitions[1].start = Number.MAX_SAFE_INTEGER;
    expect(() => validateDisk(disk)).toThrow();
    expect(() => createSample("too large", 4096, "x")).toThrow();
  });
});

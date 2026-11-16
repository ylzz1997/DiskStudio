import { describe, expect, it } from "vitest";
import { divideQuickRange, quickFreeRanges } from "./quickPartition";
import { GiB, MiB } from "./types";
import type { Disk } from "./types";

const disk: Disk = {
  id: "synthetic",
  name: "synthetic",
  filename: "",
  size: GiB,
  sectorSize: 512,
  scheme: "GPT",
  kind: "physical",
  partitions: [
    {
      id: "protected",
      number: 1,
      name: "protected",
      fs: "unknown",
      start: MiB,
      size: 15 * MiB,
      used: null,
      color: "",
      system: true,
    },
    {
      id: "data",
      number: 2,
      name: "data",
      fs: "NTFS",
      start: 128 * MiB,
      size: 128 * MiB,
      used: null,
      color: "",
      system: false,
    },
  ],
};
describe("quick partition suggestions", () => {
  it("preserves retained intervals and merges only explicitly removed ones", () => {
    expect(quickFreeRanges(disk, [], MiB)).toEqual([
      { offset: 16 * MiB, size: 112 * MiB },
      { offset: 256 * MiB, size: 767 * MiB },
    ]);
    expect(quickFreeRanges(disk, [2], MiB)).toEqual([
      { offset: 16 * MiB, size: 1007 * MiB },
    ]);
  });
  it("aligns starts inward and never crosses a retained partition", () => {
    const altered = {
      ...disk,
      partitions: [{ ...disk.partitions[0], size: 18 * MiB }],
    };
    expect(quickFreeRanges(altered, [], 16 * MiB)).toEqual([
      { offset: 32 * MiB, size: 991 * MiB },
    ]);
  });
  it("splits with aligned starts and assigns the bounded remainder to the final entry", () => {
    const entries = divideQuickRange(
      { offset: 16 * MiB, size: 1007 * MiB },
      3,
      16 * MiB,
    );
    expect(entries.map((e) => e.size / MiB)).toEqual([320, 320, 367]);
    expect(entries.map((e) => e.offset / MiB)).toEqual([16, 336, 656]);
    expect(entries[2].offset + entries[2].size).toBe(1023 * MiB);
    expect(entries.every((e) => e.format?.options.mode === "quick")).toBe(true);
  });
  it("rejects invalid counts and insufficient space before producing a proposal", () => {
    for (const count of [0, 33, 1.5, NaN])
      expect(() =>
        divideQuickRange({ offset: MiB, size: GiB }, count, MiB),
      ).toThrow();
    expect(() =>
      divideQuickRange({ offset: MiB, size: 127 * MiB }, 2, MiB),
    ).toThrow();
    expect(() =>
      divideQuickRange({ offset: MiB, size: GiB }, 2, 16 * MiB),
    ).toThrow();
    expect(() => quickFreeRanges(disk, [], 3 * MiB)).toThrow();
  });
});

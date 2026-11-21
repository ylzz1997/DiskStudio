import { describe, it, expect } from "vitest";
import {
  benchmarkOptions,
  benchmarkThroughput,
  maintenanceRange,
  maintenanceTarget,
  wipePattern,
} from "./maintenance";
import type { Disk } from "./types";
describe("maintenance request boundaries", () => {
  it("binds queue depth to read-only workloads and validates mixed reads", () => {
    const input = {
      access: "random",
      blockSize: 4096,
      operations: "64",
      mode: "read",
      queueDepth: "4",
      readPercent: "70",
    } as const;
    expect(benchmarkOptions(input)).toEqual({
      access: "random",
      blockSize: 4096,
      operations: 64,
      write: false,
      queueDepth: 4,
    });
    expect(benchmarkOptions({ ...input, mode: "mixed" })).toMatchObject({
      write: true,
      queueDepth: 1,
      readPercent: 70,
    });
    for (const queueDepth of ["0", "33", "1.1", "1e1"])
      expect(() => benchmarkOptions({ ...input, queueDepth })).toThrow();
    expect(() => benchmarkOptions({ ...input, operations: "3" })).toThrow();
    expect(() =>
      benchmarkOptions({ ...input, mode: "mixed", operations: "1" }),
    ).toThrow();
    expect(() =>
      benchmarkOptions({ ...input, mode: "mixed", readPercent: "100" }),
    ).toThrow();
  });
  it("derives separate rates over the actual intervals of cumulative I/O samples", () => {
    expect(
      benchmarkThroughput([
        {
          elapsedNanoseconds: 250_000_000,
          readBytes: 524288,
          writeBytes: 262144,
        },
        { elapsedNanoseconds: 250_000_000, readBytes: 1048576, writeBytes: 0 },
        {
          elapsedNanoseconds: 750_000_000,
          readBytes: 786432,
          writeBytes: 786432,
        },
      ]),
    ).toEqual([
      {
        startSeconds: 0,
        endSeconds: 0.25,
        readMiBPerSecond: 2,
        writeMiBPerSecond: 1,
      },
      {
        startSeconds: 0.25,
        endSeconds: 0.75,
        readMiBPerSecond: 0.5,
        writeMiBPerSecond: 1,
      },
    ]);
  });
  it("keeps 512 and 4Kn ranges bounded and rejects unsafe integers", () => {
    expect(maintenanceRange("1", "7", 4096, 512)).toEqual({
      startSector: 1,
      sectorCount: 7,
    });
    expect(maintenanceRange("0", "1", 4096, 4096).sectorCount).toBe(1);
    for (const [a, n] of [
      ["-1", "1"],
      ["0", "0"],
      ["1", "8"],
      ["1.5", "1"],
      ["1e3", "1"],
      ["9007199254740992", "1"],
    ])
      expect(() => maintenanceRange(a, n, 4096, 512)).toThrow();
  });
  it("normalizes only actual hex bytes and rejects empty/odd/oversized patterns", () => {
    expect(wipePattern(false, "a5 ff 00")).toEqual({
      kind: "bytes",
      hex: "A5FF00",
    });
    expect(wipePattern(true, "irrelevant")).toEqual({ kind: "random" });
    for (const s of ["", "A", "GG", "00".repeat(33)])
      expect(() => wipePattern(false, s)).toThrow();
  });
  it("binds the selected partition offset instead of guessing from the disk", () => {
    const disk = {
      size: 65536,
      hardware: { identity: "id", layoutKey: "layout" },
      partitions: [{ number: 3, start: 8192, size: 4096 }],
    } as Disk;
    expect(maintenanceTarget(disk, "3")).toEqual({
      source: { target: "id", layoutKey: "layout", partition: 3 },
      offset: 8192,
      size: 4096,
    });
    expect(maintenanceTarget(disk, "").size).toBe(65536);
    expect(() => maintenanceTarget(disk, "4")).toThrow();
    expect(() =>
      maintenanceTarget({ ...disk, hardware: undefined }, ""),
    ).toThrow();
  });
});

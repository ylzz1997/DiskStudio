import { describe, it, expect } from "vitest";
import {
  maintenanceRange,
  maintenanceTarget,
  wipePattern,
} from "./maintenance";
import type { Disk } from "./types";
describe("maintenance request boundaries", () => {
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

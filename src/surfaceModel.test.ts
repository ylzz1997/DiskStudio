import { describe, expect, it } from "vitest";
import {
  surfaceRange,
  surfaceCanResume,
  type SurfaceSnapshot,
} from "./surfaceModel";
describe("surface range", () => {
  it("only incomplete stopped sessions can resume", () => {
    const snapshot = {
      resumeCount: 0,
      report: { state: "paused", checkedBytes: 1048576, size: 2097152 },
    } as SurfaceSnapshot;
    expect(surfaceCanResume(snapshot)).toBe(true);
    expect(surfaceCanResume(null)).toBe(false);
    for (const state of ["running", "completed"] as const)
      expect(
        surfaceCanResume({
          ...snapshot,
          report: { ...snapshot.report, state },
        }),
      ).toBe(false);
    expect(
      surfaceCanResume({
        ...snapshot,
        report: { ...snapshot.report, checkedBytes: 2097152 },
      }),
    ).toBe(false);
    expect(surfaceCanResume({ ...snapshot, resumeCount: 1024 })).toBe(false);
  });
  it("accepts full disk and 4Kn partition-relative bounds", () => {
    expect(surfaceRange("0", "2048", 1048576, 512)).toEqual({
      startSector: 0,
      sectorCount: 2048,
    });
    expect(surfaceRange("1", "1", 8192, 4096)).toEqual({
      startSector: 1,
      sectorCount: 1,
    });
  });
  it("rejects unsafe numbers, fractions, empty and out-of-range input", () => {
    for (const [start, count] of [
      ["-1", "1"],
      ["1.5", "1"],
      ["0", "0"],
      ["", "1"],
      ["1e3", "1"],
      ["0", "3"],
      ["2", "1"],
      ["9007199254740992", "1"],
    ])
      expect(() => surfaceRange(start, count, 1024, 512)).toThrow();
    expect(() => surfaceRange("0", "1", 1025, 512)).toThrow();
    expect(() => surfaceRange("0", "1", 1024, 0)).toThrow();
  });
});

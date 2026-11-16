import { describe, expect, it } from "vitest";
import {
  defaultFormatOptions,
  effectiveFormatCluster,
  validateFormatOptions,
} from "./formatOptions";
import type { FormatOptions, WindowsFileSystem } from "./types";

describe("format option boundaries", () => {
  it("binds default NTFS compression to a real supported cluster", () => {
    const enabled: FormatOptions = {
      mode: "full",
      ntfsCompression: true,
      ntfsShortNames: true,
    };
    expect(effectiveFormatCluster("NTFS", 0, enabled)).toBe(4096);
    expect(() => validateFormatOptions("NTFS", 0, 4096, enabled)).not.toThrow();
    for (const cluster of [8192, 65536])
      expect(() =>
        validateFormatOptions("NTFS", cluster, 512, enabled),
      ).toThrow(/4 KiB/);
    expect(() => validateFormatOptions("NTFS", 512, 4096, enabled)).toThrow();
    expect(() => validateFormatOptions("NTFS", 1234, 512, enabled)).toThrow();
  });
  it("rejects NTFS-only options for every FAT family", () => {
    for (const fs of [
      "FAT12",
      "FAT16",
      "FAT32",
      "exFAT",
    ] as WindowsFileSystem[]) {
      for (const mode of ["quick", "full"] as const) {
        expect(() =>
          validateFormatOptions(fs, 0, 512, { ...defaultFormatOptions, mode }),
        ).not.toThrow();
        for (const flag of ["ntfsCompression", "ntfsShortNames"] as const)
          expect(() =>
            validateFormatOptions(fs, 0, 512, {
              ...defaultFormatOptions,
              mode,
              [flag]: true,
            }),
          ).toThrow(/仅适用于 NTFS/);
      }
    }
    expect(() =>
      validateFormatOptions("NTFS", 0, 0, defaultFormatOptions),
    ).toThrow();
    expect(effectiveFormatCluster("NTFS", 0, defaultFormatOptions)).toBe(0);
  });
});

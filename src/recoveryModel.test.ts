import { expect, it } from "vitest";
import {
  dateFilter,
  fileState,
  selectedFiles,
  sizeFilter,
} from "./recoveryModel";
import type { Candidate } from "./recoveryModel";
it("keeps selection identities across pages and same names", () => {
  const page1 = selectedFiles(new Set(), [1, 2], true);
  const page2 = selectedFiles(page1, [100, 101], true);
  expect([...selectedFiles(page2, [1, 2], false)]).toEqual([100, 101]);
  expect([...page1]).toEqual([1, 2]);
  expect(selectedFiles(page2, [1, 2], true).size).toBe(4);
});
it("rejects over-limit selection without losing previous choices", () => {
  const selected = new Set(Array.from({ length: 10000 }, (_, i) => i));
  expect(() => selectedFiles(selected, [10000], true)).toThrow("10000");
  expect(selected.size).toBe(10000);
});
it("distinguishes FAT/exFAT deletion from signature uncertainty", () => {
  for (const method of [
    "NTFS 文件表",
    "FAT12 目录",
    "FAT32 目录",
    "exFAT 目录",
  ]) {
    expect(fileState({ method, deleted: true } as Candidate)).toBe("已删除");
    expect(fileState({ method, deleted: false } as Candidate)).toBe("现存文件");
  }
  expect(fileState({ method: "文件签名", deleted: false } as Candidate)).toBe(
    "删除状态未知",
  );
});
it("validates inclusive size filters with empty and zero values", () => {
  expect(sizeFilter("", "")).toEqual({ minSize: null, maxSize: null });
  expect(sizeFilter("0", "0.5")).toEqual({ minSize: 0, maxSize: 524288 });
  for (const [min, max] of [
    ["2", "1"],
    ["-1", ""],
    ["NaN", ""],
    ["1e9", ""],
    ["9000000", ""],
  ])
    expect(() => sizeFilter(min, max)).toThrow();
});
it("validates inclusive record-date ranges without host timezone conversion", () => {
  expect(dateFilter("", "")).toEqual({ dateFrom: null, dateTo: null });
  expect(dateFilter("2024-02-29", "2024-02-29")).toEqual({
    dateFrom: "2024-02-29",
    dateTo: "2024-02-29",
  });
  expect(dateFilter("0001-01-01", "9999-12-31").dateFrom).toBe("0001-01-01");
  for (const [from, to] of [
    ["1900-02-29", ""],
    ["0000-01-01", ""],
    ["2024-01-32", ""],
    ["2025-01-01", "2024-12-31"],
    ["2024-01-01\n", ""],
  ])
    expect(() => dateFilter(from, to)).toThrow();
});

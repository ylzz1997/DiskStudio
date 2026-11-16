import { expect, it } from "vitest";
import {
  candidateRelation,
  freeSearchRanges,
  type LostCandidate,
} from "./lostPartitionModel";
import type { Partition } from "./types";
const part = (start: number, size: number) => ({ start, size }) as Partition;
it("distinguishes an existing start from overlap and unregistered extent", () => {
  const c = { offset: 4096, size: 8192 } as LostCandidate;
  expect(candidateRelation(c, [part(4096, 12288)])).toBe("现有分区起点");
  expect(candidateRelation(c, [part(8192, 8192)])).toBe("与现有分区重叠");
  expect(candidateRelation(c, [part(12288, 8192)])).toBe("未登记范围");
});
it("offers complete unallocated ranges and rejects ambiguous layouts", () => {
  expect(
    freeSearchRanges([part(16384, 8192), part(4096, 4096)], 32768, 512),
  ).toEqual([
    { start: 0, size: 4096 },
    { start: 8192, size: 8192 },
    { start: 24576, size: 8192 },
  ]);
  expect(
    freeSearchRanges([part(4096, 8192), part(8192, 8192)], 32768, 512),
  ).toEqual([]);
  expect(freeSearchRanges([part(1, 8192)], 32768, 512)).toEqual([]);
});

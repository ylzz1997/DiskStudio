import type { Partition } from "./types";
export interface LostCandidate {
  id: string;
  filesystem: string;
  offset: number;
  size: number;
  evidence: string;
  warning: string;
  canRestore: boolean;
  canBrowse: boolean;
  btrfsMemberCount: number | null;
}
export interface LostBtrfsChoice {
  session: string;
  candidate: string;
  target: string;
  layoutKey: string;
  member: string;
  startSector: number;
  size: number;
}
export interface LostSnapshot {
  id: string;
  target: string;
  layoutKey: string;
  state: "running" | "completed" | "canceled" | "failed";
  done: number;
  total: number;
  options: { startSector: number; sectorCount: number };
  candidates: LostCandidate[];
  message: string;
}
export function candidateRelation(c: LostCandidate, parts: Partition[]) {
  if (parts.some((p) => p.start === c.offset && c.size <= p.size))
    return "现有分区起点";
  if (
    parts.some(
      (p) => c.offset < p.start + p.size && p.start < c.offset + c.size,
    )
  )
    return "与现有分区重叠";
  return "未登记范围";
}
export function freeSearchRanges(
  parts: Partition[],
  size: number,
  sector: number,
) {
  if (
    ![512, 4096].includes(sector) ||
    !Number.isSafeInteger(size) ||
    size <= 0 ||
    size % sector
  )
    return [];
  const ordered = [...parts].sort((a, b) => a.start - b.start);
  let end = 0;
  const gaps: { start: number; size: number }[] = [];
  for (const p of ordered) {
    if (
      ![p.start, p.size].every(Number.isSafeInteger) ||
      p.start < end ||
      p.size <= 0 ||
      p.start % sector ||
      p.size % sector ||
      p.start + p.size > size
    )
      return [];
    if (p.start > end) gaps.push({ start: end, size: p.start - end });
    end = p.start + p.size;
  }
  if (end < size) gaps.push({ start: end, size: size - end });
  return gaps.filter((g) => g.size >= 4096);
}

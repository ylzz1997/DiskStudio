import { expect, it } from "vitest";
import {
  progressAmount,
  progressMode,
  progressMatches,
  retainedProgressMatches,
  settingsFromProgress,
  type RecoveryProgress,
} from "./recoveryProgressModel";
it("keeps NTFS record progress separate from signature byte ranges", () => {
  const p: RecoveryProgress = {
    id: "test",
    source: { target: "B", layoutKey: "layout", partition: 2 },
    engine: "ntfs",
    mode: "existing",
    options: null,
    done: 16,
    total: 32,
    found: 4,
    running: false,
    canResume: true,
    canSave: true,
    loaded: true,
  };
  expect(progressAmount(p)).toBe("16 / 32 条文件记录");
  expect(progressMode(p)).toBe("现存文件");
  expect(() => settingsFromProgress(p)).toThrow();
  expect(progressAmount({ ...p, engine: "signatures" })).not.toContain("记录");
});

it("shows retained progress only for the current source session and scan", () => {
  const progress: RecoveryProgress = {
    id: "paused-image-scan",
    source: { kind: "image", id: "opened-image", partition: 0 },
    engine: "signatures",
    mode: "signatures",
    options: { kinds: ["pdf"], start: 0, length: null, freeOnly: true },
    done: 4096,
    total: 8192,
    found: 1,
    running: false,
    canResume: true,
    canSave: true,
    loaded: true,
  };
  const current = { id: "opened-image", scanId: "paused-image-scan" };
  expect(retainedProgressMatches(progress, "image", current)).toBe(true);
  expect(retainedProgressMatches(progress, "backup", current)).toBe(false);
  expect(retainedProgressMatches(progress, "image", { ...current, id: "reopened-image" })).toBe(false);
  expect(retainedProgressMatches(progress, "image", { ...current, scanId: null })).toBe(false);
  expect(progressMatches(progress, null, null, 0)).toBe(false);
});
it("shows FAT, exFAT and Ext directory records without inventing a full-volume percentage", () => {
  for (const engine of ["fat", "exfat", "ext"] as const) {
    const p: RecoveryProgress = {
      id: "fat",
      source: { target: "B", layoutKey: "layout", partition: 2 },
      engine,
      mode: "allFiles",
      options: null,
      done: 32,
      total: 0,
      found: 4,
      running: false,
      canResume: true,
      canSave: true,
      loaded: true,
    };
    expect(progressAmount(p)).toBe("已检查 32 条目录记录");
    expect(progressAmount({ ...p, total: 32, canResume: false })).toBe(
      "32 / 32 条目录记录",
    );
    expect(() => settingsFromProgress(p)).toThrow();
  }
});

import { describe, expect, it } from "vitest";
import { advanceQuickProgress } from "./quickProgress";
import type { QuickProgress } from "./quickProgress";

const initial: QuickProgress = {
  phase: "checking",
  completed: 0,
  total: 3,
  current: null,
};
const running: QuickProgress = { ...initial, phase: "running", current: 0 };
const verified: QuickProgress = { ...initial, phase: "verified", completed: 1 };

describe("invocation-scoped batch progress", () => {
  it("counts only completed postcondition checks, not dispatched steps", () => {
    const first = advanceQuickProgress(initial, running);
    expect(first.completed).toBe(0);
    const checked = advanceQuickProgress(first, verified);
    expect(checked.completed).toBe(1);
    expect(
      advanceQuickProgress(checked, { ...running, current: 1, completed: 1 })
        .completed,
    ).toBe(1);
  });
  it("ignores stale, duplicate, skipped, malformed and other-sized run events", () => {
    for (const event of [
      initial,
      running,
      { ...running, total: 4 },
      { ...running, current: 2, completed: 1 },
      { ...running, current: 2, completed: 2 },
      { ...verified, completed: 2 },
      { ...verified, completed: NaN },
      { ...verified, completed: 4 },
      { ...verified, completed: -1 },
    ])
      expect(advanceQuickProgress(verified, event)).toBe(verified);
    expect(advanceQuickProgress(initial, verified)).toBe(initial);
    expect(advanceQuickProgress(running, { ...verified, current: 0 })).toBe(
      running,
    );
  });
  it("keeps the failed step unverified and rejects every subsequent progress event", () => {
    const stopped: QuickProgress = { ...running, phase: "stopped" };
    expect(advanceQuickProgress(running, stopped)).toEqual(stopped);
    for (const event of [
      initial,
      running,
      verified,
      { ...verified, completed: 3 },
    ]) {
      expect(advanceQuickProgress(stopped, event)).toBe(stopped);
    }
    expect(stopped.completed).toBe(0);
  });
  it("retains final step verification while waiting for the authoritative result", () => {
    let state = initial;
    for (let step = 0; step < 3; step++) {
      state = advanceQuickProgress(state, {
        ...running,
        completed: step,
        current: step,
      });
      state = advanceQuickProgress(state, { ...verified, completed: step + 1 });
    }
    expect(state).toEqual({ ...verified, completed: 3 });
    expect(
      advanceQuickProgress(state, { ...running, completed: 3, current: 3 }),
    ).toBe(state);
    expect(advanceQuickProgress(state, running)).toBe(state);
  });
});

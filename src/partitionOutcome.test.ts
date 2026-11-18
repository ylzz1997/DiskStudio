import { expect, it, vi } from "vitest";
import { batchResultLines, runPartitionOperation } from "./partitionOutcome";
import { translate } from "./i18n/translate";
import type { PartitionOutcome } from "./partitionOutcome";
import type { PartitionPlan } from "./types";

const plan: PartitionPlan = {
  token: "single-use",
  title: "管理盘符",
  targetName: "memory target",
  serial: "MEMORY",
  partition: 7,
  beforeSize: null,
  afterSize: null,
  warning: "Z → 无",
  expiresAt: 10,
};

it("renders verified, stopped and skipped steps from the terminal batch result", () => {
  const steps = ["Copy", "Move", "Resize"];
  const t: Parameters<typeof batchResultLines>[2] = (key, fields) =>
    translate("en", key, fields);
  expect(
    batchResultLines(
      {
        completed: 1,
        verified: false,
        failure: "Windows error 5",
        message: "原始批次记录",
      },
      steps,
      t,
      (text) => text,
    ),
  ).toEqual([
    "1. Completed and verified · Copy",
    "2. Stopped. Result pending verification. · Move\nWindows error 5",
    "3. Skipped · Resize",
  ]);
});

it("publishes the bound result before a refresh that removes the target", async () => {
  let owner: PartitionOutcome | null = null;
  let targetPresent = true;
  const execute = vi.fn(async () => ({ verified: true, message: "verified" }));
  const refresh = vi.fn(async () => {
    expect(owner?.status).toBe("verified");
    targetPresent = false;
  });
  const result = await runPartitionOperation(
    plan,
    execute,
    refresh,
    (value) => {
      owner = value;
    },
  );
  expect(targetPresent).toBe(false);
  expect(owner).toBe(result);
  expect(result.plan).toMatchObject({
    serial: "MEMORY",
    partition: 7,
    warning: "Z → 无",
  });
  expect(result.plan).not.toHaveProperty("token");
  expect(execute.mock.calls).toEqual([["single-use"]]);
  expect(refresh).toHaveBeenCalledOnce();
});

it.each([true, false])(
  "keeps backend verification %s separate from refresh failure",
  async (verified) => {
    const outcomes: PartitionOutcome[] = [];
    const execute = vi.fn(async () => ({
      verified,
      message: "backend result",
    }));
    const result = await runPartitionOperation(
      plan,
      execute,
      async () => {
        throw new Error("provider unavailable");
      },
      (v) => outcomes.push(v),
    );
    expect(result.status).toBe(verified ? "verified" : "unverified");
    expect(result.message).toBe("backend result");
    expect(result.refreshError).toContain("provider unavailable");
    expect(outcomes).toHaveLength(2);
    expect(outcomes[0].refreshError).toBe("");
    expect(execute).toHaveBeenCalledOnce();
  },
);

it("retains mutation failure and refresh diagnostics without resubmitting", async () => {
  const execute = vi.fn(async () => {
    throw new Error("AddAccessPath: 5");
  });
  const result = await runPartitionOperation(
    plan,
    execute,
    async () => {
      throw "metadata offline";
    },
    () => {},
  );
  expect(result).toMatchObject({
    status: "failed",
    message: "AddAccessPath: 5",
    refreshError: "metadata offline",
  });
  expect(execute).toHaveBeenCalledOnce();
});

it("does not report a result or refresh before the submitted operation settles", async () => {
  let finish!: (value: { verified: boolean; message: string }) => void;
  const execute = vi.fn(
    () =>
      new Promise<{ verified: boolean; message: string }>((resolve) => {
        finish = resolve;
      }),
  );
  const publish = vi.fn();
  const refresh = vi.fn(async () => {});
  const pending = runPartitionOperation(plan, execute, refresh, publish);
  expect(publish).not.toHaveBeenCalled();
  expect(refresh).not.toHaveBeenCalled();
  finish({ verified: false, message: "partial result" });
  await pending;
  expect(publish).toHaveBeenCalledOnce();
  expect(refresh).toHaveBeenCalledOnce();
});

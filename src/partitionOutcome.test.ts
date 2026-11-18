import { expect, it, vi } from "vitest";
import { runPartitionOperation } from "./partitionOutcome";
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

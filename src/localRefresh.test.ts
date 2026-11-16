import { expect, it, vi } from "vitest";
import { readLocalState } from "./localRefresh";
import type { DiskSnapshot, PartitionStatus } from "./types";

const snapshot: DiskSnapshot = { disks: [], observedAt: 0, warnings: [] };
const access: PartitionStatus = { enabled: true, reason: null, targets: [] };
it("returns a successful empty inventory so vanished targets can be removed", async () => {
  const disks = vi.fn(async () => snapshot);
  const capabilities = vi.fn(async () => access);
  expect(await readLocalState(disks, capabilities)).toEqual({
    snapshot,
    access,
    error: "",
  });
  expect(disks.mock.calls).toEqual([[]]);
  expect(capabilities.mock.calls).toEqual([[]]);
});
it("retains inventory but disables operations and reports a capability-query failure", async () => {
  const result = await readLocalState(
    async () => snapshot,
    async () => {
      throw new Error("capabilities failed");
    },
  );
  expect(result).toEqual({
    snapshot,
    access: null,
    error: "操作能力：capabilities failed",
  });
});
it("does not expose capabilities for a failed inventory and retains both errors", async () => {
  const failed = async () => {
    throw "inventory failed";
  };
  expect(await readLocalState(failed, async () => access)).toEqual({
    snapshot: null,
    access: null,
    error: "磁盘信息：inventory failed",
  });
  expect(
    await readLocalState(failed, async () => {
      throw "capabilities failed";
    }),
  ).toEqual({
    snapshot: null,
    access: null,
    error: "磁盘信息：inventory failed；操作能力：capabilities failed",
  });
});

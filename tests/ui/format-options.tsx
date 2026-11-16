// Development-only production component. Every callback is memory-only.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { PartitionActions } from "../../src/PartitionActions";
import { PartitionOutcomeDialog } from "../../src/PartitionOutcomeDialog";
import type { PartitionOutcome } from "../../src/partitionOutcome";
import type { Disk, PartitionTarget } from "../../src/types";
import "../../src/styles.css";
const disk: Disk = {
  id: "memory",
  name: "内存界面检查",
  filename: "",
  size: 16 * 1024 ** 3,
  sectorSize: 512,
  scheme: "GPT",
  kind: "physical",
  partitions: [
    {
      id: "p1",
      number: 1,
      name: "DATA",
      fs: "NTFS",
      start: 1048576,
      size: 128 * 1024 ** 2,
      used: null,
      color: "blue",
      system: false,
    },
  ],
};
const target: PartitionTarget = {
  target: "a".repeat(64),
  diskId: "memory",
  name: "内存界面检查",
  serial: "MEMORY-ONLY",
  revision: "memory",
  layoutKey: "memory",
  freeRanges: [],
  formatPartitions: [1],
  deletePartitions: [1],
  resizePartitions: [1],
  extPartitions: [],
  metadataPartitions: [1],
  availableDriveLetters: [],
  available: true,
  reason: null,
};
function App() {
  const [events, setEvents] = useState<string[]>([]);
  const [dark, setDark] = useState(false);
  const [result, setResult] = useState("verified");
  const [refreshFailure, setRefreshFailure] = useState(false);
  const [removeTarget, setRemoveTarget] = useState(false);
  const [present, setPresent] = useState(true);
  const [busy, setBusy] = useState(false);
  const [outcome, setOutcome] = useState<PartitionOutcome | null>(null);
  return (
    <div
      data-theme={dark ? "dark" : "light"}
      style={{ minHeight: "100vh", padding: 24 }}
    >
      <button
        onClick={() => {
          setDark(!dark);
          document.documentElement.dataset.theme = !dark ? "dark" : "light";
        }}
      >
        切换主题
      </button>
      <label>
        内存返回结果
        <select value={result} onChange={(e) => setResult(e.target.value)}>
          <option value="verified">已核对</option>
          <option value="unverified">未核对完整</option>
          <option value="failed">执行异常</option>
        </select>
      </label>
      <label>
        <input
          type="checkbox"
          checked={refreshFailure}
          onChange={(e) => setRefreshFailure(e.target.checked)}
        />
        内存刷新异常
      </label>
      <label>
        <input
          type="checkbox"
          checked={removeTarget}
          onChange={(e) => setRemoveTarget(e.target.checked)}
        />
        刷新后目标消失
      </label>
      {!present && (
        <button onClick={() => setPresent(true)}>恢复内存目标</button>
      )}
      {present && (
        <PartitionActions
          disk={disk}
          partition={disk.partitions[0]}
          target={target}
          unavailableReason=""
          onBusy={setBusy}
          onRefresh={async () => {
            if (removeTarget) setPresent(false);
            if (refreshFailure) throw new Error("内存刷新失败");
          }}
          onOutcome={setOutcome}
          service={{
            preparePartition: async (_target, action) => {
              setEvents((old) => [...old, JSON.stringify(action)]);
              return {
                token: "memory-token",
                title: "格式化为 NTFS",
                targetName: disk.name,
                serial: target.serial,
                partition: 1,
                beforeSize: disk.partitions[0].size,
                afterSize: disk.partitions[0].size,
                warning: "内存回调：检查确认布局及所选参数，没有磁盘访问。",
                expiresAt: Date.now() + 300000,
              };
            },
            executePartition: async (token) => {
              setEvents((old) => [...old, token]);
              if (result === "failed") throw new Error("内存执行异常，不重试");
              return {
                verified: result === "verified",
                message:
                  result === "verified"
                    ? "内存确认回调完成"
                    : "请求已提交，内存后置检查未完成；请检查实际状态。",
              };
            },
          }}
        />
      )}
      {outcome && (
        <PartitionOutcomeDialog
          outcome={outcome}
          busy={busy}
          onClose={() => setOutcome(null)}
        />
      )}
      <pre data-testid="events">{events.join("\n")}</pre>
    </div>
  );
}
createRoot(document.getElementById("root")!).render(<App />);

// Memory-only UI harness; no Tauri, files, devices or native dialogs.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import {
  BackupBrowserPanel,
  type BackupInfo,
} from "../../src/BackupBrowserPanel";
import "../../src/styles.css";
const backup: BackupInfo = {
  id: "memory-backup",
  name: "工作资料-2026-11-12.dsbackup",
  version: 2,
  kind: "disk",
  size: 64 * 1024 ** 3,
  sector: 512,
  layers: 3,
  points: [
    { id: "latest", label: "最新增量备份" },
    { id: "middle", label: "此前第 1 层增量备份", ordinal: 1 },
    { id: "base", label: "最早完整备份" },
  ],
  selectedPoint: "latest",
  partitions: [
    { id: 0, name: "工作资料", start: 1024 ** 2, size: 40 * 1024 ** 3 },
    { id: 1, name: "照片归档", start: 41 * 1024 ** 3, size: 20 * 1024 ** 3 },
  ],
  warning: "",
  sourceDisks: [],
  scanId: null,
  scanPartition: null,
  scanMode: null,
  search: null,
};
function Harness() {
  const [info, setInfo] = useState<BackupInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [event, setEvent] = useState("未打开");
  return (
    <main
      className="data-page"
      style={{ padding: 32, maxWidth: 1180, margin: "auto" }}
    >
      <h1>备份与克隆</h1>
      <p>UI 验证 · 数据全部在内存中</p>
      <BackupBrowserPanel
        info={info}
        busy={busy}
        available
        onOpen={() => {
          setInfo(backup);
          setEvent("已打开内存备份");
        }}
        onClose={() => {
          setInfo(null);
          setEvent("已关闭");
        }}
        onRead={(id) => {
          setInfo({ ...info!, scanPartition: id, scanId: "memory-scan" });
          setEvent(`读取分区 ${id}：${backup.partitions[id].name}`);
        }}
        onReconstruct={(id) => setEvent(`重建分区 ${id}`)}
        onPoint={(point) => {
          setInfo({
            ...info!,
            selectedPoint: point,
            scanId: null,
            scanPartition: null,
          });
          setEvent(`已切换还原点 ${point}`);
        }}
      />
      <p role="status">{event}</p>
      <button className="button" onClick={() => setBusy(!busy)}>
        切换忙碌状态
      </button>
      <button
        className="button"
        onClick={() =>
          setInfo({ ...backup, partitions: [], name: "空磁盘.dsbackup" })
        }
      >
        空分区列表
      </button>
      <button
        className="button"
        onClick={() =>
          (document.documentElement.dataset.theme =
            document.documentElement.dataset.theme === "dark"
              ? "light"
              : "dark")
        }
      >
        切换主题
      </button>
    </main>
  );
}
createRoot(document.getElementById("root")!).render(<Harness />);

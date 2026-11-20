// Production components with memory-only callbacks. Not a shipped route.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { PartitionBrowserPanel } from "../../src/PartitionBrowserPanel";
import {
  RecoveryResults,
  type RecoveryResultsService,
} from "../../src/RecoveryResults";
import type { Candidate, RecoveryFilter } from "../../src/recoveryModel";
import type { Partition } from "../../src/types";
import "../../src/styles.css";
const partition: Partition = {
  id: "memory-p1",
  number: 1,
  name: "工作资料",
  fs: "NTFS",
  start: 1048576,
  size: 16 * 1024 ** 3,
  used: null,
  color: "blue",
  system: false,
};
const dirs = [
  { id: "root", parent: null, name: "根目录", deleted: false },
  { id: "work", parent: "root", name: "项目文档", deleted: false },
  { id: "empty", parent: "root", name: "空目录", deleted: false },
];
const files: Candidate[] = [
  "说明.txt",
  "设计稿.png",
  "加密资料.txt",
  "需求.docx",
  "日程.pdf",
].map((name, id) => ({
  id,
  name,
  size: 1024 * (id + 1),
  deleted: false,
  method: id === 1 ? "FAT 目录" : id === 3 ? "Ext inode" : "NTFS 文件表",
  supported: id !== 2,
  directory: id < 3 ? "root" : "work",
  directoryNote: "",
  note: id === 2 ? "EFS 加密数据暂不支持" : "现存文件，源内容可能变化",
  metadata: {
    source: id === 1 ? "FAT 目录项" : id === 3 ? "Ext inode" : "NTFS 标准信息",
    created:
      id === 2 || id === 3
        ? null
        : {
            text:
              id === 1
                ? "2026-11-12 09:24:16.12"
                : "2026-11-12 09:24:16.1234567",
            zone: id === 1 ? "本地时间（时区未知）" : "UTC",
            precision: id === 1 ? "10 ms" : "100 ns",
          },
    modified:
      id === 2
        ? null
        : {
            text: "2026-11-12 10:30:20",
            zone: id === 1 ? "本地时间（时区未知）" : "UTC",
            precision: id === 1 ? "2 s" : "100 ns",
          },
    accessed:
      id === 1
        ? {
            text: "2026-11-12",
            zone: "本地时间（时区未知）",
            precision: "1 天",
          }
        : null,
    changed: null,
    attributes: id === 2 ? ["加密"] : id === 3 ? ["不可变"] : ["只读", "归档"],
    rawFlags: id === 2 ? "0x00004000" : id === 3 ? "0x00000010" : "0x00000021",
    permissions: id === 3 ? "0640" : null,
    uid: id === 3 ? 1000 : null,
    gid: id === 3 ? 1000 : null,
    links: id === 3 ? 2 : null,
  },
}));
function matches(query: string, filter: RecoveryFilter) {
  return files.filter(
    (c) =>
      c.name.includes(query) &&
      (!filter.folder ||
        c.directory === filter.folder ||
        (filter.folder === "root" && filter.descendants)) &&
      (!filter.supportedOnly || c.supported) &&
      (filter.state === "all" || filter.state === "existing"),
  );
}
const service: RecoveryResultsService = {
  results: async (scanId, offset, query, filter) => {
    const all = matches(query, filter);
    return {
      scanId,
      offset,
      source: "内存 UI 验证 · 工作资料",
      total: all.length,
      scannedTotal: files.length,
      paths: { root: ["根目录"], work: ["根目录", "项目文档"] },
      items: all.slice(offset, offset + 100),
    };
  },
  folders: async (_id, folder, offset) => ({
    canExportDirectory: folder !== "unknown",
    offset,
    total: folder === "root" || folder === null ? 2 : 0,
    path: dirs.filter((d) => d.id === "root" || d.id === folder),
    unknownFiles: 0,
    items: dirs
      .filter((d) => d.parent === (folder ?? "root"))
      .map((d) => ({
        ...d,
        files: files.filter((c) => c.directory === d.id).length,
      })),
  }),
  select: async (_id, query, filter) =>
    matches(query, filter)
      .filter((c) => c.supported)
      .map((c) => c.id),
  exportReport: async () => "内存目录清单（未写文件）",
};
function Harness() {
  const [loaded, setLoaded] = useState(0),
    [busy, setBusy] = useState(false),
    [missing, setMissing] = useState(false),
    [event, setEvent] = useState("未读取");
  return (
    <main
      className="data-page"
      style={{ padding: 28, maxWidth: 1250, margin: "auto" }}
    >
      <h1>浏览文件</h1>
      <p>内存 UI 验证</p>
      <PartitionBrowserPanel
        partition={missing ? undefined : partition}
        available
        busy={busy}
        loaded={loaded > 0}
        onBack={() => {
          setLoaded(0);
          setEvent("返回布局回调");
        }}
        onRead={() => {
          setLoaded(loaded + 1);
          setEvent("读取目录回调");
        }}
        onReconstruct={() => setEvent("重建 NTFS 回调")}
      />
      {loaded > 0 && (
        <RecoveryResults
          key={loaded}
          scanId="memory-files"
          revision="done"
          browsing
          busy={busy}
          running={busy}
          service={service}
          onPreview={(c) => setEvent(`预览回调：${c.name}`)}
          onRecover={(ids, layout, folder) =>
            setEvent(
              folder
                ? `目录复制回调：${folder} · ${layout}`
                : `复制回调：${ids.length} 个文件 · ${layout}`,
            )
          }
        />
      )}
      <p role="status">{event}</p>
      <button className="button" onClick={() => setBusy(!busy)}>
        切换忙碌状态
      </button>
      <button className="button" onClick={() => setMissing(!missing)}>
        切换分区缺失
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
document.body.style.overflow = "auto";
document.body.style.height = "auto";
createRoot(document.getElementById("root")!).render(<Harness />);

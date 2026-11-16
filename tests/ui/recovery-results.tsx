// UI-only harness, excluded from the production entry. All data is memory-only.
// Imports the shipped component; never calls Tauri or a device/file API.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { RecoveryResults } from "../../src/RecoveryResults";
import type { RecoveryResultsService } from "../../src/RecoveryResults";
import type {
  Candidate,
  RecoveryDirectory,
  RecoveryFilter,
} from "../../src/recoveryModel";
import "../../src/styles.css";
// Evaluate only the shipped pure offline query helper, in this development harness.
// The production entry never imports this file or uses dynamic evaluation.
import querySource from "../../src-tauri/src/directory_query.js?raw";
const queryEngine = new Function(`${querySource}\nreturn directoryQuery;`)();

const dirs: RecoveryDirectory[] = [
  { id: "root", parent: null, name: "根目录", deleted: false },
  {
    id: "work",
    parent: "root",
    name: "工作资料",
    deleted: false,
    metadata: {
      source: "NTFS 标准信息",
      created: {
        text: "2024-01-02 03:04:05",
        zone: "UTC",
        precision: "100 ns",
      },
      modified: {
        text: "2025-06-10 08:30:00",
        zone: "UTC",
        precision: "100 ns",
      },
      accessed: null,
      changed: null,
      attributes: ["目录", "存档"],
      rawFlags: "0x00000030",
      permissions: null,
      uid: null,
      gid: null,
      links: null,
    },
  },
  { id: "photos", parent: "root", name: "旅行照片", deleted: false },
  { id: "reports", parent: "work", name: "年度报告", deleted: true },
  { id: "empty", parent: "root", name: "空目录", deleted: false },
];
const files: Candidate[] = Array.from({ length: 205 }, (_, id) => ({
  id,
  name: id % 3 === 0 ? `项目报告-${id}.pdf` : `旅行照片-${id}.jpg`,
  size: (id + 1) * 1024 ** 2,
  deleted: id % 2 === 0,
  method:
    id % 5 === 0 ? "文件签名" : id % 3 === 0 ? "FAT32 目录" : "NTFS 文件表",
  note:
    id % 17 === 0
      ? "压缩或 EFS 加密数据暂不支持"
      : "可能已被覆盖；请先预览并核对恢复内容",
  supported: id % 17 !== 0,
  directory: id % 5 === 0 ? null : ["work", "photos", "reports"][id % 3],
  directoryNote: id % 5 === 0 ? "签名扫描无法确定原目录" : "",
  metadata:
    id % 4 === 0
      ? undefined
      : {
          source: "NTFS 标准信息",
          created: null,
          accessed: null,
          changed: null,
          modified: {
            text: `2024-02-${id % 2 === 0 ? "28" : "29"} 08:30:00.1234567`,
            zone: id % 3 === 0 ? "本地时间（时区未知）" : "UTC",
            precision: "100 ns",
          },
          attributes: [],
          rawFlags: null,
          permissions: null,
          uid: null,
          gid: null,
          links: null,
        },
}));
files[1] = {
  ...files[1],
  name: "会议记录.txt",
  directory: "work",
  note: "现存文件；恢复时源文件可能变化",
  directoryNote: "同一文件有多个名称；选中后分别保存副本",
};
files[2] = {
  ...files[2],
  name: "会议记录副本.txt",
  size: files[1].size,
  deleted: false,
  method: "NTFS 文件表",
  directory: "photos",
  note: files[1].note,
  directoryNote: files[1].directoryNote,
};
function path(id: string | null): RecoveryDirectory[] {
  const dir = dirs.find((d) => d.id === id);
  return dir ? [...path(dir.parent), dir] : [];
}
function matches(query: string, filter: RecoveryFilter) {
  const prepared = queryEngine.prepare(query, filter);
  return prepared.ordered(
    files.filter((c) => {
      const state =
        c.method === "文件签名"
          ? "unknown"
          : c.deleted
            ? "deleted"
            : "existing";
      const kind = c.name.endsWith("pdf") ? "documents" : "images";
      return (
        (!filter.folder ||
          (filter.folder === "unknown"
            ? !c.directory
            : filter.descendants
              ? path(c.directory).some((d) => d.id === filter.folder)
              : c.directory === filter.folder)) &&
        prepared.matches(c) &&
        (filter.state === "all" || filter.state === state) &&
        (filter.kind === "all" || filter.kind === kind) &&
        (filter.minSize === null || c.size >= filter.minSize) &&
        (filter.maxSize === null || c.size <= filter.maxSize) &&
        (!filter.supportedOnly || c.supported)
      );
    }),
  ) as Candidate[];
}
let reportOutcome = "success";
const service: RecoveryResultsService = {
  exportReport: async () => {
    if (reportOutcome === "cancel") return null;
    if (reportOutcome === "error")
      throw Error("测试：输出目录不可用，未写入文件。");
    return "内存测试（未写入文件）.html";
  },
  results: async (scanId, offset, query, filter) => {
    const all = matches(query, filter);
    offset = Math.min(
      offset,
      Math.floor(Math.max(0, all.length - 1) / 100) * 100,
    );
    return {
      scanId,
      offset,
      source: "UI 验证 · 内存测试数据",
      total: all.length,
      scannedTotal: files.length,
      items: all.slice(offset, offset + 100),
      paths: Object.fromEntries(
        dirs.map((d) => [d.id, path(d.id).map((p) => p.name)]),
      ),
    };
  },
  folders: async (_scanId, folder, offset) => {
    const all = dirs.filter((d) => d.parent === (folder ?? "root"));
    return {
      path: path(folder ?? "root"),
      canExportDirectory: folder !== "unknown",
      offset,
      total: all.length,
      unknownFiles: 41,
      items: all.map((d) => ({
        ...d,
        files: files.filter((c) => path(c.directory).some((p) => p.id === d.id))
          .length,
      })),
    };
  },
  select: async (_scanId, query, filter) =>
    matches(query, filter)
      .filter((c) => c.supported)
      .map((c) => c.id),
};
function Harness() {
  const [message, setMessage] = useState("尚未触发回调");
  const [dark, setDark] = useState(false);
  const [running, setRunning] = useState(false);
  return (
    <main className="data-page" style={{ padding: 28 }}>
      <button
        className="button"
        onClick={() => {
          setDark(!dark);
          document.documentElement.dataset.theme = dark ? "light" : "dark";
        }}
      >
        切换明暗
      </button>
      <label style={{ marginLeft: 16 }}>
        模拟保存结果
        <select
          aria-label="模拟保存结果"
          onChange={(e) => {
            reportOutcome = e.target.value;
          }}
        >
          <option value="success">成功</option>
          <option value="cancel">取消</option>
          <option value="error">失败</option>
        </select>
      </label>
      <label style={{ marginLeft: 16 }}>
        <input
          type="checkbox"
          checked={running}
          onChange={(e) => setRunning(e.target.checked)}
        />
        模拟任务运行中
      </label>
      <output style={{ marginLeft: 20 }} aria-label="测试回调">
        {message}
      </output>
      <RecoveryResults
        scanId="memory-ui-check"
        revision="complete"
        busy={running}
        running={running}
        service={service}
        onRecover={(ids, layout, folder, options) =>
          setMessage(
            (folder
              ? `目录恢复回调：${folder} · ${layout}`
              : `选择回调：${ids.length} 个文件 · ${layout === "tree" ? "原目录结构" : "同一目录"}`) +
              ` · 并发 ${options.workers} · 时间 ${options.timestamps ? "保留" : "关闭"} · 属性 ${options.attributes ? "保留" : "关闭"}`,
          )
        }
        onPreview={(c) => setMessage(`预览回调：${c.name}`)}
      />
    </main>
  );
}
document.body.style.overflow = "auto";
document.body.style.height = "auto";
document.body.style.background = "var(--bg)";
const root = createRoot(document.getElementById("root")!);
root.render(<Harness />);
if (import.meta.hot) import.meta.hot.dispose(() => root.unmount());

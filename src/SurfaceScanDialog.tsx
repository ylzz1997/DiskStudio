import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Download,
  ScanLine,
  Pause,
  Play,
  FolderOpen,
  Save,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import type { Disk } from "./types";
import { formatSize } from "./types";
import {
  surfaceRange,
  surfaceCanResume,
  type SurfaceSnapshot,
} from "./surfaceModel";

const labels = [
  "< 10 ms",
  "10–49 ms",
  "50–149 ms",
  "150–499 ms",
  "≥ 500 ms",
  "读取失败",
];
export function SurfaceScanDialog({
  disk,
  onClose,
  onBusy,
}: {
  disk: Disk;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
}) {
  const [partition, setPartition] = useState("");
  const [start, setStart] = useState("0");
  const [count, setCount] = useState(String(disk.size / disk.sectorSize));
  const [snapshot, setSnapshot] = useState<SurfaceSnapshot | null>(null);
  const [pending, setPending] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");
  const control = useRef({ pending: false, id: "", busy: false, onBusy });
  control.current.onBusy = onBusy;
  const report = snapshot?.report;
  const running = report?.state === "running";
  const busy = pending || running;
  const resumable = surfaceCanResume(snapshot);
  const selected = disk.partitions.find((p) => p.number === Number(partition));
  const size = partition ? (selected?.size ?? 0) : disk.size;
  function setBusy(value: boolean) {
    control.current.busy = value;
    control.current.onBusy(value);
  }
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const next = await invoke<SurfaceSnapshot | null>("surface_status");
        if (!alive) return;
        if (
          !control.current.pending &&
          next &&
          next.target === disk.hardware?.identity &&
          (!control.current.id || next.id === control.current.id)
        ) {
          setSnapshot(next);
          if (!control.current.id) {
            control.current.id = next.id;
            restoreOptions(next);
          }
          setBusy(next.report.state === "running");
        }
      } catch (e) {
        if (alive) setError(String(e));
      }
      if (alive) timer = setTimeout(() => void poll(), 400);
    }
    void poll();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [disk.hardware?.identity]);
  function restoreOptions(next: SurfaceSnapshot) {
    setPartition(next.partition === null ? "" : String(next.partition));
    setStart(String(next.options.startSector));
    setCount(String(next.options.sectorCount));
  }
  function choose(value: string) {
    setPartition(value);
    setStart("0");
    setCount(
      String(
        (value
          ? disk.partitions.find((p) => p.number === Number(value))!.size
          : disk.size) / disk.sectorSize,
      ),
    );
  }
  async function begin(resume = false) {
    if (control.current.busy) return;
    setError("");
    setSaved("");
    try {
      const options = resume
        ? snapshot!.options
        : surfaceRange(start, count, size, disk.sectorSize);
      if (!disk.hardware?.identity || !disk.hardware.layoutKey)
        throw Error("请刷新磁盘信息后重试。");
      control.current.pending = true;
      setPending(true);
      setBusy(true);
      setStopping(false);
      control.current.id = resume
        ? await invoke<string>("surface_resume", { id: snapshot!.id })
        : await invoke<string>("surface_start", {
            request: {
              source: {
                target: disk.hardware.identity,
                layoutKey: disk.hardware.layoutKey,
                partition: partition ? Number(partition) : null,
              },
              options,
            },
          });
      const next = await invoke<SurfaceSnapshot | null>("surface_status");
      setSnapshot(next);
      if (resume && next) restoreOptions(next);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    } finally {
      control.current.pending = false;
      setPending(false);
    }
  }
  async function cancel() {
    if (!snapshot) return;
    try {
      await invoke("surface_pause", { id: snapshot.id });
      setStopping(true);
    } catch (e) {
      setError(String(e));
    }
  }
  async function save(progressFile = false) {
    if (!snapshot || control.current.busy) return;
    control.current.pending = true;
    setPending(true);
    setBusy(true);
    setError("");
    try {
      const path = await invoke<string | null>(
        progressFile ? "surface_save_progress" : "surface_export",
        {
          id: snapshot.id,
        },
      );
      if (path) setSaved(`${progressFile ? "进度" : "报告"}已保存：${path}`);
    } catch (e) {
      setError(String(e));
    } finally {
      control.current.pending = false;
      setPending(false);
      setBusy(false);
    }
  }
  async function load() {
    if (control.current.busy || !disk.hardware?.identity) return;
    control.current.pending = true;
    setPending(true);
    setBusy(true);
    setError("");
    setSaved("");
    try {
      const next = await invoke<SurfaceSnapshot | null>(
        "surface_load_progress",
        { target: disk.hardware.identity },
      );
      if (next) {
        control.current.id = next.id;
        setSnapshot(next);
        restoreOptions(next);
        setStopping(false);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      control.current.pending = false;
      setPending(false);
      setBusy(false);
    }
  }
  const progress = report
    ? Math.min(100, (100 * report.checkedBytes) / report.size)
    : 0;
  return (
    <Dialog
      wide
      title="坏道检测"
      subtitle={`${disk.name} · ${formatSize(disk.size)} · ${disk.hardware?.model ?? ""}`}
      onClose={() => {
        if (!control.current.busy) onClose();
      }}
    >
      <div className="surface-options">
        <label>
          检测范围
          <select
            value={partition}
            disabled={busy}
            onChange={(e) => choose(e.target.value)}
          >
            <option value="">整个磁盘</option>
            {disk.partitions
              .filter((p) => p.number !== undefined)
              .map((p) => (
                <option key={p.id} value={p.number}>
                  分区 {p.number} · {p.driveLetter ? `${p.driveLetter}: ` : ""}
                  {p.name} · {formatSize(p.size)}
                </option>
              ))}
          </select>
        </label>
        <label>
          起始扇区
          <input
            inputMode="numeric"
            value={start}
            disabled={busy}
            onChange={(e) => setStart(e.target.value)}
          />
        </label>
        <label>
          扇区数量
          <input
            inputMode="numeric"
            value={count}
            disabled={busy}
            onChange={(e) => setCount(e.target.value)}
          />
        </label>
      </div>
      <p className="surface-note">
        范围内相对扇区 · 每扇区 {disk.sectorSize} B · 每次读取最多 1 MiB
      </p>
      <div className="surface-progress-heading">
        <strong>
          {running
            ? stopping
              ? "正在暂停，等待当前读取返回…"
              : "正在检测"
            : report?.state === "completed"
              ? "检测完成"
              : report?.state === "paused"
                ? "检测已暂停"
                : report?.state === "canceled"
                  ? "检测已取消"
                  : report?.state === "failed"
                    ? "检测已停止"
                    : "准备检测"}
        </strong>
        <span>{progress.toFixed(1)}%</span>
      </div>
      <progress
        className="surface-progress"
        max={100}
        value={progress}
        aria-label="检测进度"
      />
      <div className="surface-grid" aria-label="读取耗时分布">
        {(
          report?.cells ??
          Array.from({ length: 256 }, () => ({
            checkedBytes: 0,
            failedBytes: 0,
            worst: 0,
          }))
        ).map((cell, i) => (
          <span
            key={i}
            className={`surface-cell surface-level-${cell.checkedBytes ? cell.worst : "unknown"}`}
            title={
              cell.checkedBytes
                ? `已检测 ${formatSize(cell.checkedBytes)} · 最慢 ${labels[cell.worst]}${cell.failedBytes ? ` · 失败块 ${formatSize(cell.failedBytes)}` : ""}`
                : "未检测"
            }
          />
        ))}
      </div>
      <div className="surface-legend">
        {labels.map((label, i) => (
          <span key={label}>
            <i className={`surface-level-${i}`} />
            {label}
            <b>{report?.latencyBlocks[i] ?? 0}</b>
          </span>
        ))}
      </div>
      <div className="surface-totals">
        <div>
          <span>已检测</span>
          <strong>{formatSize(report?.checkedBytes ?? 0)}</strong>
        </div>
        <div>
          <span>失败块范围</span>
          <strong>{formatSize(report?.failedBytes ?? 0)}</strong>
        </div>
        <div>
          <span>平均读取速度</span>
          <strong>
            {report?.elapsedMs
              ? `${formatSize(report.checkedBytes / (report.elapsedMs / 1000))}/s`
              : "—"}
          </strong>
        </div>
      </div>
      {snapshot && (
        <p className="surface-note surface-source">
          {snapshot.source} · 绝对起始 LBA {report!.offset / report!.sectorSize}{" "}
          · {formatSize(report!.size)}
        </p>
      )}
      {report?.message && (
        <p role="status" className="surface-message">
          {report.message}
        </p>
      )}
      {!!report?.failures.length && (
        <details className="surface-failures">
          <summary>
            读取失败的位置（{report.failures.length + report.omittedFailures}{" "}
            块）
          </summary>
          <table>
            <thead>
              <tr>
                <th>绝对起始 LBA</th>
                <th>扇区数量</th>
                <th>错误码</th>
                <th>耗时</th>
              </tr>
            </thead>
            <tbody>
              {report.failures.slice(0, 20).map((f) => (
                <tr key={f.startSector}>
                  <td>{f.startSector}</td>
                  <td>{f.sectorCount}</td>
                  <td>{f.code}</td>
                  <td>{f.elapsedMs} ms</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p>
            界面显示前 20 项；报告最多保留 1024
            项，超出部分只计入总数。失败块内未逐扇区定位。
          </p>
        </details>
      )}
      <p className="surface-note">
        仅检测，不写入或修复。读取耗时受设备缓存、负载及虚拟化影响；较慢不等于坏道。取消会等待当前读取返回。
      </p>
      <FormError message={error} />
      {(snapshot?.loaded || !!snapshot?.resumeCount) && (
        <p className="surface-note" role="status">
          结果包含{snapshot.loaded ? "加载的历史记录" : "分次检测记录"}
          ；继续只读取未完成范围，已检测部分不会重读。
        </p>
      )}
      {saved && (
        <p className="surface-note" role="status">
          {saved}
        </p>
      )}
      <div className="surface-actions">
        <div className="surface-file-actions">
          <button
            className="button"
            disabled={busy}
            onClick={() => void load()}
          >
            <FolderOpen size={16} />
            加载进度
          </button>
          <button
            className="button"
            disabled={busy || !snapshot}
            onClick={() => void save(true)}
          >
            <Save size={16} />
            保存进度
          </button>
          <button
            className="button"
            disabled={busy || !snapshot}
            onClick={() => void save()}
          >
            <Download size={16} />
            报告
          </button>
        </div>
        {running ? (
          <button
            className="button"
            disabled={stopping}
            onClick={() => void cancel()}
          >
            <Pause size={15} />
            {stopping ? "正在暂停" : "暂停检测"}
          </button>
        ) : (
          <div className="surface-file-actions">
            <button
              className={`button ${resumable ? "" : "primary"}`}
              disabled={pending || !disk.hardware?.identity}
              onClick={() => void begin()}
            >
              <ScanLine size={16} />
              {pending ? "请稍候…" : snapshot ? "重新检测" : "开始检测"}
            </button>
            {resumable && (
              <button
                className="button primary"
                disabled={pending}
                onClick={() => void begin(true)}
              >
                <Play size={16} />
                继续检测
              </button>
            )}
          </div>
        )}
      </div>
    </Dialog>
  );
}

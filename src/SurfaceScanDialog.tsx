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
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import type { Disk } from "./types";
import { formatSize } from "./types";
import {
  surfaceRange,
  surfaceCanResume,
  surfaceReadTimeout,
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
  const { t, language, catalogText } = useLocale();
  const latencyLabels = labels.map(catalogText);
  const [partition, setPartition] = useState("");
  const [start, setStart] = useState("0");
  const [count, setCount] = useState(String(disk.size / disk.sectorSize));
  const [refineFailures, setRefineFailures] = useState(false);
  const [timeout, setTimeoutText] = useState("");
  const [snapshot, setSnapshot] = useState<SurfaceSnapshot | null>(null);
  const [pending, setPending] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState<{
    path: string;
    progress: boolean;
  } | null>(null);
  const control = useRef({ pending: false, id: "", busy: false, onBusy });
  control.current.onBusy = onBusy;
  const report = snapshot?.report;
  const refinement = report?.refinement;
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
        if (alive) setError(e instanceof Error ? e.message : String(e));
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
    setRefineFailures(!!next.report.refinement);
    setTimeoutText(next.report.readTimeoutSeconds?.toString() ?? "");
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
    setSaved(null);
    try {
      const options = resume
        ? snapshot!.options
        : surfaceRange(start, count, size, disk.sectorSize);
      const readTimeoutSeconds = surfaceReadTimeout(timeout);
      if (!disk.hardware?.identity || !disk.hardware.layoutKey)
        throw Error(t("请刷新磁盘信息后重试。"));
      control.current.pending = true;
      setPending(true);
      setBusy(true);
      setStopping(false);
      control.current.id = resume
        ? await invoke<string>("surface_resume", {
            id: snapshot!.id,
            readTimeoutSeconds,
          })
        : await invoke<string>("surface_start", {
            request: {
              source: {
                target: disk.hardware.identity,
                layoutKey: disk.hardware.layoutKey,
                partition: partition ? Number(partition) : null,
              },
              options,
              refineFailures,
              readTimeoutSeconds,
            },
          });
      const next = await invoke<SurfaceSnapshot | null>("surface_status");
      setSnapshot(next);
      if (resume && next) restoreOptions(next);
    } catch (e) {
      setError(e instanceof Error ? e.message.trim() : String(e));
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
      setError(e instanceof Error ? e.message : String(e));
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
      if (path) setSaved({ path, progress: progressFile });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
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
    setSaved(null);
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
      setError(e instanceof Error ? e.message : String(e));
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
      title={t("坏道检测")}
      subtitle={`${diskTitle(disk, t)} · ${formatSize(disk.size)} · ${disk.hardware?.model ?? ""}`}
      onClose={() => {
        if (!control.current.busy) onClose();
      }}
    >
      <div className="surface-options">
        <label>
          {t("检测范围")}
          <select
            value={partition}
            disabled={busy}
            onChange={(e) => choose(e.target.value)}
          >
            <option value="">{t("整个磁盘")}</option>
            {disk.partitions
              .filter((p) => p.number !== undefined)
              .map((p) => (
                <option key={p.id} value={p.number}>
                  {t("分区 {number}", { number: p.number! })} ·{" "}
                  {partitionTitle(p, catalogText)} · {formatSize(p.size)}
                </option>
              ))}
          </select>
        </label>
        <label>
          {t("起始扇区")}
          <input
            inputMode="numeric"
            value={start}
            disabled={busy}
            onChange={(e) => setStart(e.target.value)}
          />
        </label>
        <label>
          {t("扇区数量")}
          <input
            inputMode="numeric"
            value={count}
            disabled={busy}
            onChange={(e) => setCount(e.target.value)}
          />
        </label>
      </div>
      <p className="surface-note">
        {t("范围内相对扇区 · 每扇区 {bytes} B · 每次读取最多 1 MiB", {
          bytes: disk.sectorSize,
        })}
      </p>
      <label className="surface-timeout-option">
        {t("单次读取超时（秒）")}
        <input
          inputMode="numeric"
          placeholder={t("系统默认")}
          value={timeout}
          onChange={(e) => setTimeoutText(e.target.value)}
          disabled={busy}
          aria-describedby="surface-timeout-help"
        />
      </label>
      <p className="surface-note" id="surface-timeout-help">
        {t(
          "填写 1 到 120 秒，留空使用系统默认。超时时等待驱动确认取消，续扫前可调整设置。",
        )}
      </p>
      <label className="surface-refinement-option">
        <input
          type="checkbox"
          checked={refineFailures}
          disabled={busy}
          onChange={(e) => setRefineFailures(e.target.checked)}
          aria-describedby={
            refineFailures ? "surface-refinement-help" : undefined
          }
        />
        {t("细查读取失败的块")}
      </label>
      {refineFailures && (
        <p className="surface-note" id="surface-refinement-help">
          {t(
            "细查会在失败块内读取更小的范围。达到读取上限后，余下范围标为未定位。",
          )}
        </p>
      )}
      <div className="surface-progress-heading">
        <strong>
          {running
            ? stopping
              ? t("正在暂停，等待当前读取返回…")
              : t("正在检测")
            : report?.state === "completed"
              ? t("检测完成")
              : report?.state === "paused"
                ? t("检测已暂停")
                : report?.state === "timedOut"
                  ? t("检测因读取超时停止")
                  : report?.state === "canceled"
                    ? t("检测已取消")
                    : report?.state === "failed"
                      ? t("检测已停止")
                      : t("准备检测")}
        </strong>
        <span>
          {(progress / 100).toLocaleString(language, {
            style: "percent",
            minimumFractionDigits: 1,
            maximumFractionDigits: 1,
          })}
        </span>
      </div>
      <progress
        className="surface-progress"
        max={100}
        value={progress}
        aria-label={t("检测进度")}
      />
      <div className="surface-grid" aria-label={t("读取耗时分布")}>
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
                ? [
                    t("已检测 {size} · 最慢 {latency}", {
                      size: formatSize(cell.checkedBytes),
                      latency: latencyLabels[cell.worst],
                    }),
                    cell.failedBytes
                      ? t("失败块 {size}", {
                          size: formatSize(cell.failedBytes),
                        })
                      : "",
                  ]
                    .filter(Boolean)
                    .join(" · ")
                : t("未检测")
            }
          />
        ))}
      </div>
      <div className="surface-legend">
        {latencyLabels.map((label, i) => (
          <span key={label}>
            <i className={`surface-level-${i}`} />
            {label}
            <b>{(report?.latencyBlocks[i] ?? 0).toLocaleString(language)}</b>
          </span>
        ))}
      </div>
      <div className="surface-totals">
        <div>
          <span>{t("已检测")}</span>
          <strong>{formatSize(report?.checkedBytes ?? 0)}</strong>
        </div>
        <div>
          <span>{t("失败块范围")}</span>
          <strong>{formatSize(report?.failedBytes ?? 0)}</strong>
        </div>
        <div>
          <span>{t("平均读取速度")}</span>
          <strong>
            {report?.elapsedMs
              ? `${formatSize(report.checkedBytes / (report.elapsedMs / 1000))}/s`
              : "—"}
          </strong>
        </div>
      </div>
      {snapshot && report && (
        <p className="surface-note surface-source">
          {snapshot.source} · {t("绝对起始 LBA")}{" "}
          {(report.offset / report.sectorSize).toLocaleString(language)} ·{" "}
          {formatSize(report.size)}
        </p>
      )}
      {report?.message && (
        <p role="status" className="surface-message">
          {catalogText(report.message)}
        </p>
      )}
      {report?.timedOutRead && (
        <p className="surface-note">
          {t("超时请求 · 绝对 LBA {start} · {count} 扇区 · {seconds} 秒", {
            start: report.timedOutRead.startSector.toLocaleString(language),
            count: report.timedOutRead.sectorCount.toLocaleString(language),
            seconds: report.readTimeoutSeconds!,
          })}
        </p>
      )}
      {refinement && report && (
        <>
          <div className="surface-totals">
            <div>
              <span>{t("单扇区读取失败")}</span>
              <strong>
                {refinement.failedSectors.toLocaleString(language)}
              </strong>
            </div>
            <div>
              <span>{t("未定位扇区")}</span>
              <strong>
                {refinement.unresolvedSectors.toLocaleString(language)}
              </strong>
            </div>
            <div>
              <span>{t("细查时读通的扇区")}</span>
              <strong>
                {(
                  report.failedBytes / report.sectorSize -
                  refinement.failedSectors -
                  refinement.unresolvedSectors
                ).toLocaleString(language)}
              </strong>
            </div>
          </div>
          {refinement.ranges.length > 0 && (
            <details className="surface-failures">
              <summary>
                {t("细查范围（{count} 项）", {
                  count: (
                    refinement.ranges.length + refinement.omittedRanges
                  ).toLocaleString(language),
                })}
              </summary>
              <table>
                <thead>
                  <tr>
                    <th>{t("绝对起始 LBA")}</th>
                    <th>{t("扇区数量")}</th>
                    <th>{t("错误码")}</th>
                    <th>{t("扫描结果")}</th>
                  </tr>
                </thead>
                <tbody>
                  {refinement.ranges.slice(0, 20).map((range) => (
                    <tr key={range.startSector}>
                      <td>{range.startSector.toLocaleString(language)}</td>
                      <td>{range.sectorCount.toLocaleString(language)}</td>
                      <td>{range.code}</td>
                      <td>
                        {t(
                          range.resolution === "sectorReadFailed"
                            ? "单扇区读取失败"
                            : "未定位范围",
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <p>
                {t(
                  "界面显示前 20 项。报告最多保留 1024 项，超出部分计入总数。",
                )}
              </p>
            </details>
          )}
        </>
      )}
      {!!report?.failures.length && (
        <details className="surface-failures">
          <summary>
            {t("读取失败的位置（{count} 块）", {
              count: (
                report.failures.length + report.omittedFailures
              ).toLocaleString(language),
            })}
          </summary>
          <table>
            <thead>
              <tr>
                <th>{t("绝对起始 LBA")}</th>
                <th>{t("扇区数量")}</th>
                <th>{t("错误码")}</th>
                <th>{t("耗时")}</th>
              </tr>
            </thead>
            <tbody>
              {report.failures.slice(0, 20).map((f) => (
                <tr key={f.startSector}>
                  <td>{f.startSector.toLocaleString(language)}</td>
                  <td>{f.sectorCount.toLocaleString(language)}</td>
                  <td>{f.code}</td>
                  <td>{f.elapsedMs} ms</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p>
            {t(
              refinement
                ? "界面显示前 20 项。报告最多保留 1024 项，超出部分计入总数。"
                : "界面显示前 20 项；报告最多保留 1024 项，超出部分只计入总数。失败块内未逐扇区定位。",
            )}
          </p>
        </details>
      )}
      <p className="surface-note">
        {t(
          "仅检测，不写入或修复。读取耗时受设备缓存、负载及虚拟化影响；较慢不等于坏道。取消会等待当前读取返回。",
        )}
      </p>
      <FormError message={catalogText(error)} />
      {(snapshot?.loaded || !!snapshot?.resumeCount) && (
        <p className="surface-note" role="status">
          {t(
            snapshot.loaded
              ? "结果包含加载的历史记录；继续只读取未完成范围，已检测部分不会重读。"
              : "结果包含分次检测记录；继续只读取未完成范围，已检测部分不会重读。",
          )}
        </p>
      )}
      {saved && (
        <p className="surface-note" role="status">
          {t(saved.progress ? "已保存进度到 {path}" : "已保存报告到 {path}", {
            path: saved.path,
          })}
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
            {t("加载进度")}
          </button>
          <button
            className="button"
            disabled={busy || !snapshot}
            onClick={() => void save(true)}
          >
            <Save size={16} />
            {t("保存进度")}
          </button>
          <button
            className="button"
            disabled={busy || !snapshot}
            onClick={() => void save()}
          >
            <Download size={16} />
            {t("报告")}
          </button>
        </div>
        {running ? (
          <button
            className="button"
            disabled={stopping}
            onClick={() => void cancel()}
          >
            <Pause size={15} />
            {stopping ? t("正在暂停") : t("暂停检测")}
          </button>
        ) : (
          <div className="surface-file-actions">
            <button
              className={`button ${resumable ? "" : "primary"}`}
              disabled={pending || !disk.hardware?.identity}
              onClick={() => void begin()}
            >
              <ScanLine size={16} />
              {pending
                ? t("请稍候…")
                : snapshot
                  ? t("重新检测")
                  : t("开始检测")}
            </button>
            {resumable && (
              <button
                className="button primary"
                disabled={pending}
                onClick={() => void begin(true)}
              >
                <Play size={16} />
                {t("继续检测")}
              </button>
            )}
          </div>
        )}
      </div>
    </Dialog>
  );
}

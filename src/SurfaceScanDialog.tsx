import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Download,
  ScanLine,
  Pause,
  Play,
  FolderOpen,
  Save,
  Wrench,
  X,
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
  type SurfaceStatus,
  type SurfaceRepairPreview,
  type SurfaceRepairOutcome,
} from "./surfaceModel";

const labels = [
  "< 10 ms",
  "10–49 ms",
  "50–149 ms",
  "150–499 ms",
  "≥ 500 ms",
  "读取失败",
];
const stateLabels = {
  running: "正在检测",
  completed: "检测完成",
  paused: "检测已暂停",
  timedOut: "检测因读取超时停止",
  canceled: "检测已取消",
  failed: "检测已停止",
} as const satisfies Record<SurfaceSnapshot["report"]["state"], string>;
type Draft = {
  partition: string;
  start: string;
  count: string;
  refineFailures: boolean;
  timeout: string;
};

function scanDraft(disk: Disk, previous?: SurfaceSnapshot | null): Draft {
  return {
    partition: previous?.partition?.toString() ?? "",
    start: previous?.options.startSector.toString() ?? "0",
    count: String(previous?.options.sectorCount ?? disk.size / disk.sectorSize),
    refineFailures: !!previous?.report.refinement,
    timeout: previous?.report.readTimeoutSeconds?.toString() ?? "",
  };
}

export function SurfaceScanDialog({
  initialDisk,
  disks,
  onClose,
  onBusy,
  onRefresh,
}: {
  initialDisk: Disk;
  disks: Disk[];
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const { t, language, catalogText } = useLocale();
  const latencyLabels = labels.map(catalogText);
  const [selectedId, select] = useState(initialDisk.id);
  const [queued, setQueued] = useState([initialDisk.id]);
  const [edits, setEdits] = useState<Record<string, Draft>>({});
  const [status, setStatus] = useState<
    (SurfaceStatus & { target: string }) | null
  >(null);
  const [pending, setPending] = useState(false);
  const [pausingId, setPausingId] = useState("");
  const [error, setError] = useState("");
  const [repairPlan, setRepairPlan] = useState<SurfaceRepairPreview | null>(
    null,
  );
  const [confirmedRepair, setConfirmedRepair] = useState(false);
  const [repairResult, setRepairResult] = useState<{
    scanId: string;
    outcome: SurfaceRepairOutcome;
  } | null>(null);
  const [repairJob, setRepairJob] = useState<{
    id: string;
    phase: string;
    done: number;
    total: number;
  } | null>(null);
  const [saved, setSaved] = useState<{
    target: string;
    path: string;
    progress: boolean;
  } | null>(null);
  const control = useRef({
    pending: false,
    running: false,
    revision: 0,
    onBusy,
  });
  control.current.onBusy = onBusy;
  const candidates = disks.filter(
    (d) =>
      d.kind === "physical" && d.hardware?.identity && d.hardware.layoutKey,
  );
  const disk = candidates.find((d) => d.id === selectedId) ?? initialDisk;
  const target = disk.hardware?.identity ?? "";
  const snapshot = status?.target === target ? status.selected : null;
  const overview = status?.scans ?? [];
  const available = status?.target === target;
  const report = snapshot?.report;
  const refinement = report?.refinement;
  const running = report?.state === "running";
  const stopping =
    running &&
    (pausingId === snapshot?.id ||
      overview.some((scan) => scan.target === target && scan.pausing));
  const anyRunning = overview.some((scan) => scan.state === "running");
  const busy = pending || running || !available;
  const fileBusy = busy || anyRunning;
  const canRepair = available && status.repairAvailable;
  const resumable = surfaceCanResume(snapshot);
  const draft = edits[disk.id] ?? scanDraft(disk, snapshot);
  const { partition, start, count, refineFailures, timeout } = draft;
  const known = (d: Disk) =>
    overview.find((scan) => scan.target === d.hardware!.identity);
  const visible = candidates.filter((d) => queued.includes(d.id) || known(d));
  const ready = visible.filter((d) => !known(d));
  const unlisted = candidates.filter((d) => !visible.includes(d));
  const shownSave = saved?.target === target ? saved : null;
  const shownRepair =
    repairResult?.scanId === snapshot?.id ? repairResult?.outcome : null;

  function apply(next: SurfaceStatus) {
    setStatus({ ...next, target });
    control.current.running = next.scans.some(
      (scan) => scan.state === "running",
    );
    control.current.onBusy(control.current.running || control.current.pending);
  }

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      if (target && !control.current.pending) {
        const revision = control.current.revision;
        try {
          const next = await invoke<SurfaceStatus>("surface_status", {
            target,
          });
          if (alive && revision === control.current.revision) apply(next);
        } catch (err) {
          if (alive && revision === control.current.revision)
            setError(String(err instanceof Error ? err.message : err));
        }
      }
      if (alive) timer = setTimeout(() => void poll(), 400);
    }
    void poll();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [target]);

  const applyingRepair = !!repairPlan && pending;
  useEffect(() => {
    if (!applyingRepair) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    async function pollRepair() {
      try {
        const job = await invoke<{
          id: string;
          title: string;
          state: string;
          phase: string;
          done: number;
          total: number;
        } | null>("data_status");
        if (live && job?.title === "写入修复坏道" && job.state === "running") {
          setRepairJob(job);
        }
      } catch {
        // The execute command returns the authoritative outcome.
      }
      if (live) timer = setTimeout(() => void pollRepair(), 400);
    }
    void pollRepair();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [applyingRepair]);

  function edit(changes: Partial<Draft>) {
    setEdits((current) => ({
      ...current,
      [disk.id]: { ...(current[disk.id] ?? draft), ...changes },
    }));
  }

  function choose(value: string) {
    const capacity = value
      ? (disk.partitions.find((p) => p.number === Number(value))?.size ?? 0)
      : disk.size;
    edit({
      partition: value,
      start: "0",
      count: String(capacity / disk.sectorSize),
    });
  }

  function focus(id: string) {
    setError("");
    select(id);
  }

  function add(id: string) {
    setQueued((current) => [...current, id]);
    focus(id);
  }

  function remove(id: string) {
    const next = visible.find((d) => d.id !== id);
    if (!next) return;
    setQueued((current) => current.filter((item) => item !== id));
    if (disk.id === id) focus(next.id);
  }

  function clearEdits(ids: string[]) {
    setEdits((current) =>
      Object.fromEntries(
        Object.entries(current).filter(([id]) => !ids.includes(id)),
      ),
    );
  }

  function request(candidate: Disk) {
    const inputs =
      edits[candidate.id] ??
      scanDraft(candidate, candidate.id === disk.id ? snapshot : null);
    const hardware = candidate.hardware;
    if (!hardware?.identity || !hardware.layoutKey)
      throw Error(t("请刷新磁盘信息后重试。"));
    const part = inputs.partition ? Number(inputs.partition) : null;
    const capacity =
      part === null
        ? candidate.size
        : (candidate.partitions.find((p) => p.number === part)?.size ?? 0);
    try {
      return {
        source: {
          target: hardware.identity,
          layoutKey: hardware.layoutKey,
          partition: part,
        },
        options: surfaceRange(
          inputs.start,
          inputs.count,
          capacity,
          candidate.sectorSize,
        ),
        refineFailures: inputs.refineFailures,
        readTimeoutSeconds: surfaceReadTimeout(inputs.timeout),
      };
    } catch (err) {
      const message = catalogText(
        String(err instanceof Error ? err.message : err).trim(),
      );
      throw Error(`${diskTitle(candidate, t)} · ${message}`);
    }
  }

  async function perform(action: () => Promise<void>) {
    if (control.current.pending) return;
    control.current.revision++;
    control.current.pending = true;
    control.current.onBusy(true);
    setPending(true);
    setError("");
    try {
      await action();
      apply(await invoke<SurfaceStatus>("surface_status", { target }));
    } catch (err) {
      setError(String(err instanceof Error ? err.message : err).trim());
    } finally {
      control.current.pending = false;
      control.current.onBusy(control.current.running);
      setPending(false);
    }
  }

  async function begin(targets: Disk[], resume = false) {
    setSaved(null);
    await perform(async () => {
      if (resume && snapshot) {
        await invoke("surface_resume", {
          id: snapshot.id,
          readTimeoutSeconds: surfaceReadTimeout(timeout),
        });
      } else {
        await invoke("surface_start", { requests: targets.map(request) });
      }
      control.current.running = true;
      clearEdits(targets.map((d) => d.id));
    });
  }

  async function cancel() {
    if (!snapshot) return;
    setPausingId(snapshot.id);
    try {
      await invoke("surface_pause", { id: snapshot.id });
    } catch (err) {
      setPausingId("");
      setError(String(err instanceof Error ? err.message : err));
    }
  }

  async function save(progress = false) {
    if (!snapshot || fileBusy) return;
    await perform(async () => {
      const path = await invoke<string | null>(
        progress ? "surface_save_progress" : "surface_export",
        { id: snapshot.id },
      );
      if (path) setSaved({ target, path, progress });
    });
  }

  async function load() {
    if (fileBusy) return;
    setSaved(null);
    await perform(async () => {
      const next = await invoke<SurfaceSnapshot | null>(
        "surface_load_progress",
        { target },
      );
      if (next) clearEdits([disk.id]);
    });
  }
  async function prepareRepair() {
    if (!snapshot || !canRepair || fileBusy) return;
    await perform(async () => {
      const plan = await invoke<SurfaceRepairPreview>(
        "surface_prepare_repair",
        {
          scanId: snapshot.id,
        },
      );
      setConfirmedRepair(false);
      setRepairPlan(plan);
    });
  }

  function dismissRepair() {
    if (!repairPlan || control.current.pending) return;
    setRepairPlan(null);
    setError("");
    void invoke("surface_discard_repair", { token: repairPlan.token }).catch(
      () => {},
    );
  }

  async function executeRepair() {
    if (!repairPlan || !snapshot || !confirmedRepair) return;
    setRepairJob(null);
    await perform(async () => {
      try {
        const outcome = await invoke<SurfaceRepairOutcome>(
          "surface_execute_repair",
          {
            token: repairPlan.token,
          },
        );
        setRepairResult({ scanId: snapshot.id, outcome });
        if (outcome.diskLeftOffline !== null) await onRefresh();
      } finally {
        setRepairPlan(null);
      }
    });
  }

  if (repairPlan) {
    const sectors = repairPlan.blocks.flatMap((block) => block.failed);
    return (
      <Dialog
        key="repair"
        wide
        title={t("确认坏道写入修复")}
        subtitle={repairPlan.target}
        onClose={dismissRepair}
      >
        <p className="data-plan-warning destructive">
          {t(
            "将仍读不出的扇区写为零，原内容可能永久丢失，文件系统也可能受损。可读的相邻扇区保留原字节。写入后将回读校验，磁盘保持脱机。",
          )}
        </p>
        <p className="surface-note">
          {t("逻辑扇区 {logical} B · 物理扇区 {physical} B", {
            logical: repairPlan.logicalSectorSize,
            physical: repairPlan.physicalSectorSize,
          })}
        </p>
        <div className="surface-totals">
          <div>
            <span>{t("将清零的扇区")}</span>
            <strong>{sectors.length.toLocaleString(language)}</strong>
          </div>
          <div>
            <span>{t("复读成功")}</span>
            <strong>
              {repairPlan.nowReadableSectors.toLocaleString(language)}
            </strong>
          </div>
          <div>
            <span>{t("超出范围或受保护等原因跳过")}</span>
            <strong>
              {repairPlan.excludedSectors.toLocaleString(language)}
            </strong>
          </div>
        </div>
        <div className="surface-repair-sectors">
          <table>
            <thead>
              <tr>
                <th>{t("绝对起始 LBA")}</th>
                <th>{t("错误码")}</th>
              </tr>
            </thead>
            <tbody>
              {sectors.map((sector) => (
                <tr key={sector.lba}>
                  <td>{sector.lba.toLocaleString(language)}</td>
                  <td>{sector.code}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <label className="surface-refinement-option">
          <input
            type="checkbox"
            checked={confirmedRepair}
            disabled={pending}
            onChange={(event) => setConfirmedRepair(event.target.checked)}
          />
          {t("我接受所列扇区的内容可能永久丢失。")}
        </label>
        {applyingRepair && (
          <p className="surface-note" role="status">
            {repairJob
              ? `${catalogText(repairJob.phase)} · ${repairJob.done}/${repairPlan.blocks.length}`
              : t("请稍候…")}
          </p>
        )}
        <FormError message={catalogText(error)} />
        <div className="surface-actions">
          <button
            className="button"
            disabled={pending && !repairJob}
            onClick={() => {
              if (!pending) dismissRepair();
              else if (repairJob)
                void invoke("data_cancel", { id: repairJob.id }).catch((err) =>
                  setError(String(err)),
                );
            }}
          >
            {t(pending ? "取消任务" : "取消")}
          </button>
          <button
            className="button danger"
            disabled={pending || !confirmedRepair}
            onClick={() => void executeRepair()}
          >
            {t("确认写入")}
          </button>
        </div>
      </Dialog>
    );
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
        if (!control.current.pending && !control.current.running) onClose();
      }}
    >
      <section className="surface-targets" aria-label={t("检测磁盘")}>
        <div className="surface-target-toolbar">
          <strong>{t("检测磁盘")}</strong>
          <div className="surface-file-actions">
            {unlisted.length > 0 && (
              <select
                aria-label={t("添加检测磁盘")}
                value=""
                disabled={pending}
                onChange={(event) => add(event.target.value)}
              >
                <option value="" disabled>
                  {t("添加检测磁盘")}
                </option>
                {unlisted.map((candidate) => (
                  <option key={candidate.id} value={candidate.id}>
                    {diskTitle(candidate, t)} · {formatSize(candidate.size)} ·{" "}
                    {candidate.hardware?.model}
                  </option>
                ))}
              </select>
            )}
            {ready.length > 1 && (
              <button
                className="button primary"
                disabled={pending || !available}
                onClick={() => void begin(ready)}
              >
                {t("开始待扫磁盘（{count}）", { count: ready.length })}
              </button>
            )}
          </div>
        </div>
        <ul className="surface-target-list">
          {visible.map((candidate) => {
            const scan = known(candidate);
            const selected = disk.id === candidate.id;
            return (
              <li key={candidate.id} className={selected ? "selected" : ""}>
                <button
                  className="surface-target-select"
                  aria-pressed={selected}
                  onClick={() => focus(candidate.id)}
                  disabled={pending}
                >
                  <strong>{diskTitle(candidate, t)}</strong>
                  <span>
                    {scan
                      ? t(scan.pausing ? "正在暂停" : stateLabels[scan.state])
                      : t("准备检测")}
                    {scan &&
                      ` · ${(scan.checkedBytes / scan.size).toLocaleString(
                        language,
                        {
                          style: "percent",
                          maximumFractionDigits: 1,
                        },
                      )}`}
                  </span>
                </button>
                {!scan && visible.length > 1 && (
                  <button
                    className="icon-button surface-target-remove"
                    aria-label={t("移除 {disk}", {
                      disk: diskTitle(candidate, t),
                    })}
                    title={t("移除 {disk}", { disk: diskTitle(candidate, t) })}
                    disabled={pending}
                    onClick={() => remove(candidate.id)}
                  >
                    <X size={14} />
                  </button>
                )}
              </li>
            );
          })}
        </ul>
        {anyRunning && (
          <p className="surface-note">
            {t("所有检测停止后可加载进度或保存文件。")}
          </p>
        )}
      </section>
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
            onChange={(e) => edit({ start: e.target.value })}
          />
        </label>
        <label>
          {t("扇区数量")}
          <input
            inputMode="numeric"
            value={count}
            disabled={busy}
            onChange={(e) => edit({ count: e.target.value })}
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
          onChange={(e) => edit({ timeout: e.target.value })}
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
          onChange={(e) => edit({ refineFailures: e.target.checked })}
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
          {t(
            stopping
              ? "正在暂停，等待当前读取返回…"
              : report
                ? stateLabels[report.state]
                : "准备检测",
          )}
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
          "检测过程为只读。读取耗时受设备缓存、负载及虚拟化影响；较慢不等于坏道。取消会等待当前读取返回。",
        )}
      </p>
      {shownRepair && (
        <section
          className="surface-repair-result"
          aria-label={t("坏道修复结果")}
        >
          <strong>{t("坏道修复结果")}</strong>
          <p className="surface-note">{catalogText(shownRepair.message)}</p>
          <p className="surface-note">
            {t(
              "计划 {planned} 块 · 已尝试写入 {attempted} 块 · 已回读核对 {verified} 块",
              {
                planned: shownRepair.plannedBlocks.toLocaleString(language),
                attempted: shownRepair.attemptedBlocks.toLocaleString(language),
                verified:
                  shownRepair.verifiedBlocks.length.toLocaleString(language),
              },
            )}
          </p>
          {shownRepair.diskLeftOffline === true &&
            shownRepair.state !== "completed" && (
              <p className="surface-note">
                {t("磁盘保持脱机。请核对已尝试写入的范围。")}
              </p>
            )}
          {shownRepair.diskLeftOffline === false && (
            <p className="surface-note">{t("无法确认磁盘仍处于脱机状态。")}</p>
          )}
          {shownRepair.verifiedBlocks.length > 0 && (
            <details className="surface-failures">
              <summary>{t("写后回读 SHA-256")}</summary>
              <div className="surface-repair-sectors">
                <table>
                  <thead>
                    <tr>
                      <th>{t("绝对起始 LBA")}</th>
                      <th>SHA-256</th>
                    </tr>
                  </thead>
                  <tbody>
                    {shownRepair.verifiedBlocks.map((block) => (
                      <tr key={block.startLba}>
                        <td>{block.startLba.toLocaleString(language)}</td>
                        <td>
                          <code>{block.sha256}</code>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </details>
          )}
        </section>
      )}
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
      {shownSave && (
        <p className="surface-note" role="status">
          {t(
            shownSave.progress ? "已保存进度到 {path}" : "已保存报告到 {path}",
            { path: shownSave.path },
          )}
        </p>
      )}
      <div className="surface-actions">
        <div className="surface-file-actions">
          <button
            className="button"
            disabled={fileBusy}
            onClick={() => void load()}
          >
            <FolderOpen size={16} />
            {t("加载进度")}
          </button>
          <button
            className="button"
            disabled={fileBusy || !snapshot}
            onClick={() => void save(true)}
          >
            <Save size={16} />
            {t("保存进度")}
          </button>
          <button
            className="button"
            disabled={fileBusy || !snapshot}
            onClick={() => void save()}
          >
            <Download size={16} />
            {t("报告")}
          </button>
        </div>
        {running ? (
          <button
            className="button"
            disabled={stopping || pending}
            onClick={() => void cancel()}
          >
            <Pause size={15} />
            {stopping ? t("正在暂停") : t("暂停检测")}
          </button>
        ) : (
          <div className="surface-file-actions">
            {canRepair && (
              <button
                className="button"
                disabled={fileBusy}
                onClick={() => void prepareRepair()}
              >
                <Wrench size={15} />
                {t("预览坏道修复")}
              </button>
            )}
            <button
              className={`button ${resumable ? "" : "primary"}`}
              disabled={busy || !disk.hardware?.identity}
              onClick={() => void begin([disk])}
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
                disabled={busy}
                onClick={() => void begin([disk], true)}
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

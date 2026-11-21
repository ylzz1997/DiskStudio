import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Activity,
  Eraser,
  Brush,
  Gauge,
  LoaderCircle,
  Save,
  StopCircle,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { formatSize, type Disk } from "./types";
import {
  benchmarkFileSize,
  benchmarkOptions,
  benchmarkRates,
  benchmarkThroughput,
  maintenanceRange,
  maintenanceTarget,
  wipePattern,
  type BenchmarkForm,
  type BenchmarkMode,
  type BenchmarkOptions,
  type BenchmarkSample,
  type BenchmarkUnit,
  type MaintenanceMode,
  type MaintenanceOperation,
  type MaintenancePreview,
  type MaintenanceReport,
} from "./maintenance";
type Job = {
  id: string;
  title: string;
  state: string;
  phase: string;
  done: number;
  total: number;
};
const modes = [
  { id: "benchmark", title: "测速", icon: Gauge },
  { id: "wipe", title: "清除扇区", icon: Eraser },
  { id: "wipeFree", title: "清除空闲空间", icon: Brush },
  { id: "trim", title: "TRIM", icon: Activity },
] as const;
const initialWorkload: BenchmarkForm = {
  mode: "read",
  access: "sequential",
  blockSize: 1048576,
  operations: "256",
  queueDepth: "1",
  threadCount: "1",
  readPercent: "70",
  limit: "requests",
  duration: "5",
};

function workloadMode(options: BenchmarkOptions): BenchmarkMode {
  if (!options.write) return "read";
  return options.readPercent == null ? "write" : "mixed";
}

const workloadModeLabel = {
  read: "读取测速",
  write: "写入测速",
  mixed: "混合读写",
} as const;
function ThroughputChart({
  samples,
  showRead,
  showWrite,
}: {
  samples: BenchmarkSample[];
  showRead: boolean;
  showWrite: boolean;
}) {
  const { t, language } = useLocale();
  const canvas = useRef<HTMLCanvasElement>(null);
  const intervals = useMemo(() => benchmarkThroughput(samples), [samples]);
  const total = intervals.at(-1)?.endSeconds ?? 0;
  const peak = Math.max(
    1,
    ...intervals.map((interval) =>
      Math.max(interval.readMiBPerSecond, interval.writeMiBPerSecond),
    ),
  );
  useEffect(() => {
    const element = canvas.current;
    const ctx = element?.getContext("2d");
    if (!element || !ctx || total <= 0) return;
    const { width, height } = element;
    const margin = 5;
    const x = (seconds: number) =>
      margin + ((width - margin * 2) * seconds) / total;
    const y = (rate: number) =>
      height - margin - ((height - margin * 2) * rate) / peak;
    ctx.clearRect(0, 0, width, height);
    ctx.strokeStyle = getComputedStyle(element)
      .getPropertyValue("--border")
      .trim();
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (const rate of [0, peak / 2, peak]) {
      ctx.moveTo(x(0), y(rate));
      ctx.lineTo(x(total), y(rate));
    }
    ctx.stroke();
    for (const [visible, field, color, dash] of [
      [showRead, "readMiBPerSecond", "--benchmark-read", []],
      [
        showWrite,
        "writeMiBPerSecond",
        "--benchmark-write",
        showRead ? [7, 5] : [],
      ],
    ] as const) {
      if (!visible) continue;
      ctx.strokeStyle = getComputedStyle(element)
        .getPropertyValue(color)
        .trim();
      ctx.lineWidth = 2;
      ctx.setLineDash([...dash]);
      ctx.beginPath();
      intervals.forEach((interval, index) => {
        const point = x(interval.startSeconds);
        const rate = y(interval[field]);
        if (index === 0) ctx.moveTo(point, rate);
        else ctx.lineTo(point, rate);
        ctx.lineTo(x(interval.endSeconds), rate);
      });
      ctx.stroke();
    }
    ctx.setLineDash([]);
  }, [intervals, total, peak, showRead, showWrite]);
  if (!total) return null;
  return (
    <figure className="maintenance-throughput">
      <figcaption>
        <strong>{t("测速曲线")}</strong>
        <span>
          {peak.toLocaleString(language, { maximumFractionDigits: 1 })} MiB/s
        </span>
      </figcaption>
      <canvas
        width={640}
        height={110}
        ref={canvas}
        aria-label={t("分时读写吞吐量")}
        role="img"
      />
      <div className="maintenance-throughput-time">
        <span>0 s</span>
        <span>
          {total.toLocaleString(language, { maximumFractionDigits: 3 })} s
        </span>
      </div>
      <div className="maintenance-throughput-legend">
        {showRead && (
          <span className="maintenance-throughput-read">{t("读取")}</span>
        )}
        {showWrite && (
          <span className="maintenance-throughput-write">{t("写入")}</span>
        )}
      </div>
    </figure>
  );
}

export function MaintenanceDialog({
  disk,
  initialPartition,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  initialPartition?: number;
  onClose: () => void;
  onBusy: (b: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const { t, language, catalogText } = useLocale();
  const [mode, setMode] = useState<MaintenanceMode>("benchmark");
  const [partition, setPartition] = useState(
    initialPartition == null ? "" : String(initialPartition),
  );
  const first = disk.partitions.find((p) => p.number === initialPartition);
  const [start, setStart] = useState("0"),
    [count, setCount] = useState(
      String((first?.size ?? disk.size) / disk.sectorSize),
    );
  const [rawWorkload, setRawWorkload] = useState(initialWorkload);
  const [fileWorkloads, setFileWorkloads] = useState([initialWorkload]),
    [workloadIndex, setWorkloadIndex] = useState(0),
    [rounds, setRounds] = useState(1);
  const [resultIndex, setResultIndex] = useState<number | null>(null),
    [resultUnit, setResultUnit] = useState<BenchmarkUnit>("MiB/s");
  const [benchmarkScope, setBenchmarkScope] = useState<"disk" | "file">("disk"),
    [fileSize, setFileSize] = useState("64"),
    [zeroFill, setZeroFill] = useState(false);
  const [random, setRandom] = useState(false),
    [hex, setHex] = useState("00"),
    [passes, setPasses] = useState(1);
  const [preview, setPreview] = useState<MaintenancePreview | null>(null),
    [report, setReport] = useState<MaintenanceReport | null>(null);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState(""),
    [saved, setSaved] = useState("");
  const [ack, setAck] = useState(false),
    [job, setJob] = useState<Job | null>(null),
    [stopping, setStopping] = useState(false);
  const token = useRef<string | null>(null),
    phase = useRef<"idle" | "preview" | "execute" | "export">("idle");
  const active = useRef(true);
  const previewNode = useRef<HTMLElement>(null);
  const resultNode = useRef<HTMLElement>(null);
  useEffect(() => {
    if (preview) previewNode.current?.scrollIntoView({ block: "nearest" });
  }, [preview]);
  useEffect(() => {
    if (report?.state && report.state !== "running")
      resultNode.current?.scrollIntoView({ block: "nearest" });
  }, [report?.state]);
  function discard() {
    const old = token.current;
    token.current = null;
    if (old) void invoke("maintenance_discard", { token: old }).catch(() => {});
  }
  function invalidate() {
    discard();
    setPreview(null);
    setAck(false);
    setError("");
    setSaved("");
    setReport(null);
    setResultIndex(null);
  }
  useEffect(() => {
    active.current = true;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const next = await invoke<Job | null>("data_status");
        if (!active.current) return;
        if (
          phase.current !== "idle" &&
          next?.state === "running" &&
          ["执行磁盘维护", "预览磁盘维护", "保存磁盘维护报告"].includes(
            next.title,
          )
        ) {
          setJob(next);
          if (phase.current === "execute") {
            const latest = await invoke<MaintenanceReport | null>(
              "maintenance_status",
            );
            if (
              active.current &&
              latest?.id === next.id &&
              latest.source.target === disk.hardware?.identity
            )
              setReport(latest);
          }
        }
      } catch {
        /* Current command surfaces errors; memory polling never starts I/O. */
      }
      if (active.current) timer = setTimeout(() => void poll(), 400);
    }
    void poll();
    return () => {
      active.current = false;
      clearTimeout(timer);
      discard();
    };
  }, [disk.hardware?.identity]);
  function choosePartition(value: string) {
    invalidate();
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
  function chooseMode(value: MaintenanceMode) {
    invalidate();
    setMode(value);
    if (value === "trim" || value === "wipeFree") {
      setStart("0");
      setCount(
        String(
          (disk.partitions.find((p) => p.number === Number(partition))?.size ??
            disk.size) / disk.sectorSize,
        ),
      );
    }
  }
  const fileBenchmark = mode === "benchmark" && benchmarkScope === "file";
  const form = fileBenchmark ? fileWorkloads[workloadIndex]! : rawWorkload;
  const modeIsTimed = fileBenchmark && form.limit === "duration";
  const fileWrites = fileWorkloads.some((workload) => workload.mode !== "read");
  const partitionRequired =
    fileBenchmark || mode === "trim" || mode === "wipeFree";
  const modifies =
    mode !== "benchmark" || (fileBenchmark ? fileWrites : form.mode !== "read");
  const unavailable =
    !disk.hardware?.identity ||
    !disk.hardware.layoutKey ||
    (fileBenchmark && disk.hardware.offline !== false) ||
    (modifies &&
      ((!fileBenchmark && disk.hardware.isSystem) ||
        disk.hardware.deviceReadOnly !== false));
  function updateWorkload(update: Partial<BenchmarkForm>) {
    invalidate();
    if (fileBenchmark) {
      setFileWorkloads((current) =>
        current.map((workload, index) =>
          index === workloadIndex ? { ...workload, ...update } : workload,
        ),
      );
    } else {
      setRawWorkload((current) => ({ ...current, ...update }));
    }
  }
  async function run(execute: boolean) {
    if (
      phase.current !== "idle" ||
      unavailable ||
      (execute && (!token.current || (preview?.requiresAcknowledgment && !ack)))
    )
      return;
    setError("");
    setSaved("");
    setStopping(false);
    setJob(null);
    let refresh = false;
    try {
      const target = maintenanceTarget(disk, partition);
      const range = fileBenchmark
        ? undefined
        : maintenanceRange(start, count, target.size, disk.sectorSize);
      let operation: MaintenanceOperation;
      if (mode === "wipe" || mode === "wipeFree") {
        if (mode === "wipeFree" && target.source.partition == null)
          throw Error(t("请选择一个完整数据分区。"));
        operation = {
          kind: mode,
          options: { passes, pattern: wipePattern(random, hex) },
        };
      } else if (mode === "trim") {
        if (!target.source.partition)
          throw Error(t("请选择一个普通 NTFS 数据分区。"));
        operation = { kind: "trim" };
      } else {
        if (
          range &&
          ((target.offset + range.startSector * disk.sectorSize) % 4096 !== 0 ||
            (range.sectorCount * disk.sectorSize) % 4096 !== 0 ||
            range.sectorCount * disk.sectorSize < form.blockSize)
        )
          throw Error(t("测速范围须按 4 KiB 对齐且至少容纳一个测量块。"));
        if (fileBenchmark) {
          if (target.source.partition == null)
            throw Error(t("请选择一个联机的普通数据分区。"));
          const size = benchmarkFileSize(fileSize);
          const options = fileWorkloads.map((workload) => {
            if (workload.blockSize > size)
              throw Error(t("测试大小必须容纳所有负载的块。"));
            return benchmarkOptions(workload, true);
          });
          operation = {
            kind: "fileBenchmark",
            options: options[0]!,
            additionalWorkloads: options.slice(1),
            repetitions: rounds,
            fileSize: size,
            zeroFill: fileWrites && zeroFill,
          };
        } else {
          operation = { kind: "benchmark", options: benchmarkOptions(form) };
        }
      }
      phase.current = execute ? "execute" : "preview";
      setBusy(true);
      onBusy(true);
      setReport(null);
      if (execute) {
        const once = token.current;
        token.current = null;
        setPreview(null);
        refresh = modifies && !fileBenchmark;
        const next = await invoke<MaintenanceReport>("maintenance_execute", {
          token: once,
        });
        if (active.current) setReport(next);
      } else {
        discard();
        setPreview(null);
        setAck(false);
        const next = await invoke<MaintenancePreview>("maintenance_prepare", {
          request: { source: target.source, range, operation },
        });
        if (active.current) {
          token.current = next.token;
          setPreview(next);
        } else {
          void invoke("maintenance_discard", { token: next.token });
        }
      }
    } catch (e) {
      if (active.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (refresh) await onRefresh().catch(() => {});
      phase.current = "idle";
      if (active.current) {
        setBusy(false);
        setJob(null);
      }
      onBusy(false);
    }
  }
  async function cancel() {
    if (!job || (mode === "trim" && phase.current === "execute")) return;
    try {
      await invoke("data_cancel", { id: job.id });
      setStopping(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  async function save() {
    if (!report || phase.current !== "idle") return;
    phase.current = "export";
    setBusy(true);
    onBusy(true);
    setError("");
    try {
      const path = await invoke<string | null>("maintenance_export", {
        id: report.id,
      });
      if (path) setSaved(path);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      phase.current = "idle";
      setBusy(false);
      onBusy(false);
    }
  }
  const runs = report?.fileBenchmarkRuns ?? [];
  const selectedRunIndex = resultIndex ?? runs.length - 1;
  const selectedRun = runs[selectedRunIndex];
  const stats = selectedRun?.stats ?? report?.benchmark;
  const wiped = report?.wipe;
  const reportWorkloads =
    report?.operation.kind === "fileBenchmark"
      ? [
          report.operation.options,
          ...(report.operation.additionalWorkloads ?? []),
        ]
      : [];
  const workload = selectedRun
    ? reportWorkloads[selectedRun.workload - 1]
    : report?.operation.kind === "benchmark"
      ? report.operation.options
      : null;
  return (
    <Dialog
      wide
      title={t("磁盘维护")}
      subtitle={`${diskTitle(disk, t)} · ${formatSize(disk.size)}`}
      footer={
        <div className="dialog-actions">
          {report && report.state !== "running" && (
            <button
              className="button"
              disabled={busy}
              onClick={() => void save()}
            >
              <Save size={16} />
              {t("保存报告")}
            </button>
          )}
          {busy && job && !(mode === "trim" && phase.current === "execute") && (
            <button
              className="button"
              disabled={stopping}
              onClick={() => void cancel()}
            >
              <StopCircle size={16} />
              {t("停止")}
            </button>
          )}
          <button className="button" disabled={busy} onClick={onClose}>
            {t("关闭对话框")}
          </button>
          <button
            className="button primary"
            disabled={
              busy ||
              !!unavailable ||
              (!!preview?.requiresAcknowledgment && !ack)
            }
            onClick={() => void run(!!preview)}
          >
            {preview
              ? t("确认{operation}", { operation: catalogText(preview.title) })
              : t("预览操作")}
          </button>
        </div>
      }
      onClose={() => {
        if (phase.current === "idle") onClose();
      }}
    >
      <div className="maintenance-tabs" role="group" aria-label={t("维护功能")}>
        {modes.map(({ id, title, icon: Icon }) => (
          <button
            key={id}
            className={mode === id ? "selected" : ""}
            aria-pressed={mode === id}
            disabled={busy}
            onClick={() => chooseMode(id)}
          >
            <Icon size={18} />
            {catalogText(title)}
          </button>
        ))}
      </div>
      <fieldset className="maintenance-fields" disabled={busy}>
        {mode === "benchmark" && (
          <label>
            {t("测速范围")}
            <select
              value={benchmarkScope}
              onChange={(e) => {
                invalidate();
                setBenchmarkScope(e.target.value as typeof benchmarkScope);
              }}
            >
              <option value="disk">{t("原始磁盘测速")}</option>
              <option value="file">{t("文件测速")}</option>
            </select>
          </label>
        )}
        <label>
          {t("目标")}
          <select
            value={partition}
            onChange={(e) => choosePartition(e.target.value)}
          >
            <option value="" disabled={partitionRequired}>
              {partitionRequired ? t("请选择数据分区") : t("整个磁盘")}
            </option>
            {disk.partitions
              .filter((p) => p.number != null)
              .map((p) => (
                <option key={p.id} value={p.number}>
                  {t("分区 {number}", { number: p.number! })} ·{" "}
                  {partitionTitle(p, catalogText)} · {formatSize(p.size)}
                </option>
              ))}
          </select>
        </label>
        {!partitionRequired && (
          <div className="maintenance-grid">
            <label>
              {t("起始扇区（相对目标）")}
              <input
                value={start}
                inputMode="numeric"
                onChange={(e) => {
                  invalidate();
                  setStart(e.target.value);
                }}
              />
            </label>
            <label>
              {t("扇区数量")}
              <input
                value={count}
                inputMode="numeric"
                onChange={(e) => {
                  invalidate();
                  setCount(e.target.value);
                }}
              />
            </label>
          </div>
        )}
        {mode === "benchmark" && (
          <>
            {fileBenchmark && (
              <div className="maintenance-grid">
                <label>
                  {t("测试大小（MiB）")}
                  <input
                    value={fileSize}
                    inputMode="numeric"
                    onChange={(e) => {
                      invalidate();
                      setFileSize(e.target.value);
                    }}
                  />
                </label>
                <label>
                  {t("测试轮数")}
                  <select
                    value={rounds}
                    onChange={(e) => {
                      invalidate();
                      setRounds(Number(e.target.value));
                    }}
                  >
                    {[1, 2, 3, 4, 5].map((round) => (
                      <option key={round}>{round}</option>
                    ))}
                  </select>
                </label>
                {fileWrites && (
                  <label>
                    {t("测试数据")}
                    <select
                      value={zeroFill ? "zero" : "random"}
                      onChange={(e) => {
                        invalidate();
                        setZeroFill(e.target.value === "zero");
                      }}
                    >
                      <option value="random">{t("随机")}</option>
                      <option value="zero">{t("全零")}</option>
                    </select>
                  </label>
                )}
              </div>
            )}
            {fileBenchmark && (
              <div className="maintenance-workloads">
                <label>
                  {t("配置负载")}
                  <select
                    value={workloadIndex}
                    onChange={(e) => setWorkloadIndex(Number(e.target.value))}
                  >
                    {fileWorkloads.map((workload, index) => (
                      <option key={index} value={index}>
                        {t("负载 {number}", { number: index + 1 })} ·{" "}
                        {t(workloadModeLabel[workload.mode])}
                      </option>
                    ))}
                  </select>
                </label>
                <button
                  type="button"
                  className="button"
                  disabled={fileWorkloads.length >= 4}
                  onClick={() => {
                    invalidate();
                    setFileWorkloads([
                      ...fileWorkloads,
                      {
                        ...initialWorkload,
                        access: "random",
                        blockSize: 4096,
                        queueDepth: "4",
                        limit: "duration",
                      },
                    ]);
                    setWorkloadIndex(fileWorkloads.length);
                  }}
                >
                  {t("添加负载")}
                </button>
                {fileWorkloads.length > 1 && (
                  <button
                    type="button"
                    className="button"
                    onClick={() => {
                      invalidate();
                      setFileWorkloads(
                        fileWorkloads.filter(
                          (_, index) => index !== workloadIndex,
                        ),
                      );
                      setWorkloadIndex(
                        Math.min(workloadIndex, fileWorkloads.length - 2),
                      );
                    }}
                  >
                    {t("移除负载")}
                  </button>
                )}
              </div>
            )}
            <div className="maintenance-grid">
              <label>
                {t("测速方式")}
                <select
                  value={form.mode}
                  onChange={(e) =>
                    updateWorkload({ mode: e.target.value as BenchmarkMode })
                  }
                >
                  <option value="read">{t("读取测速")}</option>
                  <option value="write">{t("写入测速")}</option>
                  <option value="mixed">{t("混合读写")}</option>
                </select>
              </label>
              <label>
                {t("访问方式")}
                <select
                  value={form.access}
                  onChange={(e) =>
                    updateWorkload({
                      access: e.target.value as BenchmarkForm["access"],
                    })
                  }
                >
                  <option value="sequential">{t("顺序")}</option>
                  <option value="random">{t("随机")}</option>
                </select>
              </label>
              <label>
                {t("块大小")}
                <select
                  value={form.blockSize}
                  onChange={(e) =>
                    updateWorkload({ blockSize: Number(e.target.value) })
                  }
                >
                  {[4, 8, 16, 32, 64, 128, 256, 512].map((kib) => (
                    <option key={kib} value={kib * 1024}>
                      {kib} KiB
                    </option>
                  ))}
                  <option value={1048576}>1 MiB</option>
                  {fileBenchmark &&
                    [2, 4, 8].map((mib) => (
                      <option key={mib} value={mib * 1048576}>
                        {mib} MiB
                      </option>
                    ))}
                </select>
              </label>
              {fileBenchmark && (
                <label>
                  {t("测速停止条件")}
                  <select
                    value={form.limit}
                    onChange={(e) =>
                      updateWorkload({
                        limit: e.target.value as BenchmarkForm["limit"],
                      })
                    }
                  >
                    <option value="requests">{t("请求次数")}</option>
                    <option value="duration">{t("按时长")}</option>
                  </select>
                </label>
              )}
              <label>
                {modeIsTimed ? t("测量时长（秒）") : t("请求次数")}
                <input
                  inputMode="numeric"
                  value={modeIsTimed ? form.duration : form.operations}
                  onChange={(e) =>
                    updateWorkload(
                      modeIsTimed
                        ? { duration: e.target.value }
                        : { operations: e.target.value },
                    )
                  }
                />
              </label>
              {form.mode === "read" && (
                <label>
                  {t("队列深度")}
                  <input
                    inputMode="numeric"
                    value={form.queueDepth}
                    onChange={(e) =>
                      updateWorkload({ queueDepth: e.target.value })
                    }
                  />
                </label>
              )}
              {fileBenchmark && form.mode === "read" && (
                <label>
                  {t("读取线程数")}
                  <input
                    inputMode="numeric"
                    value={form.threadCount ?? "1"}
                    onChange={(e) =>
                      updateWorkload({ threadCount: e.target.value })
                    }
                  />
                </label>
              )}
              {form.mode === "mixed" && (
                <label>
                  {t("读取比例（%）")}
                  <input
                    inputMode="numeric"
                    value={form.readPercent}
                    onChange={(e) =>
                      updateWorkload({ readPercent: e.target.value })
                    }
                  />
                </label>
              )}
            </div>
            <p className="maintenance-note">
              {fileBenchmark
                ? fileWrites
                  ? t("预览时选择该分区内用于新建临时文件的目录。")
                  : t("预览时选择该分区内的现有文件。")
                : form.mode === "read"
                  ? t("使用 1–32 个并发读取请求，绕过 Windows 缓存。")
                  : t(
                      "含写入的测速使用 QD1，会覆盖所选范围中的数据。每次写入刷新后单独回读校验。",
                    )}
            </p>
          </>
        )}
        {(mode === "wipe" || mode === "wipeFree") && (
          <div className="maintenance-grid">
            <label>
              {t("填充方式")}
              <select
                value={random ? "random" : "bytes"}
                onChange={(e) => {
                  invalidate();
                  setRandom(e.target.value === "random");
                }}
              >
                <option value="bytes">{t("自定义十六进制")}</option>
                <option value="random">{t("随机数据")}</option>
              </select>
            </label>
            {!random && (
              <label>
                {t("填充值")}
                <input
                  value={hex}
                  maxLength={95}
                  onChange={(e) => {
                    invalidate();
                    setHex(e.target.value);
                  }}
                  placeholder="00"
                />
              </label>
            )}
            <label>
              {t("覆写次数")}
              <select
                value={passes}
                onChange={(e) => {
                  invalidate();
                  setPasses(Number(e.target.value));
                }}
              >
                {[1, 2, 3, 4, 5, 6, 7].map((n) => (
                  <option key={n} value={n}>
                    {n.toLocaleString(language)}
                  </option>
                ))}
              </select>
            </label>
          </div>
        )}
        {mode === "wipeFree" && (
          <p className="maintenance-note">
            {t(
              "支持普通 NTFS、FAT12/16/32、exFAT 数据分区。只覆写确认空闲的簇； 已用数据和文件系统元数据会做前后摘要比对。执行后整盘保持脱机。",
            )}
          </p>
        )}
        {mode === "trim" && (
          <p className="maintenance-note">
            {t(
              "通知支持 TRIM 的设备回收 NTFS 卷的空闲空间。可能影响已删除文件的恢复；不等同于安全擦除。Windows 接手后不能中途取消。",
            )}
          </p>
        )}
      </fieldset>
      {unavailable && (
        <p className="form-error">
          {t("当前设备受保护或元数据不完整，不能执行所选操作。")}
        </p>
      )}
      <FormError message={catalogText(error)} />
      {preview && (
        <section
          ref={previewNode}
          className="maintenance-preview"
          aria-label={t("维护操作预览")}
        >
          <strong>{preview.target}</strong>
          <p>
            {catalogText(preview.title)} · {formatSize(preview.size)} ·{" "}
            {fileBenchmark
              ? t("从文件开头")
              : `${t("绝对起始 LBA")} ${(preview.offset / preview.sectorSize).toLocaleString(language)}`}
          </p>
          {fileBenchmark && (fileWorkloads.length > 1 || rounds > 1) && (
            <p>
              {t("{workloads} 组负载 · {rounds} 轮", {
                workloads: fileWorkloads.length,
                rounds,
              })}{" "}
              ·{" "}
              {fileWorkloads
                .map((workload) => t(workloadModeLabel[workload.mode]))
                .join(" / ")}
            </p>
          )}
          <p>{catalogText(preview.warning)}</p>
          {preview.requiresAcknowledgment && (
            <label className="maintenance-check">
              <input
                type="checkbox"
                checked={ack}
                onChange={(e) => setAck(e.target.checked)}
                disabled={busy}
              />
              {t("我已核对目标和范围，并了解数据影响")}
            </label>
          )}
        </section>
      )}
      {busy && (
        <div className="maintenance-progress" role="status">
          <LoaderCircle className="spin" size={18} />
          <span>
            {stopping
              ? t("正在停止…")
              : report?.state === "running"
                ? catalogText(report.phase)
                : job
                  ? catalogText(job.phase)
                  : t("正在核对设备…")}
          </span>
          {job && job.total > 0 && (
            <progress value={job.done} max={job.total} />
          )}
        </div>
      )}
      {report && (
        <section
          ref={resultNode}
          className="maintenance-result"
          aria-label={t("维护结果")}
        >
          <strong>
            {report.state === "completed"
              ? t("已完成")
              : report.state === "running"
                ? t("进行中")
                : report.state === "canceled"
                  ? t("已取消")
                  : t("未完成")}{" "}
            · {catalogText(report.title)}
          </strong>
          <p>
            {report.target} · {formatSize(report.size)}
          </p>
          {runs.length > 0 &&
            report.operation.kind === "fileBenchmark" &&
            (reportWorkloads.length > 1 ||
              (report.operation.repetitions ?? 1) > 1) && (
              <div className="maintenance-benchmark-runs">
                <label>
                  {t("结果单位")}
                  <select
                    value={resultUnit}
                    onChange={(e) =>
                      setResultUnit(e.target.value as BenchmarkUnit)
                    }
                  >
                    {(["MiB/s", "MB/s", "IOPS"] as const).map((unit) => (
                      <option key={unit}>{unit}</option>
                    ))}
                  </select>
                </label>
                <table>
                  <thead>
                    <tr>
                      <th>{t("配置负载")}</th>
                      <th>{t("读取")}</th>
                      <th>{t("写入")}</th>
                      <th>{t("测量时长（秒）")}</th>
                      <th>{t("状态")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {runs.map((run, index) => {
                      const options = reportWorkloads[run.workload - 1]!;
                      const rates = benchmarkRates(run.stats, resultUnit);
                      const rate = (value: number | undefined) =>
                        value?.toLocaleString(language, {
                          maximumFractionDigits: resultUnit === "IOPS" ? 0 : 2,
                        }) ?? "—";
                      return (
                        <tr
                          key={`${run.round}:${run.workload}`}
                          className={
                            index === selectedRunIndex ? "selected" : undefined
                          }
                        >
                          <td>
                            <button
                              type="button"
                              aria-pressed={index === selectedRunIndex}
                              onClick={() => setResultIndex(index)}
                            >
                              {t("第 {round} 轮 · 负载 {workload}", {
                                round: run.round,
                                workload: run.workload,
                              })}
                              <small>
                                {t(workloadModeLabel[workloadMode(options)])} ·{" "}
                                {t(
                                  options.access === "random" ? "随机" : "顺序",
                                )}{" "}
                                · {formatSize(options.blockSize)} · QD
                                {options.queueDepth}
                                {options.threadCount != null &&
                                  ` · T${options.threadCount}`}
                              </small>
                            </button>
                          </td>
                          <td>{rate(rates?.[0])}</td>
                          <td>{rate(rates?.[1])}</td>
                          <td>
                            {run.stats
                              ? (run.stats.elapsedMs / 1000).toLocaleString(
                                  language,
                                  { maximumFractionDigits: 3 },
                                )
                              : "—"}
                          </td>
                          <td>
                            {t(
                              run.state === "completed"
                                ? "已完成"
                                : run.state === "running"
                                  ? "进行中"
                                  : run.state === "canceled"
                                    ? "已取消"
                                    : "未完成",
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          {stats && workload && (
            <>
              <div className="maintenance-metrics">
                <div>
                  <strong>
                    {stats.mibPerSecond.toLocaleString(language, {
                      minimumFractionDigits: 2,
                      maximumFractionDigits: 2,
                    })}
                  </strong>
                  <span>MiB/s</span>
                </div>
                <div>
                  <strong>
                    {stats.iops.toLocaleString(language, {
                      maximumFractionDigits: 0,
                    })}
                  </strong>
                  <span>IOPS</span>
                </div>
                <div>
                  <strong>{stats.operations.toLocaleString(language)}</strong>
                  <span>{t("完成请求")}</span>
                </div>
              </div>
              {(selectedRun?.state ?? report.state) !== "running" &&
                stats.operations > 0 && (
                  <p>
                    {t(
                      "P50 {p50} μs · P95 {p95} μs · 最短 {min} μs · 最长 {max} μs",
                      {
                        p50: stats.p50Microseconds.toLocaleString(language),
                        p95: stats.p95Microseconds.toLocaleString(language),
                        min: stats.minMicroseconds.toLocaleString(language),
                        max: stats.maxMicroseconds.toLocaleString(language),
                      },
                    )}
                    {stats.approximatePercentiles && (
                      <> · {t("P50 和 P95 为估算值")}</>
                    )}
                  </p>
                )}
              <p>
                {t("队列深度 {depth} · 峰值并发 {peak}", {
                  depth: workload.queueDepth.toLocaleString(language),
                  peak: stats.peakQueueDepth.toLocaleString(language),
                })}
                {report.operation.kind === "fileBenchmark" &&
                  !!stats.workerThreads && (
                    <>
                      {" "}
                      ·{" "}
                      {t("线程 {count}", {
                        count: stats.workerThreads.toLocaleString(language),
                      })}
                    </>
                  )}
              </p>
              <p>
                {t(
                  "读取 {read}（{reads} 次） · 写入 {write}（{writes} 次） · 校验回读 {verified}",
                  {
                    read: formatSize(stats.readBytes),
                    reads: stats.readOperations.toLocaleString(language),
                    write: formatSize(stats.writeBytes),
                    writes: stats.writeOperations.toLocaleString(language),
                    verified: formatSize(stats.verifiedBytes),
                  },
                )}
              </p>
              <ThroughputChart
                samples={stats.samples}
                showRead={!workload.write || workload.readPercent != null}
                showWrite={workload.write}
              />
            </>
          )}
          {wiped && (
            <>
              <p>
                {t(
                  "已写入 {written} · 已回读 {readBack} · 已通过摘要核验的轮数 {passes}",
                  {
                    written: formatSize(wiped.writtenBytes),
                    readBack: formatSize(wiped.readbackBytes),
                    passes: wiped.verifiedPasses.toLocaleString(language),
                  },
                )}
              </p>
              {wiped.passSha256.length > 0 && (
                <details>
                  <summary>{t("查看每轮 SHA-256")}</summary>
                  {wiped.passSha256.map((s, i) => (
                    <code key={i}>
                      {i + 1}. {s}
                    </code>
                  ))}
                </details>
              )}
            </>
          )}
          {report.freeSpace && (
            <>
              <p>
                {report.freeSpace.filesystem} ·{" "}
                {t("确认空闲 {free} · 保留区域 {preserved}", {
                  free: formatSize(report.freeSpace.freeBytes),
                  preserved: formatSize(report.freeSpace.preservedBytes),
                })}
              </p>
              <p>
                {report.freeSpace.preservedSha256
                  ? t("全部保留区域写前/写后 SHA-256 一致")
                  : t("保留区域写后核验尚未完成")}
              </p>
              <details>
                <summary>{t("查看分配信息与保留区域摘要")}</summary>
                <code>
                  {t("分配信息")} · {report.freeSpace.allocationSha256}
                </code>
                {report.freeSpace.preservedSha256 && (
                  <code>
                    {t("保留区域")} · {report.freeSpace.preservedSha256}
                  </code>
                )}
              </details>
            </>
          )}
          {report.message && (
            <p className={report.state === "failed" ? "form-error" : ""}>
              {catalogText(report.message)}
            </p>
          )}
        </section>
      )}
      {saved && (
        <p className="maintenance-note">
          {t("已保存报告到 {path}", { path: saved })}
        </p>
      )}
    </Dialog>
  );
}

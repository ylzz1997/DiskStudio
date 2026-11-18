import { useEffect, useRef, useState } from "react";
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
  maintenanceRange,
  maintenanceTarget,
  wipePattern,
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
  const [write, setWrite] = useState(false),
    [access, setAccess] = useState<"sequential" | "random">("sequential");
  const [block, setBlock] = useState(1048576),
    [operations, setOperations] = useState("256");
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
  const modifies = mode !== "benchmark" || write;
  const unavailable =
    !disk.hardware?.identity ||
    !disk.hardware.layoutKey ||
    (modifies &&
      (disk.hardware.isSystem || disk.hardware.deviceReadOnly !== false));
  async function run(execute: boolean) {
    if (
      phase.current !== "idle" ||
      unavailable ||
      (execute && (!token.current || (modifies && !ack)))
    )
      return;
    setError("");
    setSaved("");
    setStopping(false);
    setJob(null);
    let refresh = false;
    try {
      const target = maintenanceTarget(disk, partition);
      const range = maintenanceRange(
        start,
        count,
        target.size,
        disk.sectorSize,
      );
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
        const n = Number(operations);
        if (
          !/^\d+$/.test(operations) ||
          !Number.isSafeInteger(n) ||
          n < 1 ||
          n > 65536
        )
          throw Error(t("请求次数须为 1–65536。"));
        if (
          (target.offset + range.startSector * disk.sectorSize) % 4096 !== 0 ||
          (range.sectorCount * disk.sectorSize) % 4096 !== 0 ||
          range.sectorCount * disk.sectorSize < block
        )
          throw Error(t("测速范围须按 4 KiB 对齐且至少容纳一个测量块。"));
        operation = {
          kind: "benchmark",
          options: { access, blockSize: block, operations: n, write },
        };
      }
      phase.current = execute ? "execute" : "preview";
      setBusy(true);
      onBusy(true);
      setReport(null);
      if (execute) {
        const once = token.current;
        token.current = null;
        setPreview(null);
        refresh = modifies;
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
  const stats = report?.benchmark,
    wiped = report?.wipe;
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
            disabled={busy || !!unavailable || (!!preview && modifies && !ack)}
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
        <label>
          {t("目标")}
          <select
            value={partition}
            onChange={(e) => choosePartition(e.target.value)}
          >
            <option value="" disabled={mode === "trim" || mode === "wipeFree"}>
              {mode === "trim" || mode === "wipeFree"
                ? t("请选择数据分区")
                : t("整个磁盘")}
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
        {mode !== "trim" && mode !== "wipeFree" && (
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
            <div className="maintenance-grid">
              <label>
                {t("访问方式")}
                <select
                  value={access}
                  onChange={(e) => {
                    invalidate();
                    setAccess(e.target.value as typeof access);
                  }}
                >
                  <option value="sequential">{t("顺序")}</option>
                  <option value="random">{t("随机")}</option>
                </select>
              </label>
              <label>
                {t("块大小")}
                <select
                  value={block}
                  onChange={(e) => {
                    invalidate();
                    setBlock(Number(e.target.value));
                  }}
                >
                  <option value={4096}>4 KiB</option>
                  <option value={65536}>64 KiB</option>
                  <option value={1048576}>1 MiB</option>
                </select>
              </label>
              <label>
                {t("请求次数")}
                <input
                  inputMode="numeric"
                  value={operations}
                  onChange={(e) => {
                    invalidate();
                    setOperations(e.target.value);
                  }}
                />
              </label>
            </div>
            <label className="maintenance-check">
              <input
                type="checkbox"
                checked={write}
                onChange={(e) => {
                  invalidate();
                  setWrite(e.target.checked);
                }}
              />
              {t("写入测速（会覆盖所选范围中的数据）")}
            </label>
            <p className="maintenance-note">
              {t(
                "单队列深度 QD1 · 绕过 Windows 缓存。写入耗时包含刷新，校验回读单独统计。",
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
            {t("绝对起始 LBA")}{" "}
            {(preview.offset / preview.sectorSize).toLocaleString(language)}
          </p>
          <p>{catalogText(preview.warning)}</p>
          {modifies && (
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
          {stats && (
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
              </p>
              <p>
                {t("传输 {transferred} · 校验回读 {verified}", {
                  transferred: formatSize(stats.transferredBytes),
                  verified: formatSize(stats.verifiedBytes),
                })}
              </p>
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

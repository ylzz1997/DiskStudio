import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { HardDrive, LoaderCircle, StopCircle } from "lucide-react";
import { Dialog, FormError } from "./components";
import {
  conversionFormats,
  type ImageOutputFormat,
  type ImageConversionReport,
} from "./imageConversion";
import {
  creationOptions,
  tableLabels,
  type CapacityUnit,
  type ImageTable,
} from "./imageCreation";
import { formatSize } from "./types";

export function ImageCreationDialog({
  onClose,
  onBusy,
}: {
  onClose: () => void;
  onBusy: (busy: boolean) => void;
}) {
  const [format, setFormat] = useState<ImageOutputFormat>("vhdxDynamic");
  const [capacity, setCapacity] = useState("8"),
    [unit, setUnit] = useState<CapacityUnit>("GiB"),
    [preset, setPreset] = useState("8GiB");
  const [sector, setSector] = useState(512),
    [table, setTable] = useState<ImageTable>("gpt");
  const [busy, setBusy] = useState(false),
    [stopping, setStopping] = useState(false),
    [error, setError] = useState("");
  const [report, setReport] = useState<ImageConversionReport | null>(null);
  const job = useRef<string | null>(null),
    alive = useRef(true),
    working = useRef(false),
    progress = useRef<HTMLElement>(null);
  const busyCallback = useRef(onBusy);
  busyCallback.current = onBusy;
  function activity(value: boolean) {
    working.current = value;
    setBusy(value);
    busyCallback.current(value);
  }
  function reset() {
    setReport(null);
    setError("");
  }
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (job.current)
        void invoke("image_conversion_cancel", { id: job.current }).catch(
          () => {},
        );
    };
  }, []);
  useEffect(() => {
    if (!busy) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const current = await invoke<ImageConversionReport | null>(
          "image_conversion_status",
        );
        if (!disposed && current?.id === job.current && current?.creation) {
          setReport(current);
          if (["completed", "canceled", "failed"].includes(current.state)) {
            job.current = null;
            working.current = false;
            setBusy(false);
            setStopping(false);
            busyCallback.current(false);
            return;
          }
        }
      } catch (e) {
        if (!disposed)
          setError(`暂时无法读取任务状态：${String(e)}。请等待状态恢复。`);
      }
      if (!disposed) timer = setTimeout(poll, 350);
    }
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [busy]);
  useEffect(() => {
    if (["completed", "failed", "canceled"].includes(report?.state ?? ""))
      progress.current?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [report?.state]);
  let options: ReturnType<typeof creationOptions> | undefined,
    invalid = "";
  try {
    options = creationOptions(capacity, unit, sector, format, table);
  } catch (e) {
    invalid = e instanceof Error ? e.message : String(e);
  }
  async function start() {
    if (!options || working.current) return;
    activity(true);
    setStopping(false);
    reset();
    job.current = null;
    try {
      const created = await invoke<ImageConversionReport | null>(
        "image_creation_begin",
        { options },
      );
      if (!alive.current) {
        if (created)
          await invoke("image_conversion_cancel", { id: created.id });
        return;
      }
      if (!created) {
        activity(false);
        return;
      }
      job.current = created.id;
      setReport(created);
    } catch (e) {
      if (alive.current) {
        setError(e instanceof Error ? e.message : String(e));
        activity(false);
      }
    }
  }
  async function stop() {
    if (!job.current) return;
    setStopping(true);
    try {
      await invoke("image_conversion_cancel", { id: job.current });
    } catch (e) {
      setStopping(false);
      setError(String(e));
    }
  }
  const phase =
    report?.state === "checkingSource"
      ? "复核生成内容"
      : report?.state === "verifying"
        ? "回读目标镜像"
        : report?.state === "completed"
          ? "创建完成"
          : report?.state === "canceled"
            ? "创建已停止"
            : report?.state === "failed"
              ? "创建未完成"
              : "生成镜像";
  return (
    <Dialog
      title="创建虚拟磁盘"
      subtitle="选择容量与格式，保存为新的镜像文件"
      onClose={() => {
        if (!working.current) onClose();
      }}
      footer={
        <>
          {busy ? (
            <button
              className="button"
              disabled={stopping || !report}
              onClick={() => void stop()}
            >
              <StopCircle size={16} />
              {stopping ? "正在停止…" : "停止创建"}
            </button>
          ) : (
            <button
              className="button primary"
              disabled={!options}
              onClick={() => void start()}
            >
              <HardDrive size={16} />
              选择保存位置并创建
            </button>
          )}
          <button className="button" disabled={busy} onClick={onClose}>
            关闭
          </button>
        </>
      }
    >
      <fieldset className="maintenance-fields" disabled={busy}>
        <label>
          镜像格式
          <select
            value={format}
            onChange={(e) => {
              setFormat(e.target.value as ImageOutputFormat);
              reset();
            }}
          >
            {conversionFormats.map((f) => (
              <option key={f.value} value={f.value}>
                {f.label}
              </option>
            ))}
          </select>
        </label>
        <p className="image-conversion-note">
          {conversionFormats.find((f) => f.value === format)?.detail}
        </p>
        <label>
          容量
          <select
            value={preset}
            onChange={(e) => {
              const value = e.target.value;
              setPreset(value);
              if (value !== "custom") {
                setCapacity(
                  value === "64MiB" ? "64" : value.replace("GiB", ""),
                );
                setUnit(value === "64MiB" ? "MiB" : "GiB");
              }
              reset();
            }}
          >
            <option value="64MiB">64 MiB</option>
            <option value="1GiB">1 GiB</option>
            <option value="8GiB">8 GiB</option>
            <option value="64GiB">64 GiB</option>
            <option value="custom">自定义…</option>
          </select>
        </label>
        {preset === "custom" && (
          <div className="image-creation-capacity">
            <label>
              容量数值
              <input
                inputMode="decimal"
                value={capacity}
                onChange={(e) => {
                  setCapacity(e.target.value);
                  reset();
                }}
              />
            </label>
            <label>
              单位
              <select
                value={unit}
                onChange={(e) => {
                  setUnit(e.target.value as CapacityUnit);
                  reset();
                }}
              >
                <option>MiB</option>
                <option>GiB</option>
                <option>TiB</option>
              </select>
            </label>
          </div>
        )}
        <label>
          初始布局
          <select
            value={table}
            onChange={(e) => {
              setTable(e.target.value as ImageTable);
              reset();
            }}
          >
            {Object.entries(tableLabels).map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <label>
          逻辑扇区
          <select
            value={sector}
            onChange={(e) => {
              setSector(Number(e.target.value));
              reset();
            }}
          >
            <option value={512}>512 B</option>
            <option value={4096}>4 KiB</option>
          </select>
        </label>
        <p className="image-conversion-note">
          {table === "none"
            ? "逻辑内容全部为零，不写入分区表。"
            : "创建空分区表，尚无分区或文件系统。"}{" "}
          镜像不会自动挂载；完成前会完整回读校验。
        </p>
      </fieldset>
      <FormError message={invalid || error} />
      {(busy || report) && (
        <section
          ref={progress}
          className="maintenance-progress"
          aria-live="polite"
        >
          <h3>
            {busy && <LoaderCircle size={16} className="spin" />}
            {phase}
          </h3>
          {report && (
            <>
              <progress
                value={
                  report.received + report.sourceVerified + report.verified
                }
                max={report.options.size * 3}
              />
              <p>
                写入 {formatSize(report.received)} · 内容复核{" "}
                {formatSize(report.sourceVerified)} · 目标回读{" "}
                {formatSize(report.verified)}
              </p>
            </>
          )}
          <p>{report?.message ?? "请选择新文件的保存位置…"}</p>
          {report?.result && (
            <dl className="detail-list">
              <dt>输出文件</dt>
              <dd>{report.outputName}</dd>
              <dt>初始布局</dt>
              <dd>{tableLabels[report.creation!.table]}</dd>
              <dt>逻辑容量</dt>
              <dd>{formatSize(report.result.logicalBytes)}</dd>
              <dt>文件长度</dt>
              <dd>{formatSize(report.result.fileBytes)}</dd>
              <dt>逻辑扇区</dt>
              <dd>{report.options.sector} B</dd>
              {report.creation?.diskId && (
                <>
                  <dt>磁盘标识</dt>
                  <dd className="image-conversion-hash">
                    {report.creation.diskId}
                  </dd>
                </>
              )}
              <dt>逻辑 SHA-256</dt>
              <dd className="image-conversion-hash">{report.result.sha256}</dd>
            </dl>
          )}
          {report && !report.result && (
            <small>
              未完成文件：{report.partialName}
              。停止或失败时保留，不会发布为成功镜像。
            </small>
          )}
        </section>
      )}
    </Dialog>
  );
}

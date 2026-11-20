import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { errorText } from "./partitionOutcome";
import type { SectorSelection } from "./sectorModel";
import { surfaceRange } from "./surfaceModel";
import { formatSize, type DataPlan, type Disk } from "./types";

type CopyJob = {
  id: string;
  state: "running" | "completed" | "failed" | "canceled";
  phase: string;
  done: number;
  total: number;
  message: string;
  sha256: string | null;
};

type CopyDirection = "forward" | "backward";
type ErrorPolicy = "stop" | "fill" | "defer";
type CopySource =
  | { kind: "disk"; diskId: string; select: () => SectorSelection }
  | { kind: "image"; id: string; partition: number | null; sourceDisks: string[] };
type ResumedFile = {
  id: string;
  options: { startSector: number; sectorCount: number };
  direction: CopyDirection;
  errors:
    | { kind: "stop" }
    | { kind: "fill"; skipSectors: number; marker: string; reread?: boolean }
    | { kind: "defer"; marker: string };
};
type CopyReport = {
  destinationStartLba?: number;
  sectorCount: number;
  sectorSize: number;
  failures: {
    filledSectors: number;
    unreadableSectors: number;
    omitted: number;
    reread?: { queuedSectors: number; recoveredSectors: number };
    ranges: {
      failedLba: number;
      firstFilledLba: number;
      filledSectors: number;
      windowsCode: number;
    }[];
  };
};

export function SectorCopyPanel({
  source,
  disks,
  size,
  sectorSize,
  offset,
  initialSector,
  onBusy,
  onRefresh,
}: {
  source: CopySource;
  disks: Disk[];
  size: number;
  sectorSize: number;
  offset: number;
  initialSector: number;
  onBusy: (value: boolean) => void;
  onRefresh: () => Promise<unknown>;
}) {
  const { t, language, catalogText } = useLocale();
  const integer = (value: number) => value.toLocaleString(language);
  const [start, setStart] = useState(String(initialSector));
  const [count, setCount] = useState("1");
  const [direction, setDirection] = useState<CopyDirection>("forward");
  const [errorPolicy, setErrorPolicy] = useState<ErrorPolicy>("stop");
  const [skip, setSkip] = useState("0");
  const [reread, setReread] = useState(false);
  const [marker, setMarker] = useState("");
  const [output, setOutput] = useState<"file" | "disk">("file");
  const [keepCheckpoint, setKeepCheckpoint] = useState(false);
  const [destinationId, setDestinationId] = useState("");
  const [destinationPartition, setDestinationPartition] = useState("");
  const [destinationStart, setDestinationStart] = useState("");
  const [plan, setPlan] = useState<DataPlan | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [choosing, setChoosing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [id, setId] = useState<string | null>(null);
  const [job, setJob] = useState<CopyJob | null>(null);
  const [report, setReport] = useState<CopyReport | null>(null);
  const [savedReport, setSavedReport] = useState("");
  const [error, setError] = useState("");
  const busy = useRef(false);
  const submitting = useRef(false);
  const callbacks = useRef({ onBusy, onRefresh });
  useEffect(() => {
    callbacks.current = { onBusy, onRefresh };
  }, [onBusy, onRefresh]);

  let options: ReturnType<typeof surfaceRange> | undefined;
  let rangeError = "";
  try {
    options = surfaceRange(start, count, size, sectorSize);
  } catch (err) {
    rangeError = errorText(err).trim();
  }
  const running = id !== null;
  const controlsDisabled = choosing || running || saving || plan !== null;
  const diskOutput = output === "disk";
  const imageSource = source.kind === "image";
  const destinations = disks.filter(
    (candidate) =>
      candidate.kind === "physical" &&
      candidate.sectorSize === sectorSize &&
      ["GPT", "RAW"].includes(candidate.scheme) &&
      candidate.hardware?.layoutAvailable &&
      candidate.hardware?.identity &&
      (source.kind === "disk"
        ? candidate.id !== source.diskId
        : !source.sourceDisks.includes(candidate.hardware.identity)) &&
      candidate.hardware.layoutKey &&
      candidate.hardware.deviceReadOnly === false &&
      !candidate.hardware.isSystem,
  );
  const destination = destinations.find(
    (candidate) => candidate.id === destinationId,
  );
  const destinationParts =
    destination?.partitions.filter(
      (part) =>
        part.number !== undefined &&
        part.role === "数据分区" &&
        !part.system &&
        !part.hidden,
    ) ?? [];
  const selectedDestinationPart = destinationParts.find(
    (part) => String(part.number) === destinationPartition,
  );
  const destinationSize =
    selectedDestinationPart?.size ?? destination?.size ?? 0;
  let destinationOptions: ReturnType<typeof surfaceRange> | undefined;
  let destinationError = "";
  if (
    diskOutput &&
    destination &&
    destinationStart &&
    (!destinationPartition || selectedDestinationPart)
  ) {
    try {
      destinationOptions = surfaceRange(
        destinationStart,
        count,
        destinationSize,
        sectorSize,
      );
    } catch (err) {
      destinationError = errorText(err).trim();
    }
  }
  const skipCount = Number(skip);
  const policyError =
    errorPolicy === "stop"
      ? ""
      : errorPolicy === "fill" &&
          (!/^\d+$/.test(skip) || !Number.isSafeInteger(skipCount))
        ? "请输入有效的跳过扇区数。"
        : new TextEncoder().encode(marker).length > 128
          ? "填充文本最多 128 个 UTF-8 字节。"
          : "";

  useEffect(() => {
    if (!id) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function poll() {
      try {
        const next = await invoke<CopyJob | null>("data_status");
        if (!active) return;
        if (next?.id === id) {
          setJob(next);
          if (next.state === "running") {
            timer = setTimeout(() => void poll(), 400);
            return;
          }
          if (next.state === "completed") {
            try {
              const result = await invoke<CopyReport | null>(
                "sector_copy_report",
                { id },
              );
              if (active) setReport(result);
            } catch (err) {
              if (active) setError(errorText(err));
            }
          }
          if (diskOutput) await callbacks.current.onRefresh();
        } else {
          setError("任务已结束。");
        }
        if (!active) return;
        setId(null);
        busy.current = false;
        callbacks.current.onBusy(false);
      } catch (err) {
        if (!active) return;
        setError(errorText(err));
        timer = setTimeout(() => void poll(), 400);
      }
    }
    void poll();
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [diskOutput, id]);

  function selectDestination(): SectorSelection {
    if (!destination?.hardware?.identity || !destination.hardware.layoutKey ||
        (destinationPartition && !selectedDestinationPart)) {
      throw Error(t("请刷新磁盘信息后重试。"));
    }
    return {
      target: destination.hardware.identity,
      layoutKey: destination.hardware.layoutKey,
      partition: selectedDestinationPart?.number ?? null,
    };
  }

  async function begin(action: "copy" | "resume" | "reread" | "legacy" = "copy") {
    const useRecord = action !== "copy";
    const rereadFilled = action === "reread";
    const legacy = action === "legacy";
    if ((useRecord ? imageSource : !options || policyError) || busy.current) return;
    busy.current = true;
    onBusy(true);
    setChoosing(true);
    setError("");
    setJob(null);
    setReport(null);
    setSavedReport("");
    try {
      if (useRecord && source.kind === "disk" && diskOutput) {
        const preview = await invoke<DataPlan | null>("data_prepare", {
          request: legacy
            ? { type: "sectorCopyRereadReport", source: source.select(), destination: selectDestination(), keepCheckpoint }
            : { type: "sectorCopyResume", source: source.select(), rereadFilled },
        });
        if (preview) {
          setConfirmed(false);
          setPlan(preview);
          return;
        }
      } else if (useRecord && source.kind === "disk") {
        const resumed = legacy
          ? await invoke<ResumedFile | null>("sector_copy_reread_report", { source: source.select() })
          : await invoke<ResumedFile | null>("sector_copy_resume_file", { source: source.select(), rereadFilled });
        if (resumed) {
          setStart(String(resumed.options.startSector));
          setCount(String(resumed.options.sectorCount));
          setDirection(resumed.direction);
          setErrorPolicy(resumed.errors.kind);
          setSkip(resumed.errors.kind === "fill" ? String(resumed.errors.skipSectors) : "0");
          setReread(resumed.errors.kind === "fill" && Boolean(resumed.errors.reread));
          setMarker(resumed.errors.kind === "stop" ? "" : resumed.errors.marker);
          setId(resumed.id);
          return;
        }
      } else if (options) {
        const copy = source.kind === "image"
          ? { id: source.id, partition: source.partition, options, direction }
          : {
              source: source.select(),
              options,
              direction,
              errors:
                errorPolicy === "fill"
                  ? { kind: "fill", skipSectors: skipCount, marker, reread }
                  : errorPolicy === "defer"
                    ? { kind: "defer", marker }
                    : { kind: "stop" },
            };
        if (diskOutput) {
          if (!destinationOptions) {
            throw Error(t("请刷新磁盘信息后重试。"));
          }
          const preview = await invoke<DataPlan | null>("data_prepare", {
            request: {
              type: imageSource ? "imageSectorCopy" : "sectorCopy",
              copy,
              destination: selectDestination(),
              destinationStartSector: destinationOptions.startSector,
              ...(!imageSource && { keepCheckpoint }),
            },
          });
          if (preview) {
            setConfirmed(false);
            setPlan(preview);
            return;
          }
        } else {
          const taskId = await invoke<string | null>(
            imageSource ? "sector_copy_image_start" : "sector_copy_start",
            { request: copy },
          );
          if (taskId) {
            setId(taskId);
            return;
          }
        }
      }
    } catch (err) {
      setError(errorText(err));
    } finally {
      setChoosing(false);
    }
    busy.current = false;
    onBusy(false);
  }

  function discardPlan() {
    if (!plan || submitting.current) return;
    void invoke("data_discard_plan", { token: plan.token }).catch((err) =>
      setError(errorText(err)),
    );
    setPlan(null);
    setConfirmed(false);
    busy.current = false;
    onBusy(false);
  }

  async function executePlan() {
    if (!plan || !confirmed || submitting.current) return;
    submitting.current = true;
    setChoosing(true);
    setError("");
    try {
      setId(await invoke<string>("data_execute", { token: plan.token }));
    } catch (err) {
      setError(errorText(err));
      busy.current = false;
      onBusy(false);
    } finally {
      submitting.current = false;
      setPlan(null);
      setConfirmed(false);
      setChoosing(false);
    }
  }

  async function saveReport() {
    if (job?.state !== "completed" || busy.current) return;
    busy.current = true;
    onBusy(true);
    setSaving(true);
    setError("");
    try {
      const path = await invoke<string | null>("sector_copy_save_report", {
        id: job.id,
      });
      if (path) setSavedReport(path);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setSaving(false);
      busy.current = false;
      onBusy(false);
    }
  }

  function change<T>(setValue: (value: T) => void, value: T) {
    setValue(value);
    setJob(null);
    setReport(null);
    setSavedReport("");
    setError("");
  }

  const absoluteStart = offset / sectorSize + (options?.startSector ?? 0);
  return (
    <>
      <div className="sector-search-range">
        <label>
          {t("起始相对 LBA")}
          <input
            type="number"
            min={0}
            max={size / sectorSize - 1}
            value={start}
            disabled={controlsDisabled}
            onChange={(e) => change(setStart, e.target.value)}
          />
        </label>
        <label>
          {t("扇区数量")}
          <input
            type="number"
            min={1}
            max={size / sectorSize - (options?.startSector ?? 0)}
            value={count}
            disabled={controlsDisabled}
            onChange={(e) => change(setCount, e.target.value)}
          />
        </label>
      </div>
      <div className="sector-search-range">
        <label>
          {t("读取顺序")}
          <select
            value={direction}
            disabled={controlsDisabled}
            onChange={(e) =>
              change(setDirection, e.target.value as CopyDirection)
            }
          >
            <option value="forward">{t("从低 LBA 到高 LBA")}</option>
            <option value="backward">{t("从高 LBA 到低 LBA")}</option>
          </select>
        </label>
        {!imageSource && <label>
          {t("源扇区读取失败时")}
          <select
            value={errorPolicy}
            disabled={controlsDisabled}
            onChange={(e) =>
              change(setErrorPolicy, e.target.value as ErrorPolicy)
            }
          >
            <option value="stop">{t("停止复制")}</option>
            <option value="fill">{t("填充后继续")}</option>
            <option value="defer">{t("跳过大段后重读")}</option>
          </select>
        </label>}
      </div>
      {errorPolicy !== "stop" && (
        <>
          <div className="sector-search-range">
            {errorPolicy === "fill" && (
              <label>
                {t("额外跳过的扇区数")}
                <input
                  type="number"
                  min={0}
                  step={1}
                  value={skip}
                  disabled={controlsDisabled}
                  onChange={(e) => change(setSkip, e.target.value)}
                />
              </label>
            )}
            <label>
              {t("填充文本（留空填零）")}
              <input
                value={marker}
                disabled={controlsDisabled}
                onChange={(e) => change(setMarker, e.target.value)}
              />
            </label>
          </div>
          {errorPolicy === "fill" && (
            <label className="data-confirm">
              <input
                type="checkbox"
                checked={reread}
                disabled={controlsDisabled}
                onChange={(e) => change(setReread, e.target.checked)}
              />
              {t("复制其余扇区后重读跳过的范围")}
            </label>
          )}
          <p className="surface-note">
            {t(
              errorPolicy === "defer"
                ? "暂缓失败的读块，并沿读取方向跳过约八分之一的所选范围。重读后仅填充仍无法读取的扇区。"
                : reread
                  ? "重读后仅填充仍无法读取的扇区，其余扇区写入读回的内容。"
                  : "填充读取失败的扇区及按读取方向跳过的扇区，保留输出中的 LBA 位置。",
            )}
          </p>
        </>
      )}
      {diskOutput && !imageSource && (
        <label className="data-confirm">
          <input
            type="checkbox"
            checked={keepCheckpoint}
            disabled={controlsDisabled}
            onChange={(e) => change(setKeepCheckpoint, e.target.checked)}
          />
          {t("在第三块磁盘保存续作断点")}
        </label>
      )}
      <div className="sector-search-range">
        <label>
          {t("目标")}
          <select
            value={output}
            disabled={controlsDisabled}
            onChange={(e) =>
              change(setOutput, e.target.value === "disk" ? "disk" : "file")
            }
          >
            <option value="file">{t("输出文件")}</option>
            <option value="disk" disabled={!destinations.length}>
              {t("目标磁盘")}
            </option>
          </select>
        </label>
        {diskOutput && (
          <label>
            {t("目标磁盘")}
            <select
              value={destinationId}
              disabled={controlsDisabled}
              onChange={(e) => {
                setDestinationPartition("");
                setDestinationStart("");
                change(setDestinationId, e.target.value);
              }}
            >
              <option value="">{t("请选择磁盘")}</option>
              {destinations.map((candidate) => (
                <option key={candidate.id} value={candidate.id}>
                  {diskTitle(candidate, t)} · {candidate.hardware?.model} ·{" "}
                  {formatSize(candidate.size)}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      {diskOutput && (
        <div className="sector-search-range">
          <label>
            {t("目标范围")}
            <select
              value={destinationPartition}
              disabled={!destination || controlsDisabled}
              onChange={(e) => {
                setDestinationStart("");
                change(setDestinationPartition, e.target.value);
              }}
            >
              <option value="">{t("整个磁盘")}</option>
              {destinationParts.map((part) => (
                <option key={part.id} value={part.number}>
                  {t("分区 {partition}", { partition: part.number! })} ·{" "}
                  {partitionTitle(part, catalogText)} · {formatSize(part.size)}
                </option>
              ))}
            </select>
          </label>
          <label>
            {t("目标起始相对 LBA")}
            <input
              type="number"
              min={0}
              step={1}
              value={destinationStart}
              disabled={!destination || controlsDisabled}
              onChange={(e) => change(setDestinationStart, e.target.value)}
            />
          </label>
        </div>
      )}
      <div className="sector-search-range">
        {running ? (
          <button
            className="button"
            onClick={() =>
              void invoke("data_cancel", { id }).catch((err) =>
                setError(errorText(err)),
              )
            }
          >
            {t("取消任务")}
          </button>
        ) : (
          <button
            className="button primary"
            disabled={
              !options ||
              !!policyError ||
              controlsDisabled ||
              (diskOutput && !destinationOptions)
            }
            onClick={() => void begin()}
          >
            {t(diskOutput ? "预览操作" : "选择保存位置…")}
          </button>
        )}
      </div>
      {!running && !imageSource && (
        <div className="sector-file-actions">
          <button
            className="button"
            disabled={controlsDisabled}
            onClick={() => void begin("resume")}
          >
            {t(diskOutput ? "继续未完成的磁盘复制" : "继续未完成的文件导出")}
          </button>
          <button
            className="button"
            disabled={controlsDisabled}
            onClick={() => void begin("reread")}
          >
            {t("复读已填充扇区")}
          </button>
          <button
            className="button"
            disabled={controlsDisabled || (diskOutput && !destination)}
            onClick={() => void begin("legacy")}
          >
            {t("用旧 JSON 报告复读")}
          </button>
        </div>
      )}
      {options && (
        <p className="surface-note">
          {t("绝对 LBA {first}–{last} · {size} B", {
            first: integer(absoluteStart),
            last: integer(absoluteStart + options.sectorCount - 1),
            size: integer(options.sectorCount * sectorSize),
          })}
        </p>
      )}
      {!diskOutput && (
        <p className="surface-note">
          {t(
            imageSource
              ? "保存到其他物理磁盘的新 .bin 文件。完整回读并校验 SHA-256 后发布。读取失败时停止，已创建的临时文件保留为 .partial。"
              : "保存到另一块物理磁盘的新 .bin 文件。在线源卷会锁定，输出会完整回读校验 SHA-256。中断后保留 .partial 和 .dscopy 文件以便续作。",
          )}
        </p>
      )}
      <FormError
        message={catalogText(
          (error || rangeError || policyError || destinationError).trim(),
        )}
      />
      {job && (
        <>
          <progress
            className="surface-progress"
            max={job.total || 1}
            value={job.done}
            aria-label={t("数据任务进度")}
          />
          <p className="surface-note">
            {catalogText(job.phase)} · {formatSize(job.done)} /{" "}
            {formatSize(job.total)}
          </p>
          <div role="status">
            {job.message && (
              <p className="surface-message">{catalogText(job.message)}</p>
            )}
            {job.sha256 && <p className="surface-note">SHA-256 {job.sha256}</p>}
          </div>
          {report && (
            <>
              {report.destinationStartLba !== undefined && (
                <p className="surface-note">
                  {t("目标")} ·{" "}
                  {t("绝对 LBA {first}–{last} · {size} B", {
                    first: integer(report.destinationStartLba),
                    last: integer(
                      report.destinationStartLba + report.sectorCount - 1,
                    ),
                    size: integer(report.sectorCount * report.sectorSize),
                  })}
                </p>
              )}
              <p className="surface-note">
                {t("读取失败 {failed} 个 · 填充 {filled} 个扇区", {
                  failed: integer(report.failures.unreadableSectors),
                  filled: integer(report.failures.filledSectors),
                })}
              </p>
              {report.failures.reread && (
                <p className="surface-note">
                  {t("延后 {queued} 个扇区 · 重读恢复 {recovered} 个", {
                    queued: integer(report.failures.reread.queuedSectors),
                    recovered: integer(report.failures.reread.recoveredSectors),
                  })}
                </p>
              )}
              {report.failures.ranges.length > 0 && (
                <details className="sector-copy-failures">
                  <summary>{t("填充范围")}</summary>
                  <ul>
                    {report.failures.ranges.map((range) => (
                      <li key={range.failedLba}>
                        {t(
                          "失败 LBA {failed} · 填充 LBA {first}–{last} · 错误 {code}",
                          {
                            failed: integer(range.failedLba),
                            first: integer(range.firstFilledLba),
                            last: integer(
                              range.firstFilledLba + range.filledSectors - 1,
                            ),
                            code: range.windowsCode,
                          },
                        )}
                      </li>
                    ))}
                  </ul>
                  {report.failures.omitted > 0 && (
                    <p className="surface-note">
                      {t("报告省略了 {count} 段填充范围。", {
                        count: integer(report.failures.omitted),
                      })}
                    </p>
                  )}
                </details>
              )}
            </>
          )}
          {job.state === "completed" && (
            <button
              className="button"
              disabled={controlsDisabled}
              onClick={() => void saveReport()}
            >
              {t("保存报告")}
            </button>
          )}
          {savedReport && (
            <p className="surface-note" role="status">
              {t("已保存报告到 {path}", { path: savedReport })}
            </p>
          )}
        </>
      )}
      {plan && (
        <Dialog title={catalogText(plan.title)} onClose={discardPlan}>
          <dl className="data-plan">
            <dt>{t("来源")}</dt>
            <dd>{plan.source}</dd>
            <dt>{t("目标")}</dt>
            <dd>{plan.destination}</dd>
            <dt>{t("处理容量")}</dt>
            <dd>{formatSize(plan.bytes)}</dd>
          </dl>
          <div className="data-plan-warning destructive">
            {catalogText(plan.warning)}
          </div>
          <label className="data-confirm">
            <input
              type="checkbox"
              checked={confirmed}
              disabled={choosing}
              onChange={(e) => setConfirmed(e.target.checked)}
            />
            {t("我已核对目标，理解数据将被覆盖且无法撤销。")}
          </label>
          <div className="dialog-actions">
            <button
              className="button"
              disabled={choosing}
              onClick={discardPlan}
            >
              {t("取消")}
            </button>
            <button
              className="button danger"
              disabled={!confirmed || choosing}
              onClick={() => void executePlan()}
            >
              {t("确认并开始")}
            </button>
          </div>
        </Dialog>
      )}
    </>
  );
}

import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Dialog, FormError, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { MiB, formatSize } from "./types";
import type { Disk, PartitionTarget, WindowsFileSystem } from "./types";
import {
  discardQuickPartition,
  divideQuickRange,
  executeQuickPartition,
  prepareQuickPartition,
  quickFreeRanges,
  quickRoleNames,
} from "./quickPartition";
import type {
  QuickEntry,
  QuickPreview,
  QuickRole,
  QuickStep,
} from "./quickPartition";
import { advanceQuickProgress } from "./quickProgress";
import type { QuickProgress } from "./quickProgress";
import { formatClusters, validateFormatOptions } from "./formatOptions";
import {
  batchResultLines,
  runPartitionOperation,
  errorText,
} from "./partitionOutcome";
import type { PartitionOutcome } from "./partitionOutcome";
import { PartitionBatchProgress } from "./PartitionBatchProgress";

export function QuickPartitionDialog({
  disk,
  target,
  onClose,
  onBusy,
  onRefresh,
  onOutcome,
}: {
  disk: Disk;
  target: PartitionTarget;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
  onOutcome: (outcome: PartitionOutcome) => void;
}) {
  const { t, language, catalogText } = useLocale();
  const mib = (bytes: number) =>
    (bytes / MiB).toLocaleString(language, { maximumFractionDigits: 20 });
  const startLabel = (bytes: number) =>
    t("起点 {start} MiB", { start: mib(bytes) });
  const stepTitle = (step: QuickStep) =>
    step.type === "delete"
      ? t("删除分区 {number}", { number: step.partition })
      : step.type === "create"
        ? t("新建 {role}", {
            role: catalogText(quickRoleNames[step.entry.role]),
          })
        : t("格式化为 {filesystem}", {
            filesystem: step.entry.format!.fileSystem,
          });
  function stepLabel(step: QuickStep) {
    const labels = [stepTitle(step)];
    if (step.type === "format" && step.entry.format?.label)
      labels.push(step.entry.format.label);
    labels.push(
      startLabel(step.type === "delete" ? step.offset : step.entry.offset),
    );
    return labels.join(" · ");
  }
  const [deleted, setDeleted] = useState<number[]>([]);
  const [alignment, setAlignment] = useState(MiB);
  const [count, setCount] = useState(2);
  const [rangeIndex, setRangeIndex] = useState(0);
  const [entries, setEntries] = useState<QuickEntry[]>([]);
  const [preview, setPreview] = useState<QuickPreview | null>(null);
  const [ack, setAck] = useState(false);
  const [busy, setBusy] = useState(false);
  const [execution, setExecution] = useState<{
    steps: QuickStep[];
    progress: QuickProgress;
  } | null>(null);
  const [error, setError] = useState("");
  const token = useRef<string | null>(null);
  const running = useRef(false);
  const ranges = quickFreeRanges(disk, deleted, alignment);
  useEffect(
    () => () => {
      if (token.current)
        void discardQuickPartition(token.current).catch(() => {});
    },
    [],
  );
  useEffect(() => {
    if (token.current)
      void discardQuickPartition(token.current).catch(() => {});
    token.current = null;
    setPreview(null);
    setAck(false);
  }, [target.target, target.revision]);
  function close() {
    if (!running.current) onClose();
  }
  function update(index: number, change: Partial<QuickEntry>) {
    setEntries((old) =>
      old.map((e, i) => (i === index ? { ...e, ...change } : e)),
    );
  }
  function populate() {
    setError("");
    try {
      const range = ranges[rangeIndex];
      if (!range) throw new Error(t("请先选择可用空间。"));
      setEntries(divideQuickRange(range, count, alignment));
    } catch (e) {
      setError(errorText(e));
    }
  }
  async function prepare(event: FormEvent) {
    event.preventDefault();
    if (running.current) return;
    running.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    try {
      if (!entries.length) throw new Error(t("请先生成分区明细。"));
      for (const e of entries) {
        if (
          !Number.isSafeInteger(e.offset) ||
          !Number.isSafeInteger(e.size) ||
          e.offset < MiB ||
          e.size < MiB ||
          e.size % MiB !== 0 ||
          e.offset % alignment !== 0
        )
          throw new Error(
            t("起点必须按所选边界对齐，容量至少 1 MiB 且为整 MiB。"),
          );
        if (e.format)
          validateFormatOptions(
            e.format.fileSystem,
            e.format.clusterSize,
            disk.sectorSize,
            e.format.options,
          );
      }
      const result = await prepareQuickPartition(
        target,
        alignment,
        deleted,
        entries,
      );
      token.current = result.token;
      setPreview(result);
      setAck(false);
    } catch (e) {
      setError(errorText(e));
    } finally {
      running.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  async function execute() {
    if (!preview || !ack || running.current) return;
    const submitted = preview;
    token.current = null;
    setPreview(null);
    setExecution({
      steps: submitted.steps,
      progress: {
        phase: "checking",
        completed: 0,
        total: submitted.steps.length,
        current: null,
      },
    });
    setBusy(true);
    onBusy(true);
    running.current = true;
    try {
      await runPartitionOperation(
        {
          ...submitted,
          targetName: `${diskTitle(disk, t)} · ${catalogText(disk.hardware?.model ?? "未提供型号")}`,
        },
        async (token) => {
          const result = await executeQuickPartition(token, (event) => {
            setExecution(
              (previous) =>
                previous && {
                  ...previous,
                  progress: advanceQuickProgress(previous.progress, event),
                },
            );
          });
          return {
            verified: result.verified,
            message: batchResultLines(
              result,
              submitted.steps.map(stepLabel),
              t,
              catalogText,
            ).join("\n"),
          };
        },
        onRefresh,
        (outcome) => {
          onOutcome(outcome);
          onClose();
        },
      );
    } finally {
      running.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  function back() {
    if (token.current)
      void discardQuickPartition(token.current).catch(() => {});
    token.current = null;
    setPreview(null);
    setAck(false);
    setError("");
  }
  return (
    <Dialog
      wide
      title={t("GPT 快速分区")}
      subtitle={`${diskTitle(disk, t)} · ${target.serial}`}
      onClose={close}
      footer={
        <div className="dialog-actions">
          <button
            className="button"
            disabled={busy}
            onClick={preview ? back : close}
          >
            {preview ? t("返回修改") : t("取消")}
          </button>
          {preview ? (
            <button
              className="button danger"
              disabled={busy || !ack}
              onClick={() => void execute()}
            >
              {t("确认并执行整批")}
            </button>
          ) : (
            <button
              className="button primary"
              form="quick-partition-form"
              type="submit"
              disabled={busy || !entries.length}
            >
              {execution
                ? t("正在逐项执行…")
                : busy
                  ? t("正在核对…")
                  : t("预览全部步骤")}
            </button>
          )}
        </div>
      }
    >
      {execution ? (
        <PartitionBatchProgress
          labels={execution.steps.map(stepLabel)}
          progress={execution.progress}
          note={t(
            "执行期间请勿关闭应用。当前步骤会执行到完成或报告错误，失败后停止后续步骤。",
          )}
        />
      ) : preview ? (
        <>
          <Hint>{catalogText(preview.warning)}</Hint>
          <p>
            {t("保留分区 {partitions}。以下顺序已绑定本次确认。", {
              partitions: preview.retainedPartitions.length
                ? new Intl.ListFormat(language).format(
                    preview.retainedPartitions.map(String),
                  )
                : t("无"),
            })}
          </p>
          <ol className="quick-steps">
            {preview.steps.map((s, i) => (
              <li key={i}>
                {s.type === "delete" ? (
                  <>
                    <strong>{stepTitle(s)}</strong> · {startLabel(s.offset)} ·{" "}
                    {formatSize(s.size)}
                  </>
                ) : s.type === "create" ? (
                  <>
                    <strong>{stepTitle(s)}</strong> ·{" "}
                    {startLabel(s.entry.offset)} · {formatSize(s.entry.size)}
                  </>
                ) : (
                  <>
                    <strong>{stepTitle(s)}</strong> · {s.entry.format!.label} ·{" "}
                    {s.entry.format!.options.mode === "full"
                      ? t("完整格式化")
                      : t("快速格式化")}{" "}
                    ·{" "}
                    {t("簇 {size}", {
                      size: s.entry.format!.clusterSize
                        ? t("{size} 字节", {
                            size: s.entry.format!.clusterSize,
                          })
                        : s.entry.format!.options.ntfsCompression
                          ? t("4096 字节（压缩）")
                          : t("系统默认"),
                    })}
                    {s.entry.format!.fileSystem === "NTFS" && (
                      <>
                        {" · "}
                        {t("默认压缩 {state}", {
                          state: t(
                            s.entry.format!.options.ntfsCompression
                              ? "开启"
                              : "关闭",
                          ),
                        })}
                        {" · "}
                        {t("8.3 短名 {state}", {
                          state: t(
                            s.entry.format!.options.ntfsShortNames
                              ? "开启"
                              : "关闭",
                          ),
                        })}
                      </>
                    )}
                  </>
                )}
              </li>
            ))}
          </ol>
          <label className="check-row">
            <input
              type="checkbox"
              checked={ack}
              onChange={(e) => setAck(e.target.checked)}
            />
            {t(
              "我已核对序列号和全部步骤，理解所选删除分区的数据会丢失，失败时可能只完成一部分。",
            )}
          </label>
        </>
      ) : (
        <form id="quick-partition-form" onSubmit={(e) => void prepare(e)}>
          <Hint>
            {t(
              "一次配置多处分区。默认保留现有分区；只会删除下方明确勾选的普通数据分区。RAW 请先单独初始化，MBR 不支持此功能。",
            )}
          </Hint>
          <fieldset disabled={busy} className="quick-editor">
            <legend>{t("现有分区")}</legend>
            <div className="quick-existing">
              {disk.partitions.map((p) => (
                <label key={p.id} className="check-row">
                  <input
                    type="checkbox"
                    disabled={
                      p.number == null ||
                      !target.deletePartitions.includes(p.number)
                    }
                    checked={p.number != null && deleted.includes(p.number)}
                    onChange={(e) => {
                      setRangeIndex(0);
                      setDeleted((old) =>
                        e.target.checked
                          ? [...old, p.number!]
                          : old.filter((n) => n !== p.number),
                      );
                    }}
                  />
                  {p.number} · {partitionTitle(p, catalogText)} ·{" "}
                  {formatSize(p.size)} ·{" "}
                  {p.number != null &&
                  target.deletePartitions.includes(p.number)
                    ? t("勾选即删除")
                    : t("保留（受保护或不可删除）")}
                </label>
              ))}
            </div>
            <div className="quick-controls">
              <label>
                {t("对齐")}
                <select
                  value={alignment}
                  onChange={(e) => {
                    setAlignment(Number(e.target.value));
                    setRangeIndex(0);
                  }}
                >
                  {[1, 2, 4, 8, 16].map((n) => (
                    <option value={n * MiB} key={n}>
                      {n} MiB
                    </option>
                  ))}
                </select>
              </label>
              <label>
                {t("可用空间")}
                <select
                  value={rangeIndex}
                  onChange={(e) => setRangeIndex(Number(e.target.value))}
                >
                  {ranges.map((r, i) => (
                    <option value={i} key={r.offset}>
                      {t("{start} MiB 起 · {size}", {
                        start: mib(r.offset),
                        size: formatSize(r.size),
                      })}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                {t("分区数量")}
                <input
                  type="number"
                  min={1}
                  max={32}
                  step={1}
                  value={count}
                  onChange={(e) => setCount(Number(e.target.value))}
                />
              </label>
              <button
                className="button"
                type="button"
                onClick={populate}
                disabled={!ranges.length}
              >
                {t("均分并生成明细")}
              </button>
            </div>
            <p className="muted">
              {t(
                "生成会替换下方明细。可逐行调整起点、容量和类型，或留出间隔；预览会重新核对重叠与边界。",
              )}
            </p>
            <div className="quick-entries">
              {entries.map((entry, i) => (
                <section className="quick-entry" key={i}>
                  <div className="quick-entry-heading">
                    <strong>{t("新分区 {number}", { number: i + 1 })}</strong>
                    <button
                      className="button"
                      type="button"
                      onClick={() =>
                        setEntries((old) => old.filter((_, n) => n !== i))
                      }
                    >
                      {t("移除此项")}
                    </button>
                  </div>
                  <div className="quick-controls">
                    <label>
                      {t("起点 / MiB")}
                      <input
                        type="number"
                        min={1}
                        step={alignment / MiB}
                        value={entry.offset / MiB}
                        onChange={(e) =>
                          update(i, { offset: Number(e.target.value) * MiB })
                        }
                      />
                    </label>
                    <label>
                      {t("容量 / MiB")}
                      <input
                        type="number"
                        min={1}
                        step={1}
                        value={entry.size / MiB}
                        onChange={(e) =>
                          update(i, { size: Number(e.target.value) * MiB })
                        }
                      />
                    </label>
                    <label>
                      {t("分区类型")}
                      <select
                        value={entry.role}
                        onChange={(e) =>
                          update(i, {
                            role: e.target.value as QuickRole,
                            format: null,
                          })
                        }
                      >
                        {Object.entries(quickRoleNames).map(([value, name]) => (
                          <option value={value} key={value}>
                            {catalogText(name)}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label>
                      {t("文件系统")}
                      <select
                        disabled={entry.role !== "windows"}
                        value={entry.format?.fileSystem ?? "none"}
                        onChange={(e) =>
                          update(i, {
                            format:
                              e.target.value === "none"
                                ? null
                                : {
                                    fileSystem: e.target
                                      .value as WindowsFileSystem,
                                    label: `DATA${i + 1}`,
                                    clusterSize: 0,
                                    options: {
                                      mode: "quick",
                                      ntfsCompression: false,
                                      ntfsShortNames: false,
                                    },
                                  },
                          })
                        }
                      >
                        <option value="none">{t("不格式化")}</option>
                        {["NTFS", "FAT12", "FAT16", "FAT32", "exFAT"].map(
                          (fs) => (
                            <option key={fs}>{fs}</option>
                          ),
                        )}
                      </select>
                    </label>
                  </div>
                  {entry.role !== "windows" && (
                    <p className="muted">
                      {t("仅建立 GPT 条目。")}
                      {entry.role === "efi"
                        ? t("ESP 至少 260 MiB，不创建 FAT 或启动文件。")
                        : entry.role === "reserved"
                          ? t("MSR 容量须为 16 MiB。")
                          : t("Linux 文件系统需创建分区后单独格式化。")}
                    </p>
                  )}
                  {entry.format && (
                    <div className="quick-controls">
                      <label>
                        {t("卷标")}
                        <input
                          value={entry.format.label}
                          onChange={(e) =>
                            update(i, {
                              format: {
                                ...entry.format!,
                                label: e.target.value,
                              },
                            })
                          }
                        />
                      </label>
                      <label>
                        {t("簇大小")}
                        <select
                          value={entry.format.clusterSize}
                          onChange={(e) =>
                            update(i, {
                              format: {
                                ...entry.format!,
                                clusterSize: Number(e.target.value),
                              },
                            })
                          }
                        >
                          <option value={0}>{t("默认")}</option>
                          {formatClusters[entry.format.fileSystem].map((n) => (
                            <option value={n} key={n}>
                              {t("{size} 字节", { size: n })}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label>
                        {t("格式化方式")}
                        <select
                          value={entry.format.options.mode}
                          onChange={(e) =>
                            update(i, {
                              format: {
                                ...entry.format!,
                                options: {
                                  ...entry.format!.options,
                                  mode: e.target.value as "quick" | "full",
                                },
                              },
                            })
                          }
                        >
                          <option value="quick">{t("快速格式化")}</option>
                          <option value="full">{t("完整格式化")}</option>
                        </select>
                      </label>
                      {entry.format.fileSystem === "NTFS" && (
                        <>
                          <label className="check-row">
                            <input
                              type="checkbox"
                              checked={entry.format.options.ntfsCompression}
                              onChange={(e) =>
                                update(i, {
                                  format: {
                                    ...entry.format!,
                                    options: {
                                      ...entry.format!.options,
                                      ntfsCompression: e.target.checked,
                                    },
                                  },
                                })
                              }
                            />
                            {t("新文件默认压缩")}
                          </label>
                          <label className="check-row">
                            <input
                              type="checkbox"
                              checked={entry.format.options.ntfsShortNames}
                              onChange={(e) =>
                                update(i, {
                                  format: {
                                    ...entry.format!,
                                    options: {
                                      ...entry.format!.options,
                                      ntfsShortNames: e.target.checked,
                                    },
                                  },
                                })
                              }
                            />
                            {t("8.3 短文件名")}
                          </label>
                        </>
                      )}
                    </div>
                  )}
                </section>
              ))}
            </div>
          </fieldset>
        </form>
      )}
      <FormError message={catalogText(error)} />
    </Dialog>
  );
}

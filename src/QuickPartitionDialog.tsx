import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Dialog, FormError, Hint } from "./components";
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
import { runPartitionOperation, errorText } from "./partitionOutcome";
import type { PartitionOutcome } from "./partitionOutcome";

function QuickExecution({
  steps,
  progress,
}: {
  steps: QuickStep[];
  progress: QuickProgress;
}) {
  const { phase, completed, total, current } = progress;
  const status =
    phase === "checking"
      ? "正在重新核对磁盘、完整布局和卷关联…"
      : phase === "stopped"
        ? "当前步骤未完整确认，后续步骤已停止。正在接收最终结果…"
        : completed === total
          ? "全部步骤已核对，正在接收最终结果…"
          : current !== null
            ? `正在执行并核对第 ${current + 1} 步，共 ${total} 步`
            : "正在准备下一步…";
  return (
    <section className="quick-progress" aria-label="批量操作进度">
      <p role="status" aria-live="polite">
        {status}
      </p>
      <progress value={completed} max={total} aria-label="已完成并核对的步骤" />
      <p className="muted">
        已完成并核对 {completed} / {total}{" "}
        步。步骤耗时不同，此处不代表已处理的数据比例。
      </p>
      <Hint>
        执行期间请勿关闭应用。当前操作不能中途取消；失败后停止后续步骤，不会自动重试或回滚。
      </Hint>
      <ol className="quick-steps quick-progress-steps">
        {steps.map((step, index) => {
          const state =
            index < completed
              ? "verified"
              : index === current
                ? phase
                : phase === "stopped"
                  ? "skipped"
                  : "waiting";
          const label =
            state === "verified"
              ? "已完成并核对"
              : state === "running"
                ? "正在执行与核对"
                : state === "stopped"
                  ? "已停止，结果待核对"
                  : state === "skipped"
                    ? "未执行"
                    : "待执行";
          return (
            <li
              key={index}
              data-state={state}
              aria-current={state === "running" ? "step" : undefined}
            >
              <span className="quick-step-status">{label}</span>
              <span>
                {step.type === "delete"
                  ? `删除分区 ${step.partition}`
                  : step.type === "create"
                    ? `新建 ${quickRoleNames[step.entry.role]}`
                    : `格式化为 ${step.entry.format!.fileSystem} · ${step.entry.format!.label}`}{" "}
                · 起点{" "}
                {(step.type === "delete" ? step.offset : step.entry.offset) /
                  MiB}{" "}
                MiB
              </span>
            </li>
          );
        })}
      </ol>
    </section>
  );
}

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
      if (!range) throw new Error("请先选择可用空间。");
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
      if (!entries.length) throw new Error("请先生成分区明细。");
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
            "起点必须按所选边界对齐，容量至少 1 MiB 且为整 MiB。",
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
        submitted,
        (token) =>
          executeQuickPartition(token, (event) => {
            setExecution(
              (previous) =>
                previous && {
                  ...previous,
                  progress: advanceQuickProgress(previous.progress, event),
                },
            );
          }),
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
      title="GPT 快速分区"
      subtitle={`${target.name} · ${target.serial}`}
      onClose={close}
      footer={
        <div className="dialog-actions">
          <button
            className="button"
            disabled={busy}
            onClick={preview ? back : close}
          >
            {preview ? "返回修改" : "取消"}
          </button>
          {preview ? (
            <button
              className="button danger"
              disabled={busy || !ack}
              onClick={() => void execute()}
            >
              确认并执行整批
            </button>
          ) : (
            <button
              className="button primary"
              form="quick-partition-form"
              type="submit"
              disabled={busy || !entries.length}
            >
              {execution
                ? "正在逐项执行…"
                : busy
                  ? "正在核对…"
                  : "预览全部步骤"}
            </button>
          )}
        </div>
      }
    >
      {execution ? (
        <QuickExecution steps={execution.steps} progress={execution.progress} />
      ) : preview ? (
        <>
          <Hint>{preview.warning}</Hint>
          <p>
            保留分区：
            {preview.retainedPartitions.length
              ? preview.retainedPartitions.join("、")
              : "无"}
            。以下顺序已绑定本次确认：
          </p>
          <ol className="quick-steps">
            {preview.steps.map((s, i) => (
              <li key={i}>
                {s.type === "delete" ? (
                  <>
                    <strong>删除分区 {s.partition}</strong> · 起点{" "}
                    {s.offset / MiB} MiB · {formatSize(s.size)}
                  </>
                ) : s.type === "create" ? (
                  <>
                    <strong>新建 {quickRoleNames[s.entry.role]}</strong> · 起点{" "}
                    {s.entry.offset / MiB} MiB · {formatSize(s.entry.size)}
                  </>
                ) : (
                  <>
                    <strong>格式化为 {s.entry.format!.fileSystem}</strong> ·{" "}
                    {s.entry.format!.label} ·{" "}
                    {s.entry.format!.options.mode === "full"
                      ? "完整格式化"
                      : "快速格式化"}{" "}
                    · 簇{" "}
                    {s.entry.format!.clusterSize ||
                      (s.entry.format!.options.ntfsCompression
                        ? "4096 字节（压缩）"
                        : "系统默认")}
                    {s.entry.format!.fileSystem === "NTFS" && (
                      <>
                        {" "}
                        · 默认压缩
                        {s.entry.format!.options.ntfsCompression
                          ? "开启"
                          : "关闭"}{" "}
                        · 8.3 短名
                        {s.entry.format!.options.ntfsShortNames
                          ? "开启"
                          : "关闭"}
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
            我已核对序列号和全部步骤，理解所选删除分区的数据会丢失，失败时可能只完成一部分。
          </label>
        </>
      ) : (
        <form id="quick-partition-form" onSubmit={(e) => void prepare(e)}>
          <Hint>
            一次配置多处分区。默认保留现有分区；只会删除下方明确勾选的普通数据分区。RAW
            请先单独初始化，MBR 不支持此功能。
          </Hint>
          <fieldset disabled={busy} className="quick-editor">
            <legend>现有分区</legend>
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
                  {p.number} · {p.name} · {formatSize(p.size)} ·{" "}
                  {p.number != null &&
                  target.deletePartitions.includes(p.number)
                    ? "勾选即删除"
                    : "保留（受保护或不可删除）"}
                </label>
              ))}
            </div>
            <div className="quick-controls">
              <label>
                对齐
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
                可用空间
                <select
                  value={rangeIndex}
                  onChange={(e) => setRangeIndex(Number(e.target.value))}
                >
                  {ranges.map((r, i) => (
                    <option value={i} key={r.offset}>
                      {r.offset / MiB} MiB 起 · {formatSize(r.size)}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                分区数量
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
                均分并生成明细
              </button>
            </div>
            <p className="muted">
              生成会替换下方明细。可逐行调整起点、容量和类型，或留出间隔；预览会重新核对重叠与边界。
            </p>
            <div className="quick-entries">
              {entries.map((entry, i) => (
                <section className="quick-entry" key={i}>
                  <div className="quick-entry-heading">
                    <strong>新分区 {i + 1}</strong>
                    <button
                      className="button"
                      type="button"
                      onClick={() =>
                        setEntries((old) => old.filter((_, n) => n !== i))
                      }
                    >
                      移除此项
                    </button>
                  </div>
                  <div className="quick-controls">
                    <label>
                      起点 / MiB
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
                      容量 / MiB
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
                      分区类型
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
                            {name}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label>
                      文件系统
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
                        <option value="none">不格式化</option>
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
                      仅建立 GPT 条目。
                      {entry.role === "efi"
                        ? "ESP 至少 260 MiB，不创建 FAT 或启动文件。"
                        : entry.role === "reserved"
                          ? "MSR 容量须为 16 MiB。"
                          : "Linux 文件系统需创建分区后单独格式化。"}
                    </p>
                  )}
                  {entry.format && (
                    <div className="quick-controls">
                      <label>
                        卷标
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
                        簇大小
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
                          <option value={0}>默认</option>
                          {formatClusters[entry.format.fileSystem].map((n) => (
                            <option value={n} key={n}>
                              {n} 字节
                            </option>
                          ))}
                        </select>
                      </label>
                      <label>
                        格式化方式
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
                          <option value="quick">快速</option>
                          <option value="full">完整</option>
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
                            新文件默认压缩
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
                            8.3 短文件名
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
      <FormError message={error} />
    </Dialog>
  );
}

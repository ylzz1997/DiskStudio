import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Dialog, FormError, Hint } from "./components";
import { defaultFormatOptions, formatClusters } from "./formatOptions";
import { PartitionBatchProgress } from "./PartitionBatchProgress";
import { errorText, runPartitionOperation } from "./partitionOutcome";
import type { PartitionOutcome } from "./partitionOutcome";
import { advanceQuickProgress } from "./quickProgress";
import type { QuickProgress } from "./quickProgress";
import type { QuickEntry } from "./quickPartition";
import { formatSize, MiB } from "./types";
import { useLocale } from "./i18n/LanguageProvider";
import type { Translate } from "./i18n/translate";
import { diskTitle } from "./LocalDiskView";
import type {
  Disk,
  Partition,
  PartitionPlan,
  PartitionTarget,
  WindowsFileSystem,
} from "./types";

type Mode = "split" | "allocate" | "merge";
type Extent = { offset: number; size: number };
type PartitionRef = Extent & { number: number };
type Step =
  | { type: "resize"; partition: PartitionRef; newSize: number }
  | { type: "move"; partition: PartitionRef; newOffset: number }
  | { type: "create" | "format"; entry: QuickEntry }
  | { type: "copyAndRemove"; keep: PartitionRef; absorb: PartitionRef }
  | { type: "offline" };
type Preview = PartitionPlan & {
  steps: Step[];
  journals: string[];
  mergeFolder: string | null;
};

export function reallocationCandidate(
  p: Partition,
  target: PartitionTarget,
): p is Partition & { number: number } {
  return (
    target.available &&
    p.number != null &&
    !p.system &&
    p.fs.toUpperCase() === "NTFS" &&
    target.resizePartitions.includes(p.number) &&
    target.formatPartitions.includes(p.number)
  );
}

export function mergeCandidates(
  disk: Disk,
  keep: Partition,
  target: PartitionTarget,
) {
  if (
    disk.kind !== "physical" ||
    disk.scheme !== "GPT" ||
    !reallocationCandidate(keep, target)
  )
    return [];
  const ordered = disk.partitions
    .filter((part) => part.number != null)
    .sort((left, right) => left.start - right.start);
  const index = ordered.findIndex((part) => part.id === keep.id);
  if (index < 0) return [];
  return ordered
    .slice(Math.max(0, index - 1), index + 2)
    .filter(
      (part): part is Partition & { number: number } =>
        part.id !== keep.id &&
        reallocationCandidate(part, target) &&
        target.deletePartitions.includes(part.number) &&
        (part.start > keep.start ||
          (part.start < keep.start && disk.hardware?.isSystem === false)),
    );
}

function stepLabel(step: Step, t: Translate) {
  switch (step.type) {
    case "resize":
      return t("分区 {partition} 容量从 {before} 调整至 {after}", {
        partition: step.partition.number,
        before: formatSize(step.partition.size),
        after: formatSize(step.newSize),
      });
    case "move":
      return t("分区 {partition} 起点从 {before} 移至 {after} MiB", {
        partition: step.partition.number,
        before: step.partition.offset / MiB,
        after: step.newOffset / MiB,
      });
    case "create":
      return t("在 {offset} MiB 新建 {size} 的分区", {
        offset: step.entry.offset / MiB,
        size: formatSize(step.entry.size),
      });
    case "format":
      return t("在 {offset} MiB 格式化新卷为 {filesystem} · {label}", {
        offset: step.entry.offset / MiB,
        filesystem: step.entry.format?.fileSystem ?? "—",
        label: step.entry.format?.label ?? "",
      });
    case "copyAndRemove":
      return t(
        "将分区 {source} 的文件复制到分区 {target}，核验后删除分区 {source}",
        {
          source: step.absorb.number,
          target: step.keep.number,
        },
      );
    case "offline":
      return t("将磁盘脱机并核对最终布局");
  }
}

function previewWarning(preview: Preview, t: Translate) {
  const warning = preview.steps.some((step) => step.type === "move")
    ? t(
        "搬移时整块磁盘将脱机。每段数据和 GPT 核对通过后临时联机以完成后续步骤，整批完成后磁盘保持脱机。中断时请保留另一块磁盘上的断点文件。",
      )
    : t(
        "每一步写后核对通过才会继续。缩放结果取决于 Windows 的实际允许范围。失败会停止剩余步骤，请检查已完成的部分。",
      );
  if (!preview.mergeFolder) return warning;
  const merge = t(
    "被吸收卷的文件会复制到保留卷根目录下的 {folder}。保留卷当前可用空间需容纳这些文件。持锁复查源卷和目标副本后才会发出删除请求。中断时，已复制的文件会留在该目录中。",
    {
      folder: preview.mergeFolder,
    },
  );
  return `${merge} ${warning}`;
}

export function PartitionReallocationDialog({
  mode,
  disk,
  partition,
  target,
  onClose,
  onBusy,
  onRefresh,
  onOutcome,
}: {
  mode: Mode;
  disk: Disk;
  partition: Partition & { number: number };
  target: PartitionTarget;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
  onOutcome: (outcome: PartitionOutcome) => void;
}) {
  const { t, catalogText } = useLocale();
  const split = mode === "split";
  const merge = mode === "merge";
  const neighbours = merge ? mergeCandidates(disk, partition, target) : [];
  const [absorbNumber, setAbsorbNumber] = useState(neighbours[0]?.number ?? -1);
  const absorbed = neighbours.find((other) => other.number === absorbNumber);
  const donors =
    disk.hardware?.isSystem === false
      ? disk.partitions.filter(
          (p) =>
            reallocationCandidate(p, target) &&
            p.number !== partition.number &&
            p.size >= 65 * MiB,
        )
      : [];
  const [from, setFrom] = useState<"unallocated" | "partition">(
    target.freeRanges.length ? "unallocated" : "partition",
  );
  const [freeOffset, setFreeOffset] = useState(
    target.freeRanges[0]?.offset ?? -1,
  );
  const [donorNumber, setDonorNumber] = useState(donors[0]?.number ?? -1);
  const canFormatDefault = split && partition.size >= 128 * MiB;
  const [amount, setAmount] = useState(canFormatDefault ? "64" : "1");
  const [fileSystem, setFileSystem] = useState<WindowsFileSystem | "">(
    canFormatDefault ? "NTFS" : "",
  );
  const [label, setLabel] = useState("NEWVOLUME");
  const [clusterSize, setClusterSize] = useState(0);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [ack, setAck] = useState(false);
  const [busy, setBusy] = useState(false);
  const [execution, setExecution] = useState<{
    plan: Preview;
    progress: QuickProgress;
  } | null>(null);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState("");
  const pending = useRef<string | null>(null);
  const inFlight = useRef(false);
  const scope = useRef(0);

  function discard() {
    const token = pending.current;
    pending.current = null;
    if (token)
      void invoke("partition_reallocation_discard", { token }).catch(() => {});
  }
  useEffect(() => {
    setPreview(null);
    setAck(false);
    return () => {
      scope.current += 1;
      discard();
    };
  }, [target.target, target.revision, partition.number]);

  const range = target.freeRanges.find((range) => range.offset === freeOffset);
  const donor = donors.find((partition) => partition.number === donorNumber);
  const minimum =
    split && (fileSystem === "NTFS" || fileSystem === "exFAT") ? 64 : 1;
  const maximum = Math.max(
    0,
    Math.floor(
      (split
        ? partition.size - 64 * MiB
        : from === "unallocated"
          ? (range?.size ?? 0)
          : (donor?.size ?? 0) - 64 * MiB) / MiB,
    ),
  );

  async function prepare(event: FormEvent) {
    event.preventDefault();
    if (inFlight.current || !reallocationCandidate(partition, target)) return;
    const generation = scope.current;
    const amountMiB = Number(amount);
    setError("");
    try {
      if (
        !merge &&
        (!Number.isSafeInteger(amountMiB) ||
          amountMiB < minimum ||
          amountMiB > maximum)
      ) {
        throw new Error(t("请输入已选范围内的整 MiB 容量。"));
      }
      if (merge && !absorbed) throw new Error(t("请选择相邻的 NTFS 分区。"));
      const capacity = amountMiB * MiB;
      const operation = merge
        ? { type: "merge", keep: partition.number, absorb: absorbNumber }
        : split
          ? {
              type: "split",
              partition: partition.number,
              newSize: capacity,
              format: fileSystem
                ? {
                    fileSystem,
                    label,
                    clusterSize,
                    options: defaultFormatOptions,
                  }
                : null,
            }
          : from === "unallocated"
            ? {
                type: "allocate",
                partition: partition.number,
                freeOffset,
                amount: capacity,
              }
            : {
                type: "transfer",
                donor: donorNumber,
                receiver: partition.number,
                amount: capacity,
              };
      inFlight.current = true;
      setBusy(true);
      onBusy(true);
      const next = await invoke<Preview | null>(
        "partition_reallocation_prepare",
        {
          request: {
            target: target.target,
            revision: target.revision,
            operation,
          },
        },
      );
      if (generation !== scope.current) {
        if (next)
          void invoke("partition_reallocation_discard", {
            token: next.token,
          }).catch(() => {});
      } else if (next) {
        pending.current = next.token;
        setPreview(next);
        setAck(false);
      }
    } catch (err) {
      if (generation === scope.current) setError(errorText(err));
    } finally {
      if (inFlight.current) {
        inFlight.current = false;
        setBusy(false);
        onBusy(false);
      }
    }
  }

  async function execute() {
    if (
      !preview ||
      !ack ||
      pending.current !== preview.token ||
      inFlight.current
    )
      return;
    const submitted = preview;
    pending.current = null;
    inFlight.current = true;
    setPreview(null);
    setExecution({
      plan: submitted,
      progress: {
        phase: "checking",
        completed: 0,
        total: submitted.steps.length,
        current: null,
      },
    });
    setError("");
    setBusy(true);
    onBusy(true);
    try {
      await runPartitionOperation(
        submitted,
        async (token) => {
          const progress = new Channel<QuickProgress>();
          progress.onmessage = (event) =>
            setExecution(
              (previous) =>
                previous && {
                  ...previous,
                  progress: advanceQuickProgress(previous.progress, event),
                },
            );
          try {
            return await invoke("partition_reallocation_execute", {
              token,
              progress,
            });
          } finally {
            progress.onmessage = () => {};
          }
        },
        onRefresh,
        (outcome) => {
          onOutcome(outcome);
          onClose();
        },
      );
    } finally {
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }

  async function stop() {
    if (!execution || stopping) return;
    setStopping(true);
    try {
      if (
        !(await invoke<boolean>("partition_reallocation_stop", {
          token: execution.plan.token,
        }))
      )
        setStopping(false);
    } catch (err) {
      setError(errorText(err));
      setStopping(false);
    }
  }

  return (
    <Dialog
      wide
      title={merge ? t("合并分区") : split ? t("拆分分区") : t("重新分配空间")}
      subtitle={[
        diskTitle(disk, t),
        disk.hardware?.model,
        target.serial,
        !merge &&
          `${t("分区 {partition}", { partition: partition.number })} ${partition.name}`,
      ]
        .filter(Boolean)
        .join(" · ")}
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
      footer={
        <div className="dialog-actions">
          {execution ? (
            <button
              className="button"
              disabled={stopping || execution.progress.phase !== "running"}
              onClick={() => void stop()}
            >
              {stopping ? t("已请求停止") : t("停止剩余步骤")}
            </button>
          ) : (
            <>
              <button
                className="button"
                disabled={busy}
                onClick={() => {
                  if (preview) {
                    discard();
                    setPreview(null);
                    setAck(false);
                  } else onClose();
                }}
              >
                {preview ? t("返回修改") : t("取消")}
              </button>
              {preview ? (
                <button
                  className="button primary"
                  disabled={!ack || busy}
                  onClick={() => void execute()}
                >
                  {t("确认并执行整批")}
                </button>
              ) : (
                <button
                  className="button primary"
                  form="partition-reallocation-form"
                  type="submit"
                  disabled={busy || (merge ? !absorbed : maximum < minimum)}
                >
                  {busy ? t("正在核对…") : t("预览全部步骤")}
                </button>
              )}
            </>
          )}
        </div>
      }
    >
      {execution ? (
        <PartitionBatchProgress
          labels={execution.plan.steps.map((step) => stepLabel(step, t))}
          progress={execution.progress}
          note={
            merge
              ? t(
                  "停止时请保留已复制的目录和移动断点文件。已经提交的 Windows 调用会继续返回结果。",
                )
              : t(
                  "请求停止后，已提交的 Windows 调用仍会返回结果。搬移中停止时请保留断点文件。",
                )
          }
        />
      ) : preview ? (
        <>
          <Hint>{previewWarning(preview, t)}</Hint>
          <ol className="quick-steps">
            {preview.steps.map((step, index) => (
              <li key={index}>{stepLabel(step, t)}</li>
            ))}
          </ol>
          {!!preview.journals.length && (
            <section>
              <strong>{t("移动断点文件")}</strong>
              <ul className="quick-steps">
                {preview.journals.map((journal) => (
                  <li key={journal}>{journal}</li>
                ))}
              </ul>
            </section>
          )}
          <label className="check-row">
            <input
              type="checkbox"
              checked={ack}
              onChange={(event) => setAck(event.target.checked)}
            />
            {t("我已核对磁盘和各步骤，理解中断时可能只完成部分步骤。")}
          </label>
        </>
      ) : (
        <form
          id="partition-reallocation-form"
          onSubmit={(event) => void prepare(event)}
        >
          <fieldset className="quick-editor" disabled={busy}>
            <legend>
              {merge
                ? t("保留分区 {partition} · {name}", {
                    partition: partition.number,
                    name: partition.name,
                  })
                : split
                  ? t("新分区设置")
                  : t("空间来源")}
            </legend>
            <div className="quick-controls">
              {merge && (
                <label>
                  {t("要合并的相邻分区")}
                  <select
                    value={absorbNumber}
                    onChange={(event) =>
                      setAbsorbNumber(Number(event.target.value))
                    }
                  >
                    {neighbours.map((other) => (
                      <option key={other.id} value={other.number}>
                        {other.number} · {other.name} · {formatSize(other.size)}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              {!split && !merge && (
                <>
                  <label>
                    {t("来源")}
                    <select
                      value={from}
                      onChange={(event) =>
                        setFrom(
                          event.target.value === "partition"
                            ? "partition"
                            : "unallocated",
                        )
                      }
                    >
                      <option
                        value="unallocated"
                        disabled={!target.freeRanges.length}
                      >
                        {t("未分配空间")}
                      </option>
                      <option value="partition" disabled={!donors.length}>
                        {t("缩小另一分区")}
                      </option>
                    </select>
                  </label>
                  {from === "unallocated" ? (
                    <label>
                      {t("未分配范围")}
                      <select
                        value={freeOffset}
                        onChange={(event) =>
                          setFreeOffset(Number(event.target.value))
                        }
                      >
                        {target.freeRanges.map((range) => (
                          <option value={range.offset} key={range.offset}>
                            {t("{offset} MiB 起 · {size}", {
                              offset: range.offset / MiB,
                              size: formatSize(range.size),
                            })}
                          </option>
                        ))}
                      </select>
                    </label>
                  ) : (
                    <label>
                      {t("供给分区")}
                      <select
                        value={donorNumber}
                        onChange={(event) =>
                          setDonorNumber(Number(event.target.value))
                        }
                      >
                        {donors.map((donor) => (
                          <option value={donor.number} key={donor.id}>
                            {donor.number} · {donor.name} ·{" "}
                            {formatSize(donor.size)}
                          </option>
                        ))}
                      </select>
                    </label>
                  )}
                </>
              )}
              {!merge && (
                <label>
                  {split ? t("新分区容量 / MiB") : t("分配容量 / MiB")}
                  <input
                    type="number"
                    value={amount}
                    min={minimum}
                    max={Math.max(minimum, maximum)}
                    step={1}
                    onChange={(event) => setAmount(event.target.value)}
                    required
                  />
                </label>
              )}
              {split && (
                <>
                  <label>
                    {t("文件系统")}
                    <select
                      value={fileSystem}
                      onChange={(event) => {
                        setFileSystem(
                          event.target.value as WindowsFileSystem | "",
                        );
                        setClusterSize(0);
                      }}
                    >
                      <option value="">{t("保留未格式化")}</option>
                      {Object.keys(formatClusters).map((fs) => (
                        <option key={fs} value={fs}>
                          {fs}
                        </option>
                      ))}
                    </select>
                  </label>
                  {fileSystem && (
                    <>
                      <label>
                        {t("新卷卷标")}
                        <input
                          value={label}
                          onChange={(event) => setLabel(event.target.value)}
                        />
                      </label>
                      <label>
                        {t("簇大小")}
                        <select
                          value={clusterSize}
                          onChange={(event) =>
                            setClusterSize(Number(event.target.value))
                          }
                        >
                          <option value={0}>{t("默认")}</option>
                          {formatClusters[fileSystem]
                            .filter((size) => size >= disk.sectorSize)
                            .map((size) => (
                              <option value={size} key={size}>
                                {formatSize(size)}
                              </option>
                            ))}
                        </select>
                      </label>
                    </>
                  )}
                </>
              )}
            </div>
          </fieldset>
        </form>
      )}
      <FormError message={catalogText(error)} />
    </Dialog>
  );
}

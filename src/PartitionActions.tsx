import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import {
  ArrowLeftRight,
  Check,
  LoaderCircle,
  Plus,
  RefreshCcw,
  Trash2,
  MoreHorizontal,
  Tag,
  ALargeSmall,
} from "lucide-react";
import { Dialog, FormError, Hint } from "./components";
import { executePartition, preparePartition } from "./backend";
import { PartitionAttributesDialog } from "./PartitionAttributesDialog";
import { QuickPartitionDialog } from "./QuickPartitionDialog";
import { runPartitionOperation } from "./partitionOutcome";
import type { PartitionOutcome } from "./partitionOutcome";
import { formatSize, GiB, MiB } from "./types";
import {
  defaultFormatOptions,
  effectiveFormatCluster,
  formatClusters,
  validateFormatOptions,
} from "./formatOptions";
import type {
  Disk,
  Partition,
  PartitionAction,
  PartitionPlan,
  PartitionTarget,
  WindowsFileSystem,
  ExtFileSystem,
  FormatOptions,
} from "./types";

const titles = {
  initialize: "初始化磁盘",
  create: "新建分区",
  format: "格式化分区",
  resize: "调整分区大小",
  delete: "删除分区",
  setLabel: "修改卷标",
  setDriveLetter: "管理盘符",
};
export function PartitionActions({
  disk,
  partition,
  target,
  unavailableReason,
  onBusy,
  onRefresh,
  onOutcome,
  service = { preparePartition, executePartition },
}: {
  disk: Disk;
  partition?: Partition;
  target?: PartitionTarget;
  unavailableReason: string;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
  onOutcome: (outcome: PartitionOutcome) => void;
  service?: {
    preparePartition: typeof preparePartition;
    executePartition: typeof executePartition;
  };
}) {
  const [kind, setKind] = useState<keyof typeof titles | null>(null);
  const [size, setSize] = useState("2");
  const [offset, setOffset] = useState(0);
  const [label, setLabel] = useState("新建卷");
  const [fileSystem, setFileSystem] = useState<WindowsFileSystem>("NTFS");
  const [clusterSize, setClusterSize] = useState(0);
  const [formatOptions, setFormatOptions] = useState<FormatOptions>({
    ...defaultFormatOptions,
  });
  const [extFileSystem, setExtFileSystem] = useState<ExtFileSystem>("ext4");
  const [blockSize, setBlockSize] = useState(4096);
  const [partitionKind, setPartitionKind] = useState<"windows" | "linux">(
    "windows",
  );
  const [plan, setPlan] = useState<PartitionPlan | null>(null);
  const [busy, setBusy] = useState(false);
  const [executing, setExecuting] = useState(false);
  const [error, setError] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [driveLetter, setDriveLetter] = useState("");
  const [more, setMore] = useState(false);
  const [attributes, setAttributes] = useState(false);
  const [quick, setQuick] = useState(false);
  const moreAnchor = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!more) return;
    const outside = (e: PointerEvent) => {
      if (!moreAnchor.current?.contains(e.target as Node)) setMore(false);
    };
    const escape = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMore(false);
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [more]);
  useEffect(() => {
    setMore(false);
  }, [disk.id, partition?.id]);
  const available = !!target?.available;
  const reason = target?.reason ?? (!available ? unavailableReason : null);
  const extCandidate =
    available &&
    partition?.number != null &&
    !!target?.extPartitions?.includes(partition.number);
  const canMetadata =
    available &&
    partition?.number != null &&
    !!target?.metadataPartitions.includes(partition.number);
  const supports = (
    action: "format" | "delete" | "resize" | "setLabel" | "setDriveLetter",
  ) =>
    action === "setLabel" || action === "setDriveLetter"
      ? canMetadata
      : available &&
        partition?.number != null &&
        (target![`${action}Partitions`].includes(partition.number) ||
          (extCandidate && action !== "delete"));
  const canResize =
    supports("resize") && (partition?.fs === "NTFS" || extCandidate);
  const canFormat = supports("format");
  const canDelete = supports("delete");
  const destructive = kind === "format" || kind === "delete";
  const selectedRange = target?.freeRanges.find((r) => r.offset === offset);
  function open(type: keyof typeof titles) {
    if (!available) return;
    setMore(false);
    setKind(type);
    setPlan(null);
    setError("");
    setAcknowledged(false);
    setPartitionKind("windows");
    setOffset(target!.freeRanges[0]?.offset ?? 0);
    setSize(
      String(
        type === "create"
          ? Math.min(2, (target!.freeRanges[0]?.size ?? 0) / GiB)
          : (partition?.size ?? GiB) / GiB,
      ),
    );
    const fs =
      (["NTFS", "FAT12", "FAT16", "FAT32", "exFAT"] as const).find(
        (f) => f.toLowerCase() === partition?.fs.toLowerCase(),
      ) ?? "NTFS";
    setFileSystem(fs);
    setClusterSize(0);
    setFormatOptions({ ...defaultFormatOptions });
    setExtFileSystem("ext4");
    setBlockSize(4096);
    setDriveLetter(partition?.driveLetter ?? "");
    setLabel(
      type === "setLabel"
        ? (partition?.volumeLabel ?? "")
        : fs.startsWith("FAT")
          ? "NEWVOLUME"
          : partition?.fs === fs
            ? partition.name
            : "新建卷",
    );
  }
  function close() {
    if (!busy) {
      setKind(null);
      setPlan(null);
      setError("");
    }
  }
  async function preview(event: FormEvent) {
    event.preventDefault();
    if (!kind || busy || !target?.available) return;
    setError("");
    try {
      const bytes = Number(size) * GiB;
      if (
        (kind === "create" || kind === "resize") &&
        (!Number.isSafeInteger(bytes) ||
          bytes < (kind === "create" ? 1 : 64) * MiB ||
          bytes % MiB !== 0)
      )
        throw new Error(
          `容量至少 ${kind === "create" ? 1 : 64} MiB，并须为整 MiB；例如 0.5、1 或 2 GiB。`,
        );
      let action: PartitionAction;
      if (kind === "initialize") action = { type: kind };
      else if (kind === "create")
        action = { type: kind, offset, size: bytes, kind: partitionKind };
      else {
        if (!supports(kind) || partition?.number == null)
          throw new Error("所选分区不支持此操作。");
        if (kind === "setLabel") {
          const normalized = partition.fs.toUpperCase().startsWith("FAT")
            ? label.trim().toUpperCase()
            : label.trim();
          setLabel(normalized);
          action = {
            type: kind,
            partition: partition.number,
            label: normalized,
          };
        } else if (kind === "setDriveLetter") {
          action = {
            type: kind,
            partition: partition.number,
            letter: driveLetter || null,
          };
        } else if (kind === "format") {
          const normalizedLabel =
            !extCandidate && fileSystem.startsWith("FAT")
              ? label.trim().toUpperCase()
              : label.trim();
          setLabel(normalizedLabel);
          if (!extCandidate)
            validateFormatOptions(
              fileSystem,
              clusterSize,
              disk.sectorSize,
              formatOptions,
            );
          action = extCandidate
            ? {
                type: "extFormat",
                partition: partition.number,
                label: normalizedLabel,
                fileSystem: extFileSystem,
                blockSize,
              }
            : {
                type: "format",
                partition: partition.number,
                label: normalizedLabel,
                fileSystem,
                clusterSize,
                options: formatOptions,
              };
        } else if (kind === "resize")
          action = {
            type: extCandidate ? "extResize" : kind,
            partition: partition.number,
            size: bytes,
          };
        else action = { type: kind, partition: partition.number };
      }
      setBusy(true);
      onBusy(true);
      setPlan(await service.preparePartition(target, action));
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
      onBusy(false);
    }
  }
  async function execute() {
    if (!plan || busy || (destructive && !acknowledged)) return;
    setBusy(true);
    setExecuting(true);
    onBusy(true);
    setError("");
    const submitted = plan;
    setPlan(null); // The backend consumes it once, including on failure.
    try {
      await runPartitionOperation(
        submitted,
        service.executePartition,
        onRefresh,
        (result) => {
          setKind(null);
          onOutcome(result);
        },
      );
    } finally {
      setBusy(false);
      setExecuting(false);
      onBusy(false);
    }
  }
  return (
    <>
      <div className="partition-toolbar">
        <div className="partition-target">
          <strong>{disk.scheme === "RAW" ? "尚未初始化" : "分区操作"}</strong>
        </div>
        <div className="partition-action-buttons">
          {disk.scheme === "RAW" ? (
            <button
              className="button primary"
              disabled={!available}
              title={reason ?? "初始化磁盘"}
              onClick={() => open("initialize")}
            >
              <Plus size={16} />
              初始化磁盘
            </button>
          ) : (
            <>
              <button
                className="button primary"
                disabled={
                  !available || !target?.freeRanges.some((r) => r.size >= MiB)
                }
                onClick={() => open("create")}
                title={
                  reason ??
                  (target?.freeRanges.some((r) => r.size >= MiB)
                    ? "新建分区"
                    : "没有足够的连续未分配空间")
                }
              >
                <Plus size={16} />
                新建分区
              </button>
              <button
                className="button"
                disabled={!canResize}
                onClick={() => open("resize")}
                title={
                  reason ??
                  (canResize
                    ? "调整分区大小"
                    : "支持 NTFS 和符合条件的 Ext2/3/4 数据分区")
                }
              >
                <ArrowLeftRight size={16} />
                调整大小
              </button>
              <button
                className="button"
                disabled={!canFormat}
                title={
                  reason ??
                  (canFormat
                    ? extCandidate
                      ? "格式化为 Ext2/3/4"
                      : "快速格式化为 NTFS、FAT12/16/32 或 exFAT"
                    : "支持普通 Windows 数据分区和符合条件的 Linux 数据分区；系统卷受保护")
                }
                onClick={() => open("format")}
              >
                <RefreshCcw size={15} />
                格式化
              </button>
              <button
                className="button"
                disabled={!canDelete}
                title={
                  reason ??
                  (canDelete
                    ? "删除所选分区"
                    : "运行中的 Windows、EFI、保留和隐藏分区不可删除")
                }
                onClick={() => open("delete")}
              >
                <Trash2 size={15} />
                删除
              </button>
              <div className="menu-anchor" ref={moreAnchor}>
                <button
                  className="button"
                  aria-label="更多分区操作"
                  aria-expanded={more}
                  disabled={!available}
                  onClick={() => setMore(!more)}
                >
                  <MoreHorizontal size={16} /> 更多
                </button>
                {more && (
                  <div className="popover" aria-label="更多分区操作">
                    <button
                      disabled={disk.scheme !== "GPT"}
                      onClick={() => {
                        setMore(false);
                        setQuick(true);
                      }}
                    >
                      <Plus size={16} />
                      快速分区
                    </button>
                    <button
                      disabled={!canMetadata}
                      onClick={() => open("setDriveLetter")}
                    >
                      <ALargeSmall size={16} />
                      管理盘符
                    </button>
                    <button
                      disabled={!canMetadata}
                      onClick={() => open("setLabel")}
                    >
                      <Tag size={16} />
                      修改卷标
                    </button>
                    <button
                      disabled={
                        partition?.number == null ||
                        disk.scheme !== "GPT" ||
                        partition?.system ||
                        disk.hardware?.isSystem
                      }
                      onClick={() => {
                        setMore(false);
                        setAttributes(true);
                      }}
                    >
                      <MoreHorizontal size={16} />
                      分区属性
                    </button>
                  </div>
                )}
              </div>
            </>
          )}
        </div>
      </div>
      {attributes && partition && (
        <PartitionAttributesDialog
          disk={disk}
          partition={partition}
          onClose={() => setAttributes(false)}
          onBusy={onBusy}
          onRefresh={onRefresh}
        />
      )}
      {quick && target && (
        <QuickPartitionDialog
          disk={disk}
          target={target}
          onClose={() => setQuick(false)}
          onBusy={onBusy}
          onRefresh={onRefresh}
          onOutcome={onOutcome}
        />
      )}
      {kind && target && (
        <Dialog
          title={plan ? "确认分区操作" : titles[kind]}
          subtitle={`${disk.name} · ${disk.hardware?.model ?? ""} · ${formatSize(disk.size)}`}
          onClose={close}
        >
          {busy ? (
            <div className="partition-progress" role="status">
              <LoaderCircle size={28} className="spin" />
              <strong>
                {executing ? "正在执行分区操作" : "正在检查目标并准备预览"}
              </strong>
              <p>
                {executing
                  ? kind === "format" &&
                    !extCandidate &&
                    formatOptions.mode === "full"
                    ? "正在完整格式化，耗时与卷容量有关。请保持应用和计算机运行；执行中的操作不能取消。"
                    : "请保持应用和计算机运行。执行中的操作不能取消。"
                  : extCandidate
                    ? "正在核对分区边界和文件系统；这一步不会写入磁盘。"
                    : "正在向 Windows 核对目标和允许的操作范围。"}
              </p>
            </div>
          ) : plan ? (
            <>
              <div className="destructive-summary">
                <Check size={25} />
                <div>
                  <strong>{plan.title}</strong>
                  <p>
                    {plan.targetName}
                    {plan.partition != null && ` · 分区 ${plan.partition}`}
                  </p>
                </div>
              </div>
              <dl className="detail-list partition-plan-details">
                <dt>目标序列号</dt>
                <dd>{plan.serial}</dd>
                {plan.beforeSize != null &&
                  kind !== "setLabel" &&
                  kind !== "setDriveLetter" && (
                    <>
                      <dt>原容量</dt>
                      <dd>{formatSize(plan.beforeSize)}</dd>
                    </>
                  )}
                {plan.afterSize != null &&
                  kind !== "setLabel" &&
                  kind !== "setDriveLetter" && (
                    <>
                      <dt>操作后容量</dt>
                      <dd>{formatSize(plan.afterSize)}</dd>
                    </>
                  )}
                {kind === "format" && (
                  <>
                    <dt>文件系统 / 卷标</dt>
                    <dd>
                      {extCandidate ? extFileSystem : fileSystem} ·{" "}
                      {label.trim()}
                    </dd>
                    <dt>{extCandidate ? "块大小" : "簇大小"}</dt>
                    <dd>
                      {extCandidate
                        ? formatSize(blockSize)
                        : effectiveFormatCluster(
                              fileSystem,
                              clusterSize,
                              formatOptions,
                            ) === 0
                          ? fileSystem.startsWith("FAT")
                            ? "按容量自动选择（见下方预览）"
                            : "Windows 默认"
                          : formatSize(
                              effectiveFormatCluster(
                                fileSystem,
                                clusterSize,
                                formatOptions,
                              ),
                            )}
                    </dd>
                    {!extCandidate && (
                      <>
                        <dt>格式化方式</dt>
                        <dd>
                          {formatOptions.mode === "full"
                            ? "完整格式化"
                            : "快速格式化"}
                        </dd>
                      </>
                    )}
                    {!extCandidate && fileSystem === "NTFS" && (
                      <>
                        <dt>新文件默认压缩</dt>
                        <dd>
                          {formatOptions.ntfsCompression ? "开启" : "关闭"}
                        </dd>
                        <dt>卷的 8.3 短文件名</dt>
                        <dd>
                          {formatOptions.ntfsShortNames ? "开启" : "关闭"}
                        </dd>
                      </>
                    )}
                  </>
                )}
              </dl>
              <Hint>
                {plan.warning}
                {kind !== "setLabel" &&
                  kind !== "setDriveLetter" &&
                  " 变更会实际写入所选磁盘，执行后无法撤销。"}
              </Hint>
              {destructive && (
                <label className="partition-ack">
                  <input
                    type="checkbox"
                    checked={acknowledged}
                    onChange={(e) => setAcknowledged(e.target.checked)}
                  />
                  我确认此分区的数据可以清除
                </label>
              )}
              <FormError message={error} />
              <div className="dialog-actions">
                <button className="button" onClick={() => setPlan(null)}>
                  返回修改
                </button>
                <button
                  className={`button ${destructive ? "danger" : "primary"}`}
                  disabled={destructive && !acknowledged}
                  onClick={() => void execute()}
                >
                  确认执行
                </button>
              </div>
            </>
          ) : (
            <form className="editor-form" onSubmit={preview}>
              {kind === "initialize" && (
                <Hint>
                  为这块空白磁盘建立 GPT 分区表，确认后会实际写入磁盘。
                </Hint>
              )}
              {kind === "create" && (
                <>
                  <label>
                    分区类型
                    <select
                      aria-label="分区类型"
                      value={partitionKind}
                      onChange={(e) =>
                        setPartitionKind(e.target.value as "windows" | "linux")
                      }
                    >
                      <option value="windows">Windows 数据分区</option>
                      <option value="linux">Linux 数据分区</option>
                    </select>
                  </label>
                  <label>
                    未分配空间
                    <select
                      value={offset}
                      onChange={(e) => setOffset(Number(e.target.value))}
                    >
                      {target.freeRanges.map((r) => (
                        <option key={r.offset} value={r.offset}>
                          {formatSize(r.size)} · 起始 {formatSize(r.offset)}
                        </option>
                      ))}
                    </select>
                    <small>
                      {partitionKind === "windows"
                        ? "有可用盘符时自动分配；盘符用完时保留无盘符。创建后再选择“格式化”建立文件系统。"
                        : "创建后选择“格式化”建立所选 Ext 文件系统，不分配 Windows 盘符。"}
                    </small>
                  </label>
                </>
              )}
              {(kind === "create" || kind === "resize") && (
                <label>
                  分区容量
                  <div className="unit-input">
                    <input
                      aria-label="分区容量"
                      required
                      type="number"
                      min={kind === "create" ? String(MiB / GiB) : "0.0625"}
                      max={
                        kind === "create"
                          ? (selectedRange?.size ?? 0) / GiB
                          : disk.size / GiB
                      }
                      step="any"
                      value={size}
                      onChange={(e) => setSize(e.target.value)}
                    />
                    <span>GiB</span>
                  </div>
                  <small>
                    {kind === "resize"
                      ? extCandidate
                        ? "预览会读取并检查已有 Ext2/3/4，估算缩容下限；仅调整末端，不移动分区。支持干净卸载、1/2/4 KiB 块的常规特性。"
                        : "Windows 会检查可缩小的下限和相邻未分配空间，不移动其他分区。"
                      : `所选空间 ${formatSize(selectedRange?.size ?? 0)}`}
                  </small>
                </label>
              )}
              {kind === "format" && (
                <>
                  <label>
                    文件系统
                    {extCandidate ? (
                      <select
                        aria-label="格式化文件系统"
                        value={extFileSystem}
                        onChange={(e) =>
                          setExtFileSystem(e.target.value as ExtFileSystem)
                        }
                      >
                        <option value="ext4">Ext4</option>
                        <option value="ext3">Ext3</option>
                        <option value="ext2">Ext2</option>
                      </select>
                    ) : (
                      <select
                        aria-label="格式化文件系统"
                        value={fileSystem}
                        onChange={(e) => {
                          const fs = e.target.value as WindowsFileSystem;
                          setFileSystem(fs);
                          setClusterSize(0);
                          setFormatOptions((old) => ({
                            ...old,
                            ntfsCompression: false,
                            ntfsShortNames: false,
                          }));
                          if (
                            fs.startsWith("FAT") &&
                            !/^[A-Za-z0-9 _-]{1,11}$/.test(label)
                          )
                            setLabel("NEWVOLUME");
                        }}
                      >
                        <option value="NTFS">NTFS</option>
                        <option value="FAT32">FAT32</option>
                        <option value="FAT16">FAT16</option>
                        <option value="FAT12">FAT12</option>
                        <option value="exFAT">exFAT</option>
                      </select>
                    )}
                  </label>
                  {extCandidate && (
                    <label>
                      块大小
                      <select
                        aria-label="块大小"
                        value={blockSize}
                        onChange={(e) => setBlockSize(Number(e.target.value))}
                      >
                        {[1024, 2048, 4096]
                          .filter((n) => n >= (disk.sectorSize || 512))
                          .map((n) => (
                            <option key={n} value={n}>
                              {formatSize(n)}
                              {n === 4096 ? "（推荐）" : ""}
                            </option>
                          ))}
                      </select>
                    </label>
                  )}
                  {!extCandidate && (
                    <label>
                      簇大小
                      <select
                        aria-label="簇大小"
                        value={clusterSize}
                        onChange={(e) => setClusterSize(Number(e.target.value))}
                      >
                        <option value={0}>
                          {fileSystem.startsWith("FAT")
                            ? "默认（按容量选择）"
                            : formatOptions.ntfsCompression
                              ? "默认（压缩使用 4 KiB）"
                              : "默认（由 Windows 选择）"}
                        </option>
                        {formatClusters[fileSystem]
                          .filter((n) => n >= (disk.sectorSize || 512))
                          .map((n) => (
                            <option
                              key={n}
                              value={n}
                              disabled={
                                formatOptions.ntfsCompression && n > 4096
                              }
                            >
                              {formatSize(n)}
                              {formatOptions.ntfsCompression && n > 4096
                                ? "（不支持压缩）"
                                : ""}
                            </option>
                          ))}
                      </select>
                      <small>
                        {fileSystem.startsWith("FAT")
                          ? "预览时会检查容量、簇数量及所选 FAT 类型是否匹配。"
                          : "预览时会检查所选卷支持的格式和簇大小。"}
                      </small>
                    </label>
                  )}
                  {!extCandidate && (
                    <>
                      <label>
                        格式化方式
                        <select
                          aria-label="格式化方式"
                          value={formatOptions.mode}
                          onChange={(e) =>
                            setFormatOptions((old) => ({
                              ...old,
                              mode: e.target.value as FormatOptions["mode"],
                            }))
                          }
                        >
                          <option value="quick">快速格式化</option>
                          <option value="full">完整格式化</option>
                        </select>
                        <small>
                          {formatOptions.mode === "full"
                            ? "处理整个卷，耗时更长，执行期间不能取消。FAT 会完整回读校验；格式化不等同于 SSD 硬件安全擦除。"
                            : "建立新的文件系统，不逐扇区覆盖旧内容。两种方式都会清除原有文件。"}
                        </small>
                      </label>
                      {fileSystem === "NTFS" && (
                        <fieldset className="partition-format-options">
                          <legend>NTFS 选项</legend>
                          <label>
                            <input
                              type="checkbox"
                              checked={formatOptions.ntfsCompression}
                              disabled={clusterSize > 4096}
                              onChange={(e) =>
                                setFormatOptions((old) => ({
                                  ...old,
                                  ntfsCompression: e.target.checked,
                                }))
                              }
                            />
                            新文件默认启用压缩
                          </label>
                          <small>
                            需要不大于 4 KiB 的簇；选择默认簇大小时采用 4
                            KiB。压缩效果取决于文件内容。
                          </small>
                          <label>
                            <input
                              type="checkbox"
                              checked={formatOptions.ntfsShortNames}
                              onChange={(e) =>
                                setFormatOptions((old) => ({
                                  ...old,
                                  ntfsShortNames: e.target.checked,
                                }))
                              }
                            />
                            启用卷的 8.3 短文件名
                          </label>
                          <small>
                            供依赖短文件名的旧软件使用；实际创建还受 Windows
                            全局策略影响。
                          </small>
                        </fieldset>
                      )}
                    </>
                  )}
                  <label>
                    卷标
                    <input
                      aria-label="卷标"
                      required
                      maxLength={
                        extCandidate ? 16 : fileSystem === "NTFS" ? 32 : 11
                      }
                      value={label}
                      onChange={(e) => setLabel(e.target.value)}
                    />
                    {!extCandidate && fileSystem.startsWith("FAT") && (
                      <small>
                        最多 11
                        个英文字母、数字、空格、下划线或连字符，字母自动转为大写。
                      </small>
                    )}
                    {extCandidate && (
                      <small>
                        最多 16 个 UTF-8 字节，中文通常每字占 3
                        字节。执行后整块磁盘保持脱机。
                      </small>
                    )}
                  </label>
                </>
              )}
              {kind === "delete" && (
                <Hint>
                  将删除 {partition?.name}（{formatSize(partition?.size ?? 0)}
                  ）及其卷。下一步会显示待确认的目标信息。
                </Hint>
              )}
              {kind === "setLabel" && (
                <label>
                  新卷标
                  <input
                    aria-label="新卷标"
                    maxLength={32}
                    value={label}
                    onChange={(e) => setLabel(e.target.value)}
                  />
                  <small>
                    留空可清除卷标。文件内容保持不变，预览会检查文件系统支持的名称长度。
                  </small>
                </label>
              )}
              {kind === "setDriveLetter" && (
                <label>
                  盘符
                  <select
                    aria-label="盘符"
                    value={driveLetter}
                    onChange={(e) => setDriveLetter(e.target.value)}
                  >
                    <option value="">不分配盘符</option>
                    {Array.from(
                      new Set([
                        ...(target.availableDriveLetters ?? []),
                        ...(partition?.driveLetter
                          ? [partition.driveLetter]
                          : []),
                      ]),
                    )
                      .sort()
                      .map((l) => (
                        <option key={l} value={l}>
                          {l}:{l === partition?.driveLetter ? "（当前）" : ""}
                        </option>
                      ))}
                  </select>
                  <small>
                    仅显示当前空闲盘符和原盘符；提交前会再次检查占用。移除盘符不会删除分区或文件。
                  </small>
                </label>
              )}
              <FormError message={error} />
              <div className="dialog-actions">
                <button className="button" type="button" onClick={close}>
                  取消
                </button>
                <button className="button primary" type="submit">
                  预览操作
                </button>
              </div>
            </form>
          )}
        </Dialog>
      )}
    </>
  );
}

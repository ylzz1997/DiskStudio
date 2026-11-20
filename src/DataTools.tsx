import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Archive,
  ArrowRight,
  CheckCircle2,
  Copy,
  FileSearch,
  FolderOpen,
  HardDrive,
  LoaderCircle,
  Search,
  ShieldCheck,
  Square,
  Pause,
  Play,
  Save,
  Upload,
} from "lucide-react";
import type { DataPlan, Disk, Partition } from "./types";
import { formatSize } from "./types";
import { Dialog, FormError, Hint } from "./components";
import { native } from "./backend";
import { RecoveryResults } from "./RecoveryResults";
import { BackupBrowserPanel, type BackupInfo } from "./BackupBrowserPanel";
import { BackupRestoreDialog } from "./BackupRestoreDialog";
import { ImageBrowserPanel, type ImageBrowseInfo } from "./ImageBrowserPanel";
import { SectorCopyPanel } from "./SectorCopyPanel";
import { ImageLvmDialog, type ImageLvmInfo } from "./ImageLvmDialog";
import { VirtualRaidDialog, type RaidDetection, type RaidEdit, type RaidFiles, type RaidView } from "./VirtualRaidDialog";
import type { ImageBrowseMode, ImageBrowseOptions } from "./ImageBrowserPanel";
import type { NativeImageGroup } from "./imageBrowserSource";
import { ImageBrowseOpenDialog } from "./ImageBrowseOpenDialog";
import { PartitionBrowserPanel } from "./PartitionBrowserPanel";
import { LvmBrowserDialog, type LvmView } from "./LvmBrowserDialog";
import { SignatureOptions } from "./SignatureOptions";
import {
  defaultSignatureSettings,
  signatureRequest,
  previewImageMime,
} from "./signatureModel";
import {
  progressMatches,
  settingsFromProgress,
  progressAmount,
  progressMode,
  type RecoveryProgress,
} from "./recoveryProgressModel";
import type { Candidate, RecoveryLayout } from "./recoveryModel";
import { defaultExportOptions, type ExportOptions } from "./recoveryModel";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { errorText } from "./partitionOutcome";

type Selection = {
  target: string;
  layoutKey: string;
  partition: number | null;
};
type ReconstructionSource =
  | { type: "partition"; selection: Selection }
  | { type: "backup" | "image"; id: string; partition: number };
type Job = {
  id: string;
  title: string;
  state: "running" | "completed" | "failed" | "canceled" | "paused";
  phase: string;
  done: number;
  total: number;
  found: number;
  message: string;
  sha256: string | null;
};
type Preview = { name: string; bytes: number[]; truncated: boolean };
type ScanStatus = {
  id: string;
  sourceName: string;
  lost: {
    filesystem: string;
    size: number;
    startSector: number;
    members: number;
  } | null;
};
type Page = "recovery" | "backup" | "files";
function readablePaths(text: string) {
  return text.replaceAll("\\\\?\\", "");
}
const options = [
  {
    id: "backup",
    icon: Archive,
    name: "镜像备份",
    description: "将磁盘或分区保存为备份文件",
  },
  {
    id: "restore",
    icon: Upload,
    name: "从备份还原",
    description: "校验备份后还原到所选目标",
  },
  {
    id: "clone",
    icon: Copy,
    name: "磁盘与分区克隆",
    description: "按扇区、已用空间或文件克隆",
  },
] as const;
function selection(d: Disk | undefined, p: string): Selection {
  if (!d?.hardware?.identity || !d.hardware.layoutKey)
    throw Error("请刷新并选择信息完整的本机磁盘。");
  return {
    target: d.hardware.identity,
    layoutKey: d.hardware.layoutKey,
    partition: p ? Number(p) : null,
  };
}
export function DataTools({
  page,
  disk,
  disks,
  onBusy,
  onRefresh,
  fixedPartition,
  onBack,
}: {
  page: Page;
  disk: Disk | undefined;
  disks: Disk[];
  onBusy: (value: boolean) => void;
  onRefresh: () => Promise<void>;
  fixedPartition?: number;
  onBack?: () => void;
}) {
  const { language, t, catalogText } = useLocale();
  const [partition, setPartition] = useState(String(fixedPartition ?? ""));
  const [memberKeys, setMemberKeys] = useState<string[]>([]);
  const [allowMissingMembers, setAllowMissingMembers] = useState(false);
  const [mode, setMode] = useState("deleted");
  const [signatureSettings, setSignatureSettings] = useState(
    defaultSignatureSettings,
  );
  const [task, setTask] = useState<"backup" | "restore" | "clone">("backup");
  const [compression, setCompression] = useState<
    "none" | "fast" | "balanced" | "high"
  >("balanced");
  const [encryption, setEncryption] = useState(false);
  const [incremental, setIncremental] = useState(false);
  const [hot, setHot] = useState(false);
  const [copyMode, setCopyMode] = useState<"allSectors" | "usedSpace">(
    "allSectors",
  );
  const [partitionClone, setPartitionClone] = useState<"standard" | "resize" | "files" | "diskFiles">("standard");
  const [diskCloneSizes, setDiskCloneSizes] = useState<Record<number, string>>({});
  const [sourceExpanded, setSourceExpanded] = useState(true);
  const [destination, setDestination] = useState("");
  const [destinationPartition, setDestinationPartition] = useState("");
  const [job, setJob] = useState<Job | null>(null);
  const [ownJobId, setOwnJobId] = useState("");
  const [scanId, setScanId] = useState("");
  const [scanStatus, setScanStatus] = useState<ScanStatus | null>(null);
  const [chooseNewSource, setChooseNewSource] = useState(false);
  const [recoveryProgress, setRecoveryProgress] =
    useState<RecoveryProgress | null>(null);
  const [progressNotice, setProgressNotice] = useState("");
  const [browsingBackup, setBrowsingBackup] = useState(false);
  const [browsingImage, setBrowsingImage] = useState(false);
  const [imageInfo, setImageInfo] = useState<ImageBrowseInfo | null>(null);
  const [imageCopy, setImageCopy] = useState<{ id: string; partition: string } | null>(null);
  const [imageCopyBusy, setImageCopyBusy] = useState(false);
  const imageCopyPart = imageInfo?.partitions.find((part) => String(part.id) === imageCopy?.partition);
  const [imageLvmInfo, setImageLvmInfo] = useState<ImageLvmInfo | null>(null);
  const [imageLvmOpen, setImageLvmOpen] = useState(false);
  const [raidInfo, setRaidInfo] = useState<RaidView | null>(null);
  const [raidFiles, setRaidFiles] = useState<RaidFiles | null>(null);
  const [raidEdit, setRaidEdit] = useState<RaidEdit | null>(null);
  const [raidOpen, setRaidOpen] = useState(false);
  const [imageDraft, setImageDraft] = useState<{
    group: NativeImageGroup;
    options: ImageBrowseOptions;
  } | null>(null);
  const [backupInfo, setBackupInfo] = useState<BackupInfo | null>(null);
  const [backupRestore, setBackupRestore] = useState<{ id: string; partition: number | null } | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [plan, setPlan] = useState<DataPlan | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [previewUrl, setPreviewUrl] = useState("");
  const [lvmOpen, setLvmOpen] = useState(false);
  const [lvmView, setLvmView] = useState<LvmView | null>(null);
  const callbacks = useRef({ onBusy, onRefresh });
  callbacks.current = { onBusy, onRefresh };
  const previousJob = useRef("");
  const lastState = useRef("");
  const busy = pending || job?.state === "running";
  const lostSource = page === "recovery" ? scanStatus?.lost : null;
  const resultsScanId =
    page === "backup"
      ? browsingImage
        ? (raidInfo?.scanId ?? imageLvmInfo?.scanId ?? imageInfo?.scanId ?? "")
        : browsingBackup
          ? (backupInfo?.scanId ?? "")
          : ""
      : page === "recovery"
        ? (raidInfo?.scanId ?? scanId)
        : scanId;
  useEffect(() => {
    setSourceExpanded(!resultsScanId);
  }, [resultsScanId, page, browsingBackup, browsingImage]);
  const destinationDisk = disks.find((d) => d.id === destination);
  const physical = disk?.kind === "physical";
  const parts = disk?.partitions.filter((p) => p.number !== undefined) ?? [];
  const destinations = disks.filter(
    (d) => d.kind === "physical" && d.id !== disk?.id && !d.hardware?.isSystem,
  );
  const selectedPart = parts.find((p) => String(p.number) === partition);
  const allowMemberSelection =
    physical &&
    !!selectedPart &&
    !selectedPart.encryption &&
    !selectedPart.system &&
    (page === "files" || (page === "recovery" && mode !== "signatures" && mode !== "reconstruct"));
  const memberOptions =
    allowMemberSelection && selectedPart
      ? disks.flatMap((other) => {
          if (
            other.kind !== "physical" ||
            !other.hardware?.identity ||
            !other.hardware.layoutKey
          )
            return [];
          return other.partitions
            .filter(
              (p) =>
                p.number !== undefined &&
                !p.system &&
                ["Linux 数据", "数据分区"].includes(p.role ?? "") &&
                (other.id !== disk.id || p.number !== selectedPart.number),
            )
            .map((p) => {
              const source = selection(other, String(p.number));
              return {
                key: JSON.stringify(source),
                disk: other,
                partition: p,
                source,
              };
            });
        })
      : [];
  const selectedMembers = memberOptions.filter(({ key }) =>
    memberKeys.includes(key),
  );
  const useMembers =
    allowMemberSelection && (memberKeys.length > 0 || allowMissingMembers);
  const staleMembers =
    useMembers && selectedMembers.length !== memberKeys.length;
  const canHot =
    copyMode === "allSectors" &&
    !!selectedPart &&
    !selectedPart.system &&
    disk?.hardware?.offline === false &&
    selectedPart.fs.toUpperCase() === "NTFS";
  useEffect(() => {
    if (!canHot) setHot(false);
  }, [canHot]);
  const sourceSize = selectedPart?.size ?? disk?.size ?? 0;
  const linuxSource = selectedPart?.role === "Linux 数据";
  const canResizeClone =
    task === "clone" && disk?.scheme === "GPT" && !!selectedPart && !selectedPart.system &&
    (!linuxSource || (disk.hardware?.offline != null && !disk.hardware.isSystem));
  const fileClonePart = (part: Partition) =>
    part.role === "数据分区" && !part.system && !part.hidden &&
    ["FAT", "FAT12", "FAT16", "FAT32", "EXFAT", "NTFS"].includes(part.fs.toUpperCase());
  const linuxDiskClonePart = (part: Partition) =>
    part.role === "Linux 数据" && !part.system && part.number !== undefined;
  const diskClonePart = (part: Partition) =>
    linuxDiskClonePart(part) ||
    (part.role === "数据分区" && !part.system && !part.hidden && part.number !== undefined);
  const resizableDiskClonePart = (part: Partition) =>
    fileClonePart(part) || linuxDiskClonePart(part);
  const canFileClone = canResizeClone && fileClonePart(selectedPart);
  const canDiskFileClone =
    task === "clone" && !partition && physical && disk.scheme === "GPT" &&
    disk.hardware?.offline === false && !disk.hardware.isSystem &&
    parts.some(diskClonePart) &&
    parts.every((part) => diskClonePart(part) || part.role === "MSR 保留");
  const resizingClone = canResizeClone && partitionClone === "resize";
  const cloningFiles = canFileClone && partitionClone === "files";
  const cloningDiskFiles = canDiskFileClone && partitionClone === "diskFiles";
  const specialClone = resizingClone || cloningFiles || cloningDiskFiles;
  const cloneDisks = cloningDiskFiles
    ? destinations.filter((d) =>
        (d.scheme === "RAW" || d.scheme === "GPT") &&
        d.sectorSize === disk.sectorSize && d.hardware?.layoutAvailable &&
        d.hardware.deviceReadOnly === false &&
        d.partitions.every((part) => part.number === undefined))
    : destinations;
  const cloneDiskSelected = cloneDisks.some((d) => d.id === destination);
  const partitionSizes = parts.filter(resizableDiskClonePart).flatMap((part) => {
    const partitionNumber = part.number!;
    const value = diskCloneSizes[partitionNumber]?.trim();
    return value ? [{ partition: partitionNumber, bytes: Number(value) * (1024 ** 2) }] : [];
  });
  const invalidDiskSizes = partitionSizes.some(({ bytes }) =>
    !Number.isSafeInteger(bytes) || bytes <= 0 || bytes % (1024 ** 2) !== 0);
  const cloneTargets = destinationDisk?.partitions.filter(
    (part) =>
      part.number !== undefined &&
      (specialClone
        ? destinationDisk.scheme === "GPT" &&
          destinationDisk.sectorSize === disk?.sectorSize &&
          !part.system &&
          (resizingClone && linuxSource
            ? destinationDisk.hardware?.offline != null && part.role === "Linux 数据"
            : !part.hidden && part.role === "数据分区") &&
          (cloningFiles || part.size !== sourceSize)
        : part.size === sourceSize),
  ) ?? [];
  const matchingProgress = progressMatches(
    recoveryProgress,
    disk?.hardware?.identity,
    disk?.hardware?.layoutKey,
    partition ? Number(partition) : null,
  );
  let signatureError = "";
  if (mode === "signatures" && sourceSize > 0) {
    try {
      signatureRequest(signatureSettings, sourceSize);
    } catch (e) {
      signatureError = e instanceof Error ? e.message : String(e);
    }
  }
  useEffect(() => {
    setSignatureSettings((old) => ({
      ...old,
      customRange: false,
      start: "0",
      end: "",
    }));
  }, [disk?.id, partition]);
  const canScan =
    physical &&
    !!disk.hardware?.identity &&
    (mode === "signatures" || !!partition);
  useEffect(() => {
    setPartition(String(fixedPartition ?? ""));
    setDestination("");
    setDestinationPartition("");
    setDiskCloneSizes({});
    setError("");
  }, [disk?.id, fixedPartition]);
  useEffect(() => {
    setMemberKeys([]);
    setAllowMissingMembers(false);
  }, [disk?.id, partition]);
  useEffect(() => {
    callbacks.current.onBusy(!!busy || !!imageDraft || imageCopyBusy);
  }, [busy, imageDraft, imageCopyBusy]);
  useEffect(() => () => callbacks.current.onBusy(false), []);
  useEffect(() => {
    if (!native) return;
    let alive = true;
    let polling = false;
    setScanId("");
    const poll = async () => {
      if (polling) return;
      polling = true;
      try {
        const [next, backup, liveScan, savedProgress, image, imageLvm, raid] =
          await Promise.all([
            invoke<Job | null>("data_status"),
            invoke<BackupInfo | null>("data_backup_status"),
            page === "files"
              ? invoke<string | null>("data_browse_id", {
                  source: selection(disk, String(fixedPartition ?? "")),
                })
              : invoke<ScanStatus | null>("data_scan_status"),
            invoke<RecoveryProgress | null>("data_recovery_progress_status"),
            invoke<ImageBrowseInfo | null>("data_image_status"),
            invoke<ImageLvmInfo | null>("data_image_lvm_status"),
            invoke<RaidView | null>("data_virtual_raid_status"),
          ]);
        if (!alive) return;
        setJob(next);
        setBackupInfo(backup);
        setImageInfo(image);
        setImageLvmInfo(imageLvm);
        setRaidInfo(raid);
        setScanId(
          typeof liveScan === "string" ? liveScan : (liveScan?.id ?? ""),
        );
        setScanStatus(typeof liveScan === "object" ? liveScan : null);
        setRecoveryProgress(savedProgress);
        if (
          next &&
          next.id === previousJob.current &&
          lastState.current === "running" &&
          next.state !== "running"
        )
          void callbacks.current.onRefresh();
        previousJob.current = next?.id ?? "";
        lastState.current = next?.state ?? "";
      } catch (e) {
        if (alive) setError(errorText(e));
      } finally {
        polling = false;
      }
    };
    void poll();
    const timer = setInterval(() => void poll(), 750);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [
    page,
    disk?.hardware?.identity,
    disk?.hardware?.layoutKey,
    fixedPartition,
  ]);
  useEffect(() => {
    if (!preview || preview.truncated || !previewImageMime(preview.name)) {
      setPreviewUrl("");
      return;
    }
    const url = URL.createObjectURL(
      new Blob([new Uint8Array(preview.bytes)], {
        type: previewImageMime(preview.name)!,
      }),
    );
    setPreviewUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [preview]);
  async function action(work: () => Promise<void>) {
    if (busy) return;
    setPending(true);
    setError("");
    try {
      await work();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setPending(false);
    }
  }
  async function startScan() {
    await action(async () => {
      if (staleMembers)
        throw Error(t("所选 Btrfs 成员已变化，请清除后重新选择。"));
      setProgressNotice("");
      const source = selection(disk, partition);
      const scanMode = page === "files" ? "existing" : mode;
      let id: string;
      if (scanMode === "reconstruct") {
        id = await invoke<string>("data_reconstruct_filesystem", {
          request: { type: "partition", selection: source },
          mode: "allFiles",
        });
      } else if (selectedPart?.encryption && scanMode !== "signatures") {
        id = await invoke<string>("data_scan_bitlocker", {
          source,
          mode: scanMode,
        });
      } else if (useMembers) {
        id = await invoke<string>("data_scan_btrfs_members", {
            members: [
              source,
              ...selectedMembers.map((member) => member.source),
            ],
            mode: scanMode,
            allowMissing: allowMissingMembers,
          });
      } else {
        id = await invoke<string>("data_scan", {
            source,
            mode: scanMode,
            signatures:
              scanMode === "signatures"
                ? signatureRequest(signatureSettings, sourceSize)
                : null,
          });
      }
      setScanId(id);
      setOwnJobId(id);
      setJob(await invoke<Job>("data_status"));
    });
  }
  async function readReconstructed(source: () => ReconstructionSource, mode: ImageBrowseMode) {
    await action(async () => {
      setProgressNotice("");
      const request = source();
      const id = await invoke<string>("data_reconstruct_filesystem", { request, mode });
      setOwnJobId(id);
      if (request.type === "partition") setScanId(id);
      setJob(await invoke<Job>("data_status"));
    });
  }
  async function loadProgress() {
    await action(async () => {
      const loaded = await invoke<RecoveryProgress | null>(
        "data_recovery_progress_load",
        { source: selection(disk, partition) },
      );
      if (!loaded) return;
      setRecoveryProgress(loaded);
      setMode(loaded.mode === "existing" ? "allFiles" : loaded.mode);
      if (loaded.engine === "signatures")
        setSignatureSettings(settingsFromProgress(loaded));
      setScanId(loaded.id);
      setOwnJobId(loaded.id);
      setProgressNotice("");
      setJob(await invoke<Job>("data_status"));
    });
  }
  async function prepare(
    recoveryIds?: number[],
    layout: RecoveryLayout = "tree",
    folder?: string,
    exportOptions: ExportOptions = defaultExportOptions,
  ) {
    await action(async () => {
      let request: unknown;
      if (recoveryIds)
        request = {
          type: "recover",
          scanId: resultsScanId,
          fileIds: recoveryIds,
          folder: folder ?? null,
          layout,
          options: exportOptions,
        };
      else if (task === "backup")
        request = {
          type: "backup",
          source: selection(disk, partition),
          options: {
            compression: compression !== "none",
            compressionLevel: compression,
            encryption,
            incremental,
            hot: hot && canHot,
            mode: copyMode,
          },
        };
      else if (task === "restore")
        request = { type: "restore", destination: selection(disk, partition) };
      else if (cloningDiskFiles)
        request = {
          type: "diskFileClone",
          source: selection(disk, ""),
          destination: selection(destinationDisk, ""),
          partitionSizes,
        };
      else if (specialClone)
        request = {
          type: cloningFiles ? "fileClone" : "resizeClone",
          source: selection(disk, partition),
          destination: selection(destinationDisk, destinationPartition),
        };
      else
        request = {
          type: "clone",
          mode: copyMode,
          source: selection(disk, partition),
          destination: selection(destinationDisk, destinationPartition),
        };
      setConfirmed(false);
      setPlan(await invoke<DataPlan | null>("data_prepare", { request }));
    });
  }
  async function execute() {
    if (!plan) return;
    const token = plan.token;
    setPlan(null);
    await action(async () => {
      setOwnJobId(await invoke<string>("data_execute", { token }));
      setJob(await invoke<Job>("data_status"));
    });
  }
  function closePlan() {
    if (plan)
      void invoke("data_discard_plan", { token: plan.token }).catch((e) =>
        setError(String(e)),
      );
    setPlan(null);
  }
  async function showPreview(c: Candidate) {
    await action(async () =>
      setPreview(
        await invoke<Preview>("data_preview", {
          scanId: resultsScanId,
          fileId: c.id,
        }),
      ),
    );
  }
  const sourceSelector = (target = false) => (
    <div className="data-source-card">
      <HardDrive size={25} />
      <div>
        <span>{target ? t("还原目标") : t("所选磁盘")}</span>
        <strong>
          {disk ? diskTitle(disk, t) : t("请选择磁盘")}
          {disk?.hardware?.model ? ` · ${disk.hardware.model}` : ""}
        </strong>
      </div>
      <select
        aria-label={t("选择数据范围")}
        value={partition}
        disabled={busy || !physical}
        onChange={(e) => {
          setPartition(e.target.value);
          setMemberKeys([]);
          setDestinationPartition("");
        }}
      >
        <option value="">
          {t("整个磁盘")} · {formatSize(disk?.size ?? 0)}
        </option>
        {parts.map((p) => (
          <option key={p.id} value={p.number}>
            {t("分区 {number}", { number: p.number! })} ·{" "}
            {partitionTitle(p, catalogText)} · {formatSize(p.size)} ·{" "}
            {catalogText(p.fs)}
          </option>
        ))}
      </select>
    </div>
  );
  const memberSelector = allowMemberSelection && (
    <details className="btrfs-member-selector">
      <summary>
        {t("其他 Btrfs 成员")}
        {memberKeys.length > 0 && (
          <>
            {" "}
            · {t("已选中")} {memberKeys.length}
          </>
        )}
        {allowMissingMembers && <> · {t("缺盘读取")}</>}
      </summary>
      <p>{t("选择同一 Btrfs 卷的其他可用成员。")}</p>
      {staleMembers && (
        <FormError message={t("所选 Btrfs 成员已变化，请清除后重新选择。")} />
      )}
      <div className="btrfs-member-options">
        <label>
          <input
            type="checkbox"
            checked={allowMissingMembers}
            disabled={busy}
            onChange={(event) =>
              setAllowMissingMembers(event.currentTarget.checked)
            }
          />
          <span>{t("允许缺盘读取（仅限可校验的数据）")}</span>
        </label>
        {memberOptions.map(({ key, partition: p, disk: memberDisk }) => (
          <label key={key}>
            <input
              type="checkbox"
              checked={memberKeys.includes(key)}
              disabled={busy}
              onChange={(event) => {
                const checked = event.currentTarget.checked;
                setMemberKeys((old) =>
                  checked ? [...old, key] : old.filter((item) => item !== key),
                );
              }}
            />
            <span>
              {diskTitle(memberDisk, t)} ·{" "}
              {t("分区 {number}", { number: p.number! })} ·{" "}
              {partitionTitle(p, catalogText)} · {formatSize(p.size)}
            </span>
          </label>
        ))}
      </div>
      {memberKeys.length > 0 && (
        <button
          className="button text-button"
          disabled={busy}
          onClick={() => setMemberKeys([])}
        >
          {t("清除 Btrfs 成员选择")}
        </button>
      )}
    </details>
  );
  return (
    <div className="data-page">
      <header className="data-heading">
        <div className="data-heading-icon">
          {page === "files" ? (
            <FolderOpen size={29} />
          ) : page === "recovery" ? (
            <FileSearch size={29} />
          ) : (
            <Archive size={29} />
          )}
        </div>
        <div>
          <h1>
            {page === "files"
              ? t("浏览文件")
              : page === "recovery"
                ? t("文件恢复")
                : t("备份与克隆")}
          </h1>
          <p>
            {page === "files"
              ? [disk && diskTitle(disk, t), t("按目录查看分区中的现存文件。")]
                  .filter(Boolean)
                  .join(" · ")
              : page === "recovery"
                ? t("扫描丢失的文件，预览后恢复到另一块磁盘。")
                : t("为重要数据创建副本，并在需要时还原。")}
          </p>
        </div>
        <span className="data-source-tag">
          <ShieldCheck size={14} />
          {page !== "backup" ? t("源盘只读") : t("SHA-256 校验")}
        </span>
      </header>
      {page === "backup" && (
        <div className="backup-navigation">
          <button
            className={`button ${!browsingBackup && !browsingImage ? "active" : ""}`}
            aria-pressed={!browsingBackup && !browsingImage}
            disabled={busy}
            onClick={() => {
              setBrowsingBackup(false);
              setBrowsingImage(false);
            }}
          >
            {t("备份与还原")}
          </button>
          <button
            className={`button ${browsingBackup ? "active" : ""}`}
            aria-pressed={browsingBackup}
            disabled={busy}
            onClick={() => {
              setBrowsingBackup(true);
              setBrowsingImage(false);
            }}
          >
            <FolderOpen size={16} />
            {t("浏览备份")}
          </button>
          <button
            className={`button ${browsingImage ? "active" : ""}`}
            aria-pressed={browsingImage}
            disabled={busy}
            onClick={() => {
              setBrowsingImage(true);
              setBrowsingBackup(false);
            }}
          >
            <FolderOpen size={16} />
            {t("浏览镜像")}
          </button>
        </div>
      )}
      {resultsScanId &&
        (page !== "backup" || browsingBackup || browsingImage) && (
          <div className="source-summary-toggle">
            <span>{t("来源与读取选项")}</span>
            <button
              className="button text-button"
              aria-expanded={sourceExpanded}
              onClick={() => setSourceExpanded((v) => !v)}
            >
              {sourceExpanded ? t("收起") : t("展开")}
            </button>
          </div>
        )}
      <div
        className={`data-source-options ${resultsScanId && !sourceExpanded && (page !== "backup" || browsingBackup || browsingImage) ? "collapsed" : ""}`}
      >
        {page === "files" ? (
          <PartitionBrowserPanel
            partition={selectedPart}
            busy={!!busy}
            available={native && !!physical && !staleMembers}
            loaded={!!resultsScanId}
            onRead={() => void startScan()}
            onReconstruct={() => void readReconstructed(() => ({ type: "partition", selection: selection(disk, partition) }), "existing")}
            onBack={() => onBack?.()}
          >
            {memberSelector}
          </PartitionBrowserPanel>
        ) : page === "recovery" ? (
          <>
            {lostSource && (
              <section className="data-panel" aria-label={t("找到的分区来源")}>
                <div className="section-heading">
                  <h2>
                    {lostSource.members > 1
                      ? `${t("联合浏览 Btrfs 成员")} · ${lostSource.members}`
                      : `${t("找到的分区")} · ${lostSource.filesystem} · ${formatSize(lostSource.size)}`}
                  </h2>
                </div>
                <p>{scanStatus?.sourceName}</p>
                <div className="data-action-row">
                  <span>
                    {t("当前结果来自此候选中的文件与目录；尚未修改分区表。")}
                  </span>
                  <button
                    className="button"
                    disabled={busy}
                    onClick={() => setChooseNewSource((v) => !v)}
                  >
                    {chooseNewSource ? t("收起扫描选项") : t("扫描其他位置")}
                  </button>
                </div>
              </section>
            )}
            {(!lostSource || chooseNewSource) && (
              <>
                {sourceSelector()}
                {mode !== "signatures" && mode !== "reconstruct" && memberSelector}
                <section className="data-panel">
                  <div className="section-heading">
                    <h2>{t("选择扫描方式")}</h2>
                    <span className="section-caption">
                      {t("扫描不会修改源盘")}
                    </span>
                    <button
                      className="button"
                      disabled={busy || !native}
                      onClick={() => {
                        setError("");
                        setLvmView(null);
                        setLvmOpen(true);
                      }}
                    >
                      {t("浏览 LVM2 逻辑卷")}
                    </button>
                    <button
                      className="button"
                      disabled={busy || !native}
                      onClick={() => {
                        setError("");
                        setRaidOpen(true);
                      }}
                    >
                      {t("虚拟 RAID")}
                    </button>
                  </div>
                  <div className="scan-modes">
                    {[
                      {
                        id: "deleted",
                        title: t("找回删除文件"),
                        body: t(
                          "从保留记录、Btrfs 只读快照或旧事务查找删除候选。",
                        ),
                      },
                      {
                        id: "allFiles",
                        title: t("扫描全部文件"),
                        body: t(
                          "列出文件和目录，包括干净卸载的 Ext2/3/4 和 Btrfs。",
                        ),
                      },
                      {
                        id: "reconstruct",
                        title: t("重建文件视图"),
                        body: t("使用 NTFS、FAT32 或 exFAT 的备用元数据读取文件，提取前重新核对。"),
                      },
                      {
                        id: "signatures",
                        title: t("按文件签名扫描"),
                        body: t(
                          "按文件结构查找图片、文档、音频与 ZIP，可选择类型和范围",
                        ),
                      },
                    ].map((m) => (
                      <button
                        className={`scan-mode ${mode === m.id ? "selected" : ""}`}
                        key={m.id}
                        disabled={busy}
                        onClick={() => setMode(m.id)}
                        aria-pressed={mode === m.id}
                      >
                        <span className="mode-radio" />
                        <strong>{m.title}</strong>
                        <p>{m.body}</p>
                      </button>
                    ))}
                  </div>
                  {mode === "signatures" && (
                    <>
                      <SignatureOptions
                        value={signatureSettings}
                        onChange={setSignatureSettings}
                        disabled={!!busy}
                      />
                      {signatureError && (
                        <FormError message={catalogText(signatureError)} />
                      )}
                    </>
                  )}
                  <div className="data-action-row">
                    {mode !== "reconstruct" && <span>
                      {mode === "signatures"
                        ? t(
                            "支持上列 11 类；BMP 限未压缩格式，WAV 限 RIFF/WAVE。ZIP/Office 限单卷未加密 Stored/Deflate，逐项验证 CRC；不含旧版 Office 或宏类型识别。单文件上限 256 MiB，不重组碎片。",
                          )
                        : selectedPart?.encryption
                          ? t("请在 Windows 解锁已完成加密的 BitLocker NTFS 数据卷，再读取目录。")
                        : mode === "allFiles"
                          ? t(
                              "NTFS、FAT、exFAT 支持现存文件和删除候选。Btrfs 可从只读快照或保留事务查找删除候选。Ext2/3/4 支持干净卸载卷的现存文件。",
                            )
                          : t(
                              "支持 NTFS（含常规 LZNT1 压缩与普通文件的命名数据流）、FAT12/16/32 和 exFAT。命名数据流单独导出为普通文件；EFS、WOF 与云端占位文件暂不支持，删除内容可能已被覆盖。",
                            ) +
                            " " +
                            t(
                              "Btrfs 删除恢复需要可核对的只读快照或保留事务，且提取时必须通过数据校验。",
                            )}
                    </span>}
                    {!useMembers && mode !== "reconstruct" && (
                      <button
                        className="button"
                        disabled={
                          busy ||
                          !physical ||
                          !disk?.hardware?.identity ||
                          !native
                        }
                        onClick={() => void loadProgress()}
                      >
                        <FolderOpen size={16} />
                        {t("加载进度")}
                      </button>
                    )}
                    {job?.state === "running" &&
                    recoveryProgress?.id === job.id ? (
                      <button
                        className="button primary"
                        onClick={() =>
                          void invoke("data_recovery_progress_pause", {
                            id: job.id,
                          }).catch((e) => setError(String(e)))
                        }
                      >
                        <Pause size={16} />
                        {t("暂停扫描")}
                      </button>
                    ) : (
                      <button
                        className="button primary"
                        disabled={
                          busy ||
                          !canScan ||
                          !native ||
                          !!signatureError ||
                          staleMembers
                        }
                        onClick={() => void startScan()}
                      >
                        <Search size={16} />
                        {t("开始扫描")}
                      </button>
                    )}
                  </div>
                </section>
              </>
            )}
          </>
        ) : browsingImage ? (
          <ImageBrowserPanel
            info={imageInfo}
            busy={!!busy}
            available={native}
            onCopy={() => {
              if (imageInfo) setImageCopy({ id: imageInfo.id, partition: "" });
            }}
            lvmName={imageLvmInfo?.name}
            raidName={raidInfo?.name}
            onRaid={() => {
              setError("");
              setRaidOpen(true);
            }}
            onLvm={() => {
              setError("");
              setImageLvmOpen(true);
            }}
            onOpen={(options) =>
              void action(async () => {
                const group = await invoke<NativeImageGroup | null>(
                  "image_source_select",
                );
                if (group) setImageDraft({ group, options });
              })
            }
            onClose={() =>
              void action(async () => {
                if (imageInfo)
                  await invoke("data_close_image", { id: imageInfo.id });
                setImageInfo(null);
              })
            }
            onRead={(partition, mode) =>
              void action(async () => {
                if (!imageInfo) return;
                setOwnJobId(
                  await invoke<string>("data_scan_image", {
                    request: { id: imageInfo.id, partition, mode },
                  }),
                );
                setImageInfo(
                  await invoke<ImageBrowseInfo>("data_image_status"),
                );
                setJob(await invoke<Job>("data_status"));
              })
            }
            onReconstruct={(partition, mode) => {
              if (imageInfo) void readReconstructed(() => ({ type: "image", id: imageInfo.id, partition }), mode);
            }}
          />
        ) : browsingBackup ? (
          <BackupBrowserPanel
            info={backupInfo}
            busy={!!busy}
            available={native}
            onOpen={() =>
              void action(async () => {
                await invoke<string | null>("data_open_backup");
                setJob(await invoke<Job | null>("data_status"));
              })
            }
            onClose={() =>
              void action(async () => {
                if (backupInfo)
                  await invoke("data_close_backup", { id: backupInfo.id });
                setBackupInfo(null);
              })
            }
            onRead={(partition) =>
              void action(async () => {
                if (!backupInfo) return;
                await invoke<string>("data_scan_backup", {
                  request: { id: backupInfo.id, partition },
                });
                setBackupInfo(await invoke<BackupInfo>("data_backup_status"));
                setJob(await invoke<Job>("data_status"));
              })
            }
            onReconstruct={(partition) => {
              if (backupInfo) void readReconstructed(() => ({ type: "backup", id: backupInfo.id, partition }), "allFiles");
            }}
            onPoint={(point) =>
              void action(async () => {
                if (!backupInfo || point === backupInfo.selectedPoint) return;
                await invoke<string>("data_select_backup_point", {
                  request: { id: backupInfo.id, point },
                });
                setJob(await invoke<Job>("data_status"));
              })
            }
            onRestore={(partition) => {
              if (!backupInfo) return;
              setError("");
              setBackupRestore({ id: backupInfo.id, partition });
            }}
          />
        ) : (
          <>
            <div className="data-task-options">
              {options.map((o) => (
                <button
                  key={o.id}
                  className={`data-task-card ${task === o.id ? "selected" : ""}`}
                  onClick={() => setTask(o.id)}
                  disabled={busy}
                >
                  <o.icon size={23} />
                  <strong>{t(o.name)}</strong>
                  <span>{t(o.description)}</span>
                  {task === o.id && (
                    <CheckCircle2 size={16} className="task-selected" />
                  )}
                </button>
              ))}
            </div>
            {sourceSelector(task === "restore")}
            <section className="data-panel">
              <div className="section-heading">
                <h2>
                  {task === "backup"
                    ? t("备份选项")
                    : task === "restore"
                      ? t("选择备份并核对目标")
                      : t("选择克隆目标")}
                </h2>
                <span className="section-caption">
                  {formatSize(sourceSize)}
                </span>
              </div>
              {task !== "restore" && (
                <fieldset className="copy-mode-options" disabled={!!busy}>
                  <legend>{t("复制内容")}</legend>
                  {(
                    [
                      [
                        "allSectors",
                        t("逐扇区"),
                        t("包含空闲空间，保留完整扇区内容"),
                      ],
                      [
                        "usedSpace",
                        t("按已用空间"),
                        t("检查分配信息，跳过确认空闲的区域"),
                      ],
                    ] as const
                  ).map(([value, title, detail]) => (
                    <label key={value}>
                      <input
                        type="radio"
                        name="copy-mode"
                        value={value}
                        checked={!specialClone && copyMode === value}
                        onChange={() => {
                          setCopyMode(value);
                          if (specialClone) setDestinationPartition("");
                          if (task === "clone") setPartitionClone("standard");
                        }}
                      />
                      <span>
                        <strong>{title}</strong>
                        <small>{detail}</small>
                      </span>
                    </label>
                  ))}
                  {canResizeClone && (
                    <label>
                      <input
                        type="radio"
                        name="copy-mode"
                        value="resize"
                        checked={resizingClone}
                        onChange={() => {
                          setPartitionClone("resize");
                          setDestinationPartition("");
                        }}
                      />
                      <span>
                        <strong>{t("调整容量克隆")}</strong>
                        <small>{linuxSource
                          ? t("核对干净卸载的单设备 Btrfs，并适配目标分区容量")
                          : t("复制 FAT 或 exFAT 文件系统，并适配目标分区容量")}</small>
                      </span>
                    </label>
                  )}
                  {canFileClone && (
                    <label>
                      <input
                        type="radio"
                        name="copy-mode"
                        value="files"
                        checked={cloningFiles}
                        onChange={() => {
                          setPartitionClone("files");
                          setDestinationPartition("");
                        }}
                      />
                      <span>
                        <strong>{t("按文件克隆")}</strong>
                        <small>{t("在新 NTFS、FAT 或 exFAT 卷重建活动文件和目录")}</small>
                      </span>
                    </label>
                  )}
                  {canDiskFileClone && (
                    <label>
                      <input
                        type="radio"
                        name="copy-mode"
                        value="diskFiles"
                        checked={cloningDiskFiles}
                        onChange={() => {
                          setPartitionClone("diskFiles");
                          setDestination("");
                          setDestinationPartition("");
                        }}
                      />
                      <span>
                        <strong>{t("整盘按文件克隆")}</strong>
                        <small>{t("在空目标盘重建 GPT，按文件复制或逐扇区保留数据分区")}</small>
                      </span>
                    </label>
                  )}
                </fieldset>
              )}
              {task !== "restore" && !specialClone && copyMode === "usedSpace" && (
                <p className="copy-mode-note">
                  {t(
                    "检查 NTFS、FAT、exFAT、Ext2/3/4 和单设备 Btrfs 的分配信息。多设备 Btrfs 和未识别的文件系统完整保留。",
                  )}{" "}
                  {task === "backup"
                    ? t("备份中的空闲区按零保存，不保留这些区域的已删除内容。")
                    : t("保持源分区布局，目标空闲区不会被清除。")}
                </p>
              )}
              {task === "backup" && (
                <div className="backup-settings">
                  <label>
                    <span>
                      <strong>{t("备份方式")}</strong>
                      <small>
                        {t("增量只保存变化的数据块，还原需要完整依赖链")}
                      </small>
                    </span>
                    <select
                      aria-label={t("备份方式")}
                      value={incremental ? "incremental" : "full"}
                      disabled={busy}
                      onChange={(e) =>
                        setIncremental(e.target.value === "incremental")
                      }
                    >
                      <option value="full">{t("完整备份")}</option>
                      <option value="incremental">{t("增量备份")}</option>
                    </select>
                  </label>
                  <label>
                    <span>
                      <strong>{t("压缩程度")}</strong>
                      <small>
                        {t("压缩使用多个 CPU 线程；高压缩通常需要更长时间")}
                      </small>
                    </span>
                    <select
                      aria-label={t("压缩程度")}
                      value={compression}
                      disabled={busy}
                      onChange={(e) =>
                        setCompression(e.target.value as typeof compression)
                      }
                    >
                      <option value="none">{t("不压缩")}</option>
                      <option value="fast">{t("快速")}</option>
                      <option value="balanced">{t("均衡")}</option>
                      <option value="high">{t("高压缩")}</option>
                    </select>
                  </label>
                  <label>
                    <span>
                      <strong>{t("密码保护")}</strong>
                      <small>
                        {t("开始时设置密码，打开和还原备份时需要使用")}
                      </small>
                    </span>
                    <input
                      type="checkbox"
                      checked={encryption}
                      disabled={busy}
                      onChange={(e) => setEncryption(e.target.checked)}
                    />
                  </label>
                  <label>
                    <span>
                      <strong>{t("VSS 热备份")}</strong>
                      <small>
                        {copyMode === "usedSpace"
                          ? t(
                              "按已用空间备份需要锁定源卷；切换逐扇区方式可用热备份",
                            )
                          : canHot
                            ? t(
                                "源卷可继续使用；保证文件系统一致性，不保证应用事务一致性",
                              )
                            : t(
                                "选择在线 NTFS 数据分区后可用；整盘备份仍需锁定源卷",
                              )}
                      </small>
                    </span>
                    <input
                      type="checkbox"
                      checked={hot && canHot}
                      disabled={busy || !canHot}
                      onChange={(e) => setHot(e.target.checked)}
                    />
                  </label>
                </div>
              )}
              {task === "clone" ? (
                <>
                  <div className="clone-flow">
                    <div>
                      <span>{t("源")}</span>
                      <strong>
                        {disk ? diskTitle(disk, t) : t("未选择")}
                        {selectedPart
                          ? ` / ${partitionTitle(selectedPart, catalogText)}`
                          : ` / ${t("整个磁盘")}`}
                      </strong>
                      <small>{formatSize(sourceSize)}</small>
                    </div>
                    <ArrowRight size={22} />
                    <div>
                      <label htmlFor="clone-disk">{t("目标磁盘")}</label>
                      <select
                        id="clone-disk"
                        disabled={busy}
                        value={cloneDiskSelected ? destination : ""}
                        onChange={(e) => {
                          setDestination(e.target.value);
                          setDestinationPartition("");
                        }}
                      >
                        <option value="">{t("选择另一块数据磁盘")}</option>
                        {cloneDisks.map((d) => (
                          <option key={d.id} value={d.id}>
                            {diskTitle(d, t)} · {d.hardware?.model} ·{" "}
                            {formatSize(d.size)}
                          </option>
                        ))}
                      </select>
                      {partition && (
                        <select
                          aria-label={t("克隆目标分区")}
                          disabled={busy}
                          value={destinationPartition}
                          onChange={(e) =>
                            setDestinationPartition(e.target.value)
                          }
                        >
                          <option value="">
                            {cloningFiles
                              ? t("选择接收分区")
                              : resizingClone
                                ? t("选择不同容量的目标分区")
                                : t("选择同容量的目标分区")}
                          </option>
                          {cloneTargets.map((p) => (
                              <option key={p.id} value={p.number}>
                                {t("分区 {number}", { number: p.number! })} ·{" "}
                                {partitionTitle(p, catalogText)} ·{" "}
                                {formatSize(p.size)}
                              </option>
                            ))}
                        </select>
                      )}
                    </div>
                  </div>
                  {cloningDiskFiles && (
                    <fieldset className="disk-clone-sizes" disabled={!!busy}>
                      <legend>{t("目标数据分区容量（MiB）")}</legend>
                      {parts.filter(resizableDiskClonePart).map((part) => (
                        <label key={part.id}>
                          <span>
                            {t("分区 {number}", { number: part.number! })} ·{" "}
                            {linuxDiskClonePart(part)
                              ? t("Linux 数据（调整容量需核对为 Btrfs）")
                              : catalogText(part.fs)}
                          </span>
                          <input
                            type="number"
                            min={1}
                            step={1}
                            placeholder={formatSize(part.size)}
                            value={diskCloneSizes[part.number!] ?? ""}
                            onChange={(e) => {
                              const value = e.target.value;
                              setDiskCloneSizes((old) => ({ ...old, [part.number!]: value }));
                            }}
                          />
                        </label>
                      ))}
                    </fieldset>
                  )}
                  <Hint>
                    {cloningDiskFiles
                      ? t("容量留空则沿用源分区大小。选择逻辑扇区大小相同的空 RAW 或 GPT 盘，写入后目标保持脱机。")
                      : specialClone
                      ? t("源与目标必须使用相同的逻辑扇区大小。目标保持脱机，两盘的 GPT 分区表保持原样。")
                      : t("源与目标容量、逻辑扇区大小必须一致。整个目标磁盘会保持脱机，避免副本中的重复标识自动挂载。")}
                  </Hint>
                </>
              ) : (
                <div className="backup-explanation">
                  <div className="backup-step">
                    <span>1</span>
                    <p>
                      {task === "backup"
                        ? t("选择另一块磁盘上的保存位置")
                        : t("打开 DiskStudio 的 .dsbackup 备份文件")}
                    </p>
                  </div>
                  <div className="backup-step">
                    <span>2</span>
                    <p>
                      {task === "backup"
                        ? hot
                          ? t("创建 VSS 快照，读取时间点副本")
                          : t("锁定源卷，读取一致的数据副本")
                        : t("校验备份完整性，确认覆盖范围")}
                    </p>
                  </div>
                  <div className="backup-step">
                    <span>3</span>
                    <p>
                      {task === "backup"
                        ? t("读取备份并核对 SHA-256")
                        : t("覆盖目标后再次读取并校验，目标保持脱机")}
                    </p>
                  </div>
                </div>
              )}
              <div className="data-action-row">
                <span>
                  {task === "backup"
                    ? incremental
                      ? t("先选择上次备份及其依赖，再选择新的保存位置。")
                      : copyMode === "usedSpace"
                        ? t("空闲区按零保存，完成后校验全部逻辑内容。")
                        : t("完整备份包含空闲空间；开启压缩可减少占用。")
                    : task === "restore"
                      ? t("还原要求备份类型、容量及逻辑扇区与目标一致。")
                      : t("目标原有文件系统及数据将被替换，无法撤销。")}
                </span>
                <button
                  className="button primary"
                  disabled={
                    busy ||
                    !physical ||
                    !native ||
                    (task === "clone" &&
                      (!cloneDiskSelected || (cloningDiskFiles && invalidDiskSizes) || (!!partition && !cloneTargets.some(
                        (part) => String(part.number) === destinationPartition,
                      ))))
                  }
                  onClick={() => void prepare()}
                >
                  <FolderOpen size={16} />
                  {task === "backup"
                    ? t("选择保存位置…")
                    : task === "restore"
                      ? t("选择备份文件…")
                      : t("预览克隆")}
                </button>
              </div>
            </section>
            <p className="data-footnote">
              {t(
                "复制采用有界并行流水线，写入按顺序执行并完整回读校验。VSS 仅支持 NTFS 数据分区。",
              )}
            </p>
          </>
        )}
      </div>
      {page !== "backup" && recoveryProgress && matchingProgress && (
        <section
          className="data-panel signature-progress"
          aria-label={t("恢复扫描进度")}
        >
          <div>
            <strong>
              {recoveryProgress.loaded
                ? t("已加载的扫描进度")
                : t("恢复扫描进度")}{" "}
              · {progressMode(recoveryProgress, language)}
            </strong>
            <p>
              {progressAmount(recoveryProgress, language)} ·{" "}
              {t("{count} 个候选", {
                count: recoveryProgress.found.toLocaleString(language),
              })}
            </p>
            <small>
              {t(
                "续扫沿用保存时的模式与范围。保存进度后可关闭程序，下次加载继续。",
              )}
            </small>
          </div>
          <div className="signature-progress-actions">
            <button
              className="button"
              disabled={busy || !recoveryProgress.canSave}
              onClick={() =>
                void action(async () => {
                  const name = await invoke<string | null>(
                    "data_recovery_progress_save",
                    { id: recoveryProgress.id },
                  );
                  if (name) setProgressNotice(t("已保存：{name}", { name }));
                })
              }
            >
              <Save size={16} />
              {t("保存进度")}
            </button>
            <button
              className="button primary"
              disabled={busy || !recoveryProgress.canResume}
              onClick={() =>
                void action(async () => {
                  const id = await invoke<string>(
                    "data_recovery_progress_resume",
                    {
                      id: recoveryProgress.id,
                      source: selection(disk, partition),
                    },
                  );
                  setMode(
                    recoveryProgress.mode === "existing"
                      ? "allFiles"
                      : recoveryProgress.mode,
                  );
                  if (recoveryProgress.engine === "signatures")
                    setSignatureSettings(
                      settingsFromProgress(recoveryProgress),
                    );
                  setScanId(id);
                  setOwnJobId(id);
                  setProgressNotice("");
                  setJob(await invoke<Job>("data_status"));
                })
              }
            >
              <Play size={16} />
              {t("继续扫描")}
            </button>
          </div>
          {progressNotice && <p role="status">{progressNotice}</p>}
        </section>
      )}
      <FormError message={catalogText(error)} />
      {pending && !plan && (
        <div className="data-wait">
          <LoaderCircle className="spin" size={17} />
          {t("正在准备，请完成文件选择或等待检查…")}
        </div>
      )}
      {job &&
        (page !== "files" ||
          job.state === "running" ||
          job.id === resultsScanId ||
          job.id === ownJobId) && (
          <section
            className={`data-job ${job.state}`}
            aria-label={t("数据任务进度")}
          >
            <div className="data-job-heading">
              <span>
                {job.state === "running" ? (
                  <LoaderCircle size={18} className="spin" />
                ) : job.state === "completed" ? (
                  <CheckCircle2 size={18} />
                ) : (
                  <Square size={16} />
                )}
                <strong>{catalogText(job.title)}</strong>
                <small>{catalogText(job.phase)}</small>
              </span>
              {job.state === "running" &&
                (recoveryProgress?.id === job.id ? (
                  <button
                    className="button"
                    onClick={() =>
                      void invoke("data_recovery_progress_pause", {
                        id: job.id,
                      }).catch((e) => setError(String(e)))
                    }
                  >
                    <Pause size={14} />
                    {t("暂停扫描")}
                  </button>
                ) : (
                  <button
                    className="button"
                    onClick={() =>
                      void invoke("data_cancel", { id: job.id }).catch((e) =>
                        setError(String(e)),
                      )
                    }
                  >
                    <Square size={13} />
                    {t("取消任务")}
                  </button>
                ))}
            </div>
            <progress
              value={
                job.total
                  ? job.done
                  : job.state === "completed"
                    ? 1
                    : job.state === "running"
                      ? undefined
                      : 0
              }
              max={job.total || 1}
            />
            <div className="data-progress-meta">
              <span>
                {job.total
                  ? `${Math.min(100, Math.round((job.done / job.total) * 100))}%`
                  : job.state === "completed"
                    ? t("已完成")
                    : job.state !== "running"
                      ? catalogText(job.phase)
                      : job.done > 0
                        ? t("正在扫描")
                        : t("正在准备")}
              </span>
              <span>
                {job.title === "文件恢复扫描" ||
                job.title === "读取分区目录" ||
                job.title === "读取备份目录" ||
                job.title === "读取镜像目录" ||
                job.title === "读取 Btrfs 成员" ||
                job.title === "读取 LVM2 逻辑卷" ||
                job.title === "读取虚拟 RAID 目录" ||
                job.title === "读取找到的分区目录" ||
                job.title === "重建文件视图"
                  ? t("找到 {count} 个文件", {
                      count: job.found.toLocaleString(language),
                    })
                  : `${formatSize(job.done)} / ${formatSize(job.total)}`}
              </span>
            </div>
            {job.message && (
              <p className="job-message">
                {readablePaths(catalogText(job.message))}
              </p>
            )}
            {job.sha256 && (
              <div className="job-checksum">
                <ShieldCheck size={14} />
                <span>SHA-256 {job.sha256}</span>
              </div>
            )}
          </section>
        )}
      {resultsScanId && (
        <RecoveryResults
          key={resultsScanId}
          scanId={resultsScanId}
          revision={`${job?.found ?? 0}-${job?.state ?? ""}`}
          busy={!!busy}
          running={job?.state === "running"}
          browsing={
            page === "files" ||
            (page === "recovery" && raidInfo?.scanMode === "existing") ||
            (page === "backup" &&
              browsingImage &&
              (raidInfo?.scanId ? raidInfo.scanMode : imageLvmInfo?.scanId ? imageLvmInfo.scanMode : imageInfo?.scanMode) === "existing")
          }
          onRecover={(ids, layout, folder, options) =>
            void prepare(ids, layout, folder, options)
          }
          onPreview={(c) => void showPreview(c)}
        />
      )}
      {raidOpen && (page === "recovery" || (page === "backup" && browsingImage)) && (
        <VirtualRaidDialog
          key={raidEdit ? `edit-${raidEdit.request.replaces}` : raidInfo?.id}
          busy={!!busy}
          pending={pending}
          error={error}
          disks={disks}
          files={raidFiles ?? raidEdit?.sources ?? null}
          initial={raidEdit?.request}
          initialMdVersion={raidInfo?.mdVersion}
          view={raidEdit ? null : raidInfo}
          onSelect={() => void action(async () => {
            const selected = await invoke<RaidFiles | null>("data_virtual_raid_select");
            if (selected) setRaidFiles(selected);
          })}
          onSelectPhysical={(members) => void action(async () => {
            setRaidFiles(await invoke<RaidFiles>("data_virtual_raid_select_devices", {
              request: { members },
            }));
          })}
          onDetectMd={(request, apply) => void action(async () => {
            apply(await invoke<RaidDetection>("data_virtual_raid_detect_md", { request }));
          })}
          onOpen={(request) => void action(async () => {
            setRaidInfo(await invoke<RaidView>("data_virtual_raid_open", { request }));
            setRaidFiles(null);
            setRaidEdit(null);
          })}
          onEdit={() => void action(async () => {
            if (!raidInfo) return;
            setRaidEdit(await invoke<RaidEdit>("data_virtual_raid_configuration", { id: raidInfo.id }));
          })}
          onScan={(partition, mode) => void action(async () => {
            if (!raidInfo) return;
            const id = await invoke<string>("data_virtual_raid_scan", {
              request: { id: raidInfo.id, partition, mode },
            });
            setOwnJobId(id);
            setJob(await invoke<Job>("data_status"));
            setRaidInfo(await invoke<RaidView>("data_virtual_raid_status"));
            setRaidOpen(false);
          })}
          onSearch={(options) => void action(async () => {
            if (!raidInfo) return;
            setOwnJobId(await invoke<string>("data_virtual_raid_search", {
              request: { id: raidInfo.id, options },
            }));
            setJob(await invoke<Job>("data_status"));
            setRaidInfo(await invoke<RaidView>("data_virtual_raid_status"));
          })}
          onStopSearch={(id) => void action(async () => {
            await invoke("data_cancel", { id });
          })}
          onBrowseFound={(candidate, mode) => void action(async () => {
            if (!raidInfo) return;
            setOwnJobId(await invoke<string>("data_virtual_raid_browse_found", {
              request: { id: raidInfo.id, candidate, mode },
            }));
            setJob(await invoke<Job>("data_status"));
            setRaidInfo(await invoke<RaidView>("data_virtual_raid_status"));
            setRaidOpen(false);
          })}
          onClose={() => void action(async () => {
            if (raidFiles) await invoke("data_virtual_raid_discard", { id: raidFiles.id });
            setRaidFiles(null);
            if (raidEdit) setRaidEdit(null);
            else setRaidOpen(false);
          })}
          onCloseRaid={() => void action(async () => {
            if (!raidInfo) return;
            await invoke("data_virtual_raid_close", { id: raidInfo.id });
            setRaidInfo(null);
            setRaidOpen(false);
          })}
        />
      )}
      {imageInfo && imageCopy?.id === imageInfo.id && page === "backup" && browsingImage && (
        <Dialog
          title={t("复制镜像扇区")}
          subtitle={imageInfo.name}
          wide
          onClose={() => {
            if (!imageCopyBusy) setImageCopy(null);
          }}
        >
          <div className="sector-search-range">
            <label>
              {t("镜像范围")}
              <select
                value={imageCopy.partition}
                disabled={imageCopyBusy}
                onChange={(e) => setImageCopy({ id: imageInfo.id, partition: e.target.value })}
              >
                <option value="">{t("整个镜像")}</option>
                {imageInfo.options.kind === "disk" && imageInfo.partitions.map((part) => (
                  <option key={part.id} value={part.id}>
                    {part.name} · {formatSize(part.size)}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <SectorCopyPanel
            key={imageCopy.partition}
            source={{
              kind: "image",
              id: imageInfo.id,
              partition: imageCopyPart?.id ?? null,
              sourceDisks: imageInfo.sourceDisks,
            }}
            disks={disks}
            size={imageCopyPart?.size ?? imageInfo.size}
            sectorSize={imageInfo.options.sector}
            offset={imageCopyPart?.start ?? 0}
            initialSector={0}
            onBusy={setImageCopyBusy}
            onRefresh={onRefresh}
          />
        </Dialog>
      )}
      {imageLvmOpen && page === "backup" && browsingImage && (
        <ImageLvmDialog
          key={imageInfo?.id}
          image={imageInfo}
          view={imageLvmInfo}
          busy={!!busy}
          error={error}
          onClose={() => {
            setError("");
            setImageLvmOpen(false);
          }}
          onInspect={(id, partitions, allowDegradedRaid) =>
            void action(async () => {
              setImageLvmInfo(
                await invoke<ImageLvmInfo>("data_image_lvm_inspect", {
                  request: { id, partitions, allowDegradedRaid },
                }),
              );
            })
          }
          onOpenRaw={(allowDegradedRaid) =>
            void action(async () => {
              const next = await invoke<ImageLvmInfo | null>("data_image_lvm_open", { allowDegradedRaid });
              if (next) setImageLvmInfo(next);
            })
          }
          onCloseGroup={() =>
            void action(async () => {
              if (!imageLvmInfo) return;
              await invoke("data_image_lvm_close", { id: imageLvmInfo.id });
              setImageLvmInfo(null);
            })
          }
          onBrowse={(id, volume, mode) =>
            void action(async () => {
              const jobId = await invoke<string>("data_image_lvm_scan", {
                request: { id, volume, mode },
              });
              setOwnJobId(jobId);
              const [next, nextJob] = await Promise.all([
                invoke<ImageLvmInfo | null>("data_image_lvm_status"),
                invoke<Job>("data_status"),
              ]);
              setImageLvmInfo(next);
              setJob(nextJob);
              setImageLvmOpen(false);
            })
          }
        />
      )}
      {lvmOpen && page === "recovery" && (
        <LvmBrowserDialog
          disks={disks}
          primary={
            physical &&
            selectedPart &&
            !selectedPart.system &&
            disk.hardware?.identity &&
            disk.hardware.layoutKey
              ? selection(disk, partition)
              : undefined
          }
          busy={!!busy}
          error={error}
          view={lvmView}
          onReselect={() => {
            setError("");
            setLvmView(null);
          }}
          onClose={() => {
            setError("");
            setLvmOpen(false);
          }}
          onInspect={(members, allowDegradedRaid) =>
            void action(async () => {
              setLvmView(
                await invoke<LvmView>("data_lvm_inspect", {
                  request: { members, allowDegradedRaid },
                }),
              );
            })
          }
          onBrowse={(view, volume, deleted) =>
            void action(async () => {
              const id = await invoke<string>("data_lvm_browse", {
                request: { id: view.id, volume, deleted },
              });
              setProgressNotice("");
              setScanId(id);
              setOwnJobId(id);
              setJob(await invoke<Job>("data_status"));
              setLvmOpen(false);
            })
          }
        />
      )}
      {!physical &&
        !(page === "backup" && (browsingBackup || browsingImage)) && (
          <Hint>
            {t(
              "从左侧选择真实磁盘。此处不会自动扫描；导入镜像的分区表查看仍在“磁盘管理”中。",
            )}
          </Hint>
        )}
      {imageDraft && (
        <ImageBrowseOpenDialog
          initial={imageDraft.group}
          options={imageDraft.options}
          onWorking={setPending}
          onClose={() => setImageDraft(null)}
          onOpened={async (id) => {
            setOwnJobId(id);
            setJob(await invoke<Job>("data_status"));
            setImageDraft(null);
          }}
        />
      )}
      {page === "backup" && browsingBackup && backupInfo && backupRestore?.id === backupInfo.id && (
        <BackupRestoreDialog
          backup={backupInfo}
          partition={backupRestore.partition}
          disks={disks}
          busy={!!busy}
          error={error}
          onClose={() => {
            setError("");
            setBackupRestore(null);
          }}
          onPrepare={(targetDisk, targetPartition, resize) => void action(async () => {
            const next = await invoke<DataPlan | null>("data_prepare", {
              request: {
                type: "restoreBackupPoint",
                id: backupInfo.id,
                point: backupInfo.selectedPoint,
                partition: backupRestore.partition,
                destination: selection(targetDisk, targetPartition),
                resize,
              },
            });
            if (next) {
              setConfirmed(false);
              setBackupRestore(null);
              setPlan(next);
            }
          })}
        />
      )}
      {plan && (
        <Dialog
          title={catalogText(plan.title)}
          subtitle={t("核对来源、目标和操作范围")}
          onClose={closePlan}
          wide
        >
          <dl className="data-plan">
            <dt>{t("来源")}</dt>
            <dd>{readablePaths(plan.source)}</dd>
            <dt>{t("目标")}</dt>
            <dd>{readablePaths(plan.destination)}</dd>
            <dt>{t("处理容量")}</dt>
            <dd>{formatSize(plan.bytes)}</dd>
          </dl>
          <div
            className={`data-plan-warning ${plan.destructive ? "destructive" : ""}`}
          >
            {catalogText(plan.warning)}
          </div>
          <label className="data-confirm">
            <input
              type="checkbox"
              checked={confirmed}
              onChange={(e) => setConfirmed(e.target.checked)}
            />
            {plan.destructive
              ? t("我已核对目标，理解数据将被覆盖且无法撤销。")
              : t("我已核对来源、输出位置与处理范围。")}
          </label>
          <div className="dialog-actions">
            <button className="button" onClick={closePlan}>
              {t("取消")}
            </button>
            <button
              className={`button ${plan.destructive ? "danger" : "primary"}`}
              disabled={!confirmed || busy}
              onClick={() => void execute()}
            >
              {t("确认并开始")}
            </button>
          </div>
        </Dialog>
      )}
      {preview && (
        <Dialog
          title={t("文件预览")}
          subtitle={preview.name}
          wide
          onClose={() => setPreview(null)}
        >
          <Hint>
            {t(
              "预览只读取候选内容，不执行文件。显示正常也不能保证整个文件完整。",
            )}
          </Hint>
          {previewUrl ? (
            <img
              className="recovery-preview-image"
              src={previewUrl}
              alt={preview.name}
            />
          ) : (
            <pre className="recovery-preview-text">
              {/\.(txt|md|csv|json|log|xml|ini|yml|yaml)$/i.test(preview.name)
                ? new TextDecoder().decode(new Uint8Array(preview.bytes))
                : Array.from(new Uint8Array(preview.bytes).slice(0, 1024))
                    .map(
                      (b, i) =>
                        `${i % 16 === 0 ? `${i ? "\n" : ""}${i.toString(16).padStart(8, "0")}  ` : ""}${b.toString(16).padStart(2, "0")} `,
                    )
                    .join("")}
            </pre>
          )}
          <p className="data-footnote">
            {preview.truncated
              ? t("仅显示文件开头，内容已截断。")
              : t(
                  "非图片和文本文件显示十六进制摘要；请导出后使用对应软件核对。",
                )}
          </p>
        </Dialog>
      )}
    </div>
  );
}

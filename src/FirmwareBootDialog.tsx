import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError, Hint } from "./components";
import { FirmwareLoaderSecurity } from "./FirmwareLoaderSecurity";
import type { LoaderSecurityReport } from "./FirmwareLoaderSecurity";
import { FirmwareSecureBootPanel } from "./FirmwareSecureBootPanel";
import type { SecureBootReport } from "./FirmwareSecureBootPanel";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle } from "./LocalDiskView";
import {
  MigrationTargets, ReplacedMigrationPartitions, canReplaceMigrationTarget, invalidMigrationSizes, migrationOptions,
  type MigrationSelection, type MigrationTarget, type PartitionSize,
} from "./MigrationSizes";
import { formatSize } from "./types";
import type { Disk, MigrationReplacedPartition } from "./types";

type Entry = {
  id: number;
  status: "ready" | "missing" | "invalid";
  active: boolean | null;
  bootCategory: boolean;
  description: string | null;
  filePaths: string[];
  gptPartitions: string[];
  parametersBytes: number | null;
};

type Report = {
  revision: string;
  order: number[];
  current: number | null;
  next: number | null;
  setupNext: boolean | null;
  allEntries: boolean;
  entries: Entry[];
};

type Change =
  | { kind: "order"; entries: number[] }
  | { kind: "once"; entry: number }
  | { kind: "clearOnce" }
  | { kind: "active"; entry: number; active: boolean }
  | { kind: "rename"; entry: number; description: string }
  | { kind: "create"; candidate: string; description: string }
  | { kind: "remove"; entry: number }
  | { kind: "attach"; entry: number }
  | {
      kind: "retarget";
      entry: number;
      candidate: string;
      preserveParameters: boolean;
    }
  | { kind: "restore" }
  | { kind: "setup"; next: boolean };

type Preview = { token: string; before: Report; after: Report; change: Change };
type WinPePreview = {
  token: string;
  entry: number;
  createsEntry: boolean;
  media: {
    partition: string;
    wimBytes: number;
    wimImage: number;
    wimSha256: string;
    appSha256: string;
  };
  task?: {
    source: string;
    destination: string;
    bytes: number;
    destructive: boolean;
    migrationDestinations?: {
      destination: string;
      bytes: number;
      partitionSizes: PartitionSize[];
      partitionSelection: MigrationSelection;
      replaced?: MigrationReplacedPartition[];
    }[];
  };
};
export type WinPeTaskForm =
  | { kind: "backup" | "restore"; selection: string }
  | { kind: "clone"; selection: string; destination?: string; mode: "allSectors" | "usedSpace" }
  | { kind: "migrate"; selection: string; destinations: MigrationTarget[] };
type Loader = { id: string; path: string };
type LoaderVerification = { sha256: string; partition: string };
type Source = {
  label: string;
  target: string;
  layoutKey: string;
  partition: number;
};

function bootId(id: number) {
  return `Boot${id.toString(16).toUpperCase().padStart(4, "0")}`;
}

function entryLabel(entry: Entry) {
  return entry.description
    ? `${bootId(entry.id)} · ${entry.description}`
    : bootId(entry.id);
}

function entryById(report: Report, id: number) {
  return report.entries.find((entry) => entry.id === id);
}

function dispose(token: string | null) {
  if (token)
    void invoke("firmware_boot_discard", { token }).catch(() => undefined);
}

function ChangeDetails({ preview }: { preview: Preview }) {
  const { t } = useLocale();
  const { change, before, after } = preview;
  const entry =
    change.kind === "create" || change.kind === "restore"
      ? after.entries.find(
          (item) =>
            !before.order.includes(item.id) && after.order.includes(item.id),
        )
      : change.kind === "remove"
        ? entryById(before, change.entry)
        : undefined;
  function describe(report: Report) {
    switch (change.kind) {
      case "order":
      case "create":
      case "attach":
      case "remove":
      case "restore":
        return report.order
          .map((id) => {
            const entry = entryById(report, id);
            return entry ? entryLabel(entry) : bootId(id);
          })
          .join(" › ");
      case "once":
      case "clearOnce": {
        if (report.next === null) return t("无");
        const next = entryById(report, report.next);
        return next ? entryLabel(next) : bootId(report.next);
      }
      case "rename": {
        const entry = entryById(report, change.entry);
        return entry ? entryLabel(entry) : bootId(change.entry);
      }
      case "retarget": {
        const entry = entryById(report, change.entry);
        if (!entry) return bootId(change.entry);
        const locations = [...entry.filePaths, ...entry.gptPartitions];
        return [
          entryLabel(entry),
          ...(locations.length ? locations : [t("未知")]),
        ].join(" · ");
      }
      case "active":
        return `${bootId(change.entry)} · ${t(entryById(report, change.entry)?.active ? "已启用" : "启动已停用")}`;
      case "setup":
        return t(report.setupNext ? "下次启动进入固件设置" : "无");
    }
  }
  const parameters =
    change.kind === "retarget"
      ? {
          before: entryById(before, change.entry)?.parametersBytes ?? 0,
          after: entryById(after, change.entry)?.parametersBytes ?? 0,
        }
      : null;
  return (
    <div className="partition-plan-details">
      {entry && (
        <div>
          <strong>{entryLabel(entry)}</strong>
          {entry.filePaths.map((path, index) => (
            <div key={index}>
              <code>{path}</code>
            </div>
          ))}
          {entry.gptPartitions.map((id, index) => (
            <div key={index}>
              GPT · <code>{id}</code>
            </div>
          ))}
        </div>
      )}
      <p>
        {t("更改前")} · <bdi>{describe(before)}</bdi>
      </p>
      <p>
        {t("更改后")} · <bdi>{describe(after)}</bdi>
      </p>
      {parameters && (
        <p>{t("启动参数字节数 {before} → {after}", parameters)}</p>
      )}
    </div>
  );
}

export function FirmwareBootDialog({
  disks,
  onClose,
  onBusy,
  initialTask,
}: {
  disks: Disk[];
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  initialTask?: WinPeTaskForm;
}) {
  const { t, catalogText } = useLocale();
  const [report, setReport] = useState<Report | null>(null);
  const [secureBootOpen, setSecureBootOpen] = useState(false);
  const [secureBoot, setSecureBoot] = useState<SecureBootReport | null>(null);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [winPePreview, setWinPePreview] = useState<WinPePreview | null>(null);
  const [winPeOpen, setWinPeOpen] = useState(initialTask !== undefined);
  const [winPeSource, setWinPeSource] = useState("");
  const [taskKind, setTaskKind] = useState<"none" | WinPeTaskForm["kind"]>(initialTask?.kind ?? "none");
  const [taskSelection, setTaskSelection] = useState(initialTask?.selection ?? "");
  const [taskDestination, setTaskDestination] = useState(initialTask?.kind === "clone" ? initialTask.destination ?? "" : "");
  const [cloneMode, setCloneMode] = useState(initialTask?.kind === "clone" ? initialTask.mode : "allSectors");
  const [migrationTargets, setMigrationTargets] = useState(initialTask?.kind === "migrate" ? initialTask.destinations : []);
  const [overwriteConfirmed, setOverwriteConfirmed] = useState(false);
  const [selected, setSelected] = useState<number | null>(null);
  const [order, setOrder] = useState<number[]>([]);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [sourceKey, setSourceKey] = useState("");
  const [loaders, setLoaders] = useState<Loader[]>([]);
  const [loader, setLoader] = useState("");
  const [verifiedLoader, setVerifiedLoader] =
    useState<LoaderVerification | null>(null);
  const [loaderSecurity, setLoaderSecurity] =
    useState<LoaderSecurityReport | null>(null);
  const [description, setDescription] = useState("");
  const [retargetId, setRetargetId] = useState<number | null>(null);
  const [preserveParameters, setPreserveParameters] = useState(false);
  const loaderEditor = useRef<HTMLDetailsElement>(null);
  const token = useRef<string | null>(null);
  const inFlight = useRef(false);
  useEffect(() => () => dispose(token.current), []);

  function discard() {
    dispose(token.current);
    token.current = null;
    setPreview(null);
    setWinPePreview(null);
    setOverwriteConfirmed(false);
  }

  function load(next: Report) {
    const entry =
      next.entries.find((item) => item.id === selected) ?? next.entries[0];
    setReport(next);
    setOrder(next.order);
    setSelected(entry?.id ?? null);
    setName(entry?.description ?? "");
    setRetargetId(null);
    setPreserveParameters(false);
  }

  const sources = new Map<string, Source>();
  const winPeSources = new Map<string, Source>();
  const taskSources = new Map<string, Omit<Source, "partition"> & {
    partition: number | null;
    disk: Disk;
    size: number;
    targetWritable: boolean;
  }>();
  for (const disk of disks) {
    const { identity, layoutKey, offline } = disk.hardware ?? {};
    if (
      disk.kind !== "physical" ||
      typeof offline !== "boolean" ||
      !identity ||
      !layoutKey
    )
      continue;
    taskSources.set(`${disk.id}/disk`, {
      label: `${diskTitle(disk, t)} · ${t("整盘")} · ${t(offline ? "脱机" : "联机")}`,
      target: identity,
      layoutKey,
      partition: null,
      disk,
      size: disk.size,
      targetWritable: !offline && disk.hardware?.deviceReadOnly === false,
    });
    for (const partition of disk.partitions) {
      if (!partition.number) continue;
      const key = `${disk.id}/${partition.number}`;
      const source = {
        label: `${diskTitle(disk, t)} · ${partition.name} · ${t(offline ? "脱机" : "联机")}`,
        target: identity,
        layoutKey,
        partition: partition.number,
      };
      const efi = partition.role === "EFI 系统";
      if (!partition.hidden && !efi) taskSources.set(key, {
        ...source,
        disk,
        size: partition.size,
        targetWritable: !offline && disk.scheme === "GPT" && disk.hardware?.deviceReadOnly === false &&
          (partition.role === "数据分区" || partition.role === "Windows 系统"),
      });
      if (disk.scheme !== "GPT") continue;
      if (efi) sources.set(key, source);
      if (!offline && (efi || (!partition.system && partition.fs.toUpperCase() === "FAT32"))) {
        winPeSources.set(key, source);
      }
    }
  }
  const winPeDisk = winPeSources.get(winPeSource)?.target;
  const hasDestination = taskKind === "clone" || taskKind === "migrate";
  const eligibleTasks = [...taskSources].filter(([, source]) => {
    if (taskKind === "migrate") {
      return source.partition === null && (source.disk.scheme === "GPT" ||
        (source.disk.scheme === "MBR" && source.disk.sectorSize === 512));
    }
    if (taskKind === "clone") return true;
    return source.target !== winPeDisk && source.disk.hardware?.offline === false &&
      (taskKind !== "restore" || source.targetWritable);
  });
  const selectedTask = eligibleTasks.find(([key]) => key === taskSelection)?.[1];
  const eligibleDestinations = hasDestination && selectedTask ? [...taskSources].filter(([, candidate]) =>
    candidate.target !== winPeDisk && candidate.target !== selectedTask.target &&
    candidate.disk.sectorSize === selectedTask.disk.sectorSize &&
    (taskKind === "clone"
      ? candidate.targetWritable && (candidate.partition === null) === (selectedTask.partition === null) && candidate.size === selectedTask.size
      : candidate.partition === null && canReplaceMigrationTarget(selectedTask.disk, candidate.disk))) : [];
  const selectedDestination = eligibleDestinations.find(([key]) => key === taskDestination)?.[1];
  const migrationDestinations = migrationTargets.map((target) => ({
    choice: eligibleDestinations.find(([key]) => key === `${target.id}/disk`)?.[1],
    options: migrationOptions(selectedTask?.disk, target),
  }));
  const invalidMigration = !migrationDestinations.length || migrationDestinations.some(({ choice, options }) =>
    !choice || invalidMigrationSizes(options.partitionSizes));
  const targetRequired = (taskKind === "clone" && !selectedDestination) || (taskKind === "migrate" && invalidMigration);

  async function run(
    action:
      | "read"
      | "secureBoot"
      | "apply"
      | "previewWinPe"
      | "restartWinPe"
      | "loaders"
      | "verifyLoader"
      | "inspectSecurity"
      | "backup"
      | Change,
  ) {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    setMessage("");
    try {
      if (action === "read") {
        discard();
        setReport(null);
        load(await invoke<Report>("firmware_boot_inspect"));
      } else if (action === "secureBoot") {
        setSecureBootOpen(true);
        setSecureBoot(null);
        setSecureBoot(
          await invoke<SecureBootReport>("firmware_boot_inspect_secure_boot"),
        );
      } else if (action === "loaders") {
        const source = sources.get(sourceKey);
        setLoaders([]);
        setLoader("");
        setVerifiedLoader(null);
        setLoaderSecurity(null);
        if (!source) return;
        const result = await invoke<Loader[]>("firmware_boot_list_loaders", {
          source: {
            target: source.target,
            layoutKey: source.layoutKey,
            partition: source.partition,
          },
        });
        setLoaders(result);
        setLoader(result[0]?.id ?? "");
        if (!result.length) setMessage(t("未找到可选的 EFI 文件。"));
      } else if (action === "verifyLoader") {
        setVerifiedLoader(null);
        setLoaderSecurity(null);
        setVerifiedLoader(
          await invoke<LoaderVerification>("firmware_boot_verify_loader", {
            id: loader,
          }),
        );
      } else if (action === "inspectSecurity") {
        setVerifiedLoader(null);
        setLoaderSecurity(null);
        setLoaderSecurity(
          await invoke<LoaderSecurityReport>(
            "firmware_boot_inspect_loader_security",
            {
              id: loader,
            },
          ),
        );
      } else if (action === "backup") {
        if (!report || selected === null) return;
        const saved = await invoke<{ name: string; sha256: string } | null>(
          "firmware_boot_backup",
          { request: { revision: report.revision, entry: selected } },
        );
        if (saved) setMessage(`${t("启动项备份已保存并核对。")} ${saved.name}`);
      } else if (action === "previewWinPe") {
        const selected = winPeSources.get(winPeSource);
        if (!report || !selected || (taskKind !== "none" && !selectedTask) ||
          targetRequired) return;
        discard();
        const { target, layoutKey, partition } = selected;
        const selection = selectedTask && { target: selectedTask.target, layoutKey: selectedTask.layoutKey, partition: selectedTask.partition };
        const destination = selectedDestination && { target: selectedDestination.target, layoutKey: selectedDestination.layoutKey, partition: selectedDestination.partition };
        const task = taskKind === "backup" ? { kind: taskKind, source: selection } :
          taskKind === "restore" ? { kind: taskKind, destination: selection } :
          taskKind === "clone" ? { kind: taskKind, source: selection, destination, mode: cloneMode } :
          taskKind === "migrate" ? {
            kind: "migrateBatch",
            source: selection,
            destinations: migrationDestinations.flatMap(({ choice, options }) => choice ? [{
              selection: { target: choice.target, layoutKey: choice.layoutKey, partition: null },
              ...options,
            }] : []),
          } : undefined;
        const result = await invoke<WinPePreview | null>("firmware_winpe_preview", {
          request: { revision: report.revision, source: { target, layoutKey, partition }, task },
        });
        if (!result) return;
        token.current = result.token;
        setWinPePreview(result);
      } else if (action === "restartWinPe") {
        if (winPePreview?.task?.destructive && !overwriteConfirmed) return;
        const applyToken = token.current;
        token.current = null;
        setWinPePreview(null);
        setReport(null);
        if (!applyToken) return;
        const result = await invoke<{ report: Report; error: string | null }>(
          "firmware_winpe_restart",
          { token: applyToken },
        );
        load(result.report);
        if (result.error) {
          setError(`${t("一次性 WinPE 启动已设置。Windows 未接受重启请求。")} ${result.error}`);
        } else {
          setMessage(t("一次性 WinPE 启动已设置，Windows 已接受重启请求。"));
        }
      } else if (action === "apply") {
        const applyToken = token.current;
        token.current = null;
        setPreview(null);
        setReport(null);
        if (applyToken) {
          load(
            await invoke<Report>("firmware_boot_apply", { token: applyToken }),
          );
          setMessage(t("固件启动设置已写入并回读核对。"));
        }
      } else if (report) {
        const result = await invoke<Preview | null>("firmware_boot_preview", {
          request: { revision: report.revision, change: action },
        });
        if (result) {
          token.current = result.token;
          setPreview(result);
        }
      }
    } catch (cause) {
      setError(String(cause));
    } finally {
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }

  const entry = report?.entries.find((item) => item.id === selected);
  const orderChanged =
    report !== null && order.some((id, index) => id !== report.order[index]);
  const orderedEntries = report
    ? [
        ...order,
        ...report.entries
          .filter((item) => !order.includes(item.id))
          .map((item) => item.id),
      ]
    : [];
  const position = selected === null ? -1 : order.indexOf(selected);
  const writable = !busy && !orderChanged && entry?.status === "ready";
  const retargetEntry = report?.entries.find((item) => item.id === retargetId);
  const canToggle =
    writable &&
    entry?.bootCategory &&
    (!entry.active ||
      (report?.current !== null &&
        selected !== report?.current &&
        selected !== report?.next));
  function move(delta: number) {
    const other = position + delta;
    if (position < 0 || other < 0 || other >= order.length) return;
    const updated = [...order];
    [updated[position], updated[other]] = [updated[other], updated[position]];
    setOrder(updated);
  }

  if (secureBootOpen) {
    return (
      <Dialog
        title={t("安全启动证书")}
        wide
        onClose={() => {
          if (!inFlight.current) onClose();
        }}
      >
        <FirmwareSecureBootPanel
          report={secureBoot}
          busy={busy}
          error={error}
          onRead={() => void run("secureBoot")}
          onBack={() => {
            setError("");
            setSecureBoot(null);
            setSecureBootOpen(false);
          }}
        />
      </Dialog>
    );
  }

  return (
    <Dialog
      title={t("UEFI 固件启动项")}
      wide
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <Hint>{t("更改会影响本机固件的后续启动。")}</Hint>
      <FormError message={catalogText(error)} />
      {message && <p role="status">{message}</p>}
      {winPePreview ? (
        <>
          <div className="partition-plan-details">
            <h3>{t("重启到 WinPE")}</h3>
            <p>{winPeSources.get(winPeSource)?.label}</p>
            <p>
              <bdi>{bootId(winPePreview.entry)}</bdi> · {t(winPePreview.createsEntry ? "新增独立 WinPE 启动项" : "复用已核对的 WinPE 启动项")}
            </p>
            <p>{t("本次重启使用一次性 UEFI 启动项，常规启动顺序保持不变。请先保存其他应用中的工作。")}</p>
            <dl>
              <dt>GPT</dt>
              <dd><code>{winPePreview.media.partition}</code></dd>
              <dt>{t("WinPE 启动索引")}</dt>
              <dd>{winPePreview.media.wimImage} · {winPePreview.media.wimBytes} B</dd>
              <dt>boot.wim · SHA-256</dt>
              <dd><code>{winPePreview.media.wimSha256}</code></dd>
              <dt>DiskStudio.exe · SHA-256</dt>
              <dd><code>{winPePreview.media.appSha256}</code></dd>
              {winPePreview.task && <>
                <dt>{t(hasDestination ? "来源" : winPePreview.task.destructive ? "还原备份" : "离线备份来源")}</dt>
                <dd>{winPePreview.task.source}{!winPePreview.task.migrationDestinations &&
                  ` · ${winPePreview.task.bytes.toLocaleString()} B`}</dd>
                {!winPePreview.task.migrationDestinations && <>
                  <dt>{t(hasDestination ? "目标磁盘" : winPePreview.task.destructive ? "离线还原目标" : "离线备份输出")}</dt>
                  <dd>{winPePreview.task.destination}</dd>
                </>}
                {winPePreview.task.migrationDestinations?.map((target) => (
                  <div key={target.destination} className="migration-target-preview">
                    <dt>{t("目标磁盘")}</dt>
                    <dd>{target.destination} · {formatSize(target.bytes)}</dd>
                    <ReplacedMigrationPartitions partitions={target.replaced} />
                    <dt>{t("迁移范围")}</dt>
                    {target.partitionSelection.mode === "selected" ? <>
                      <dd>{t("Windows 必需分区和所选数据分区")}</dd>
                      {target.partitionSelection.partitions.map((number) =>
                        <dd key={number}>{t("分区 {number}", { number })}</dd>)}
                    </> : <dd>{t("复制全部分区")}</dd>}
                    {target.partitionSizes.map((part) => (
                      <div key={part.partition}>
                        <dt>{t("分区 {number}", { number: part.partition })}</dt>
                        {part.startBytes !== undefined && <dd>{t("起点（MiB）")} · {(part.startBytes / 1024 ** 2).toLocaleString()}</dd>}
                        {part.bytes !== undefined && <dd>{formatSize(part.bytes)}</dd>}
                      </div>
                    ))}
                  </div>
                ))}
              </>}
            </dl>
            {winPePreview.task && <Hint>{t(
              taskKind === "migrate" ? "WinPE 将核对 Windows 启动链并锁定源卷。各目标盘保持脱机。启动任一副本前，请断开源盘和其余副本。"
                : taskKind === "clone" ? "WinPE 将锁定卷、克隆确认的范围并执行 SHA-256 回读。目标磁盘保持脱机。"
                : winPePreview.task.destructive ? "WinPE 将先验证完整备份链，然后覆盖确认的目标范围。还原完成后目标磁盘保持脱机。"
                : "WinPE 启动后将自动锁定源卷、运行完整扇区备份并回读校验。")}</Hint>}
            {winPePreview.task?.destructive && <label className="data-confirm">
              <input type="checkbox" checked={overwriteConfirmed} disabled={busy} onChange={(event) => setOverwriteConfirmed(event.currentTarget.checked)} />
              {t("我已核对目标，理解数据将被覆盖且无法撤销。")}
            </label>}
          </div>
          <div className="dialog-actions">
            <button className="button" disabled={busy} onClick={discard}>{t("取消预览")}</button>
            <button className={`button ${winPePreview.task?.destructive ? "danger" : "primary"}`}
              disabled={busy || (winPePreview.task?.destructive && !overwriteConfirmed)} onClick={() => void run("restartWinPe")}>
              {t("确认并重启到 WinPE")}
            </button>
          </div>
        </>
      ) : preview ? (
        <>
          <ChangeDetails preview={preview} />
          <div className="dialog-actions">
            <button className="button" disabled={busy} onClick={discard}>
              {t("取消预览")}
            </button>
            <button
              className="button primary"
              disabled={busy}
              onClick={() => void run("apply")}
            >
              {t("确认写入")}
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="partition-action-buttons">
            <button
              className="button"
              disabled={busy}
              onClick={() => void run("read")}
            >
              {t(report ? "重新读取" : "读取固件启动项")}
            </button>
            <button
              className="button"
              disabled={busy}
              onClick={() => void run("secureBoot")}
            >
              {t("查看安全启动证书")}
            </button>
            {report?.next != null && (
              <button
                className="button"
                disabled={busy || orderChanged}
                onClick={() => void run({ kind: "clearOnce" })}
              >
                {t("预览清除一次性启动")}
              </button>
            )}
            {report && (
              <button
                className="button"
                disabled={busy || orderChanged}
                onClick={() => void run({ kind: "restore" })}
              >
                {t("还原 UEFI 启动项")}
              </button>
            )}
            {report && (
              <button
                className="button"
                disabled={busy || orderChanged}
                aria-expanded={winPeOpen}
                onClick={() => setWinPeOpen(!winPeOpen)}
              >
                {t("重启到 WinPE…")}
              </button>
            )}
            {report?.setupNext != null && (
              <button
                className="button"
                disabled={busy || orderChanged}
                onClick={() =>
                  void run({ kind: "setup", next: !report.setupNext })
                }
              >
                {t(
                  report.setupNext
                    ? "预览取消进入固件设置"
                    : "预览进入固件设置",
                )}
              </button>
            )}
          </div>
          {winPeOpen && report && (
            <div className="boot-repair-fields">
              <label>
                {t("WinPE 启动分区")}
                <select disabled={busy || orderChanged} value={winPeSource} onChange={(event) => setWinPeSource(event.currentTarget.value)}>
                  <option value="">{t("选择分区")}</option>
                  {[...winPeSources].map(([key, item]) => (
                    <option value={key} key={key}>{item.label}</option>
                  ))}
                </select>
              </label>
              <Hint>{t("选择已准备好 WinPE 和当前版本 DiskStudio.exe 的 GPT FAT32 分区。")}</Hint>
              <label>
                {t("重启后的任务")}
                <select value={taskKind} disabled={busy || orderChanged}
                  onChange={(event) => setTaskKind(event.currentTarget.value as typeof taskKind)}>
                  <option value="none">{t("打开 DiskStudio")}</option>
                  <option value="backup">{t("在 WinPE 执行完整扇区备份")}</option>
                  <option value="restore">{t("在 WinPE 从备份还原")}</option>
                  <option value="clone">{t("在 WinPE 克隆")}</option>
                  <option value="migrate">{t("在 WinPE 迁移 Windows")}</option>
                </select>
              </label>
              {taskKind !== "none" && <label>
                {t(hasDestination ? "来源" : taskKind === "restore" ? "离线还原目标" : "离线备份来源")}
                <select disabled={busy || orderChanged} value={selectedTask ? taskSelection : ""}
                  onChange={(event) => {
                    setTaskSelection(event.currentTarget.value);
                    setMigrationTargets([]);
                  }}>
                  <option value="">{t("选择磁盘或分区")}</option>
                  {eligibleTasks.map(([key, item]) => <option value={key} key={key}>{item.label}</option>)}
                </select>
              </label>}
              {taskKind === "clone" && <label>
                {t("目标磁盘")}
                <select disabled={busy || orderChanged || !selectedTask} value={selectedDestination ? taskDestination : ""}
                  onChange={(event) => setTaskDestination(event.currentTarget.value)}>
                  <option value="">{t("选择磁盘或分区")}</option>
                  {eligibleDestinations.map(([key, item]) => <option value={key} key={key}>{item.label}</option>)}
                </select>
              </label>}
              {taskKind === "clone" && <label>
                {t("复制方式")}
                <select disabled={busy || orderChanged} value={cloneMode}
                  onChange={(event) => setCloneMode(event.currentTarget.value === "usedSpace" ? "usedSpace" : "allSectors")}>
                  <option value="allSectors">{t("包含空闲空间，保留完整扇区内容")}</option>
                  <option value="usedSpace">{t("已用空间")}</option>
                </select>
              </label>}
              {taskKind === "migrate" && selectedTask && <MigrationTargets
                source={selectedTask.disk} choices={eligibleDestinations.map(([, choice]) => ({
                  id: choice.disk.id, label: `${diskTitle(choice.disk, t)} · ${formatSize(choice.size)}`,
                }))}
                targets={migrationTargets} onChange={setMigrationTargets} disabled={busy || orderChanged} />}
              <button
                className="button"
                disabled={busy || orderChanged || !winPeSources.has(winPeSource) || (taskKind !== "none" && !selectedTask) ||
                  targetRequired}
                onClick={() => void run("previewWinPe")}
              >
                {taskKind === "backup" || taskKind === "restore" ? t("选择备份文件并预览重启") : t("核对 WinPE 并预览重启")}
              </button>
            </div>
          )}
          {report && (
            <div className="editor-form boot-report">
              <div>
                <div>
                  {t("当前启动项")} ·{" "}
                  {report.current === null ? t("无") : bootId(report.current)}
                </div>
                <div>
                  {t("一次性启动项")} ·{" "}
                  {report.next === null ? t("无") : bootId(report.next)}
                </div>
                {report.setupNext && <div>{t("下次启动进入固件设置")}</div>}
                {!report.allEntries && (
                  <p className="muted">{t("此环境仅列出固件引用的启动项。")}</p>
                )}
              </div>
              {orderedEntries.length > 0 ? (
                <>
                  <label>
                    {t("固件启动顺序")}
                    <select
                      size={Math.min(8, Math.max(3, orderedEntries.length))}
                      disabled={busy}
                      value={selected ?? ""}
                      onChange={(event) => {
                        const id = Number(event.currentTarget.value);
                        setSelected(id);
                        setName(entryById(report, id)?.description ?? "");
                        setRetargetId(null);
                        setPreserveParameters(false);
                      }}
                    >
                      {orderedEntries.map((id, index) => {
                        const item = entryById(report, id)!;
                        return (
                          <option value={id} key={id}>
                            {index < order.length
                              ? `${index + 1}. ${entryLabel(item)}`
                              : t("{entry}（未列入启动顺序）", {
                                  entry: entryLabel(item),
                                })}
                          </option>
                        );
                      })}
                    </select>
                  </label>
                  <div className="partition-action-buttons">
                    <button
                      className="button"
                      disabled={busy || position <= 0}
                      onClick={() => move(-1)}
                    >
                      {t("上移")}
                    </button>
                    <button
                      className="button"
                      disabled={
                        busy || position < 0 || position >= order.length - 1
                      }
                      onClick={() => move(1)}
                    >
                      {t("下移")}
                    </button>
                    {orderChanged && (
                      <>
                        <button
                          className="button"
                          disabled={busy}
                          onClick={() => setOrder(report.order)}
                        >
                          {t("取消")}
                        </button>
                        <button
                          className="button primary"
                          disabled={busy}
                          onClick={() =>
                            void run({ kind: "order", entries: order })
                          }
                        >
                          {t("预览启动顺序")}
                        </button>
                      </>
                    )}
                  </div>
                </>
              ) : (
                <p>{t("没有可显示的固件启动项。")}</p>
              )}
              {entry && (
                <>
                  <div>
                    <div>
                      {bootId(entry.id)} ·{" "}
                      {t(
                        entry.status === "missing"
                          ? "缺失"
                          : entry.status === "invalid"
                            ? "无效"
                            : entry.active
                              ? "已启用"
                              : "启动已停用",
                      )}
                    </div>
                    {entry.filePaths.map((path, index) => (
                      <div key={index}>
                        <code>{path}</code>
                      </div>
                    ))}
                    {entry.gptPartitions.map((id, index) => (
                      <div key={index}>
                        GPT · <code>{id}</code>
                      </div>
                    ))}
                  </div>
                  {entry.status === "ready" && (
                    <>
                      <label>
                        {t("启动项名称")}
                        <input
                          disabled={!writable}
                          value={name}
                          maxLength={255}
                          onChange={(event) =>
                            setName(event.currentTarget.value)
                          }
                        />
                      </label>
                      <div className="partition-action-buttons">
                        <button
                          className="button"
                          disabled={
                            !writable ||
                            !name.trim() ||
                            name === entry.description
                          }
                          onClick={() =>
                            void run({
                              kind: "rename",
                              entry: entry.id,
                              description: name,
                            })
                          }
                        >
                          {t("预览重命名")}
                        </button>
                        <button
                          className="button"
                          disabled={!canToggle}
                          onClick={() =>
                            void run({
                              kind: "active",
                              entry: entry.id,
                              active: !entry.active,
                            })
                          }
                        >
                          {t(
                            entry.active ? "预览停用启动项" : "预览启用启动项",
                          )}
                        </button>
                        <button
                          className="button"
                          disabled={
                            !writable ||
                            !entry.active ||
                            !entry.bootCategory ||
                            report.next === entry.id
                          }
                          onClick={() =>
                            void run({ kind: "once", entry: entry.id })
                          }
                        >
                          {t("预览一次性启动")}
                        </button>
                        <button
                          className="button"
                          disabled={!writable}
                          onClick={() => void run("backup")}
                        >
                          {t("备份 UEFI 启动项")}
                        </button>
                        <button
                          className="button"
                          disabled={
                            !writable ||
                            !entry.bootCategory ||
                            entry.id === report.next
                          }
                          onClick={() => {
                            setRetargetId(entry.id);
                            setPreserveParameters(false);
                            if (loaderEditor.current) {
                              loaderEditor.current.open = true;
                              loaderEditor.current.scrollIntoView({
                                block: "nearest",
                              });
                            }
                          }}
                        >
                          {t("更改启动目标")}
                        </button>
                        {position < 0 && entry.bootCategory && (
                          <button
                            className="button"
                            disabled={!writable}
                            onClick={() =>
                              void run({ kind: "attach", entry: entry.id })
                            }
                          >
                            {t("预览加入启动顺序")}
                          </button>
                        )}
                      </div>
                    </>
                  )}
                  {entry.status !== "missing" && (
                    <div className="partition-action-buttons">
                      <button
                        className="button"
                        disabled={
                          busy ||
                          orderChanged ||
                          report.current === null ||
                          entry.id === report.current ||
                          entry.id === report.next
                        }
                        onClick={() =>
                          void run({ kind: "remove", entry: entry.id })
                        }
                      >
                        {t("预览删除启动项")}
                      </button>
                    </div>
                  )}
                </>
              )}
            </div>
          )}
          {sources.size > 0 && (
            <details className="boot-repair" ref={loaderEditor}>
              <summary>
                {retargetEntry
                  ? t("更改 {entry} 的启动目标", {
                      entry: entryLabel(retargetEntry),
                    })
                  : t(report ? "新建 UEFI 启动项" : "EFI 加载器")}
              </summary>
              {retargetEntry && (
                <div className="partition-action-buttons">
                  <button
                    className="button"
                    disabled={busy}
                    onClick={() => {
                      setRetargetId(null);
                      setPreserveParameters(false);
                    }}
                  >
                    {t("新建 UEFI 启动项")}
                  </button>
                </div>
              )}
              <div className="boot-repair-fields">
                <label>
                  {t("EFI 系统分区")}
                  <select
                    disabled={busy || orderChanged}
                    value={sourceKey}
                    onChange={(event) => {
                      setSourceKey(event.currentTarget.value);
                      setLoaders([]);
                      setLoader("");
                      setVerifiedLoader(null);
                      setLoaderSecurity(null);
                    }}
                  >
                    <option value="">{t("选择分区")}</option>
                    {[...sources].map(([key, value]) => (
                      <option key={key} value={key}>
                        {value.label}
                      </option>
                    ))}
                  </select>
                </label>
                <button
                  className="button"
                  disabled={busy || orderChanged || !sources.has(sourceKey)}
                  onClick={() => void run("loaders")}
                >
                  {t("读取 EFI 文件")}
                </button>
                {loaders.length > 0 && (
                  <>
                    <label>
                      {t("EFI 加载器")}
                      <select
                        disabled={busy || orderChanged}
                        value={loader}
                        onChange={(event) => {
                          setLoader(event.currentTarget.value);
                          setVerifiedLoader(null);
                          setLoaderSecurity(null);
                        }}
                      >
                        {loaders.map((item) => (
                          <option value={item.id} key={item.id}>
                            {item.path}
                          </option>
                        ))}
                      </select>
                    </label>
                    <button
                      className="button"
                      disabled={busy || !loader || !sources.has(sourceKey)}
                      onClick={() => void run("verifyLoader")}
                    >
                      {t("核对 EFI 加载器")}
                    </button>
                    <button
                      className="button"
                      disabled={busy || !loader || !sources.has(sourceKey)}
                      onClick={() => void run("inspectSecurity")}
                    >
                      {t("核对安全启动摘要与签名")}
                    </button>
                    {verifiedLoader && (
                      <div role="status">
                        <div>
                          GPT · <code>{verifiedLoader.partition}</code>
                        </div>
                        <div>
                          SHA-256 · <code>{verifiedLoader.sha256}</code>
                        </div>
                      </div>
                    )}
                    {loaderSecurity && (
                      <FirmwareLoaderSecurity report={loaderSecurity} />
                    )}
                    {report &&
                      (retargetEntry ? (
                        <label className="checkbox-row">
                          <input
                            type="checkbox"
                            checked={preserveParameters}
                            disabled={busy || orderChanged}
                            onChange={(event) =>
                              setPreserveParameters(event.currentTarget.checked)
                            }
                          />
                          {t("保留原启动参数")}
                        </label>
                      ) : (
                        <label>
                          {t("启动项名称")}
                          <input
                            disabled={busy || orderChanged}
                            value={description}
                            maxLength={255}
                            onChange={(event) =>
                              setDescription(event.currentTarget.value)
                            }
                          />
                        </label>
                      ))}
                    {report && (
                      <button
                        className="button"
                        disabled={
                          busy ||
                          orderChanged ||
                          !sources.has(sourceKey) ||
                          !loader ||
                          (!retargetEntry && !description.trim())
                        }
                        onClick={() =>
                          void run(
                            retargetEntry
                              ? {
                                  kind: "retarget",
                                  entry: retargetEntry.id,
                                  candidate: loader,
                                  preserveParameters,
                                }
                              : {
                                  kind: "create",
                                  candidate: loader,
                                  description,
                                },
                          )
                        }
                      >
                        {t(
                          retargetEntry ? "预览更改启动目标" : "预览新建启动项",
                        )}
                      </button>
                    )}
                  </>
                )}
              </div>
            </details>
          )}
        </>
      )}
    </Dialog>
  );
}

import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Check,
  Activity,
  ArrowLeftRight,
  ChevronDown,
  Copy,
  Database,
  FolderOpen,
  Info,
  LoaderCircle,
  Minus,
  Monitor,
  Moon,
  RefreshCcw,
  Settings2,
  ScanLine,
  ShieldCheck,
  Square,
  Sun,
  X,
} from "lucide-react";
import type { Disk, DiskSnapshot, PartitionStatus, TableReport } from "./types";
import { formatSize } from "./types";
import {
  native,
  openImage,
  readLocalDisks,
  readPartitionStatus,
  ImageInspectionError,
} from "./backend";
import { PartitionActions } from "./PartitionActions";
import { PartitionOutcomeDialog } from "./PartitionOutcomeDialog";
import type { PartitionOutcome } from "./partitionOutcome";
import { readLocalState } from "./localRefresh";
import { matchPartitionTarget } from "./partitionCapabilities";
import {
  diskTitle,
  LocalDiskHeader,
  LocalDiskContent,
  LocalDetails,
} from "./LocalDiskView";
import { ImageDiskContent, ImageDetails } from "./ImageDiskView";
import { DataTools } from "./DataTools";
import { BackupPasswordPrompt } from "./BackupPasswordPrompt";
import { TableCheckReport } from "./TableCheckReport";
import { MissingImageFilesError } from "./imageDependencies";
import { Dialog, DiskIcon, Empty } from "./components";
import { SurfaceScanDialog } from "./SurfaceScanDialog";
import { SectorViewer } from "./SectorViewer";
import { HealthDialog } from "./HealthDialog";
import { TableBackupDialog } from "./TableBackupDialog";
import { TableInspectDialog } from "./TableInspectDialog";
import { LostPartitionDialog } from "./LostPartitionDialog";
import { DeviceStateDialog } from "./DeviceStateDialog";
import { PartitionMoveDialog } from "./PartitionMoveDialog";
import { MaintenanceDialog } from "./MaintenanceDialog";
import { ImageConversionDialog } from "./ImageConversionDialog";
import { ImageCreationDialog } from "./ImageCreationDialog";
import { imageFiles, releaseImageFiles } from "./imageReader";
import { LanguageProvider, useLanguageSettings } from "./i18n/LanguageProvider";
import { languagePreference, languages } from "./i18n/languages";

type Modal =
  | "details"
  | "settings"
  | "surface"
  | "sector"
  | "health"
  | "tableBackup"
  | "tableInspect"
  | "lostPartitions"
  | "deviceState"
  | "resumePartitionMove"
  | "maintenance"
  | "imageConversion"
  | "imageCreation"
  | null;
function preference(key: string, fallback: string) {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}
export default function App() {
  return (
    <LanguageProvider>
      <Workspace />
    </LanguageProvider>
  );
}

function Workspace() {
  const {
    language,
    preference: selectedLanguage,
    setPreference,
    t,
    catalogText,
  } = useLanguageSettings();
  const [page, setPage] = useState<"disks" | "recovery" | "backup">("disks");
  const [images, setImages] = useState<Disk[]>([]);
  const [snapshot, setSnapshot] = useState<DiskSnapshot | null>(null);
  const [access, setAccess] = useState<PartitionStatus | null>(null);
  const [loading, setLoading] = useState(native);
  const [readError, setReadError] = useState("");
  const [diskId, setDiskId] = useState("");
  const [partitionId, setPartitionId] = useState("");
  const [browsingPartition, setBrowsingPartition] = useState("");
  const [modal, setModal] = useState<Modal>(null);
  const [tableReport, setTableReport] = useState<TableReport | null>(null);
  const [partitionOutcome, setPartitionOutcome] =
    useState<PartitionOutcome | null>(null);
  const [imageParts, setImageParts] = useState<{
    primary: File;
    files: File[];
    missing: string[];
    kind: "data" | "parent";
  } | null>(null);
  const [menu, setMenu] = useState(false);
  const [busy, setBusy] = useState(false);
  const [toast, setToast] = useState<{ text: string; error?: boolean } | null>(
    null,
  );
  const [theme, setTheme] = useState(() =>
    preference("diskstudio.theme", "light"),
  );
  const [compact, setCompact] = useState(
    () => preference("diskstudio.compact", "false") === "true",
  );
  const [maximized, setMaximized] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const partsInput = useRef<HTMLInputElement>(null);
  const workspaceScroll = useRef<HTMLDivElement>(null);
  const reading = useRef(false);
  const busyRef = useRef(false);
  useEffect(() => {
    workspaceScroll.current?.scrollTo({ top: 0 });
  }, [page]);
  const disks = [...(snapshot?.disks ?? []), ...images];
  const disk = disks.find((d) => d.id === diskId) ?? disks[0];
  const physical = disk?.kind === "physical";
  const target = disk ? matchPartitionTarget(disk, access) : undefined;
  const partition =
    disk?.partitions.find((p) => p.id === partitionId) ??
    disk?.partitions.find((p) => p.driveLetter === "C") ??
    disk?.partitions.find((p) => p.driveLetter) ??
    disk?.partitions.find((p) => !p.system && p.fs !== "—") ??
    disk?.partitions[0];
  function notify(text: string, error = false) {
    setToast({ text, error });
  }
  function operationBusy(value: boolean) {
    busyRef.current = value;
    setBusy(value);
  }
  function selectDisk(d: Disk) {
    if (busyRef.current) return;
    setDiskId(d.id);
    setPartitionId("");
    setBrowsingPartition("");
    setMenu(false);
  }
  async function refreshLocal(strict = false) {
    if (!native || reading.current) {
      if (strict)
        throw new Error(
          !native
            ? t("当前环境无法读取本机元数据。")
            : t("另一项元数据刷新仍在进行，请稍后手动刷新。"),
        );
      return;
    }
    reading.current = true;
    setLoading(true);
    setReadError("");
    try {
      const result = await readLocalState(readLocalDisks, readPartitionStatus);
      setAccess(result.access);
      const next = result.snapshot;
      if (next) {
        setSnapshot(next);
        setDiskId((current) => {
          if (current && !current.startsWith("local-")) return current;
          if (next.disks.some((d) => d.id === current)) return current;
          return (
            (next.disks.find((d) => d.hardware?.isSystem) ?? next.disks[0])
              ?.id ?? ""
          );
        });
      }
      if (result.error) throw new Error(result.error);
    } catch (e) {
      setReadError(String(e instanceof Error ? e.message : e));
      setAccess(null);
      if (strict) throw e;
    } finally {
      setLoading(false);
      reading.current = false;
    }
  }
  async function importImage(file?: File, companions: File[] = []) {
    if (!file || busyRef.current) return;
    operationBusy(true);
    try {
      if (images.length >= 24) throw new Error(t("最多同时打开 24 个镜像。"));
      const opened = await openImage(file, companions);
      setImageParts(null);
      setImages((all) =>
        all.some((d) => d.id === opened.id)
          ? all.map((d) => (d.id === opened.id ? opened : d))
          : [...all, opened],
      );
      setDiskId(opened.id);
      setPartitionId("");
      if (opened.tableReport && opened.tableReport.state !== "passed")
        setTableReport(opened.tableReport);
      notify(t("已读取镜像分区表，源文件保持只读。"));
    } catch (e) {
      if (e instanceof MissingImageFilesError) {
        setImageParts({
          primary: file,
          files: companions,
          missing: e.missing,
          kind: e.kind,
        });
        setToast(null);
        return;
      }
      setImageParts(null);
      if (e instanceof ImageInspectionError) setTableReport(e.report);
      notify(String(e instanceof Error ? e.message : e), true);
    } finally {
      operationBusy(false);
      if (input.current) input.current.value = "";
      if (partsInput.current) partsInput.current.value = "";
    }
  }
  function closeImage() {
    if (busyRef.current || disk?.kind !== "image") return;
    releaseImageFiles(disk.id);
    const remaining = images.filter((d) => d.id !== disk.id);
    setImages(remaining);
    setDiskId((snapshot?.disks[0] ?? remaining[0])?.id ?? "");
    setPartitionId("");
  }
  function closeModal() {
    if (!busyRef.current) setModal(null);
  }
  function windowAction(action: "minimize" | "toggleMaximize" | "close") {
    if (action === "close" && busyRef.current) {
      notify(t("正在等待操作完成，请保持应用运行。"));
      return;
    }
    if (!native) return;
    getCurrentWindow()
      [action]()
      .catch((e) => notify(String(e), true));
  }
  useEffect(() => {
    // Remove only this application's obsolete demo state; retain preferences.
    try {
      localStorage.removeItem("diskstudio.disks.v1");
      localStorage.removeItem("diskstudio.tasks.v1");
    } catch {
      /* Storage is optional. */
    }
    void refreshLocal();
  }, []);
  useEffect(() => {
    const media = matchMedia("(prefers-color-scheme: dark)");
    const update = () => {
      document.documentElement.dataset.theme =
        theme === "system" ? (media.matches ? "dark" : "light") : theme;
    };
    update();
    media.addEventListener("change", update);
    try {
      localStorage.setItem("diskstudio.theme", theme);
    } catch {
      /* Optional preference. */
    }
    return () => media.removeEventListener("change", update);
  }, [theme]);
  useEffect(() => {
    try {
      localStorage.setItem("diskstudio.compact", String(compact));
    } catch {
      /* Optional preference. */
    }
  }, [compact]);
  useEffect(() => {
    if (!toast) return;
    const timer = setTimeout(() => setToast(null), 5000);
    return () => clearTimeout(timer);
  }, [toast]);
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(false);
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("click", close);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("click", close);
      document.removeEventListener("keydown", key);
    };
  }, [menu]);
  useEffect(() => {
    if (!native) return;
    const win = getCurrentWindow();
    let alive = true;
    const cleanups: (() => void)[] = [];
    win.isMaximized().then((value) => {
      if (alive) setMaximized(value);
    });
    win
      .onResized(async () => {
        const value = await win.isMaximized();
        if (alive) setMaximized(value);
      })
      .then((fn) => (alive ? cleanups.push(fn) : fn()));
    win
      .onCloseRequested((e) => {
        if (busyRef.current) {
          e.preventDefault();
          notify(t("正在等待操作完成，请保持应用运行。"));
        }
      })
      .then((fn) => (alive ? cleanups.push(fn) : fn()));
    return () => {
      alive = false;
      cleanups.forEach((fn) => fn());
    };
  }, [t]);
  return (
    <div
      className={`app ${compact ? "compact" : ""} ${maximized ? "maximized" : ""}`}
    >
      <header
        className="titlebar"
        data-tauri-drag-region
        onDoubleClick={(e) => {
          if ((e.target as HTMLElement).hasAttribute("data-tauri-drag-region"))
            windowAction("toggleMaximize");
        }}
      >
        <div className="brand" data-tauri-drag-region>
          <Database size={29} strokeWidth={2.1} />
          <span data-tauri-drag-region>DiskStudio</span>
        </div>
        <nav className="main-nav" aria-label={t("功能导航")}>
          {(
            [
              { id: "disks", name: t("磁盘管理") },
              { id: "recovery", name: t("文件恢复") },
              { id: "backup", name: t("备份与克隆") },
            ] as const
          ).map((item) => (
            <button
              key={item.id}
              className={page === item.id ? "active" : ""}
              aria-current={page === item.id ? "page" : undefined}
              disabled={busy}
              onClick={() => setPage(item.id)}
            >
              {item.name}
            </button>
          ))}
        </nav>
        <div className="drag-space" data-tauri-drag-region />
        <div className="app-tools">
          <div className="menu-anchor disk-menu-anchor">
            <button
              className="disk-menu-trigger"
              aria-label={t("磁盘菜单")}
              aria-expanded={menu}
              disabled={busy}
              onClick={(e) => {
                e.stopPropagation();
                setMenu(!menu);
              }}
            >
              {t("磁盘")} <ChevronDown size={14} />
            </button>
            {menu && (
              <div className="popover disk-menu" aria-label={t("磁盘菜单")}>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("tableBackup");
                  }}
                >
                  <Copy size={16} />
                  {t("分区表备份与恢复…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("tableInspect");
                  }}
                >
                  <ShieldCheck size={16} />
                  {t("分区表检查与修复…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("lostPartitions");
                  }}
                >
                  <ScanLine size={16} />
                  {t("搜索丢失分区…")}
                </button>
                <div className="menu-divider" />
                <button
                  disabled={
                    !physical ||
                    disk?.hardware?.offline !== true ||
                    !disk.hardware.identity ||
                    !disk.hardware.layoutKey ||
                    loading
                  }
                  onClick={() => {
                    setMenu(false);
                    setModal("resumePartitionMove");
                  }}
                >
                  <ArrowLeftRight size={16} />
                  {t("继续分区移动…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("deviceState");
                  }}
                >
                  <Settings2 size={16} />
                  {t("更改设备状态…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("health");
                  }}
                >
                  <Activity size={16} />
                  {t("健康信息（SMART）…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("sector");
                  }}
                >
                  <Database size={16} />
                  {t("扇区查看…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("surface");
                  }}
                >
                  <ScanLine size={16} />
                  {t("坏道检测…")}
                </button>
                <button
                  disabled={!physical || !disk?.hardware?.identity || loading}
                  onClick={() => {
                    setMenu(false);
                    setModal("maintenance");
                  }}
                >
                  <Settings2 size={16} />
                  {t("测速与磁盘维护…")}
                </button>
                <div className="menu-divider" />
                <button onClick={() => input.current?.click()}>
                  <FolderOpen size={16} />
                  {t("打开镜像…")}
                </button>
                <button disabled={disk?.kind !== "image"} onClick={closeImage}>
                  <X size={16} />
                  {t("关闭当前镜像")}
                </button>
                <button
                  disabled={!native}
                  onClick={() => {
                    setMenu(false);
                    setModal("imageConversion");
                  }}
                >
                  <Copy size={16} />
                  {t("镜像格式转换…")}
                </button>
                <button
                  disabled={!native}
                  onClick={() => {
                    setMenu(false);
                    setModal("imageCreation");
                  }}
                >
                  <Database size={16} />
                  {t("创建虚拟磁盘…")}
                </button>
                <div className="menu-divider" />
                <button
                  disabled={!native || loading}
                  onClick={() => void refreshLocal()}
                >
                  <RefreshCcw size={16} />
                  {t("刷新磁盘列表")}
                </button>
              </div>
            )}
          </div>
          <button
            className="icon-button"
            title={t("设置")}
            aria-label={t("设置")}
            disabled={busy}
            onClick={() => setModal("settings")}
          >
            <Settings2 size={21} />
          </button>
        </div>
        <div className="window-controls">
          <button
            aria-label={t("最小化")}
            onClick={() => windowAction("minimize")}
          >
            <Minus size={17} />
          </button>
          <button
            aria-label={maximized ? t("还原窗口") : t("最大化")}
            onClick={() => windowAction("toggleMaximize")}
          >
            {maximized ? <Copy size={14} /> : <Square size={14} />}
          </button>
          <button
            className="close-window"
            aria-label={t("关闭窗口")}
            onClick={() => windowAction("close")}
          >
            <X size={19} />
          </button>
        </div>
      </header>
      {modal === "health" && disk && (
        <HealthDialog
          disk={disk}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
        />
      )}
      {modal === "imageConversion" && (
        <ImageConversionDialog
          initial={disk?.kind === "image" ? imageFiles(disk.id) : undefined}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
        />
      )}
      {modal === "imageCreation" && (
        <ImageCreationDialog
          onClose={() => setModal(null)}
          onBusy={operationBusy}
        />
      )}
      {modal === "maintenance" && disk && (
        <MaintenanceDialog
          disk={disk}
          initialPartition={partition?.number}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "deviceState" && disk && (
        <DeviceStateDialog
          disk={disk}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "resumePartitionMove" && disk && (
        <PartitionMoveDialog
          disk={disk}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "tableBackup" && disk && (
        <TableBackupDialog
          disk={disk}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "tableInspect" && disk && (
        <TableInspectDialog
          disk={disk}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "surface" && disk && (
        <SurfaceScanDialog
          initialDisk={disk}
          disks={disks}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      {modal === "lostPartitions" && disk && (
        <LostPartitionDialog
          disk={disk}
          disks={disks}
          onRefresh={refreshLocal}
          onBusy={operationBusy}
          onClose={() => setModal(null)}
          onBrowse={() => {
            setModal(null);
            setPage("recovery");
          }}
        />
      )}
      {modal === "sector" && disk && (
        <SectorViewer
          disk={disk}
          disks={disks}
          initialPartition={partition?.number}
          onClose={() => setModal(null)}
          onBusy={operationBusy}
          onRefresh={refreshLocal}
        />
      )}
      <div className="app-body">
        <aside className="sidebar">
          <div className="sidebar-title">
            <span>{t("磁盘")}</span>
            <span className="count">{disks.length}</span>
            <button
              className="icon-button add-disk"
              aria-label={t("刷新本机磁盘信息")}
              title={t("重新读取 Windows 存储信息")}
              disabled={loading || !native || busy}
              onClick={() => void refreshLocal()}
            >
              <RefreshCcw size={16} className={loading ? "spin" : ""} />
            </button>
          </div>
          <div className="sidebar-scroll">
            <div className="disk-list local-disk-list">
              {disks.map((d) => (
                <button
                  key={d.id}
                  className={`disk-item local-disk-item ${disk?.id === d.id ? "selected" : ""}`}
                  disabled={busy}
                  onClick={() => selectDisk(d)}
                  aria-pressed={disk?.id === d.id}
                >
                  <DiskIcon size={27} />
                  <span className="disk-copy">
                    <strong>
                      {diskTitle(d, t)}
                      {d.hardware?.isSystem && (
                        <span className="sidebar-system">{t("系统")}</span>
                      )}
                    </strong>
                    <small title={d.hardware?.model ?? d.filename}>
                      {d.hardware?.model ?? d.filename}
                    </small>
                    <span>
                      {formatSize(d.size)}
                      <i />
                      {d.hardware ? catalogText(d.hardware.bus) : t("镜像文件")}
                    </span>
                  </span>
                </button>
              ))}
              {!disks.length && (
                <div className="sidebar-message" role="status">
                  {loading
                    ? t("正在读取磁盘信息…")
                    : native
                      ? t("未发现可显示的磁盘")
                      : t("本机磁盘在桌面版中显示")}
                </div>
              )}
              {snapshot && readError && (
                <p className="sidebar-message error">
                  {t("刷新失败，显示上次信息")}
                </p>
              )}
            </div>
          </div>
        </aside>
        <main className="workspace">
          <div className="workspace-scroll" ref={workspaceScroll}>
            {page !== "disks" ? (
              <DataTools
                page={page}
                disk={disk}
                disks={disks}
                onBusy={operationBusy}
                onRefresh={refreshLocal}
              />
            ) : !disk ? (
              <>
                <header className="disk-header">
                  <div className="disk-heading">
                    <DiskIcon size={39} />
                    <h1>{t("磁盘管理")}</h1>
                  </div>
                </header>
                <Empty
                  icon={
                    loading ? (
                      <LoaderCircle size={34} className="spin" />
                    ) : (
                      <DiskIcon size={34} />
                    )
                  }
                  title={
                    loading ? t("正在读取本机磁盘") : t("暂无可显示的磁盘")
                  }
                  text={
                    readError ||
                    (native
                      ? t("刷新磁盘列表，或通过磁盘菜单打开镜像。")
                      : t("请在 Windows 桌面应用中查看磁盘。"))
                  }
                >
                  {native && (
                    <button
                      className="button"
                      disabled={loading || busy}
                      onClick={() => void refreshLocal()}
                    >
                      {t("刷新信息")}
                    </button>
                  )}
                </Empty>
              </>
            ) : physical &&
              browsingPartition === `${disk.id}/${partition?.id}` ? (
              <DataTools
                key={`files-${disk.id}-${partition?.id}-${disk.hardware?.layoutKey}`}
                page="files"
                disk={disk}
                disks={disks}
                fixedPartition={partition?.number}
                onBusy={operationBusy}
                onRefresh={refreshLocal}
                onBack={() => {
                  setBrowsingPartition("");
                  workspaceScroll.current?.scrollTo({ top: 0 });
                }}
              />
            ) : physical ? (
              <>
                <LocalDiskHeader
                  disk={disk}
                  loading={loading || busy}
                  managed={!!target?.available}
                  onRefresh={() => void refreshLocal()}
                  onDetails={() => setModal("details")}
                />
                <LocalDiskContent
                  disk={disk}
                  target={target}
                  selectedId={partition?.id}
                  onSelect={(id) => {
                    if (!busyRef.current) setPartitionId(id);
                  }}
                  onDetails={() => setModal("details")}
                  error={readError}
                  busy={busy || loading}
                  onBrowse={() => {
                    if (busyRef.current || partition?.number === undefined)
                      return;
                    setBrowsingPartition(`${disk.id}/${partition.id}`);
                    workspaceScroll.current?.scrollTo({ top: 0 });
                  }}
                  warnings={snapshot?.warnings ?? []}
                  actions={
                    <PartitionActions
                      key={disk.id}
                      disk={disk}
                      partition={partition}
                      target={target}
                      unavailableReason={
                        loading
                          ? t("正在读取磁盘操作能力…")
                          : (access?.reason ??
                            t("无法确认此磁盘的完整信息，请刷新后重试。"))
                      }
                      onBusy={operationBusy}
                      onRefresh={() => refreshLocal(true)}
                      onOutcome={setPartitionOutcome}
                    />
                  }
                />
              </>
            ) : (
              <>
                <header className="disk-header">
                  <div className="disk-heading">
                    <DiskIcon size={39} />
                    <div>
                      <h1>{disk.name}</h1>
                      <p>
                        {t("镜像文件")}
                        <i />
                        {formatSize(disk.size)}
                        <i />
                        {disk.scheme}
                        <i />
                        <span>{disk.filename}</span>
                        <span className="readonly-badge">{t("只读")}</span>
                      </p>
                    </div>
                  </div>
                  <button
                    className="button text-button"
                    onClick={() => setModal("details")}
                  >
                    <Info size={18} />
                    {t("详细信息")}
                  </button>
                </header>
                <ImageDiskContent
                  disk={disk}
                  selectedId={partition?.id}
                  onSelect={setPartitionId}
                  onDetails={() => setModal("details")}
                  onCheck={() => setTableReport(disk.tableReport ?? null)}
                />
              </>
            )}
          </div>
          <footer className="local-statusbar">
            <span>
              <ShieldCheck size={16} />
              {page === "recovery"
                ? t("文件恢复 · 按需只读扫描")
                : page === "backup"
                  ? t("备份与克隆 · 执行前确认")
                  : disk?.kind === "image"
                    ? t("镜像分区表 · 只读")
                    : browsingPartition === `${disk?.id}/${partition?.id}`
                      ? t("浏览文件 · 按需读取目录")
                      : t("Windows 分区管理")}
            </span>
            {disk?.kind !== "image" && (
              <span className="status-time" role="status">
                {loading
                  ? t("正在读取…")
                  : snapshot
                    ? t("上次读取 {time}", {
                        time: new Date(snapshot.observedAt).toLocaleTimeString(
                          language,
                        ),
                      })
                    : t("尚未读取")}
              </span>
            )}
          </footer>
        </main>
      </div>
      <input
        ref={input}
        hidden
        type="file"
        accept=".img,.raw,.vhd,.avhd,.vhdx,.vdi,.vmdk,.hdd,.hds"
        aria-label={t("选择磁盘镜像文件")}
        onChange={(e) => void importImage(e.target.files?.[0])}
      />
      <input
        ref={partsInput}
        hidden
        type="file"
        multiple
        aria-label={t("选择镜像数据文件")}
        onChange={(e) => {
          const added = Array.from(e.target.files ?? []);
          if (!imageParts || !added.length) return;
          const keys = new Set(
            added.map((f) => f.name.normalize("NFC").toLowerCase()),
          );
          const files =
            imageParts.kind === "parent"
              ? [...imageParts.files, ...added]
              : [
                  ...imageParts.files.filter(
                    (f) => !keys.has(f.name.normalize("NFC").toLowerCase()),
                  ),
                  ...added,
                ];
          void importImage(imageParts.primary, files);
        }}
      />
      {imageParts && (
        <Dialog
          title={
            imageParts.kind === "parent"
              ? t("选择父盘文件")
              : t("选择镜像数据文件")
          }
          subtitle={imageParts.primary.name}
          onClose={() => {
            if (!busyRef.current) setImageParts(null);
          }}
        >
          <div className="image-parts">
            <p>
              {imageParts.kind === "parent"
                ? t(
                    "此差分镜像需要以下父盘。可一次选择整条父盘链，也可逐层补齐。",
                  )
                : t("还需要以下数据文件。可以一次选择多个，或分次补齐。")}
            </p>
            <ul aria-label={t("缺少的数据文件")}>
              {imageParts.missing.map((name) => (
                <li key={name}>
                  <Database size={17} />
                  <span>{name}</span>
                </li>
              ))}
            </ul>
            <p className="image-parts-note">
              {t(
                "请选择同一离线镜像的配套文件。只读取所选文件，不修改源文件。",
              )}
            </p>
            <div className="dialog-actions">
              <button
                className="button"
                disabled={busy}
                onClick={() => setImageParts(null)}
              >
                {t("取消")}
              </button>
              <button
                className="button primary"
                disabled={busy}
                onClick={() => partsInput.current?.click()}
              >
                {busy ? (
                  <LoaderCircle size={17} className="spin" />
                ) : (
                  <FolderOpen size={17} />
                )}
                {imageParts.kind === "parent"
                  ? t("选择父盘文件")
                  : t("选择数据文件")}
              </button>
            </div>
          </div>
        </Dialog>
      )}
      {toast && (
        <div
          className={`toast ${toast.error ? "error" : ""}`}
          role={toast.error ? "alert" : "status"}
        >
          {toast.error ? <Info size={18} /> : <Check size={18} />}
          <span>{toast.text}</span>
          <button aria-label={t("关闭通知")} onClick={() => setToast(null)}>
            <X size={15} />
          </button>
        </div>
      )}
      {modal === "details" && disk && (
        <Dialog
          title={t("详细信息")}
          subtitle={diskTitle(disk, t)}
          onClose={closeModal}
          drawer
        >
          {physical ? (
            <LocalDetails disk={disk} partition={partition} target={target} />
          ) : (
            <ImageDetails disk={disk} partition={partition} />
          )}
        </Dialog>
      )}
      {tableReport && (
        <Dialog
          title={t("分区表检查")}
          subtitle={tableReport.filename}
          onClose={() => setTableReport(null)}
          wide
        >
          <TableCheckReport report={tableReport} />
        </Dialog>
      )}
      {partitionOutcome && (
        <PartitionOutcomeDialog
          outcome={partitionOutcome}
          busy={busy}
          onClose={() => setPartitionOutcome(null)}
        />
      )}
      <BackupPasswordPrompt />
      {modal === "settings" && (
        <Dialog title={t("设置")} onClose={closeModal}>
          <div className="settings-panel">
            <span className="settings-label">{t("外观")}</span>
            <div className="theme-options">
              {(
                [
                  ["light", t("浅色"), Sun],
                  ["dark", t("深色"), Moon],
                  ["system", t("跟随系统"), Monitor],
                ] as const
              ).map(([value, label, Icon]) => (
                <button
                  key={value}
                  className={theme === value ? "chosen" : ""}
                  onClick={() => setTheme(value)}
                >
                  <Icon size={21} />
                  {label}
                  {theme === value && <Check size={12} />}
                </button>
              ))}
            </div>
            <label className="setting-row">
              <strong>{t("界面语言")}</strong>
              <select
                className="language-select"
                value={selectedLanguage}
                onChange={(e) =>
                  setPreference(languagePreference(e.target.value))
                }
              >
                <option value="system">{t("跟随系统语言")}</option>
                {Object.entries(languages).map(([code, name]) => (
                  <option key={code} value={code} lang={code}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <label className="setting-row">
              <span>
                <strong>{t("紧凑列表")}</strong>
                <small>{t("在同一屏幕显示更多分区")}</small>
              </span>
              <input
                type="checkbox"
                role="switch"
                checked={compact}
                onChange={(e) => setCompact(e.target.checked)}
              />
            </label>
            <div className="about">
              <Database size={22} />
              <div>
                <strong>
                  DiskStudio <span>0.1.0</span>
                </strong>
                <p>{t("磁盘与分区管理")}</p>
              </div>
            </div>
          </div>
        </Dialog>
      )}
    </div>
  );
}

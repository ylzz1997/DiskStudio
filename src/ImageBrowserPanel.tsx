import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { FileSearch, FolderOpen, HardDrive, X } from "lucide-react";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";
import type { RetainedSearch } from "./RetainedPartitionSearch";
import { RetainedSignatureScan, type RetainedSignatureRequest } from "./RetainedSignatureScan";
import { retainedProgressMatches, type RecoveryProgress, type RetainedScanSelection } from "./recoveryProgressModel";

export type ImageBrowseOptions = {
  kind: "disk" | "partition" | "search";
  sector: 512 | 4096;
};
export type ImageBrowseMode = "existing" | "allFiles" | "deleted";
export type ImageBrowseInfo = {
  id: string;
  name: string;
  format: string;
  files: string[];
  size: number;
  sourceDisks: string[];
  options: ImageBrowseOptions;
  partitions: { id: number; name: string; start: number; size: number }[];
  partitionTable?: "MBR" | "GPT";
  warning: string;
  verificationNotes?: string[];
  parallelsSnapshot?: { id: string; name: string; current: boolean };
  scanId: string | null;
  scanPartition: number | null;
  scanMode: ImageBrowseMode | "signatures" | null;
  search: RetainedSearch | null;
};
export function ImageBrowserPanel({
  info,
  busy,
  available,
  onOpen,
  onClose,
  onBackup,
  onBootInspection,
  onConvertTable,
  onConvertPlacement,
  onBackupTable,
  onRestoreTable,
  onInspectGpt,
  onVirtualize,
  onRead,
  onReconstruct,
  onSignatures,
  onLoadProgress,
  recoveryProgress,
  onCopy,
  lvmName,
  onLvm,
  raidName,
  onRaid,
  lostSearch,
}: {
  info: ImageBrowseInfo | null;
  busy: boolean;
  available: boolean;
  onOpen: (options: ImageBrowseOptions) => void;
  onClose: () => void;
  onBackup: () => void;
  onBootInspection?: () => void;
  onConvertTable?: (table: "mbr" | "gpt") => void;
  onConvertPlacement?: (partition: number) => void;
  onBackupTable: () => void;
  onRestoreTable: () => void;
  onInspectGpt: () => void;
  onVirtualize: () => void;
  onRead: (partition: number, mode: ImageBrowseMode) => void;
  onReconstruct: (partition: number, mode: ImageBrowseMode) => void;
  onSignatures: (request: RetainedSignatureRequest) => void;
  onLoadProgress: (source: RetainedScanSelection) => void;
  recoveryProgress: RecoveryProgress | null;
  onCopy: () => void;
  lvmName?: string;
  onLvm: () => void;
  raidName?: string;
  onRaid: () => void;
  lostSearch?: ReactNode;
}) {
  const { t, language, catalogText } = useLocale();
  const [options, setOptions] = useState<ImageBrowseOptions>({
    kind: "disk",
    sector: 512,
  });
  const [partition, setPartition] = useState("");
  const [mode, setMode] = useState<ImageBrowseMode>("existing");
  useEffect(() => {
    setPartition(String(info?.scanPartition ?? info?.partitions[0]?.id ?? ""));
    setMode(info?.scanMode === "signatures" ? "existing" : (info?.scanMode ?? "existing"));
  }, [info?.id, info?.scanPartition, info?.scanMode]);
  return (
    <section className="data-panel backup-browser" aria-label={t("浏览镜像")}>
      <div className="section-heading">
        <h2>{t("浏览镜像文件")}</h2>
        {info && (
          <div className="header-actions">
            {onBootInspection && info.options.kind !== "search" && (
              <button className="button" disabled={busy || !available} onClick={onBootInspection}>
                {t("Windows 启动检查")}
              </button>
            )}
            {onConvertTable && info.partitionTable && info.options.kind === "disk" && (
              <button className="button" disabled={busy || !available}
                onClick={() => onConvertTable(info.partitionTable === "MBR" ? "gpt" : "mbr")}>
                {t("生成 {style} 镜像副本", { style: info.partitionTable === "MBR" ? "GPT" : "MBR" })}
              </button>
            )}
            <button className="button" disabled={busy || !available} onClick={onBackup}>
              {t("备份镜像内容")}
            </button>
            <button className="button" disabled={busy} onClick={onClose}>
              <X size={15} />
              {t("关闭镜像")}
            </button>
          </div>
        )}
      </div>
      {info && (
        <div className="backup-browser-file">
          <HardDrive size={28} />
          <div>
            <strong title={info.name}>{info.name}</strong>
            <span>
              {info.format} ·{" "}
              {info.options.kind === "partition" ? t("分区镜像") : t("整盘镜像")} ·{" "}
              {formatSize(info.size)} ·{" "}
              {t("{size} B 扇区", { size: info.options.sector })}
              {info.partitionTable && ` · ${info.partitionTable}`}
            </span>
            {info.parallelsSnapshot && (
              <span title={info.parallelsSnapshot.id}>
                {info.parallelsSnapshot.current ? t("当前快照") : t("Parallels 快照")} · {info.parallelsSnapshot.name}
              </span>
            )}
          </div>
        </div>
      )}
      <details className="image-source-settings" open={!info}>
        <summary>
          {info
            ? t("打开其他镜像 · 当前 {count} 个来源文件", {
                count: info.files.length.toLocaleString(language),
              })
            : t("选择镜像文件")}
        </summary>
        <div className="image-browser-open-row">
          <label>
            {t("RAW 范围")}
            <select
              aria-label={t("镜像范围")}
              value={options.kind}
              disabled={busy}
              onChange={(e) =>
                setOptions({
                  ...options,
                  kind: e.target.value as ImageBrowseOptions["kind"],
                })
              }
            >
              <option value="disk">{t("整盘镜像（GPT / MBR）")}</option>
              <option value="search">{t("整盘镜像（搜索丢失分区）")}</option>
              <option value="partition">{t("分区镜像（文件系统）")}</option>
            </select>
          </label>
          <label>
            {t("RAW 扇区")}
            <select
              aria-label={t("镜像浏览扇区大小")}
              value={options.sector}
              disabled={busy}
              onChange={(e) =>
                setOptions({
                  ...options,
                  sector: Number(e.target.value) as 512 | 4096,
                })
              }
            >
              <option value={512}>512 B</option>
              <option value={4096}>4096 B</option>
            </select>
          </label>
          <button
            className={`button ${info ? "" : "primary"}`}
            disabled={busy || !available}
            onClick={() => onOpen(options)}
          >
            <FolderOpen size={16} />
            {info ? t("更换镜像…") : t("打开镜像…")}
          </button>
        </div>
        {info && <p className="data-footnote">{info.files.join(" · ")}</p>}
      </details>
      {info && info.options.kind !== "partition" && (
        <div className="data-action-row">
          {info.options.kind === "disk" && info.partitionTable && (
            <button className="button" disabled={busy || !available} onClick={onBackupTable}>
              {t("备份镜像分区表")}
            </button>
          )}
          <button className="button" disabled={busy || !available} onClick={onRestoreTable}>
            {t("恢复镜像分区表")}
          </button>
          {(info.partitionTable === "GPT" || info.options.kind === "search") && (
            <button className="button" disabled={busy || !available} onClick={onInspectGpt}>
              {t("检查并修复镜像 GPT")}
            </button>
          )}
          {info.options.kind === "disk" && (
            <button className="button" disabled={busy || !available} onClick={onVirtualize}>
              {t("导出 Windows 虚拟机")}
            </button>
          )}
        </div>
      )}
      <div className="data-action-row">
        {lvmName && <span>{lvmName}</span>}
        <button className="button" disabled={busy || !available} onClick={onLvm}>
          {t("浏览镜像中的 LVM2 逻辑卷")}
        </button>
      </div>
      <div className="data-action-row">
        {raidName && <span>{raidName}</span>}
        <button className="button" disabled={busy || !available} onClick={onRaid}>
          {t("虚拟 RAID")}
        </button>
      </div>
      {lostSearch}
      {info && info.options.kind !== "search" && (
        <>
          <label className="backup-browser-partition">
            {t("镜像中的分区")}
            <select
              aria-label={t("镜像中的分区")}
              value={partition}
              disabled={busy || !info.partitions.length}
              onChange={(e) => setPartition(e.target.value)}
            >
              {!info.partitions.length && (
                <option value="">{t("没有可浏览的分区")}</option>
              )}
              {info.partitions.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} · {formatSize(p.size)}
                </option>
              ))}
            </select>
          </label>
          <label className="backup-browser-partition">
            {t("文件范围")}
            <select
              aria-label={t("镜像文件范围")}
              value={mode}
              disabled={busy}
              onChange={(e) => setMode(e.target.value as ImageBrowseMode)}
            >
              <option value="existing">{t("现存文件")}</option>
              <option value="allFiles">{t("现存文件与删除候选")}</option>
              <option value="deleted">{t("仅删除候选")}</option>
            </select>
          </label>
          {(info.warning || !!info.verificationNotes?.length) && (
            <details className="image-source-settings">
              <summary>{t("镜像检查说明")}</summary>
              {info.warning && <p className="data-footnote">{catalogText(info.warning)}</p>}
              {info.verificationNotes?.map((note, index) => {
                const prefix = "读取顺序：";
                return (
                  <p className="data-footnote" key={index}>
                    {note.startsWith(prefix)
                      ? `${catalogText("镜像链读取顺序")} · ${note.slice(prefix.length).replace(/。$/, "")}`
                      : catalogText(note)}
                  </p>
                );
              })}
            </details>
          )}
          <div className="data-action-row">
            <span>{t("读取后可预览、搜索并提取文件或整个目录。")}</span>
            {onConvertPlacement && info.partitionTable === "MBR" && (
              <button className="button" disabled={busy || !available || partition === ""}
                onClick={() => onConvertPlacement(Number(partition))}>
                {t("转换选中分区的主/逻辑形式")}
              </button>
            )}
            <button className="button" disabled={busy || !available} onClick={onCopy}>
              {t("复制镜像扇区")}
            </button>
            <button className="button" disabled={busy || !available || partition === ""}
              onClick={() => onReconstruct(Number(partition), mode)}
              title={t("使用 NTFS、FAT32、exFAT 或 Ext4 的备用元数据读取文件，提取前重新核对。")}
            >
              {t("重建文件视图")}
            </button>
            <button
              className="button primary"
              disabled={busy || !available || partition === ""}
              onClick={() => onRead(Number(partition), mode)}
            >
              <FileSearch size={16} />
              {t("读取目录")}
            </button>
          </div>
        </>
      )}
      {info && (info.options.kind !== "partition" || info.partitions.length > 0) && (
        <RetainedSignatureScan key={`signatures-${info.id}`} kind="image" info={info} whole={info.options.kind !== "partition"}
          disabled={busy || !available} onStart={onSignatures} onLoad={onLoadProgress}
          progress={retainedProgressMatches(recoveryProgress, "image", info) ? recoveryProgress : null} />
      )}
      <details className="image-source-settings">
        <summary>{t("支持范围与读取说明")}</summary>
        <p className="data-footnote">
          {t(
            "支持 RAW/IMG、VHD/AVHD、VHDX/AVHDX、VDI、VMDK 和 Parallels HDD/HDS。请用 DiskDescriptor.xml 打开 Parallels 目录包，并明确选择父盘和数据分卷。",
          )}
        </p>
        <p className="data-footnote">
          {t(
            "可读取 NTFS、FAT、exFAT 以及干净卸载的 Ext2/3/4 和 Btrfs 镜像。提取到其他磁盘并逐文件核对 SHA-256。RAW 分区镜像需选定正确的范围和扇区大小。",
          )}
        </p>
      </details>
    </section>
  );
}

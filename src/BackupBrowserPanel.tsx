import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { Archive, FolderOpen, Search, X } from "lucide-react";
import { formatSize } from "./types";
import { useLocale } from "./i18n/LanguageProvider";
import type { Translate } from "./i18n/translate";
import type { ImageBrowseMode } from "./ImageBrowserPanel";
import type { RetainedSearch } from "./RetainedPartitionSearch";
import { RetainedSignatureScan, type RetainedSignatureRequest } from "./RetainedSignatureScan";
import { retainedProgressMatches, type RecoveryProgress, type RetainedScanSelection } from "./recoveryProgressModel";

export type BackupInfo = {
  id: string;
  name: string;
  version: number;
  kind: string;
  size: number;
  sector: number;
  layers: number;
  points: { id: string; label: string; ordinal?: number }[];
  selectedPoint: string;
  partitions: {
    id: number;
    name: string;
    start: number;
    size: number;
    restoreFormat?: "fat" | "exfat" | "ntfs" | "btrfs" | "ext";
  }[];
  warning: string;
  tableIssue?: string;
  allocationSkippedBytes?: number;
  sourceDisks: string[];
  scanId: string | null;
  scanPartition: number | null;
  scanMode: ImageBrowseMode | "signatures" | null;
  search: RetainedSearch | null;
};

export function backupPointLabel(
  point: BackupInfo["points"][number],
  t: Translate,
  catalogText: (label: string) => string,
) {
  return point.ordinal === undefined
    ? catalogText(point.label)
    : t("此前第 {layer} 层增量备份", { layer: point.ordinal });
}

export function BackupBrowserPanel({
  info,
  busy,
  available,
  onOpen,
  onClose,
  onBootInspection,
  onRead,
  onReconstruct,
  onSignatures,
  onLoadProgress,
  recoveryProgress,
  onPoint,
  onRestore,
  lostSearch,
}: {
  info: BackupInfo | null;
  busy: boolean;
  available: boolean;
  onOpen: () => void;
  onClose: () => void;
  onBootInspection?: () => void;
  onRead: (partition: number) => void;
  onReconstruct: (partition: number) => void;
  onSignatures: (request: RetainedSignatureRequest) => void;
  onLoadProgress: (source: RetainedScanSelection) => void;
  recoveryProgress: RecoveryProgress | null;
  onPoint: (point: string) => void;
  onRestore?: (partition: number | null) => void;
  lostSearch?: ReactNode;
}) {
  const { language, t, catalogText } = useLocale();
  const [partition, setPartition] = useState("");
  useEffect(() => {
    setPartition(String(info?.scanPartition ?? info?.partitions[0]?.id ?? ""));
  }, [info?.id, info?.scanPartition]);
  return (
    <section className="data-panel backup-browser" aria-label={t("浏览备份")}>
      <div className="section-heading">
        <h2>{t("浏览备份文件")}</h2>
        {info && (
          <div className="header-actions">
            {onBootInspection && (
              <button className="button" disabled={busy || !available} onClick={onBootInspection}>
                {t("Windows 启动检查")}
              </button>
            )}
            <button className="button" disabled={busy} onClick={onClose}>
              <X size={15} />
              {t("关闭备份")}
            </button>
          </div>
        )}
      </div>
      {info ? (
        <>
          <div className="backup-browser-file">
            <Archive size={28} />
            <div>
              <strong title={info.name}>{info.name}</strong>
              <span>
                {info.kind === "disk" ? t("整盘备份") : t("分区备份")} ·{" "}
                {formatSize(info.size)} ·{" "}
                {info.layers > 1
                  ? t("增量链 {count} 层", {
                      count: info.layers.toLocaleString(language),
                    })
                  : t("完整备份")}{" "}
                · {info.layers > 1 ? t("所选还原点已校验") : t("已校验")}
              </span>
            </div>
            <button
              className="button"
              disabled={busy || !available}
              onClick={onOpen}
            >
              <FolderOpen size={16} />
              {t("更换备份…")}
            </button>
          </div>
          {info.points.length > 1 && (
            <label className="backup-browser-partition">
              {t("还原点")}
              <select
                aria-label={t("备份还原点")}
                value={info.selectedPoint}
                disabled={busy || !available}
                onChange={(e) => onPoint(e.target.value)}
              >
                {info.points.map((p) => (
                  <option key={p.id} value={p.id}>
                    {backupPointLabel(p, t, catalogText)}
                  </option>
                ))}
              </select>
            </label>
          )}
          {lostSearch}
          <label className="backup-browser-partition">
            {t("备份中的分区")}
            <select
              aria-label={t("备份中的分区")}
              value={partition}
              disabled={busy || !info.partitions.length}
              onChange={(e) => setPartition(e.target.value)}
            >
              {!info.partitions.length && (
                <option value="">{t("没有可浏览的分区")}</option>
              )}
              {info.partitions.map((p) => (
                <option key={p.id} value={p.id}>
                  {info.kind === "partition" ? t("备份分区") : p.name} ·{" "}
                  {formatSize(p.size)}
                </option>
              ))}
            </select>
          </label>
          {info.warning && (
            <p className="data-footnote">{catalogText(info.warning)}</p>
          )}
          {info.allocationSkippedBytes !== undefined && (
            <p className="data-footnote">
              {t("按已用空间备份：{free} B 确认空闲区域按零保存，不包含这些区域的已删除内容。", {
                free: info.allocationSkippedBytes.toLocaleString(language),
              })}
            </p>
          )}
          {info.tableIssue !== undefined && (
            <details className="data-footnote">
              <summary>
                {t("此还原点的分区表无法解析或未通过校验。整盘还原会保留归档中的分区表原始状态。")}
              </summary>
              <p>{catalogText(info.tableIssue)}</p>
            </details>
          )}
          <div className="data-action-row">
            <span>
              {t(
                "读取目录后可预览、选择文件，提取到其他磁盘。文件系统支持范围与“文件恢复”一致。",
              )}
            </span>
            <button
              className="button"
              disabled={busy || !available || partition === ""}
              onClick={() => onReconstruct(Number(partition))}
              title={t(
                "使用 NTFS、FAT32、exFAT 或 Ext4 的备用元数据读取文件，提取前重新核对。",
              )}
            >
              {t("重建文件视图")}
            </button>
            <button
              className="button primary"
              disabled={busy || !available || partition === ""}
              onClick={() => onRead(Number(partition))}
            >
              <Search size={16} />
              {t("读取目录")}
            </button>
          </div>
          {(info.kind === "disk" || info.partitions.length > 0) && (
            <RetainedSignatureScan key={`signatures-${info.id}`} kind="backup" info={info} whole={info.kind === "disk"}
              disabled={busy || !available} onStart={onSignatures} onLoad={onLoadProgress}
              progress={retainedProgressMatches(recoveryProgress, "backup", info) ? recoveryProgress : null} />
          )}
          {onRestore && (
            <div className="data-action-row">
              <span>{t("使用当前还原点，将所选范围写入另一块物理磁盘。")}</span>
              {info.kind === "disk" && (
                <button
                  className="button"
                  disabled={busy || !available}
                  onClick={() => onRestore(null)}
                >
                  {t("还原备份中的整盘…")}
                </button>
              )}
              <button
                className="button"
                disabled={
                  busy ||
                  !available ||
                  !info.partitions.some((part) => String(part.id) === partition)
                }
                onClick={() => onRestore(Number(partition))}
              >
                {t("还原所选备份分区…")}
              </button>
            </div>
          )}
        </>
      ) : (
        <div className="backup-browser-empty">
          <Archive size={36} />
          <h3>{t("从备份中找到需要的文件")}</h3>
          <p>{t("直接浏览目录并提取文件，无需还原整盘或挂载备份。")}</p>
          <button
            className="button primary"
            disabled={busy || !available}
            onClick={onOpen}
          >
            <FolderOpen size={17} />
            {t("打开备份…")}
          </button>
        </div>
      )}
      <p className="data-footnote">
        {t(
          "支持 DiskStudio .dsbackup 完整、压缩及增量备份。打开时校验内容；增量备份需选择完整依赖链。暂不支持 DiskGenius 专有备份文件。",
        )}
      </p>
    </section>
  );
}

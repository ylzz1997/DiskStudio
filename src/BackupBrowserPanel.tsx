import { useEffect, useState } from "react";
import { Archive, FolderOpen, Search, X } from "lucide-react";
import { formatSize } from "./types";
import { useLocale } from "./i18n/LanguageProvider";

export type BackupInfo = {
  id: string;
  name: string;
  version: number;
  kind: string;
  size: number;
  sector: number;
  layers: number;
  points: { id: string; label: string }[];
  selectedPoint: string;
  partitions: { id: number; name: string; start: number; size: number }[];
  warning: string;
  scanId: string | null;
  scanPartition: number | null;
};

export function BackupBrowserPanel({
  info,
  busy,
  available,
  onOpen,
  onClose,
  onRead,
  onPoint,
}: {
  info: BackupInfo | null;
  busy: boolean;
  available: boolean;
  onOpen: () => void;
  onClose: () => void;
  onRead: (partition: number) => void;
  onPoint: (point: string) => void;
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
          <button className="button" disabled={busy} onClick={onClose}>
            <X size={15} />
            {t("关闭备份")}
          </button>
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
                    {p.label}
                  </option>
                ))}
              </select>
            </label>
          )}
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
                  {p.name} · {formatSize(p.size)}
                </option>
              ))}
            </select>
          </label>
          {info.warning && (
            <p className="data-footnote">{catalogText(info.warning)}</p>
          )}
          <div className="data-action-row">
            <span>
              {t(
                "读取目录后可预览、选择文件，提取到其他磁盘。文件系统支持范围与“文件恢复”一致。",
              )}
            </span>
            <button
              className="button primary"
              disabled={busy || !available || partition === ""}
              onClick={() => onRead(Number(partition))}
            >
              <Search size={16} />
              {t("读取目录")}
            </button>
          </div>
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

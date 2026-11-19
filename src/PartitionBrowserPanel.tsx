import { ArrowLeft, FolderOpen, RefreshCcw } from "lucide-react";
import type { ReactNode } from "react";
import type { Partition } from "./types";
import { formatSize } from "./types";
import { partitionTitle } from "./LocalDiskView";
import { useLocale } from "./i18n/LanguageProvider";

export function PartitionBrowserPanel({
  partition,
  busy,
  available,
  loaded,
  onRead,
  onBack,
  children,
}: {
  partition: Partition | undefined;
  busy: boolean;
  available: boolean;
  loaded: boolean;
  onRead: () => void;
  onBack: () => void;
  children?: ReactNode;
}) {
  const { t, catalogText } = useLocale();
  return (
    <section
      className="data-panel partition-browser"
      aria-label={t("分区文件浏览")}
    >
      <button className="button text-button" disabled={busy} onClick={onBack}>
        <ArrowLeft size={16} />
        {t("返回分区布局")}
      </button>
      <div className="partition-browser-source">
        <FolderOpen size={30} />
        <div>
          <h2>
            {partition
              ? partitionTitle(partition, catalogText)
              : t("分区已不存在")}
          </h2>
          {partition && (
            <p>
              {formatSize(partition.size)} · {catalogText(partition.fs)} ·{" "}
              {partition.driveLetter
                ? `${partition.driveLetter}:`
                : t("未分配盘符")}
            </p>
          )}
        </div>
        <button
          className="button primary"
          disabled={busy || !available || partition?.number === undefined}
          onClick={onRead}
        >
          {loaded ? <RefreshCcw size={16} /> : <FolderOpen size={16} />}
          {loaded ? t("重新读取目录") : t("读取目录")}
        </button>
      </div>
      {children}
      <p className="data-footnote">
        {t(
          "可浏览 NTFS、FAT、exFAT 以及干净卸载的 Ext2/3/4 和 Btrfs 分区。读取后可预览文件并复制到其他磁盘。",
        )}
      </p>
      <p className="data-footnote">
        {t(
          "仅在点击后读取分区内容，不挂载、不修改源。在线文件可能变化；达到数量上限、取消或读取失败时，列表只包含已读取的部分。",
        )}
      </p>
    </section>
  );
}

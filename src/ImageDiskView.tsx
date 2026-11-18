import type { CSSProperties } from "react";
import { Info, ShieldCheck } from "lucide-react";
import type { Disk, Partition } from "./types";
import { formatSize } from "./types";
import { DiskIcon, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";

export function ImageDiskContent({
  disk,
  selectedId,
  onSelect,
  onDetails,
  onCheck,
}: {
  disk: Disk;
  selectedId?: string;
  onSelect: (id: string) => void;
  onDetails: () => void;
  onCheck: () => void;
}) {
  const { t } = useLocale();
  const selected = disk.partitions.find((p) => p.id === selectedId);
  return (
    <>
      <section className="layout-section" aria-label={t("镜像分区布局")}>
        <div className="section-heading">
          <h2>{t("分区布局")}</h2>
          <button
            className="button text-button image-check-button"
            onClick={onCheck}
            disabled={!disk.tableReport}
          >
            <ShieldCheck size={16} />
            {t("分区表检查")}
          </button>
        </div>
        <div className="partition-map physical-map image-partition-map">
          {disk.partitions.map((p) => (
            <button
              key={p.id}
              className={`partition-block ${p.color} ${p.id === selectedId ? "selected" : ""} ${p.fs !== "—" ? "usage-unknown" : ""} ${p.size / disk.size < 0.012 ? "small-block" : ""}`}
              style={
                {
                  flexGrow: p.size / disk.size,
                  "--used": "0%",
                } as CSSProperties
              }
              onClick={() => onSelect(p.id)}
              aria-pressed={p.id === selectedId}
              aria-label={t("选择分区 {name}，{size}", {
                name: p.name,
                size: formatSize(p.size),
              })}
              title={`${p.name} · ${formatSize(p.size)}`}
            >
              {p.size / disk.size >= 0.012 && (
                <span className="block-label">
                  <strong>{p.name}</strong>
                  <small>
                    {formatSize(p.size)}
                    {p.fs !== "—" && ` · ${p.fs}`}
                  </small>
                </span>
              )}
            </button>
          ))}
        </div>
        <div className="legend">
          <span>
            <b className="free-key" />
            {t("未分配")}
          </span>
          <em>{t("文件系统占用未知")}</em>
        </div>
      </section>
      {selected && (
        <div className="context-toolbar">
          <div className="selection-label">
            <span className="selection-dot" />
            <span>
              {t("已选中")} <strong>{selected.name}</strong>
            </span>
          </div>
          <button className="button" onClick={onDetails}>
            <Info size={16} />
            {t("分区信息")}
          </button>
        </div>
      )}
      <div className="table-wrap">
        <table className="partition-table">
          <thead>
            <tr>
              <th>{t("分区名称")}</th>
              <th>{t("文件系统")}</th>
              <th>{t("容量")}</th>
              <th>{t("起始偏移")}</th>
            </tr>
          </thead>
          <tbody>
            {disk.partitions.map((p) => (
              <tr
                key={p.id}
                className={p.id === selectedId ? "selected" : ""}
                tabIndex={0}
                aria-selected={p.id === selectedId}
                onClick={() => onSelect(p.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onSelect(p.id);
                  }
                }}
              >
                <td>
                  <span className="partition-name">
                    <DiskIcon size={21} />
                    {p.name}
                  </span>
                </td>
                <td>{p.fs}</td>
                <td>{formatSize(p.size)}</td>
                <td>{formatSize(p.start)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="import-note">
        <ShieldCheck size={15} />
        {t("镜像以只读方式打开，仅显示分区表信息，不写回源文件。")}
      </div>
    </>
  );
}

export function ImageDetails({
  disk,
  partition,
}: {
  disk: Disk;
  partition?: Partition;
}) {
  const { language, t } = useLocale();
  return (
    <>
      <div className="details-hero">
        <DiskIcon size={40} />
        <h3>{partition?.name ?? disk.name}</h3>
        <span>{partition?.fs === "—" ? partition.name : partition?.fs}</span>
      </div>
      <dl className="detail-list">
        <dt>{t("磁盘容量")}</dt>
        <dd>{formatSize(disk.size)}</dd>
        <dt>{t("分区表")}</dt>
        <dd>{disk.scheme}</dd>
        <dt>{t("逻辑扇区")}</dt>
        <dd>{disk.sectorSize} B</dd>
        <dt>{t("数据来源")}</dt>
        <dd>{t("导入镜像 · 只读")}</dd>
        {disk.imageFormat && (
          <>
            <dt>{t("镜像格式")}</dt>
            <dd>{disk.imageFormat}</dd>
          </>
        )}
        {disk.imageFileSize !== undefined && (
          <>
            <dt>
              {(disk.imageFileCount ?? 1) > 1 ? t("文件合计") : t("文件长度")}
            </dt>
            <dd>{formatSize(disk.imageFileSize)}</dd>
          </>
        )}
        {(disk.imageFileCount ?? 1) > 1 && (
          <>
            <dt>{t("文件数量")}</dt>
            <dd>
              {t(
                disk.imageChain
                  ? "{count} 个（含当前镜像与父盘）"
                  : "{count} 个（含描述文件）",
                {
                  count: (disk.imageFileCount ?? 1).toLocaleString(language),
                },
              )}
            </dd>
          </>
        )}
        <dt>{t("文件名")}</dt>
        <dd>{disk.filename}</dd>
        {partition && (
          <>
            <dt>{t("分区容量")}</dt>
            <dd>{formatSize(partition.size)}</dd>
            <dt>{t("起始偏移")}</dt>
            <dd>{partition.start.toLocaleString(language)} B</dd>
            <dt>{t("4 KiB 对齐")}</dt>
            <dd>{partition.start % 4096 === 0 ? t("已对齐") : t("未对齐")}</dd>
          </>
        )}
      </dl>
      <Hint>
        {t(
          "仅解析所选镜像文件的分区表，不读取分区内文件，不推测文件系统占用。",
        )}
        {disk.imageNotes?.map((note) => (
          <span key={note}>
            <br />
            {note}
          </span>
        ))}
      </Hint>
    </>
  );
}

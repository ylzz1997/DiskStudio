import type { CSSProperties, ReactNode } from "react";
import {
  CircleHelp,
  FolderOpen,
  Info,
  LoaderCircle,
  RefreshCcw,
  ShieldCheck,
} from "lucide-react";
import type { BitLockerState, Disk, Partition, PartitionTarget } from "./types";
import { formatSize } from "./types";
import { DiskIcon, Empty, Hint } from "./components";
import { PartitionPlatformIcon } from "./PartitionPlatformIcon";
import { FilesystemSupportPanel } from "./FilesystemSupportPanel";
import { useLocale } from "./i18n/LanguageProvider";
import type { Message, Translate } from "./i18n/translate";

const bitLockerStates: Record<BitLockerState, Message> = {
  encrypted: "已加密",
  encrypting: "正在加密",
  decrypting: "正在解密",
  encryptionPaused: "加密已暂停",
  decryptionPaused: "解密已暂停",
  unknown: "未知",
};

export function bitLockerState(state: BitLockerState, t: Translate) {
  return t(bitLockerStates[state]);
}

export function diskTitle(disk: Disk, t: Translate) {
  return disk.hardware
    ? t("磁盘 {number}", { number: disk.hardware.number })
    : disk.name;
}

function partitionName(p: Partition, catalogText: (text: string) => string) {
  return p.number === undefined || p.volumeLabel?.trim()
    ? p.name
    : catalogText(p.name);
}

export function partitionTitle(
  p: Partition,
  catalogText: (text: string) => string,
) {
  const name = partitionName(p, catalogText);
  return p.driveLetter ? `${name} (${p.driveLetter}:)` : name;
}

export function LocalDiskHeader({
  disk,
  loading,
  managed = false,
  onRefresh,
  onDetails,
}: {
  disk: Disk;
  loading: boolean;
  managed?: boolean;
  onRefresh: () => void;
  onDetails: () => void;
}) {
  const { t } = useLocale();
  const h = disk.hardware!;
  const model = h.model === "未提供型号" ? t("未提供型号") : h.model;
  return (
    <header className="disk-header local-header">
      <div className="disk-heading">
        <DiskIcon size={39} />
        <div>
          <div className="local-title">
            <h1>{diskTitle(disk, t)}</h1>
            {h.isSystem && <span className="system-tag">{t("系统磁盘")}</span>}
            <span className="view-badge">
              <ShieldCheck size={13} />
              {managed ? t("可管理") : t("暂不可操作")}
            </span>
          </div>
          <p className="device-model" title={model}>
            {model}
          </p>
        </div>
      </div>
      <div className="header-actions">
        <button className="button" disabled={loading} onClick={onRefresh}>
          {loading ? (
            <LoaderCircle size={16} className="spin" />
          ) : (
            <RefreshCcw size={16} />
          )}
          {loading ? t("正在读取") : t("刷新信息")}
        </button>
        <button className="button text-button" onClick={onDetails}>
          <Info size={17} />
          {t("详细信息")}
        </button>
      </div>
    </header>
  );
}

export function LocalDiskContent({
  disk,
  selectedId,
  onSelect,
  onDetails,
  error,
  warnings,
  target,
  actions,
  onBrowse,
  busy = false,
}: {
  disk: Disk;
  selectedId?: string;
  onSelect: (id: string) => void;
  onDetails: () => void;
  error: string;
  warnings: string[];
  target?: PartitionTarget;
  actions?: ReactNode;
  onBrowse?: () => void;
  busy?: boolean;
}) {
  const { language, t, catalogText } = useLocale();
  const h = disk.hardware!;
  const small = disk.partitions.filter((p) => p.size / disk.size < 0.012);
  const selected = disk.partitions.find((p) => p.id === selectedId);
  const mapEntries: { partition?: Partition; offset: number; size: number }[] =
    [
      ...disk.partitions.map((partition) => ({
        partition,
        offset: partition.start,
        size: partition.size,
      })),
      ...(target?.freeRanges ?? []).map((range) => ({ ...range })),
    ].sort((a, b) => a.offset - b.offset);
  const driveLetters = disk.partitions
    .filter((p) => p.driveLetter)
    .map((p) => `${p.driveLetter}:`)
    .join("  ");
  return (
    <>
      <div className="hardware-facts">
        <div>
          <span>{t("总容量")}</span>
          <strong>{formatSize(disk.size)}</strong>
        </div>
        <div>
          <span>{t("连接 / 分区表")}</span>
          <strong>
            {catalogText(h.bus)}
            <i />
            {catalogText(disk.scheme)}
          </strong>
        </div>
        <div>
          <span>{t("盘符")}</span>
          <strong className="drive-letter-list">
            {driveLetters || t("未分配盘符")}
          </strong>
        </div>
        <div>
          <span>{t("Windows 状态")}</span>
          <strong
            className={
              h.health === "正常" && h.offline === false ? "device-healthy" : ""
            }
          >
            <b className="status-dot" />
            {h.offline === true ? t("脱机") : catalogText(h.health)}
          </strong>
        </div>
      </div>
      {actions}
      {error && (
        <div className="device-notice error" role="alert">
          {t("读取失败：{error} 当前显示上次读取的信息。", {
            error: catalogText(error),
          })}
        </div>
      )}
      {warnings.map((w) => (
        <div key={w} className="device-notice" role="status">
          {catalogText(w)}
        </div>
      ))}
      {!h.layoutAvailable ? (
        <Empty
          icon={<CircleHelp size={34} />}
          title={t("分区信息暂不可用")}
          text={
            h.layoutNote ? catalogText(h.layoutNote) : t("请稍后重新读取。")
          }
        />
      ) : disk.partitions.length === 0 && (!target || disk.scheme === "RAW") ? (
        <Empty
          icon={<DiskIcon size={34} />}
          title={t("Windows 未报告分区")}
          text={
            disk.scheme === "RAW"
              ? target?.available
                ? t("先初始化为 GPT，再创建分区和文件系统。")
                : t("这块磁盘尚未初始化；操作能力及限制见上方工具栏。")
              : t("当前没有可显示的分区信息。")
          }
        />
      ) : (
        <>
          <section
            className="layout-section local-layout"
            aria-label={t("本机磁盘分区布局")}
          >
            <div className="section-heading">
              <h2>{t("分区布局")}</h2>
              <span className="section-caption">
                {t("{count} 个分区", {
                  count: disk.partitions.length.toLocaleString(language),
                })}
              </span>
            </div>
            <div className="partition-map physical-map">
              {mapEntries.map(({ partition: p, offset, size }) => {
                if (!p)
                  return (
                    <div
                      key={`free-${offset}`}
                      className="partition-block free partition-free-block"
                      style={{ flexGrow: size / disk.size }}
                      title={t("未分配 · {size}", { size: formatSize(size) })}
                    >
                      <span className="block-label">
                        <strong>{t("未分配")}</strong>
                        <small>{formatSize(size)}</small>
                      </span>
                    </div>
                  );
                const tiny = p.size / disk.size < 0.012;
                const title = partitionTitle(p, catalogText);
                return (
                  <button
                    key={p.id}
                    className={`partition-block ${p.color} ${tiny ? "small-block" : ""} ${!tiny && p.platform ? "has-platform" : ""} ${selectedId === p.id ? "selected" : ""} ${p.used === null ? "usage-unknown" : ""}`}
                    style={
                      {
                        flexGrow: p.size / disk.size,
                        "--used": `${p.used === null ? 0 : (p.used / p.size) * 100}%`,
                      } as CSSProperties
                    }
                    onClick={() => onSelect(p.id)}
                    aria-pressed={selectedId === p.id}
                    aria-label={t("选择分区 {name}，{size}", {
                      name: title,
                      size: formatSize(p.size),
                    })}
                    title={[
                      title,
                      formatSize(p.size),
                      p.role && catalogText(p.role),
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  >
                    <div className="usage-fill" />
                    {!tiny && (
                      <>
                        <span className="block-label">
                          <strong>{title}</strong>
                          <small>
                            {formatSize(p.size)} ·{" "}
                            {p.fs === "未知" ? t("文件系统未知") : p.fs}
                          </small>
                          {p.available != null && (
                            <em>
                              {t("{size} 可用", {
                                size: formatSize(p.available),
                              })}
                            </em>
                          )}
                        </span>
                        <PartitionPlatformIcon platform={p.platform} />
                      </>
                    )}
                  </button>
                );
              })}
            </div>
            {small.length > 0 && (
              <div className="small-partitions" aria-label={t("小分区")}>
                {small.map((p) => (
                  <button
                    key={p.id}
                    className={`${p.color} ${selectedId === p.id ? "selected" : ""}`}
                    onClick={() => onSelect(p.id)}
                    aria-pressed={selectedId === p.id}
                  >
                    <b />
                    {partitionName(p, catalogText)}
                    <span>{formatSize(p.size)}</span>
                  </button>
                ))}
              </div>
            )}
            <div className="legend physical-legend">
              <span>
                <b className="used-key" />
                {t("已用")}
              </span>
              <span>
                <b className="available-key" />
                {t("可用")}
              </span>
              <span>
                <b className="unknown-key" />
                {t("占用未知")}
              </span>
              {target && (
                <span>
                  <b className="free-key" />
                  {t("未分配")}
                </span>
              )}
              <em>
                {small.length > 0
                  ? t("小分区已放大显示")
                  : t("按 Windows 提供的分区信息显示")}
              </em>
            </div>
          </section>
          <div className="context-toolbar local-context">
            <div className="selection-label">
              <span className="selection-dot" />
              <span>
                {t("已选中")}{" "}
                <strong>
                  {selected ? partitionTitle(selected, catalogText) : t("磁盘")}
                </strong>
              </span>
            </div>
            <div className="header-actions">
              {onBrowse && (
                <button
                  className="button"
                  onClick={onBrowse}
                  disabled={
                    busy ||
                    selected?.number === undefined ||
                    !h.identity ||
                    !h.layoutKey
                  }
                >
                  <FolderOpen size={16} />
                  {t("浏览文件")}
                </button>
              )}
              <button className="button" onClick={onDetails}>
                <Info size={16} />
                {t("分区信息")}
              </button>
            </div>
          </div>
          <div className="table-wrap">
            <table className="partition-table local-table">
              <thead>
                <tr>
                  <th>{t("分区 / 盘符")}</th>
                  <th>{t("文件系统")}</th>
                  <th>{t("容量")}</th>
                  <th>{t("已用")}</th>
                  <th>{t("可用")}</th>
                  <th>{t("用途")}</th>
                </tr>
              </thead>
              <tbody>
                {!disk.partitions.length && (
                  <tr>
                    <td colSpan={6} className="muted-value">
                      {t("暂无分区，可在未分配空间中新建。")}
                    </td>
                  </tr>
                )}
                {disk.partitions.map((p) => (
                  <tr
                    key={p.id}
                    tabIndex={0}
                    aria-selected={selectedId === p.id}
                    className={selectedId === p.id ? "selected" : ""}
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
                        {p.driveLetter ? (
                          <span className="drive-letter">{p.driveLetter}:</span>
                        ) : (
                          <DiskIcon size={20} />
                        )}
                        <span>{partitionName(p, catalogText)}</span>
                      </span>
                    </td>
                    <td className={p.fs === "未知" ? "muted-value" : ""}>
                      {catalogText(p.fs)}
                    </td>
                    <td>{formatSize(p.size)}</td>
                    <td>{formatSize(p.used)}</td>
                    <td>{formatSize(p.available ?? null)}</td>
                    <td>
                      <span className="partition-role">
                        {p.role && catalogText(p.role)}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {disk.partitions.some((p) => p.used === null) && (
            <div className="import-note">
              <CircleHelp size={15} />
              {t(
                "Windows 未提供部分分区的文件系统或占用信息，以“未知”和“—”表示。",
              )}
            </div>
          )}
        </>
      )}
    </>
  );
}

export function LocalDetails({
  disk,
  partition,
  target,
}: {
  disk: Disk;
  partition?: Partition;
  target?: PartitionTarget;
}) {
  const { language, t, catalogText } = useLocale();
  const h = disk.hardware!;
  const model = h.model === "未提供型号" ? t("未提供型号") : h.model;
  return (
    <>
      <div className="details-hero">
        <DiskIcon size={40} />
        <h3>
          {partition
            ? partitionTitle(partition, catalogText)
            : diskTitle(disk, t)}
        </h3>
        <span>{partition?.role ? catalogText(partition.role) : model}</span>
      </div>
      <dl className="detail-list">
        <dt>{t("磁盘编号")}</dt>
        <dd>{h.number}</dd>
        <dt>{t("设备型号")}</dt>
        <dd>{model}</dd>
        <dt>{t("总容量")}</dt>
        <dd>{formatSize(disk.size)}</dd>
        <dt>{t("连接类型")}</dt>
        <dd>{catalogText(h.bus)}</dd>
        <dt>{t("分区表")}</dt>
        <dd>{catalogText(disk.scheme)}</dd>
        <dt>{t("逻辑扇区")}</dt>
        <dd>{disk.sectorSize ? `${disk.sectorSize} B` : t("未提供")}</dd>
        <dt>{t("Windows 状态")}</dt>
        <dd>
          {catalogText(h.health)}
          {h.offline !== null && <> · {t(h.offline ? "脱机" : "联机")}</>}
        </dd>
        <dt>{t("设备只读属性")}</dt>
        <dd>
          {h.deviceReadOnly === null
            ? t("未提供")
            : h.deviceReadOnly
              ? t("是")
              : t("否")}
          {t("（系统报告）")}
        </dd>
        <dt>{t("分区管理")}</dt>
        <dd>
          {target?.available
            ? t("支持操作 · 提交前需确认")
            : target?.reason
              ? catalogText(target.reason)
              : t("暂不可操作")}
        </dd>
        {target && (
          <>
            <dt>{t("磁盘序列号")}</dt>
            <dd>{target.serial}</dd>
          </>
        )}
        {partition && (
          <>
            <dt>{t("分区编号")}</dt>
            <dd>{partition.number}</dd>
            <dt>{t("盘符")}</dt>
            <dd>
              {partition.driveLetter ? `${partition.driveLetter}:` : t("无")}
            </dd>
            <dt>{t("用途")}</dt>
            <dd>{partition.role && catalogText(partition.role)}</dd>
            <dt>{t("文件系统")}</dt>
            <dd>{catalogText(partition.fs)}</dd>
            {partition.encryption && (
              <>
                <dt>BitLocker</dt>
                <dd>{bitLockerState(partition.encryption, t)}</dd>
              </>
            )}
            <dt>{t("分区容量")}</dt>
            <dd>{formatSize(partition.size)}</dd>
            <dt>{t("卷容量")}</dt>
            <dd>{formatSize(partition.volumeSize ?? null)}</dd>
            <dt>{t("已用 / 可用")}</dt>
            <dd>
              {formatSize(partition.used)} /{" "}
              {formatSize(partition.available ?? null)}
            </dd>
            <dt>{t("起始偏移")}</dt>
            <dd>{partition.start.toLocaleString(language)} B</dd>
            <dt>{t("4 KiB 对齐")}</dt>
            <dd>{partition.start % 4096 === 0 ? t("已对齐") : t("未对齐")}</dd>
            <dt>{t("Windows 卷状态")}</dt>
            <dd>
              {partition.volumeStatus && catalogText(partition.volumeStatus)}
            </dd>
          </>
        )}
      </dl>
      <FilesystemSupportPanel
        disk={disk}
        partition={partition}
        target={target}
      />
      <Hint>
        {t(
          "数据来自 Windows 存储接口，Windows 状态不是 SMART 检测结果。 初始化、新建、调整、格式化或删除须单独预览和确认，确认后会实际写入磁盘。",
        )}
      </Hint>
    </>
  );
}

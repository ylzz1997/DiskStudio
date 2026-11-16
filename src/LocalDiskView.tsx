import type { CSSProperties, ReactNode } from "react";
import {
  CircleHelp,
  FolderOpen,
  Info,
  LoaderCircle,
  RefreshCcw,
  ShieldCheck,
} from "lucide-react";
import type { Disk, Partition, PartitionTarget } from "./types";
import { formatSize } from "./types";
import { DiskIcon, Empty, Hint } from "./components";
import { PartitionPlatformIcon } from "./PartitionPlatformIcon";
import { FilesystemSupportPanel } from "./FilesystemSupportPanel";

export function partitionTitle(p: Partition) {
  return p.driveLetter ? `${p.name} (${p.driveLetter}:)` : p.name;
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
  const h = disk.hardware!;
  return (
    <header className="disk-header local-header">
      <div className="disk-heading">
        <DiskIcon size={39} />
        <div>
          <div className="local-title">
            <h1>{disk.name}</h1>
            {h.isSystem && <span className="system-tag">系统磁盘</span>}
            <span className="view-badge">
              <ShieldCheck size={13} />
              {managed ? "可管理" : "暂不可操作"}
            </span>
          </div>
          <p className="device-model" title={h.model}>
            {h.model}
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
          {loading ? "正在读取" : "刷新信息"}
        </button>
        <button className="button text-button" onClick={onDetails}>
          <Info size={17} />
          详细信息
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
          <span>总容量</span>
          <strong>{formatSize(disk.size)}</strong>
        </div>
        <div>
          <span>连接 / 分区表</span>
          <strong>
            {h.bus}
            <i />
            {disk.scheme}
          </strong>
        </div>
        <div>
          <span>盘符</span>
          <strong className="drive-letter-list">
            {driveLetters || "未分配盘符"}
          </strong>
        </div>
        <div>
          <span>Windows 状态</span>
          <strong
            className={
              h.health === "正常" && h.offline === false ? "device-healthy" : ""
            }
          >
            <b className="status-dot" />
            {h.offline === true ? "脱机" : h.health}
          </strong>
        </div>
      </div>
      {actions}
      {error && (
        <div className="device-notice error" role="alert">
          读取失败：{error} 当前显示上次读取的信息。
        </div>
      )}
      {warnings.map((w) => (
        <div key={w} className="device-notice" role="status">
          {w}
        </div>
      ))}
      {!h.layoutAvailable ? (
        <Empty
          icon={<CircleHelp size={34} />}
          title="分区信息暂不可用"
          text={h.layoutNote ?? "请稍后重新读取。"}
        />
      ) : disk.partitions.length === 0 && (!target || disk.scheme === "RAW") ? (
        <Empty
          icon={<DiskIcon size={34} />}
          title="Windows 未报告分区"
          text={
            disk.scheme === "RAW"
              ? target?.available
                ? "先初始化为 GPT，再创建分区和文件系统。"
                : "这块磁盘尚未初始化；操作能力及限制见上方工具栏。"
              : "当前没有可显示的分区信息。"
          }
        />
      ) : (
        <>
          <section
            className="layout-section local-layout"
            aria-label="本机磁盘分区布局"
          >
            <div className="section-heading">
              <h2>分区布局</h2>
              <span className="section-caption">
                {disk.partitions.length} 个分区
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
                      title={`未分配 · ${formatSize(size)}`}
                    >
                      <span className="block-label">
                        <strong>未分配</strong>
                        <small>{formatSize(size)}</small>
                      </span>
                    </div>
                  );
                const tiny = p.size / disk.size < 0.012;
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
                    aria-label={`选择分区 ${partitionTitle(p)}，${formatSize(p.size)}`}
                    title={`${partitionTitle(p)} · ${formatSize(p.size)} · ${p.role}`}
                  >
                    <div className="usage-fill" />
                    {!tiny && (
                      <>
                        <span className="block-label">
                          <strong>{partitionTitle(p)}</strong>
                          <small>
                            {formatSize(p.size)} ·{" "}
                            {p.fs === "未知" ? "文件系统未知" : p.fs}
                          </small>
                          {p.available != null && (
                            <em>{formatSize(p.available)} 可用</em>
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
              <div className="small-partitions" aria-label="小分区">
                {small.map((p) => (
                  <button
                    key={p.id}
                    className={`${p.color} ${selectedId === p.id ? "selected" : ""}`}
                    onClick={() => onSelect(p.id)}
                    aria-pressed={selectedId === p.id}
                  >
                    <b />
                    {p.name}
                    <span>{formatSize(p.size)}</span>
                  </button>
                ))}
              </div>
            )}
            <div className="legend physical-legend">
              <span>
                <b className="used-key" />
                已用
              </span>
              <span>
                <b className="available-key" />
                可用
              </span>
              <span>
                <b className="unknown-key" />
                占用未知
              </span>
              {target && (
                <span>
                  <b className="free-key" />
                  未分配
                </span>
              )}
              <em>
                {small.length > 0
                  ? "小分区已放大显示"
                  : "按 Windows 提供的分区信息显示"}
              </em>
            </div>
          </section>
          <div className="context-toolbar local-context">
            <div className="selection-label">
              <span className="selection-dot" />
              <span>
                已选中{" "}
                <strong>{selected ? partitionTitle(selected) : "磁盘"}</strong>
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
                  浏览文件
                </button>
              )}
              <button className="button" onClick={onDetails}>
                <Info size={16} />
                分区信息
              </button>
            </div>
          </div>
          <div className="table-wrap">
            <table className="partition-table local-table">
              <thead>
                <tr>
                  <th>分区 / 盘符</th>
                  <th>文件系统</th>
                  <th>容量</th>
                  <th>已用</th>
                  <th>可用</th>
                  <th>用途</th>
                </tr>
              </thead>
              <tbody>
                {!disk.partitions.length && (
                  <tr>
                    <td colSpan={6} className="muted-value">
                      暂无分区，可在未分配空间中新建。
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
                        <span>{p.name}</span>
                      </span>
                    </td>
                    <td className={p.fs === "未知" ? "muted-value" : ""}>
                      {p.fs}
                    </td>
                    <td>{formatSize(p.size)}</td>
                    <td>{formatSize(p.used)}</td>
                    <td>{formatSize(p.available ?? null)}</td>
                    <td>
                      <span className="partition-role">{p.role}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {disk.partitions.some((p) => p.used === null) && (
            <div className="import-note">
              <CircleHelp size={15} />
              Windows 未提供部分分区的文件系统或占用信息，以“未知”和“—”表示。
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
  const h = disk.hardware!;
  return (
    <>
      <div className="details-hero">
        <DiskIcon size={40} />
        <h3>{partition ? partitionTitle(partition) : disk.name}</h3>
        <span>{partition?.role ?? h.model}</span>
      </div>
      <dl className="detail-list">
        <dt>磁盘编号</dt>
        <dd>{h.number}</dd>
        <dt>设备型号</dt>
        <dd>{h.model}</dd>
        <dt>总容量</dt>
        <dd>{formatSize(disk.size)}</dd>
        <dt>连接类型</dt>
        <dd>{h.bus}</dd>
        <dt>分区表</dt>
        <dd>{disk.scheme}</dd>
        <dt>逻辑扇区</dt>
        <dd>{disk.sectorSize ? `${disk.sectorSize} B` : "未提供"}</dd>
        <dt>Windows 状态</dt>
        <dd>
          {h.health}
          {h.offline === true
            ? " · 脱机"
            : h.offline === false
              ? " · 联机"
              : ""}
        </dd>
        <dt>设备只读属性</dt>
        <dd>
          {h.deviceReadOnly === null
            ? "未提供"
            : h.deviceReadOnly
              ? "是"
              : "否"}
          （系统报告）
        </dd>
        <dt>分区管理</dt>
        <dd>
          {target?.available
            ? "支持操作 · 提交前需确认"
            : (target?.reason ?? "暂不可操作")}
        </dd>
        {target && (
          <>
            <dt>磁盘序列号</dt>
            <dd>{target.serial}</dd>
          </>
        )}
        {partition && (
          <>
            <dt>分区编号</dt>
            <dd>{partition.number}</dd>
            <dt>盘符</dt>
            <dd>
              {partition.driveLetter ? `${partition.driveLetter}:` : "无"}
            </dd>
            <dt>用途</dt>
            <dd>{partition.role}</dd>
            <dt>文件系统</dt>
            <dd>{partition.fs}</dd>
            <dt>分区容量</dt>
            <dd>{formatSize(partition.size)}</dd>
            <dt>卷容量</dt>
            <dd>{formatSize(partition.volumeSize ?? null)}</dd>
            <dt>已用 / 可用</dt>
            <dd>
              {formatSize(partition.used)} /{" "}
              {formatSize(partition.available ?? null)}
            </dd>
            <dt>起始偏移</dt>
            <dd>{partition.start.toLocaleString()} B</dd>
            <dt>4 KiB 对齐</dt>
            <dd>{partition.start % 4096 === 0 ? "已对齐" : "未对齐"}</dd>
            <dt>Windows 卷状态</dt>
            <dd>{partition.volumeStatus}</dd>
          </>
        )}
      </dl>
      <FilesystemSupportPanel disk={disk} partition={partition} target={target} />
      <Hint>
        数据来自 Windows 存储接口，Windows 状态不是 SMART 检测结果。
        初始化、新建、调整、格式化或删除须单独预览和确认，确认后会实际写入磁盘。
      </Hint>
    </>
  );
}

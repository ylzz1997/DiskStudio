import type { CSSProperties } from "react";
import { Info, ShieldCheck } from "lucide-react";
import type { Disk, Partition } from "./types";
import { formatSize } from "./types";
import { DiskIcon, Hint } from "./components";

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
  const selected = disk.partitions.find((p) => p.id === selectedId);
  return (
    <>
      <section className="layout-section" aria-label="镜像分区布局">
        <div className="section-heading">
          <h2>分区布局</h2>
          <button
            className="button text-button image-check-button"
            onClick={onCheck}
            disabled={!disk.tableReport}
          >
            <ShieldCheck size={16} />
            分区表检查
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
              aria-label={`选择分区 ${p.name}，${formatSize(p.size)}`}
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
            未分配
          </span>
          <em>文件系统占用未知</em>
        </div>
      </section>
      {selected && (
        <div className="context-toolbar">
          <div className="selection-label">
            <span className="selection-dot" />
            <span>
              已选中 <strong>{selected.name}</strong>
            </span>
          </div>
          <button className="button" onClick={onDetails}>
            <Info size={16} />
            分区信息
          </button>
        </div>
      )}
      <div className="table-wrap">
        <table className="partition-table">
          <thead>
            <tr>
              <th>分区名称</th>
              <th>文件系统</th>
              <th>容量</th>
              <th>起始偏移</th>
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
        镜像以只读方式打开，仅显示分区表信息，不写回源文件。
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
  return (
    <>
      <div className="details-hero">
        <DiskIcon size={40} />
        <h3>{partition?.name ?? disk.name}</h3>
        <span>{partition?.fs === "—" ? partition.name : partition?.fs}</span>
      </div>
      <dl className="detail-list">
        <dt>磁盘容量</dt>
        <dd>{formatSize(disk.size)}</dd>
        <dt>分区表</dt>
        <dd>{disk.scheme}</dd>
        <dt>逻辑扇区</dt>
        <dd>{disk.sectorSize} B</dd>
        <dt>数据来源</dt>
        <dd>导入镜像 · 只读</dd>
        {disk.imageFormat && (
          <>
            <dt>镜像格式</dt>
            <dd>{disk.imageFormat}</dd>
          </>
        )}
        {disk.imageFileSize !== undefined && (
          <>
            <dt>{(disk.imageFileCount ?? 1) > 1 ? "文件合计" : "文件长度"}</dt>
            <dd>{formatSize(disk.imageFileSize)}</dd>
          </>
        )}
        {(disk.imageFileCount ?? 1) > 1 && (
          <>
            <dt>文件数量</dt>
            <dd>
              {disk.imageFileCount} 个（
              {disk.imageChain ? "含当前镜像与父盘" : "含描述文件"}）
            </dd>
          </>
        )}
        <dt>文件名</dt>
        <dd>{disk.filename}</dd>
        {partition && (
          <>
            <dt>分区容量</dt>
            <dd>{formatSize(partition.size)}</dd>
            <dt>起始偏移</dt>
            <dd>{partition.start.toLocaleString()} B</dd>
            <dt>4 KiB 对齐</dt>
            <dd>{partition.start % 4096 === 0 ? "已对齐" : "未对齐"}</dd>
          </>
        )}
      </dl>
      <Hint>
        仅解析所选镜像文件的分区表，不读取分区内文件，不推测文件系统占用。
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

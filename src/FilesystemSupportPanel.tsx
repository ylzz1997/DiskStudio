import type { Disk, Partition, PartitionTarget } from "./types";
import {
  filesystemSupport,
  selectedFilesystemSupport,
} from "./filesystemSupport";

export function FilesystemSupportPanel({
  disk,
  partition,
  target,
}: {
  disk: Disk;
  partition?: Partition;
  target?: PartitionTarget;
}) {
  const selected = partition
    ? selectedFilesystemSupport(disk, partition, target)
    : null;
  return (
    <details className="filesystem-support">
      <summary>文件系统支持范围</summary>
      {selected && (
        <dl className="detail-list">
          <dt>当前报告的格式</dt>
          <dd>{selected.filesystem}</dd>
          <dt>可格式化为</dt>
          <dd>{selected.format}</dd>
          <dt>调整大小</dt>
          <dd>{selected.resize}</dd>
        </dl>
      )}
      <div
        className="filesystem-support-scroll"
        tabIndex={0}
        role="region"
        aria-label="各格式按操作的支持情况"
      >
        <table>
          <thead>
            <tr>
              <th>文件系统</th>
              <th>浏览与提取</th>
              <th>删除文件恢复</th>
              <th>格式化</th>
              <th>缩放</th>
              <th>按已用空间复制</th>
              <th>清除空闲空间</th>
            </tr>
          </thead>
          <tbody>
            {filesystemSupport.map((row) => (
              <tr key={row.name}>
                <th scope="row">{row.name}</th>
                <td>{row.browse}</td>
                <td>{row.recover}</td>
                <td>{row.format}</td>
                <td>{row.resize}</td>
                <td>{row.allocatedCopy}</td>
                <td>{row.freeWipe}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        表中为引擎支持范围。具体操作仍需检查设备身份、保护状态、文件系统特性与容量；格式化会清除现有内容。删除候选不保证内容完整。
      </p>
      <p>
        逐扇区备份与同容量克隆不依赖文件系统格式；文件签名扫描独立提供。VSS
        热备和 TRIM 目前限普通 NTFS 数据卷。
      </p>
    </details>
  );
}

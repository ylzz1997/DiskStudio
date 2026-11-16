import { useEffect, useState } from "react";
import { FileSearch, FolderOpen, HardDrive, X } from "lucide-react";
import { formatSize } from "./types";

export type ImageBrowseOptions = {
  kind: "disk" | "partition";
  sector: 512 | 4096;
};
export type ImageBrowseMode = "existing" | "allFiles" | "deleted";
export type ImageBrowseInfo = {
  id: string;
  name: string;
  format: string;
  files: string[];
  size: number;
  options: ImageBrowseOptions;
  partitions: { id: number; name: string; start: number; size: number }[];
  warning: string;
  scanId: string | null;
  scanPartition: number | null;
  scanMode: ImageBrowseMode | null;
};
export function ImageBrowserPanel({
  info,
  busy,
  available,
  onOpen,
  onClose,
  onRead,
}: {
  info: ImageBrowseInfo | null;
  busy: boolean;
  available: boolean;
  onOpen: (options: ImageBrowseOptions) => void;
  onClose: () => void;
  onRead: (partition: number, mode: ImageBrowseMode) => void;
}) {
  const [options, setOptions] = useState<ImageBrowseOptions>({
    kind: "disk",
    sector: 512,
  });
  const [partition, setPartition] = useState("");
  const [mode, setMode] = useState<ImageBrowseMode>("existing");
  useEffect(() => {
    setPartition(String(info?.scanPartition ?? info?.partitions[0]?.id ?? ""));
    setMode(info?.scanMode ?? "existing");
  }, [info?.id, info?.scanPartition, info?.scanMode]);
  return (
    <section className="data-panel backup-browser" aria-label="浏览镜像">
      <div className="section-heading">
        <h2>浏览镜像文件</h2>
        {info && (
          <button className="button" disabled={busy} onClick={onClose}>
            <X size={15} />
            关闭镜像
          </button>
        )}
      </div>
      {info && (
        <div className="backup-browser-file">
          <HardDrive size={28} />
          <div>
            <strong title={info.name}>{info.name}</strong>
            <span>
              {info.format} ·{" "}
              {info.options.kind === "disk" ? "整盘镜像" : "分区镜像"} ·{" "}
              {formatSize(info.size)} · {info.options.sector} B 扇区
            </span>
          </div>
        </div>
      )}
      <details className="image-source-settings" open={!info}>
        <summary>
          {info
            ? `打开其他镜像 · 当前 ${info.files.length} 个来源文件`
            : "选择镜像文件"}
        </summary>
        <div className="image-browser-open-row">
          <label>
            RAW 范围
            <select
              aria-label="镜像范围"
              value={options.kind}
              disabled={busy}
              onChange={(e) =>
                setOptions({
                  ...options,
                  kind: e.target.value as ImageBrowseOptions["kind"],
                })
              }
            >
              <option value="disk">整盘镜像（GPT / MBR）</option>
              <option value="partition">分区镜像（文件系统）</option>
            </select>
          </label>
          <label>
            RAW 扇区
            <select
              aria-label="镜像浏览扇区大小"
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
            {info ? "更换镜像…" : "打开镜像…"}
          </button>
        </div>
        {info && <p className="data-footnote">{info.files.join(" · ")}</p>}
      </details>
      {info && (
        <>
          <label className="backup-browser-partition">
            镜像中的分区
            <select
              aria-label="镜像中的分区"
              value={partition}
              disabled={busy || !info.partitions.length}
              onChange={(e) => setPartition(e.target.value)}
            >
              {!info.partitions.length && (
                <option value="">没有可浏览的分区</option>
              )}
              {info.partitions.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} · {formatSize(p.size)}
                </option>
              ))}
            </select>
          </label>
          <label className="backup-browser-partition">
            文件范围
            <select
              aria-label="镜像文件范围"
              value={mode}
              disabled={busy}
              onChange={(e) => setMode(e.target.value as ImageBrowseMode)}
            >
              <option value="existing">现存文件</option>
              <option value="allFiles">现存文件与删除候选</option>
              <option value="deleted">仅删除候选</option>
            </select>
          </label>
          {info.warning && (
            <details className="image-source-settings">
              <summary>镜像检查说明</summary>
              <p className="data-footnote">{info.warning}</p>
            </details>
          )}
          <div className="data-action-row">
            <span>读取后可预览、搜索并提取文件或整个目录。</span>
            <button
              className="button primary"
              disabled={busy || !available || partition === ""}
              onClick={() => onRead(Number(partition), mode)}
            >
              <FileSearch size={16} />
              读取目录
            </button>
          </div>
        </>
      )}
      <details className="image-source-settings">
        <summary>支持范围与读取说明</summary>
        <p className="data-footnote">
          支持 RAW/IMG、VHD/AVHD、VHDX、VDI、VMDK 和独立 Parallels
          文件的已实现容器类型；可明确补选 VHD 父链与 VMDK 分卷。脏 VHDX
          日志、其他差分链和目录包快照暂不支持。
        </p>
        <p className="data-footnote">
          文件系统支持 NTFS、FAT、exFAT，以及干净卸载的 Ext2/3/4
          现存文件。源镜像只读、不挂载；提取到其他磁盘并逐文件核对
          SHA-256。删除候选不保证内容完整。RAW
          分区镜像需手动选择正确范围和扇区。
        </p>
      </details>
    </section>
  );
}

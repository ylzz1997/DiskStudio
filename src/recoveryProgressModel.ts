import type { SignatureKind, SignatureSettings } from "./signatureModel";
import { formatSize } from "./types";
export type RecoveryProgress = {
  id: string;
  source: { target: string; layoutKey: string; partition: number | null };
  engine: "signatures" | "ntfs" | "fat" | "exfat" | "ext";
  mode: "deleted" | "allFiles" | "existing" | "signatures";
  options: {
    kinds: SignatureKind[];
    start: number;
    length: number | null;
  } | null;
  done: number;
  total: number;
  found: number;
  running: boolean;
  canResume: boolean;
  canSave: boolean;
  loaded: boolean;
};
export function progressMatches(
  progress: RecoveryProgress | null,
  target?: string | null,
  layoutKey?: string | null,
  partition: number | null = null,
) {
  return (
    !!progress &&
    progress.source.target === target &&
    progress.source.layoutKey === layoutKey &&
    progress.source.partition === partition
  );
}
export function settingsFromProgress(p: RecoveryProgress): SignatureSettings {
  if (p.engine !== "signatures" || !p.options)
    throw Error("此进度不是签名扫描");
  return {
    kinds: [...p.options.kinds],
    customRange: p.options.start !== 0 || p.options.length !== null,
    start: (p.options.start / 1024 ** 2).toFixed(9),
    end:
      p.options.length === null
        ? ""
        : ((p.options.start + p.options.length) / 1024 ** 2).toFixed(9),
  };
}
export function progressAmount(p: RecoveryProgress) {
  if (p.engine === "fat" || p.engine === "exfat" || p.engine === "ext") {
    return p.total === 0
      ? `已检查 ${p.done.toLocaleString()} 条目录记录`
      : `${p.done.toLocaleString()} / ${p.total.toLocaleString()} 条目录记录`;
  }
  return p.engine === "ntfs"
    ? `${p.done.toLocaleString()} / ${p.total.toLocaleString()} 条文件记录`
    : `${formatSize(p.done)} / ${formatSize(p.total)}`;
}
export function progressMode(p: RecoveryProgress) {
  return {
    deleted: "已删除文件",
    allFiles: "全部文件",
    existing: "现存文件",
    signatures: "文件签名",
  }[p.mode];
}

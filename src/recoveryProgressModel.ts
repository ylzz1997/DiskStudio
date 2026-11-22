import type { SignatureKind, SignatureSettings } from "./signatureModel";
import type { Language } from "./i18n/languages";
import { translate } from "./i18n/translate";
import { formatSize } from "./types";
export type RecoveryProgress = {
  id: string;
  source: { target: string; layoutKey: string; partition: number | null };
  engine: "signatures" | "ntfs" | "fat" | "exfat" | "ext";
  mode: "deleted" | "allFiles" | "existing" | "signatures";
  combined?: boolean;
  options: {
    kinds: SignatureKind[];
    start: number;
    length: number | null;
    freeOnly?: boolean;
    sectorAligned?: boolean;
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
  if ((!p.combined && p.engine !== "signatures") || !p.options)
    throw Error("此进度不是签名扫描");
  return {
    kinds: [...p.options.kinds],
    freeOnly: p.options.freeOnly === true,
    sectorAligned: p.options.sectorAligned === true,
    customRange: p.options.start !== 0 || p.options.length !== null,
    start: (p.options.start / 1024 ** 2).toFixed(9),
    end:
      p.options.length === null
        ? ""
        : ((p.options.start + p.options.length) / 1024 ** 2).toFixed(9),
  };
}
export function progressAmount(
  p: RecoveryProgress,
  language: Language = "zh-CN",
) {
  if (p.engine === "signatures")
    return `${formatSize(p.done)} / ${formatSize(p.total)}`;
  const done = p.done.toLocaleString(language);
  const total = p.total.toLocaleString(language);
  if (p.engine === "fat" || p.engine === "exfat" || p.engine === "ext") {
    return p.total === 0
      ? translate(language, "已检查 {count} 条目录记录", { count: done })
      : translate(language, "{done} / {total} 条目录记录", { done, total });
  }
  return translate(language, "{done} / {total} 条文件记录", { done, total });
}
export function progressMode(
  p: RecoveryProgress,
  language: Language = "zh-CN",
) {
  if (p.combined) return translate(language, "完整恢复");
  const modes = {
    deleted: "已删除文件",
    allFiles: "全部文件",
    existing: "现存文件",
    signatures: "文件签名",
  } as const;
  return translate(language, modes[p.mode]);
}

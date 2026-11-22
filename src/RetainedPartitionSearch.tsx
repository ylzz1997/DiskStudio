import { useState } from "react";
import type { ImageBrowseMode } from "./ImageBrowserPanel";
import type { LostSearchOptions } from "./lostPartitionModel";
import { useLocale } from "./i18n/LanguageProvider";
import { surfaceRange } from "./surfaceModel";
import { formatSize } from "./types";

export type RetainedSearch = {
  id: string;
  state: "running" | "completed" | "canceled" | "failed";
  candidates: {
    id: string;
    filesystem: string;
    offset: number;
    size: number;
    evidence: string;
    warning: string;
    canBrowse: boolean;
  }[];
};

export function RetainedPartitionSearch({
  search, size, sector, busy, expanded, canSearchUnallocated, onSearch, onBrowse,
}: {
  search: RetainedSearch | null;
  size: number;
  sector: number;
  busy: boolean;
  expanded: boolean;
  canSearchUnallocated: boolean;
  onSearch: (options: LostSearchOptions) => void;
  onBrowse: (searchId: string, candidate: string, mode: ImageBrowseMode) => void;
}) {
  const { t, language, catalogText } = useLocale();
  const [start, setStart] = useState("0");
  const [count, setCount] = useState(String(size / sector));
  const [mode, setMode] = useState<ImageBrowseMode>("existing");
  const [open, setOpen] = useState(expanded);
  const [all, setAll] = useState(false);
  let options: LostSearchOptions | undefined;
  try {
    options = all
      ? { scope: "allUnallocated" }
      : surfaceRange(start, count, size, sector);
  } catch {
    // Validation is also enforced by the native command.
  }
  return (
    <details className="image-source-settings" open={open} onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>{t("搜索丢失分区")}</summary>
      <div className="image-browser-open-row">
        {canSearchUnallocated && (
          <label>
            {t("搜索范围")}
            <select value={all ? "all" : "range"} disabled={busy} onChange={(event) => setAll(event.target.value === "all")}>
              <option value="range">{t("指定扇区范围")}</option>
              <option value="all">{t("全部未分配空间")}</option>
            </select>
          </label>
        )}
        {!all && <>
        <label>
          {t("起始 LBA")}
          <input value={start} disabled={busy} inputMode="numeric" onChange={(event) => setStart(event.target.value)} />
        </label>
        <label>
          {t("扇区数量")}
          <input value={count} disabled={busy} inputMode="numeric" onChange={(event) => setCount(event.target.value)} />
        </label>
        </>}
        <button className="button" disabled={busy || !options} onClick={() => options && onSearch(options)}>
          {t(search ? "重新搜索" : "开始搜索")}
        </button>
      </div>
      {search && (
        <div aria-live="polite">
          <p className="lost-note">{t("找到 {count} 个候选", { count: search.candidates.length.toLocaleString(language) })}</p>
          {search.candidates.some((candidate) => candidate.canBrowse) && (
            <label className="backup-browser-partition">
              {t("文件范围")}
              <select value={mode} disabled={busy} onChange={(event) => setMode(event.target.value as ImageBrowseMode)}>
                <option value="existing">{t("现存文件")}</option>
                <option value="allFiles">{t("现存文件与删除候选")}</option>
                <option value="deleted">{t("仅删除候选")}</option>
              </select>
            </label>
          )}
          {search.candidates.map((candidate) => (
            <article className="lost-candidate" key={candidate.id}>
              <div>
                <strong>{candidate.filesystem} · {formatSize(candidate.size)}</strong>
                <small>{t("起始 LBA")} {(candidate.offset / sector).toLocaleString(language)}</small>
                {candidate.evidence && <small>{catalogText(candidate.evidence)}</small>}
                {candidate.warning && <small>{catalogText(candidate.warning)}</small>}
              </div>
              {candidate.canBrowse && (
                <button className="button" disabled={busy || search.state === "running"} onClick={() => onBrowse(search.id, candidate.id, mode)}>
                  {t("读取目录")}
                </button>
              )}
            </article>
          ))}
        </div>
      )}
    </details>
  );
}

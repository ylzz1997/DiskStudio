import { useState } from "react";
import { FileSearch } from "lucide-react";
import { Dialog, FormError } from "./components";
import type { ImageBrowseInfo, ImageBrowseMode } from "./ImageBrowserPanel";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";

export type StorageSpacesView = {
  id: string;
  pool: string;
  members: string[];
  spaces: { id: string; name: string; size: number | null; degraded: boolean }[];
};

export function StorageSpacesOpenDialog({ view, busy, error, onClose, onOpen }: {
  view: StorageSpacesView;
  busy: boolean;
  error: string;
  onClose: () => void;
  onOpen: (space: string, sector: number, allowDegraded: boolean) => void;
}) {
  const { t, catalogText } = useLocale();
  const [choice, setChoice] = useState(() => view.spaces.find((space) => space.size !== null)?.id ?? "");
  const [sector, setSector] = useState(4096);
  const [allowDegraded, setAllowDegraded] = useState(false);
  const selected = view.spaces.find((space) => space.id === choice);
  const openable = selected && selected.size !== null && (!selected.degraded || allowDegraded);
  return (
    <Dialog title={t("浏览存储空间成员镜像")} subtitle={view.pool} onClose={onClose}>
      <p className="data-footnote">{view.members.join(" · ")}</p>
      <label className="backup-browser-partition">
        {t("存储空间")}
        <select value={choice} disabled={busy || !view.spaces.length} onChange={(event) => {
          setChoice(event.currentTarget.value);
          setAllowDegraded(false);
        }}>
          {!choice && <option value="">{t("没有可重组的存储空间")}</option>}
          {view.spaces.map((space) => (
            <option key={space.id} value={space.id} disabled={space.size === null}>
              {space.name} · {space.size === null ? t("无法重组") : formatSize(space.size)}
              {space.degraded ? ` · ${t("可降级读取")}` : ""}
            </option>
          ))}
        </select>
      </label>
      <label className="backup-browser-partition">
        {t("存储空间逻辑扇区大小")}
        <select disabled={busy} value={sector} onChange={(event) => setSector(Number(event.currentTarget.value))}>
          <option value={512}>512 B</option>
          <option value={4096}>4096 B</option>
        </select>
      </label>
      {selected?.degraded && (
        <label className="data-confirm">
          <input type="checkbox" checked={allowDegraded} disabled={busy} onChange={(event) => setAllowDegraded(event.currentTarget.checked)} />
          {t("允许缺少镜像副本时降级读取")}
        </label>
      )}
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>{t("取消")}</button>
        <button className="button primary" disabled={busy || !openable} onClick={() => onOpen(choice, sector, allowDegraded)}>
          {t("打开存储空间")}
        </button>
      </div>
    </Dialog>
  );
}

export function StorageSpacesRecoveryPanel({ info, busy, onClose, onRead }: {
  info: ImageBrowseInfo;
  busy: boolean;
  onClose: () => void;
  onRead: (partition: number, mode: ImageBrowseMode) => void;
}) {
  const { t, catalogText } = useLocale();
  const [partition, setPartition] = useState(String(info.scanPartition ?? info.partitions[0]?.id ?? ""));
  const [mode, setMode] = useState<ImageBrowseMode>(
    info.scanMode === "signatures" ? "existing" : (info.scanMode ?? "existing"),
  );
  return (
    <section className="data-panel backup-browser" aria-label={t("存储空间")}>
      <div className="section-heading">
        <h2>{info.name}</h2>
        <button className="button" disabled={busy} onClick={onClose}>{t("关闭存储空间")}</button>
      </div>
      <p className="data-footnote">{info.files.join(" · ")}</p>
      {info.warning && <p>{catalogText(info.warning)}</p>}
      <label className="backup-browser-partition">
        {t("镜像中的分区")}
        <select disabled={busy || !info.partitions.length} value={partition} onChange={(event) => setPartition(event.currentTarget.value)}>
          {!info.partitions.length && <option value="">{t("没有可浏览的分区")}</option>}
          {info.partitions.map((part) => <option key={part.id} value={part.id}>{part.name} · {formatSize(part.size)}</option>)}
        </select>
      </label>
      <label className="backup-browser-partition">
        {t("文件范围")}
        <select disabled={busy} value={mode} onChange={(event) => setMode(event.currentTarget.value as ImageBrowseMode)}>
          <option value="existing">{t("现存文件")}</option>
          <option value="allFiles">{t("现存文件与删除候选")}</option>
          <option value="deleted">{t("仅删除候选")}</option>
        </select>
      </label>
      <div className="data-action-row">
        <button className="button primary" disabled={busy || partition === ""} onClick={() => onRead(Number(partition), mode)}>
          <FileSearch size={16} />{t("读取目录")}
        </button>
      </div>
    </section>
  );
}

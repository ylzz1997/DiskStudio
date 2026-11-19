import { useState } from "react";
import { Dialog, FormError } from "./components";
import type { ImageBrowseInfo, ImageBrowseMode } from "./ImageBrowserPanel";
import { LvmDegradedOption, LvmVolumes, type LvmView } from "./LvmBrowserDialog";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";

export type ImageLvmInfo = LvmView & {
  scanId: string | null;
  scanMode: ImageBrowseMode | null;
};

export function ImageLvmDialog({
  image,
  view,
  busy,
  error,
  onInspect,
  onOpenRaw,
  onCloseGroup,
  onBrowse,
  onClose,
}: {
  image: ImageBrowseInfo | null;
  view: ImageLvmInfo | null;
  busy: boolean;
  error: string;
  onInspect: (imageId: string, partitions: number[], allowDegradedRaid: boolean) => void;
  onOpenRaw: (allowDegradedRaid: boolean) => void;
  onCloseGroup: () => void;
  onBrowse: (id: string, volume: string, mode: ImageBrowseMode) => void;
  onClose: () => void;
}) {
  const { t, catalogText } = useLocale();
  const [sources, setSources] = useState(false);
  const [selected, setSelected] = useState<number[]>(
    image?.partitions.length === 1 ? [image.partitions[0].id] : [],
  );
  const [mode, setMode] = useState<ImageBrowseMode>(view?.scanMode ?? "existing");
  const [allowDegradedRaid, setAllowDegradedRaid] = useState(view?.allowDegradedRaid ?? false);
  const showSources = sources || !view;
  const inspect = (id: string) => onInspect(id, selected, allowDegradedRaid);
  return (
    <Dialog title={t("浏览镜像中的 LVM2 逻辑卷")} wide onClose={() => !busy && onClose()}>
      {showSources && (
        <>
          <LvmDegradedOption checked={allowDegradedRaid} busy={busy} onChange={setAllowDegradedRaid} />
          {image && (
            <section className="lost-results">
              <h3>{image.name}</h3>
              <p className="lost-note">{t("选择镜像中属于同一 LVM2 卷组的物理卷分区。")}</p>
              <div className="btrfs-member-options">
                {image.partitions.map((part) => (
                  <label key={part.id}>
                    <input
                      type="checkbox"
                      checked={selected.includes(part.id)}
                      disabled={busy}
                      onChange={(event) =>
                        setSelected(
                          event.currentTarget.checked
                            ? [...selected, part.id]
                            : selected.filter((id) => id !== part.id),
                        )
                      }
                    />
                    <span>{part.name} · {formatSize(part.size)}</span>
                  </label>
                ))}
              </div>
              <button className="button" disabled={busy || !selected.length || selected.length > 64}
                onClick={() => inspect(image.id)}>
                {t("读取 LVM2 卷组")}
              </button>
            </section>
          )}
          <div className="data-action-row">
            <span>{t("为独立物理卷镜像一次选齐同一卷组的文件。")}</span>
            <button className="button" disabled={busy} onClick={() => onOpenRaw(allowDegradedRaid)}>
              {t("选择 RAW/IMG 物理卷…")}
            </button>
          </div>
        </>
      )}
      {view && (
        <LvmVolumes
          view={view}
          action={(volume) => (
            <button
              className="button"
              disabled={busy}
              onClick={() => onBrowse(view.id, volume.id, mode)}
            >
              {t("读取目录")}
            </button>
          )}
        >
          <label className="backup-browser-partition">
            {t("文件范围")}
            <select value={mode} disabled={busy}
              onChange={(event) => setMode(event.currentTarget.value as ImageBrowseMode)}>
              <option value="existing">{t("现存文件")}</option>
              <option value="allFiles">{t("现存文件与删除候选")}</option>
              <option value="deleted">{t("仅删除候选")}</option>
            </select>
          </label>
        </LvmVolumes>
      )}
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        {view && !showSources && (
          <button className="button" disabled={busy} onClick={() => setSources(true)}>
            {t("重新选择来源")}
          </button>
        )}
        {view && (
          <button className="button" disabled={busy} onClick={onCloseGroup}>
            {t("关闭镜像卷组")}
          </button>
        )}
        <button className="button" disabled={busy} onClick={onClose}>
          {t("完成")}
        </button>
      </div>
    </Dialog>
  );
}

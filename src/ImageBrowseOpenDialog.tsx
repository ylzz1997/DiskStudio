import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FolderOpen, LoaderCircle, Plus } from "lucide-react";
import { Dialog, FormError } from "./components";
import type { ImageBrowseOptions } from "./ImageBrowserPanel";
import {
  acceptsImageCompanions,
  formatForName,
  type NativeImageGroup,
} from "./imageBrowserSource";
import {
  openImageBrowser,
  ImageSourceConsumedError,
  retainedPrimaryImage,
} from "./nativeImageBrowser";
import { ParallelsSnapshotPicker, useParallelsSnapshots } from "./ParallelsSnapshotPicker";
import { MissingImageFilesError } from "./imageDependencies";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";
export function ImageBrowseOpenDialog({
  initial,
  options,
  onOpened,
  onClose,
  onWorking,
}: {
  initial: NativeImageGroup;
  options: ImageBrowseOptions;
  onOpened: (id: string) => Promise<void>;
  onClose: () => void;
  onWorking: (busy: boolean) => void;
}) {
  const { t, language, catalogText } = useLocale();
  const [group, setGroup] = useState(initial);
  const [settings, setSettings] = useState(options);
  const [independent, setIndependent] = useState(false);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<{
    message: string;
    missing?: string[];
  } | null>(null);
  const [consumed, setConsumed] = useState(false);
  const [snapshot, setSnapshot] = useState<string>();
  const format = formatForName(group.files[0].name);
  const bundle = format === "parallelsBundle";
  const descriptor = useMemo(() => bundle ? retainedPrimaryImage(group) : undefined, [group, bundle]);
  const snapshotList = useParallelsSnapshots(descriptor);
  const snapshotReady = !bundle || !!snapshotList?.list?.snapshots.some(
    (point) => point.id === (snapshot ?? snapshotList.list?.top) && point.supported,
  );
  async function action(f: () => Promise<void>) {
    setWorking(true);
    onWorking(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      if (e instanceof ImageSourceConsumedError) setConsumed(true);
      setError(
        e instanceof MissingImageFilesError
          ? { message: e.message, missing: e.missing }
          : { message: e instanceof Error ? e.message : String(e) },
      );
    } finally {
      setWorking(false);
      onWorking(false);
    }
  }
  const close = () => {
    if (!working)
      void action(async () => {
        await invoke("image_source_discard", { id: group.id }).catch(
          () => undefined,
        );
        onClose();
      });
  };
  return (
    <Dialog
      title={t("打开镜像")}
      subtitle={t("检查容器与分区布局后，再按需读取文件目录")}
      onClose={close}
    >
      <div className="image-source-files">
        {group.files.map((f, i) => (
          <div key={f.id}>
            <strong>{f.name}</strong>
            <span>
              {i === 0 ? t("主文件") : t("父盘 / 分卷")} · {formatSize(f.size)}
            </span>
          </div>
        ))}
      </div>
      <div className="image-source-buttons">
        <button
          className="button"
          disabled={working}
          onClick={() =>
            void action(async () => {
              const next = await invoke<NativeImageGroup | null>(
                "image_source_select",
              );
              if (next) {
                setGroup(next);
                setIndependent(false);
                setConsumed(false);
                setSnapshot(undefined);
              }
            })
          }
        >
          <FolderOpen size={16} />
          {t("更换主文件…")}
        </button>
        <button
          className="button"
          disabled={working || consumed || !acceptsImageCompanions(format)}
          onClick={() =>
            void action(async () =>
              setGroup(
                await invoke<NativeImageGroup>("image_source_add", {
                  id: group.id,
                }),
              ),
            )
          }
        >
          <Plus size={16} />
          {t("添加父盘或分卷…")}
        </button>
      </div>
      {format === "raw" ? (
        <div className="image-browser-open-row">
          <label>
            {t("镜像范围")}
            <select
              aria-label={t("打开 RAW 的范围")}
              disabled={working}
              value={settings.kind}
              onChange={(e) =>
                setSettings({
                  ...settings,
                  kind: e.target.value as ImageBrowseOptions["kind"],
                })
              }
            >
              <option value="disk">{t("整盘镜像（GPT / MBR）")}</option>
              <option value="search">{t("整盘镜像（搜索丢失分区）")}</option>
              <option value="partition">{t("分区镜像（文件系统）")}</option>
            </select>
          </label>
          <label>
            {t("扇区大小")}
            <select
              aria-label={t("打开 RAW 的扇区大小")}
              disabled={working}
              value={settings.sector}
              onChange={(e) =>
                setSettings({
                  ...settings,
                  sector: Number(e.target.value) as 512 | 4096,
                })
              }
            >
              <option value={512}>512 B</option>
              <option value={4096}>4096 B</option>
            </select>
          </label>
        </div>
      ) : (
        <>
          <p className="data-footnote">
            {bundle ? "Parallels HDD" : format.toUpperCase()}{" "}
            {t("容器按整盘打开，容量和逻辑扇区由容器元数据确定。")}
          </p>
          <label className="image-source-check">
            <input
              type="checkbox"
              checked={settings.kind === "search"}
              disabled={working}
              onChange={(event) =>
                setSettings({
                  ...settings,
                  kind: event.target.checked ? "search" : "disk",
                })
              }
            />
            {t("搜索丢失分区")}
          </label>
        </>
      )}
      {snapshotList?.list && (
        <ParallelsSnapshotPicker list={snapshotList.list} id={snapshot} disabled={working || consumed} onChange={(id) => { setSnapshot(id); setError(null); }} />
      )}
      {snapshotList?.error && <FormError message={catalogText(snapshotList.error)} />}
      {bundle && snapshotList?.list && !snapshotReady && <FormError message={catalogText("所选 Parallels 快照需要超过 16 层或 64 个文件。")} />}
      {format === "parallels" && (
        <label className="image-source-check">
          <input
            type="checkbox"
            checked={independent}
            disabled={working}
            onChange={(e) => setIndependent(e.target.checked)}
          />
          {t("我确认这是独立 Parallels 文件，不依赖目录包中的其他快照。")}
        </label>
      )}
      <p className="data-footnote">
        {t(
          "所选文件以禁止共享写入的句柄保持打开，不挂载、不修改来源；父盘和分卷需要明确选择，不按镜像内路径自动查找。使用中的虚拟磁盘可能无法打开，请先关闭使用它的虚拟机。",
        )}
      </p>
      {error && (
        <FormError
          message={
            error.missing
              ? t("{error} 还缺少：{files}", {
                  error: catalogText(error.message),
                  files: new Intl.ListFormat(language).format(error.missing),
                })
              : catalogText(error.message)
          }
        />
      )}
      <div className="dialog-actions">
        <button className="button" disabled={working} onClick={close}>
          {t("取消")}
        </button>
        <button
          className="button primary"
          disabled={
            working || consumed || !snapshotReady || (format === "parallels" && !independent)
          }
          onClick={() =>
            void action(async () => {
              const id = await openImageBrowser(group, settings, independent, snapshot);
              setConsumed(true);
              await onOpened(id);
            })
          }
        >
          {working ? (
            <LoaderCircle className="spinning" size={16} />
          ) : (
            <FolderOpen size={16} />
          )}
          {working ? t("正在检查…") : t("检查并打开")}
        </button>
      </div>
    </Dialog>
  );
}

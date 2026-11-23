import { useState } from "react";
import {
  BackupEncodingFields,
  type BackupCompression,
} from "./BackupEncodingFields";
import type { ImageBrowseInfo } from "./ImageBrowserPanel";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";

type ImageBackupOptions = {
  compression: boolean;
  compressionLevel: BackupCompression;
  encryption: boolean;
  incremental: boolean;
  mode: "allSectors" | "usedSpace";
};

export function ImageBackupDialog({
  image,
  busy,
  error,
  onClose,
  onPrepare,
}: {
  image: ImageBrowseInfo;
  busy: boolean;
  error: string;
  onClose: () => void;
  onPrepare: (partition: number | null, options: ImageBackupOptions) => void;
}) {
  const { t, catalogText } = useLocale();
  const [partition, setPartition] = useState("");
  const [mode, setMode] = useState<ImageBackupOptions["mode"]>("allSectors");
  const [compression, setCompression] = useState<BackupCompression>("balanced");
  const [encryption, setEncryption] = useState(false);
  const [incremental, setIncremental] = useState(false);
  const selected = image.partitions.find(
    (part) => String(part.id) === partition,
  );
  const size = selected?.size ?? image.size;
  const supported = size > 0 && size <= 4 * 1024 ** 4;
  const canMask = image.options.kind !== "search";
  return (
    <Dialog
      title={t("备份镜像内容")}
      subtitle={
        image.parallelsSnapshot
          ? `${image.name} · ${image.parallelsSnapshot.name}`
          : image.name
      }
      onClose={() => {
        if (!busy) onClose();
      }}
      footer={
        <>
          <button className="button" disabled={busy} onClick={onClose}>
            {t("取消")}
          </button>
          <button
            className="button primary"
            disabled={busy || !supported}
            onClick={() =>
              onPrepare(selected?.id ?? null, {
                compression: compression !== "none",
                compressionLevel: compression,
                encryption,
                incremental,
                mode,
              })
            }
          >
            {t("选择保存位置…")}
          </button>
        </>
      }
    >
      <div className="backup-settings backup-restore-settings">
        <label>
          <strong>{t("镜像范围")}</strong>
          <select
            value={partition}
            disabled={busy || image.options.kind !== "disk"}
            onChange={(event) => setPartition(event.target.value)}
          >
            <option value="">
              {t(image.options.kind === "partition" ? "分区镜像" : "整个镜像")}{" "}
              · {formatSize(image.size)}
            </option>
            {image.options.kind === "disk" &&
              image.partitions.map((part) => (
                <option key={part.id} value={part.id}>
                  {part.name} · {formatSize(part.size)}
                </option>
              ))}
          </select>
        </label>
        <label>
          <span>
            <strong>{t("选择数据范围")}</strong>
            {mode === "usedSpace" && (
              <small>
                {t("备份中的空闲区按零保存，不保留这些区域的已删除内容。")}
              </small>
            )}
            {!canMask && (
              <small>{t("按已用空间备份需要通过检查的镜像分区表。")}</small>
            )}
          </span>
          <select
            value={mode}
            disabled={busy || !canMask}
            onChange={(event) =>
              setMode(event.target.value as ImageBackupOptions["mode"])
            }
          >
            <option value="allSectors">{t("逐扇区")}</option>
            <option value="usedSpace">{t("按已用空间")}</option>
          </select>
        </label>
        <BackupEncodingFields
          busy={busy}
          incremental={incremental}
          setIncremental={setIncremental}
          compression={compression}
          setCompression={setCompression}
          encryption={encryption}
          setEncryption={setEncryption}
        />
      </div>
      {!supported && (
        <FormError message={t("备份范围上限为 4 TiB。请缩小备份范围。")} />
      )}
      {error && <FormError message={catalogText(error)} />}
    </Dialog>
  );
}

import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import {
  conversionFormats,
  imageOutputOptions,
  type ImageOutputFormat,
} from "./imageConversion";
import { formatSize } from "./types";

export function DeviceImageExportDialog({
  name,
  size,
  sector,
  partition,
  busy,
  error,
  onClose,
  onPrepare,
}: {
  name: string;
  size: number;
  sector: number;
  partition: boolean;
  busy: boolean;
  error: string;
  onClose: () => void;
  onPrepare: (
    format: ImageOutputFormat,
    mode: "allSectors" | "usedSpace",
  ) => void;
}) {
  const { t, catalogText } = useLocale();
  const [format, setFormat] = useState<ImageOutputFormat>(
    partition ? "raw" : "vhdxDynamic",
  );
  const [mode, setMode] = useState<"allSectors" | "usedSpace">("allSectors");
  function formatError(value: ImageOutputFormat) {
    try {
      imageOutputOptions(size, sector, value);
      return "";
    } catch (error) {
      return (error as Error).message;
    }
  }
  const formats = partition
    ? conversionFormats.filter(({ value }) => value === "raw")
    : conversionFormats;
  const formatIssue = formatError(format);
  return (
    <Dialog
      title={t("导出磁盘镜像")}
      subtitle={name}
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
            disabled={busy || !!formatIssue}
            onClick={() => onPrepare(format, mode)}
          >
            {t("选择保存位置…")}
          </button>
        </>
      }
    >
      <dl className="data-plan">
        <dt>{t("容量")}</dt>
        <dd>
          {formatSize(size)} · {t("{size} B 扇区", { size: sector })}
        </dd>
      </dl>
      <div className="backup-settings backup-restore-settings">
        <label>
          <strong>{t("镜像格式")}</strong>
          <select
            value={format}
            disabled={busy || partition}
            onChange={(event) =>
              setFormat(event.target.value as ImageOutputFormat)
            }
          >
            {formats.map(({ value, label }) => (
              <option key={value} value={value} disabled={!!formatError(value)}>
                {catalogText(label)}
              </option>
            ))}
          </select>
        </label>
        <label>
          <strong>{t("选择数据范围")}</strong>
          <select
            value={mode}
            disabled={busy}
            onChange={(event) =>
              setMode(event.target.value as "allSectors" | "usedSpace")
            }
          >
            <option value="allSectors">{t("逐扇区")}</option>
            <option value="usedSpace">{t("按已用空间")}</option>
          </select>
        </label>
      </div>
      <p className="data-footnote">
        {partition
          ? t(
              "导出文件包含所选分区的内容，重新打开时请选择分区镜像和源逻辑扇区大小。",
            )
          : catalogText(
              formats.find(({ value }) => value === format)?.detail ?? "",
            )}
      </p>
      {mode === "usedSpace" && (
        <p className="data-footnote">
          {t("空闲区按零保存，完成后校验全部逻辑内容。")}
        </p>
      )}
      {(formatIssue || error) && (
        <FormError message={catalogText(formatIssue || error)} />
      )}
    </Dialog>
  );
}

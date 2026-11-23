import { useLocale } from "./i18n/LanguageProvider";

export type BackupCompression = "none" | "fast" | "balanced" | "high";

export function BackupEncodingFields({
  busy,
  incremental,
  setIncremental,
  compression,
  setCompression,
  encryption,
  setEncryption,
}: {
  busy: boolean;
  incremental: boolean;
  setIncremental: (value: boolean) => void;
  compression: BackupCompression;
  setCompression: (value: BackupCompression) => void;
  encryption: boolean;
  setEncryption: (value: boolean) => void;
}) {
  const { t } = useLocale();
  return (
    <>
      <label>
        <span>
          <strong>{t("备份方式")}</strong>
          <small>{t("增量只保存变化的数据块，还原需要完整依赖链")}</small>
        </span>
        <select
          aria-label={t("备份方式")}
          value={incremental ? "incremental" : "full"}
          disabled={busy}
          onChange={(event) =>
            setIncremental(event.target.value === "incremental")
          }
        >
          <option value="full">{t("完整备份")}</option>
          <option value="incremental">{t("增量备份")}</option>
        </select>
      </label>
      <label>
        <span>
          <strong>{t("压缩程度")}</strong>
          <small>{t("压缩使用多个 CPU 线程；高压缩通常需要更长时间")}</small>
        </span>
        <select
          aria-label={t("压缩程度")}
          value={compression}
          disabled={busy}
          onChange={(event) =>
            setCompression(event.target.value as BackupCompression)
          }
        >
          <option value="none">{t("不压缩")}</option>
          <option value="fast">{t("快速")}</option>
          <option value="balanced">{t("均衡")}</option>
          <option value="high">{t("高压缩")}</option>
        </select>
      </label>
      <label>
        <span>
          <strong>{t("密码保护")}</strong>
          <small>{t("开始时设置密码，打开和还原备份时需要使用")}</small>
        </span>
        <input
          type="checkbox"
          checked={encryption}
          disabled={busy}
          onChange={(event) => setEncryption(event.target.checked)}
        />
      </label>
    </>
  );
}

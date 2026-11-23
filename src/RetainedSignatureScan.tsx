import { useEffect, useState } from "react";
import { SignatureOptions } from "./SignatureOptions";
import { defaultSignatureSettings, signatureRequest } from "./signatureModel";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";
import { settingsFromProgress, type RecoveryProgress, type RetainedScanSelection } from "./recoveryProgressModel";

export type RetainedSignatureRequest = RetainedScanSelection & {
  options: ReturnType<typeof signatureRequest>;
};

export function RetainedSignatureScan({ kind, info, whole, disabled, progress, onStart, onLoad }: {
  kind: RetainedSignatureRequest["kind"];
  info: { id: string; size: number; partitions: { id: number; name: string; size: number }[] };
  whole: boolean;
  disabled: boolean;
  progress: (RecoveryProgress & { source: RetainedScanSelection }) | null;
  onStart: (request: RetainedSignatureRequest) => void;
  onLoad: (source: RetainedScanSelection) => void;
}) {
  const { t, catalogText } = useLocale();
  const [scope, setScope] = useState(String(info.partitions[0]?.id ?? "whole"));
  const [settings, setSettings] = useState(defaultSignatureSettings);
  const [error, setError] = useState("");
  const partition = info.partitions.find((part) => String(part.id) === scope);
  const size = scope === "whole" && whole ? info.size : partition?.size;
  const selection = { kind, id: info.id, partition: partition?.id ?? null };

  useEffect(() => {
    if (progress?.options) {
      setScope(String(progress.source.partition ?? "whole"));
      setSettings(settingsFromProgress(progress));
    }
  }, [progress?.id]);

  function start() {
    if (size === undefined) return;
    setError("");
    try {
      const options = signatureRequest({ ...settings, freeOnly: !!partition && settings.freeOnly }, size);
      onStart({ ...selection, options });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  return (
    <details className="image-source-settings">
      <summary>{t("按文件签名扫描")}</summary>
      <label className="backup-browser-partition">
        {catalogText("签名扫描范围")}
        <select value={scope} disabled={disabled} onChange={(event) => {
          setScope(event.target.value);
          setError("");
        }}>
          {whole && <option value="whole">
            {kind === "image" ? t("整盘镜像") : t("整盘备份")} · {formatSize(info.size)}
          </option>}
          {info.partitions.map((part) => <option key={part.id} value={part.id}>
            {part.name} · {formatSize(part.size)}
          </option>)}
        </select>
      </label>
      <SignatureOptions value={settings} onChange={setSettings} disabled={disabled} canScanFree={!!partition} />
      <div className="data-action-row">
        {error && <span role="alert">{catalogText(error)}</span>}
        <button type="button" className="button" disabled={disabled || size === undefined} onClick={() => onLoad(selection)}>
          {t("加载进度")}
        </button>
        <button type="button" className="button primary" disabled={disabled || size === undefined} onClick={start}>
          {t("开始扫描")}
        </button>
      </div>
    </details>
  );
}

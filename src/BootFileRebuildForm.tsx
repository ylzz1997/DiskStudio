import { useState } from "react";
import type { BootInspectionSource, BootVolume } from "./BootInspectionDialog";
import { FormError, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";
import { useBootRepair } from "./useBootRepair";

type Target = { id: string; name: string; offset: number; size: number };
type RetiredLog = { path: string; bytes: number };
type DamagedBcdAction = "" | "recover" | "replace";
type RebuildSummary = {
  bootOffset: number;
  windowsOffset: number;
  machine: number;
  loaderSha256: string;
  files: {
    path: string;
    bytes: number;
    originalSha256: string | null;
    installedSha256: string;
  }[];
  physicalWrites: number;
  newBcd?: {
    loader: string;
    templateSha256: string;
    retiredLogs: RetiredLog[];
  };
  recoveredBcd?: {
    defaultEntry: string;
    firstSequence: number;
    lastSequence: number;
    transactions: number;
    windowsEntries: number;
    retiredLogs: RetiredLog[];
  };
};

export function BootFileRebuildForm({
  source,
  volumes,
  partitions,
  onBusy,
  onChanged,
  reportStale,
}: {
  source: Extract<BootInspectionSource, { kind: "disk" }>["source"];
  volumes: BootVolume[];
  partitions: Target[];
  onBusy: (busy: boolean) => boolean;
  onChanged: () => void;
  reportStale: boolean;
}) {
  const { t, language, catalogText } = useLocale();
  const candidates = volumes.filter(
    (volume) => volume.repairVolume && volume.filesystem === "FAT32",
  );
  const [boot, setBoot] = useState(() => candidates[0]?.repairVolume ?? "");
  const [windows, setWindows] = useState("");
  const [fallback, setFallback] = useState(false);
  const [rebuildBcd, setRebuildBcd] = useState(false);
  const [damagedBcd, setDamagedBcd] = useState<DamagedBcdAction>("");
  const { preview, result, phase, message, error, prepare, apply, discard } =
    useBootRepair<RebuildSummary>(onBusy, t("已取消启动修复。"), onChanged);
  if (!boot || !candidates.length) return null;
  const summary = result?.summary ?? preview?.summary;
  const bcdArtifact = candidates
    .find((candidate) => candidate.repairVolume === boot)
    ?.artifacts.find(
      ({ path }) => path.toLowerCase() === "\\efi\\microsoft\\boot\\bcd",
    );
  const missingBcd = bcdArtifact?.state === "missing";
  const invalidBcd = bcdArtifact?.state === "invalid" ? bcdArtifact.sha256 : null;
  const retiredLogs = summary?.recoveredBcd?.retiredLogs ?? summary?.newBcd?.retiredLogs;

  return (
    <details
      className="boot-repair boot-files-rebuild"
      name="diskstudio-boot-repair"
      onToggle={(event) => {
        if (!event.currentTarget.open && !phase) discard();
      }}
    >
      <summary>{t("恢复 Windows EFI 启动文件")}</summary>
      <Hint>
        {t("从同一脱机磁盘上的 Windows 安装复制经过核对的 EFI 启动管理器。")}
      </Hint>
      <fieldset
        className="boot-repair-fields"
        disabled={reportStale || !!(phase || preview || result)}
      >
        <label>
          {t("EFI 系统分区")}
          <select
            value={boot}
            onChange={(event) => {
              setBoot(event.target.value);
              setRebuildBcd(false);
              setDamagedBcd("");
            }}
          >
            {candidates.map((volume) => (
              <option key={volume.repairVolume} value={volume.repairVolume!}>
                {catalogText(volume.name)} ·{" "}
                {volume.offset.toLocaleString(language)} B
              </option>
            ))}
          </select>
        </label>
        <label>
          {t("Windows 所在分区")}
          <select
            value={windows}
            onChange={(event) => setWindows(event.target.value)}
          >
            <option value="">{t("选择分区")}</option>
            {partitions.map(({ id, name, offset, size }) => (
              <option key={id} value={id}>
                {catalogText(name)} · {formatSize(size)} ·{" "}
                {offset.toLocaleString(language)} B
              </option>
            ))}
          </select>
        </label>
        <label className="checkbox-row">
          <input
            type="checkbox"
            checked={fallback}
            onChange={(event) => setFallback(event.target.checked)}
          />
          {t("同时恢复 EFI 回退启动文件")}
        </label>
        {missingBcd && (
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={rebuildBcd}
              onChange={(event) => setRebuildBcd(event.target.checked)}
            />
            {t("从 Windows 模板新建缺失的 BCD")}
          </label>
        )}
        {invalidBcd && (
          <label>
            {t("损坏的 BCD")}
            <select
              value={damagedBcd}
              onChange={(event) =>
                setDamagedBcd(event.target.value as DamagedBcdAction)
              }
            >
              <option value="">{t("保留原有 BCD")}</option>
              <option value="recover">{t("从现有 BCD 日志恢复")}</option>
              <option value="replace">{t("用 Windows 模板替换损坏的 BCD")}</option>
            </select>
          </label>
        )}
      </fieldset>
      {damagedBcd === "replace" && !result && (
        <Hint>{t("新 BCD 只包含所选 Windows 的一个启动项。原 BCD 和日志会先备份。")}</Hint>
      )}
      {summary && (
        <ul className="boot-artifacts boot-repair-preview">
          {summary.files.map((file) => (
            <li key={file.path}>
              <div>
                <code>{file.path}</code> · {formatSize(file.bytes)}
              </div>
              <small>
                {t("原始文件")} ·{" "}
                {file.originalSha256 ? (
                  <code>{file.originalSha256}</code>
                ) : (
                  t("缺失")
                )}
              </small>
              <small>
                {t("恢复后的文件")} · SHA-256{" "}
                <code>{file.installedSha256}</code>
              </small>
            </li>
          ))}
          {summary.newBcd && (
            <li>
              <small>
                {t("默认启动项")} · <code>{summary.newBcd.loader}</code>
              </small>
              <small>
                {t("BCD 模板")} · SHA-256{" "}
                <code>{summary.newBcd.templateSha256}</code>
              </small>
            </li>
          )}
          {summary.recoveredBcd && (
            <li>
              <small>
                {t("默认启动项")} · <code>{summary.recoveredBcd.defaultEntry}</code>
              </small>
              <small>
                {t("恢复 {count} 个 Windows 启动项", {
                  count: summary.recoveredBcd.windowsEntries.toLocaleString(language),
                })}
              </small>
              <small>
                {t("回放 {count} 条 BCD 日志记录", {
                  count: summary.recoveredBcd.transactions.toLocaleString(language),
                })}{" "}
                · {summary.recoveredBcd.firstSequence} → {summary.recoveredBcd.lastSequence}
              </small>
            </li>
          )}
          {!!retiredLogs?.length && (
            <li>
              <small>{t("备份后从 EFI 分区移除的旧 BCD 日志")}</small>
              {retiredLogs.map((log) => (
                <small key={log.path}>
                  <code>{log.path}</code> · {formatSize(log.bytes)}
                </small>
              ))}
            </li>
          )}
          {preview && (
            <li>
              <strong>
                {t("预计 {count} 次物理块写入", {
                  count: summary.physicalWrites.toLocaleString(language),
                })}
              </strong>
              <small>
                {t("Windows 加载器")} · SHA-256{" "}
                <code>{summary.loaderSha256}</code>
              </small>
            </li>
          )}
        </ul>
      )}
      {preview && (
        <Hint>
          {t("将 EFI 文件备份保存到另一块磁盘。备份通过回读校验后才开始恢复。")}
        </Hint>
      )}
      {result && (
        <div role="status">
          <p>
            {result.summary.recoveredBcd
              ? t("已从日志恢复 BCD，并通过 SHA-256 回读校验。")
              : result.summary.newBcd
                ? t("已创建 BCD，启动文件通过 SHA-256 回读校验。")
                : t("EFI 启动文件已恢复并通过 SHA-256 回读校验。")}
          </p>
          <p>
            {t("备份文件")} · <code>{result.backupPath}</code>
          </p>
        </div>
      )}
      {message && <p role="status">{message}</p>}
      <FormError message={catalogText(error)} />
      {!result && (
        <div className="dialog-actions">
          {preview ? (
            <>
              <button className="button" disabled={!!phase} onClick={discard}>
                {t("取消预览")}
              </button>
              <button
                className="button primary"
                disabled={reportStale || !!phase}
                onClick={() => void apply()}
              >
                {phase === "apply" ? t("正在恢复 EFI 文件…") : t("备份并修复")}
              </button>
            </>
          ) : (
            <button
              className="button"
              disabled={reportStale || !windows || !!phase}
              onClick={() =>
                void prepare("data_preview_boot_files", {
                  source,
                  change: {
                    bootPartition: boot,
                    windowsPartition: windows,
                    rebuildFallback: fallback,
                    rebuildMissingBcd: !!missingBcd && rebuildBcd,
                    damagedBcd: invalidBcd && damagedBcd
                      ? { method: damagedBcd, originalSha256: invalidBcd }
                      : null,
                  },
                })
              }
            >
              {phase === "preview"
                ? t("正在预览 EFI 文件恢复…")
                : t("预览 EFI 文件恢复")}
            </button>
          )}
        </div>
      )}
    </details>
  );
}

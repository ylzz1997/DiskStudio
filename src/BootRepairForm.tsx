import { useState } from "react";
import type {
  BootDevice,
  BootInspectionSource,
  BootVolume,
} from "./BootInspectionDialog";
import { FormError, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";
import { useBootRepair } from "./useBootRepair";

type Target = { id: string; name: string; offset: number; size: number };
type Summary = {
  bootOffset: number;
  windowsOffset: number;
  applicationBefore: BootDevice;
  systemBefore: BootDevice;
  applicationAfter: BootDevice;
  defaultBefore: string | null;
  defaultAfter: string | null;
  beforeSha256: string;
  afterSha256: string;
  loaderSha256: string;
  blocks: number;
};
type Store = NonNullable<BootVolume["store"]>;

function supportedStores(volumes: BootVolume[]) {
  return volumes.flatMap((volume) => {
    const { repairVolume, store } = volume;
    const artifact = volume.artifacts.find(
      (file) =>
        file.path.toLowerCase() === "\\efi\\microsoft\\boot\\bcd" &&
        file.state === "present",
    );
    if (
      !repairVolume ||
      !store ||
      !artifact?.sha256 ||
      volume.filesystem !== "FAT32"
    )
      return [];
    const entries = store.entries.filter((entry) => {
      const binding = volume.references.find(
        (reference) => reference.id === entry.id,
      );
      return (
        (entry.objectType & 0x000f_ffff) === 3 &&
        !!entry.systemRoot &&
        entry.path?.toLowerCase() ===
          `${entry.systemRoot.toLowerCase()}\\system32\\winload.efi` &&
        binding?.application?.device.kind === "gpt" &&
        binding?.system?.device.kind === "gpt" &&
        !entry.inheritedSettings.some(
          ({ field }) =>
            field === "applicationDevice" || field === "systemDevice",
        )
      );
    });
    return entries.length
      ? [{ id: repairVolume, volume, sha256: artifact.sha256, store, entries }]
      : [];
  });
}

function deviceText(device: BootDevice): string {
  return device.kind === "gpt" ? `${device.disk} / ${device.partition}` : "—";
}

function RepairSummary({ summary, store }: { summary: Summary; store: Store }) {
  const { t, language } = useLocale();
  const entryName = (id: string | null) =>
    store.entries.find((entry) => entry.id === id)?.description ?? id ?? "—";
  return (
    <ul className="boot-artifacts boot-repair-preview">
      <li>
        <strong>{t("启动设备")}</strong>
        <code>
          {deviceText(summary.applicationBefore)} →{" "}
          {deviceText(summary.applicationAfter)}
        </code>
      </li>
      <li>
        <strong>{t("系统设备")}</strong>
        <code>
          {deviceText(summary.systemBefore)} →{" "}
          {deviceText(summary.applicationAfter)}
        </code>
      </li>
      {summary.defaultBefore !== summary.defaultAfter && (
        <li>
          <strong>{t("默认启动项")}</strong>
          <span>
            {entryName(summary.defaultBefore)} →{" "}
            {entryName(summary.defaultAfter)}
          </span>
        </li>
      )}
      <li>
        <strong>
          {t("将写入 {count} 个物理块", {
            count: summary.blocks.toLocaleString(language),
          })}
        </strong>
        <small>
          EFI · {t("起始偏移")} {summary.bootOffset.toLocaleString(language)} B
        </small>
        <small>
          Windows · {t("起始偏移")}{" "}
          {summary.windowsOffset.toLocaleString(language)} B
        </small>
        <small>
          {t("原始 BCD")} · SHA-256 <code>{summary.beforeSha256}</code>
        </small>
        <small>
          {t("修复后的 BCD")} · SHA-256 <code>{summary.afterSha256}</code>
        </small>
        <small>
          {t("Windows 加载器")} · SHA-256 <code>{summary.loaderSha256}</code>
        </small>
      </li>
    </ul>
  );
}

export function BootRepairForm({
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
  const stores = supportedStores(volumes);
  const [bootPartition, setBootPartition] = useState(() => stores[0]?.id ?? "");
  const [entryId, setEntryId] = useState("");
  const [windowsPartition, setWindowsPartition] = useState("");
  const [makeDefault, setMakeDefault] = useState(false);
  const { preview, result, phase, message, error, prepare, apply, discard } =
    useBootRepair<Summary>(onBusy, t("已取消 BCD 修复。"), onChanged);

  const selected = stores.find((store) => store.id === bootPartition);
  if (!selected) return null;
  const entry =
    selected.entries.find((entry) => entry.id === entryId) ??
    selected.entries[0];
  const defaultAvailable =
    selected.store.default !== null &&
    selected.store.default !== entry.id &&
    selected.store.displayOrder.includes(entry.id) &&
    !selected.store.manager?.inheritedSettings.some(
      ({ field }) => field === "default",
    );

  return (
    <details
      className="boot-repair"
      name="diskstudio-boot-repair"
      onToggle={(event) => {
        if (!event.currentTarget.open && !phase) discard();
      }}
    >
      <summary>{t("修复 BCD 引用")}</summary>
      <Hint>
        {t("仅修复脱机 GPT 磁盘上现有的 Windows UEFI 启动项。目标盘保持脱机。")}
      </Hint>
      <fieldset
        className="boot-repair-fields"
        disabled={reportStale || phase !== null || preview !== null || result !== null}
      >
        <label>
          {t("EFI 系统分区")}
          <select
            value={selected.id}
            onChange={(event) => {
              setBootPartition(event.target.value);
              setEntryId("");
              setMakeDefault(false);
            }}
          >
            {stores.map(({ id, volume }) => (
              <option key={id} value={id}>
                {catalogText(volume.name)} ·{" "}
                {volume.offset.toLocaleString(language)} B
              </option>
            ))}
          </select>
        </label>
        <label>
          {t("Windows 启动项")}
          <select
            value={entry.id}
            onChange={(event) => {
              setEntryId(event.target.value);
              setMakeDefault(false);
            }}
          >
            {selected.entries.map(({ id, description }) => (
              <option key={id} value={id}>
                {description ? `${description} · ${id}` : id}
              </option>
            ))}
          </select>
        </label>
        <label>
          {t("Windows 所在分区")}
          <select
            value={windowsPartition}
            onChange={(event) => setWindowsPartition(event.target.value)}
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
        {defaultAvailable && (
          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={makeDefault}
              onChange={(event) => setMakeDefault(event.target.checked)}
            />
            {t("设为默认启动项")}
          </label>
        )}
      </fieldset>
      {preview && (
        <>
          <RepairSummary summary={preview.summary} store={selected.store} />
          <Hint>
            {t(
              "请选择另一块磁盘保存原始 BCD。备份通过回读校验后才会开始修复。",
            )}
          </Hint>
        </>
      )}
      {result && (
        <div role="status">
          <p>{t("BCD 修复完成，已通过 SHA-256 回读校验。")}</p>
          <p>
            {t("原始 BCD")} · <code>{result.backupPath}</code>
          </p>
          <p>
            {t("修复后的 BCD")} · SHA-256{" "}
            <code>{result.summary.afterSha256}</code>
          </p>
        </div>
      )}
      {message && <p role="status">{message}</p>}
      <FormError message={catalogText(error)} />
      {!result && (
        <div className="dialog-actions">
          {preview ? (
            <>
              <button
                className="button"
                disabled={phase !== null}
                onClick={discard}
              >
                {t("取消预览")}
              </button>
              <button
                className="button primary"
                disabled={reportStale || phase !== null}
                onClick={() => void apply()}
              >
                {phase === "apply" ? t("正在备份并修复 BCD…") : t("备份并修复")}
              </button>
            </>
          ) : (
            <button
              className="button"
              disabled={reportStale || !windowsPartition || phase !== null}
              onClick={() =>
                void prepare("data_preview_boot_repair", {
                  source,
                  change: {
                    bootPartition: selected.id,
                    storeSha256: selected.sha256,
                    entry: entry.id,
                    windowsPartition,
                    makeDefault: defaultAvailable && makeDefault,
                  },
                })
              }
            >
              {phase === "preview"
                ? t("正在预览 BCD 修复…")
                : t("预览 BCD 修复")}
            </button>
          )}
        </div>
      )}
    </details>
  );
}

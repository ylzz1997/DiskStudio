import { useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { LoaderCircle, ScanLine } from "lucide-react";
import { Dialog, FormError, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { errorText } from "./partitionOutcome";
import { formatSize } from "./types";
import { BootRepairForm } from "./BootRepairForm";
import { BootFileRebuildForm } from "./BootFileRebuildForm";

export type BootInspectionSource =
  | { kind: "image"; id: string }
  | { kind: "backup"; id: string; point: string }
  | {
      kind: "disk";
      source: { target: string; layoutKey: string; partition: null };
    };

type BootEntry = {
  id: string;
  objectType: number;
  description: string | null;
  locale: string | null;
  path: string | null;
  systemRoot: string | null;
  recoverySequence: string[];
  recoveryEnabled: boolean | null;
  inheritedSettings: { field: BootSetting; object: string }[];
  settingIssues: {
    field: BootSetting;
    problem: "invalid" | "conflict";
    object: string;
  }[];
  inheritanceIssue: {
    problem:
      | "malformedList"
      | "missingObject"
      | "incompatibleObject"
      | "cycle"
      | "limit";
    object: string;
  } | null;
};
type BootSetting =
  | "description"
  | "locale"
  | "applicationDevice"
  | "applicationPath"
  | "systemDevice"
  | "systemRoot"
  | "recoverySequence"
  | "recoveryEnabled"
  | "default"
  | "displayOrder"
  | "bootSequence"
  | "toolsOrder"
  | "timeout";
type BootArtifact = {
  path: string;
  state: "present" | "missing" | "invalid" | "unreadable";
  bytes: number | null;
  sha256: string | null;
  machine: number | null;
  detail: string | null;
};
export type BootDevice =
  | { kind: "boot" | "invalid" }
  | { kind: "gpt"; disk: string; partition: string }
  | { kind: "mbr"; signature: number; offset: string }
  | { kind: "unresolved"; deviceType: number };
type BootBinding = {
  device: BootDevice;
  state:
    | "matched"
    | "relative"
    | "outsideSource"
    | "missingPartition"
    | "noDiskIdentity"
    | "unresolved"
    | "invalid";
  offset: number | null;
};
type BootReference = {
  id: string;
  application: BootBinding | null;
  system: BootBinding | null;
  loader: BootArtifact | null;
  systemRoot: BootArtifact | null;
};
export type BootVolume = {
  name: string;
  offset: number;
  size: number;
  mode: "uefi" | "bios";
  filesystem: string | null;
  bootSignature: boolean;
  repairVolume: string | null;
  error: string | null;
  artifacts: BootArtifact[];
  references: BootReference[];
  store: {
    manager: BootEntry | null;
    default: string | null;
    displayOrder: string[];
    bootSequence: string[];
    toolsOrder: string[];
    timeoutSeconds: string | null;
    unresolvedObjects: string[];
    entries: BootEntry[];
  } | null;
};
type BootReport = {
  source: string;
  scheme: string;
  layoutWarning: string;
  mbrCodePresent: boolean | null;
  volumes: BootVolume[];
  repairPartitions: { id: string; name: string; offset: number; size: number }[];
};

function ArtifactDetails({ file }: { file: BootArtifact }) {
  const { t, catalogText } = useLocale();
  const stateLabel = {
    present: t("存在"),
    missing: t("缺失"),
    invalid: t("无效"),
    unreadable: t("读取失败"),
  };
  const machines = {
    0x8664: "x64",
    0x014c: "IA32",
    0xaa64: "ARM64",
    0x01c2: "ARM",
    0x01c4: "ARM",
  } as const;
  return (
    <>
      <div>
        <code>{file.path}</code>
        {file.bytes !== null && <span>{formatSize(file.bytes)}</span>}
        {file.machine !== null && (
          <span>{machines[file.machine as keyof typeof machines]}</span>
        )}
        <strong data-state={file.state}>{stateLabel[file.state]}</strong>
      </div>
      {file.sha256 && (
        <small>
          SHA-256 <code>{file.sha256}</code>
        </small>
      )}
      {file.detail && <FormError message={catalogText(file.detail)} />}
    </>
  );
}

function BindingDetails({
  binding,
  label,
  unavailable,
}: {
  binding: BootBinding | null;
  label: string;
  unavailable: boolean;
}) {
  const { t, language } = useLocale();
  if (!binding)
    return (
      <small>
        {label} · {unavailable ? t("无法核对") : t("设备未设置")}
      </small>
    );
  const labels = {
    matched: t("已匹配来源分区"),
    relative: t("相对于当前启动分区"),
    outsideSource: t("指向其他磁盘"),
    missingPartition: t("来源中缺少引用的分区"),
    noDiskIdentity: t("来源缺少可核对的磁盘标识"),
    unresolved: t("暂不解析此设备类型"),
    invalid: t("无效"),
  };
  const device = binding.device;
  return (
    <>
      <small>
        {label} · {labels[binding.state]}
        {binding.offset !== null && (
          <>
            {" "}
            · {t("起始偏移")} {binding.offset.toLocaleString(language)} B
          </>
        )}
      </small>
      {device.kind === "gpt" && (
        <small>
          GPT <code>{device.disk}</code> / <code>{device.partition}</code>
        </small>
      )}
      {device.kind === "mbr" && (
        <small>
          MBR <code>0x{device.signature.toString(16).padStart(8, "0")}</code> /{" "}
          {BigInt(device.offset).toLocaleString(language)} B
        </small>
      )}
      {device.kind === "unresolved" && (
        <small>
          <code>0x{device.deviceType.toString(16)}</code>
        </small>
      )}
    </>
  );
}

function BcdSettings({ entry }: { entry: BootEntry }) {
  const { t } = useLocale();
  const labels: Record<BootSetting, string> = {
    description: t("描述"),
    locale: t("启动语言"),
    applicationDevice: t("启动设备"),
    applicationPath: t("加载器路径"),
    systemDevice: t("系统设备"),
    systemRoot: t("系统目录"),
    recoverySequence: t("恢复序列"),
    recoveryEnabled: t("自动恢复"),
    default: t("默认启动项"),
    displayOrder: t("启动菜单顺序"),
    bootSequence: t("启动序列"),
    toolsOrder: t("工具菜单顺序"),
    timeout: t("启动菜单等待时间"),
  };
  const problems = {
    malformedList: t("BCD 继承列表无效。"),
    missingObject: t("BCD 继承对象缺失。"),
    incompatibleObject: t("BCD 继承对象类型不匹配。"),
    cycle: t("BCD 继承存在循环。"),
    limit: t("BCD 继承超过检查上限。"),
  };
  return (
    <>
      {entry.locale && (
        <small>
          {labels.locale} · <code>{entry.locale}</code>
        </small>
      )}
      {entry.recoveryEnabled !== null && (
        <small>
          {labels.recoveryEnabled} ·{" "}
          {entry.recoveryEnabled ? t("已启用") : t("已禁用")}
        </small>
      )}
      {entry.recoverySequence.length > 0 && (
        <small>
          {labels.recoverySequence} ·{" "}
          <code>{entry.recoverySequence.join(" → ")}</code>
        </small>
      )}
      {entry.inheritedSettings.length > 0 && (
        <details className="boot-inherited">
          <summary>{t("继承的设置")}</summary>
          {entry.inheritedSettings.map(({ field, object }) => (
            <small key={field}>
              {labels[field]} · <code>{object}</code>
            </small>
          ))}
        </details>
      )}
      {entry.inheritanceIssue && (
        <FormError
          message={`${problems[entry.inheritanceIssue.problem]} ${entry.inheritanceIssue.object}`}
        />
      )}
      {entry.settingIssues.map(({ field, problem, object }) => (
        <FormError
          key={field}
          message={`${labels[field]} · ${problem === "conflict" ? t("继承设置冲突") : t("无效")} · ${object}`}
        />
      ))}
    </>
  );
}

function EntryDetails({
  entry,
  reference,
}: {
  entry: BootEntry;
  reference?: BootReference;
}) {
  const { t } = useLocale();
  const osLoader = (entry.objectType & 0x000f_ffff) === 3;
  const unavailable = (field: BootSetting) =>
    !!entry.inheritanceIssue ||
    entry.settingIssues.some((issue) => issue.field === field);
  return (
    <>
      {(osLoader || entry.path || reference?.application) && (
        <BindingDetails
          binding={reference?.application ?? null}
          label={t("启动设备")}
          unavailable={unavailable("applicationDevice")}
        />
      )}
      {reference?.loader ? (
        <ArtifactDetails file={reference.loader} />
      ) : (
        (entry.path || osLoader) && (
          <small>
            {t("加载器路径")} ·{" "}
            {entry.path ? (
              <code>{entry.path}</code>
            ) : (
              t(unavailable("applicationPath") ? "无法核对" : "缺失")
            )}
          </small>
        )
      )}
      {(osLoader || entry.systemRoot || reference?.system) && (
        <BindingDetails
          binding={reference?.system ?? null}
          label={t("系统设备")}
          unavailable={unavailable("systemDevice")}
        />
      )}
      {reference?.systemRoot ? (
        <ArtifactDetails file={reference.systemRoot} />
      ) : (
        (entry.systemRoot || osLoader) && (
          <small>
            {t("系统目录")} ·{" "}
            {entry.systemRoot ? (
              <code>{entry.systemRoot}</code>
            ) : (
              t(unavailable("systemRoot") ? "无法核对" : "缺失")
            )}
          </small>
        )
      )}
      <BcdSettings entry={entry} />
    </>
  );
}

function Volume({ report }: { report: BootVolume }) {
  const { t, language, catalogText } = useLocale();
  const store = report.store;
  const references = new Map(
    report.references.map((reference) => [reference.id, reference]),
  );
  return (
    <section className="boot-volume">
      <h3>
        {report.mode === "uefi" ? "UEFI" : "Legacy BIOS"} ·{" "}
        {catalogText(report.name)}
      </h3>
      <p className="field-note">
        {report.filesystem ?? t("未知")} · {formatSize(report.size)} ·{" "}
        {t("起始偏移")} {report.offset.toLocaleString(language)} B
      </p>
      {!report.bootSignature && (
        <FormError message={t("缺少分区引导扇区签名。")} />
      )}
      <FormError message={report.error ? catalogText(report.error) : ""} />
      {!!report.artifacts.length && (
        <ul className="boot-artifacts">
          {report.artifacts.map((file) => (
            <li key={file.path}>
              <ArtifactDetails file={file} />
            </li>
          ))}
        </ul>
      )}
      {store && (
        <section className="boot-store">
          <h4>BCD</h4>
          {store.manager ? (
            <div className="boot-entry-manager">
              <p>{store.manager.description ?? "Windows Boot Manager"}</p>
              <EntryDetails
                entry={store.manager}
                reference={references.get(store.manager.id)}
              />
            </div>
          ) : (
            <FormError message={t("BCD 缺少 Windows 启动管理器对象。")} />
          )}
          {store.timeoutSeconds !== null && (
            <p>
              {t("启动菜单等待 {seconds} 秒", {
                seconds: BigInt(store.timeoutSeconds).toLocaleString(language),
              })}
            </p>
          )}
          {store.unresolvedObjects.length > 0 && (
            <FormError
              message={`${t("BCD 引用了缺失的启动项。")} ${store.unresolvedObjects.join(", ")}`}
            />
          )}
          <ul className="boot-artifacts">
            {store.entries.map((entry) => {
              const position = store.displayOrder.indexOf(entry.id);
              const sequence = store.bootSequence.indexOf(entry.id);
              const tool = store.toolsOrder.indexOf(entry.id);
              return (
                <li key={entry.id}>
                  <div>
                    <strong>{entry.description ?? entry.id}</strong>
                    <code>
                      0x{entry.objectType.toString(16).padStart(8, "0")}
                    </code>
                    {entry.id === store.default && (
                      <span>{t("默认启动项")}</span>
                    )}
                    {position >= 0 && (
                      <span>
                        {t("菜单顺序 {position}", { position: position + 1 })}
                      </span>
                    )}
                    {sequence >= 0 && (
                      <span>
                        {t("启动序列 {position}", { position: sequence + 1 })}
                      </span>
                    )}
                    {tool >= 0 && (
                      <span>
                        {t("工具菜单 {position}", { position: tool + 1 })}
                      </span>
                    )}
                  </div>
                  <small>
                    <code>{entry.id}</code>
                  </small>
                  <EntryDetails
                    entry={entry}
                    reference={references.get(entry.id)}
                  />
                </li>
              );
            })}
          </ul>
        </section>
      )}
    </section>
  );
}

export function BootInspectionDialog({
  request,
  label,
  onBusy,
  onClose,
}: {
  request: BootInspectionSource;
  label: string;
  onBusy: (busy: boolean) => void;
  onClose: () => void;
}) {
  const { t, catalogText } = useLocale();
  const [report, setReport] = useState<BootReport | null>(null);
  const [error, setError] = useState("");
  const [reading, setReading] = useState(false);
  const [repairing, setRepairing] = useState(false);
  const [reportStale, setReportStale] = useState(false);
  const inFlight = useRef(false);
  const close = () => {
    if (!inFlight.current) onClose();
  };

  function repairBusy(busy: boolean) {
    if (busy && inFlight.current) return false;
    inFlight.current = busy;
    setRepairing(busy);
    onBusy(busy);
    return true;
  }

  async function inspect() {
    if (inFlight.current) return;
    inFlight.current = true;
    onBusy(true);
    setReading(true);
    setError("");
    setReportStale(false);
    setReport(null);
    try {
      setReport(await invoke<BootReport>("data_inspect_boot", { request }));
    } catch (error) {
      setError(errorText(error));
    } finally {
      inFlight.current = false;
      setReading(false);
      onBusy(false);
    }
  }

  return (
    <Dialog
      title={t("Windows 启动检查")}
      subtitle={report?.source ?? label}
      wide
      onClose={close}
      footer={
        <div className="dialog-actions">
          <button className="button" onClick={close} disabled={reading || repairing}>
            {t("关闭面板")}
          </button>
          <button
            className="button primary"
            onClick={() => void inspect()}
            disabled={reading || repairing}
          >
            {reading ? (
              <LoaderCircle size={16} className="spin" />
            ) : (
              <ScanLine size={16} />
            )}
            {reading ? t("正在检查启动文件…") : t("检查启动文件")}
          </button>
        </div>
      }
    >
      <Hint>
        {t(
          "只读检查启动文件和 BCD。固件启动顺序、签名信任和实际启动需另行验证。",
        )}
      </Hint>
      <FormError message={catalogText(error)} />
      {report && (
        <div className="boot-report">
          {report.layoutWarning && (
            <Hint>{catalogText(report.layoutWarning)}</Hint>
          )}
          {report.scheme === "MBR" && !report.mbrCodePresent && (
            <FormError message={t("MBR 引导代码区域为空。")} />
          )}
          {report.volumes.length === 0 && (
            <FormError
              message={
                report.scheme === "GPT"
                  ? t("未找到 EFI 系统分区。")
                  : t("未找到活动 MBR 主分区。")
              }
            />
          )}
          {request.kind === "disk" && report.repairPartitions?.length > 0 && (
            <>
              <BootRepairForm
                source={request.source}
                volumes={report.volumes}
                partitions={report.repairPartitions}
                onBusy={repairBusy}
                onChanged={() => setReportStale(true)}
                reportStale={reportStale}
              />
              <BootFileRebuildForm
                source={request.source}
                volumes={report.volumes}
                partitions={report.repairPartitions}
                onBusy={repairBusy}
                onChanged={() => setReportStale(true)}
                reportStale={reportStale}
              />
            </>
          )}
          {reportStale && (
            <Hint>{t("请重新检查，读取目前的启动文件和 BCD。")}</Hint>
          )}
          {!reportStale && report.volumes.map((volume) => (
            <Volume key={`${volume.offset}:${volume.mode}`} report={volume} />
          ))}
        </div>
      )}
    </Dialog>
  );
}

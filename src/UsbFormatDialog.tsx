import { Channel, invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import { Dialog, FormError, Hint } from "./components";
import { defaultFormatOptions, formatClusters } from "./formatOptions";
import { useLocale } from "./i18n/LanguageProvider";
import { PartitionBatchProgress } from "./PartitionBatchProgress";
import { advanceQuickProgress, type QuickProgress } from "./quickProgress";
import {
  formatSize,
  MiB,
  type FormatOptions,
  type WindowsFileSystem,
} from "./types";

type Candidate = {
  target: string;
  revision: string;
  name: string;
  number: number;
  serial: string;
  size: number;
  sectorSize: number;
  unpartitioned: boolean;
  partitions: { number: number; size: number; driveLetter: string | null }[];
};
type Format = {
  fileSystem: WindowsFileSystem;
  label: string;
  clusterSize: number;
  options: FormatOptions;
};
type Operation = "initialize" | "create" | "format";
type DevicePlan = {
  number: number;
  name: string;
  serial: string;
  partition: number | null;
  extent: { offset: number; size: number };
  operation: Operation;
  driveLetter: string | null;
  oldFileSystem: string | null;
  oldLabel: string | null;
  clusterSize: number;
};
type Preview = { token: string; format: Format; devices: DevicePlan[] };
type Outcome = {
  devices: {
    device: DevicePlan;
    state: "verified" | "stopped" | "skipped";
    completed: Operation[];
    failure: string | null;
  }[];
};

const operationLabel = {
  initialize: "初始化 GPT 并创建数据分区",
  create: "创建数据分区",
  format: "格式化分区",
} as const;
const completedLabel = {
  initialize: "初始化为 GPT",
  create: "新建分区",
  format: "格式化分区",
} as const;
const stateLabel = {
  verified: "已完成并核对",
  stopped: "已停止，结果待核对",
  skipped: "未执行",
} as const;

export function UsbFormatDialog({
  onClose,
  onBusy,
  onRefresh,
}: {
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const { t, catalogText } = useLocale();
  const [candidates, setCandidates] = useState<Candidate[] | null>(null);
  const [query, setQuery] = useState("");
  const [min, setMin] = useState("");
  const [max, setMax] = useState("");
  const [choices, setChoices] = useState<Record<string, number | null>>({});
  const [format, setFormat] = useState<Format>({
    fileSystem: "exFAT",
    label: "",
    clusterSize: 0,
    options: defaultFormatOptions,
  });
  const [preview, setPreview] = useState<Preview | null>(null);
  const [progress, setProgress] = useState<QuickProgress | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const running = useRef(false);
  const token = useRef<string | null>(null);

  function discard() {
    if (token.current) {
      void invoke("usb_format_discard", { token: token.current }).catch(
        () => undefined,
      );
    }
    token.current = null;
  }
  useEffect(() => discard, []);

  function targetTitle(disk: { number: number; name: string }) {
    return [t("磁盘 {number}", { number: disk.number }), disk.name]
      .filter(Boolean)
      .join(" · ");
  }
  function filterChanged() {
    setChoices({});
  }
  const invalidFilter =
    [min, max].some(
      (value) => !Number.isFinite(Number(value)) || Number(value) < 0,
    ) || Boolean(min && max && Number(min) > Number(max));
  const visible = (candidates ?? []).filter(
    (disk) =>
      !invalidFilter &&
      disk.size >= Number(min) * MiB &&
      (!max || disk.size <= Number(max) * MiB) &&
      [disk.name, disk.serial, String(disk.number)].some((value) =>
        value.toLowerCase().includes(query.trim().toLowerCase()),
      ),
  );
  const selected = visible.filter((disk) =>
    Object.hasOwn(choices, disk.target),
  );
  const clusterChoices = formatClusters[format.fileSystem].filter(
    (bytes) =>
      selected.every((disk) => bytes >= disk.sectorSize) &&
      (!format.options.ntfsCompression || bytes <= 4096),
  );
  const invalid =
    selected.length === 0 ||
    selected.length > 32 ||
    selected.some(
      (disk) => !disk.unpartitioned && choices[disk.target] === null,
    ) ||
    (format.clusterSize !== 0 && !clusterChoices.includes(format.clusterSize));

  async function run(work: () => Promise<void>) {
    if (running.current) return;
    running.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    try {
      await work();
    } catch (cause) {
      setError(String(cause));
    } finally {
      running.current = false;
      onBusy(false);
      setBusy(false);
    }
  }

  async function read() {
    discard();
    setCandidates(null);
    setChoices({});
    setCandidates(await invoke<Candidate[]>("usb_format_candidates"));
  }
  async function prepare() {
    if (invalid) return;
    const next = await invoke<Preview>("usb_format_prepare", {
      request: {
        devices: selected.map((disk) => ({
          target: disk.target,
          revision: disk.revision,
          partition: choices[disk.target],
        })),
        format,
      },
    });
    token.current = next.token;
    setAcknowledged(false);
    setPreview(next);
  }
  async function execute() {
    if (!token.current || !preview || !acknowledged) return;
    const confirmed = token.current;
    token.current = null;
    const initial: QuickProgress = {
      phase: "checking",
      completed: 0,
      total: preview.devices.length,
      current: null,
    };
    const channel = new Channel<QuickProgress>();
    channel.onmessage = (event) =>
      setProgress((previous) =>
        previous ? advanceQuickProgress(previous, event) : previous,
      );
    setProgress(initial);
    setOutcome(null);
    try {
      setOutcome(
        await invoke<Outcome>("usb_format_execute", {
          token: confirmed,
          progress: channel,
        }),
      );
    } finally {
      channel.onmessage = () => {};
      setProgress(null);
      setAcknowledged(false);
      setPreview(null);
      setCandidates(null);
      setChoices({});
      await onRefresh();
    }
  }

  return (
    <Dialog
      title={t("批量格式化 USB 磁盘")}
      wide
      onClose={() => {
        if (!running.current) onClose();
      }}
    >
      <FormError message={catalogText(error)} />
      {outcome ? (
        <>
          <ol className="quick-steps">
            {outcome.devices.map(({ device, state, completed, failure }) => (
              <li key={device.serial} data-state={state}>
                <strong>
                  {targetTitle(device)} · {device.serial}
                </strong>
                <div>{t(stateLabel[state])}</div>
                {completed.length > 0 && (
                  <div>
                    {completed
                      .map((step) => t(completedLabel[step]))
                      .join(" → ")}
                  </div>
                )}
                {failure && <p>{catalogText(failure)}</p>}
              </li>
            ))}
          </ol>
          <div className="dialog-actions">
            <button
              className="button"
              onClick={() => {
                setOutcome(null);
                void run(read);
              }}
              disabled={busy}
            >
              {t("重新读取")}
            </button>
          </div>
        </>
      ) : preview ? (
        <>
          {progress ? (
            <PartitionBatchProgress
              labels={preview.devices.map(targetTitle)}
              progress={progress}
              note={t("请保持应用和计算机运行。执行中的操作不能取消。")}
            />
          ) : (
            <>
              <Hint>
                {t(
                  "本批将清除所选目标分区的文件。遇到错误或无法核对结果时会停止，已完成的操作会保留。",
                )}
              </Hint>
              {preview.devices.some(
                (device) => device.operation !== "format",
              ) && <p>{t("初始化和新建分区会写入磁盘，可能覆盖原有数据。")}</p>}
              <p>
                {preview.format.fileSystem} ·{" "}
                {t(
                  preview.format.options.mode === "full"
                    ? "完整格式化"
                    : "快速格式化",
                )}
                {preview.format.label && ` · ${preview.format.label}`}
              </p>
              {preview.format.fileSystem === "NTFS" && (
                <dl className="partition-plan-details">
                  <dt>{t("新文件默认启用压缩")}</dt>
                  <dd>
                    {t(
                      preview.format.options.ntfsCompression ? "开启" : "关闭",
                    )}
                  </dd>
                  <dt>{t("卷的 8.3 短文件名")}</dt>
                  <dd>
                    {t(preview.format.options.ntfsShortNames ? "开启" : "关闭")}
                  </dd>
                </dl>
              )}
              <ol className="quick-steps">
                {preview.devices.map((disk) => (
                  <li key={disk.serial} className="usb-device">
                    <strong>
                      {targetTitle(disk)} · {disk.serial}
                    </strong>
                    <div>
                      {t(operationLabel[disk.operation])}
                      {disk.partition !== null &&
                        ` · ${t("分区 {number}", { number: disk.partition })}`}
                      {disk.driveLetter && ` · ${disk.driveLetter}:`}
                    </div>
                    <div>
                      {formatSize(disk.extent.size)} ·{" "}
                      {t("起点 {start} MiB", {
                        start: disk.extent.offset / MiB,
                      })}{" "}
                      ·{" "}
                      {disk.clusterSize
                        ? `${disk.clusterSize} B`
                        : t("默认簇大小")}
                    </div>
                    {(disk.oldLabel || disk.oldFileSystem) && (
                      <div>
                        {[disk.oldLabel, disk.oldFileSystem]
                          .filter(Boolean)
                          .join(" · ")}
                      </div>
                    )}
                  </li>
                ))}
              </ol>
              <label className="partition-ack">
                <input
                  type="checkbox"
                  checked={acknowledged}
                  onChange={(event) =>
                    setAcknowledged(event.currentTarget.checked)
                  }
                />
                {t("我已核对本批每块磁盘的序列号和目标分区。")}
              </label>
              <div className="dialog-actions">
                <button
                  className="button"
                  disabled={busy}
                  onClick={() => {
                    discard();
                    setPreview(null);
                    setAcknowledged(false);
                  }}
                >
                  {t("取消预览")}
                </button>
                <button
                  className="button primary"
                  disabled={!acknowledged || busy}
                  onClick={() => void run(execute)}
                >
                  {t("确认执行")}
                </button>
              </div>
            </>
          )}
        </>
      ) : (
        <div className="editor-form usb-format-form">
          <Hint>
            {t("支持普通 GPT 数据分区及 Windows 报告为无分区的 USB 磁盘。")}
          </Hint>
          <div className="partition-action-buttons">
            <button
              className="button"
              disabled={busy}
              onClick={() => void run(read)}
            >
              {t(candidates ? "重新读取" : "读取 USB 磁盘")}
            </button>
          </div>
          {candidates?.length === 0 && (
            <p role="status">{t("未找到可操作的 USB 磁盘。")}</p>
          )}
          {candidates && candidates.length > 0 && (
            <>
              <div className="usb-format-fields">
                <label>
                  {t("筛选型号或序列号")}
                  <input
                    type="search"
                    value={query}
                    onChange={(event) => {
                      filterChanged();
                      setQuery(event.currentTarget.value);
                    }}
                  />
                </label>
                <label>
                  {t("最小容量 MiB")}
                  <input
                    type="number"
                    min={0}
                    value={min}
                    onChange={(event) => {
                      filterChanged();
                      setMin(event.currentTarget.value);
                    }}
                  />
                </label>
                <label>
                  {t("最大容量 MiB")}
                  <input
                    type="number"
                    min={0}
                    value={max}
                    onChange={(event) => {
                      filterChanged();
                      setMax(event.currentTarget.value);
                    }}
                  />
                </label>
              </div>
              {!visible.length && (
                <p role="status">{t("没有符合筛选条件的 USB 磁盘。")}</p>
              )}
              {visible.map((disk) => {
                const checked = Object.hasOwn(choices, disk.target);
                return (
                  <div key={disk.target} className="usb-device">
                    <label className="usb-device-selection">
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={(event) => {
                          const chosen = event.currentTarget.checked;
                          setChoices((previous) => {
                            const next = { ...previous };
                            if (chosen) {
                              next[disk.target] =
                                disk.partitions.length === 1
                                  ? disk.partitions[0].number
                                  : null;
                            } else {
                              delete next[disk.target];
                            }
                            return next;
                          });
                        }}
                      />
                      <span>
                        {targetTitle(disk)} · {disk.serial} ·{" "}
                        {formatSize(disk.size)}
                      </span>
                    </label>
                    {checked && disk.unpartitioned && (
                      <p>{t("创建数据分区")}</p>
                    )}
                    {checked && !disk.unpartitioned && (
                      <label>
                        {t("选择分区")}
                        <select
                          value={choices[disk.target] ?? ""}
                          onChange={(event) => {
                            const number =
                              Number(event.currentTarget.value) || null;
                            setChoices((previous) => ({
                              ...previous,
                              [disk.target]: number,
                            }));
                          }}
                        >
                          <option value="">{t("选择分区")}</option>
                          {disk.partitions.map((part) => (
                            <option key={part.number} value={part.number}>
                              {t("分区 {number}", { number: part.number })} ·{" "}
                              {formatSize(part.size)}
                              {part.driveLetter && ` · ${part.driveLetter}:`}
                            </option>
                          ))}
                        </select>
                      </label>
                    )}
                  </div>
                );
              })}
              {selected.length > 0 && (
                <>
                  <div className="usb-format-fields">
                    <label>
                      {t("文件系统")}
                      <select
                        value={format.fileSystem}
                        onChange={(event) => {
                          setFormat({
                            ...format,
                            fileSystem: event.currentTarget
                              .value as WindowsFileSystem,
                            clusterSize: 0,
                            options: {
                              ...defaultFormatOptions,
                              mode: format.options.mode,
                            },
                          });
                        }}
                      >
                        {Object.keys(formatClusters).map((fs) => (
                          <option key={fs}>{fs}</option>
                        ))}
                      </select>
                    </label>
                    <label>
                      {t("卷标")}
                      <input
                        value={format.label}
                        onChange={(event) =>
                          setFormat({
                            ...format,
                            label: event.currentTarget.value,
                          })
                        }
                      />
                    </label>
                    <label>
                      {t("簇大小")}
                      <select
                        value={format.clusterSize}
                        onChange={(event) =>
                          setFormat({
                            ...format,
                            clusterSize: Number(event.currentTarget.value),
                          })
                        }
                      >
                        <option value={0}>{t("默认")}</option>
                        {clusterChoices.map((cluster) => (
                          <option key={cluster} value={cluster}>
                            {cluster} B
                          </option>
                        ))}
                      </select>
                    </label>
                    <label>
                      {t("格式化方式")}
                      <select
                        value={format.options.mode}
                        onChange={(event) => {
                          setFormat({
                            ...format,
                            options: {
                              ...format.options,
                              mode: event.currentTarget
                                .value as FormatOptions["mode"],
                            },
                          });
                        }}
                      >
                        <option value="quick">{t("快速格式化")}</option>
                        <option value="full">{t("完整格式化")}</option>
                      </select>
                    </label>
                  </div>
                  {format.fileSystem === "NTFS" && (
                    <div className="partition-action-buttons">
                      <label className="usb-device-selection">
                        <input
                          type="checkbox"
                          disabled={format.clusterSize > 4096}
                          checked={format.options.ntfsCompression}
                          onChange={(event) => {
                            setFormat({
                              ...format,
                              options: {
                                ...format.options,
                                ntfsCompression: event.currentTarget.checked,
                              },
                            });
                          }}
                        />
                        {t("新文件默认启用压缩")}
                      </label>
                      <label className="usb-device-selection">
                        <input
                          type="checkbox"
                          checked={format.options.ntfsShortNames}
                          onChange={(event) => {
                            setFormat({
                              ...format,
                              options: {
                                ...format.options,
                                ntfsShortNames: event.currentTarget.checked,
                              },
                            });
                          }}
                        />
                        {t("启用卷的 8.3 短文件名")}
                      </label>
                    </div>
                  )}
                </>
              )}
              <div className="dialog-actions">
                <button
                  className="button primary"
                  disabled={busy || invalid}
                  onClick={() => void run(prepare)}
                >
                  {t("预览操作")}
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </Dialog>
  );
}

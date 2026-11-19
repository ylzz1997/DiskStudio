import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  CheckCircle2,
  FileInput,
  LoaderCircle,
} from "lucide-react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle } from "./LocalDiskView";
import type { Disk } from "./types";

interface Entry {
  slot: number;
  guid: string;
  kind: string;
  offset: number;
  size: number;
  attributes: number;
  name: number[];
}
interface Preview {
  recovery: boolean;
  token: string | null;
  target: string;
  name: string;
  fileSha256: string;
  partitions: number;
  changedBytes: number;
  writeBytes: number;
  ranges: number;
  includesBoot: boolean;
  changes: { guid: string; before: Entry | null; after: Entry | null }[];
  warnings: string[];
}
interface Outcome {
  message: string;
  verification: { writtenBytes: number; ranges: number; sha256: string };
}
export function TableRestoreDialog({
  disk,
  onClose,
  onBack,
  onBusy,
  onRefresh,
  recovery,
}: {
  disk: Disk;
  onClose: () => void;
  onBack: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
  recovery?: { id: string; candidates: string[] };
}) {
  const { t, language, catalogText } = useLocale();
  const size = (bytes: number) =>
    `${(bytes / 2 ** 20).toLocaleString(language, { maximumFractionDigits: 3 })} MiB`;
  const range = (entry: Entry | null) =>
    entry
      ? t("条目 {slot} · 起始 {start} · 容量 {size}", {
          slot: entry.slot.toLocaleString(language),
          start: size(entry.offset),
          size: size(entry.size),
        })
      : t("不存在");
  const [boot, setBoot] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [preview, setPreview] = useState<Preview | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const inFlight = useRef(false);
  const token = useRef<string | null>(null);
  function discard() {
    if (token.current)
      void invoke("table_restore_discard", { token: token.current }).catch(
        () => {},
      );
    token.current = null;
  }
  useEffect(() => () => discard(), []);
  async function run(execute: boolean) {
    if (inFlight.current || (execute && !token.current)) return;
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    setOutcome(null);
    try {
      if (execute) {
        const once = token.current;
        token.current = null;
        setPreview(null);
        setOutcome(
          await invoke<Outcome>("table_restore_execute", { token: once }),
        );
      } else {
        discard();
        setPreview(null);
        const result = recovery
          ? await invoke<Preview>("lost_partition_prepare_recovery", {
              request: recovery,
            })
          : await invoke<Preview | null>("table_restore_prepare", {
              request: {
                source: {
                  target: disk.hardware?.identity,
                  layoutKey: disk.hardware?.layoutKey,
                  partition: null,
                },
                includesBoot: boot,
              },
            });
        if (recovery && !result?.recovery)
          throw new Error(t("候选恢复预览类型不匹配，请重新搜索。"));
        token.current = result?.token ?? null;
        setPreview(result);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      // A failed write can still have changed the actual disk state.
      if (execute) await onRefresh().catch(() => {});
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }
  return (
    <Dialog
      title={recovery ? t("恢复找到的分区") : t("恢复分区表")}
      subtitle={`${diskTitle(disk, t)} · ${disk.hardware?.model ?? ""}`}
      wide
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
    >
      <p className="table-backup-intro">
        {recovery
          ? t(
              "根据所选候选恢复 GPT 分区记录。先核对文件系统边界和现有布局，预览前后均不会写入或让磁盘脱机；确认后执行。",
            )
          : t(
              "从 .dspt 备份恢复这块磁盘的 GPT 布局。选择文件后先检查差异，确认前不会写入或让磁盘脱机。",
            )}
      </p>
      {!recovery && (
        <label className="table-backup-option">
          <input
            type="checkbox"
            checked={boot}
            disabled={busy}
            onChange={(e) => {
              discard();
              setPreview(null);
              setBoot(e.target.checked);
            }}
          />
          <span>
            <strong>{t("同时恢复分区的首扇区")}</strong>
            <small>
              {t(
                "仅适用于备份中包含首扇区的普通数据分区；不会改写受保护分区。",
              )}
            </small>
          </span>
        </label>
      )}
      <div className="table-backup-actions">
        <button
          className="button"
          disabled={busy}
          onClick={() => {
            discard();
            onBack();
          }}
        >
          {recovery ? t("返回搜索") : t("返回备份")}
        </button>
        <button
          className="button primary"
          disabled={busy}
          onClick={() => void run(false)}
        >
          <FileInput size={16} />
          {recovery ? t("预览恢复操作") : t("选择备份并预览…")}
        </button>
      </div>
      {busy && (
        <p className="table-backup-progress" role="status">
          <LoaderCircle className="spin" size={16} />
          {t("正在核对恢复来源、磁盘和卷状态…")}
        </p>
      )}
      <FormError message={catalogText(error)} />
      {preview && (
        <section className="table-backup-result" aria-label={t("恢复预览")}>
          <h3>
            {preview.token
              ? t("请核对恢复目标与变化")
              : t("当前内容与备份一致")}
          </h3>
          <p>
            <strong>{preview.target}</strong>
            <br />
            {recovery
              ? t("恢复 {count} 个找到的分区记录", {
                  count: preview.changes.length.toLocaleString(language),
                })
              : preview.name}
          </p>
          <dl>
            <div>
              <dt>{t("恢复后分区")}</dt>
              <dd>{preview.partitions.toLocaleString(language)}</dd>
            </div>
            <div>
              <dt>{t("改变的字节")}</dt>
              <dd>{preview.changedBytes.toLocaleString(language)} B</dd>
            </div>
            <div>
              <dt>{t("写入范围")}</dt>
              <dd>
                {t("{count} 处 · {bytes} B", {
                  count: preview.ranges.toLocaleString(language),
                  bytes: preview.writeBytes.toLocaleString(language),
                })}
              </dd>
            </div>
          </dl>
          {preview.changes.map((c) => (
            <div className="table-restore-change" key={c.guid}>
              <strong>
                {!c.before
                  ? t("恢复分区条目")
                  : !c.after
                    ? t("移除分区条目")
                    : t("更改分区条目")}
              </strong>
              <span>
                {t("当前")} · {range(c.before)}
              </span>
              <span>
                {t(recovery ? "恢复后" : "备份")} · {range(c.after)}
              </span>
              <details>
                <summary>{t("身份和属性")}</summary>
                <code>{c.guid}</code>
                <p>
                  {t("类型")} · {c.before?.kind ?? "—"} → {c.after?.kind ?? "—"}
                  <br />
                  {t("属性")} · {c.before?.attributes ?? "—"} →{" "}
                  {c.after?.attributes ?? "—"}
                </p>
              </details>
            </div>
          ))}
          {!!preview.warnings.length && (
            <details>
              <summary>{t("当前分区表检查提示")}</summary>
              <ul>
                {preview.warnings.map((w, i) => (
                  <li key={i}>{catalogText(w)}</li>
                ))}
              </ul>
            </details>
          )}
          <details>
            <summary>
              {recovery ? t("分区表计划 SHA-256") : t("备份文件 SHA-256")}
            </summary>
            <p className="table-backup-digest">
              <code>{preview.fileSha256}</code>
            </p>
          </details>
          {preview.token ? (
            <>
              <p className="table-backup-warning">
                <AlertTriangle size={17} />
                <span>
                  {recovery
                    ? t(
                        "确认后向主备 GPT 添加所选记录，保留现有分区。新增记录使用新的 GUID 和文件系统所需范围，可能不等于原分区精确末端。",
                      )
                    : t("恢复会替换分区表，可能使当前文件无法访问。")}{" "}
                  {t(
                    "确认后将锁定全部同盘卷并使整盘脱机。逐段写入并回读校验，成功或失败均保持脱机；不会自动重试或回滚。确认有效期 5 分钟。",
                  )}
                </span>
              </p>
              <button
                className="button danger"
                disabled={busy}
                onClick={() => void run(true)}
              >
                {recovery ? t("确认恢复所选分区") : t("确认恢复分区表")}
              </button>
            </>
          ) : (
            <p>{t("无需写入，磁盘状态保持不变。")}</p>
          )}
        </section>
      )}
      {outcome && (
        <section className="table-backup-result" role="status">
          <h3>
            <CheckCircle2 size={19} />
            {t("恢复完成")}
          </h3>
          <p>{catalogText(outcome.message)}</p>
          <p>
            {t("已回读校验 {count} 个范围、{bytes} B。", {
              count: outcome.verification.ranges.toLocaleString(language),
              bytes: outcome.verification.writtenBytes.toLocaleString(language),
            })}
          </p>
          <details>
            <summary>{t("写入范围摘要")}</summary>
            <p className="table-backup-digest">
              <code>{outcome.verification.sha256}</code>
            </p>
          </details>
        </section>
      )}
      <p className="table-backup-note">
        {recovery
          ? t(
              "当前恢复要求主备 GPT 完整一致。只添加无重叠的普通数据分区记录，不改变已有条目；不会格式化或修复文件系统。MBR、损坏 GPT 和重叠候选重建尚不支持。",
            )
          : t(
              "当前支持恢复到原磁盘、相同容量及扇区大小的常规 GPT，保留已有系统和保留分区。需要至少一份有效的当前 GPT；MBR/EBR 恢复、主备 GPT 同时损坏和迁移到另一块磁盘尚不支持。",
            )}
      </p>
    </Dialog>
  );
}

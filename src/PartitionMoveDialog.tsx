import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { LoaderCircle } from "lucide-react";
import { Dialog, FormError, Hint } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { formatSize, MiB, type Disk, type Partition } from "./types";

interface Preview {
  token: string;
  target: string;
  sourceOffset: number;
  destinationOffset: number;
  size: number;
  overlap: boolean;
  journalName: string;
  resuming: boolean;
  copiedBytes: number;
  completed: boolean;
}

interface MoveJob {
  id: string;
  title: string;
  state: string;
  phase: string;
  done: number;
  total: number;
}

export function PartitionMoveDialog({
  disk,
  partition,
  onClose,
  onBusy,
  onRefresh,
}: {
  disk: Disk;
  partition?: Partition;
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  onRefresh: () => Promise<void>;
}) {
  const { t, language, catalogText } = useLocale();
  const displayDisk = diskTitle(disk, t);
  const displayOffset = (bytes: number) =>
    (bytes / MiB).toLocaleString(language, { maximumFractionDigits: 20 });
  const resuming = partition == null;
  const [destination, setDestination] = useState(
    partition ? String(partition.start / MiB) : "",
  );
  const [preview, setPreview] = useState<Preview | null>(null);
  const [job, setJob] = useState<MoveJob | null>(null);
  const [busy, setBusy] = useState(false);
  const [executing, setExecuting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<{ message: string; sha256: string }>();
  const token = useRef<string | null>(null);
  const inFlight = useRef(false);

  function releasePreview() {
    const pending = token.current;
    token.current = null;
    if (pending) {
      void invoke("partition_move_discard", { token: pending }).catch(() => {});
    }
  }

  useEffect(() => () => releasePreview(), []);
  useEffect(() => {
    if (!executing) return;
    let active = true;
    let timer: number;
    async function poll() {
      try {
        const next = await invoke<MoveJob | null>("data_status");
        if (active && next?.title === "移动分区" && next.state === "running") {
          setJob(next);
        }
      } catch {
        // Progress polling is independent of the execution result.
      }
      if (active) timer = window.setTimeout(() => void poll(), 300);
    }
    void poll();
    return () => {
      active = false;
      window.clearTimeout(timer);
    };
  }, [executing]);

  async function run() {
    if (inFlight.current || submitted) return;
    const identity = disk.hardware?.identity;
    const layoutKey = disk.hardware?.layoutKey;
    if (!identity || !layoutKey) {
      setError(t("磁盘身份或布局不完整，请刷新后重选。"));
      return;
    }
    const once = token.current;
    inFlight.current = true;
    setBusy(true);
    onBusy(true);
    setError("");
    try {
      if (once) {
        token.current = null;
        setSubmitted(true);
        setExecuting(true);
        setResult(await invoke("partition_move_execute", { token: once }));
      } else {
        const source = {
          target: identity,
          layoutKey,
          partition: partition?.number ?? null,
        };
        const offset = Number(destination) * MiB;
        if (
          partition &&
          (!destination.trim() ||
            !Number.isSafeInteger(offset) ||
            offset < 0 ||
            offset % disk.sectorSize !== 0 ||
            offset === partition.start ||
            offset > disk.size - partition.size)
        ) {
          throw new Error(t("请输入不同、按逻辑扇区对齐且位于磁盘内的起点。"));
        }
        const next = await invoke<Preview | null>(
          resuming ? "partition_move_resume" : "partition_move_prepare",
          resuming ? { source } : { request: { source, destination: offset } },
        );
        token.current = next?.token ?? null;
        setPreview(next);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setExecuting(false);
      if (once) await onRefresh().catch(() => {});
      inFlight.current = false;
      setBusy(false);
      onBusy(false);
    }
  }

  async function stop() {
    if (!job || stopping) return;
    setStopping(true);
    try {
      await invoke("data_cancel", { id: job.id });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setStopping(false);
    }
  }

  return (
    <Dialog
      title={resuming ? t("继续分区移动") : t("移动分区")}
      subtitle={
        partition
          ? `${displayDisk} · ${partitionTitle(partition, catalogText)}`
          : displayDisk
      }
      onClose={() => {
        if (!inFlight.current) onClose();
      }}
      footer={
        <div className="dialog-actions">
          <button className="button" disabled={busy} onClick={onClose}>
            {submitted ? t("关闭对话框") : t("取消")}
          </button>
          {preview && !busy && !submitted && (
            <button
              className="button"
              onClick={() => {
                releasePreview();
                setPreview(null);
              }}
            >
              {t("返回")}
            </button>
          )}
          {executing && job && (
            <button
              className="button"
              disabled={stopping}
              onClick={() => void stop()}
            >
              {stopping
                ? t("正在停止…")
                : preview?.completed
                  ? t("停止复核")
                  : t("停止并保留断点")}
            </button>
          )}
          {!submitted && (
            <button
              className="button primary"
              disabled={busy}
              onClick={() => void run()}
            >
              {preview
                ? preview.completed
                  ? t("复核已完成的断点")
                  : resuming
                    ? t("确认续作")
                    : t("确认移动")
                : resuming
                  ? t("选择断点文件")
                  : t("选择断点位置并预览")}
            </button>
          )}
        </div>
      }
    >
      {!preview && partition && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void run();
          }}
        >
          <label className="field">
            {t("新起点（MiB）")}
            <input
              type="number"
              min="0"
              step="any"
              value={destination}
              disabled={busy}
              onChange={(e) => setDestination(e.target.value)}
            />
          </label>
          <p className="table-backup-intro">
            {t("原起点 {start} MiB · 大小 {size}", {
              start: displayOffset(partition.start),
              size: formatSize(partition.size),
            })}
          </p>
          <Hint>
            {t("断点文件保存在另一块物理磁盘上。请在移动前备份重要数据。")}
          </Hint>
        </form>
      )}
      {!preview && resuming && (
        <Hint>
          {t("在当前 Windows 用户下，选择此脱机磁盘对应的 .dsmove 断点。")}
        </Hint>
      )}
      {preview && (
        <div className="table-backup-result" aria-label={t("移动预览")}>
          <strong>{preview.target}</strong>
          <dl>
            <div>
              <dt>{t("原起点")}</dt>
              <dd>{displayOffset(preview.sourceOffset)} MiB</dd>
            </div>
            <div>
              <dt>{t("新起点")}</dt>
              <dd>{displayOffset(preview.destinationOffset)} MiB</dd>
            </div>
            <div>
              <dt>{t("分区大小")}</dt>
              <dd>{formatSize(preview.size)}</dd>
            </div>
            <div>
              <dt>{t("断点文件")}</dt>
              <dd>{preview.journalName}</dd>
            </div>
            {preview.resuming && (
              <div>
                <dt>{t("断点状态")}</dt>
                <dd>
                  {preview.completed
                    ? t("已完成")
                    : t("{size} 已写入断点", {
                        size: formatSize(preview.copiedBytes),
                      })}
                </dd>
              </div>
            )}
          </dl>
          <p>
            {preview.completed ? (
              t("复核目标数据、卷引导区及 GPT，磁盘保持脱机。")
            ) : (
              <>
                {preview.overlap && (
                  <>{t("新旧范围重叠，将按方向搬移并逐块回读。")} </>
                )}
                {t(
                  "磁盘在移动期间及结束后保持脱机。中途停止时请保留断点，从磁盘菜单继续。",
                )}
              </>
            )}
          </p>
        </div>
      )}
      {busy && (
        <div aria-live="polite">
          <p className="table-backup-progress">
            <LoaderCircle size={17} className="spin" />
            {executing
              ? job
                ? catalogText(job.phase)
                : t("锁定并核对磁盘…")
              : t("核对移动范围和断点…")}
          </p>
          {executing && job && job.total > 0 && (
            <progress
              aria-label={t("当前移动阶段进度")}
              max={job.total}
              value={job.done}
            />
          )}
        </div>
      )}
      <FormError message={catalogText(error)} />
      {result && (
        <div className="table-backup-result" role="status">
          <p>{catalogText(result.message)}</p>
          <p className="table-backup-digest">
            {t("原始数据 SHA-256")} <code>{result.sha256}</code>
          </p>
        </div>
      )}
    </Dialog>
  );
}

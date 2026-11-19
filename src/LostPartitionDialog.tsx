import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { TableRestoreDialog } from "./TableRestoreDialog";
import { LostBtrfsDialog } from "./LostBtrfsDialog";
import { formatSize, type Disk } from "./types";
import { surfaceRange } from "./surfaceModel";
import {
  candidateRelation,
  freeSearchRanges,
  type LostBtrfsChoice,
  type LostCandidate,
  type LostSnapshot,
} from "./lostPartitionModel";
export function LostPartitionDialog({
  disk,
  disks,
  onClose,
  onBusy,
  onBrowse,
  onRefresh,
}: {
  disk: Disk;
  disks: Disk[];
  onClose: () => void;
  onBusy: (value: boolean) => void;
  onBrowse: () => void;
  onRefresh: () => Promise<void>;
}) {
  const { t, language, catalogText } = useLocale();
  const [start, setStart] = useState("0"),
    [count, setCount] = useState(String(disk.size / disk.sectorSize));
  const [preset, setPreset] = useState("whole");
  const [selected, setSelected] = useState<string[]>([]);
  const [btrfs, setBtrfs] = useState<{
    id: string;
    anchor: LostCandidate;
    choices: LostBtrfsChoice[];
  } | null>(null);
  const [recovery, setRecovery] = useState<{
    id: string;
    candidates: string[];
  } | null>(null);
  const recovering = useRef(false);
  recovering.current = !!recovery;
  const [snapshot, setSnapshot] = useState<LostSnapshot | null>(null),
    [pending, setPending] = useState(false),
    [error, setError] = useState("");
  const control = useRef({ id: "", pending: false, onBusy, handedOff: false });
  control.current.onBusy = onBusy;
  const running = snapshot?.state === "running";
  const busy = pending || running;
  useEffect(() => {
    if (!recovering.current) onBusy(!!busy);
  }, [busy, onBusy]);
  const gaps = freeSearchRanges(disk.partitions, disk.size, disk.sectorSize);
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const next = await invoke<LostSnapshot | null>(
          "lost_partition_status",
          {
            target: disk.hardware?.identity,
          },
        );
        if (
          alive &&
          !recovering.current &&
          next &&
          !control.current.pending &&
          next?.target === disk.hardware?.identity &&
          next?.layoutKey === disk.hardware?.layoutKey &&
          (!control.current.id || control.current.id === next.id)
        ) {
          setSnapshot(next);
          if (!control.current.id) {
            setPreset("custom");
            setStart(String(next.options.startSector));
            setCount(String(next.options.sectorCount));
          }
          control.current.id = next.id;
          control.current.onBusy(next.state === "running");
        }
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      }
      if (alive) timer = setTimeout(() => void poll(), 500);
    }
    void poll();
    return () => {
      alive = false;
      clearTimeout(timer);
      if (!control.current.handedOff && !recovering.current)
        control.current.onBusy(false);
    };
  }, [disk.hardware?.identity, disk.hardware?.layoutKey]);
  async function action(fn: () => Promise<void>) {
    if (control.current.pending) return;
    control.current.pending = true;
    setPending(true);
    onBusy(true);
    setError("");
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      control.current.pending = false;
      setPending(false);
    }
  }
  function choose(value: string) {
    setPreset(value);
    const range = value.startsWith("p:")
      ? disk.partitions[Number(value.slice(2))]
      : value.startsWith("g:")
        ? gaps[Number(value.slice(2))]
        : { start: 0, size: disk.size };
    if (range) {
      setStart(String(range.start / disk.sectorSize));
      setCount(String(range.size / disk.sectorSize));
    }
  }
  let rangeError = "";
  try {
    surfaceRange(start, count, disk.size, disk.sectorSize);
  } catch (e) {
    rangeError = (e instanceof Error ? e.message : String(e)).trim();
  }
  async function begin() {
    await action(async () => {
      const options = surfaceRange(start, count, disk.size, disk.sectorSize);
      setSelected([]);
      const id = await invoke<string>("lost_partition_start", {
        request: {
          source: {
            target: disk.hardware?.identity,
            layoutKey: disk.hardware?.layoutKey,
            partition: null,
          },
          options,
        },
      });
      control.current.id = id;
      setSnapshot({
        id,
        target: disk.hardware!.identity!,
        layoutKey: disk.hardware!.layoutKey!,
        state: "running",
        done: 0,
        total: options.sectorCount * disk.sectorSize,
        options,
        candidates: [],
        message: "",
      });
    });
  }
  if (recovery)
    return (
      <TableRestoreDialog
        disk={disk}
        recovery={recovery}
        onBusy={onBusy}
        onRefresh={onRefresh}
        onClose={onClose}
        onBack={() => setRecovery(null)}
      />
    );
  if (btrfs)
    return (
      <LostBtrfsDialog
        disk={disk}
        disks={disks}
        group={btrfs}
        busy={!!busy}
        error={catalogText(error)}
        onBack={() => {
          setBtrfs(null);
          setError("");
        }}
        onRead={(others, allowMissing) =>
          void action(async () => {
            await invoke("lost_partition_browse_btrfs", {
              request: {
                anchor: { id: btrfs.id, candidate: btrfs.anchor.id },
                others: others.map((choice) => ({
                  id: choice.session,
                  candidate: choice.candidate,
                })),
                allowMissing,
              },
            });
            control.current.handedOff = true;
            onBrowse();
          })
        }
      />
    );
  return (
    <Dialog
      title={t("搜索丢失分区")}
      subtitle={`${diskTitle(disk, t)} · ${formatSize(disk.size)}`}
      wide
      onClose={() => {
        if (!busy) onClose();
      }}
    >
      <div className="lost-range">
        <label>
          {t("搜索范围")}
          <select
            value={preset}
            disabled={busy}
            onChange={(e) => choose(e.target.value)}
          >
            <option value="whole">{t("整个磁盘")}</option>
            <option value="custom">{t("指定扇区范围")}</option>
            {gaps.map((g, i) => (
              <option key={`g:${i}`} value={`g:${i}`}>
                {t("未分配范围 {number}", { number: i + 1 })} ·{" "}
                {formatSize(g.size)}
              </option>
            ))}
            {disk.partitions.map((p, i) => (
              <option key={`p:${i}`} value={`p:${i}`}>
                {t("{partition} 所在范围", {
                  partition: partitionTitle(p, catalogText),
                })}{" "}
                · {formatSize(p.size)}
              </option>
            ))}
          </select>
        </label>
        <div className="lost-range-numbers">
          <label>
            {t("起始 LBA")}
            <input
              value={start}
              disabled={busy}
              inputMode="numeric"
              onChange={(e) => {
                setPreset("custom");
                setStart(e.target.value);
              }}
            />
          </label>
          <label>
            {t("扇区数量")}
            <input
              value={count}
              disabled={busy}
              inputMode="numeric"
              onChange={(e) => {
                setPreset("custom");
                setCount(e.target.value);
              }}
            />
          </label>
        </div>
      </div>
      <p className="lost-note">
        {t(
          "搜索 NTFS、FAT、exFAT、Ext 和 Btrfs 文件系统。候选须完整位于所选范围，请通过文件内容核对。搜索和浏览均为只读。",
        )}
      </p>
      <div className="dialog-actions">
        {running ? (
          <button
            className="button"
            disabled={pending}
            onClick={() =>
              void action(async () => {
                await invoke("data_cancel", { id: snapshot.id });
              })
            }
          >
            {t("停止搜索")}
          </button>
        ) : (
          <button
            className="button primary"
            disabled={pending || !!rangeError}
            onClick={() => void begin()}
          >
            {snapshot ? t("重新搜索") : t("开始搜索")}
          </button>
        )}
      </div>
      <FormError message={catalogText(error || (!busy ? rangeError : ""))} />
      {snapshot && (
        <section className="lost-results" aria-live="polite">
          <div className="lost-progress">
            <span>
              {running
                ? t("正在搜索")
                : snapshot.state === "completed"
                  ? t("搜索结束")
                  : snapshot.state === "canceled"
                    ? t("已停止")
                    : t("搜索未完成")}
            </span>
            <span>
              {formatSize(snapshot.done)} / {formatSize(snapshot.total)}
            </span>
          </div>
          <progress value={snapshot.done} max={snapshot.total} />
          <p className="lost-note">
            {snapshot.message
              ? catalogText(snapshot.message)
              : t("找到 {count} 个候选", {
                  count: snapshot.candidates.length.toLocaleString(language),
                })}
          </p>
          {snapshot.candidates.map((c) => (
            <article className="lost-candidate" key={c.id}>
              <div>
                <label>
                  <input
                    type="checkbox"
                    aria-label={t("选择 {filesystem} 起始 LBA {lba}", {
                      filesystem: c.filesystem,
                      lba: (c.offset / disk.sectorSize).toLocaleString(
                        language,
                      ),
                    })}
                    checked={selected.includes(c.id)}
                    disabled={
                      busy ||
                      !c.canRestore ||
                      candidateRelation(c, disk.partitions) !== "未登记范围"
                    }
                    onChange={(e) =>
                      setSelected((old) =>
                        e.target.checked
                          ? [...old, c.id]
                          : old.filter((id) => id !== c.id),
                      )
                    }
                  />
                  <strong>
                    {c.filesystem} · {formatSize(c.size)}
                  </strong>
                </label>
                <small>
                  {t("起始 LBA")}{" "}
                  {(c.offset / disk.sectorSize).toLocaleString(language)} ·{" "}
                  {catalogText(candidateRelation(c, disk.partitions))}
                  {!c.canBrowse ? (
                    <> · {t("需联合读取")}</>
                  ) : (
                    !c.canRestore && <> · {t("仅浏览与提取")}</>
                  )}
                </small>
              </div>
              <div className="lost-candidate-actions">
                {c.canBrowse && (
                  <button
                    className="button"
                    disabled={busy}
                    onClick={() =>
                      void action(async () => {
                        await invoke("lost_partition_browse", {
                          request: { id: snapshot.id, candidate: c.id },
                        });
                        control.current.handedOff = true;
                        onBrowse();
                      })
                    }
                  >
                    {t("浏览文件")}
                  </button>
                )}
                {(c.btrfsMemberCount ?? 0) > 1 && (
                  <button
                    className="button"
                    disabled={busy}
                    onClick={() =>
                      void action(async () => {
                        const choices = await invoke<LostBtrfsChoice[]>(
                          "lost_partition_btrfs_choices",
                          { request: { id: snapshot.id, candidate: c.id } },
                        );
                        setBtrfs({ id: snapshot.id, anchor: c, choices });
                      })
                    }
                  >
                    {t("联合浏览 Btrfs 成员")}
                  </button>
                )}
              </div>
              <details>
                <summary>{t("识别依据与限制")}</summary>
                <p>
                  {catalogText(c.evidence)}
                  <br />
                  {catalogText(c.warning)}
                </p>
              </details>
            </article>
          ))}
          {!running && !snapshot.candidates.length && (
            <p>
              {t(
                "此范围未找到符合当前检查条件的候选。没有结果不代表文件不可恢复。",
              )}
            </p>
          )}
          {snapshot.candidates.length > 0 && (
            <div className="dialog-actions">
              <button
                className="button primary"
                disabled={busy || !selected.length || selected.length > 128}
                onClick={() =>
                  setRecovery({ id: snapshot.id, candidates: selected })
                }
              >
                {t("恢复所选分区…")}
                {selected.length
                  ? ` (${selected.length.toLocaleString(language)})`
                  : ""}
              </button>
            </div>
          )}
          <p className="lost-note">
            {t(
              "可浏览文件，再选择未登记的候选预览恢复。当前只向完整 GPT 添加普通数据分区；重叠候选、MBR 和损坏分区表重建尚不支持。",
            )}
          </p>
        </section>
      )}
    </Dialog>
  );
}

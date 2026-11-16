import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, FormError } from "./components";
import { TableRestoreDialog } from "./TableRestoreDialog";
import { formatSize, type Disk } from "./types";
import { surfaceRange } from "./surfaceModel";
import {
  candidateRelation,
  freeSearchRanges,
  type LostSnapshot,
} from "./lostPartitionModel";
export function LostPartitionDialog({
  disk,
  onClose,
  onBusy,
  onBrowse,
  onRefresh,
}: {
  disk: Disk;
  onClose: () => void;
  onBusy: (value: boolean) => void;
  onBrowse: () => void;
  onRefresh: () => Promise<void>;
}) {
  const [start, setStart] = useState("0"),
    [count, setCount] = useState(String(disk.size / disk.sectorSize));
  const [preset, setPreset] = useState("whole");
  const [selected, setSelected] = useState<string[]>([]);
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
        const next = await invoke<LostSnapshot | null>("lost_partition_status");
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
        if (alive) setError(String(e));
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
      setError(String(e));
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
    rangeError = String(e);
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
  return (
    <Dialog
      title="搜索丢失分区"
      subtitle={`${disk.name} · ${formatSize(disk.size)}`}
      wide
      onClose={() => {
        if (!busy) onClose();
      }}
    >
      <div className="lost-range">
        <label>
          搜索范围
          <select
            value={preset}
            disabled={busy}
            onChange={(e) => choose(e.target.value)}
          >
            <option value="whole">整个磁盘</option>
            <option value="custom">指定扇区范围</option>
            {gaps.map((g, i) => (
              <option key={`g:${i}`} value={`g:${i}`}>
                未分配范围 {i + 1} · {formatSize(g.size)}
              </option>
            ))}
            {disk.partitions.map((p, i) => (
              <option key={`p:${i}`} value={`p:${i}`}>
                {p.name} 所在范围 · {formatSize(p.size)}
              </option>
            ))}
          </select>
        </label>
        <div className="lost-range-numbers">
          <label>
            起始 LBA
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
            扇区数量
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
        搜索 NTFS、FAT、exFAT 和 Ext
        文件系统边界，候选须完整位于所选范围。识别结果可能来自旧格式或嵌入的镜像，请通过文件内容核对；搜索和浏览均不会修改分区表。
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
            停止搜索
          </button>
        ) : (
          <button
            className="button primary"
            disabled={pending || !!rangeError}
            onClick={() => void begin()}
          >
            {snapshot ? "重新搜索" : "开始搜索"}
          </button>
        )}
      </div>
      <FormError message={error || (!busy ? rangeError : "")} />
      {snapshot && (
        <section className="lost-results" aria-live="polite">
          <div className="lost-progress">
            <span>
              {running
                ? "正在搜索"
                : snapshot.state === "completed"
                  ? "搜索结束"
                  : snapshot.state === "canceled"
                    ? "已停止"
                    : "搜索未完成"}
            </span>
            <span>
              {formatSize(snapshot.done)} / {formatSize(snapshot.total)}
            </span>
          </div>
          <progress value={snapshot.done} max={snapshot.total} />
          <p className="lost-note">
            {snapshot.message || `找到 ${snapshot.candidates.length} 个候选`}
          </p>
          {snapshot.candidates.map((c) => (
            <article className="lost-candidate" key={c.id}>
              <div>
                <label>
                  <input
                    type="checkbox"
                    aria-label={`选择 ${c.filesystem} 起始 LBA ${c.offset / disk.sectorSize}`}
                    checked={selected.includes(c.id)}
                    disabled={
                      busy ||
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
                  起始 LBA {c.offset / disk.sectorSize} ·{" "}
                  {candidateRelation(c, disk.partitions)}
                </small>
              </div>
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
                浏览文件
              </button>
              <details>
                <summary>识别依据与限制</summary>
                <p>
                  {c.evidence}。{c.warning}
                </p>
              </details>
            </article>
          ))}
          {!running && !snapshot.candidates.length && (
            <p>
              此范围未找到符合当前检查条件的候选。没有结果不代表文件不可恢复。
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
                恢复所选分区…{selected.length ? `（${selected.length}）` : ""}
              </button>
            </div>
          )}
          <p className="lost-note">
            可浏览文件，再选择未登记的候选预览恢复。当前只向完整 GPT
            添加普通数据分区；重叠候选、MBR 和损坏分区表重建尚不支持。
          </p>
        </section>
      )}
    </Dialog>
  );
}

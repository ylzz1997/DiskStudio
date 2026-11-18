import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { errorText } from "./partitionOutcome";
import type { SectorSelection } from "./sectorModel";
import { surfaceRange } from "./surfaceModel";
import { formatSize } from "./types";

type CopyJob = {
  id: string;
  state: "running" | "completed" | "failed" | "canceled";
  phase: string;
  done: number;
  total: number;
  message: string;
  sha256: string | null;
};

export function SectorCopyPanel({
  selectSource,
  size,
  sectorSize,
  offset,
  initialSector,
  onBusy,
}: {
  selectSource: () => SectorSelection;
  size: number;
  sectorSize: number;
  offset: number;
  initialSector: number;
  onBusy: (value: boolean) => void;
}) {
  const { t, language, catalogText } = useLocale();
  const integer = (value: number) => value.toLocaleString(language);
  const [start, setStart] = useState(String(initialSector));
  const [count, setCount] = useState("1");
  const [choosing, setChoosing] = useState(false);
  const [id, setId] = useState<string | null>(null);
  const [job, setJob] = useState<CopyJob | null>(null);
  const [error, setError] = useState("");
  const busy = useRef(false);
  const reportBusy = useRef(onBusy);
  useEffect(() => {
    reportBusy.current = onBusy;
  }, [onBusy]);

  let options: ReturnType<typeof surfaceRange> | undefined;
  let rangeError = "";
  try {
    options = surfaceRange(start, count, size, sectorSize);
  } catch (err) {
    rangeError = errorText(err).trim();
  }
  const running = id !== null;

  useEffect(() => {
    if (!id) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function poll() {
      try {
        const next = await invoke<CopyJob | null>("data_status");
        if (!active) return;
        if (next?.id === id) {
          setJob(next);
          if (next.state === "running") {
            timer = setTimeout(() => void poll(), 400);
            return;
          }
        } else {
          setError("任务已结束。");
        }
        setId(null);
        busy.current = false;
        reportBusy.current(false);
      } catch (err) {
        if (!active) return;
        setError(errorText(err));
        timer = setTimeout(() => void poll(), 400);
      }
    }
    void poll();
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [id]);

  async function begin() {
    if (!options || busy.current) return;
    busy.current = true;
    onBusy(true);
    setChoosing(true);
    setError("");
    setJob(null);
    try {
      const taskId = await invoke<string | null>("sector_copy_start", {
        request: { source: selectSource(), options },
      });
      if (taskId) {
        setId(taskId);
        return;
      }
    } catch (err) {
      setError(errorText(err));
    } finally {
      setChoosing(false);
    }
    busy.current = false;
    onBusy(false);
  }

  function changeRange(setValue: (value: string) => void, value: string) {
    setValue(value);
    setJob(null);
    setError("");
  }

  const absoluteStart = offset / sectorSize + (options?.startSector ?? 0);
  return (
    <>
      <div className="sector-search-range">
        <label>
          {t("起始相对 LBA")}
          <input
            type="number"
            min={0}
            max={size / sectorSize - 1}
            value={start}
            disabled={choosing || running}
            onChange={(e) => changeRange(setStart, e.target.value)}
          />
        </label>
        <label>
          {t("扇区数量")}
          <input
            type="number"
            min={1}
            max={size / sectorSize - (options?.startSector ?? 0)}
            value={count}
            disabled={choosing || running}
            onChange={(e) => changeRange(setCount, e.target.value)}
          />
        </label>
        {running ? (
          <button
            className="button"
            onClick={() =>
              void invoke("data_cancel", { id }).catch((err) =>
                setError(errorText(err)),
              )
            }
          >
            {t("取消任务")}
          </button>
        ) : (
          <button
            className="button primary"
            disabled={!options || choosing}
            onClick={() => void begin()}
          >
            {t("选择保存位置…")}
          </button>
        )}
      </div>
      {options && (
        <p className="surface-note">
          {t("绝对 LBA {first}–{last} · {size} B", {
            first: integer(absoluteStart),
            last: integer(absoluteStart + options.sectorCount - 1),
            size: integer(options.sectorCount * sectorSize),
          })}
        </p>
      )}
      <p className="surface-note">
        {t(
          "保存到另一块物理磁盘的新 .bin 文件。复制时会锁定在线源卷，并完整回读文件校验 SHA-256。",
        )}
      </p>
      <FormError message={catalogText((rangeError || error).trim())} />
      {job && (
        <>
          <progress
            className="surface-progress"
            max={job.total || 1}
            value={job.done}
            aria-label={t("数据任务进度")}
          />
          <p className="surface-note">
            {catalogText(job.phase)} · {formatSize(job.done)} /{" "}
            {formatSize(job.total)}
          </p>
          <div role="status">
            {job.message && (
              <p className="surface-message">{catalogText(job.message)}</p>
            )}
            {job.sha256 && <p className="surface-note">SHA-256 {job.sha256}</p>}
          </div>
        </>
      )}
    </>
  );
}
